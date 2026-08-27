//! `zai cloud …` — Cloud Execution の **CLI アダプタ**。
//!
//! ここは薄い皮で、引数を写して結果を印字するだけ。**判断はコアが持つ**
//! (どの実行先を選ぶか・消してよいか・秘密を伏せるか)。
//!
//! ## 終了コード (§43)
//!
//! | 値 | 意味 |
//! |---|---|
//! | 0 | 成功 |
//! | 1 | 実行時 / Provider / リモートの失敗 |
//! | 2 | 使い方の誤り |
//! | 3 | 認証・設定の誤り |
//! | 4 | 条件に合う実行先が無い |
//!
//! `--json` を付けたときは、**標準出力に JSON 以外を混ぜない** (人向けの
//! 装飾は標準エラーへ)。機械から読めなくなるため。

use std::path::PathBuf;
use std::time::Duration;

use serde_json::json;

use super::model::{CloudError, ExecutionTarget, TargetLifecycle, TransportKind};
use super::provider::static_ssh::{make_target, SshTargetSpec};
use super::provider::{ProviderKind, ProviderProfile, ProvisionSpec};
use super::registry::{all_profiles, Registry};
use super::{command::LaunchSpec, runner, scheduler, store};

const EXIT_OK: i32 = 0;
const EXIT_ERR: i32 = 1;
const EXIT_USAGE: i32 = 2;
const EXIT_CONFIG: i32 = 3;
const EXIT_NO_TARGET: i32 = 4;

/// `zai cloud --help` の本文。
pub const HELP: &str = "\
cloud (クラウド実行 — どのマシン・どのクラウドでも同じように仕事を走らせます):
  zai cloud doctor [--json]             使える道具・置き場・登録済みの一覧を診断
  zai cloud target list [--json]        実行先の一覧
  zai cloud target add ssh --name <名前> --host <ホスト> --user <ユーザー>
                 [--port N] [--identity-file <パス>] [--max-jobs N]
                                        SSH で入れる Linux を実行先として登録
  zai cloud target probe <名前>          届くか・何者かを確かめて台帳を更新
  zai cloud target trust <名前> [--yes]  相手の公開鍵の指紋を見せ、--yes で記録する
                                        (最初の接続の前に 1 度だけ必要です)
  zai cloud target remove <名前>         一覧から外す (機械には触りません)
  zai cloud exec --target <名前|auto> -- <コマンド...>
                                        実行先でコマンドを 1 回走らせる
  zai cloud shell <名前>                 対話シェルを開く
  zai cloud launch --target <名前|auto> [--command \"<コマンド行>\" | -- <コマンド...>]
                 [--cwd <リモートのパス>] [--run]
                                        エージェントをリモートで起動する 1 行を作る
                                        (エージェント設定の command に貼れます。--run でその場実行)
  zai cloud copy --target <名前> <手元のファイル> <リモートのパス>
  zai cloud copy --target <名前> --from <リモートのパス> <手元のファイル>
                                        ファイルを 1 つ送る / 受け取る
  zai cloud job run --target <名前|auto> [--timeout <秒>] -- <コマンド...>
                                        リモートに worktree を作って走らせ、結果を持ち帰る
  zai cloud job list [--json] [--limit N]
                                        仕事の記録
  zai cloud provider list [--json]      Provider プロファイルの一覧
  zai cloud provider add hetzner --name <名前> [--location fsn1] [--token-env HCLOUD_TOKEN]
                 [--server-type cx33] [--image ubuntu-24.04] [--ssh-key <鍵の名前>]
                 [--ssh-user zaivern] [--max-jobs N]
                                        API で VM を作れる Provider を登録
                                        (API トークンは環境変数 HCLOUD_TOKEN から読みます)
  zai cloud provider remove <名前>       Provider プロファイルを外す
  zai cloud provider types <名前> [--json]
                                        使えるサーバー種別と**その時点の**費用 (API から取得)
  zai cloud provider locations <名前> [--json]
                                        使える場所
  zai cloud worker create --provider <名前> --name <名前> [--server-type ...]
                 [--location ...] [--image ...] [--ssh-key ...] [--max-jobs N] [--wait]
                                        VM を作る (**課金されます**。明示操作でのみ実行)
  zai cloud worker destroy <名前> [--yes]
                                        Zaivern が作った VM を消す

  実行先の条件 (exec / job run / launch / copy で使えます):
    --os linux|macos|windows / --arch x86_64|aarch64 / --min-cpu N
    --min-memory-mib N / --gpu / --tool <道具名> / --label <札>

  終了コード: 0 = 成功 / 1 = 実行時エラー / 2 = 使い方の誤り /
              3 = 認証・設定の誤り / 4 = 条件に合う実行先が無い
  秘密は保存も表示もしません (保存するのは環境変数の名前とパスだけ)。
  --target auto は**すでに在る Ready な実行先から選ぶ**だけで、VM を勝手に作りません。
";

/// `zai cloud <sub>` の実体。argv は `\"cloud\"` の**次**から渡される。
pub fn cli_main(argv: &[String]) -> i32 {
    if argv.is_empty() || argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", HELP.trim_end());
        return EXIT_OK;
    }
    match run(&argv[0], &argv[1..]) {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
            EXIT_OK
        }
        Err(Fail::Usage(m)) => {
            eprintln!("{m}\n\n{}", HELP.trim_end());
            EXIT_USAGE
        }
        Err(Fail::Cloud(e)) => {
            eprintln!("{e}");
            match e.exit_code() {
                3 => EXIT_CONFIG,
                4 => EXIT_NO_TARGET,
                _ => EXIT_ERR,
            }
        }
        Err(Fail::Code(code, m)) => {
            if !m.is_empty() {
                eprintln!("{m}");
            }
            code
        }
    }
}

/// 失敗の理由。**使い方の誤りと実行時エラーを混ぜない** (終了コードが違う)。
///
/// `Debug` は手で書く — 導出すると [`CloudError`] の中身が生で出て、
/// あちらで畳んだ秘密がこちらから漏れる。
enum Fail {
    Usage(String),
    Cloud(CloudError),
    Code(i32, String),
}

impl std::fmt::Debug for Fail {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Usage(m) => write!(f, "Usage({m})"),
            // CloudError の Display は伏せた後の文字列しか出さない
            Self::Cloud(e) => write!(f, "Cloud({e})"),
            Self::Code(c, m) => write!(f, "Code({c}, {m})"),
        }
    }
}

impl From<CloudError> for Fail {
    fn from(e: CloudError) -> Self {
        Fail::Cloud(e)
    }
}

fn run(sub: &str, rest: &[String]) -> Result<String, Fail> {
    match sub {
        "doctor" => doctor(rest),
        "target" => target_cmd(rest),
        "exec" => exec_cmd(rest),
        "shell" => shell_cmd(rest),
        "launch" => launch_cmd(rest),
        "copy" => copy_cmd(rest),
        "job" => job_cmd(rest),
        "provider" => provider_cmd(rest),
        "worker" => worker_cmd(rest),
        other => Err(Fail::Usage(format!("知らないサブコマンドです: {other}"))),
    }
}

// ───────────────────────── 引数の解析 ─────────────────────────

