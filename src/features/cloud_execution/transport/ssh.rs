//! **SSH の正準実装** — `assets/plugins/remote-host/` にあった組み立てを
//! Rust 側へ移したもの (§4 / §14 / §15)。
//!
//! ## OpenSSH をそのまま使う
//!
//! Rust 製 SSH ライブラリを新規実装しない。system の OpenSSH を使うと
//! `ssh-agent` / OS のキーチェーン / 既存鍵 / `ProxyJump` / `known_hosts` が
//! そのまま効く。**互換性のほうが価値が高い。**
//!
//! ## ただし呼び出しは必ず引数配列で
//!
//! ```ignore
//! // やらない: シェルを 1 枚挟むと、そこから先は文字列の世界になる
//! Command::new("sh").arg("-c").arg(format!("ssh {host} {cmd}"));
//! // やる: 引数は 1 つずつ渡す
//! Command::new("ssh").args(["-p", "22", "-l", user, host, script]);
//! ```
//!
//! ## それでも残る 1 か所 (正直に)
//!
//! **`ssh` は remote 側のコマンドを必ず 1 本の文字列としてリモートのシェルへ
//! 渡す** (引数を複数渡しても空白で連結される。これは OpenSSH の仕様で、
//! こちら側では変えられない)。だから境界に 1 つだけ「リモートのシェル向けに
//! 引用する」処理が要る。その 1 か所が [`posix_quote`] で、
//!
//! * 入口は [`remote_script`] だけ (他から文字列を組ませない)
//! * `'` `"` `;` `&&` `|` `$` `` ` `` 改行 unicode `-host` を並べた表で固定
//!
//! してある ([`tests`])。**「可能な限りシェル文字列を渡さない」(§13) の
//! "可能な限り" が具体的にどこまでかを、ここに書いておく。**
//!
//! ## host key (§15)
//!
//! * `StrictHostKeyChecking=no` は**書かない**。番人テストがソースを走査する。
//! * known_hosts は Zaivern 専用 (`~/.zaivern/cloud/known_hosts`)。
//!   利用者の `~/.ssh/known_hosts` を汚さない。
//! * 作りたてのの VM の**初回だけ** `accept-new` を許す。2 回目からは strict。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use crate::features::cloud_execution::model::{
    CloudError, CollectSink, EventSink, ExecRequest, ExecResult, ExecutionTarget, ProbeResult,
    RemotePath, TargetEndpoint, TransportKind,
};

use super::{parse_probe_output, run_child, ExecutionTransport, PROBE_SCRIPT};

/// host key の確かめ方。**`no` は無い** (型として持たない = 書けない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HostKeyPolicy {
    /// 既知の鍵と一致しなければ断る。**既定はこれ。**
    #[default]
    Strict,
    /// まだ知らない相手なら覚える。既知の鍵と違えば断る。
    ///
    /// **作りたての VM の初回接続にだけ使う** — その時点ではどこにも
    /// 鍵が無く、strict では必ず失敗するため。1 度成功すれば以後は strict。
    AcceptNew,
}

impl HostKeyPolicy {
    fn value(self) -> &'static str {
        match self {
            // **ここに "no" を足さない。** 足した瞬間に、中間者を検出できなくなる
            Self::Strict => "yes",
            Self::AcceptNew => "accept-new",
        }
    }
}

/// SSH の呼び出し方。
#[derive(Debug, Clone)]
pub struct SshOptions {
    pub host_key: HostKeyPolicy,
    /// Zaivern 専用の known_hosts。
    pub known_hosts: PathBuf,
    /// 接続の上限 (秒)。OpenSSH の `ConnectTimeout`。
    pub connect_timeout_secs: u64,
    /// 擬似端末を割り当てるか。
    pub tty: bool,
    /// 鍵以外の認証を試させない (自動実行でパスワードを聞かれて固まらないため)。
    pub batch: bool,
}

impl Default for SshOptions {
    fn default() -> Self {
        Self {
            host_key: HostKeyPolicy::Strict,
            known_hosts: crate::features::cloud_execution::store::known_hosts_path(),
            connect_timeout_secs: 15,
            tty: false,
            batch: true,
        }
    }
}

// ───────────────────────── 入力の検査 ─────────────────────────

