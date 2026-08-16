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
            // UI 表示言語の確認と、翻訳ファイルの検査 (コミュニティ翻訳者の入口)
            | "i18n"
            // 言語パックの導入・切替 (GitHub から取ってくる)
            | "lang"
            | "app"
            | "firewall"
            | "worktree"
            | "session"
            | "agent"
            | "lease"
            | "hook"
            // ベンダー非依存の書き込み強制 (git フックがここを呼ぶ)
            | "guard"
            // 順次統合 (マージトレイン) と、配る前の担当分割
            | "train"
            | "split"
            // 衝突ゼロ証明と一撃統合。`czero` パネルがここを叩いて段を上げる
            // ので、**登録し忘れると `zai` が未知語をワークスペース指定と
            // 解釈して GUI の窓が生える** (実測で発見された罠)。
            | "coedit"
            // Erlang 風プロセスメッシュ (エージェント同士が裏で認識し合う層)
            | "mesh"
            // 断らずにずらす交渉層 (拒否された担当を近くの空き行域へ振り替える)
            | "negotiate"
            // どのリポジトリでも 1 コマンドで導入・診断・実証・撤去する入口。
            // **門に足し忘れると `zai czero doctor` が GUI の窓を開く**
            | "czero"
            // git が custom merge driver として起動する入口 (人が打つものではない)
            | "merge-driver"
            | "update"
            | "uninstall"
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
        "app"
            | "firewall"
            | "worktree"
            | "session"
            | "agent"
            | "lease"
            | "hook"
            | "guard"
            | "train"
            | "split"
            | "help"
    )
}