/// `--key value` を 1 つ取り出す。**同じ鍵を 2 度書いたら後ろが勝つ**
/// (打ち間違えを黙って無視しない形にはしていない — 打ち直しは普通の操作)。
fn opt(args: &[String], key: &str) -> Option<String> {
    let mut out = None;
    let mut i = 0;
    while i < args.len() {
        if args[i] == key {
            out = args.get(i + 1).cloned();
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// 同じ鍵を何度でも受け取る (`--tool git --tool docker`)。
fn opts_all(args: &[String], key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == key {
            if let Some(v) = args.get(i + 1) {
                out.push(v.clone());
            }
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

fn flag(args: &[String], key: &str) -> bool {
    args.iter().any(|a| a == key)
}

fn opt_u16(args: &[String], key: &str, default: u16) -> Result<u16, Fail> {
    match opt(args, key) {
        None => Ok(default),
        Some(v) => v
            .parse()
            .map_err(|_| Fail::Usage(format!("{key} には数を指定してください: {v}"))),
    }
}

/// `--` の後ろを取り出す。
fn after_dashdash(args: &[String]) -> Option<Vec<String>> {
    let at = args.iter().position(|a| a == "--")?;
    Some(args[at + 1..].to_vec())
}

/// 位置引数 (`--` より前の、`--key` でないもの) を 1 つ取る。
fn positional(args: &[String]) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            return None;
        }
        if a.starts_with("--") {
            // 値を取る鍵なら 1 つ飛ばす
            if TAKES_VALUE.contains(&a.as_str()) {
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        return Some(a.clone());
    }
    None
}

/// 値を取る鍵の一覧。**ここに書き忘れると、値が位置引数として拾われる。**
const TAKES_VALUE: &[&str] = &[
    "--name",
    "--host",
    "--user",
    "--port",
    "--identity-file",
    "--max-jobs",
    "--target",
    "--provider",
    "--location",
    "--server-type",
    "--image",
    "--ssh-key",
    "--ssh-user",
    "--limit",
    "--timeout",
    "--os",
    "--arch",
    "--min-cpu",
    "--min-memory-mib",
    "--tool",
    "--label",
    "--token-env",
    "--command",
    "--from",
    // **`--cwd` を書き忘れると、その値が位置引数として拾われる**
    // (`copy` が「リモートのパス」だと思って別のファイルを触る)。
    "--cwd",
];

/// 位置引数を**全部**取る (`copy` の 2 つのパス)。
fn positionals(args: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if a == "--" {
            break;
        }
        if a.starts_with("--") {
            i += if TAKES_VALUE.contains(&a.as_str()) { 2 } else { 1 };
            continue;
        }
        out.push(a.clone());
        i += 1;
    }
    out
}

fn config() -> crate::config::Config {
    let root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    crate::config::load(std::slice::from_ref(&root), false)
}

fn registry() -> Result<Registry, CloudError> {
    let cfg = config();
    let mut reg = Registry::load(&cfg)?;
    // 組み込み (local / static-ssh) を必ず持たせる
    reg = Registry::with_ctx(
        all_profiles(reg.profiles()),
        super::provider::ProviderCtx::live(
            super::api_timeout(&cfg),
            super::default_max_jobs(&cfg),
        ),
        super::ssh_timeout(&cfg),
    );
    Ok(reg)
}

// ───────────────────────── doctor ─────────────────────────

/// 診断。**読めなかったことを「0 件」に化けさせない** (P2-2)。
///
/// 状態ファイルが壊れているのと、まだ何も登録していないのは**まったく違う**。
/// 前者で「0 件」と出すと、利用者は登録し直そうとして**壊れたファイルを
/// 上書きし、登録済みの内容を失う**。だから読めなかった段は件数を出さず、
/// 理由 (どのファイルか) を出して、診断そのものを不合格にする。
fn doctor(args: &[String]) -> Result<String, Fail> {
    let (text, ok) = doctor_report(args);
    if ok {
        return Ok(text);
    }
    // 本文は標準出力へ (機械が `--json` を読む経路を壊さない)。
    // 終了コードだけを不合格にする。
    if !text.is_empty() {
        println!("{text}");
    }
    Err(Fail::Code(EXIT_ERR, String::new()))
}

/// 診断の本文と合否。**読めなかった段があれば `false`。**
fn doctor_report(args: &[String]) -> (String, bool) {
    let as_json = flag(args, "--json");
    let cfg = config();
    // **`unwrap_or_default()` を使わない。** ここが P2-2 の急所で、
    // 「読めなかった」を握り潰すと下流はすべて 0 件に見える。
    let profiles = store::load_providers().map(|list| all_profiles(&list));
    let targets = store::load_targets().map(|list| {
        // 手元の行は枠を数えるためだけに台帳へ在る (registry::claim_local_slot)
        list.into_iter()
            .filter(|t| t.transport != TransportKind::Local)
            .collect::<Vec<_>>()
    });
    let jobs = store::load_jobs();
    let ok = profiles.is_ok() && targets.is_ok() && jobs.is_ok();
    let kh = store::known_hosts_path();

    let git = tool_version("git");
    let ssh = tool_version("ssh");

    if as_json {
        let v = json!({
            // **合否を 1 つの欄で出す。** 機械は段ごとに見なくてよい
            "ok": ok,
            "git": git,
            "ssh": ssh,
            "known_hosts": kh.display().to_string(),
            "known_hosts_exists": kh.exists(),
            "store": store::cloud_dir().display().to_string(),
            "providers": match &profiles {
                Ok(list) => json!({
                    "status": "ok",
                    "count": list.len(),
                    "items": list.iter().map(|p| json!({
                        "name": p.name,
                        "kind": p.kind.id(),
                        "token_env": p.token_env,
                        // **値は出さない。** あるか無いかだけ
                        "token_present": p.token_present(),
                    })).collect::<Vec<_>>(),
                }),
                Err(e) => read_error_json(e),
            },
            "targets": match &targets {
                Ok(list) => json!({"status": "ok", "count": list.len()}),
                Err(e) => read_error_json(e),
            },
            "jobs": match &jobs {
                Ok(list) => json!({
                    "status": "ok",
                    "count": list.len(),
                    "unfinished": runner::unfinished(list).len(),
                }),
                Err(e) => read_error_json(e),
            },
            "prefer": super::prefer(&cfg).id(),
            "ssh_timeout_secs": super::ssh_timeout(&cfg).as_secs(),
            "api_timeout_secs": super::api_timeout(&cfg).as_secs(),
        });
        return (serde_json::to_string_pretty(&v).unwrap_or_default(), ok);
    }

    let mut out = String::new();
    out.push_str("クラウド実行の診断\n\n");
    out.push_str(&format!(
        "  git                {}\n",
        git.as_deref().unwrap_or("見つかりません (必須)")
    ));
    out.push_str(&format!(
        "  ssh                {}\n",
        ssh.as_deref()
            .unwrap_or("見つかりません (SSH の実行先に必須)")
    ));
    out.push_str(&format!(
        "  置き場              {}\n",
        store::cloud_dir().display()
    ));
    out.push_str(&format!(
        "  known_hosts        {} ({})\n",
        kh.display(),
        if kh.exists() {
            "あり"
        } else {
            "まだありません"
        }
    ));
    out.push_str(&format!(
        "  待ち時間            SSH {} 秒 / API {} 秒\n",
        super::ssh_timeout(&cfg).as_secs(),
        super::api_timeout(&cfg).as_secs()
    ));
    out.push_str(&format!(
        "  実行先の好み        {}\n",
        super::prefer(&cfg).id()
    ));
    match &profiles {
        Ok(list) => {
            out.push_str(&format!("\n  Provider ({} 件)\n", list.len()));
            for p in list {
                let token = if p.token_env.is_empty() {
                    "―".to_string()
                } else if p.token_present() {
                    // **値は出さない。** 設定されているかどうかだけ
                    format!("{} = 設定あり", p.token_env)
                } else {
                    format!("{} = 未設定", p.token_env)
                };
                out.push_str(&format!("    {:<16} {:<12} {token}\n", p.name, p.kind.id()));
            }
        }
        Err(e) => out.push_str(&read_error_text("Provider", e)),
    }
    match &targets {
        Ok(list) => {
            out.push_str(&format!("\n  実行先 ({} 件 + local)\n", list.len()));
            for t in list {
                out.push_str(&format!(
                    "    {:<16} {:<10} {}\n",
                    t.name,
                    t.lifecycle.id(),
                    t.endpoint.summary()
                ));
            }
        }
        Err(e) => out.push_str(&read_error_text("実行先", e)),
    }
    match &jobs {
        Ok(list) => {
            let stuck = runner::unfinished(list);
            if !stuck.is_empty() {
                out.push_str(&format!(
                    "\n  ⚠ まだ終わっていない仕事が {} 件あります (zai cloud job list)\n",
                    stuck.len()
                ));
            }
        }
        Err(e) => out.push_str(&read_error_text("仕事の記録", e)),
    }
    if !ok {
        out.push_str(
            "\n  読めなかったファイルがあります。**登録し直す前に退避してください** —\n             \x20 上書きすると、読めていないだけの中身を失います。\n",
        );
    }
    (out, ok)
}

/// 読めなかった段の JSON。**件数は出さない** (出すと 0 件に見える)。
fn read_error_json(e: &CloudError) -> serde_json::Value {
    json!({"status": "error", "error": e.to_string()})
}

/// 読めなかった段の本文。
fn read_error_text(label: &str, e: &CloudError) -> String {
    format!(
        "\n  {label} (読めません)\n    {}\n",
        e.to_string().replace('\n', "\n    ")
    )
}

fn tool_version(program: &str) -> Option<String> {
    let out = crate::procx::hidden_command(program)
        .arg("--version")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    // ssh は版を標準エラーへ出す
    let text = if text.trim().is_empty() {
        String::from_utf8_lossy(&out.stderr).into_owned()
    } else {
        text.into_owned()
    };
    text.lines().next().map(|l| l.trim().to_string())
}

// ───────────────────────── target ─────────────────────────

fn target_cmd(args: &[String]) -> Result<String, Fail> {
    let sub = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| Fail::Usage("zai cloud target <list|add|probe|remove>".into()))?;
    let rest = &args[1..];
    match sub {
        "list" => target_list(rest),
        "add" => target_add(rest),
        "probe" => target_probe(rest),
        "remove" | "rm" => target_remove(rest),
        "trust" => target_trust(rest),
        other => Err(Fail::Usage(format!("知らない操作です: target {other}"))),
    }
}

fn target_json(t: &ExecutionTarget) -> serde_json::Value {
    json!({
        "id": t.id.as_str(),
        "name": t.name,
        "provider": t.provider.as_str(),
        "transport": t.transport.id(),
        "endpoint": t.endpoint.summary(),
        "lifecycle": t.lifecycle.id(),
        "os": t.capabilities.os.id(),
        "arch": t.capabilities.arch.id(),
        "cpu_cores": t.capabilities.cpu_cores,
        "memory_mib": t.capabilities.memory_mib,
        "max_jobs": t.capacity.max_jobs,
        "active_jobs": t.capacity.active_jobs,
        "managed": t.managed,
        "cost": t.billing.summary(),
    })
}

fn target_list(args: &[String]) -> Result<String, Fail> {
    let reg = registry()?;
    let targets = reg.targets()?;
    if flag(args, "--json") {
        let v: Vec<_> = targets.iter().map(target_json).collect();
        return Ok(serde_json::to_string_pretty(&v).unwrap_or_default());
    }
    let mut out = format!(
        "{:<16} {:<12} {:<10} {:<22} {:<9} {}\n",
        "名前", "PROVIDER", "状態", "接続先", "枠", "能力"
    );
    for t in &targets {
        let caps = match (t.capabilities.cpu_cores, t.capabilities.memory_mib) {
            (Some(c), Some(m)) => format!("{} / {c} core / {} GiB", t.capabilities.os.id(), m / 1024),
            _ => format!("{} (未確認)", t.capabilities.os.id()),
        };
        out.push_str(&format!(
            "{:<16} {:<12} {:<10} {:<22} {:<9} {caps}\n",
            t.name,
            t.provider.as_str(),
            t.lifecycle.id(),
            t.endpoint.summary(),
            format!("{}/{}", t.capacity.active_jobs, t.capacity.max_jobs),
        ));
    }
    Ok(out)
}

fn target_add(args: &[String]) -> Result<String, Fail> {
    let kind = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| Fail::Usage("zai cloud target add ssh --name … --host … --user …".into()))?;
    if kind != "ssh" {
        return Err(Fail::Usage(format!(
            "対応しているのは ssh だけです: {kind}"
        )));
    }
    let rest = &args[1..];
    let cfg = config();
    let spec = SshTargetSpec {
        name: opt(rest, "--name").unwrap_or_default(),
        host: opt(rest, "--host").unwrap_or_default(),
        user: opt(rest, "--user").unwrap_or_default(),
        port: opt_u16(rest, "--port", 22)?,
        identity_file: opt(rest, "--identity-file").map(PathBuf::from),
        max_jobs: opt_u16(rest, "--max-jobs", super::default_max_jobs(&cfg))?,
        ..SshTargetSpec::default()
    };
    if spec.name.is_empty() || spec.host.is_empty() || spec.user.is_empty() {
        return Err(Fail::Usage(
            "--name / --host / --user は必須です".into(),
        ));
    }
    let target = make_target(&spec)?;
    let reg = registry()?;
    reg.add_target(target.clone())?;
    Ok(format!(
        "実行先 {} を登録しました ({})。\n\
         次にすること:\n\
         \x20 1. zai cloud target trust {}        相手の鍵の指紋を見て記録する\n\
         \x20 2. zai cloud target probe {}        届くことを確かめる",
        target.name,
        target.endpoint.summary(),
        target.name,
        target.name
    ))
}

fn target_probe(args: &[String]) -> Result<String, Fail> {
    let name = positional(args)
        .ok_or_else(|| Fail::Usage("zai cloud target probe <名前>".into()))?;
    let reg = registry()?;
    let (target, probe) = reg.probe(&name)?;
    if flag(args, "--json") {
        return Ok(serde_json::to_string_pretty(&json!({
            "reachable": probe.reachable,
            "latency_ms": probe.latency_ms,
            "target": target_json(&target),
            "kernel": probe.kernel,
            "shell": probe.shell,
            "tools": probe.capabilities.tools,
            "error": probe.error,
        }))
        .unwrap_or_default());
    }
    if !probe.reachable {
        return Err(Fail::Code(
            EXIT_ERR,
            format!("{name} へ届きませんでした ({} ms)\n{}", probe.latency_ms, probe.error),
        ));
    }
    let c = &probe.capabilities;
    Ok(format!(
        "{name} は使えます ({} ms)\n  OS       {} / {}\n  CPU      {}\n  メモリ    {}\n  \
         ディスク  {}\n  シェル    {}\n  カーネル  {}\n  道具      {}",
        probe.latency_ms,
        c.os.id(),
        c.arch.id(),
        c.cpu_cores.map(|v| format!("{v} core")).unwrap_or_else(|| "不明".into()),
        c.memory_mib
            .map(|v| format!("{} GiB", v / 1024))
            .unwrap_or_else(|| "不明".into()),
        c.disk_mib
            .map(|v| format!("{} GiB", v / 1024))
            .unwrap_or_else(|| "不明".into()),
        probe.shell,
        probe.kernel,
        c.tools.iter().cloned().collect::<Vec<_>>().join(" "),
    ))
}

fn target_remove(args: &[String]) -> Result<String, Fail> {
    let name = positional(args)
        .ok_or_else(|| Fail::Usage("zai cloud target remove <名前>".into()))?;
    let reg = registry()?;
    let t = reg.remove_target(&name)?;
    Ok(format!(
        "{} を一覧から外しました (機械には触っていません)",
        t.name
    ))
}

/// `zai cloud target trust <名前> [--yes]` — 相手の公開鍵を記録する。
///
/// ## なぜ `accept-new` を既定にしないのか
///
/// 最初の接続では**必ず**「この鍵を知らない」になる。ここで自動的に受け入れる
/// (`StrictHostKeyChecking=accept-new`) と、**その 1 回だけは中間者を検出できない**。
/// 一方で何の手段も用意しないと、`zai cloud target add ssh` した実行先へ
/// 永久に繋がらない (実際に最初の版がそうなっていた)。
///
/// だから「取ってきて、指紋を見せて、人が同意したら記録する」の 3 段にする。
/// `--yes` が無ければ**記録しない** — 見せるだけで止まる。
fn target_trust(args: &[String]) -> Result<String, Fail> {
    use super::transport::ssh;

    let name = positional(args)
        .ok_or_else(|| Fail::Usage("zai cloud target trust <名前> [--yes]".into()))?;
    let reg = registry()?;
    let target = reg.find(&name)?;
    let super::model::TargetEndpoint::Ssh { host, port, .. } = &target.endpoint else {
        return Err(Fail::Usage(
            "手元の実行先にホスト鍵はありません".into(),
        ));
    };

    let entries = ssh::scan_host_keys(host, *port, reg.ssh_timeout())?;
    let dir = store::ensure_dir()?;
    let path = store::known_hosts_path();
    let pattern = ssh::known_hosts_pattern(host, *port);
    let existing = std::fs::read_to_string(&path).unwrap_or_default();

    // **すでに別の鍵を記録している相手は、黙って上書きしない。**
    // ここで上書きすると、中間者攻撃をそのまま通すことになる。
    if let Some(old) = ssh::conflicting_entry(&existing, &pattern, &entries) {
        let kind = old.split_whitespace().nth(1).unwrap_or("?");
        return Err(Fail::Cloud(CloudError::security(format!(
            "{name} には、いまと違う {kind} の鍵がすでに記録されています。
             機械を作り直したのでなければ、**中間者攻撃と区別が付きません**。
             作り直したと確信できる場合だけ、{} の {pattern} の行を手で消してから
             もう一度 trust してください",
            path.display()
        ))));
    }

    let prints = ssh::fingerprints(&entries, &dir);
    let mut out = format!("{name} ({host}:{port}) が名乗っている鍵:\n");
    if prints.is_empty() {
        for e in &entries {
            let kind = e.split_whitespace().nth(1).unwrap_or("?");
            out.push_str(&format!("  {kind}\n"));
        }
        out.push_str("  (指紋は ssh-keygen が無いため出せません)\n");
    } else {
        for p in &prints {
            out.push_str(&format!("  {p}\n"));
        }
    }

    if !flag(args, "--yes") {
        out.push_str(&format!(
            "\nこの指紋が、相手 (VPS の管理画面やコンソール) の示すものと\n\
             同じかを確かめてください。同じなら記録します:\n\
             \x20 zai cloud target trust {name} --yes"
        ));
        return Ok(out);
    }

    let added = ssh::append_known_hosts(&path, &entries)?;
    if added == 0 {
        out.push_str("\nすでに記録済みでした (変更なし)");
    } else {
        out.push_str(&format!(
            "\n{added} 件を {} へ記録しました。\n\
             これ以降は厳密に照合します — 鍵が変わったら接続を断ります。\n\n\
             届くことを確かめる: zai cloud target probe {name}",
            path.display()
        ));
    }
    Ok(out)
}

// ───────────────────────── exec / shell ─────────────────────────

/// `--target` と能力の指定から、要求を組む。
///
/// **ここが [`scheduler`] への唯一の入口**。要求の組み立てが 2 か所に散ると、
/// 片方だけ渡し忘れた条件が黙って無視される。
fn requirements(args: &[String]) -> Result<super::model::ExecutionRequirements, Fail> {
    let want = opt(args, "--target").unwrap_or_else(|| "auto".to_string());
    let mut req = scheduler::requirements_for_target(&want);
    req.prefer = super::prefer(&config());

    if let Some(v) = opt(args, "--os") {
        req.os = Some(super::model::OsFamily::from_id(&v).ok_or_else(|| {
            Fail::Usage(format!("--os は linux / macos / windows のいずれかです: {v}"))
        })?);
    }
    if let Some(v) = opt(args, "--arch") {
        req.arch = Some(super::model::Architecture::from_id(&v).ok_or_else(|| {
            Fail::Usage(format!("--arch は x86_64 / aarch64 のいずれかです: {v}"))
        })?);
    }
    if let Some(v) = opt(args, "--min-cpu") {
        req.min_cpu_cores = Some(
            v.parse()
                .map_err(|_| Fail::Usage(format!("--min-cpu には数を指定してください: {v}")))?,
        );
    }
    if let Some(v) = opt(args, "--min-memory-mib") {
        req.min_memory_mib = Some(
            v.parse()
                .map_err(|_| Fail::Usage(format!("--min-memory-mib には数を指定してください: {v}")))?,
        );
    }
    req.requires_gpu = flag(args, "--gpu");
    for v in opts_all(args, "--tool") {
        req.required_tools.insert(v);
    }
    for v in opts_all(args, "--label") {
        req.labels.insert(v);
    }
    Ok(req)
}

/// `--target` の指定から実行先を 1 つ決める。
fn pick_target(args: &[String], reg: &Registry) -> Result<ExecutionTarget, Fail> {
    let want = opt(args, "--target").unwrap_or_else(|| "auto".to_string());
    let targets = reg.targets()?;
    let req = requirements(args)?;
    match scheduler::select_target(&req, &targets) {
        Some(id) => Ok(targets
            .into_iter()
            .find(|t| t.id == id)
            .expect("選んだものは一覧にある")),
        None => {
            // **なぜ選べなかったのかを言う。** 「空きがありません」だけだと、
            // 利用者は VM を増やして直そうとする (実際は RAM 不足かもしれない)
            let why = scheduler::explain(&req, &targets);
            if flag(args, "--json") {
                // 機械から読むときは**分類の ID** を出す (日本語の文面は変わりうる)
                let v: Vec<_> = why
                    .iter()
                    .map(|(id, r)| json!({ "target": id.as_str(), "reason": r.id() }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
            }
            let mut msg = if want == "auto" {
                "条件に合う実行先がありません".to_string()
            } else {
                format!("{want} は使えません")
            };
            for (id, r) in why.iter().take(8) {
                let name = targets
                    .iter()
                    .find(|t| &t.id == id)
                    .map(|t| t.name.clone())
                    .unwrap_or_else(|| id.to_string());
                msg.push_str(&format!("\n  {name}: {}", reject_text(*r)));
            }
            if targets.iter().any(|t| t.lifecycle != TargetLifecycle::Ready) {
                msg.push_str("\n\n確かめるには: zai cloud target probe <名前>");
            }
            Err(Fail::Cloud(CloudError::no_capacity(msg)))
        }
    }
}

fn reject_text(r: scheduler::Reject) -> &'static str {
    use scheduler::Reject as R;
    match r {
        R::NotReady => "まだ届くことを確かめていません",
        R::Destroying => "破棄の途中です (probe で確かめ直すか、target remove で外してください)",
        R::Os => "OS が合いません",
        R::Arch => "アーキテクチャが合いません",
        R::Cpu => "CPU が足りません",
        R::Memory => "メモリが足りません",
        R::Gpu => "GPU がありません",
        R::Tools => "必要な道具がありません",
        R::Labels => "指定された札がありません",
        R::Full => "同時実行の枠が埋まっています",
        R::NotPreferred => "名指しされた実行先ではありません",
    }
}

fn exec_cmd(args: &[String]) -> Result<String, Fail> {
    let argv = after_dashdash(args).ok_or_else(|| {
        Fail::Usage("zai cloud exec --target <名前> -- <コマンド...>".into())
    })?;
    if argv.is_empty() {
        return Err(Fail::Usage("-- の後ろにコマンドを書いてください".into()));
    }
    let reg = registry()?;
    let target = pick_target(args, &reg)?;
    let launch = LaunchSpec::from_argv(&argv)?;

    let mut sink = StdSink;
    let job = runner::exec_once(&target, &launch, reg.ssh_timeout(), &mut sink)?;
    match job.exit_code {
        Some(0) => Ok(String::new()),
        // **相手の終了コードをそのまま返さない。** 1〜4 は Zaivern 自身の
        // 意味を持つので、リモートの 2 が「使い方の誤り」に化ける。
        // 代わりに 1 (実行時エラー) へ畳み、実際の値を標準エラーへ書く。
        Some(code) => Err(Fail::Code(
            EXIT_ERR,
            format!("コマンドが {code} で終了しました"),
        )),
        None => Err(Fail::Code(EXIT_ERR, "コマンドが異常終了しました".into())),
    }
}

fn shell_cmd(args: &[String]) -> Result<String, Fail> {
    let name = positional(args).ok_or_else(|| Fail::Usage("zai cloud shell <名前>".into()))?;
    let reg = registry()?;
    let target = reg.find(&name)?;
    if target.transport == TransportKind::Local {
        return Err(Fail::Usage(
            "手元の機械にはそのままシェルがあります".into(),
        ));
    }
    let opts = runner::ssh_options(reg.ssh_timeout());
    let mut cmd = super::transport::ssh::ssh_shell_command(&target, &opts)?;
    // **対話なので標準入出力をそのまま渡す** (溜めない)。
    let status = cmd
        .status()
        .map_err(|e| CloudError::io(format!("ssh を起動できません: {e}")))?;
    if status.success() {
        Ok(String::new())
    } else {
        Err(Fail::Code(EXIT_ERR, String::new()))
    }
}

/// `zai cloud launch` — **エージェントをリモートで起動するためのコマンド行**を作る。
///
/// 既存のエージェント起動経路 ([`crate::agents::AgentManager`] →
/// [`crate::terminal::Session`]) は「1 本のコマンド行」を受け取る形なので、
/// ここが返す行をそのままエージェント設定 (`config.toml` の preset) の
/// `command` に貼れば、**同じ Supervisor が同じように見張る** (§36)。
///
/// **v1 では GUI から直接リモート起動する導線は無い** (既知の制限)。
/// ここは「貼れる 1 行を作る」ところまでを担う。`--run` を付けると
/// その場で前面実行する。
fn launch_cmd(args: &[String]) -> Result<String, Fail> {
    let reg = registry()?;
    let target = pick_target(args, &reg)?;
    let opts = runner::ssh_options(reg.ssh_timeout());
    let cwd = opt(args, "--cwd");

    // 2 通りの渡し方を持つ:
    //   --command "<行>"   … 既存カタログが組んだコマンド行をそのまま運ぶ
    //   -- <program> <引数> … 構造のまま渡す (空白や引用符で割れない)
    let command = opt(args, "--command");
    let argv = after_dashdash(args);
    let spec = match (&command, &argv) {
        (Some(_), _) => None,
        (None, Some(a)) if !a.is_empty() => {
            let mut spec = LaunchSpec::from_argv(a)?;
            spec.cwd = cwd.clone();
            Some(spec)
        }
        _ => {
            return Err(Fail::Usage(
                "zai cloud launch --target <名前> [--command \"<コマンド行>\" | -- <コマンド...>]"
                    .into(),
            ))
        }
    };

    if !flag(args, "--run") {
        // **貼るための 1 行。** どのシェル向けの引用かを必ず添える —
        // POSIX 用の行を「どこでも使える」ように見せると、Windows の
        // preset へ貼った人のところで静かに壊れる。
        let line = match (&command, &spec) {
            (Some(c), _) => super::command::session_command_line(&target, c, cwd.as_deref(), &opts)?,
            (None, Some(s)) => super::command::launch_command_line(&target, s, &opts)?,
            _ => unreachable!("上で使い方の誤りとして返している"),
        };
        // 注記は**標準エラーへ**。標準出力に混ぜると貼り付けに使えない
        eprintln!(
            "# 次の 1 行は POSIX シェル (sh / bash / zsh) 向けの引用です。\n\
             # Windows の PowerShell / cmd.exe へはそのまま貼れません — \
             その場で走らせるには --run を使ってください。"
        );
        return Ok(line);
    }

    // **その場で走らせる。** ここでは表示用の 1 行を作らない —
    // ローカルのシェルへ渡すと、その引用規則で読み直されて壊れる
    // (`cmd.exe` は POSIX の単一引用符を引用として扱わない)。
    let plan = match (&command, &spec) {
        (Some(c), _) => super::command::run_plan_for_command(&target, c, cwd.as_deref(), &opts)?,
        (None, Some(s)) => super::command::run_plan_for_spec(&target, s, &opts)?,
        _ => unreachable!("上で使い方の誤りとして返している"),
    };
    let status = spawn_plan(&plan)?;
    match status.code() {
        Some(0) => Ok(String::new()),
        // **相手の終了コードをそのまま返さない。** 1〜4 は Zaivern 自身の
        // 意味を持つので、リモートの 2 が「使い方の誤り」に化ける。
        // 代わりに 1 へ畳み、実際の値を標準エラーへ書く (`exec` と同じ)。
        Some(code) => Err(Fail::Code(
            EXIT_ERR,
            format!("コマンドが {code} で終了しました"),
        )),
        None => Err(Fail::Code(EXIT_ERR, "コマンドが異常終了しました".into())),
    }
}

/// [`RunPlan`] を起動する。**標準入出力はそのまま引き継ぐ** (対話できる)。
///
/// `Command::status` は既定で親の stdin / stdout / stderr を継承するので、
/// リダイレクトも終了コードもそのまま呼び出し元へ伝わる。
fn spawn_plan(plan: &super::command::RunPlan) -> Result<std::process::ExitStatus, CloudError> {
    let mut cmd = if plan.via_local_shell {
        // 利用者が書いた 1 行を手元のシェルで走らせる場合だけ、シェルを挟む
        let mut c = crate::procx::hidden_command(if cfg!(windows) { "cmd" } else { "sh" });
        c.arg(if cfg!(windows) { "/C" } else { "-c" })
            .arg(&plan.argv[0]);
        c
    } else {
        let mut c = crate::procx::hidden_command(&plan.argv[0]);
        c.args(&plan.argv[1..]);
        c
    };
    if let Some(dir) = &plan.cwd {
        cmd.current_dir(expand_local_home(dir));
    }
    cmd.status()
        .map_err(|e| CloudError::io(format!("{} を起動できません: {e}", plan.argv[0])))
}

/// 手元のパスの `~` を広げる。
fn expand_local_home(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// `zai cloud copy` — ファイルを 1 つ送る / 受け取る。
///
/// `assets/plugins/remote-host/` の push / pull を Rust 側へ移したもの (§4)。
/// プラグインと違い、**host key の確認を外さず**、パスをシェルへ連結しない。
fn copy_cmd(args: &[String]) -> Result<String, Fail> {
    let reg = registry()?;
    let target = pick_target(args, &reg)?;
    let transport = super::transport::for_target(&target, reg.ssh_timeout());
    let positionals: Vec<String> = positionals(args);

    if let Some(remote) = opt(args, "--from") {
        // 受け取る: --from <リモート> <手元>
        let dest = positionals
            .first()
            .ok_or_else(|| Fail::Usage("zai cloud copy --target <名前> --from <リモート> <手元>".into()))?;
        let src = super::model::RemotePath::new(remote)?;
        transport.download(&target, &src, std::path::Path::new(dest))?;
        return Ok(format!("{src} を {dest} へ受け取りました"));
    }
    // 送る: <手元> <リモート>
    if positionals.len() < 2 {
        return Err(Fail::Usage(
            "zai cloud copy --target <名前> <手元のファイル> <リモートのパス>".into(),
        ));
    }
    let dest = super::model::RemotePath::new(positionals[1].clone())?;
    transport.upload(&target, std::path::Path::new(&positionals[0]), &dest)?;
    Ok(format!("{} を {dest} へ送りました", positionals[0]))
}

/// 標準出力／標準エラーへそのまま流す受け口。**溜めない。**
struct StdSink;

impl super::model::EventSink for StdSink {
    fn on_stdout(&mut self, chunk: &[u8]) {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = out.write_all(chunk);
        let _ = out.flush();
    }
    fn on_stderr(&mut self, chunk: &[u8]) {
        use std::io::Write;
        let mut err = std::io::stderr();
        let _ = err.write_all(chunk);
        let _ = err.flush();
    }
}

// ───────────────────────── job ─────────────────────────

fn job_cmd(args: &[String]) -> Result<String, Fail> {
    let sub = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| Fail::Usage("zai cloud job <run|list>".into()))?;
    let rest = &args[1..];
    match sub {
        "list" => job_list(rest),
        "run" => job_run(rest),
        other => Err(Fail::Usage(format!("知らない操作です: job {other}"))),
    }
}

fn job_list(args: &[String]) -> Result<String, Fail> {
    let limit: usize = opt(args, "--limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    let jobs = runner::recent_jobs(limit)?;
    if flag(args, "--json") {
        // 機械が読む側は **ID のまま** (名前は変わりうる)
        return Ok(serde_json::to_string_pretty(&jobs).unwrap_or_default());
    }
    // **人が読む側は名前で出す。** 記録が持つのは ID (名前を変えても
    // 同一性が続く) だが、一覧に生の ID が並んでもどれのことか分からない。
    let names: std::collections::BTreeMap<String, String> = registry()
        .and_then(|r| r.targets())
        .map(|ts| {
            ts.into_iter()
                .map(|t| (t.id.as_str().to_string(), t.name))
                .collect()
        })
        .unwrap_or_default();
    let mut out = format!(
        "{:<20} {:<12} {:<10} {:<6} {}\n",
        "ID", "実行先", "状態", "終了", "コマンド"
    );
    for j in &jobs {
        out.push_str(&format!(
            "{:<20} {:<12} {:<10} {:<6} {}\n",
            j.id.as_str(),
            names
                .get(j.target.as_str())
                .map(String::as_str)
                // 一覧から外された実行先の仕事も残るので、そのときは ID を出す
                .unwrap_or_else(|| j.target.as_str()),
            j.state.id(),
            j.exit_code
                .map(|c| c.to_string())
                .unwrap_or_else(|| "―".into()),
            j.command,
        ));
    }
    Ok(out)
}

fn job_run(args: &[String]) -> Result<String, Fail> {
    let argv = after_dashdash(args).ok_or_else(|| {
        Fail::Usage("zai cloud job run --target <名前> -- <コマンド...>".into())
    })?;
    if argv.is_empty() {
        return Err(Fail::Usage("-- の後ろにコマンドを書いてください".into()));
    }
    let cwd = std::env::current_dir().map_err(CloudError::from)?;
    let repo = super::git_workspace::local_repo_root(&cwd)?;
    let reg = registry()?;
    let target = pick_target(args, &reg)?;
    if target.transport == TransportKind::Local {
        return Err(Fail::Usage(
            "分離した作業場はリモートの実行先にだけ作れます (--target を指定してください)".into(),
        ));
    }

    let spec = runner::JobSpec {
        target,
        launch: LaunchSpec::from_argv(&argv)?,
        local_repo: Some(repo),
        workspace_key: crate::history::workspace_key(&cwd),
        isolated: true,
        timeout: opt(args, "--timeout")
            .and_then(|v| v.parse().ok())
            .map(Duration::from_secs)
            .unwrap_or_else(|| reg.ssh_timeout().max(Duration::from_secs(1800))),
    };
    let mut sink = StdSink;
    let job = runner::run(&spec, &mut sink)?;
    let mut out = format!(
        "仕事 {} は {} で終わりました\n  結果の枝  {}",
        job.id,
        job.state.id(),
        job.result_ref
    );
    if !job.message.is_empty() {
        out.push_str(&format!("\n  備考      {}", job.message));
    }
    out.push_str(&format!(
        "\n\n手元では次のように見られます:\n  git log {}\n  git diff HEAD..{}",
        job.result_ref, job.result_ref
    ));
    if job.exit_code != Some(0) {
        return Err(Fail::Code(EXIT_ERR, out));
    }
    Ok(out)
}

// ───────────────────────── provider ─────────────────────────

fn provider_cmd(args: &[String]) -> Result<String, Fail> {
    let sub = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| Fail::Usage("zai cloud provider <list|add|remove>".into()))?;
    let rest = &args[1..];
    match sub {
        "list" => provider_list(rest),
        "add" => provider_add(rest),
        "remove" | "rm" => provider_remove(rest),
        "types" => provider_types(rest),
        "locations" => provider_locations(rest),
        other => Err(Fail::Usage(format!("知らない操作です: provider {other}"))),
    }
}

fn provider_list(args: &[String]) -> Result<String, Fail> {
    let profiles = all_profiles(&store::load_providers()?);
    if flag(args, "--json") {
        let v: Vec<_> = profiles
            .iter()
            .map(|p| {
                json!({
                    "name": p.name,
                    "kind": p.kind.id(),
                    "mode": if p.kind.mode() == super::provider::ProvisioningMode::Dynamic { "dynamic" } else { "static" },
                    "location": p.location,
                    "server_type": p.server_type,
                    "image": p.image,
                    "ssh_key": p.ssh_key,
                    "ssh_user": p.ssh_user,
                    "token_env": p.token_env,
                    "token_present": p.token_present(),
                })
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&v).unwrap_or_default());
    }
    let mut out = format!("{:<16} {:<12} {:<10} {}\n", "名前", "種別", "方式", "既定");
    for p in &profiles {
        let mode = if p.kind.mode() == super::provider::ProvisioningMode::Dynamic {
            "dynamic"
        } else {
            "static"
        };
        let mut detail = Vec::new();
        if !p.location.is_empty() {
            detail.push(p.location.clone());
        }
        if !p.server_type.is_empty() {
            detail.push(p.server_type.clone());
        }
        if !p.image.is_empty() {
            detail.push(p.image.clone());
        }
        if !p.token_env.is_empty() {
            detail.push(format!(
                "{}={}",
                p.token_env,
                if p.token_present() { "設定あり" } else { "未設定" }
            ));
        }
        out.push_str(&format!(
            "{:<16} {:<12} {:<10} {}\n",
            p.name,
            p.kind.id(),
            mode,
            detail.join(" / ")
        ));
    }
    Ok(out)
}

fn provider_add(args: &[String]) -> Result<String, Fail> {
    let kind_id = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| Fail::Usage("zai cloud provider add hetzner --name …".into()))?;
    let kind = ProviderKind::from_id(kind_id)
        .ok_or_else(|| Fail::Usage(format!("知らない Provider です: {kind_id}")))?;
    let rest = &args[1..];
    let name = opt(rest, "--name")
        .ok_or_else(|| Fail::Usage("--name を指定してください".into()))?;

    // **トークンを引数で受け取らない** (§42)。コマンド履歴と `ps` に残る。
    if rest.iter().any(|a| a == "--token" || a == "--api-token") {
        return Err(Fail::Cloud(CloudError::security(
            "API トークンは引数では受け取りません (履歴とプロセス一覧に残るため)。\n\
             環境変数に入れてください:  export HCLOUD_TOKEN=…",
        )));
    }

    let profile = ProviderProfile {
        name: name.clone(),
        kind,
        token_env: opt(rest, "--token-env").unwrap_or_else(|| match kind {
            ProviderKind::Hetzner => "HCLOUD_TOKEN".to_string(),
            _ => String::new(),
        }),
        location: opt(rest, "--location").unwrap_or_default(),
        server_type: opt(rest, "--server-type").unwrap_or_default(),
        image: opt(rest, "--image").unwrap_or_default(),
        ssh_key: opt(rest, "--ssh-key").unwrap_or_default(),
        ssh_user: opt(rest, "--ssh-user").unwrap_or_else(|| "zaivern".to_string()),
        max_jobs: opt_u16(rest, "--max-jobs", 1)?,
        api_base: String::new(),
        identity_file: opt(rest, "--identity-file").map(PathBuf::from),
    };
    profile.assert_no_secret()?;

    let mut list = store::load_providers()?;
    if list.iter().any(|p| p.name == name) {
        return Err(Fail::Cloud(CloudError::config(format!(
            "Provider {name} はすでに登録されています"
        ))));
    }
    list.push(profile.clone());
    store::save_providers(&list)?;

    let mut out = format!("Provider {name} ({}) を登録しました", kind.id());
    if !profile.token_env.is_empty() && !profile.token_present() {
        out.push_str(&format!(
            "\n\n⚠ 環境変数 {} がまだ設定されていません:\n  export {}=…",
            profile.token_env, profile.token_env
        ));
    }
    Ok(out)
}

fn provider_remove(args: &[String]) -> Result<String, Fail> {
    let name = positional(args)
        .ok_or_else(|| Fail::Usage("zai cloud provider remove <名前>".into()))?;
    let mut list = store::load_providers()?;
    let before = list.len();
    list.retain(|p| p.name != name);
    if list.len() == before {
        return Err(Fail::Cloud(CloudError::config(format!(
            "Provider {name} は登録されていません"
        ))));
    }
    store::save_providers(&list)?;
    Ok(format!("Provider {name} を外しました"))
}

/// `zai cloud provider types <名前>` — 使えるサーバー種別と**その時点の価格**。
///
/// **価格表をコードへ書かない** (§21) ので、知りたければ API に聞く。
fn provider_types(args: &[String]) -> Result<String, Fail> {
    let name = positional(args)
        .ok_or_else(|| Fail::Usage("zai cloud provider types <名前>".into()))?;
    let reg = registry()?;
    let profile = reg.profile(&name)?.clone();
    if profile.kind != ProviderKind::Hetzner {
        return Err(Fail::Cloud(CloudError::unsupported(format!(
            "{name} はサーバー種別の一覧を持ちません"
        ))));
    }
    let p = super::provider::HetznerProvider::new(
        profile,
        std::sync::Arc::new(super::provider::http::UreqClient::new(reg.ssh_timeout())),
        reg.ssh_timeout(),
    );
    let types = p.list_server_types()?;
    if flag(args, "--json") {
        let v: Vec<_> = types
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "cores": t.cores,
                    "memory_mib": t.memory_mib,
                    "disk_mib": t.disk_mib,
                    "cost": t.billing.summary(),
                })
            })
            .collect();
        return Ok(serde_json::to_string_pretty(&v).unwrap_or_default());
    }
    let mut out = format!("{:<12} {:<6} {:<10} {:<10} {}\n", "種別", "CPU", "メモリ", "ディスク", "費用");
    for t in &types {
        out.push_str(&format!(
            "{:<12} {:<6} {:<10} {:<10} {}\n",
            t.name,
            t.cores,
            format!("{} GiB", t.memory_mib / 1024),
            format!("{} GiB", t.disk_mib / 1024),
            t.billing.summary()
        ));
    }
    Ok(out)
}