/// ホスト名として受け取ってよい形か。
///
/// **`-` で始まるものを断るのが要**。`ssh` から見ると `-oProxyCommand=…` は
/// ホスト名ではなく**オプション**なので、そのまま渡すと任意コマンドが走る。
pub fn validate_host(host: &str) -> Result<(), CloudError> {
    if host.is_empty() {
        return Err(CloudError::config("ホスト名が空です"));
    }
    if host.starts_with('-') {
        return Err(CloudError::security(format!(
            "ホスト名が - で始まっています ({host})。\
             ssh のオプションとして解釈されるため受け付けません"
        )));
    }
    // IPv6 は `[fe80::1%eth0]` の形を許す。それ以外は英数と `.` `-` `_` だけ。
    let body = host
        .strip_prefix('[')
        .and_then(|s| s.strip_suffix(']'))
        .unwrap_or(host);
    let ok = body.chars().all(|c| {
        c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%')
    });
    if !ok {
        return Err(CloudError::security(format!(
            "ホスト名に使えない文字が入っています ({host})"
        )));
    }
    Ok(())
}

/// ユーザー名として受け取ってよい形か。
pub fn validate_user(user: &str) -> Result<(), CloudError> {
    if user.is_empty() {
        return Err(CloudError::config("ユーザー名が空です"));
    }
    if user.starts_with('-') {
        return Err(CloudError::security(format!(
            "ユーザー名が - で始まっています ({user})"
        )));
    }
    let ok = user
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
    if !ok {
        return Err(CloudError::security(format!(
            "ユーザー名に使えない文字が入っています ({user})"
        )));
    }
    Ok(())
}

/// 環境変数の名前として受け取ってよい形か (リモートのシェルへ `K=V` で渡すため)。
fn validate_env_key(key: &str) -> Result<(), CloudError> {
    let ok = !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !key.starts_with(|c: char| c.is_ascii_digit());
    if !ok {
        return Err(CloudError::security(format!(
            "環境変数の名前に使えない文字が入っています ({key})"
        )));
    }
    Ok(())
}

// ───────────────────────── 引用 ─────────────────────────

/// POSIX シェル向けの単一引用符クォート。
///
/// 単一引用符の中では**あらゆる文字が字義どおり**になる。唯一の例外は
/// `'` 自身なので、そこだけ `'\''` (閉じる → エスケープした ' → 開く) で継ぐ。
/// これで空白・`;`・`&&`・`|`・`$`・`` ` ``・改行・unicode のすべてが安全になる。
pub fn posix_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for ch in s.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// [`ExecRequest`] を、リモートのシェルが受け取る 1 本の文字列へ畳む。
///
/// **文字列を組むのはここだけ。** 呼び出し側は `program` と `args` を
/// 構造のまま渡す。
pub fn remote_script(req: &ExecRequest) -> Result<String, CloudError> {
    if req.program.is_empty() {
        return Err(CloudError::config("実行するコマンドがありません"));
    }
    let mut parts: Vec<String> = Vec::new();

    if let Some(cwd) = &req.cwd {
        // `~` で始まるときだけリモートに展開させる (引用すると `~` が
        // ディレクトリ名として扱われて必ず失敗する)。それ以外は必ず引用。
        let arg = if let Some(rest) = cwd.strip_prefix("~/") {
            format!("~/{}", strip_tilde_quote(rest))
        } else if cwd == "~" {
            "~".to_string()
        } else {
            posix_quote(cwd)
        };
        parts.push(format!("cd {arg}"));
        parts.push("&&".to_string());
    }

    if !req.env.is_empty() {
        parts.push("env".to_string());
        for (k, v) in &req.env {
            validate_env_key(k)?;
            parts.push(format!("{k}={}", posix_quote(v)));
        }
    }

    parts.push(posix_quote(&req.program));
    for a in &req.args {
        parts.push(posix_quote(a));
    }
    Ok(parts.join(" "))
}

/// `~/` の後ろは展開させないので、単一引用符で包み直す。
/// `~/'my dir'` の形になり、`~` だけが展開される。
fn strip_tilde_quote(rest: &str) -> String {
    posix_quote(rest)
}

// ───────────────────────── コマンドの組み立て ─────────────────────────

/// 実行先から SSH の接続情報を取り出す。
fn endpoint_of(target: &ExecutionTarget) -> Result<(&str, &str, u16, Option<&Path>), CloudError> {
    match &target.endpoint {
        TargetEndpoint::Ssh {
            host,
            user,
            port,
            identity_file,
        } => {
            validate_host(host)?;
            validate_user(user)?;
            Ok((host, user, *port, identity_file.as_deref()))
        }
        TargetEndpoint::Local => Err(CloudError::unsupported(
            "この実行先はローカルなので SSH では動かせません",
        )),
    }
}