/// ヘルプ本文。`zai help` / `zai --help` はこれを丸ごと出し、
/// `zai <cmd> --help` は該当セクションだけを出す。
/// **セクションの実体は 1 箇所** — 全体ヘルプと個別ヘルプが食い違わない。
pub fn help_text() -> String {
    format!(
        "{HELP_HEAD}{HELP_WORKTREE}{HELP_SESSION}{HELP_AGENT}{HELP_LEASE}{HELP_GUARD}\n\
         {HELP_CZERO}{HELP_TRAIN_SPLIT}{HELP_UPDATE}{HELP_UNINSTALL}{HELP_TAIL}"
    )
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

表示言語 (Language Pack):
  zai lang                              いまの言語と、選べる言語の一覧
  zai lang list [--remote]              --remote で配布元にある言語も出す
  zai lang install <id> [--from owner/repo] [--ref <branch>] [--force]
                                        GitHub から取って ~/.zaivern/locales へ入れる
  zai lang remove <id>                  入れた言語ファイルを消す (同梱は消さない)
  zai lang set <id>|auto                表示言語を切り替える (config.toml へ保存)
  zai lang check [<id>|<file.json>]     翻訳ファイルの過不足を検査 (合わなければ終了コード 1)
  zai lang export <id> [<file.json>]    翻訳の雛形を書き出す (新しい言語はここから)
  ※ `zai i18n …` は同じものの別名です

保守 (このリポジトリで開発するとき):
  zai lang missing [<srcディレクトリ>] [<out.json>]
                                        画面に出るのに辞書へ無い文字列を出す
  zai lang apply <shard.json> [<localesディレクトリ>]
                                        訳を locales/*.json へ取り込む

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

/// `zai lease --help` のセクション。
pub const HELP_LEASE: &str = "\
lease (ファイル所有 — 並列エージェントの衝突を「起こさせない」):
  zai lease status                      いまの段 (強制 / 勧告 / 無効) と台帳の場所
  zai lease enable                      このリポジトリで有効にする
                                        (以後、書き込みの所有が記録されます)
  zai lease disable                     無効にする (台帳を消します)
  zai lease list [--json]               確保中のファイルと持ち主
  zai lease claim [--shift] <パターン...> [--agent 名前]
                                        自分のものとして確保する (glob 可)
                                        重なっていたら拒否されます (後勝ちにしません)
                                        --shift を付けると、埋まっていたら**同じ幅が
                                        入るいちばん近い空き行域へずらして**取ります
                                        (最後の行に granted <確保した仕様> を出します)
  zai lease release [--agent 名前 | --all]
                                        確保を手放す (引き継ぐとき)
  スコープは git の**元のリポジトリ**です。worktree は同じ台帳を共有します
  (worktree ごとに分けると、同じファイルを 2 人が編集する事故を防げません)

";

/// `zai guard --help` のセクション。
///
/// **本文は [`crate::guard`] 側の唯一の出所を指すだけ**。ここへ写経すると
/// `zai guard --help` と `zai --help` が食い違う (それを戒めるテストが下にある)。
pub const HELP_GUARD: &str = crate::features::guard::HELP;

/// 競合ゼロの導入・証明・プロセスメッシュ・交渉。
///
/// **本文はここに 1 行ずつの索引だけ置く。** 詳しい使い方は各サブコマンドの
/// `--help` がそれぞれの実体から出す (写経すると必ず食い違う。`HELP_GUARD` と同じ方針)。
///
/// ここが空だった間、`czero` / `coedit` / `mesh` / `negotiate` の 4 つは
/// **動くのに `zai help` に 1 文字も出ていなかった** — 導入コマンドが CLI から
/// 発見できないという、いちばん惜しい壊れ方をしていた。番人は
/// [`全サブコマンドがヘルプに出ている`]。
pub const HELP_CZERO: &str = "\
競合ゼロ (並列で走らせても同じ行を 2 人に配らない仕組み):
  zai czero init                        このリポジトリへ導入する (フック / 追記の自動マージ)
  zai czero doctor                      いま何段まで効いているかを診断する
  zai czero prove                       効いていることを実測で示す
  zai czero uninstall                   導入したものを外す
  zai coedit proof                      配る前に「後で一撃で統合できる」ことを証明する
  zai coedit regions                    いま誰がどの行域を持っているかを出す
  zai mesh spawn|list|register          エージェント同士が互いを監視するメッシュ
                                        (Erlang のプロセスリンク相当。落ちたら気付ける)
  zai negotiate offer|allocate|deal     行域がぶつかったとき、断らずに「ずらす」
  詳しい使い方は zai czero --help / zai coedit --help /
                 zai mesh --help / zai negotiate --help

";

/// 順次統合 (マージトレイン) と、配る前の担当分割、そして git マージドライバ。
///
/// **本文はここに 1 行ずつの索引だけ置く。** 詳しい使い方は
/// `zai train --help` / `zai split --help` がそれぞれの実体から出す
/// (写経すると必ず食い違う。`HELP_GUARD` と同じ方針)。
pub const HELP_TRAIN_SPLIT: &str = "\
統合 (並列で走らせた成果を、衝突ゼロで1本にまとめます):
  zai split plan --tasks <ファイル|->   配る前に、互いに素な担当表を作る
                                        (終了コード: 0=互いに素 / 1=共有パスが残った)
  zai train plan [--onto <ブランチ>]    統合の順序と、予想される衝突を出す
  zai train run [--onto <ブランチ>] [--dry-run]
                                        重なりの少ない順に自動リベースして統合する
                                        衝突したら全部戻す (部分統合を残しません)
  zai merge-driver ...                  git が custom merge driver として起動する入口
                                        (人が打つものではありません。導入はパレットの
                                         「追記の自動マージ」から)
  詳しい使い方は zai train --help / zai split --help

";

/// `zai update --help` のセクション。
pub const HELP_UPDATE: &str = "\
update (Zaivern Code 自身を更新します — エディタが起動していなくても使えます):
  zai update                            最新版を確認し、実行するコマンドを見せてから更新
  zai update --check                    最新かどうかを確認するだけ (何も実行しません)
  zai update --yes | -y                 確認を求めずに更新する
                                        更新手段は入っている場所で自動的に選ばれます
                                        (~/.cargo/bin なら cargo、それ以外はインストーラ)

";

/// `zai uninstall --help` のセクション。
pub const HELP_UNINSTALL: &str = "\
uninstall (Zaivern Code を消します — 消す前に必ず一覧と合計サイズを出します):
  zai uninstall                         消すものを一覧表示して確認を求める
  zai uninstall --dry-run               一覧を出すだけ (何も消しません)
  zai uninstall --keep-config           設定 (config.toml / state.toml) は残す
  zai uninstall --yes | -y              確認を求めずに削除する
                                        消すのは実行ファイル本体と ~/.zaivern だけです

";

const HELP_TAIL: &str = "\
ベンダーフック (エージェント CLI から自動的に呼ばれます — 手で打つものではありません):
  zai hook --zaivern <エージェント> <イベント>
                                        フック通知を受け取る (標準入力の JSON を投函)

その他:
  zai help                              このヘルプ
  zai <サブコマンド> --help             そのサブコマンドの使い方だけを表示
  zai --help | -h                       このヘルプ
  zai --version | -V                    バージョン

終了コード: 0 = 成功 / 1 = 実行時エラー / 2 = 引数の指定ミス
";

/// CLI 経路の表示言語を決める。
///
/// GUI と同じ規則 (`config.toml` の `ui_language` → OS の判定 → 原文言語) で
/// 引くが、**原文言語 (日本語) のままなら何も読まない** — `zai` は短命な
/// プロセスなので、日本語で使う人に辞書の読み込み費用を払わせない。
pub fn init_cli_locale() {
    let choice = crate::config::ui_language_pref();
    let known: Vec<String> = crate::locale::available(&[])
        .into_iter()
        .map(|i| i.id)
        .collect();
    let id = crate::locale::resolve(
        &choice,
        crate::locale::detected().as_deref(),
        &known,
        crate::locale::SOURCE_LANG,
    );
    if id == crate::locale::SOURCE_LANG {
        return;
    }
    // 読めない辞書があっても CLI は止めない (同梱ぶんで動く)。
    let _ = crate::i18n::set_locale(&id, &[]);
}

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
        // git フック (pre-commit 等) と CI がここを呼ぶ。実体は src/guard.rs。
        "guard" => crate::features::guard::cli_main(rest),
        // 順次統合。実体は src/train.rs。
        "train" => crate::features::train::cli_main(rest),
        // 配る前の担当分割。実体は src/split.rs。
        "split" => crate::features::split::cli_main(rest),
        // 衝突ゼロ証明と一撃統合。実体は src/coedit.rs。
        "coedit" => crate::features::coedit::cli_main(rest),
        // Erlang 風プロセスメッシュ。実体は src/mesh.rs。
        "mesh" => crate::features::mesh::cli_main(rest),
        // 行域の交渉 (ずらす / 分割する / 待つ)。実体は src/negotiate.rs。
        "negotiate" => crate::features::negotiate::cli_main(rest),
        // 競合ゼロの導入・診断・実証・撤去。実体は src/czero_init.rs。
        "czero" => {
            // lease と同じ理由で、czero の置き場も旧キーから引き取ってから触る。
            if let Ok(cwd) = std::env::current_dir() {
                crate::history::adopt_legacy_keys(&cwd);
            }
            crate::features::czero_init::cli_main(rest)
        }
        // git が `%O %A %B %L %P` を付けて起動する。実体は src/union.rs。
        "merge-driver" => crate::features::union::cli_main(rest),
        // `zai status` (引数なし / --json のみ) はレジスタリ一覧 = 実行検知。
        // テキスト付きは従来どおりステータスバー更新 (下の run_remote へ落ちる)。
        "status" if status_list_mode(rest).is_some() => run_status_list(
            &crate::instances::instances_dir(),
            status_list_mode(rest).unwrap_or(StatusFmt::Table),
        ),
        "plugin" => run_plugin(rest),
        "i18n" | "lang" => run_lang(rest),
        "app" => crate::desktop::run(rest),
        "firewall" => crate::firewall::run(rest),
        // ヘッドレス導線 (実行中インスタンスが無くても動くものが大半)
        "worktree" => finish(worktree_dispatch(rest)),
        "session" => finish(session_dispatch(rest)),
        "agent" => finish(agent_dispatch(rest)),
        // ファイル所有リース (GUI が無くても設定・確認できる導線)
        "lease" => finish(lease_dispatch(rest)),
        // 自分自身の面倒 (更新・削除)。どちらもエディタの起動を要求しない。
        "update" => finish(update_dispatch(rest)),
        "uninstall" => finish(uninstall_dispatch(rest)),
        // ベンダー提供フックの受け口 (状態ラダー 2 段目)。
        // ベンダー CLI がこれを呼ぶ。GUI が居なくても成功して構わない
        // (投函箱に置くだけ — GUI は次のサンプリングで拾う)。
        "hook" => run_hook(rest),
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

// ───────────────────────── hook: ベンダーフックの受け口 ─────────────────────────

/// `zai hook --zaivern <agent> <event>` — ベンダー CLI のフックから呼ばれる。
///
/// 2 つの仕事をする:
/// 1. 標準入力の JSON ペイロードを `~/.zaivern/hooks/` へ 1 ファイル投函する
///    (状態ラダー 2 段目。**ポートは開かない** — remote.rs の 8899〜 と
///    衝突させない)。GUI が動いていなくても失敗しない。
/// 2. **ファイル所有リースの強制** ([`crate::lease::gate`])。書き込み系ツールが
///    他人の持つパスへ向いていたら、ベンダーの許可判断へ `deny` を返して
///    ツール呼び出しそのものを止める。この分岐が「衝突を後で発見させない」の
///    実効部で、GUI が居なくても効く。
///
/// フック自身のエラーでベンダー CLI を妨げないこと (fail-open) は
/// [`crate::lease::gate`] 側の責務。
///
/// 引数の並びは [`crate::supervisor::hooks::HOOK_MARK`] が作る形と対。
fn run_hook(args: &[String]) -> i32 {
    use crate::supervisor::hooks;
    // "--zaivern" は「これは Zaivern が仕掛けたフックだ」という目印
    // (設置/解除で自分の項目だけを見分けるために要る)。
    let rest: Vec<&str> = args
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with("--"))
        .collect();
    let agent = rest.first().copied().unwrap_or_default();
    let event = rest.get(1).copied().unwrap_or_default();
    let mut payload = String::new();
    let _ = std::io::stdin().read_to_string(&mut payload);
    let ev = hooks::event_from_payload(agent, event, &payload);
    let _ = hooks::post(&hooks::inbox_dir(), &ev);
    // 投函のあとで判断する (投函は状態表示、判断は強制で、目的が別)。
    let answer = crate::lease::gate(agent, event, &payload);
    if !answer.stdout.is_empty() {
        println!("{}", answer.stdout);
    }
    if !answer.stderr.is_empty() {
        eprintln!("{}", answer.stderr);
    }
    answer.exit
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

/// **位置引数を取るサブコマンドで、知らない旗を弾く。**
///
/// [`reject_extra`] は「位置引数を 1 つも取らない」サブコマンド用なので、
/// `zai lease claim <パターン...>` のように位置引数が可変長のものには使えない。
/// 使えないからと素通しにしていたため、`zai lease claim a.rs --zzzz` が
/// **rc=0 で `--zzzz` という名前のファイルを確保していた** (実バイナリで再現)。
/// 打ち間違えた旗が黙って担当表に載るのは、**効いていると思わせて
/// 効いていない**いちばん危ない壊れ方。
///
/// `--` 以降は旗として解釈しない (`-` で始まる実在のファイルを指すため)。
fn reject_unknown_flags(rest: &[String], usage: &str) -> Result<Vec<String>, CliError> {
    let mut out = Vec::with_capacity(rest.len());
    let mut literal = false;
    for a in rest {
        if literal {
            out.push(a.clone());
            continue;
        }
        if a == "--" {
            literal = true;
            continue;
        }
        // `-` 単体はファイル名や標準入力の意味で使われ得るので通す。
        if a.starts_with('-') && a.len() > 1 {
            return Err(CliError::Usage(format!(
                "不明なオプションです: {a} — 使い方: {usage}\n\
                 (`-` で始まるファイルを指すなら `--` の後ろに置いてください)"
            )));
        }
        out.push(a.clone());
    }
    Ok(out)
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

// ───────────────────────── i18n サブコマンド (インスタンス不要) ─────────────────────────

/// `zai i18n` — 表示言語の確認と、翻訳ファイルの検査・雛形出力。
///
/// **コミュニティが言語を足すための入口**。GUI を起動しなくても、
/// `~/.zaivern/locales/fr.json` が同梱 `en.json` と噛み合っているかを
/// ここだけで確かめられる (噛み合っていなければ終了コード 1 = fail-closed)。
fn run_lang(args: &[String]) -> i32 {
    // フラグと位置引数を分ける (`--from owner/repo` はここで拾う)
    let mut positional: Vec<String> = Vec::new();
    let mut from: Option<String> = None;
    let mut git_ref: Option<String> = None;
    let mut force = false;
    let mut remote = false;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--from" => from = it.next().cloned(),
            // 既定ブランチ以外から取る (公開前の下見・自分のフォークの検証用)
            "--ref" | "--branch" => git_ref = it.next().cloned(),
            "--force" | "-f" => force = true,
            "--remote" => remote = true,
            other => positional.push(other.to_string()),
        }
    }
    let sub = positional.first().map(|s| s.as_str()).unwrap_or("");
    let arg = positional.get(1).cloned().unwrap_or_default();
    let result = match sub {
        "" | "list" | "status" => lang_list(remote, from.as_deref(), git_ref.as_deref()),
        "install" | "add" => lang_install(&arg, from.as_deref(), git_ref.as_deref(), force),
        "remove" | "uninstall" | "rm" => lang_remove(&arg),
        "set" | "use" => lang_set(&arg),
        "check" => i18n_check(&arg),
        "export" => i18n_export(&arg, positional.get(2).map(String::as_str)),
        "missing" => i18n_missing(&arg, positional.get(2).map(String::as_str)),
        "apply" => i18n_apply(&arg, positional.get(2).map(String::as_str)),
        other => Err(crate::i18n::trf(
            "不明な lang サブコマンドです: {other} (list / install / remove / set / check / export)",
            &[("other", other.to_string())],
        )),
    };
    match result {
        Ok(out) => {
            if !out.is_empty() {
                println!("{out}");
            }
            0
        }
        Err(e) => {
            eprintln!("{e}");
            1
        }
    }
}

/// 端末の**表示幅**で右へ空白を足す。
///
/// `{:<8}` は**文字数**で数えるので、`日本語` (3 文字 / 6 桁) や `한국어` が
/// 混ざると列がずれる。桁の数え方は端末グリッドと同じ [`crate::textenc::char_width`]
/// (wcwidth) に合わせる。
fn pad_display(s: &str, width: usize) -> String {
    let w: usize = s.chars().map(crate::textenc::char_width).sum();
    let mut out = s.to_string();
    for _ in w..width {
        out.push(' ');
    }
    out
}

/// 言語パックの配布元 (`owner/repo`)。
///
/// **決め打ちしない。** 既定はこのビルドの配布元 (`install.sh` から読む) で、
/// `ZAIVERN_LANG_REPO` か `--from owner/repo` で差し替えられる。
/// こうしておくと、コミュニティが自分のリポジトリで言語パックを配れる
/// (`zai lang install fr --from someone/zaivern-lang-fr`)。
fn lang_repo(from: Option<&str>) -> Result<String, String> {
    if let Some(f) = from.map(str::trim).filter(|s| !s.is_empty()) {
        return validate_slug(f);
    }
    if let Ok(v) = std::env::var("ZAIVERN_LANG_REPO") {
        if !v.trim().is_empty() {
            return validate_slug(v.trim());
        }
    }
    distribution()
        .map(|d| d.slug)
        .map_err(|_| crate::i18n::tr("配布元が分からないので --from owner/repo で指定してください"))
}

/// `owner/repo` の形だけを通す (URL を組む前に必ず確かめる)。
fn validate_slug(s: &str) -> Result<String, String> {
    let parts: Vec<&str> = s.split('/').collect();
    let ok = parts.len() == 2
        && parts.iter().all(|p| {
            !p.is_empty()
                && p.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        });
    if ok {
        Ok(s.to_string())
    } else {
        Err(crate::i18n::trf(
            "配布元は owner/repo の形で指定してください: {s}",
            &[("s", s.to_string())],
        ))
    }
}

/// 配布元の `locales/` に何があるか。`(言語ID, 取得URL)` の一覧。
///
/// **既定ブランチを当てにしない** — GitHub の contents API は既定ブランチを
/// 見てくれるので、`main` / `master` の違いで壊れない。
fn lang_remote_index(slug: &str, git_ref: Option<&str>) -> Result<Vec<(String, String)>, String> {
    let url = match git_ref.map(str::trim).filter(|r| !r.is_empty()) {
        Some(r) => {
            // ブランチ名もそのまま URL へ入るので形を確かめる
            if !r
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "-_./".contains(c))
            {
                return Err(crate::i18n::trf(
                    "ブランチ名に使えない文字が入っています: {r}",
                    &[("r", r.to_string())],
                ));
            }
            format!("https://api.github.com/repos/{slug}/contents/locales?ref={r}")
        }
        None => format!("https://api.github.com/repos/{slug}/contents/locales"),
    };
    let body = fetch_text(&url).map_err(|e| {
        // 404 は「まだ locales/ を置いていない配布元」。原因が分かる案内にする
        // (`Runtime("…")` のような Debug 表記をそのまま人へ見せない)。
        if e.message().contains("404") {
            crate::i18n::trf(
                "{slug} に locales/ がありません (配布元を確かめてください: --from owner/repo)",
                &[("slug", slug.to_string())],
            )
        } else {
            e.message().to_string()
        }
    })?;
    parse_lang_index(&body, slug)
}

/// GitHub contents API の応答から `(言語ID, 取得URL)` を取り出す**純関数**。
///
/// I/O を含まないので表で固定できる。`https` 以外の `download_url` は捨てる
/// (応答が差し替えられても、こちらから平文で取りに行かない)。
fn parse_lang_index(body: &str, slug: &str) -> Result<Vec<(String, String)>, String> {
    let v: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        crate::i18n::trf("配布元の応答を解釈できません: {e}", &[("e", e.to_string())])
    })?;
    let arr = v.as_array().ok_or_else(|| {
        crate::i18n::trf(
            "{slug} に locales/ がありません",
            &[("slug", slug.to_string())],
        )
    })?;
    let mut out: Vec<(String, String)> = arr
        .iter()
        .filter_map(|e| {
            let name = e.get("name")?.as_str()?;
            let dl = e.get("download_url")?.as_str()?;
            if !dl.starts_with("https://") {
                return None;
            }
            let id = name.strip_suffix(".json")?;
            (!id.is_empty()).then(|| (crate::locale::normalize(id), dl.to_string()))
        })
        .collect();
    out.sort();
    out.dedup_by(|a, b| a.0 == b.0);
    Ok(out)
}

/// 言語の一覧。`--remote` を付けると配布元にあるものも並べる。
fn lang_list(remote: bool, from: Option<&str>, git_ref: Option<&str>) -> Result<String, String> {
    let mut out = vec![i18n_status()];
    if !remote {
        out.push(String::new());
        out.push(crate::i18n::tr(
            "配布元にある言語も見るには: zai lang list --remote",
        ));
        return Ok(out.join("\n"));
    }
    let slug = lang_repo(from)?;
    let idx = lang_remote_index(&slug, git_ref)?;
    let here: std::collections::HashSet<String> = crate::locale::available(&[])
        .into_iter()
        .map(|i| i.id)
        .collect();
    out.push(String::new());
    out.push(crate::i18n::trf(
        "配布元 {slug} の言語 ({n} 件):",
        &[("slug", slug), ("n", idx.len().to_string())],
    ));
    for (id, _) in &idx {
        let mark = if here.contains(id) { "✓" } else { "+" };
        out.push(format!(
            "{mark} {} {}",
            pad_display(id, 8),
            crate::locale::display_name(id)
        ));
    }
    out.push(String::new());
    out.push(crate::i18n::tr("入れるには: zai lang install <id>"));
    Ok(out.join("\n"))
}

/// 配布元から言語ファイルを取って `~/.zaivern/locales/` へ入れる。
///
/// **検証してから置く** — 壊れた JSON や、プレースホルダが基準と食い違う訳は
/// 実行時に穴を開けるので、書き出す前に断る (fail-closed)。書き込みは
/// 一時ファイル + rename で、途中で切れても半端なファイルを残さない。
fn lang_install(
    id: &str,
    from: Option<&str>,
    git_ref: Option<&str>,
    force: bool,
) -> Result<String, String> {
    if id.trim().is_empty() {
        return Err(crate::i18n::tr(
            "入れる言語 ID を指定してください (例: zai lang install zh-CN)",
        ));
    }
    let want = crate::locale::normalize(id);
    let slug = lang_repo(from)?;
    let idx = lang_remote_index(&slug, git_ref)?;
    let Some((_, url)) = idx.iter().find(|(i, _)| *i == want) else {
        let have: Vec<&str> = idx.iter().map(|(i, _)| i.as_str()).collect();
        return Err(crate::i18n::trf(
            "{slug} に {id}.json がありません (あるのは: {have})",
            &[("slug", slug), ("id", want), ("have", have.join(" "))],
        ));
    };

    let body = fetch_text(url).map_err(|e| e.message().to_string())?;
    let map = crate::locale::parse_json(&body, &format!("{slug}:locales/{want}.json"))?;
    if map.is_empty() {
        return Err(crate::i18n::trf(
            "{id}.json が空です",
            &[("id", want.clone())],
        ));
    }
    let mut errs = Vec::new();
    let base = crate::locale::load_one(crate::locale::BASE, &[], &mut errs);
    let report = crate::locale::compare(&base, &map);
    if !report.placeholder.is_empty() {
        let head: Vec<&str> = report
            .placeholder
            .iter()
            .take(5)
            .map(|s| s.as_str())
            .collect();
        return Err(crate::i18n::trf(
            "{id}.json はプレースホルダが基準と違うので入れません ({n} 件: {head})",
            &[
                ("id", want.clone()),
                ("n", report.placeholder.len().to_string()),
                ("head", head.join(" ")),
            ],
        ));
    }

    let dir = crate::config::zaivern_dir().join("locales");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作れません: {e}", dir.display()))?;
    let dest = dir.join(format!("{want}.json"));
    if dest.exists() && !force {
        return Err(crate::i18n::trf(
            "{path} は既にあります (上書きするなら --force)",
            &[("path", dest.display().to_string())],
        ));
    }
    let tmp = dir.join(format!(".{want}.json.tmp"));
    std::fs::write(&tmp, &body).map_err(|e| format!("{} を書けません: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &dest).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{} へ置けません: {e}", dest.display())
    })?;

    let mut out = vec![crate::i18n::trf(
        "🌐 {name} ({id}) を入れました — {path} ({n} 件)",
        &[
            ("name", crate::locale::display_name(&want)),
            ("id", want.clone()),
            ("path", dest.display().to_string()),
            ("n", map.len().to_string()),
        ],
    )];
    if !report.missing.is_empty() {
        out.push(crate::i18n::trf(
            "  訳が無い項目が {n} 件あります (そこは英語で出ます)",
            &[("n", report.missing.len().to_string())],
        ));
    }
    out.push(crate::i18n::trf(
        "  使うには: zai lang set {id}",
        &[("id", want)],
    ));
    Ok(out.join("\n"))
}

/// 入れた言語ファイルを消す。**同梱の言語は消せない** (バイナリの中なので)。
fn lang_remove(id: &str) -> Result<String, String> {
    if id.trim().is_empty() {
        return Err(crate::i18n::tr("消す言語 ID を指定してください"));
    }
    let want = crate::locale::normalize(id);
    let mut removed = Vec::new();
    for d in crate::locale::user_dirs() {
        let p = d.join(format!("{want}.json"));
        if p.is_file() {
            std::fs::remove_file(&p).map_err(|e| format!("{} を消せません: {e}", p.display()))?;
            removed.push(p.display().to_string());
        }
    }
    if removed.is_empty() {
        return Err(crate::i18n::trf(
            "{id} は入っていません (同梱の言語はファイルではないので消せません)",
            &[("id", want)],
        ));
    }
    Ok(crate::i18n::trf(
        "🗑 {id} を消しました: {paths}",
        &[("id", want), ("paths", removed.join(" "))],
    ))
}

/// 表示言語を切り替えて `config.toml` へ保存する。
///
/// **入っていない言語は断る** — 書けてしまうと、次の起動で黙って英語になり
/// 「設定したのに効かない」になる。入れ方まで案内する。
fn lang_set(id: &str) -> Result<String, String> {
    if id.trim().is_empty() {
        return Err(crate::i18n::tr(
            "切り替える言語 ID を指定してください (auto も可)",
        ));
    }
    let want = if id.eq_ignore_ascii_case(crate::locale::AUTO) {
        crate::locale::AUTO.to_string()
    } else {
        let n = crate::locale::normalize(id);
        let known: Vec<String> = crate::locale::available(&[])
            .into_iter()
            .map(|i| i.id)
            .collect();
        if !known.contains(&n) {
            return Err(crate::i18n::trf(
                "{id} は入っていません — まず: zai lang install {id}",
                &[("id", n)],
            ));
        }
        n
    };
    let mut v = std::collections::BTreeMap::new();
    v.insert(
        "ui_language".to_string(),
        crate::config::SettingValue::Text(want.clone()).to_toml(),
    );
    crate::config::save_settings(&v)?;
    Ok(crate::i18n::trf(
        "🌐 表示言語を {name} ({id}) にしました (起動中の窓は次のフレームから、では無く再読み込みで反映されます)",
        &[
            ("name", crate::locale::display_name(&want)),
            ("id", want),
        ],
    ))
}

fn i18n_status() -> String {
    let choice = crate::config::ui_language_pref();
    let now = crate::i18n::current();
    let mut out = vec![
        crate::i18n::trf(
            "設定: {choice} / いま使う言語: {now} ({name}){extra}",
            &[
                ("choice", choice),
                ("now", now.clone()),
                ("name", crate::locale::display_name(&now)),
                (
                    "extra",
                    if crate::i18n::active() {
                        String::new()
                    } else {
                        crate::i18n::tr(" — 原文のまま").to_string()
                    },
                ),
            ],
        ),
        String::new(),
    ];
    for info in crate::locale::available(&[]) {
        let mark = if info.id == now { "*" } else { " " };
        let kind = if info.builtin {
            crate::i18n::tr("同梱")
        } else {
            crate::i18n::tr("追加")
        };
        let where_ = info
            .path
            .map(|p| format!("  {}", p.display()))
            .unwrap_or_default();
        out.push(format!(
            "{mark} {} {} {kind}{where_}",
            pad_display(&info.id, 8),
            pad_display(&info.name, 22)
        ));
    }
    out.push(String::new());
    for d in crate::locale::user_dirs() {
        out.push(crate::i18n::trf(
            "言語ファイルの置き場: {dir}",
            &[("dir", d.display().to_string())],
        ));
    }
    out.join("\n")
}

/// 言語 ID かファイルパスを受けて、同梱 `en` と突き合わせる。
fn i18n_check(arg: &str) -> Result<String, String> {
    if arg.trim().is_empty() {
        return Err(crate::i18n::tr(
            "検査する言語 ID かファイルを指定してください (例: zai i18n check fr)",
        ));
    }
    let path = Path::new(arg);
    let (label, map) = if path.is_file() {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("{} を読めません: {e}", path.display()))?;
        (
            path.display().to_string(),
            crate::locale::parse_json(&raw, &path.display().to_string())?,
        )
    } else {
        let id = crate::locale::normalize(arg);
        let mut errs = Vec::new();
        let m = crate::locale::load_one(&id, &[], &mut errs);
        if !errs.is_empty() {
            return Err(errs.join("\n"));
        }
        if m.is_empty() {
            return Err(crate::i18n::trf(
                "{id} の辞書がありません (同梱にも ~/.zaivern/locales にもない)",
                &[("id", id)],
            ));
        }
        (id, m)
    };

    let mut errs = Vec::new();
    let base = crate::locale::load_one(crate::locale::BASE, &[], &mut errs);
    let report = crate::locale::compare(&base, &map);
    let mut out = vec![crate::i18n::trf(
        "{label}: {n} 件 (基準 {b} 件)",
        &[
            ("label", label),
            ("n", map.len().to_string()),
            ("b", base.len().to_string()),
        ],
    )];
    for (title, list) in [
        (crate::i18n::tr("訳が無い"), &report.missing),
        (crate::i18n::tr("基準に無い鍵"), &report.extra),
        (crate::i18n::tr("プレースホルダ不一致"), &report.placeholder),
        (crate::i18n::tr("空の訳"), &report.empty),
    ] {
        if list.is_empty() {
            continue;
        }
        out.push(format!("\n{title} ({}):", list.len()));
        for k in list.iter().take(40) {
            out.push(format!("  {k}"));
        }
        if list.len() > 40 {
            out.push(crate::i18n::trf(
                "  … ほか {n} 件",
                &[("n", (list.len() - 40).to_string())],
            ));
        }
    }
    if report.is_clean() {
        out.push(crate::i18n::tr("✅ 過不足なし"));
        Ok(out.join("\n"))
    } else {
        // fail-closed: 「確かめられなかった」を成功にしない
        Err(out.join("\n"))
    }
}

/// 翻訳の雛形を書き出す。既存ファイルは**上書きしない**。
fn i18n_export(id: &str, out_path: Option<&str>) -> Result<String, String> {
    if id.trim().is_empty() {
        return Err(crate::i18n::tr(
            "書き出す言語 ID を指定してください (例: zai i18n export fr)",
        ));
    }
    let id = crate::locale::normalize(id);
    let dest = match out_path {
        Some(p) => PathBuf::from(p),
        None => crate::config::zaivern_dir()
            .join("locales")
            .join(format!("{id}.json")),
    };
    if dest.exists() {
        return Err(crate::i18n::trf(
            "{path} は既にあります (上書きしません)",
            &[("path", dest.display().to_string())],
        ));
    }
    if let Some(d) = dest.parent() {
        std::fs::create_dir_all(d).map_err(|e| format!("{} を作れません: {e}", d.display()))?;
    }
    let mut errs = Vec::new();
    let map = crate::locale::resolved(&id, &[], &mut errs);
    let sorted: std::collections::BTreeMap<&String, &String> = map.iter().collect();
    let body = serde_json::to_string_pretty(&sorted).map_err(|e| e.to_string())?;
    std::fs::write(&dest, body + "\n")
        .map_err(|e| format!("{} を書けません: {e}", dest.display()))?;
    Ok(crate::i18n::trf(
        "🌐 {path} に {n} 件の雛形を書き出しました",
        &[
            ("path", dest.display().to_string()),
            ("n", map.len().to_string()),
        ],
    ))
}

/// 翻訳シャード 1 件。`zai i18n missing` が出し、翻訳して `apply` で戻す。
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct I18nRow {
    /// ID の名前空間 (`<module>.<action>` の左側)。
    #[serde(default)]
    module: String,
    /// 安定 ID。`missing` の出力では空で、翻訳する人が埋める。
    #[serde(default)]
    id: String,
    /// 原文 (日本語)。**書き換えない**。
    ja: String,
    #[serde(default)]
    en: String,
    #[serde(default, rename = "zh-CN")]
    zh_cn: String,
    #[serde(default)]
    ko: String,
    #[serde(default, rename = "pt-BR")]
    pt_br: String,
    #[serde(default)]
    es: String,
}

impl I18nRow {
    fn get(&self, lang: &str) -> &str {
        match lang {
            "en" => &self.en,
            "zh-CN" => &self.zh_cn,
            "ko" => &self.ko,
            "pt-BR" => &self.pt_br,
            "es" => &self.es,
            _ => &self.ja,
        }
    }
}

/// 画面に出るのに辞書へ載っていない `tr("…")` を探す。
///
/// **見つかったら終了コード 1**。「訳し忘れたまま出荷した」を静かに通さない。
fn i18n_missing(dir: &str, out: Option<&str>) -> Result<String, String> {
    let src = if dir.trim().is_empty() {
        PathBuf::from("src")
    } else {
        PathBuf::from(dir)
    };
    if !src.is_dir() {
        return Err(crate::i18n::trf(
            "{dir} がありません (リポジトリの中で実行してください)",
            &[("dir", src.display().to_string())],
        ));
    }
    let mut errs = Vec::new();
    let ja = crate::locale::load_one(crate::locale::SOURCE_LANG, &[], &mut errs);
    let known: std::collections::HashSet<&str> = ja
        .keys()
        .map(|s| s.as_str())
        .chain(ja.values().map(|s| s.as_str()))
        .collect();

    let mut seen = std::collections::HashSet::new();
    let mut rows: Vec<I18nRow> = Vec::new();
    for (module, lit) in crate::locale::scan_source_literals(&src) {
        if lit.trim().is_empty()
            || crate::locale::NOT_TRANSLATED.contains(&lit.as_str())
            || known.contains(lit.as_str())
            || !seen.insert(lit.clone())
        {
            continue;
        }
        rows.push(I18nRow {
            module,
            ja: lit,
            ..Default::default()
        });
    }

    if let Some(path) = out {
        let body = serde_json::to_string_pretty(&rows).map_err(|e| e.to_string())?;
        std::fs::write(path, body + "\n").map_err(|e| format!("{path} を書けません: {e}"))?;
        let msg = crate::i18n::trf(
            "{n} 件を {path} へ書き出しました",
            &[("n", rows.len().to_string()), ("path", path.to_string())],
        );
        return if rows.is_empty() { Ok(msg) } else { Err(msg) };
    }
    if rows.is_empty() {
        return Ok(crate::i18n::tr("✅ 訳が無い文字列はありません"));
    }
    let mut out_lines: Vec<String> = rows
        .iter()
        .map(|r| format!("{}\t{:?}", r.module, r.ja))
        .collect();
    out_lines.push(crate::i18n::trf(
        "--- 訳が無い文字列: {n} 件",
        &[("n", rows.len().to_string())],
    ));
    Err(out_lines.join("\n"))
}

/// 翻訳シャードを `locales/*.json` 6 枚へ取り込む。
///
/// **プレースホルダが合わない訳は採らない** — 実行時に穴が開くより、英語のまま
/// 出るほうが害が小さい。ID が衝突したら `_2` を付けて分ける (先に居るほうを
/// 動かさない)。
fn i18n_apply(shard: &str, dir: Option<&str>) -> Result<String, String> {
    if shard.trim().is_empty() {
        return Err(crate::i18n::tr(
            "取り込むシャード JSON を指定してください (zai i18n missing --out で作れます)",
        ));
    }
    let raw = std::fs::read_to_string(shard).map_err(|e| format!("{shard} を読めません: {e}"))?;
    let rows: Vec<I18nRow> =
        serde_json::from_str(&raw).map_err(|e| format!("{shard} の解析に失敗: {e}"))?;
    let root = PathBuf::from(dir.unwrap_or("locales"));
    if !root.is_dir() {
        return Err(crate::i18n::trf(
            "{dir} がありません",
            &[("dir", root.display().to_string())],
        ));
    }

    let langs: Vec<&str> = crate::locale::BUILTIN.iter().map(|(i, _, _)| *i).collect();
    let mut maps: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>> =
        Default::default();
    for lg in &langs {
        let p = root.join(format!("{lg}.json"));
        let body = std::fs::read_to_string(&p)
            .map_err(|e| format!("{} を読めません: {e}", p.display()))?;
        let m = crate::locale::parse_json(&body, &p.display().to_string())?;
        maps.insert((*lg).to_string(), m.into_iter().collect());
    }

    let mut used: std::collections::HashSet<String> =
        maps[crate::locale::SOURCE_LANG].keys().cloned().collect();
    let mut warns = Vec::new();
    let mut added = 0usize;
    for r in &rows {
        let base = if r.id.contains('.') {
            r.id.clone()
        } else if r.id.is_empty() {
            format!(
                "{}.x",
                if r.module.is_empty() {
                    "misc"
                } else {
                    &r.module
                }
            )
        } else {
            format!("{}.{}", r.module, r.id)
        };
        let mut ident = base.clone();
        let mut n = 2;
        while used.contains(&ident) {
            ident = format!("{base}_{n}");
            n += 1;
        }
        used.insert(ident.clone());

        let want = crate::locale::placeholders(&r.ja);
        let en = if r.en.trim().is_empty() {
            r.ja.clone()
        } else {
            r.en.clone()
        };
        for lg in &langs {
            let v = if *lg == crate::locale::SOURCE_LANG {
                r.ja.clone()
            } else {
                let t = r.get(lg);
                let t = if t.trim().is_empty() {
                    en.clone()
                } else {
                    t.to_string()
                };
                if crate::locale::placeholders(&t) == want {
                    t
                } else {
                    warns.push(format!("⚠ {ident} [{lg}] プレースホルダ不一致 — en で代替"));
                    if crate::locale::placeholders(&en) == want {
                        en.clone()
                    } else {
                        r.ja.clone()
                    }
                }
            };
            maps.get_mut(*lg).expect("lang").insert(ident.clone(), v);
        }
        added += 1;
    }

    for lg in &langs {
        let p = root.join(format!("{lg}.json"));
        let body = serde_json::to_string_pretty(&maps[*lg]).map_err(|e| e.to_string())?;
        std::fs::write(&p, body + "\n")
            .map_err(|e| format!("{} を書けません: {e}", p.display()))?;
    }
    let mut out = warns;
    out.push(crate::i18n::trf(
        "{n} 件を取り込みました (合計 {total} 件)",
        &[
            ("n", added.to_string()),
            ("total", maps[crate::locale::SOURCE_LANG].len().to_string()),
        ],
    ));
    Ok(out.join("\n"))
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

// ───────────────────────── lease: ファイル所有 ─────────────────────────

/// `zai lease …` — ファイル所有リースのヘッドレス導線。
///
/// **GUI を必要としない。** 強制を担う `zai hook` が短命プロセスで動く以上、
/// その設定と確認も CLI から完結できないと、サーバ / CI / リモートで使えない。
/// 対象リポジトリは**カレントディレクトリから導出**する (`--dir` で明示も可)。
fn lease_dispatch(args: &[String]) -> CliOut {
    use crate::lease;
    if wants_help(args) {
        return Ok(HELP_LEASE.trim_end().to_string());
    }
    let sub = args.first().map(String::as_str).unwrap_or("");
    let rest: &[String] = if args.is_empty() { &[] } else { &args[1..] };
    let (dir, rest) = take_opt(rest, "--dir");
    // **`--dir` を打ったこと自体が「ここがルートだ」という明示。**
    // git 管理でないフォルダでも、利用者が名指ししたなら推測ではない。
    let explicit_dir = dir.is_some();
    let start = match dir {
        Some(d) => PathBuf::from(d),
        None => std::env::current_dir()
            .map_err(|e| CliError::Runtime(format!("カレントディレクトリが判りません: {e}")))?,
    };
    let roots = lease::roots_of(&start);
    // **旧キーの台帳を引き取ってから読む。** GUI 側 (`agents.rs` / `session_picker.rs`)
    // にはこの引き取りが入っているが CLI には無く、rustc の版が変わって
    // キーが変わったあとの `zai lease status` が、実際には残っている台帳を
    // 「確保中: 0 件」と答えていた (実バイナリで実証済み)。
    crate::history::adopt_legacy_keys(&roots.key);
    let store = lease::store_path_in(&lease::store_dir(), &roots.key);
    let now = lease::now_secs();
    match sub {
        "status" => {
            reject_extra(&rest, "zai lease status [--dir <フォルダ>]")?;
            let tier = lease::current_tier(&roots);
            let n = lease::read_store(&store).map(|s| s.leases.len()).unwrap_or(0);
            // worktree のときは 2 つのルートが別物になる。**それを隠さない** —
            // 「なぜ別フォルダの相手と衝突するのか」がここでしか判らない。
            let tree = if roots.tree == roots.key {
                String::new()
            } else {
                format!("\n作業ツリー: {}", roots.tree.display())
            };
            Ok(format!(
                "段: {}\n{}\nリポジトリ (台帳の単位): {}{tree}\n台帳: {}\n確保中: {n} 件",
                crate::i18n::tr(tier.label()),
                crate::i18n::tr(tier.detail()),
                roots.key.display(),
                store.display()
            ))
        }
        "enable" => {
            reject_extra(&rest, "zai lease enable [--dir <フォルダ>]")?;
            require_known_root(&roots, explicit_dir, "enable")?;
            lease::enable(&store).map_err(CliError::Runtime)?;
            Ok(format!("有効にしました: {}", store.display()))
        }
        "disable" => {
            reject_extra(&rest, "zai lease disable [--dir <フォルダ>]")?;
            if store.exists() {
                std::fs::remove_file(&store)
                    .map_err(|e| CliError::Runtime(format!("台帳を消せません: {e}")))?;
            }
            Ok("無効にしました".to_string())
        }
        "list" => {
            let (json, rest) = take_flag(&rest, "--json");
            reject_extra(&rest, "zai lease list [--json]")?;
            let st = lease::read_store(&store).map_err(CliError::Runtime)?;
            Ok(render_leases(&st, now, json))
        }
        "claim" => {
            const CLAIM_USAGE: &str = "zai lease claim [--dir <フォルダ>] [--agent 名前] [--shift [--max-shift <行>]] <パターン...>";
            let (agent, rest) = take_opt(&rest, "--agent");
            // **`--shift` を付けなければ、以下は 1 バイトも変わらない。**
            // 「要求どおりか、拒否か」という既存の契約を守る側と、
            // 「断らずにずらす」側を、この 1 つの旗だけで分ける。
            let (shift, rest) = take_flag(&rest, "--shift");
            let (max_shift, rest) = take_opt(&rest, "--max-shift");
            // **知らない旗をファイル名として飲み込まない** ([`reject_unknown_flags`])。
            let rest = reject_unknown_flags(&rest, CLAIM_USAGE)?;
            if rest.is_empty() {
                return Err(CliError::Usage(format!(
                    "確保するパターンを 1 つ以上指定してください: {CLAIM_USAGE}"
                )));
            }
            if max_shift.is_some() && !shift {
                return Err(CliError::Usage(
                    "--max-shift は --shift と一緒に指定してください".into(),
                ));
            }
            // 既定は交渉層と同じ設定 (`negotiate.max_shift`) から。
            // **無制限を既定にしない** — 1 万行ずらされたら、利用者が
            // 頼んだ場所とは無関係な場所を確保することになる。
            let max_shift: Option<u32> = match max_shift {
                Some(v) => Some(v.parse::<u32>().map_err(|_| {
                    CliError::Usage(format!(
                        "--max-shift には行数 (0 以上の整数) を指定してください: {v}"
                    ))
                })?),
                None => Some(lease::default_max_shift_in(&roots.tree)),
            };
            require_known_root(&roots, explicit_dir, "claim")?;
            // **絶対パスをスコープ相対へ直す。** ここを通さないと
            // `normalize_path` が先頭の `/` を落とし、`/repo/src/a.rs` が
            // `repo/src/a.rs` という実在しない鍵で台帳に載る
            // (= 相対指定と永久に一致せず、「確保しました」が嘘になる)。
            // スコープ外なら**成功と偽らずに失敗**する。
            let rest = lease::resolve_spec_args(&roots.tree, &rest)
                .map_err(CliError::Usage)?;
            let holder = lease::Holder {
                agent: agent.unwrap_or_else(|| "cli".to_string()),
                session: String::new(),
                cwd: lease::normalize_path(&start.to_string_lossy()),
                pid: std::process::id(),
            };
            // 行域を頼まれたのに**ファイル全体でしか守れない**ものを先に言う。
            let notes: Vec<String> = rest
                .iter()
                .filter_map(|p| lease::degradation_note(&roots.tree, p))
                .map(|n| format!("注意: {n}"))
                .collect();
            // 台帳はできてもフックが無ければ**他プロセスは止まらない**。
            // **確保した後に調べる** — `claim` は台帳ファイルを新規作成して
            // 暗黙に有効化するので、先に調べると「まだ無効」と読めてしまい
            // 肝心の未初期化のときだけ黙る (実バイナリで踏んだ)。
            let tier_note = || uninitialized_note(&roots, &store);
            // **`with_store_retry` を使う。** 素の `with_store` は台帳ロックが
            // 取れなかった時点で即座に諦めるので、高並列だと「他人が持っている」
            // でも「取れた」でもない *busy* が大量に出る。実測 (64 体が同じ
            // ファイルの離れた行域を取る) で、素の `with_store` は
            // **64 件中 18 件しか通らず 46 件が busy**。retry 版では 64/64。
            // 原因は混雑そのものではなく待ち方で、譲る＋揺らぎ付き指数
            // バックオフに替えると消える (`with_store_retry` の doc 参照)。
            if shift {
                // **位置決めは台帳ロックの内側で行う** — 外で空きを探すと、
                // 64 体が同じ空きを見つけて同じ場所を取りに行く。
                let out = lease::with_store_retry(&store, |s| {
                    lease::try_claim_shift_in(
                        &roots.tree,
                        s,
                        &holder,
                        &rest,
                        now,
                        lease::DEFAULT_TTL_SECS,
                        &|p| crate::instances::pid_alive(p),
                        max_shift,
                    )
                })
                .map_err(CliError::Runtime)?;
                return match out {
                    lease::ShiftClaim::Granted(gs) => {
                        let moved = gs.iter().filter(|g| g.moved()).count();
                        let mut lines = vec![if moved == 0 {
                            format!("{} 件を確保しました", gs.len())
                        } else {
                            format!("{} 件を確保しました ({moved} 件をずらしました)", gs.len())
                        }];
                        lines.extend(
                            gs.iter()
                                .filter(|g| g.moved())
                                .map(|g| format!("  ずらしました: {} → {}", g.asked, g.spec)),
                        );
                        lines.extend(notes);
                        lines.extend(tier_note());
                        // **最後の行は必ず `granted <仕様>`。** 機械が読む面なので
                        // 装飾を付けない (人向けの説明は上の行に出し切る)。
                        lines.extend(gs.iter().map(|g| format!("granted {}", g.spec)));
                        Ok(lines.join("\n"))
                    }
                    lease::ShiftClaim::Refused { owner, pattern, .. } => {
                        Err(CliError::Runtime(refusal(&pattern, &owner)))
                    }
                };
            }
            // **`--dir` を渡した相対化の起点をここでも使う。** 以前は
            // `try_claim` (= プロセスの作業フォルダ) へ落ちていたので、
            // `--dir` を付けても `#fn:` / `#L` の解決が cwd 基準になっていた
            // (`--shift` 側だけが `roots.tree` を渡していた = 経路が 2 つ
            //  あることそのものが原因だった)。**両方が同じ起点を通る。**
            let out = lease::with_store_retry(&store, |s| {
                lease::try_claim_in(
                    &roots.tree,
                    s,
                    &holder,
                    &rest,
                    now,
                    lease::DEFAULT_TTL_SECS,
                    &|p| crate::instances::pid_alive(p),
                )
            })
            .map_err(CliError::Runtime)?;
            match out {
                lease::Claim::Granted(n) => {
                    let mut lines = vec![format!("{n} 件を確保しました")];
                    lines.extend(notes);
                    lines.extend(tier_note());
                    Ok(lines.join("\n"))
                }
                lease::Claim::Refused { owner, pattern, .. } => {
                    Err(CliError::Runtime(refusal(&pattern, &owner)))
                }
            }
        }
        "release" => {
            let (all, rest) = take_flag(&rest, "--all");
            let (agent, rest) = take_opt(&rest, "--agent");
            reject_extra(&rest, "zai lease release [--agent 名前 | --all]")?;
            // 解放も同じ理由で retry 版を使う (解放が busy で落ちると、
            // 持ち主が居ないのに担当が残り続ける = 誰も書けなくなる)。
            let n = lease::with_store_retry(&store, |s| {
                let before = s.leases.len();
                if all {
                    s.leases.clear();
                } else if let Some(a) = &agent {
                    s.leases.retain(|l| &l.holder.agent != a);
                }
                before - s.leases.len()
            })
            .map_err(CliError::Runtime)?;
            if !all && agent.is_none() {
                return Err(CliError::Usage(
                    "--agent <名前> か --all を指定してください".into(),
                ));
            }
            Ok(format!("{n} 件を解放しました"))
        }
        "" => Err(CliError::Usage(
            "lease のサブコマンドを指定してください: status / enable / disable / list / claim / release"
                .into(),
        )),
        other => Err(CliError::Usage(format!(
            "不明な lease サブコマンドです: {other}"
        ))),
    }
}

/// 確保できなかったときの 1 行 (**純粋関数**)。
///
/// `owner` は 2 種類ある: 本当の持ち主の名前と、**指定そのものが解決できない
/// 理由** (`fn a を探せません: …`)。後者を「〜が持っています」に流し込むと
/// 「`fn a を探せません: …` **が持っています**」という意味の通らない文になり、
/// 実バイナリで実際にそう出ていた。理由文かどうかで文型を変える。
fn refusal(pattern: &str, owner: &str) -> String {
    // 持ち主の表示名 ([`lease::Holder::display`]) には `:` が入らないが、
    // 解決できない理由には必ず `:` が入る (`… を探せません: …`)。
    //
    // **理由を新しく足す側の約束**: 先頭に `:` を含む見出しを置くこと
    // (`ずらせる上限に当たりました: …`)。忘れると「〜が持っています」へ
    // 流し込まれて意味の通らない文になる。
    // [`lease::tests::断る理由は必ず見出しに区切りを持つ`] が番人。
    if owner.contains(':') {
        return format!("確保できません: 「{pattern}」— {owner}");
    }
    format!("確保できません: 「{pattern}」は {owner} が持っています")
}

/// 台帳を**新しく作る**操作の前に、ルートが推測でないことを確かめる。
///
/// git 管理下でも既存の台帳でもないフォルダで台帳を生やすと、
/// **サブフォルダごとに別の台帳**が積み上がって、同じファイルを見ている
/// 2 人が互いに見えなくなる (実バイナリで再現: `/w`・`/w/a`・`/w/a/b` が
/// 3 つの別の鍵になった)。`czero init` は `git rev-parse --show-toplevel` を
/// 要求して失敗するのに `lease claim` だけが黙って作る、という**非対称**も
/// ここで消える。読むだけの `status` / `list` は従来どおり通す。
fn require_known_root(
    roots: &crate::lease::Roots,
    explicit_dir: bool,
    what: &str,
) -> Result<(), CliError> {
    if roots.rooted || explicit_dir {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "git リポジトリではないので、どこをルートにすべきか決められません: {}\n\
         このまま {what} すると、サブフォルダごとに別の台帳ができて互いに見えなくなります。\n\
         対処: (1) このフォルダで `git init` する (2) ルートを明示する: zai lease enable --dir <ルート>\n\
         (--dir を付けた呼び出しは「そこがルート」として通ります)",
        roots.tree.display()
    )))
}

/// 台帳はできたが**フックが無いので他プロセスは止まらない**ときの注意書き。
///
/// `zai lease claim` は台帳ファイルを新規作成し、有効判定はファイルの存在な
/// ので**暗黙に有効化**される。しかしフック未導入では段は「勧告」どまりで、
/// 他プロセスは 1 つも止まらない。それでも `claim` を**拒否せずに通す**のは:
///
/// * 「勧告」は設計上の正規の段で、GUI・`czero` の計画分割・人手のレビューは
///   台帳だけで機能する (止めないだけで、記録は正しく効いている)
/// * ここで拒否すると `lease enable` → `claim` という既存の導線が丸ごと死ぬ
/// * 拒否は**利用者が求めていない**方向の fail-closed — 守れないのは
///   「他人を止めること」だけで、記録が嘘になるわけではない
///
/// 直すべきなのは**黙っていたこと**なので、確保は通して事実を必ず出す。
fn uninitialized_note(roots: &crate::lease::Roots, store: &Path) -> Vec<String> {
    if crate::lease::current_tier(roots) != crate::lease::Tier::Advisory {
        return Vec::new();
    }
    let _ = store;
    vec![
        "注意: 所有は記録しましたが、フックが未導入のため**他のプロセスは止まりません**。"
            .to_string(),
        "      強制するには: zai czero init".to_string(),
    ]
}

/// 確保中の一覧を表示用に整える (**純粋関数** — テーブルテストできる形)。
fn render_leases(st: &crate::lease::Store, now: u64, json: bool) -> String {
    if json {
        return serde_json::to_string_pretty(st).unwrap_or_else(|_| "[]".into());
    }
    if st.leases.is_empty() {
        return "確保中のファイルはありません。".to_string();
    }
    let mut out = String::new();
    for l in &st.leases {
        out.push_str(&format!(
            "{}\t{}\t残り {}\n",
            l.holder.display(),
            l.patterns.join(", "),
            crate::instances::humanize_uptime(l.expires_at.saturating_sub(now))
        ));
    }
    out.trim_end().to_string()
}

// ───────────────────────── update / uninstall: 自分自身の面倒を見る ─────────────────────────

/// ワンライナーインストーラの実体をビルド時に取り込む。
///
/// **owner/repo も配布 URL も「ここから読む」。** cli.rs に直書きすると、
/// リポジトリを移したときに install.sh だけが直って CLI は古い URL を
/// 案内し続ける — 直書き禁止の一形態。README が案内しているワンライナーと
/// 同一であることは下のテストが番人になる。
const INSTALL_SH: &str = include_str!("../install.sh");
const INSTALL_PS1: &str = include_str!("../install.ps1");

/// 配布元 (GitHub) の各 URL。すべて [`INSTALL_SH`] / [`INSTALL_PS1`] から導出する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Distribution {
    /// `owner/name`
    pub slug: String,
    /// リポジトリの Web URL (`cargo install --git` に渡すもの)
    pub repo_url: String,
    /// 最新リリースを返す API の URL
    pub latest_api: String,
    /// macOS / Linux 用インストーラの URL
    pub installer_sh: String,
    /// Windows 用インストーラの URL
    pub installer_ps1: String,
}

/// `https://…` のトークンを引用符・空白・パイプで切り出す。
/// シェルと PowerShell の両方を同じ関数で読むための最小限のスキャナ。
fn https_tokens(text: &str) -> Vec<&str> {
    const HEAD: &str = "https://";
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(i) = rest.find(HEAD) {
        let tail = &rest[i..];
        let end = tail
            .find(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | ')' | '`' | '|'))
            .unwrap_or(tail.len());
        out.push(&tail[..end]);
        rest = &tail[end.max(HEAD.len())..];
    }
    out
}

/// 条件に合う最初の `https://…` を返す。
fn find_url(text: &str, pred: impl Fn(&str) -> bool) -> Option<&str> {
    https_tokens(text).into_iter().find(|u| pred(u))
}

/// スクリプト内の変数参照 (`$REPO` / `$repo`) を実際の slug に置き換える。
fn subst_repo(url: &str, slug: &str) -> String {
    url.replace("$REPO", slug).replace("$repo", slug)
}

/// `REPO="owner/name"` / `$repo = "owner/name"` から `owner/name` を取り出す。
///
/// URL 形 (`REPO_URL="https://…/$REPO"`) を誤って拾わないよう、
/// **スラッシュ 1 個・コロン無し**の形だけを受け付ける。
pub fn parse_repo_slug(script: &str) -> Option<String> {
    for line in script.lines() {
        let t = line.trim_start();
        if t.starts_with('#') {
            continue;
        }
        let lower = t.to_ascii_lowercase();
        if !(lower.starts_with("repo=")
            || lower.starts_with("$repo=")
            || lower.starts_with("$repo "))
        {
            continue;
        }
        let Some(value) = t.split('"').nth(1) else {
            continue;
        };
        let parts: Vec<&str> = value.split('/').collect();
        let ok = parts.len() == 2
            && parts.iter().all(|p| !p.is_empty())
            && !value.contains(':')
            && !value.contains(char::is_whitespace);
        if ok {
            return Some(value.to_string());
        }
    }
    None
}

/// インストーラ 2 本から配布元の URL 一式を組み立てる。
/// どれか 1 つでも読めなければ `None` (中途半端な URL を案内しない)。
pub fn distribution_from(sh: &str, ps1: &str) -> Option<Distribution> {
    let slug = parse_repo_slug(sh).or_else(|| parse_repo_slug(ps1))?;
    // リポジトリ Web URL は `"https://github.com/$REPO"` の形で書かれている。
    let repo_url = find_url(sh, |u| u.ends_with("$REPO") || u.ends_with("$repo"))
        .or_else(|| find_url(ps1, |u| u.ends_with("$REPO") || u.ends_with("$repo")))
        .map(|u| subst_repo(u, &slug))?;
    let latest_api = find_url(sh, |u| u.ends_with("/releases/latest"))
        .or_else(|| find_url(ps1, |u| u.ends_with("/releases/latest")))
        .map(|u| subst_repo(u, &slug))?;
    // ヘッダコメントのワンライナー = README が案内しているものと同一。
    let installer_sh = find_url(sh, |u| u.ends_with("/install.sh"))?.to_string();
    let installer_ps1 = find_url(ps1, |u| u.ends_with("/install.ps1"))?.to_string();
    Some(Distribution {
        slug,
        repo_url,
        latest_api,
        installer_sh,
        installer_ps1,
    })
}

/// 実行時に使う配布元情報。
fn distribution() -> Result<Distribution, CliError> {
    distribution_from(INSTALL_SH, INSTALL_PS1).ok_or_else(|| {
        CliError::Runtime(
            "配布元の URL を特定できませんでした (install.sh を確認してください)".into(),
        )
    })
}

/// `v0.8.0` / `0.8.0-rc1` → `[0, 8, 0]`。数値として読めない要素は 0 に落とす。
pub fn parse_version(s: &str) -> [u64; 3] {
    let core = s.trim().trim_start_matches(['v', 'V']);
    let mut out = [0u64; 3];
    for (i, part) in core.split('.').take(3).enumerate() {
        let digits: String = part.chars().take_while(char::is_ascii_digit).collect();
        out[i] = digits.parse().unwrap_or(0);
    }
    out
}

/// 配布元の方が新しいか。同じ・古い場合は false (= 更新不要)。
pub fn version_is_newer(latest: &str, current: &str) -> bool {
    parse_version(latest) > parse_version(current)
}

/// URL の本文を取る。
///
/// **HTTP クライアントのクレートは足さない** — どの OS にも標準で入っている
/// ものを子プロセスで呼ぶ (macOS / Linux: curl、Windows: PowerShell)。
/// 依存を 1 つ増やすと配布バイナリと監査対象が増えるが、ここで欲しいのは
/// 「タグ名 1 個」だけなので割に合わない。
fn fetch_text(url: &str) -> Result<String, CliError> {
    // 自前の定数由来の URL しか来ないが、埋め込む前に必ず形を確認する。
    if !url.starts_with("https://") || url.contains('\'') || url.contains(char::is_whitespace) {
        return Err(CliError::Runtime(format!("取得できない URL です: {url}")));
    }
    let out = if cfg!(windows) {
        crate::procx::hidden_command("powershell")
            .arg("-NoProfile")
            .arg("-NonInteractive")
            .arg("-Command")
            .arg(format!(
                "[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12; \
                 (Invoke-WebRequest -UseBasicParsing -TimeoutSec 20 -Uri '{url}').Content"
            ))
            .output()
    } else {
        crate::procx::hidden_command("curl")
            .arg("-fsSL")
            .arg("--max-time")
            .arg("20")
            .arg(url)
            .output()
    };
    let out = out.map_err(|e| {
        CliError::Runtime(format!(
            "ネットワーク取得コマンドを起動できません: {e} (curl / PowerShell が必要です)"
        ))
    })?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
        return Err(CliError::Runtime(format!(
            "配布元へ接続できませんでした: {url}{}",
            if err.is_empty() {
                String::new()
            } else {
                format!(" — {err}")
            }
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// 最新リリースのタグ (`v0.8.1` など)。
fn fetch_latest_tag(api_url: &str) -> Result<String, CliError> {
    let body = fetch_text(api_url)?;
    let v: serde_json::Value = serde_json::from_str(&body)
        .map_err(|e| CliError::Runtime(format!("配布元の応答を解釈できません: {e}")))?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .filter(|t| !t.is_empty())
        .map(str::to_string)
        .ok_or_else(|| CliError::Runtime("配布元に公開済みのリリースがありません".into()))
}

/// 更新の手段。**実行ファイルの置き場所**で決まる。
/// `cargo install` で入れた人にインストーラを流し込むと、`~/.cargo/bin` と
/// `~/.local/bin` に別バージョンが並んで「更新したのに古いまま」になる。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateMethod {
    /// `cargo install --git … --force`
    Cargo,
    /// `curl -fsSL <install.sh> | sh`
    Shell,
    /// `irm <install.ps1> | iex`
    PowerShell,
}

/// 更新手段を選ぶ純関数 (テストから OS と置き場所を差し込めるようにしてある)。
pub fn choose_update_method(exe: &Path, cargo_bin: Option<&Path>, windows: bool) -> UpdateMethod {
    if let (Some(dir), Some(parent)) = (cargo_bin, exe.parent()) {
        if parent == dir {
            return UpdateMethod::Cargo;
        }
    }
    if windows {
        UpdateMethod::PowerShell
    } else {
        UpdateMethod::Shell
    }
}

/// `cargo install` の置き場 (`CARGO_HOME` を尊重する)。
fn cargo_bin_dir() -> Option<PathBuf> {
    if let Some(home) = std::env::var_os("CARGO_HOME") {
        return Some(PathBuf::from(home).join("bin"));
    }
    dirs::home_dir().map(|h| h.join(".cargo").join("bin"))
}

/// 画面に見せる更新コマンド。**実行 ([`run_update`]) と対で必ずここから作る** —
/// 表示と実行が食い違うと、ユーザーは同意していないコマンドを踏むことになる。
pub fn update_command_line(method: UpdateMethod, dist: &Distribution) -> String {
    match method {
        UpdateMethod::Cargo => format!(
            "cargo install --git {} --locked --force {}",
            dist.repo_url,
            env!("CARGO_PKG_NAME")
        ),
        UpdateMethod::Shell => format!("curl -fsSL {} | sh", dist.installer_sh),
        UpdateMethod::PowerShell => format!("irm {} | iex", dist.installer_ps1),
    }
}

/// 更新を実行する。**自分自身を消しには行かない** — 上書きはインストーラ
/// (Windows は実行中 exe を改名して差し替える) に任せ、ここは待つだけ。
fn run_update(method: UpdateMethod, dist: &Distribution) -> Result<(), CliError> {
    let line = update_command_line(method, dist);
    let mut cmd = match method {
        UpdateMethod::Cargo => {
            let mut c = std::process::Command::new("cargo");
            c.arg("install")
                .arg("--git")
                .arg(&dist.repo_url)
                .arg("--locked")
                .arg("--force")
                .arg(env!("CARGO_PKG_NAME"));
            c
        }
        UpdateMethod::Shell => {
            let mut c = std::process::Command::new("sh");
            c.arg("-c")
                .arg(format!("curl -fsSL {} | sh", dist.installer_sh));
            c
        }
        UpdateMethod::PowerShell => {
            let mut c = std::process::Command::new("powershell");
            c.arg("-NoProfile")
                .arg("-Command")
                .arg(format!("irm {} | iex", dist.installer_ps1));
            c
        }
    };
    let status = cmd.status().map_err(|e| {
        CliError::Runtime(format!(
            "更新コマンドを起動できませんでした: {e}\n手動で次を実行してください:\n  {line}"
        ))
    })?;
    if status.success() {
        return Ok(());
    }
    Err(CliError::Runtime(format!(
        "更新に失敗しました (終了コード {})。手動で次を実行してください:\n  {line}",
        status.code().unwrap_or(-1)
    )))
}

/// 自分自身の絶対パス。シンボリックリンク経由でも実体を指すよう canonicalize する
/// (リンクだけ消しても実体が残り、PATH 次第でまだ起動できてしまうため)。
fn resolve_exe() -> Result<PathBuf, CliError> {
    let p = std::env::current_exe()
        .map_err(|e| CliError::Runtime(format!("実行ファイルの場所を特定できません: {e}")))?;
    Ok(p.canonicalize().unwrap_or(p))
}

/// 破壊的な操作の前に `y` を求める。
/// 標準入力が無い (パイプ・CI) 場合は **中止側に倒す** — 確認できないまま消さない。
fn confirm(prompt: &str) -> bool {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    match std::io::stdin().read_line(&mut line) {
        Ok(0) | Err(_) => false,
        Ok(_) => {
            let a = line.trim().to_ascii_lowercase();
            a == "y" || a == "yes"
        }
    }
}

/// `zai update` のディスパッチ。
fn update_dispatch(args: &[String]) -> CliOut {
    if wants_help(args) {
        return Ok(HELP_UPDATE.trim_end().to_string());
    }
    const USAGE: &str = "zai update [--check] [--yes|-y]";
    let (check, rest) = take_flag(args, "--check");
    let (yes_long, rest) = take_flag(&rest, "--yes");
    let (yes_short, rest) = take_flag(&rest, "-y");
    reject_extra(&rest, USAGE)?;
    let yes = yes_long || yes_short;

    let dist = distribution()?;
    let current = env!("CARGO_PKG_VERSION");
    println!("現在のバージョン: {current}");
    println!("配布元を確認しています: {}", dist.repo_url);
    let latest = fetch_latest_tag(&dist.latest_api)?;
    println!("配布元の最新版:   {latest}");
    if !version_is_newer(&latest, current) {
        return Ok("✅ 最新です。更新の必要はありません。".into());
    }

    let exe = resolve_exe()?;
    let method = choose_update_method(&exe, cargo_bin_dir().as_deref(), cfg!(windows));
    let line = update_command_line(method, &dist);
    println!();
    println!("🆕 新しいバージョンがあります: {current} → {latest}");
    println!("インストール先: {}", exe.display());
    println!("次のコマンドで更新します:");
    println!("  {line}");
    if check {
        return Ok("(--check のため実行していません)".into());
    }
    if !yes && !confirm("\n実行しますか? [y/N]: ") {
        return Ok("中止しました。".into());
    }
    println!();
    run_update(method, &dist)?;
    Ok("✅ 更新しました。`zai --version` で確認してください。".into())
}

// ───────────────────────── uninstall ─────────────────────────

/// `--keep-config` のときに残すもの (= 設定そのもの)。
/// セッション記録や端末ログは「設定」ではないので残さない
/// (容量の大半がここなので、残すと消したい人が消せなくなる)。
const KEEP_ON_CONFIG: &[&str] = &["config.toml", "state.toml"];

/// 削除候補 1 件 (`~/.zaivern` 直下の 1 エントリ)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallEntry {
    /// 表示名 (ディレクトリは末尾に `/`)
    pub label: String,
    pub path: PathBuf,
    pub size: u64,
    /// `--keep-config` で残すもの
    pub keep: bool,
}

/// `zai uninstall` が消すもの一式。**表示 → 確認 → 削除**の順に使う。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub exe: PathBuf,
    pub exe_size: u64,
    pub data_dir: PathBuf,
    pub entries: Vec<UninstallEntry>,
    /// PATH 上に残る別の `zai`。**消さない** — 案内だけする。
    pub others: Vec<PathBuf>,
    pub keep_config: bool,
}

impl UninstallPlan {
    /// 実際に消える合計サイズ (残すものは数えない)。
    pub fn total(&self) -> u64 {
        self.exe_size
            + self
                .entries
                .iter()
                .filter(|e| !e.keep)
                .map(|e| e.size)
                .sum::<u64>()
    }

    /// データ側の削除対象をこの順に消す。実行ファイルは含めない
    /// (自分を消してから他を消しに行かないよう、呼び出し側で最後に扱う)。
    pub fn removals(&self) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = self
            .entries
            .iter()
            .filter(|e| !e.keep)
            .map(|e| e.path.clone())
            .collect();
        // 何も残さないなら入れ物ごと消す (空ディレクトリを置き去りにしない)。
        if !self.keep_config && self.data_dir.exists() {
            v.push(self.data_dir.clone());
        }
        v
    }
}

/// パス配下の合計サイズ。**シンボリックリンクは辿らない** —
/// リンク先の容量を「消える容量」に数えると桁が嘘になる。
pub fn dir_size(p: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(p) else {
        return 0;
    };
    if meta.file_type().is_symlink() {
        return 0;
    }
    if !meta.is_dir() {
        return meta.len();
    }
    let Ok(rd) = std::fs::read_dir(p) else {
        return 0;
    };
    rd.flatten().map(|e| dir_size(&e.path())).sum()
}

/// 人が読むサイズ表記。
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = bytes as f64;
    let mut i = 0usize;
    while v >= 1024.0 && i + 1 < UNITS.len() {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{bytes} B")
    } else if v < 10.0 {
        format!("{v:.1} {}", UNITS[i])
    } else {
        format!("{v:.0} {}", UNITS[i])
    }
}

/// PATH 上に残る、いま動いているものとは別の `zai`。
///
/// **消さない。** 削除して良いのは `current_exe()` 自身と `~/.zaivern` 配下だけ、
/// という安全規則を崩さないため、見つけたら一覧に出して手で消してもらう。
fn other_binaries_on_path(exe: &Path) -> Vec<PathBuf> {
    let (Some(name), Some(path)) = (exe.file_name(), std::env::var_os("PATH")) else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(name);
        if !cand.is_file() {
            continue;
        }
        let real = cand.canonicalize().unwrap_or(cand);
        if real == exe || out.contains(&real) {
            continue;
        }
        out.push(real);
    }
    out
}

/// 削除計画を組み立てる。fs を読むだけで**何も消さない**ので、
/// テストは一時ディレクトリを渡してそのまま検証できる。
pub fn build_uninstall_plan(
    exe: &Path,
    data_dir: &Path,
    keep_config: bool,
    others: Vec<PathBuf>,
) -> UninstallPlan {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(data_dir) {
        let mut paths: Vec<PathBuf> = rd.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            let label = if p.is_dir() {
                format!("{name}/")
            } else {
                name.clone()
            };
            entries.push(UninstallEntry {
                keep: keep_config && KEEP_ON_CONFIG.contains(&name.as_str()),
                size: dir_size(&p),
                label,
                path: p,
            });
        }
    }
    UninstallPlan {
        exe_size: dir_size(exe),
        exe: exe.to_path_buf(),
        data_dir: data_dir.to_path_buf(),
        entries,
        others,
        keep_config,
    }
}

/// 削除計画を人が読む形にする。**確認を求める前に必ずこれを出す。**
pub fn render_uninstall_plan(plan: &UninstallPlan, dry_run: bool) -> String {
    let mut s = String::new();
    if dry_run {
        s.push_str("(--dry-run: 一覧を出すだけで、何も消しません)\n\n");
    }
    s.push_str("Zaivern Code をアンインストールします。\n\n削除するもの:\n");
    s.push_str(&format!(
        "  [実行ファイル]   {} ({})\n",
        plan.exe.display(),
        human_size(plan.exe_size)
    ));
    if plan.entries.is_empty() {
        s.push_str(&format!(
            "  [設定・データ]   {} — ありません\n",
            plan.data_dir.display()
        ));
    } else {
        s.push_str(&format!("  [設定・データ]   {}\n", plan.data_dir.display()));
        for e in &plan.entries {
            let size = human_size(e.size);
            if e.keep {
                s.push_str(&format!(
                    "      - {:<20} {:>9}  (--keep-config のため残します)\n",
                    e.label, size
                ));
            } else {
                s.push_str(&format!("      - {:<20} {:>9}\n", e.label, size));
            }
        }
    }
    s.push_str("  [OS のアプリ登録] Launchpad / アプリメニュー / スタートメニューの登録を解除\n");
    s.push_str(&format!(
        "\n消える合計サイズ: {}\n",
        human_size(plan.total())
    ));
    if !plan.others.is_empty() {
        s.push_str(
            "\n⚠ PATH 上に別の zai が残ります (安全のため自動では消しません。手で削除してください):\n",
        );
        for p in &plan.others {
            s.push_str(&format!("  {}\n", p.display()));
        }
    }
    s
}

/// 削除してよい対象かを **1 件ずつ、消す直前に** 判定する純関数。
///
/// 通すのは (1) 実行ファイル自身 (2) `data_dir` そのものか配下 の 2 系統だけ。
/// ここを通らないものは消さずに中止する — `~` やルートを巻き込む経路を
/// 構造的に残さないため。相対パスや `..` 混じりは「判定できない」ので拒否する。
pub fn removal_is_safe(target: &Path, exe: &Path, data_dir: &Path) -> bool {
    use std::path::Component;
    let sane = |p: &Path| {
        p.file_name().is_some()
            && p.parent().is_some()
            && !p
                .components()
                .any(|c| matches!(c, Component::ParentDir | Component::CurDir))
    };
    // 入れ物側が壊れている (ルート直下・相対) なら、何一つ消さない。
    if !sane(data_dir) || !sane(target) {
        return false;
    }
    if target == exe {
        return sane(exe);
    }
    target.starts_with(data_dir)
}

/// 1 件消す。**必ず [`removal_is_safe`] を通してから**消す。
/// 既に無いものは成功扱い (何度実行しても同じ結果になるようにする)。
fn remove_path_guarded(target: &Path, exe: &Path, data_dir: &Path) -> Result<(), String> {
    if !removal_is_safe(target, exe, data_dir) {
        return Err("削除対象として安全でないため消しませんでした".into());
    }
    let Ok(meta) = std::fs::symlink_metadata(target) else {
        return Ok(());
    };
    // シンボリックリンクは「リンクだけ」消す (リンク先を巻き込まない)。
    if meta.is_dir() && !meta.file_type().is_symlink() {
        std::fs::remove_dir_all(target).map_err(|e| e.to_string())
    } else {
        std::fs::remove_file(target).map_err(|e| e.to_string())
    }
}

/// 実行ファイル自身を消す。
///
/// **Windows は実行中の exe を削除できない**ので、隣へ改名して案内に切り替える
/// (削除は不可でも改名は可能 — install.ps1 の差し替えと同じ手口)。
fn remove_self(exe: &Path, data_dir: &Path) -> Result<String, String> {
    if !removal_is_safe(exe, exe, data_dir) {
        return Err("実行ファイルの場所を確定できないため消しませんでした".into());
    }
    match std::fs::remove_file(exe) {
        Ok(()) => Ok(format!("削除しました: {}", exe.display())),
        Err(e) if cfg!(windows) => {
            let old = exe.with_extension("old");
            match std::fs::rename(exe, &old) {
                Ok(()) => Ok(format!(
                    "実行中のため {} へ改名しました (サインインし直した後に削除してください)",
                    old.display()
                )),
                Err(e2) => Err(format!("{}: {e} / 改名も失敗しました: {e2}", exe.display())),
            }
        }
        Err(e) => Err(format!("{}: {e}", exe.display())),
    }
}

/// `zai uninstall` のディスパッチ。
fn uninstall_dispatch(args: &[String]) -> CliOut {
    if wants_help(args) {
        return Ok(HELP_UNINSTALL.trim_end().to_string());
    }
    const USAGE: &str = "zai uninstall [--dry-run] [--keep-config] [--yes|-y]";
    let (dry_run, rest) = take_flag(args, "--dry-run");
    let (keep_config, rest) = take_flag(&rest, "--keep-config");
    let (yes_long, rest) = take_flag(&rest, "--yes");
    let (yes_short, rest) = take_flag(&rest, "-y");
    reject_extra(&rest, USAGE)?;
    let yes = yes_long || yes_short;

    let exe = resolve_exe()?;
    // `~/.zaivern` は config から導く。存在するなら canonicalize して
    // `./.zaivern` フォールバックでも絶対パスで安全判定できるようにする。
    let data_dir = {
        let d = zaivern_dir();
        d.canonicalize().unwrap_or(d)
    };
    let plan = build_uninstall_plan(&exe, &data_dir, keep_config, other_binaries_on_path(&exe));
    println!("{}", render_uninstall_plan(&plan, dry_run));
    if dry_run {
        return Ok("(--dry-run のため何も消していません)".into());
    }
    if !yes && !confirm("本当に削除しますか? [y/N]: ") {
        return Ok("中止しました。".into());
    }

    // OS のアプリ登録は実行ファイルより先に外す。
    // 後だと .app / .desktop / .lnk が実体を失ったまま残る。失敗しても続行。
    let _ = crate::desktop::run(&["uninstall".to_string()]);

    let mut failed: Vec<String> = Vec::new();
    for t in plan.removals() {
        if let Err(e) = remove_path_guarded(&t, &exe, &data_dir) {
            failed.push(format!("  {} — {e}", t.display()));
        }
    }
    // 実行ファイルは最後 (消した後に他の削除へ進まない)。
    let self_note = match remove_self(&exe, &data_dir) {
        Ok(msg) => msg,
        Err(e) => {
            failed.push(format!("  {e}"));
            String::new()
        }
    };

    if !failed.is_empty() {
        return Err(CliError::Runtime(format!(
            "一部を削除できませんでした (権限を確認して手で消してください):\n{}",
            failed.join("\n")
        )));
    }
    let mut out = String::from("✅ アンインストールしました。");
    if !self_note.is_empty() {
        out.push_str(&format!("\n{self_note}"));
    }
    if keep_config {
        out.push_str(&format!("\n設定は残しました: {}", data_dir.display()));
    }
    if !plan.others.is_empty() {
        out.push_str("\n⚠ PATH 上に残った zai は手で削除してください:");
        for p in &plan.others {
            out.push_str(&format!("\n  {}", p.display()));
        }
    }
    Ok(out)
}

// ───────────────────────── テスト ─────────────────────────

#[cfg(test)]
mod tests {
    // ── zai lang (言語パックの導入) ────────────────────────────────

    #[test]
    fn 配布元の指定はowner_repoの形だけ通す() {
        assert_eq!(
            validate_slug("tacyan/zaivern-code").unwrap(),
            "tacyan/zaivern-code"
        );
        assert_eq!(validate_slug("a_b/c.d-e").unwrap(), "a_b/c.d-e");
        // URL や空要素、余計な階層は通さない (そのまま URL へ埋めるため)
        for bad in [
            "https://github.com/a/b",
            "a/b/c",
            "a/",
            "/b",
            "a b/c",
            "a/b?x=1",
            "../../etc",
            "",
        ] {
            assert!(validate_slug(bad).is_err(), "{bad:?} を通してはいけない");
        }
    }

    #[test]
    fn 配布元の一覧はjsonから言語idと取得urlを取る() {
        let body = r#"[
          {"name":"en.json","download_url":"https://raw.example/en.json"},
          {"name":"zh_CN.json","download_url":"https://raw.example/zh_CN.json"},
          {"name":"README.md","download_url":"https://raw.example/README.md"},
          {"name":"ko.json","download_url":"http://insecure/ko.json"},
          {"name":"fr.json"}
        ]"#;
        let got = parse_lang_index(body, "o/r").unwrap();
        // .json 以外は落ちる / http は落ちる / download_url が無いものも落ちる
        // ファイル名は正規化されて zh_CN → zh-CN になる
        assert_eq!(
            got,
            vec![
                ("en".to_string(), "https://raw.example/en.json".to_string()),
                (
                    "zh-CN".to_string(),
                    "https://raw.example/zh_CN.json".to_string()
                ),
            ]
        );
    }

    #[test]
    fn 配布元の応答が配列でなければエラー() {
        assert!(parse_lang_index(r#"{"message":"Not Found"}"#, "o/r").is_err());
        assert!(parse_lang_index("not json", "o/r").is_err());
        // 空の locales/ は「0 件」であってエラーではない
        assert_eq!(parse_lang_index("[]", "o/r").unwrap().len(), 0);
    }

    #[test]
    fn 入っていない言語へは切り替えない() {
        // 同梱にもユーザー置き場にも無い言語は断る (書けても効かないため)
        let e = lang_set("xx").unwrap_err();
        assert!(
            e.contains("zai lang install"),
            "入れ方を案内していない: {e}"
        );
        // 引数無しも断る
        assert!(lang_set("").is_err());
    }

    #[test]
    fn 入っていない言語は消せない() {
        let e = lang_remove("xx").unwrap_err();
        assert!(!e.is_empty());
        assert!(lang_remove("").is_err());
    }

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
            "lease",
            "update",
            "uninstall",
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
            // ファイル所有リース (GUI 無しでも設定できる導線)
            "zai lease status",
            "zai lease enable",
            "zai lease list",
            "zai lease claim",
            "zai lease release",
            // 自分自身の更新・削除
            "zai update",
            "zai update --check",
            "zai uninstall",
            "zai uninstall --dry-run",
            "zai uninstall --keep-config",
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

    /// **門が受け付ける語は、全部ヘルプに出ていること。**
    ///
    /// `czero` / `coedit` / `mesh` / `negotiate` の 4 つは**動くのに
    /// `zai help` に 1 文字も出ていなかった** — 導入コマンドが CLI から
    /// 発見できないという、いちばん惜しい壊れ方をしていた。
    /// 門 (`is_cli_subcommand`) へ足したのにヘルプへ書き忘れる、を構造で禁じる。
    #[test]
    fn 全サブコマンドがヘルプに出ている() {
        // 門そのものを引く (一覧を写経すると必ずずれる)
        const WORDS: &[&str] = &[
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
            "lease",
            "hook",
            "guard",
            "train",
            "split",
            "coedit",
            "mesh",
            "negotiate",
            "czero",
            "merge-driver",
            "update",
            "uninstall",
            "i18n",
            "lang",
        ];
        let help = help_text();
        let mut missing = Vec::new();
        for w in WORDS {
            // 門が本当にその語を受けることを先に確かめる (表が腐っていないか)
            assert!(is_cli_subcommand(w), "門が {w} を受けていない (表が古い)");
            if !help.contains(&format!("zai {w}")) {
                missing.push(*w);
            }
        }
        assert!(
            missing.is_empty(),
            "動くのに zai help に出ていないサブコマンド: {missing:?}"
        );
    }

    /// 個別ヘルプは全体ヘルプの一部でなければならない (二重管理で食い違わせない)。
    #[test]
    fn per_command_help_is_a_slice_of_the_full_help() {
        let help = help_text();
        for section in [
            HELP_WORKTREE,
            HELP_SESSION,
            HELP_AGENT,
            HELP_LEASE,
            HELP_GUARD,
            HELP_CZERO,
            HELP_TRAIN_SPLIT,
            HELP_UPDATE,
            HELP_UNINSTALL,
        ] {
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
        // update / uninstall は --help だけでネットワークにも fs にも触らない
        // (ヘルプを見ただけで削除の確認が走ったら事故なので、ここで固定する)。
        assert_eq!(
            update_dispatch(&v(&["--help"])),
            Ok(HELP_UPDATE.trim_end().to_string())
        );
        assert_eq!(
            uninstall_dispatch(&v(&["-h"])),
            Ok(HELP_UNINSTALL.trim_end().to_string())
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

    /// `zai lease` が **GUI 無しで**一巡すること
    /// (有効化 → 確保 → 一覧 → 競合で拒否 → 解放)。
    ///
    /// 台帳の場所は `~/.zaivern` 由来なので、HOME を差し替えられない
    /// このテストでは `lease` 側の純粋 API を同じ順序で叩いて経路を担保し、
    /// CLI 側は「引数の解釈と表示」だけを見る。
    #[test]
    fn lease_サブコマンドの引数解釈と表示() {
        use crate::lease;
        // 不明なサブコマンドは使い方エラー (終了コード 2)
        assert_eq!(finish(lease_dispatch(&v(&["ないよ"]))), EXIT_USAGE);
        // サブコマンド無しも使い方エラー
        assert_eq!(finish(lease_dispatch(&[])), EXIT_USAGE);
        // --help はヘルプ本文の一部
        let h = lease_dispatch(&v(&["--help"])).expect("ヘルプ");
        assert!(help_text().contains(h.trim_end()));
        // 一覧の描画 (純粋関数)
        let mut st = lease::Store::default();
        assert!(render_leases(&st, 0, false).contains("ありません"));
        lease::try_claim(
            &mut st,
            &lease::Holder {
                agent: "A".into(),
                session: "s".into(),
                cwd: "/w".into(),
                pid: 0,
            },
            &["src/**".to_string()],
            0,
            600,
            &|_| false,
        );
        let table = render_leases(&st, 0, false);
        assert!(table.contains("src/**") && table.contains('A'), "{table}");
        let json = render_leases(&st, 0, true);
        assert!(json.contains("\"patterns\""), "{json}");
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
        // update / uninstall は「動詞」であってフォルダ名ではない。
        // 譲ってしまうと ./update があるだけで更新が GUI 起動にすり替わる。
        for w in [
            "open",
            "prompt",
            "run",
            "state",
            "plugin",
            "status",
            "update",
            "uninstall",
        ] {
            assert!(!yields_to_directory(w), "{w} は譲らない");
        }
    }

    // ── update: 配布元の URL は install.sh / install.ps1 が単一の真実 ──

    // ── 日本語 README は英語版から遅れない ──────────────────────────

    /// **節が片方にしか無い状態を許さない。**
    ///
    /// 実際に英語版だけに `## Resource Use` (Zed との実測比較) があり、
    /// 日本語版には 1 行も無かった。読む人の言語で内容が変わるのは、
    /// 「測っていないことは書かない」と同じくらい避けたい嘘なので、
    /// **見出しの階層の並び**で釘を刺す (本文の長さは言語で当然変わるため見ない)。
    #[test]
    fn 二つのreadmeは同じ節を持つ() {
        let levels = |src: &str| -> Vec<usize> {
            src.replace("\r\n", "\n")
                .lines()
                .filter_map(|l| {
                    let n = l.len() - l.trim_start_matches('#').len();
                    let head = (2..=3).contains(&n) && l[n..].starts_with(' ');
                    head.then_some(n)
                })
                .collect()
        };
        let en = levels(include_str!("../README.md"));
        let ja = levels(include_str!("../README.ja.md"));
        assert_eq!(
            en.len(),
            ja.len(),
            "節の数が違う (英語 {} / 日本語 {}) — 片方にしか無い節がある",
            en.len(),
            ja.len()
        );
        assert_eq!(en, ja, "見出しの階層の並びが違う");
    }

    // ── 検証スクリプトは「読める形」で結果を言う ────────────────────

    /// 検証コマンドの結果を**終了コードだけ**に持たせない。
    ///
    /// `tools/verify.sh … | tail` のようにパイプを挟むと `$?` は tail のものに
    /// なるので、**中止したのに rc=0** に見える (実際にこれで「docker が
    /// 起動していないのに緑」と読み違えた)。どの経路で終わっても最後の 1 行に
    /// 判定を書くこと。`exec` で置き換えると EXIT の trap が発火しないので、
    /// そこも併せて禁じる。
    #[test]
    fn 検証スクリプトは終了時に判定行を出す() {
        for (name, src) in [
            ("tools/verify.sh", include_str!("../tools/verify.sh")),
            (
                "tools/linux-test.sh",
                include_str!("../tools/linux-test.sh"),
            ),
            (
                "tools/windows-check.sh",
                include_str!("../tools/windows-check.sh"),
            ),
        ] {
            // Windows のチェックアウトは CRLF なので正規化してから探す。
            let sh = src.replace("\r\n", "\n");
            // コメントアウトを緑にしないため、**行そのもの**で照合する。
            assert!(
                sh.lines().any(|l| l.trim() == "trap _verdict EXIT"),
                "{name}: 終了時の判定行が無い (パイプ越しに嘘の緑が出る)"
            );
            assert!(sh.contains("_LABEL="), "{name}: 判定行のラベルが無い");
            // **行頭固定では足りない。** 実際に `    exec env … cargo xwin check`
            // (字下げ + env 経由) がこの検査をすり抜けていて、
            // `tools/windows-check.sh` は判定行を 1 行も出していなかった。
            // 字下げを落として**行の先頭語**で見る。
            let bad: Vec<&str> = sh
                .lines()
                .map(str::trim)
                .filter(|l| l.starts_with("exec ") && !l.starts_with("exec wine"))
                .collect();
            assert!(
                bad.is_empty(),
                "{name}: exec でプロセスを置き換えると判定行が出ない: {bad:?}"
            );
        }
    }

    // ── 供給網: インストーラは「展開する前に」SHA-256 を突き合わせる ──

    /// release.yml が checksums.txt を作っていても、**インストーラが見ていなければ
    /// 意味が無い**。しかも検証は *展開の前* でなければならない — tar / Expand-Archive
    /// が中身を書き出した後で気付いても、そのファイルはもうディスク上にある。
    ///
    /// ここは「呼んでいるか」だけでなく **順序** を固定する。
    #[test]
    fn インストーラは展開前にsha256を検証する() {
        // Windows のチェックアウトは CRLF なので改行を正規化してから探す。
        let sh = INSTALL_SH.replace("\r\n", "\n");
        let ps1 = INSTALL_PS1.replace("\r\n", "\n");

        for (name, src, tokens) in [
            (
                "install.sh",
                &sh,
                ["checksums.txt", "verify_checksum", "abort_unverified"].as_slice(),
            ),
            (
                "install.ps1",
                &ps1,
                ["checksums.txt", "Test-Checksum", "Get-FileHash"].as_slice(),
            ),
        ] {
            for t in tokens {
                assert!(src.contains(t), "{name} に {t} が無い (検証していない)");
            }
        }

        // 順序: 検証の呼び出し < 展開の呼び出し。
        let verify = sh
            .find(r#"verify_checksum "$tmp/$base""#)
            .expect("sh: 検証の呼び出し");
        let extract = sh.find("tar xzf").expect("sh: 展開");
        assert!(verify < extract, "install.sh: tar xzf の前に検証していない");

        let verify = ps1.find("Test-Checksum $zip").expect("ps1: 検証の呼び出し");
        // **実際の呼び出し行**を探す。素の "Expand-Archive" だと、なぜ展開前に
        // 検証するのかを説明した**コメント**が先に当たって常に失敗する
        // (「検証していない」と嘘の赤を出す)。
        let extract = ps1
            .find("Expand-Archive $zip -DestinationPath")
            .expect("ps1: 展開の呼び出し");
        assert!(
            verify < extract,
            "install.ps1: Expand-Archive の前に検証していない"
        );
    }

    /// 検証できなかったときに **黙って続けない** こと (fail-closed)。
    /// 「checksums.txt が取れなかったので素通しした」は、検証していないのと同じ。
    #[test]
    fn 検証できないときは中止する() {
        let sh = INSTALL_SH.replace("\r\n", "\n");
        let ps1 = INSTALL_PS1.replace("\r\n", "\n");
        assert!(
            sh.contains("|| abort_unverified \"checksums.txt を取得できませんでした\""),
            "install.sh: checksums.txt を取れなかったときに中止していない"
        );
        assert!(
            sh.contains("exit 1"),
            "install.sh: abort_unverified が終了していない"
        );
        assert!(
            ps1.contains("$script:zaiGiveUp = $true   # 検証に失敗した以上"),
            "install.ps1: 検証失敗後にソースビルドへ降りてしまう"
        );
    }

    #[test]
    fn 配布元は付属インストーラから導出できる() {
        let d = distribution_from(INSTALL_SH, INSTALL_PS1).expect("配布元を導出できるべき");
        assert_eq!(
            d.slug.split('/').count(),
            2,
            "slug は owner/name: {}",
            d.slug
        );
        for url in [
            &d.repo_url,
            &d.latest_api,
            &d.installer_sh,
            &d.installer_ps1,
        ] {
            assert!(url.starts_with("https://"), "https で始まるべき: {url}");
            assert!(!url.contains('$'), "変数が残っている: {url}");
        }
        assert!(d.repo_url.ends_with(&d.slug), "{}", d.repo_url);
        assert!(d.latest_api.contains(&d.slug), "{}", d.latest_api);
        assert!(d.installer_sh.ends_with("/install.sh"));
        assert!(d.installer_ps1.ends_with("/install.ps1"));
    }

    /// `zai update` が案内するコマンドは、README のワンライナーと**同一**でなければ
    /// ならない。片方だけ直ると「案内どおりにしたのに入らない」が起きる。
    #[test]
    fn 更新コマンドは_readme_のワンライナーと一致する() {
        let d = distribution_from(INSTALL_SH, INSTALL_PS1).expect("配布元");
        // Windows のチェックアウトは CRLF なので改行を正規化してから探す。
        let readmes = [
            include_str!("../README.md").replace("\r\n", "\n"),
            include_str!("../README.ja.md").replace("\r\n", "\n"),
        ];
        let sh = update_command_line(UpdateMethod::Shell, &d);
        let ps = update_command_line(UpdateMethod::PowerShell, &d);
        for r in &readmes {
            assert!(r.contains(&sh), "README に無い: {sh}");
            assert!(r.contains(&ps), "README に無い: {ps}");
        }
    }

    #[test]
    fn repo_slug_は_url_行を拾わない() {
        let sh = "# https://raw.githubusercontent.com/o/n/main/install.sh\n\
                  REPO=\"owner/name\"\nREPO_URL=\"https://github.com/$REPO\"\n";
        assert_eq!(parse_repo_slug(sh).as_deref(), Some("owner/name"));
        let ps1 = "$repo = \"owner/name\"\n$repoUrl = \"https://github.com/$repo\"\n";
        assert_eq!(parse_repo_slug(ps1).as_deref(), Some("owner/name"));
        // 形が違えば拾わない (中途半端な URL を案内しないため)
        assert_eq!(parse_repo_slug("REPO=\"https://x/y/z\"\n"), None);
        assert_eq!(parse_repo_slug("# REPO=\"owner/name\"\n"), None);
    }

    #[test]
    fn バージョン比較は数値順() {
        assert_eq!(parse_version("v0.8.0"), [0, 8, 0]);
        assert_eq!(parse_version("0.8.10-rc1"), [0, 8, 10]);
        assert_eq!(parse_version("1.2"), [1, 2, 0]);
        assert_eq!(parse_version("なんだこれ"), [0, 0, 0]);
        for (latest, current, expect) in [
            ("v0.8.1", "0.8.0", true),
            ("v0.9.0", "0.8.99", true),
            ("v1.0.0", "0.9.9", true),
            ("v0.8.0", "0.8.0", false),
            ("v0.7.9", "0.8.0", false),
            // 文字列比較なら "0.10.0" < "0.9.0" になる — 数値順であることの番人
            ("v0.10.0", "0.9.0", true),
        ] {
            assert_eq!(
                version_is_newer(latest, current),
                expect,
                "{latest} vs {current}"
            );
        }
    }

    #[test]
    fn 更新手段は実行ファイルの置き場所で決まる() {
        let cargo_bin = PathBuf::from("/opt/cargo/bin");
        let in_cargo = cargo_bin.join("zai");
        let elsewhere = PathBuf::from("/opt/local/bin/zai");
        // cargo install で入れた形跡があれば OS を問わず cargo
        for windows in [false, true] {
            assert_eq!(
                choose_update_method(&in_cargo, Some(&cargo_bin), windows),
                UpdateMethod::Cargo
            );
        }
        assert_eq!(
            choose_update_method(&elsewhere, Some(&cargo_bin), false),
            UpdateMethod::Shell
        );
        assert_eq!(
            choose_update_method(&elsewhere, Some(&cargo_bin), true),
            UpdateMethod::PowerShell
        );
        // CARGO_HOME が取れない環境でも両側が決まる
        assert_eq!(
            choose_update_method(&elsewhere, None, true),
            UpdateMethod::PowerShell
        );
        assert_eq!(
            choose_update_method(&elsewhere, None, false),
            UpdateMethod::Shell
        );
    }

    #[test]
    fn 更新コマンドは手段ごとに違う形になる() {
        let d = distribution_from(INSTALL_SH, INSTALL_PS1).expect("配布元");
        let cargo = update_command_line(UpdateMethod::Cargo, &d);
        assert!(cargo.starts_with("cargo install --git "), "{cargo}");
        assert!(
            cargo.contains(&d.repo_url) && cargo.contains("--force"),
            "{cargo}"
        );
        assert!(cargo.ends_with(env!("CARGO_PKG_NAME")), "{cargo}");
        assert!(update_command_line(UpdateMethod::Shell, &d).starts_with("curl -fsSL "));
        assert!(update_command_line(UpdateMethod::PowerShell, &d).starts_with("irm "));
    }

    #[test]
    fn update_の引数ミスは終了コード2() {
        assert_eq!(
            update_dispatch(&v(&["いらない引数"])).map_err(|e| e.code()),
            Err(EXIT_USAGE)
        );
        assert_eq!(
            uninstall_dispatch(&v(&["いらない引数"])).map_err(|e| e.code()),
            Err(EXIT_USAGE)
        );
    }

    // ── uninstall: 消す前の一覧と、消して良い対象の検証 ──

    #[test]
    fn サイズ表記は桁ごとに単位が変わる() {
        for (bytes, expect) in [
            (0u64, "0 B"),
            (999, "999 B"),
            (1024, "1.0 KB"),
            (1536, "1.5 KB"),
            (20 * 1024, "20 KB"),
            (5 * 1024 * 1024, "5.0 MB"),
            (3 * 1024 * 1024 * 1024, "3.0 GB"),
        ] {
            assert_eq!(human_size(bytes), expect, "{bytes}");
        }
    }

    /// 一時ディレクトリに `~/.zaivern` を模した構造を作る (実 HOME には触らない)。
    fn fake_install(tag: &str) -> (PathBuf, PathBuf, PathBuf) {
        let root = crate::test_util::unique_temp_dir("zaivern-cli-uninstall", tag);
        let bin = root.join("bin");
        std::fs::create_dir_all(&bin).expect("bin");
        let exe = bin.join("zai");
        std::fs::write(&exe, vec![0u8; 2048]).expect("exe");
        let data = root.join(".zaivern");
        std::fs::create_dir_all(data.join("sessions")).expect("sessions");
        std::fs::create_dir_all(data.join("term_logs")).expect("term_logs");
        std::fs::write(data.join("config.toml"), vec![b'x'; 100]).expect("config");
        std::fs::write(data.join("state.toml"), vec![b'x'; 50]).expect("state");
        std::fs::write(data.join("sessions/a.toml"), vec![b'x'; 300]).expect("session");
        std::fs::write(data.join("term_logs/a.log"), vec![b'x'; 700]).expect("log");
        (root, exe, data)
    }

    #[test]
    fn 削除計画は内訳とサイズを出す() {
        let (_root, exe, data) = fake_install("plan");
        let plan = build_uninstall_plan(&exe, &data, false, Vec::new());
        let labels: Vec<&str> = plan.entries.iter().map(|e| e.label.as_str()).collect();
        assert_eq!(
            labels,
            ["config.toml", "sessions/", "state.toml", "term_logs/"]
        );
        assert_eq!(plan.exe_size, 2048);
        assert_eq!(plan.total(), 2048 + 100 + 300 + 50 + 700);
        let text = render_uninstall_plan(&plan, true);
        assert!(text.contains("--dry-run"), "{text}");
        for needle in ["config.toml", "sessions/", "term_logs/", "消える合計サイズ"] {
            assert!(text.contains(needle), "一覧に {needle} が無い:\n{text}");
        }
    }

    #[test]
    fn keep_config_は設定だけ残して合計から外す() {
        let (_root, exe, data) = fake_install("keep");
        let plan = build_uninstall_plan(&exe, &data, true, Vec::new());
        let kept: Vec<&str> = plan
            .entries
            .iter()
            .filter(|e| e.keep)
            .map(|e| e.label.as_str())
            .collect();
        assert_eq!(kept, ["config.toml", "state.toml"]);
        assert_eq!(plan.total(), 2048 + 300 + 700);
        // 設定を残すのだから、入れ物ごとの削除は計画に入らない
        assert!(!plan.removals().contains(&data));
        assert!(render_uninstall_plan(&plan, false).contains("--keep-config のため残します"));
    }

    #[test]
    fn 安全判定は実行ファイルとデータ配下だけを通す() {
        let home = PathBuf::from("/home/someone");
        let data = home.join(".zaivern");
        let exe = home.join(".local/bin/zai");
        // 通るもの
        for ok in [
            exe.clone(),
            data.clone(),
            data.join("sessions"),
            data.join("term_logs/a.log"),
        ] {
            assert!(
                removal_is_safe(&ok, &exe, &data),
                "{} は通るべき",
                ok.display()
            );
        }
        // 通してはいけないもの
        for ng in [
            home.clone(),
            PathBuf::from("/"),
            home.join(".zaivern-backup"),
            home.join(".local/bin/other"),
            home.join(".ssh"),
            PathBuf::from("relative/path"),
            data.join("../../etc"),
        ] {
            assert!(
                !removal_is_safe(&ng, &exe, &data),
                "{} は拒否すべき",
                ng.display()
            );
        }
        // 入れ物側が壊れていたら (ルート・相対) 何一つ消さない
        assert!(!removal_is_safe(&data, &exe, Path::new("/")));
        assert!(!removal_is_safe(&data, &exe, Path::new(".zaivern")));
    }

    #[test]
    fn ガード付き削除は対象外に触らない() {
        let (root, exe, data) = fake_install("remove");
        // データ配下は消える
        let sessions = data.join("sessions");
        assert!(remove_path_guarded(&sessions, &exe, &data).is_ok());
        assert!(!sessions.exists());
        // 対象外は「消えない」だけでなくエラーになる (黙って見逃さない)
        let outsider = root.join("大事なファイル");
        std::fs::write(&outsider, b"keep me").expect("outsider");
        assert!(remove_path_guarded(&outsider, &exe, &data).is_err());
        assert!(outsider.exists(), "対象外を消してしまった");
        assert!(remove_path_guarded(&root, &exe, &data).is_err());
        assert!(root.exists());
        // 既に無いものは成功扱い (何度でも実行できる)
        assert!(remove_path_guarded(&sessions, &exe, &data).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 計画どおりに消すとデータと実行ファイルだけが消える() {
        let (root, exe, data) = fake_install("apply");
        let outsider = root.join("bin/他のツール");
        std::fs::write(&outsider, b"keep me").expect("outsider");
        let plan = build_uninstall_plan(&exe, &data, false, Vec::new());
        for t in plan.removals() {
            remove_path_guarded(&t, &exe, &data).expect("削除できるべき");
        }
        assert!(remove_self(&exe, &data).is_ok());
        assert!(!data.exists(), "~/.zaivern 相当が残っている");
        assert!(!exe.exists(), "実行ファイルが残っている");
        assert!(outsider.exists(), "無関係なファイルを消してしまった");
        let _ = std::fs::remove_dir_all(&root);
    }
}