/// `zai cloud provider locations <名前>` — 使える場所。
fn provider_locations(args: &[String]) -> Result<String, Fail> {
    let name = positional(args)
        .ok_or_else(|| Fail::Usage("zai cloud provider locations <名前>".into()))?;
    let reg = registry()?;
    let profile = reg.profile(&name)?.clone();
    if profile.kind != ProviderKind::Hetzner {
        return Err(Fail::Cloud(CloudError::unsupported(format!(
            "{name} は場所の一覧を持ちません"
        ))));
    }
    let p = super::provider::HetznerProvider::new(
        profile,
        std::sync::Arc::new(super::provider::http::UreqClient::new(reg.ssh_timeout())),
        reg.ssh_timeout(),
    );
    let locations = p.list_locations()?;
    if flag(args, "--json") {
        return Ok(serde_json::to_string_pretty(&locations).unwrap_or_default());
    }
    Ok(locations.join("\n"))
}

// ───────────────────────── worker ─────────────────────────

fn worker_cmd(args: &[String]) -> Result<String, Fail> {
    let sub = args
        .first()
        .map(String::as_str)
        .ok_or_else(|| Fail::Usage("zai cloud worker <create|destroy>".into()))?;
    let rest = &args[1..];
    match sub {
        "create" => worker_create(rest),
        "destroy" => worker_destroy(rest),
        other => Err(Fail::Usage(format!("知らない操作です: worker {other}"))),
    }
}

