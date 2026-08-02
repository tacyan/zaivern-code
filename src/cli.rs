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
use std::path::{Path, PathBuf};
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
    std::fs::write(instance_path(), json).map_err(|e| format!("instance.json を書けません: {e}"))
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
        crate::procx::hidden_command("tasklist")
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
fn http(
    inst: &Instance,
    method: &str,
    path: &str,
    body: Option<String>,
) -> Result<(u16, String), String> {
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
        "実行中の Zaivern Code が見つかりません。先に `zai` でエディタを起動してください。"
            .to_string()
    })
}

/// API を叩き、成功なら本文を返す。エラーは日本語メッセージにする。
fn call(inst: &Instance, method: &str, path: &str, body: Option<String>) -> Result<String, String> {
    match http(inst, method, path, body)? {
        (200, b) => Ok(b),
        (401, _) => {
            Err("認証に失敗しました。instance.json のトークンが古い可能性があります。".into())
        }
        (404, _) => Err(format!(
            "この操作は実行中のインスタンスが対応していません ({path})。"
        )),
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
            | "firewall"
            | "worktree"
            | "session"
            | "agent"
            | "help"
            | "--help"
            | "-h"
            | "--version"
            | "-V"
    )
}

/// 単独で指定されたとき、同名のディレクトリが実在するならワークスペース指定
/// (GUI 起動) に譲るサブコマンド。`app` / `session` / `agent` あたりは
/// プロジェクトによくあるフォルダ名なので、副作用のある操作は
/// `zai session list` のようにサブコマンドを明示させる。
fn yields_to_directory(word: &str) -> bool {
    matches!(
        word,
        "app" | "firewall" | "worktree" | "session" | "agent" | "help"
    )
}

/// ヘルプ本文。`zai help` / `zai --help` はこれを丸ごと出し、
/// `zai <cmd> --help` は該当セクションだけを出す。
/// **セクションの実体は 1 箇所** — 全体ヘルプと個別ヘルプが食い違わない。
pub fn help_text() -> String {
    format!("{HELP_HEAD}{HELP_WORKTREE}{HELP_SESSION}{HELP_AGENT}{HELP_TAIL}")
}

const HELP_HEAD: &str = "\
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

実行検知 (どの OS でもスクリプトから起動を確認できます):
  zai status                            実行中の Zaivern Code を一覧 (終了コード: 0=あり 1=なし)
  zai status --json                     一覧を JSON で出力
  zai status --pid-only                 PID だけを 1 行ずつ (| xargs kill 用)

プラグイン (エディタが起動していなくても使えます):
  zai plugin list                       導入済みプラグインを一覧表示
  zai plugin new <名前>                 雛形を作成してパスを表示
  zai plugin enable <名前>              有効化
  zai plugin disable <名前>             無効化

アプリ登録 (OS のアプリ一覧から起動できるようにします):
  zai app install                       Launchpad / アプリメニュー / スタートメニューへ登録
  zai app uninstall                     登録を解除

ファイアウォール (Windows のみ — 📱 スマホリモートの受信許可):
  zai firewall status                   受信許可の状態と、繋がらない原因を表示
  zai firewall allow                    受信を許可 (TCP 8899-8919・管理者の確認あり)
  zai firewall revoke                   受信許可を取り消す
  zai firewall unblock                  「すべての受信接続をブロックする」を解除
                                        (この設定が入っていると許可規則は無視されます)

";

/// `zai worktree --help` のセクション。
pub const HELP_WORKTREE: &str = "\
worktree (ヘッドレス導線 — エディタが起動していなくても使えます):
  zai worktree create <ブランチ> [--from <ベース>]
                                        .claude/worktrees/ 配下に worktree を作り
                                        絶対パスを 1 行で出力 (--from の既定は HEAD)
  zai worktree list [--json]            worktree を一覧
                                        (既定は ブランチ→HEAD→状態→パス のタブ区切り)
  zai worktree remove <ブランチ> [--force]
                                        worktree を削除 (ブランチ自体は残ります)

";

/// `zai session --help` のセクション。
pub const HELP_SESSION: &str = "\
session (エージェントセッションの一覧・操作):
  zai session list [--json]             ID / 状態 / エージェント / 最終更新 / 作業フォルダ
                                        状態: running=実行中 exited=終了済 stored=記録のみ
  zai session send <ID> <テキスト>      実行中セッションへ送信 (終了済みなら何もせずエラー)
  zai session log <ID> [--tail N]       生ログの末尾 N 行 (既定 50 行)

";

/// `zai agent --help` のセクション。
pub const HELP_AGENT: &str = "\
agent (対応エージェント CLI):
  zai agent list [--json]               名前 / 導入状況 / 起動コマンド / 実体のパス
                                        (PATH を自前で走査。Windows は PATHEXT も見ます)

";

const HELP_TAIL: &str = "\
その他:
  zai help                              このヘルプ
  zai <サブコマンド> --help             そのサブコマンドの使い方だけを表示
  zai --help | -h                       このヘルプ
  zai --version | -V                    バージョン

終了コード: 0 = 成功 / 1 = 実行時エラー / 2 = 引数の指定ミス
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
    // "session" / "agent" / "worktree" / "firewall" / "help" も同じ扱い
    // (副作用のある操作はサブコマンドを明示させる)。
    if rest.is_empty() && yields_to_directory(first) && Path::new(first).is_dir() {
        return None;
    }
    Some(match first.as_str() {
        "help" | "--help" | "-h" => {
            println!("{}", help_text());
            0
        }
        "--version" | "-V" => {
            println!("Zaivern Code {}", env!("CARGO_PKG_VERSION"));
            0
        }
        // `zai status` (引数なし / --json のみ) はレジスタリ一覧 = 実行検知。
        // テキスト付きは従来どおりステータスバー更新 (下の run_remote へ落ちる)。
        "status" if status_list_mode(rest).is_some() => run_status_list(
            &crate::instances::instances_dir(),
            status_list_mode(rest).unwrap_or(StatusFmt::Table),
        ),
        "plugin" => run_plugin(rest),
        "app" => crate::desktop::run(rest),
        "firewall" => crate::firewall::run(rest),
        // ヘッドレス導線 (実行中インスタンスが無くても動くものが大半)
        "worktree" => finish(worktree_dispatch(rest)),
        "session" => finish(session_dispatch(rest)),
        "agent" => finish(agent_dispatch(rest)),
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

// ───────────────────────── status: 実行検知 (インスタンス不要) ─────────────────────────

/// `zai status` を「実行中インスタンスの一覧表示」として扱うか。
/// `Some(json)` = レジストリ一覧 (ローカルで完結)、`None` = 従来の
/// ステータスバー更新 (テキスト付き — 実行中のエディタへリモート送信)。
fn status_list_mode(args: &[String]) -> Option<StatusFmt> {
    match args {
        [] => Some(StatusFmt::Table),
        [flag] if flag == "--json" => Some(StatusFmt::Json),
        [flag] if flag == "--pid-only" => Some(StatusFmt::Pids),
        _ => None,
    }
}

/// `zai status` の出力形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StatusFmt {
    /// 人が読む表。
    Table,
    /// 機械可読な JSON。
    Json,
    /// PID を 1 行ずつ (`zai status --pid-only | xargs kill` 用)。
    Pids,
}

/// レジストリ (`~/.zaivern/instances`) を走査して一覧を出す。
/// 終了コード: 0 = 1 つ以上実行中、1 = なし (スクリプト/CI から使える)。
fn run_status_list(dir: &std::path::Path, fmt: StatusFmt) -> i32 {
    let entries = crate::instances::scan_and_prune(dir);
    match fmt {
        StatusFmt::Json => println!("{}", crate::instances::render_json(&entries)),
        // 空のときは何も出さない (空行を xargs に渡さないため)。
        StatusFmt::Pids => {
            if !entries.is_empty() {
                println!("{}", crate::instances::render_pids(&entries));
            }
        }
        StatusFmt::Table => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            println!("{}", crate::instances::render_table(&entries, now));
        }
    }
    if entries.is_empty() {
        1
    } else {
        0
    }
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
                return Err(format!(
                    "--level は info / warn / error のいずれかです: {level}"
                ));
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
        let mark = if cfg.is_enabled(&p.name) {
            "有効"
        } else {
            "無効"
        };
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

// ───────────────────────── 終了コードとエラー ─────────────────────────

/// 成功。
pub const EXIT_OK: i32 = 0;
/// 引数は正しいが実行できなかった (git の失敗・対象が無い・接続できない等)。
pub const EXIT_RUNTIME: i32 = 1;
/// 引数の指定が間違っている (欠落・未知のサブコマンド・数値でない等)。
pub const EXIT_USAGE: i32 = 2;

/// サブコマンドの失敗。**終了コードを型で持つ** — 呼び出し側で 1 と 2 を
/// 取り違えると、スクリプトが「使い方の間違い」と「実行できなかった」を
/// 区別できなくなるため、メッセージと一緒に運ぶ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CliError {
    /// → 終了コード 2
    Usage(String),
    /// → 終了コード 1
    Runtime(String),
}