/// `ssh` に渡す引数を組む (リモートで走らせる中身は含まない)。
///
/// 返すのは**引数の配列**であって文字列ではない。ここを 1 本の文字列にすると、
/// そこから先は誰でも文字を足せる場所になる。
pub fn ssh_argv(target: &ExecutionTarget, opts: &SshOptions) -> Result<Vec<String>, CloudError> {
    let (host, user, port, identity) = endpoint_of(target)?;
    let mut argv: Vec<String> = Vec::new();

    if opts.tty {
        // 2 つ重ねると「端末が無くても割り当てる」。対話するときだけ。
        argv.push("-tt".to_string());
    } else {
        // 端末を割り当てない (自動実行が端末の設定に左右されない)
        argv.push("-T".to_string());
    }
    if opts.batch {
        // 鍵で入れなければ**聞かずに失敗する**。聞くと自動実行が固まる。
        argv.push("-o".into());
        argv.push("BatchMode=yes".into());
    }
    argv.push("-o".into());
    argv.push(format!("StrictHostKeyChecking={}", opts.host_key.value()));
    argv.push("-o".into());
    argv.push(format!(
        "UserKnownHostsFile={}",
        opts.known_hosts.display()
    ));
    // **利用者の known_hosts を読むが書かない**、が既定の OpenSSH の挙動なので
    // 明示的に「Zaivern の分だけを見る」ことにする (混ざると消せなくなる)。
    argv.push("-o".into());
    argv.push("GlobalKnownHostsFile=/dev/null".into());
    argv.push("-o".into());
    argv.push(format!("ConnectTimeout={}", opts.connect_timeout_secs));
    // 相手が黙り込んだまま握られ続けるのを防ぐ (§48 の「永久待ち禁止」)
    argv.push("-o".into());
    argv.push("ServerAliveInterval=15".into());
    argv.push("-o".into());
    argv.push("ServerAliveCountMax=3".into());

    if let Some(id) = identity {
        argv.push("-i".into());
        argv.push(id.display().to_string());
        // 明示した鍵だけを使う (agent の鍵を総当たりして弾かれるのを防ぐ)
        argv.push("-o".into());
        argv.push("IdentitiesOnly=yes".into());
    }
    argv.push("-p".into());
    argv.push(port.to_string());
    // **`user@host` へ畳まない。** `-l` で分けておくと、ユーザー名に `@` が
    // 混ざったときの取り違えが起こらない。
    argv.push("-l".into());
    argv.push(user.to_string());
    argv.push(host.to_string());
    Ok(argv)
}

/// 実行の 1 回ぶんの `ssh` コマンド。
pub fn ssh_command(
    target: &ExecutionTarget,
    req: &ExecRequest,
    opts: &SshOptions,
) -> Result<Command, CloudError> {
    let mut argv = ssh_argv(target, opts)?;
    argv.push(remote_script(req)?);
    let mut cmd = crate::procx::hidden_command("ssh");
    cmd.args(&argv);
    Ok(cmd)
}

/// 対話シェルを開くための `ssh` コマンド (`zai cloud shell`)。
///
/// 実行するコマンドを渡さないので、リモートのログインシェルがそのまま開く。
pub fn ssh_shell_command(
    target: &ExecutionTarget,
    opts: &SshOptions,
) -> Result<Command, CloudError> {
    let mut opts = opts.clone();
    opts.tty = true;
    // 対話ではパスフレーズを聞かれてよい (人が見ている)
    opts.batch = false;
    let argv = ssh_argv(target, &opts)?;
    let mut cmd = crate::procx::hidden_command("ssh");
    cmd.args(&argv);
    Ok(cmd)
}

/// 既存の PTY セッション ([`crate::terminal::Session`]) へ渡すための**コマンド行**。
///
/// 既存の起動経路は「1 本のコマンド行」を受け取る形なので、ここだけは
/// 文字列へ畳む。畳み方は [`posix_quote`] と同じ規則を使う。
pub fn ssh_command_line(target: &ExecutionTarget, opts: &SshOptions) -> Result<String, CloudError> {
    let argv = ssh_argv(target, opts)?;
    let mut line = String::from("ssh");
    for a in argv {
        line.push(' ');
        line.push_str(&posix_quote(&a));
    }
    Ok(line)
}