fn worker_create(args: &[String]) -> Result<String, Fail> {
    let provider = opt(args, "--provider")
        .ok_or_else(|| Fail::Usage("--provider を指定してください".into()))?;
    let name = opt(args, "--name").ok_or_else(|| Fail::Usage("--name を指定してください".into()))?;
    let reg = registry()?;
    let profile = reg.profile(&provider)?.clone();

    let spec = ProvisionSpec {
        name: name.clone(),
        server_type: opt(args, "--server-type").unwrap_or_else(|| profile.server_type.clone()),
        location: opt(args, "--location")
            .or_else(|| Some(profile.location.clone()))
            .filter(|s| !s.is_empty()),
        image: opt(args, "--image").unwrap_or_else(|| profile.image.clone()),
        ssh_key: opt(args, "--ssh-key").unwrap_or_else(|| profile.ssh_key.clone()),
        labels: Default::default(),
        ephemeral: flag(args, "--ephemeral"),
        max_jobs: opt_u16(args, "--max-jobs", profile.max_jobs.max(1))?,
    };

    // **課金される操作なので、何を作るかを先に書く。**
    eprintln!(
        "{} に {} ({}) を作ります。課金されます。",
        provider, spec.name, spec.server_type
    );
    let target = reg.provision(&provider, &spec)?;
    if !flag(args, "--wait") {
        return Ok(format!(
            "{} を作りました ({})。\n  状態      {}\n\n\
             SSH が開いたら確かめてください:\n  zai cloud target probe {}\n\
             (待ってから使いたいときは --wait を付けてください)",
            target.name,
            target.endpoint.summary(),
            target.lifecycle.id(),
            target.name
        ));
    }
    eprintln!("SSH が開くのを待っています…");
    let ready = reg.wait_ready(&target.id)?;
    Ok(format!(
        "{} は使えます ({})。\n  OS        {} / {}\n  枠        {}",
        ready.name,
        ready.endpoint.summary(),
        ready.capabilities.os.id(),
        ready.capabilities.arch.id(),
        ready.capacity.max_jobs
    ))
}