impl CliError {
    pub fn code(&self) -> i32 {
        match self {
            Self::Usage(_) => EXIT_USAGE,
            Self::Runtime(_) => EXIT_RUNTIME,
        }
    }
    pub fn message(&self) -> &str {
        match self {
            Self::Usage(m) | Self::Runtime(m) => m,
        }
    }
}

/// サブコマンドの結果。`Ok` の中身は標準出力へ出す本文 (空なら何も出さない)。
type CliOut = Result<String, CliError>;

/// 結果を出力して終了コードへ落とす。成功は stdout、失敗は stderr。
fn finish(r: CliOut) -> i32 {
    match r {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            EXIT_OK
        }
        Err(e) => {
            eprintln!("{}", e.message());
            e.code()
        }
    }
}

/// `--help` / `-h` が含まれているか (どのサブコマンドでも同じ綴りで効かせる)。
fn wants_help(args: &[String]) -> bool {
    args.iter().any(|a| a == "--help" || a == "-h")
}

/// 先頭の位置引数を取り出す。無ければ使い方を添えて引数エラー。
fn positional(rest: &[String], usage: &str) -> Result<String, CliError> {
    match rest.first() {
        Some(v) if !v.is_empty() => Ok(v.clone()),
        _ => Err(CliError::Usage(usage.to_string())),
    }
}

/// 余分な位置引数を拒否する (打ち間違いを黙って無視しない)。
fn reject_extra(rest: &[String], usage: &str) -> Result<(), CliError> {
    if rest.is_empty() {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "余分な引数です: {} — 使い方: {usage}",
        rest.join(" ")
    )))
}

/// カレントディレクトリ (取得できない環境でも panic しない)。
fn current_dir() -> Result<PathBuf, CliError> {
    std::env::current_dir()
        .map_err(|e| CliError::Runtime(format!("カレントディレクトリを取得できません: {e}")))
}

/// ファイルの最終更新 (epoch 秒)。取れなければ 0。
fn mtime_epoch(p: &Path) -> u64 {
    std::fs::metadata(p)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `{"ok":true}` / `{"ok":false,"error":"…"}` を終了コードへ落とす。
///
/// remote.rs は「実行できなかった」も HTTP 200 + `ok:false` で返すので、
/// ここを通さないと失敗が終了コード 0 になってしまう。
pub fn check_api_ok(body: &str) -> Result<(), CliError> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(body) else {
        return Err(CliError::Runtime(format!(
            "応答を解釈できません: {}",
            body.trim()
        )));
    };
    if v.get("ok").and_then(|o| o.as_bool()).unwrap_or(false) {
        return Ok(());
    }
    Err(CliError::Runtime(
        v.get("error")
            .and_then(|e| e.as_str())
            .unwrap_or("失敗しました")
            .to_string(),
    ))
}

// ───────────────────────── worktree サブコマンド ─────────────────────────

/// `git -C <dir> <args>`。呼び出し方 (色無効・quotepath 無効・エンコーディング
/// 復号) は git.rs に集約してあるので、そこへ委譲する。
fn git_out(dir: &Path, args: &[&str]) -> Result<String, String> {
    let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    crate::git::run_git_at(dir, &owned)
}

/// worktree の置き場: `<メインリポジトリ>/.claude/worktrees`。
///
/// **リポジトリルートからの導出のみ** — 絶対パスを直書きしないので、
/// どの OS・どのユーザー名でも同じように動く。
pub fn worktrees_base(main_root: &Path) -> PathBuf {
    main_root.join(".claude").join("worktrees")
}