/// `scp` のコマンド (`upload` / `download` が使う)。
fn scp_command(
    target: &ExecutionTarget,
    opts: &SshOptions,
    from: &str,
    to: &str,
) -> Result<Command, CloudError> {
    let (host, user, port, identity) = endpoint_of(target)?;
    let _ = (host, user);
    let mut cmd = crate::procx::hidden_command("scp");
    cmd.arg("-o").arg("BatchMode=yes");
    cmd.arg("-o")
        .arg(format!("StrictHostKeyChecking={}", opts.host_key.value()));
    cmd.arg("-o")
        .arg(format!("UserKnownHostsFile={}", opts.known_hosts.display()));
    cmd.arg("-o")
        .arg(format!("ConnectTimeout={}", opts.connect_timeout_secs));
    if let Some(id) = identity {
        cmd.arg("-i").arg(id);
        cmd.arg("-o").arg("IdentitiesOnly=yes");
    }
    // scp のポート指定は大文字 `-P` (ssh の `-p` と違う)
    cmd.arg("-P").arg(port.to_string());
    // **`--` で以降を引数として固定する。** `-` で始まるパスを渡されても
    // オプションとして解釈されない。
    cmd.arg("--").arg(from).arg(to);
    Ok(cmd)
}

/// `user@host:path` の形。**`scp` はこの形しか受けない。**
fn scp_remote_spec(target: &ExecutionTarget, path: &RemotePath) -> Result<String, CloudError> {
    let (host, user, _, _) = endpoint_of(target)?;
    // IPv6 は `[...]` のまま渡す
    Ok(format!("{user}@{host}:{}", path.as_str()))
}

// ───────────────────────── Transport ─────────────────────────

/// system の OpenSSH を使う Transport。
pub struct SshTransport {
    timeout: Duration,
    opts: SshOptions,
}

impl SshTransport {
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout,
            opts: SshOptions {
                connect_timeout_secs: timeout.as_secs().clamp(5, 120),
                ..SshOptions::default()
            },
        }
    }

    /// host key の扱いを変えた写し (**作りたての VM の初回だけ**)。
    pub fn with_host_key(mut self, policy: HostKeyPolicy) -> Self {
        self.opts.host_key = policy;
        self
    }
}

impl ExecutionTransport for SshTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Ssh
    }

    fn probe(&self, target: &ExecutionTarget) -> Result<ProbeResult, CloudError> {
        let req = ExecRequest::new("sh", ["-c".to_string(), PROBE_SCRIPT.to_string()]);
        let mut sink = CollectSink::with_limit(64 * 1024);
        let started = Instant::now();
        let cmd = ssh_command(target, &req, &self.opts)?;
        let res = run_child(cmd, self.timeout, "ssh", &mut sink);
        let latency = started.elapsed().as_millis() as u64;
        match res {
            Ok(r) if r.ok() => {
                let mut p = parse_probe_output(&sink.stdout_text());
                p.latency_ms = latency;
                if !p.reachable {
                    p.error = crate::features::cloud_execution::redact::redact(
                        sink.stderr_text().trim(),
                    );
                }
                Ok(p)
            }
            Ok(r) => Ok(ProbeResult {
                reachable: false,
                latency_ms: latency,
                error: ssh_failure_hint(r.exit_code, &sink.stderr_text()),
                ..ProbeResult::default()
            }),
            Err(e) => Ok(ProbeResult {
                reachable: false,
                latency_ms: latency,
                error: e.to_string(),
                ..ProbeResult::default()
            }),
        }
    }

    fn exec(
        &self,
        target: &ExecutionTarget,
        request: &ExecRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ExecResult, CloudError> {
        let mut opts = self.opts.clone();
        opts.tty = request.tty;
        let cmd = ssh_command(target, request, &opts)?;
        let timeout = request.timeout.unwrap_or(self.timeout);
        run_child(cmd, timeout, "ssh", sink)
    }

    fn upload(
        &self,
        target: &ExecutionTarget,
        source: &Path,
        destination: &RemotePath,
    ) -> Result<(), CloudError> {
        let to = scp_remote_spec(target, destination)?;
        let cmd = scp_command(target, &self.opts, &source.display().to_string(), &to)?;
        let mut sink = CollectSink::with_limit(64 * 1024);
        let r = run_child(cmd, self.timeout, "scp", &mut sink)?;
        if r.ok() {
            Ok(())
        } else {
            Err(CloudError::transport(format!(
                "送れませんでした: {}",
                sink.stderr_text().trim()
            )))
        }
    }

    fn download(
        &self,
        target: &ExecutionTarget,
        source: &RemotePath,
        destination: &Path,
    ) -> Result<(), CloudError> {
        let from = scp_remote_spec(target, source)?;
        let cmd = scp_command(
            target,
            &self.opts,
            &from,
            &destination.display().to_string(),
        )?;
        let mut sink = CollectSink::with_limit(64 * 1024);
        let r = run_child(cmd, self.timeout, "scp", &mut sink)?;
        if r.ok() {
            Ok(())
        } else {
            Err(CloudError::transport(format!(
                "受け取れませんでした: {}",
                sink.stderr_text().trim()
            )))
        }
    }
}