fn worker_destroy(args: &[String]) -> Result<String, Fail> {
    let name = positional(args)
        .ok_or_else(|| Fail::Usage("zai cloud worker destroy <名前> [--yes]".into()))?;
    let reg = registry()?;
    let target = reg.find(&name)?;
    if !flag(args, "--yes") {
        // **非対話では `--yes` を必須にする** (§42)。パイプの向こうで
        // 黙って消えるのがいちばん困る。
        return Err(Fail::Cloud(CloudError::config(format!(
            "{} ({}) を消します。取り消せません。\n\
             実行するには --yes を付けてください:\n  zai cloud worker destroy {} --yes",
            target.name,
            target.endpoint.summary(),
            target.name
        ))));
    }
    let t = reg.destroy(&name)?;
    Ok(format!("{} を消しました", t.name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::test_support::home_guard;

    #[test]
    fn 鍵と値を取り出す() {
        let args: Vec<String> = ["--name", "dev-01", "--port", "2222", "--json"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(opt(&args, "--name").as_deref(), Some("dev-01"));
        assert_eq!(opt_u16(&args, "--port", 22).expect("読める"), 2222);
        assert!(flag(&args, "--json"));
        assert_eq!(opt(&args, "--host"), None);
        assert_eq!(opt_u16(&args, "--max-jobs", 4).expect("既定"), 4);
    }

    #[test]
    fn 数でない値は使い方の誤り() {
        let args = vec!["--port".to_string(), "abc".to_string()];
        assert!(matches!(opt_u16(&args, "--port", 22), Err(Fail::Usage(_))));
    }

    #[test]
    fn 位置引数は鍵の値を拾わない() {
        // `--target dev-01 probe` のとき、dev-01 を位置引数として拾わない
        let args: Vec<String> = ["--target", "dev-01", "myname"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(positional(&args).as_deref(), Some("myname"));
        // 値を取らない鍵の後ろは拾える
        let args: Vec<String> = ["--json", "myname"].iter().map(|s| s.to_string()).collect();
        assert_eq!(positional(&args).as_deref(), Some("myname"));
        // `--` より後ろは見ない
        let args: Vec<String> = ["--", "cargo"].iter().map(|s| s.to_string()).collect();
        assert_eq!(positional(&args), None);
    }

    #[test]
    fn 値を取る鍵の表が本文と合っている() {
        // 表に書き忘れると、値が位置引数として拾われて別のものを操作する
        for key in TAKES_VALUE {
            assert!(
                HELP.contains(key),
                "{key} が表にあるのにヘルプへ出ていない"
            );
        }
    }

    #[test]
    fn ダッシュ二つの後ろを取り出す() {
        let args: Vec<String> = ["--target", "a", "--", "cargo", "test", "--workspace"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            after_dashdash(&args),
            Some(vec![
                "cargo".to_string(),
                "test".to_string(),
                "--workspace".to_string()
            ])
        );
        assert_eq!(after_dashdash(&["a".to_string()]), None);
    }

    #[test]
    fn トークンを引数で受け取らない() {
        let _home = home_guard("cli-token-arg");
        let args: Vec<String> = [
            "hetzner",
            "--name",
            "h",
            "--token",
            "super-secret-test-token",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let e = provider_add(&args).expect_err("断る");
        match e {
            Fail::Cloud(c) => {
                assert!(matches!(c, CloudError::Security(_)), "{c:?}");
                assert!(!format!("{c}").contains("super-secret-test-token"), "{c}");
                assert!(format!("{c}").contains("HCLOUD_TOKEN"), "{c}");
            }
            _ => panic!("安全のための拒否になっていない"),
        }
    }

    #[test]
    fn providerを足して一覧に出る() {
        let _home = home_guard("cli-provider-add");
        let args: Vec<String> = [
            "hetzner",
            "--name",
            "hetzner-eu",
            "--location",
            "fsn1",
            "--server-type",
            "cx33",
            "--image",
            "ubuntu-24.04",
            "--ssh-key",
            "zaivern",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = provider_add(&args).expect("足せる");
        assert!(out.contains("hetzner-eu"), "{out}");

        let listed = provider_list(&["--json".to_string()]).expect("一覧");
        let v: serde_json::Value = serde_json::from_str(&listed).expect("JSON");
        let names: Vec<&str> = v
            .as_array()
            .expect("配列")
            .iter()
            .filter_map(|p| p["name"].as_str())
            .collect();
        assert!(names.contains(&"hetzner-eu"), "{names:?}");
        // 組み込みも出る
        assert!(names.contains(&"local") && names.contains(&"static-ssh"));
        // **値は出さない**
        assert!(!listed.contains("super-secret"), "{listed}");
        assert!(listed.contains("HCLOUD_TOKEN"), "名前は出す: {listed}");

        // 2 度目は断る (後勝ちにしない)
        assert!(provider_add(&args).is_err());
    }

    #[test]
    fn 実行先を足して一覧とjsonに出る() {
        let _home = home_guard("cli-target-add");
        let args: Vec<String> = [
            "ssh",
            "--name",
            "dev-01",
            "--host",
            "example.com",
            "--user",
            "zaivern",
            "--max-jobs",
            "4",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let out = target_add(&args).expect("足せる");
        assert!(out.contains("dev-01"), "{out}");

        let listed = target_list(&["--json".to_string()]).expect("一覧");
        let v: serde_json::Value = serde_json::from_str(&listed).expect("JSON");
        let arr = v.as_array().expect("配列");
        assert_eq!(arr.len(), 2, "local と dev-01");
        let dev = arr.iter().find(|t| t["name"] == "dev-01").expect("居る");
        assert_eq!(dev["max_jobs"], 4);
        assert_eq!(dev["lifecycle"], "unknown", "確かめる前は Ready にしない");
    }

    #[test]
    fn 危ないホストは登録の時点で断る() {
        let _home = home_guard("cli-target-bad-host");
        let args: Vec<String> = [
            "ssh",
            "--name",
            "x",
            "--host",
            "-oProxyCommand=id",
            "--user",
            "zaivern",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert!(target_add(&args).is_err());
    }

    #[test]
    fn 消すときは明示の同意が要る() {
        let _home = home_guard("cli-destroy-confirm");
        // 実行先を 1 つ登録して、--yes 無しで消そうとする
        let add: Vec<String> = [
            "ssh", "--name", "dev-01", "--host", "example.com", "--user", "zaivern",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        target_add(&add).expect("足せる");
        let e = worker_destroy(&["dev-01".to_string()]).expect_err("断る");
        match e {
            Fail::Cloud(c) => assert!(format!("{c}").contains("--yes"), "{c}"),
            _ => panic!("同意を求めていない"),
        }
    }

    #[test]
    fn 診断は秘密を出さない() {
        let _home = home_guard("cli-doctor");
        std::env::set_var("HCLOUD_TOKEN", "super-secret-test-token");
        // トークンを使う Provider が居る状態で診断する
        // (居なければトークンの行そのものが出ないので、何も守っていない)
        provider_add(
            &["hetzner", "--name", "hetzner-eu"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .expect("足せる");
        let out = doctor(&[]).expect("診断できる");
        assert!(!out.contains("super-secret-test-token"), "{out}");
        // あるかどうかは分かる
        assert!(out.contains("HCLOUD_TOKEN"), "{out}");
        assert!(out.contains("設定あり"), "{out}");
        let js = doctor(&["--json".to_string()]).expect("診断できる");
        assert!(!js.contains("super-secret-test-token"), "{js}");
        let v: serde_json::Value = serde_json::from_str(&js).expect("JSON");
        assert!(v["known_hosts"].as_str().is_some());
        std::env::remove_var("HCLOUD_TOKEN");
    }

    /// **P2-2 の再現。** 壊れた状態ファイルを `unwrap_or_default()` で
    /// 飲み込むと「0 件」と表示される。利用者から見て「まだ登録していない」と
    /// 区別が付かないので、**次にすることが「登録する」になってしまう** —
    /// 実際には登録済みの内容が読めていないだけで、そこで登録し直すと
    /// 壊れたファイルを上書きして失う。
    #[test]
    fn 壊れた状態ファイルを零件と言わない() {
        let _home = home_guard("cli-doctor-broken");
        // 先にまともな内容を作ってから壊す (「まだ無い」と区別するため)
        provider_add(
            &["hetzner", "--name", "hetzner-eu"]
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>(),
        )
        .expect("足せる");
        std::fs::write(store::providers_path(), b"{ this is not json").expect("壊せる");
        std::fs::write(store::targets_path(), b"[[[").expect("壊せる");
        std::fs::write(store::jobs_path(), b"nope").expect("壊せる");

        let (out, ok) = doctor_report(&[]);
        assert!(!ok, "壊れているのに合格にしている: {out}");
        assert!(!out.contains("Provider (0 件)"), "0 件と言っている: {out}");
        assert!(out.contains("読めません"), "理由が出ていない: {out}");

        let (js, _) = doctor_report(&["--json".to_string()]);
        let v: serde_json::Value = serde_json::from_str(&js).expect("JSON として読める");
        assert_eq!(v["providers"]["status"], "error", "{js}");
        assert_eq!(v["targets"]["status"], "error", "{js}");
        assert_eq!(v["jobs"]["status"], "error", "{js}");
        assert!(
            v["targets"]["error"]
                .as_str()
                .unwrap_or_default()
                .contains("targets.json"),
            "どのファイルか分からない: {js}"
        );
        // 数は出さない (出すと 0 件に見える)
        assert!(v["targets"]["count"].is_null(), "{js}");
        // 診断としては不合格
        assert_eq!(v["ok"], false, "{js}");
        assert_eq!(
            cli_main(&["doctor".to_string(), "--json".to_string()]),
            EXIT_ERR,
            "壊れているのに終了コードが 0"
        );
    }

    /// 壊れていないときは、これまでどおり件数を出して合格にする。
    #[test]
    fn 読めているときは件数を出す() {
        let _home = home_guard("cli-doctor-ok");
        let (js, ok) = doctor_report(&["--json".to_string()]);
        assert!(ok, "{js}");
        let v: serde_json::Value = serde_json::from_str(&js).expect("JSON");
        assert_eq!(v["targets"]["status"], "ok", "{js}");
        assert_eq!(v["targets"]["count"], 0, "{js}");
        assert_eq!(v["ok"], true, "{js}");
        let out = doctor(&[]).expect("診断できる");
        assert!(out.contains("実行先 (0 件"), "{out}");
        assert!(!out.contains("読めません"), "{out}");
    }

    #[test]
    fn 知らないサブコマンドは使い方の誤り() {
        assert_eq!(cli_main(&["nope".to_string()]), EXIT_USAGE);
        // ヘルプは 0
        assert_eq!(cli_main(&["--help".to_string()]), EXIT_OK);
        assert_eq!(cli_main(&[]), EXIT_OK);
    }

    #[test]
    fn 選べない理由を言う() {
        let _home = home_guard("cli-no-target");
        let add: Vec<String> = [
            "ssh", "--name", "dev-01", "--host", "example.com", "--user", "zaivern",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        target_add(&add).expect("足せる");
        let reg = registry().expect("組める");
        let e = pick_target(&["--target".to_string(), "dev-01".to_string()], &reg)
            .expect_err("選べない");
        match e {
            Fail::Cloud(c) => {
                assert_eq!(c.exit_code(), EXIT_NO_TARGET);
                // 「空きが無い」ではなく「確かめていない」と言う
                assert!(format!("{c}").contains("確かめていません"), "{c}");
                assert!(format!("{c}").contains("probe"), "{c}");
            }
            _ => panic!("理由を言っていない"),
        }
    }

    // ───────── launch --run の起動経路 (指摘 4) ─────────

    /// **終了コードがそのまま呼び出し元へ伝わる。**
    #[test]
    fn 起動計画の終了コードが伝わる() {
        use super::super::command::RunPlan;
        let plan = if cfg!(windows) {
            RunPlan {
                argv: vec!["cmd".into(), "/C".into(), "exit 3".into()],
                via_local_shell: false,
                cwd: None,
            }
        } else {
            RunPlan {
                argv: vec!["sh".into(), "-c".into(), "exit 3".into()],
                via_local_shell: false,
                cwd: None,
            }
        };
        let status = spawn_plan(&plan).expect("起動できる");
        assert_eq!(status.code(), Some(3));

        // 成功も伝わる
        let ok = if cfg!(windows) {
            RunPlan {
                argv: vec!["cmd".into(), "/C".into(), "exit 0".into()],
                via_local_shell: false,
                cwd: None,
            }
        } else {
            RunPlan {
                argv: vec!["true".into()],
                via_local_shell: false,
                cwd: None,
            }
        };
        assert!(spawn_plan(&ok).expect("起動できる").success());
    }

    /// **相手の終了コードが失われない。**
    ///
    /// 実機で `sh -c 'exit 7'` を `--run` したら `rc=1` になり、7 がどこにも
    /// 出ていなかった。1〜4 は Zaivern 自身の意味を持つので値をそのまま
    /// 返すわけにはいかないが、**捨ててよいことにはならない**。
    #[test]
    fn 相手の終了コードは標準エラーへ残す() {
        use super::super::command::RunPlan;
        let plan = if cfg!(windows) {
            RunPlan {
                argv: vec!["cmd".into(), "/C".into(), "exit 7".into()],
                via_local_shell: false,
                cwd: None,
            }
        } else {
            RunPlan {
                argv: vec!["sh".into(), "-c".into(), "exit 7".into()],
                via_local_shell: false,
                cwd: None,
            }
        };
        let status = spawn_plan(&plan).expect("起動できる");
        assert_eq!(status.code(), Some(7));

        // CLI の層は 1 へ畳むが、値は文面に残す
        let src = include_str!("cli.rs").replace("\r\n", "\n");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        let at = body.find("let status = spawn_plan(&plan)?;").expect("起動している");
        let tail = &body[at..at + 700.min(body.len() - at)];
        assert!(
            tail.contains("コマンドが {code} で終了しました"),
            "終了コードを捨てている:\n{tail}"
        );
    }

    /// 起動できないものは、実行時の失敗として返る (黙って成功にしない)。
    #[test]
    fn 起動できなければ失敗として返る() {
        use super::super::command::RunPlan;
        let plan = RunPlan {
            argv: vec!["zaivern-no-such-program-xyz".into()],
            via_local_shell: false,
            cwd: None,
        };
        assert!(spawn_plan(&plan).is_err());
    }

    /// **`--run` は POSIX 用の 1 行をローカルのシェルへ渡さない。**
    ///
    /// 渡していたのがこの版で直した壊れ方で、`cmd.exe` は POSIX の
    /// 単一引用符を引用として扱わないため Windows で引数が壊れていた。
    #[test]
    fn runはposix用の行をローカルシェルへ渡さない() {
        let src = include_str!("cli.rs").replace("\r\n", "\n");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        // 起動する関数の**中だけ**を見る (範囲を広げると空回りする)
        let at = body.find("fn spawn_plan(").expect("起動する関数がある");
        let end = body[at..]
            .find("\nfn ")
            .map(|e| at + e)
            .unwrap_or(body.len());
        let spawn = &body[at..end];
        // シェルを挟むのは via_local_shell のときだけ
        assert!(
            spawn.contains("if plan.via_local_shell"),
            "シェルを挟む条件が無い:\n{spawn}"
        );
        let cmd_at = spawn.find("\"cmd\"").expect("windows 側の分岐がある");
        let cond_at = spawn.find("if plan.via_local_shell").expect("条件がある");
        assert!(cond_at < cmd_at, "条件より先にシェルを挟んでいる");
        // **標準入出力を差し替えない** (対話とリダイレクトを壊さない)
        for banned in ["Stdio::piped", "Stdio::null", "stdout(", "stderr(", "stdin("] {
            assert!(!spawn.contains(banned), "{banned} が起動経路にある:\n{spawn}");
        }

        // 貼り付け用の行を出すときは、対象シェルを明示している
        let launch_at = body.find("fn launch_cmd(").expect("launch がある");
        let launch_end = body[launch_at..]
            .find("\n/// [`RunPlan`]")
            .map(|e| launch_at + e)
            .unwrap_or(body.len());
        let launch = &body[launch_at..launch_end];
        assert!(
            launch.contains("POSIX シェル"),
            "どのシェル向けの行なのか言っていない"
        );
        // 注記は標準エラーへ (標準出力に混ぜると貼り付けに使えない)
        assert!(launch.contains("eprintln!"), "注記を標準出力へ混ぜている");
    }

    #[test]
    fn ヘルプに全部の操作が出ている() {
        for cmd in [
            "zai cloud doctor",
            "zai cloud target list",
            "zai cloud target add ssh",
            "zai cloud target probe",
            "zai cloud target trust",
            "zai cloud target remove",
            "zai cloud exec",
            "zai cloud shell",
            "zai cloud job run",
            "zai cloud job list",
            "zai cloud provider list",
            "zai cloud provider add hetzner",
            "zai cloud worker create",
            "zai cloud worker destroy",
        ] {
            assert!(HELP.contains(cmd), "ヘルプに {cmd} が無い");
        }
        // 終了コードの表が本文にある
        for code in ["0 = 成功", "2 = 使い方の誤り", "4 ="] {
            assert!(HELP.contains(code), "{code} がヘルプに無い");
        }
    }
}