/// メインリポジトリのルート。
///
/// `--git-common-dir` は全 worktree で共有される `.git` を返すので、
/// **worktree の中で実行しても本体のルートを指す** (= `.claude/worktrees` が
/// 入れ子にならない)。取れない git では `--show-toplevel` へ落とす。
fn main_repo_root(cwd: &Path) -> Result<PathBuf, String> {
    let common = git_out(
        cwd,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .or_else(|_| git_out(cwd, &["rev-parse", "--git-common-dir"]))
    .map_err(|e| format!("git リポジトリではありません: {e}"))?;
    let p = PathBuf::from(common.trim());
    let p = if p.is_absolute() { p } else { cwd.join(p) };
    if p.file_name()
        .map(|n| n.to_string_lossy() == ".git")
        .unwrap_or(false)
    {
        if let Some(parent) = p.parent() {
            return Ok(parent.to_path_buf());
        }
    }
    // bare リポジトリなど `.git` で終わらない形はトップレベルで代替する
    git_out(cwd, &["rev-parse", "--show-toplevel"])
        .map(|s| PathBuf::from(s.trim()))
        .map_err(|e| format!("リポジトリのルートを特定できません: {e}"))
}

/// ブランチ名 → worktree のフォルダ名。
///
/// `feat/x` のような区切り入りをそのままフォルダ名にはできないので、
/// 英数字・`-`・`_` 以外を `-` へ潰して連続を畳む。**`.` も落ちる**ため
/// `..` による親ディレクトリ脱出は原理的に起こらない。
pub fn worktree_dir_name(branch: &str) -> String {
    let mut out = String::with_capacity(branch.len());
    for c in branch.chars() {
        if c.is_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// ブランチ名として受け付けられるか。git へ渡す前にここで弾く。
fn validate_branch(b: &str) -> Result<(), CliError> {
    if b.is_empty() {
        return Err(CliError::Usage("ブランチ名が空です".into()));
    }
    if b.starts_with('-') {
        return Err(CliError::Usage(format!(
            "ブランチ名がオプションと区別できません: {b}"
        )));
    }
    if b.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(CliError::Usage(format!(
            "ブランチ名に空白や制御文字は使えません: {b}"
        )));
    }
    Ok(())
}

/// `git worktree list --porcelain` の 1 レコード。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct WorktreeRow {
    pub path: String,
    pub head: String,
    /// `refs/heads/` を落としたブランチ名。detached / bare では空。
    pub branch: String,
    pub detached: bool,
    pub locked: bool,
    pub prunable: bool,
}

/// `git worktree list --porcelain` を行レコードへ (純粋関数)。
///
/// Windows のチェックアウトや git のバージョン差で改行が CRLF になっても
/// 同じ結果になるよう、先に正規化してから読む。
pub fn parse_worktree_porcelain(text: &str) -> Vec<WorktreeRow> {
    let text = text.replace("\r\n", "\n");
    let mut rows: Vec<WorktreeRow> = Vec::new();
    let mut cur: Option<WorktreeRow> = None;
    for line in text.lines() {
        let line = line.trim_end();
        if let Some(p) = line.strip_prefix("worktree ") {
            if let Some(r) = cur.take() {
                rows.push(r);
            }
            cur = Some(WorktreeRow {
                path: p.to_string(),
                ..Default::default()
            });
            continue;
        }
        let Some(r) = cur.as_mut() else { continue };
        if let Some(h) = line.strip_prefix("HEAD ") {
            r.head = h.to_string();
        } else if let Some(b) = line.strip_prefix("branch ") {
            r.branch = b.strip_prefix("refs/heads/").unwrap_or(b).to_string();
        } else if line == "detached" {
            r.detached = true;
        } else if line == "locked" || line.starts_with("locked ") {
            r.locked = true;
        } else if line == "prunable" || line.starts_with("prunable ") {
            r.prunable = true;
        }
    }
    if let Some(r) = cur.take() {
        rows.push(r);
    }
    rows
}

/// ブランチ名 / フォルダ名 のどちらでも worktree を引く (純粋関数)。
pub fn find_worktree<'a>(rows: &'a [WorktreeRow], key: &str) -> Option<&'a WorktreeRow> {
    let dir_name = worktree_dir_name(key);
    let by_dir = |want: &str, r: &WorktreeRow| {
        !want.is_empty()
            && Path::new(&r.path)
                .file_name()
                .map(|n| n.to_string_lossy() == want)
                .unwrap_or(false)
    };
    rows.iter()
        .find(|r| !r.branch.is_empty() && r.branch == key)
        .or_else(|| rows.iter().find(|r| by_dir(key, r)))
        .or_else(|| rows.iter().find(|r| by_dir(&dir_name, r)))
}

/// worktree 一覧の整形 (純粋関数)。
/// 既定は `ブランチ\tHEAD\t状態\tパス` の 1 行 1 レコード。
pub fn render_worktrees(rows: &[WorktreeRow], json: bool) -> String {
    if json {
        return serde_json::to_string(rows).unwrap_or_else(|_| "[]".to_string());
    }
    if rows.is_empty() {
        return "worktree はありません。".to_string();
    }
    rows.iter()
        .map(|r| {
            let branch = if !r.branch.is_empty() {
                r.branch.clone()
            } else if r.detached {
                "(detached)".to_string()
            } else {
                "-".to_string()
            };
            let head: String = r.head.chars().take(7).collect();
            let head = if head.is_empty() {
                "-".to_string()
            } else {
                head
            };
            let mut state: Vec<&str> = Vec::new();
            if r.locked {
                state.push("locked");
            }
            if r.prunable {
                state.push("prunable");
            }
            let state = if state.is_empty() {
                "ok".to_string()
            } else {
                state.join(",")
            };
            format!("{branch}\t{head}\t{state}\t{}", r.path)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn worktree_dispatch(args: &[String]) -> CliOut {
    if wants_help(args) {
        return Ok(HELP_WORKTREE.trim_end().to_string());
    }
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest: &[String] = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "create" => worktree_create(rest),
        "list" => worktree_list(rest),
        "remove" => worktree_remove(rest),
        "" => Err(CliError::Usage(
            "worktree のサブコマンドを指定してください: create / list / remove".into(),
        )),
        other => Err(CliError::Usage(format!(
            "不明な worktree サブコマンドです: {other}"
        ))),
    }
}

fn worktree_create(args: &[String]) -> CliOut {
    const USAGE: &str = "zai worktree create <ブランチ> [--from <ベース>]";
    let (from, rest) = take_opt(args, "--from");
    let branch = positional(&rest, USAGE)?;
    validate_branch(&branch)?;
    reject_extra(&rest[1..], USAGE)?;
    let dir_name = worktree_dir_name(&branch);
    if dir_name.is_empty() {
        return Err(CliError::Usage(format!(
            "フォルダ名に使える文字がありません: {branch}"
        )));
    }
    let cwd = current_dir()?;
    let root = main_repo_root(&cwd).map_err(CliError::Runtime)?;
    let base_dir = worktrees_base(&root);
    let dir = base_dir.join(&dir_name);
    if dir.exists() {
        return Err(CliError::Runtime(format!(
            "すでに存在します: {}",
            dir.display()
        )));
    }
    std::fs::create_dir_all(&base_dir)
        .map_err(|e| CliError::Runtime(format!("worktree の置き場を作れません: {e}")))?;
    let base = from.unwrap_or_else(|| "HEAD".to_string());
    let dir_s = dir.to_string_lossy().into_owned();
    git_out(&root, &["worktree", "add", "-b", &branch, &dir_s, &base])
        .map_err(CliError::Runtime)?;
    // 出力は作成したパス 1 行だけ — `cd "$(zai worktree create x)"` が書ける。
    Ok(dir_s)
}

fn worktree_list(args: &[String]) -> CliOut {
    const USAGE: &str = "zai worktree list [--json]";
    let (json, rest) = take_flag(args, "--json");
    reject_extra(&rest, USAGE)?;
    let cwd = current_dir()?;
    let root = main_repo_root(&cwd).map_err(CliError::Runtime)?;
    let out = git_out(&root, &["worktree", "list", "--porcelain"]).map_err(CliError::Runtime)?;
    Ok(render_worktrees(&parse_worktree_porcelain(&out), json))
}

fn worktree_remove(args: &[String]) -> CliOut {
    const USAGE: &str = "zai worktree remove <ブランチ> [--force]";
    let (force, rest) = take_flag(args, "--force");
    let branch = positional(&rest, USAGE)?;
    validate_branch(&branch)?;
    reject_extra(&rest[1..], USAGE)?;
    let cwd = current_dir()?;
    let root = main_repo_root(&cwd).map_err(CliError::Runtime)?;
    let listing =
        git_out(&root, &["worktree", "list", "--porcelain"]).map_err(CliError::Runtime)?;
    let rows = parse_worktree_porcelain(&listing);
    let target = find_worktree(&rows, &branch).ok_or_else(|| {
        CliError::Runtime(format!(
            "{branch} の worktree が見つかりません (zai worktree list で確認してください)"
        ))
    })?;
    // 削除の引数組み立ては race.rs と共有する (--force の付け方を二重に持たない)。
    let git_args = crate::race::worktree_remove_args(&target.path, force);
    let refs: Vec<&str> = git_args.iter().map(String::as_str).collect();
    git_out(&root, &refs).map_err(CliError::Runtime)?;
    let _ = git_out(&root, &["worktree", "prune"]);
    Ok(format!(
        "削除しました: {} (ブランチ {branch} はそのまま残ります)",
        target.path
    ))
}

// ───────────────────────── session サブコマンド ─────────────────────────

/// `zai session log` の既定行数。
const DEFAULT_TAIL: usize = 50;

/// `zai session list` の 1 レコード。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionRow {
    /// セッション ID (生ログのファイル名末尾の数字)。
    pub id: String,
    /// エージェント名 (プリセット名。無ければタブのタイトル)。
    pub agent: String,
    /// 起動ディレクトリ。
    pub workspace: String,
    /// `running` = 実行中 / `exited` = 終了済 / `stored` = 記録のみ。
    pub state: String,
    /// 最終更新 (epoch 秒)。
    pub updated: u64,
    /// 生ログの絶対パス。
    pub log: String,
}

/// 実行中インスタンスから見えるセッション 1 本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub id: String,
    pub title: String,
    pub running: bool,
}