/// `ssh` の失敗を、利用者が次に打つ手が分かる形へ言い換える。
fn ssh_failure_hint(exit: Option<i32>, stderr: &str) -> String {
    let e = crate::features::cloud_execution::redact::redact(stderr.trim());
    let low = e.to_lowercase();
    if low.contains("host key verification failed") || low.contains("remote host identification") {
        return format!(
            "ホスト鍵が既知のものと違います。中間者攻撃の可能性があるため接続しません。\n\
             作り直した機械なら {} の該当行を消してから再実行してください\n{e}",
            crate::features::cloud_execution::store::known_hosts_path().display()
        );
    }
    if low.contains("permission denied") {
        return format!("鍵で認証できませんでした (ssh-agent か --identity-file を確認してください)\n{e}");
    }
    if low.contains("connection refused") || low.contains("could not resolve") {
        return format!("接続できませんでした\n{e}");
    }
    match exit {
        Some(c) => format!("ssh が {c} で終了しました\n{e}"),
        None => format!("ssh が異常終了しました\n{e}"),
    }
}

/// リモートの環境変数として渡してよいものだけを残す。
///
/// **エージェントの認証情報を勝手に運ばない** (§24)。ここを通さずに
/// `std::env::vars()` を丸ごと渡すと、手元のトークンがリモートへ流れる。
pub fn forwardable_env(env: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    env.iter()
        .filter(|(k, _)| validate_env_key(k).is_ok())
        .filter(|(k, _)| !looks_secret(k))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// 名前だけで「秘密らしい」と判る環境変数か。
fn looks_secret(key: &str) -> bool {
    let k = key.to_ascii_uppercase();
    ["TOKEN", "SECRET", "PASSWORD", "PASSWD", "API_KEY", "APIKEY", "CREDENTIAL"]
        .iter()
        .any(|m| k.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::test_support::{ssh_target, TargetOpts};

    fn opts() -> SshOptions {
        SshOptions {
            known_hosts: PathBuf::from("/tmp/kh"),
            ..SshOptions::default()
        }
    }

    #[test]
    fn ssh_never_disables_host_key_checking() {
        // 1. 組み立てた引数に "no" が出てこない
        let t = ssh_target("dev-01", TargetOpts::default());
        let argv = ssh_argv(&t, &opts()).expect("組める");
        let joined = argv.join(" ");
        assert!(
            joined.contains("StrictHostKeyChecking=yes"),
            "strict になっていない: {joined}"
        );
        assert!(
            !joined.contains("StrictHostKeyChecking=no"),
            "無効化している: {joined}"
        );
        // accept-new でも "no" にはならない
        let mut o = opts();
        o.host_key = HostKeyPolicy::AcceptNew;
        let joined = ssh_argv(&t, &o).expect("組める").join(" ");
        assert!(joined.contains("StrictHostKeyChecking=accept-new"), "{joined}");

        // 2. **ソースにも書かれていない。** 型で防いでいるつもりでも、
        //    別の場所で文字列を組まれたら終わりなので走査で確かめる。
        let product = product_code(include_str!("ssh.rs"));
        for banned in [
            "StrictHostKeyChecking=no",
            "UserKnownHostsFile=/dev/null",
            "CheckHostIP=no",
        ] {
            assert!(
                !product.contains(banned),
                "{banned:?} が製品コードに書かれている"
            );
        }
    }

    #[test]
    fn ssh_rejects_option_like_host() {
        // `-oProxyCommand=…` をホスト名として渡されると任意コマンドが走る
        for host in [
            "-oProxyCommand=touch /tmp/pwned",
            "-l root",
            "--",
            "-",
        ] {
            let t = ssh_target(host, TargetOpts::default());
            let e = ssh_argv(&t, &opts()).expect_err("断る: {host}");
            assert!(matches!(e, CloudError::Security(_)), "{host}: {e:?}");
        }
        // ユーザー名も同じ
        let mut t = ssh_target("example.com", TargetOpts::default());
        if let TargetEndpoint::Ssh { user, .. } = &mut t.endpoint {
            *user = "-oProxyCommand=x".into();
        }
        assert!(matches!(
            ssh_argv(&t, &opts()),
            Err(CloudError::Security(_))
        ));
    }

    #[test]
    fn ssh_rejects_injected_host() {
        for host in [
            "example.com; touch /tmp/x",
            "example.com && id",
            "example.com | id",
            "example.com$(id)",
            "example.com`id`",
            "exa mple.com",
            "example.com\nid",
            "例え.com",
        ] {
            let t = ssh_target(host, TargetOpts::default());
            assert!(
                ssh_argv(&t, &opts()).is_err(),
                "通してしまった: {host:?}"
            );
        }
        // まっとうなものは通る
        for host in ["example.com", "10.0.0.1", "[fe80::1]", "host-1_a.example"] {
            let t = ssh_target(host, TargetOpts::default());
            assert!(ssh_argv(&t, &opts()).is_ok(), "断ってしまった: {host:?}");
        }
    }

    #[test]
    fn ssh_quotes_spaces() {
        let req = ExecRequest::new("echo", vec!["hello world".to_string()])
            .with_cwd("/srv/my project");
        let s = remote_script(&req).expect("組める");
        assert_eq!(s, "cd '/srv/my project' && 'echo' 'hello world'");
    }

    #[test]
    fn ssh_quotes_single_quotes() {
        let req = ExecRequest::new("echo", vec!["it's a 'test'".to_string()]);
        let s = remote_script(&req).expect("組める");
        assert_eq!(s, r#"'echo' 'it'\''s a '\''test'\'''"#);
        // 実際に POSIX sh へ食わせて、元の文字列が戻ることまで確かめる
        assert_eq!(sh_echo("it's a 'test'"), "it's a 'test'");
    }

    #[test]
    fn ssh_handles_unicode() {
        let req = ExecRequest::new("echo", vec!["日本語 と 絵文字 🌏".to_string()]);
        let s = remote_script(&req).expect("組める");
        assert!(s.contains("'日本語 と 絵文字 🌏'"), "{s}");
        assert_eq!(sh_echo("日本語 と 絵文字 🌏"), "日本語 と 絵文字 🌏");
    }

    /// **注入されうる文字を並べた表。** 1 つでも素通ししたら赤くなる。
    #[test]
    fn 危ない文字はすべて字義どおりに渡る() {
        const CASES: &[&str] = &[
            "a b",
            "it's",
            "say \"hi\"",
            "a;id",
            "a&&id",
            "a||id",
            "a|id",
            "$(id)",
            "${HOME}",
            "`id`",
            "a\nb",
            "a\tb",
            "-rf",
            "--host",
            "*",
            "~/secret",
            "\\",
            "🌏",
        ];
        for c in CASES {
            let q = posix_quote(c);
            // 引用の外に危ない字が残っていないこと
            assert!(q.starts_with('\'') && q.ends_with('\''), "{c:?} → {q}");
            // 実際に sh へ通して、1 バイトも変わらずに戻ること
            assert_eq!(&sh_echo(c), c, "{c:?} が変わって届く");
        }
    }

    /// 実 `sh` を通して「引用が効いている」ことを確かめる補助。
    ///
    /// **`printf '%s'` を使う** — `echo` は実装によって `\n` を解釈するので、
    /// 制御文字を含む入力で嘘の結果が出る。
    #[cfg(unix)]
    fn sh_echo(text: &str) -> String {
        let script = format!("printf '%s' {}", posix_quote(text));
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(&script)
            .output()
            .expect("sh を起動できる");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Windows には POSIX sh が居ないので、規則そのものを検算する。
    ///
    /// **引用の中身を機械的に戻す** (`'\''` を `'` へ) ことで、
    /// 「1 バイトも変わらずに渡る」を実 sh 無しで確かめる。
    #[cfg(not(unix))]
    fn sh_echo(text: &str) -> String {
        let q = posix_quote(text);
        let inner = &q[1..q.len() - 1];
        inner.replace("'\\''", "'")
    }

    #[test]
    fn ホーム相対の作業ディレクトリは展開させる() {
        // `~` を引用すると、ディレクトリ名 "~" を探して必ず失敗する
        let req = ExecRequest::new("pwd", vec![]).with_cwd("~/my work");
        let s = remote_script(&req).expect("組める");
        assert!(s.starts_with("cd ~/'my work' &&"), "{s}");
        // ~ の後ろは引用されているので空白があっても割れない
        assert!(!s.contains("cd ~/my work &&"), "{s}");
    }

    #[test]
    fn 環境変数は名前を検査してから渡す() {
        let req = ExecRequest::new("env", vec![])
            .with_env("ZV_JOB", "a b")
            .with_env("ZV_X", "'; id; '");
        let s = remote_script(&req).expect("組める");
        assert!(s.contains("env ZV_JOB='a b'"), "{s}");
        assert!(s.contains(r#"ZV_X=''\''; id; '\'''"#), "{s}");

        // 名前に危ない字が入っていたら断る (値と違い、名前は引用できない)
        let bad = ExecRequest::new("env", vec![]).with_env("A=B; id", "x");
        assert!(matches!(remote_script(&bad), Err(CloudError::Security(_))));
    }

    #[test]
    fn 秘密らしい環境変数は転送しない() {
        let mut env = BTreeMap::new();
        env.insert("ZV_JOB".to_string(), "1".to_string());
        env.insert("HCLOUD_TOKEN".to_string(), "x".to_string());
        env.insert("MY_API_KEY".to_string(), "x".to_string());
        env.insert("GH_PASSWORD".to_string(), "x".to_string());
        let out = forwardable_env(&env);
        assert_eq!(out.keys().collect::<Vec<_>>(), vec!["ZV_JOB"]);
    }

    #[test]
    fn 引数は配列で渡しシェルを挟まない() {
        // `sh -c` を挟むと、そこから先は文字列の世界になる。
        // **コメント行は除く** — 冒頭の「やらない例」がまさにこの綴りなので、
        // 除かないと番人が自分の説明で赤くなる。
        let body = product_code(include_str!("ssh.rs"));
        assert!(
            !body.contains("Command::new(\"sh\")") && !body.contains("Command::new(\"cmd\")"),
            "SSH の組み立てでシェルを挟んでいる"
        );
        // **番人が空回りしていないことを証明する**
        assert!(
            product_code("let c = Command::new(\"sh\");\n").contains("Command::new(\"sh\")"),
            "番人が空回りしている"
        );
    }

    /// 製品コードだけを残す (コメント行と `#[cfg(test)]` 以降を落とす)。
    fn product_code(src: &str) -> String {
        let text = src.replace("\r\n", "\n");
        let head = text.split("#[cfg(test)]").next().unwrap_or_default().to_string();
        head.lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('*')
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn 空のコマンドは断る() {
        let req = ExecRequest::new("", vec![]);
        assert!(remote_script(&req).is_err());
    }

    #[test]
    fn ポートと鍵は構造のまま渡る() {
        let t = ssh_target(
            "example.com",
            TargetOpts {
                port: 2222,
                identity_file: Some(PathBuf::from("/home/u/.ssh/id_ed25519")),
                ..TargetOpts::default()
            },
        );
        let argv = ssh_argv(&t, &opts()).expect("組める");
        let joined = argv.join(" ");
        assert!(joined.contains("-p 2222"), "{joined}");
        assert!(joined.contains("-i /home/u/.ssh/id_ed25519"), "{joined}");
        assert!(joined.contains("IdentitiesOnly=yes"), "{joined}");
        // user@host へ畳んでいない
        assert!(joined.contains("-l zaivern"), "{joined}");
    }

    #[test]
    fn ホスト鍵の不一致は次の手が分かる形で返す() {
        let msg = ssh_failure_hint(Some(255), "Host key verification failed.");
        assert!(msg.contains("中間者"), "{msg}");
        assert!(msg.contains("known_hosts"), "{msg}");
    }

    #[test]
    fn ローカルの実行先はsshで動かさない() {
        let t = crate::features::cloud_execution::model::ExecutionTarget::local(1);
        assert!(matches!(
            ssh_argv(&t, &opts()),
            Err(CloudError::Unsupported(_))
        ));
    }
}