/// 生ログのパス (`<タイトル>-<セッションID>.log`) から ID を取り出す (純粋関数)。
/// 形が違えば空文字 = ID 不明。
pub fn session_id_from_log(log_file: &str) -> String {
    let stem = Path::new(log_file)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match stem.rsplit_once('-') {
        Some((_, tail)) if !tail.is_empty() && tail.chars().all(|c| c.is_ascii_digit()) => {
            tail.to_string()
        }
        _ => String::new(),
    }
}

/// 同じ ID の重複を畳み、最終更新の新しい順 → ID 順に並べる (純粋関数)。
///
/// マルチルートのワークスペースは同じ内容を 2 つのキーで保存するので、
/// 畳まないと同じセッションが二重に出る。
pub fn dedup_and_sort_sessions(mut rows: Vec<SessionRow>) -> Vec<SessionRow> {
    rows.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| a.id.cmp(&b.id)));
    let mut seen = std::collections::HashSet::new();
    rows.retain(|r| seen.insert(r.id.clone()));
    rows
}

/// 実行中インスタンスの状態を記録へ重ねる (純粋関数)。
/// 記録に無いセッションは行として足す (起動直後で未保存でも `send` できる)。
pub fn merge_live_sessions(
    mut rows: Vec<SessionRow>,
    live: &[LiveSession],
    workspace: &str,
) -> Vec<SessionRow> {
    for l in live {
        let state = if l.running { "running" } else { "exited" };
        match rows.iter_mut().find(|r| r.id == l.id) {
            Some(r) => {
                r.state = state.to_string();
                if r.agent.is_empty() {
                    r.agent = l.title.clone();
                }
            }
            None => rows.push(SessionRow {
                id: l.id.clone(),
                agent: l.title.clone(),
                workspace: workspace.to_string(),
                state: state.to_string(),
                updated: 0,
                log: String::new(),
            }),
        }
    }
    dedup_and_sort_sessions(rows)
}

/// `/api/state` の応答から実行中セッションを取り出す (純粋関数)。
pub fn parse_live_sessions(state_json: &str) -> Vec<LiveSession> {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(state_json) else {
        return Vec::new();
    };
    v.get("agents")
        .and_then(|a| a.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|a| {
                    let id = a.get("id")?.as_u64()?;
                    Some(LiveSession {
                        id: id.to_string(),
                        title: a
                            .get("title")
                            .and_then(|t| t.as_str())
                            .unwrap_or("")
                            .to_string(),
                        running: a.get("running").and_then(|r| r.as_bool()).unwrap_or(false),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// セッション一覧の整形 (純粋関数)。
/// 既定は `ID\t状態\tエージェント\t最終更新\t作業フォルダ` の 1 行 1 レコード。
pub fn render_sessions(rows: &[SessionRow], json: bool) -> String {
    if json {
        return serde_json::to_string(rows).unwrap_or_else(|_| "[]".to_string());
    }
    if rows.is_empty() {
        return "記録されているセッションはありません。".to_string();
    }
    rows.iter()
        .map(|r| {
            format!(
                "{}\t{}\t{}\t{}\t{}",
                r.id, r.state, r.agent, r.updated, r.workspace
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// PTY 生ログを「1 行 1 レコード」に均す (純粋関数)。
///
/// 端末は `\r` 単独でも行頭へ戻って上書きする (スピナー・プロンプトの再描画)。
/// そのまま出すと何画面分もが 1 行に詰まって見えるうえ、パイプの先で
/// 行数が数えられないので、`\r` も改行として扱う。
pub fn normalize_log_lines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// 末尾 `n` 行 (純粋関数)。`n` が 0 なら空、行数がそれ未満なら全部。
pub fn tail_lines(text: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let lines: Vec<&str> = text.lines().collect();
    let from = lines.len().saturating_sub(n);
    lines[from..].join("\n")
}

/// `~/.zaivern/sessions/*.toml` を走査して記録を行へ (I/O はここだけ)。
fn collect_session_rows(dir: &Path) -> Vec<SessionRow> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().map(|x| x == "toml").unwrap_or(false))
        .collect();
    files.sort();
    let mut rows: Vec<SessionRow> = Vec::new();
    for f in &files {
        let Ok(raw) = std::fs::read_to_string(f) else {
            continue;
        };
        let Ok(data) = toml::from_str::<crate::session::SessionData>(&raw) else {
            continue;
        };
        let file_epoch = mtime_epoch(f);
        for a in &data.agents {
            let id = session_id_from_log(&a.log_file);
            if id.is_empty() {
                continue;
            }
            let agent = if a.preset_name.is_empty() {
                a.title.clone()
            } else {
                a.preset_name.clone()
            };
            rows.push(SessionRow {
                id,
                agent,
                workspace: a.cwd.clone(),
                state: "stored".to_string(),
                updated: mtime_epoch(Path::new(&a.log_file)).max(file_epoch),
                log: a.log_file.clone(),
            });
        }
    }
    dedup_and_sort_sessions(rows)
}

fn session_dispatch(args: &[String]) -> CliOut {
    if wants_help(args) {
        return Ok(HELP_SESSION.trim_end().to_string());
    }
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest: &[String] = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "list" => session_list(rest),
        "send" => session_send(rest),
        "log" => session_log(rest),
        "" => Err(CliError::Usage(
            "session のサブコマンドを指定してください: list / send / log".into(),
        )),
        other => Err(CliError::Usage(format!(
            "不明な session サブコマンドです: {other}"
        ))),
    }
}

fn session_list(args: &[String]) -> CliOut {
    const USAGE: &str = "zai session list [--json]";
    let (json, rest) = take_flag(args, "--json");
    reject_extra(&rest, USAGE)?;
    let mut rows = collect_session_rows(&crate::session::sessions_dir());
    // 実行中インスタンスがあれば状態を上書きする。無くても一覧は出る
    // (記録の閲覧にエディタの起動を要求しない)。
    if let Some(inst) = read_instance_file() {
        if let Ok(state) = call(&inst, "GET", "/api/state", None) {
            rows = merge_live_sessions(rows, &parse_live_sessions(&state), &inst.workspace);
        }
    }
    Ok(render_sessions(&rows, json))
}

fn session_send(args: &[String]) -> CliOut {
    const USAGE: &str = "zai session send <ID> <テキスト>";
    let id = positional(args, USAGE)?;
    let id_num: i64 = id
        .parse()
        .map_err(|_| CliError::Usage(format!("セッション ID は数値です: {id}")))?;
    if id_num < 0 {
        // 負値は remote.rs では「全セッションへブロードキャスト」を意味する。
        // `send <ID>` の約束と食い違うので、ここで弾く。
        return Err(CliError::Usage(format!(
            "セッション ID は 0 以上です: {id}"
        )));
    }
    let text = args[1..].join(" ");
    if text.trim().is_empty() {
        return Err(CliError::Usage(format!(
            "送るテキストがありません: {USAGE}"
        )));
    }
    let inst = require_instance().map_err(CliError::Runtime)?;
    let body = serde_json::json!({ "text": text, "id": id_num, "submit": true }).to_string();
    let resp = call(&inst, "POST", "/api/voice", Some(body)).map_err(CliError::Runtime)?;
    // 終了済み / 見つからないセッションは **エラーが返るだけ** で終わる。
    // ここから kill は絶対に撃たない (PID 再利用の巻き添えを避ける)。
    check_api_ok(&resp)?;
    Ok(format!("送信しました: セッション {id_num}"))
}

fn session_log(args: &[String]) -> CliOut {
    const USAGE: &str = "zai session log <ID> [--tail N]";
    let (tail, rest) = take_opt(args, "--tail");
    let n: usize = match tail {
        None => DEFAULT_TAIL,
        Some(v) => v
            .parse()
            .map_err(|_| CliError::Usage(format!("--tail は 0 以上の整数です: {v}")))?,
    };
    let id = positional(&rest, USAGE)?;
    reject_extra(&rest[1..], USAGE)?;
    let rows = collect_session_rows(&crate::session::sessions_dir());
    let row = rows.iter().find(|r| r.id == id).ok_or_else(|| {
        CliError::Runtime(format!(
            "セッション {id} の記録がありません (zai session list で確認してください)"
        ))
    })?;
    if row.log.is_empty() {
        return Err(CliError::Runtime(format!(
            "セッション {id} には生ログがありません"
        )));
    }
    let path = PathBuf::from(&row.log);
    if !path.exists() && !path.with_extension("log.old").exists() {
        return Err(CliError::Runtime(format!(
            "ログファイルがありません: {}",
            path.display()
        )));
    }
    let raw = crate::session::read_term_log_tail(&path, crate::session::REPLAY_TAIL_CAP);
    let text = crate::textenc::decode_output(&raw);
    // ANSI と `\r` 上書きを落として「1 行 1 レコード」にする
    // (端末を壊さずパイプへ流せる)。
    let clean = normalize_log_lines(&crate::supervisor::strip_ansi(&text));
    Ok(tail_lines(&clean, n))
}

// ───────────────────────── agent サブコマンド ─────────────────────────

/// `zai agent list` の 1 レコード。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentRow {
    pub bin: String,
    pub label: String,
    /// そのまま端末へ打てる起動コマンド (`kiro-cli chat --tui` 等)。
    pub command: String,
    pub installed: bool,
    /// 見つかった実体の絶対パス。未導入なら空。
    pub path: String,
    /// 未導入時の導入コマンド。
    pub install_hint: String,
}

/// カタログ + 実体探索 → 一覧行 (純粋関数)。
///
/// 探索は引数で受け取るので、PATH を汚さずにテーブルテストできる。
/// 実運用では [`crate::shellenv::which`] を渡す — サブプロセスを起こさず
/// `PATH` を自前で走査し、Windows では `PATHEXT` の拡張子も試す実装。
pub fn agent_rows(probe: impl Fn(&str) -> Option<PathBuf>) -> Vec<AgentRow> {
    crate::agents::AGENT_CATALOG
        .iter()
        .map(|s| {
            let found = probe(s.bin);
            AgentRow {
                bin: s.bin.to_string(),
                label: s.label.to_string(),
                command: s.launch_command(),
                installed: found.is_some(),
                path: found
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                install_hint: s.install.to_string(),
            }
        })
        .collect()
}

/// エージェント一覧の整形 (純粋関数)。
/// 既定は `実行ファイル\t導入状況\t表示名\t起動コマンド\tパスまたは導入方法`。
pub fn render_agents(rows: &[AgentRow], json: bool) -> String {
    if json {
        return serde_json::to_string(rows).unwrap_or_else(|_| "[]".to_string());
    }
    if rows.is_empty() {
        return "対応エージェントがありません。".to_string();
    }
    rows.iter()
        .map(|r| {
            let mark = if r.installed { "installed" } else { "missing" };
            let tail = if r.path.is_empty() {
                r.install_hint.clone()
            } else {
                r.path.clone()
            };
            format!("{}\t{mark}\t{}\t{}\t{tail}", r.bin, r.label, r.command)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn agent_dispatch(args: &[String]) -> CliOut {
    if wants_help(args) {
        return Ok(HELP_AGENT.trim_end().to_string());
    }
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest: &[String] = if args.is_empty() { &[] } else { &args[1..] };
    match sub {
        "list" => {
            const USAGE: &str = "zai agent list [--json]";
            let (json, rest) = take_flag(rest, "--json");
            reject_extra(&rest, USAGE)?;
            Ok(render_agents(&agent_rows(crate::shellenv::which), json))
        }
        "" => Err(CliError::Usage(
            "agent のサブコマンドを指定してください: list".into(),
        )),
        other => Err(CliError::Usage(format!(
            "不明な agent サブコマンドです: {other}"
        ))),
    }
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
            "open",
            "notify",
            "prompt",
            "run",
            "panel",
            "status",
            "state",
            "plugin",
            "app",
            "firewall",
            "worktree",
            "session",
            "agent",
            "help",
            "--help",
            "-h",
            "--version",
            "-V",
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
        assert_eq!(try_run_cli(&v(&["help"])), Some(0));
        assert_eq!(try_run_cli(&v(&["--help"])), Some(0));
        assert_eq!(try_run_cli(&v(&["-h"])), Some(0));
        assert_eq!(try_run_cli(&v(&["--version"])), Some(0));
        assert_eq!(try_run_cli(&v(&["-V"])), Some(0));
    }

    // ── ヘルプ文言 ──

    #[test]
    fn help_lists_every_subcommand() {
        let help = help_text();
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
            "zai firewall status",
            "zai firewall allow",
            "zai firewall revoke",
            "zai firewall unblock",
            // ヘッドレス導線 (到達できないコマンドを作らないための番人)
            "zai worktree create",
            "zai worktree list",
            "zai worktree remove",
            "zai session list",
            "zai session send",
            "zai session log",
            "zai agent list",
            "zai help",
            "--help",
            "--version",
        ] {
            assert!(help.contains(needle), "ヘルプに {needle} が無い");
        }
    }

    #[test]
    fn help_is_japanese() {
        let help = help_text();
        assert!(help.contains("使い方:"));
        assert!(help.contains("サブコマンド"));
        assert!(help.contains("終了コード: 0 = 成功 / 1 = 実行時エラー / 2 = 引数の指定ミス"));
    }

    /// 個別ヘルプは全体ヘルプの一部でなければならない (二重管理で食い違わせない)。
    #[test]
    fn per_command_help_is_a_slice_of_the_full_help() {
        let help = help_text();
        for section in [HELP_WORKTREE, HELP_SESSION, HELP_AGENT] {
            assert!(
                help.contains(section),
                "全体ヘルプに含まれていない:\n{section}"
            );
        }
        for (args, section) in [
            (v(&["--help"]), HELP_WORKTREE),
            (v(&["list", "-h"]), HELP_WORKTREE),
        ] {
            assert_eq!(worktree_dispatch(&args), Ok(section.trim_end().to_string()));
        }
        assert_eq!(
            session_dispatch(&v(&["--help"])),
            Ok(HELP_SESSION.trim_end().to_string())
        );
        assert_eq!(
            agent_dispatch(&v(&["-h"])),
            Ok(HELP_AGENT.trim_end().to_string())
        );
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

    // ── status: 実行検知 ──

    #[test]
    fn status_list_mode_classification() {
        // 引数なし = 一覧 (テーブル)、--json のみ = 一覧 (JSON)
        assert_eq!(status_list_mode(&[]), Some(StatusFmt::Table));
        assert_eq!(status_list_mode(&v(&["--json"])), Some(StatusFmt::Json));
        assert_eq!(status_list_mode(&v(&["--pid-only"])), Some(StatusFmt::Pids));
        // テキスト付きは従来のステータスバー更新 (リモート) のまま
        assert_eq!(status_list_mode(&v(&["hello"])), None);
        assert_eq!(status_list_mode(&v(&["--json", "x"])), None);
        assert_eq!(status_list_mode(&v(&["ビルド中…"])), None);
    }

    #[test]
    fn status_list_empty_dir_exits_one() {
        let dir = crate::test_util::unique_temp_dir("zaivern-cli-test", "status-empty");
        assert_eq!(
            run_status_list(&dir, StatusFmt::Table),
            1,
            "実行中なし = 終了コード 1"
        );
        assert_eq!(run_status_list(&dir, StatusFmt::Json), 1);
        // 存在しないディレクトリでも落ちずに 1
        assert_eq!(run_status_list(&dir.join("ghost"), StatusFmt::Table), 1);
    }

    #[test]
    fn status_list_running_instance_exits_zero() {
        let dir = crate::test_util::unique_temp_dir("zaivern-cli-test", "status-alive");
        let _guard = crate::instances::register_in(&dir, &[std::path::PathBuf::from("/ws")])
            .expect("register");
        assert_eq!(
            run_status_list(&dir, StatusFmt::Table),
            0,
            "実行中あり = 終了コード 0"
        );
        assert_eq!(run_status_list(&dir, StatusFmt::Json), 0);
    }

    #[test]
    fn help_lists_status_detection() {
        let help = help_text();
        assert!(help.contains("zai status --json"));
        assert!(help.contains("実行検知"));
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

    // ── 終了コードの約束 ──────────────────────────────────────────

    #[test]
    fn exit_codes_are_stable_per_error_kind() {
        assert_eq!(CliError::Usage("x".into()).code(), 2);
        assert_eq!(CliError::Runtime("x".into()).code(), 1);
        assert_eq!(CliError::Usage("使い方".into()).message(), "使い方");
        assert_eq!(finish(Ok(String::new())), 0);
        assert_eq!(finish(Ok("out".into())), 0);
        assert_eq!(finish(Err(CliError::Usage("u".into()))), EXIT_USAGE);
        assert_eq!(finish(Err(CliError::Runtime("r".into()))), EXIT_RUNTIME);
    }

    /// 引数不正は 2、実行時エラーは 1 — サブコマンドを跨いで一貫していること。
    #[test]
    fn argument_errors_map_to_exit_two() {
        let cases: Vec<(&str, Vec<String>)> = vec![
            ("worktree", v(&[])),
            ("worktree", v(&["frobnicate"])),
            ("worktree", v(&["create"])),
            ("worktree", v(&["create", "--from"])),
            ("worktree", v(&["create", "-x"])),
            ("worktree", v(&["create", "a b"])),
            ("worktree", v(&["create", "ok", "extra"])),
            ("worktree", v(&["list", "extra"])),
            ("worktree", v(&["remove"])),
            ("session", v(&[])),
            ("session", v(&["frobnicate"])),
            ("session", v(&["list", "extra"])),
            ("session", v(&["send"])),
            ("session", v(&["send", "abc", "hi"])),
            ("session", v(&["send", "-1", "hi"])),
            ("session", v(&["send", "7"])),
            ("session", v(&["send", "7", "   "])),
            ("session", v(&["log"])),
            ("session", v(&["log", "7", "--tail", "x"])),
            ("session", v(&["log", "7", "extra"])),
            ("agent", v(&[])),
            ("agent", v(&["frobnicate"])),
            ("agent", v(&["list", "extra"])),
        ];
        for (cmd, args) in cases {
            let r = match cmd {
                "worktree" => worktree_dispatch(&args),
                "session" => session_dispatch(&args),
                _ => agent_dispatch(&args),
            };
            match r {
                Err(CliError::Usage(_)) => {}
                other => panic!("zai {cmd} {args:?} は引数エラー (2) であるべき: {other:?}"),
            }
        }
    }

    // ── worktree: パス導出とパーサ ────────────────────────────────

    #[test]
    fn worktrees_base_is_derived_from_repo_root() {
        // 絶対パスの直書きをしない — どの環境でもルートからの相対で決まる
        let root = std::env::temp_dir().join("some-repo");
        assert_eq!(
            worktrees_base(&root),
            root.join(".claude").join("worktrees")
        );
    }

    #[test]
    fn worktree_dir_name_is_filesystem_safe() {
        for (branch, want) in [
            ("feature", "feature"),
            ("feat/login", "feat-login"),
            ("feat//login", "feat-login"),
            ("night/2026-08-02", "night-2026-08-02"),
            ("../../etc/passwd", "etc-passwd"),
            ("..", ""),
            ("...", ""),
            ("/", ""),
            ("release.v1.2", "release-v1-2"),
            ("日本語/ブランチ", "日本語-ブランチ"),
            ("-lead-and-trail-", "lead-and-trail"),
        ] {
            let got = worktree_dir_name(branch);
            assert_eq!(got, want, "branch={branch}");
            // 生成名はパス区切りも親参照も含まない (脱出できない)
            assert!(!got.contains('/') && !got.contains('\\') && !got.contains(".."));
        }
    }

    #[test]
    fn validate_branch_rejects_option_like_and_blank() {
        assert!(validate_branch("ok/name").is_ok());
        assert!(validate_branch("日本語").is_ok());
        for bad in ["", "--json", "-b", "with space", "tab\there"] {
            assert!(
                matches!(validate_branch(bad), Err(CliError::Usage(_))),
                "{bad:?} は拒否されるべき"
            );
        }
    }

    /// 実際の `git worktree list --porcelain` の形をそのまま食わせる。
    /// Windows のチェックアウトを想定して CRLF 版も同じ結果になること。
    #[test]
    fn parse_worktree_porcelain_table() {
        let text = "\
worktree /repo
HEAD 1111111111111111111111111111111111111111
branch refs/heads/main

worktree /repo/.claude/worktrees/feat-login
HEAD 2222222222222222222222222222222222222222
branch refs/heads/feat/login

worktree /repo/detached-one
HEAD 3333333333333333333333333333333333333333
detached

worktree /repo/locked-one
HEAD 4444444444444444444444444444444444444444
branch refs/heads/old
locked reason text
prunable gitdir file points to non-existent location
";
        for src in [text.to_string(), text.replace('\n', "\r\n")] {
            let rows = parse_worktree_porcelain(&src);
            assert_eq!(rows.len(), 4, "{rows:?}");
            assert_eq!(rows[0].branch, "main");
            assert_eq!(rows[0].path, "/repo");
            assert_eq!(rows[1].branch, "feat/login", "refs/heads/ が落ちる");
            assert!(rows[2].detached && rows[2].branch.is_empty());
            assert!(rows[3].locked && rows[3].prunable);
            assert!(!rows[0].locked && !rows[0].prunable && !rows[0].detached);
        }
        // 空・ゴミ入力でも落ちない
        assert!(parse_worktree_porcelain("").is_empty());
        assert!(parse_worktree_porcelain("HEAD abc\nbranch refs/heads/x\n").is_empty());
    }

    #[test]
    fn find_worktree_by_branch_or_directory_name() {
        let rows = parse_worktree_porcelain(
            "worktree /repo\nHEAD a\nbranch refs/heads/main\n\n\
             worktree /repo/.claude/worktrees/feat-login\nHEAD b\nbranch refs/heads/feat/login\n",
        );
        assert_eq!(find_worktree(&rows, "main").unwrap().path, "/repo");
        // ブランチ名でもフォルダ名でも同じものを引ける
        for key in ["feat/login", "feat-login"] {
            assert_eq!(
                find_worktree(&rows, key).unwrap().path,
                "/repo/.claude/worktrees/feat-login",
                "key={key}"
            );
        }
        assert!(find_worktree(&rows, "nope").is_none());
        assert!(find_worktree(&[], "main").is_none());
    }

    #[test]
    fn render_worktrees_json_and_lines() {
        let rows = parse_worktree_porcelain(
            "worktree /repo\nHEAD 1234567890abcdef\nbranch refs/heads/main\n\n\
             worktree /repo/wt\nHEAD abcdef1234567890\ndetached\nlocked\n",
        );
        let lines = render_worktrees(&rows, false);
        let split: Vec<&str> = lines.lines().collect();
        assert_eq!(split.len(), 2, "1 行 1 レコード");
        assert_eq!(split[0], "main\t1234567\tok\t/repo");
        assert_eq!(split[1], "(detached)\tabcdef1\tlocked\t/repo/wt");
        for l in &split {
            assert_eq!(l.split('\t').count(), 4, "列数が揃っている: {l}");
        }
        // JSON は配列で、空でも壊れない
        let json: serde_json::Value =
            serde_json::from_str(&render_worktrees(&rows, true)).expect("JSON");
        assert_eq!(json.as_array().map(|a| a.len()), Some(2));
        assert_eq!(json[0]["branch"], "main");
        assert_eq!(render_worktrees(&[], true), "[]");
        assert_eq!(render_worktrees(&[], false), "worktree はありません。");
    }

    // ── session: ID 抽出・整形・末尾行 ────────────────────────────

    #[test]
    fn session_id_from_log_table() {
        for (log, want) in [
            ("/logs/Claude_Code-1.log", "1"),
            ("/logs/Codex__2-42.log", "42"),
            ("Claude-Code-7.log", "7"),
            ("/logs/no-number.log", ""),
            ("/logs/trailing-.log", ""),
            ("", ""),
            ("/logs/plain.log", ""),
        ] {
            assert_eq!(session_id_from_log(log), want, "log={log}");
        }
        // Windows 形式のパスでもファイル名部分だけを見る
        assert_eq!(session_id_from_log(r"C:\logs\Claude_Code-9.log"), "9");
    }

    #[test]
    fn dedup_and_sort_sessions_keeps_newest_per_id() {
        let row = |id: &str, updated: u64, state: &str| SessionRow {
            id: id.into(),
            agent: "Claude Code".into(),
            workspace: "/ws".into(),
            state: state.into(),
            updated,
            log: String::new(),
        };
        let got = dedup_and_sort_sessions(vec![
            row("1", 100, "stored"),
            row("2", 300, "stored"),
            // マルチルート保存で二重になった同じセッション (古い方が消える)
            row("1", 200, "stored"),
        ]);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "2");
        assert_eq!((got[1].id.as_str(), got[1].updated), ("1", 200));
        assert!(dedup_and_sort_sessions(Vec::new()).is_empty());
    }

    #[test]
    fn parse_live_sessions_from_state_json() {
        let json = r#"{"ok":true,"agents":[
            {"id":1,"title":"Claude Code","running":true},
            {"id":2,"title":"Codex #2","running":false},
            {"title":"壊れた行 (id なし)"}
        ]}"#;
        let live = parse_live_sessions(json);
        assert_eq!(live.len(), 2, "id の無い要素は捨てる");
        assert_eq!(
            live[0],
            LiveSession {
                id: "1".into(),
                title: "Claude Code".into(),
                running: true
            }
        );
        assert!(!live[1].running);
        // 壊れた入力・agents 無しでも空を返す (panic しない)
        assert!(parse_live_sessions("{").is_empty());
        assert!(parse_live_sessions(r#"{"ok":true}"#).is_empty());
    }

    #[test]
    fn merge_live_sessions_updates_state_and_adds_unknown() {
        let stored = vec![SessionRow {
            id: "1".into(),
            agent: "Claude Code".into(),
            workspace: "/ws".into(),
            state: "stored".into(),
            updated: 500,
            log: "/logs/Claude_Code-1.log".into(),
        }];
        let live = vec![
            LiveSession {
                id: "1".into(),
                title: "Claude Code".into(),
                running: false,
            },
            // 起動直後でまだ保存されていないセッションも一覧へ出す
            LiveSession {
                id: "9".into(),
                title: "Codex".into(),
                running: true,
            },
        ];
        let got = merge_live_sessions(stored, &live, "/ws");
        assert_eq!(got.len(), 2);
        let by = |id: &str| got.iter().find(|r| r.id == id).cloned().expect(id);
        assert_eq!(by("1").state, "exited", "終了済みは exited");
        assert_eq!(by("1").log, "/logs/Claude_Code-1.log", "記録側の情報は残る");
        assert_eq!(by("9").state, "running");
        assert_eq!(by("9").workspace, "/ws");
        // 実行中インスタンスが無ければ (live が空) 記録のまま
        let stored_only = merge_live_sessions(vec![by("1")], &[], "/ws");
        assert_eq!(stored_only[0].state, "exited");
    }

    #[test]
    fn render_sessions_json_and_lines() {
        let rows = vec![
            SessionRow {
                id: "2".into(),
                agent: "Codex".into(),
                workspace: "/ws/sub".into(),
                state: "running".into(),
                updated: 900,
                log: "/logs/Codex-2.log".into(),
            },
            SessionRow {
                id: "1".into(),
                agent: "Claude Code".into(),
                workspace: "/ws".into(),
                state: "stored".into(),
                updated: 800,
                log: "/logs/Claude_Code-1.log".into(),
            },
        ];
        let lines = render_sessions(&rows, false);
        assert_eq!(lines.lines().count(), 2);
        assert_eq!(
            lines.lines().next(),
            Some("2\trunning\tCodex\t900\t/ws/sub")
        );
        for l in lines.lines() {
            assert_eq!(l.split('\t').count(), 5, "列数が揃っている: {l}");
        }
        let json: serde_json::Value =
            serde_json::from_str(&render_sessions(&rows, true)).expect("JSON");
        assert_eq!(json[1]["id"], "1");
        assert_eq!(json[0]["state"], "running");
        // 空リスト
        assert_eq!(render_sessions(&[], true), "[]");
        assert_eq!(
            render_sessions(&[], false),
            "記録されているセッションはありません。"
        );
    }

    #[test]
    fn tail_lines_table() {
        let text = "a\nb\nc\nd\n";
        assert_eq!(tail_lines(text, 2), "c\nd");
        assert_eq!(tail_lines(text, 4), "a\nb\nc\nd");
        assert_eq!(tail_lines(text, 99), "a\nb\nc\nd", "行数より大きくても全部");
        assert_eq!(tail_lines(text, 0), "");
        assert_eq!(tail_lines("", 5), "");
        // CRLF の行末は行として割れる (PTY 生ログは \r\n)
        assert_eq!(tail_lines("a\r\nb\r\n", 1), "b");
    }

    #[test]
    fn normalize_log_lines_treats_bare_cr_as_a_record_break() {
        // `\r` 単独の再描画も 1 レコードに割る (パイプの先で行数が数えられる)
        assert_eq!(normalize_log_lines("a\rb\rc"), "a\nb\nc");
        assert_eq!(normalize_log_lines("a\r\nb"), "a\nb");
        assert_eq!(normalize_log_lines("plain"), "plain");
        assert_eq!(normalize_log_lines(""), "");
        // 実際の並び: プロンプト再描画が末尾に固まっているログ
        let raw = "done\r\n(END)\r(END)\r$ \r$ ";
        assert_eq!(tail_lines(&normalize_log_lines(raw), 2), "$ \n$ ");
        // 出力に生の `\r` を残さない (端末を上書きで壊さない)
        assert!(!normalize_log_lines(raw).contains('\r'));
    }

    /// 記録の走査は実 `~/.zaivern` に触れず、壊れたファイルで落ちない。
    #[test]
    fn collect_session_rows_reads_toml_and_skips_garbage() {
        let dir = crate::test_util::unique_temp_dir("zaivern-cli-test", "sessions");
        // 空ディレクトリ・存在しないディレクトリ → 空
        assert!(collect_session_rows(&dir).is_empty());
        assert!(collect_session_rows(&dir.join("ghost")).is_empty());

        let data = crate::session::SessionData {
            agents: vec![
                crate::session::AgentSessionRec {
                    preset_name: "Claude Code".into(),
                    title: "Claude Code".into(),
                    cwd: "/ws".into(),
                    log_file: dir.join("Claude_Code-1.log").to_string_lossy().into_owned(),
                    ..Default::default()
                },
                // ID を取れない記録は一覧に出さない (send / log の対象にできない)
                crate::session::AgentSessionRec {
                    preset_name: "Codex".into(),
                    log_file: dir.join("no-id.log").to_string_lossy().into_owned(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        std::fs::write(
            dir.join("aaaa.toml"),
            toml::to_string_pretty(&data).expect("serialize"),
        )
        .expect("write");
        std::fs::write(dir.join("broken.toml"), "これは TOML ではない [[[").expect("write");
        std::fs::write(dir.join("ignored.txt"), "not toml").expect("write");

        let rows = collect_session_rows(&dir);
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].id, "1");
        assert_eq!(rows[0].agent, "Claude Code");
        assert_eq!(rows[0].workspace, "/ws");
        assert_eq!(rows[0].state, "stored");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 実行中インスタンスが無ければ `send` は**接続を試みて失敗するだけ**。
    /// kill は撃たない (このテストはプロセスを一切起こさない)。
    #[test]
    fn session_send_without_instance_is_runtime_error() {
        // instance.json が無い環境では実行時エラー (2 ではなく 1)
        if read_instance_file().is_none() {
            match session_dispatch(&v(&["send", "1", "hello"])) {
                Err(CliError::Runtime(_)) => {}
                other => panic!("実行時エラーであるべき: {other:?}"),
            }
        }
    }

    #[test]
    fn check_api_ok_maps_response_to_exit_code() {
        assert!(check_api_ok(r#"{"ok":true,"sent":1}"#).is_ok());
        // 終了済みセッションは 200 + ok:false で返る — ここを 0 にしない
        let err = check_api_ok(r#"{"ok":false,"error":"セッションが停止しています"}"#)
            .expect_err("失敗であるべき");
        assert_eq!(err.code(), EXIT_RUNTIME);
        assert_eq!(err.message(), "セッションが停止しています");
        assert_eq!(
            check_api_ok(r#"{"ok":false}"#).expect_err("失敗").message(),
            "失敗しました"
        );
        assert!(matches!(
            check_api_ok("not json"),
            Err(CliError::Runtime(_))
        ));
    }

    // ── agent: カタログ + PATH 探索 ───────────────────────────────

    #[test]
    fn agent_rows_reflect_catalog_and_probe() {
        // 探索を差し替えるので、実際の PATH に何が入っていても結果が決まる
        let found_dir = std::env::temp_dir().join("zaivern-fake-bin");
        let rows = agent_rows(|bin| (bin == "claude").then(|| found_dir.join(bin)));
        assert_eq!(
            rows.len(),
            crate::agents::AGENT_CATALOG.len(),
            "カタログを 1 件も落とさない"
        );
        let claude = rows.iter().find(|r| r.bin == "claude").expect("claude");
        assert!(claude.installed);
        assert_eq!(claude.label, "Claude Code");
        assert_eq!(claude.command, "claude");
        assert_eq!(claude.path, found_dir.join("claude").to_string_lossy());
        let others: Vec<&AgentRow> = rows.iter().filter(|r| r.bin != "claude").collect();
        assert!(others.iter().all(|r| !r.installed && r.path.is_empty()));
        assert!(
            others.iter().any(|r| !r.install_hint.is_empty()),
            "未導入には導入方法を添える"
        );
        // 何も見つからない環境でも全件出る (0 件の一覧にはしない)
        assert_eq!(agent_rows(|_| None).len(), rows.len());
    }

    #[test]
    fn render_agents_json_and_lines() {
        let rows = agent_rows(|bin| {
            (bin == "claude").then(|| std::env::temp_dir().join("bin").join("claude"))
        });
        let lines = render_agents(&rows, false);
        assert_eq!(lines.lines().count(), rows.len());
        for l in lines.lines() {
            assert_eq!(l.split('\t').count(), 5, "列数が揃っている: {l}");
        }
        assert!(lines.lines().any(|l| l.starts_with("claude\tinstalled\t")));
        assert!(lines.lines().any(|l| l.contains("\tmissing\t")));
        let json: serde_json::Value =
            serde_json::from_str(&render_agents(&rows, true)).expect("JSON");
        assert_eq!(json.as_array().map(|a| a.len()), Some(rows.len()));
        assert_eq!(json[0]["bin"], "claude");
        assert_eq!(json[0]["installed"], true);
        assert_eq!(render_agents(&[], true), "[]");
        assert_eq!(render_agents(&[], false), "対応エージェントがありません。");
    }

    /// `zai agent list` は実 PATH を見るだけ (プロセスを起こさない)。
    #[test]
    fn agent_list_succeeds_and_is_one_record_per_line() {
        let out = agent_dispatch(&v(&["list"])).expect("agent list は常に成功する");
        assert_eq!(out.lines().count(), crate::agents::AGENT_CATALOG.len());
        let json = agent_dispatch(&v(&["list", "--json"])).expect("--json");
        assert!(serde_json::from_str::<serde_json::Value>(&json).is_ok());
    }

    /// `zai session` / `zai agent` / `zai worktree` は、同名のフォルダが
    /// 実在するときだけ GUI 起動 (ワークスペース指定) に譲る。
    #[test]
    fn bare_subcommand_words_yield_to_existing_directories() {
        for w in ["app", "firewall", "worktree", "session", "agent", "help"] {
            assert!(yields_to_directory(w), "{w} は譲るべき");
        }
        for w in ["open", "prompt", "run", "state", "plugin", "status"] {
            assert!(!yields_to_directory(w), "{w} は譲らない");
        }
    }
}
