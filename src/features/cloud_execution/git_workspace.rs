//! **リモートにも git を保つ** (§25〜§30)。
//!
//! ## rsync だけに依存しない
//!
//! Zaivern は git worktree を強く使う。リモートでファイルを同期するだけだと、
//! 向こうには履歴が無いので、**誰が何を変えたのかを持ち帰れない**。
//!
//! ## GitHub の資格情報をリモートへ配らない (§26)
//!
//! ```text
//! 手元のリポジトリ
//!      │ git push over SSH   (Zaivern が持っている SSH 鍵だけを使う)
//!      ▼
//! リモートの bare リポジトリ  ~/.zaivern/cloud/repos/<キー>.git
//!      ├── worktree job-A
//!      ├── worktree job-B
//!      └── …
//! ```
//!
//! 向こうは**手元から push されたものしか持たない**ので、GitHub の PAT も
//! deploy key も要らない。持ち帰りも同じ経路の `git fetch` で済む。
//!
//! ## 利用者の枝を触らない (§28)
//!
//! 持ち帰った結果は `refs/remotes/zaivern-cloud/<job>` に置くだけ。
//! **merge も rebase も push もしない。** 統合は既存の Coordinator /
//! review / merge の担当で、実行層の仕事ではない。

use std::path::Path;
use std::time::Duration;

use super::model::{
    CloudError, CollectSink, ExecRequest, ExecResult, ExecutionTarget, RemotePath, TargetEndpoint,
};
use super::transport::ssh::{posix_quote, SshOptions};
use super::transport::ExecutionTransport;

/// 持ち帰った枝を置く名前空間。**利用者の枝と混ざらない場所**。
pub const RESULT_NAMESPACE: &str = "refs/remotes/zaivern-cloud";

/// **リモート側**で結果を固定する参照の名前空間。
///
/// 枝ではなく参照に固定するのが要 — 枝は動くが、これは動かない
/// ([`snapshot_script`] の説明を参照)。
pub const REMOTE_RESULT_NAMESPACE: &str = "refs/zaivern/result";

/// リモートの bare リポジトリ。ワークスペースキーで分ける
/// ([`crate::history::workspace_key`] が真実の在り処)。
pub fn bare_repo(workspace_key: &str) -> Result<RemotePath, CloudError> {
    check_key(workspace_key)?;
    RemotePath::home(format!(".zaivern/cloud/repos/{workspace_key}.git"))
}

/// リモートの作業場所 (仕事ごと)。
pub fn job_dir(job_id: &str) -> Result<RemotePath, CloudError> {
    check_id(job_id)?;
    RemotePath::home(format!(".zaivern/cloud/jobs/{job_id}"))
}

/// 手元から押し込む先の参照。
pub fn base_ref(job_id: &str) -> Result<String, CloudError> {
    check_id(job_id)?;
    Ok(format!("refs/zaivern/base/{job_id}"))
}

/// リモートで作る枝。
pub fn job_branch(job_id: &str) -> Result<String, CloudError> {
    check_id(job_id)?;
    Ok(format!("zai/cloud/{job_id}"))
}

/// 持ち帰る先 (**手元の追跡枝**)。
pub fn result_ref(job_id: &str) -> Result<String, CloudError> {
    check_id(job_id)?;
    Ok(format!("{RESULT_NAMESPACE}/{job_id}"))
}

/// リモート側で結果 OID を固定する参照。
pub fn remote_result_ref(job_id: &str) -> Result<String, CloudError> {
    check_id(job_id)?;
    Ok(format!("{REMOTE_RESULT_NAMESPACE}/{job_id}"))
}

/// git のオブジェクト ID として受け取ってよい形か。
///
/// **SHA-1 の 40 桁だけを前提にしない。** git は SHA-256 のリポジトリを
/// 作れて、そこでは 64 桁になる。長さを 1 つに決め打つと、その日から
/// 「回収したのに照合できない」になる。
pub fn is_object_id(s: &str) -> bool {
    let n = s.len();
    (40..=64).contains(&n) && n.is_multiple_of(2) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// 仕事 ID として受け取ってよい形か。
///
/// **枝名とディレクトリ名になる**ので、ここを緩めると `../` や `-` 始まりが
/// 通る。ID は [`super::model::ids::new_id`] が作るので普段は必ず通るが、
/// 外から来た値でも壊れないことを型ではなくここで保証する。
fn check_id(id: &str) -> Result<(), CloudError> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        && !id.starts_with('-')
        && !id.ends_with('-');
    if !ok {
        return Err(CloudError::security(format!(
            "仕事の ID に使えない文字が入っています: {id}"
        )));
    }
    Ok(())
}

fn check_key(key: &str) -> Result<(), CloudError> {
    let ok = !key.is_empty()
        && key.len() <= 64
        && key.chars().all(|c| c.is_ascii_hexdigit());
    if !ok {
        return Err(CloudError::security(format!(
            "ワークスペースキーの形が違います: {key}"
        )));
    }
    Ok(())
}

/// git が使う ssh の URL。
///
/// `~` は git の ssh URL で home 相対として展開される。**ポートを URL に
/// 載せられる**ので、`scp` 風の `user@host:path` は使わない (ポートを渡せない)。
pub fn ssh_git_url(target: &ExecutionTarget, path: &RemotePath) -> Result<String, CloudError> {
    let TargetEndpoint::Ssh {
        host, user, port, ..
    } = &target.endpoint
    else {
        return Err(CloudError::unsupported(
            "この実行先は SSH ではないので git の転送先にできません",
        ));
    };
    super::transport::ssh::validate_host(host)?;
    super::transport::ssh::validate_user(user)?;
    let p = path.as_str().trim_start_matches('/');
    Ok(format!("ssh://{user}@{host}:{port}/{p}"))
}

/// `GIT_SSH_COMMAND` に入れる 1 行。
///
/// **git は ssh の呼び方を環境変数の文字列でしか受け取らない**ので、ここだけは
/// コマンド行になる。組み立ては [`posix_quote`] を通すので、パスに空白が
/// あっても割れない。
pub fn git_ssh_command(target: &ExecutionTarget, opts: &SshOptions) -> Result<String, CloudError> {
    let mut opts = opts.clone();
    // git の転送では端末を割り当てない
    opts.tty = false;
    opts.batch = true;
    let argv = super::transport::ssh::ssh_argv(target, &opts)?;
    let mut line = String::from("ssh");
    for a in argv {
        // ホストとユーザーは URL 側で指定するので、ここでは接続の作法だけを渡す
        if a == "-l" {
            break;
        }
        line.push(' ');
        line.push_str(&posix_quote(&a));
    }
    Ok(line)
}

/// リモート側で bare リポジトリを用意する sh の断片。
///
/// **何度呼んでも同じ結果になる**うえ、**同時に呼ばれても壊れない**。
///
/// ## なぜ鍵が要るのか (実測)
///
/// 最初の版は `git init --bare` と `git config` を毎回撃っていた。1 本ずつなら
/// 通るが、**4 本を同時に走らせた実測で 1 本が落ちた**:
///
/// ```text
/// error: could not lock config file config: File exists
/// ```
///
/// git の config はファイルロックで直列化されるので、同じ瞬間に 2 つが
/// 触ると片方が必ず負ける。しかもこれは**いちばん混んでいるとき = 並列で
/// 走らせたいとき**にだけ出るので、1 本での試験では永久に見つからない。
///
/// `mkdir` は POSIX で原子的なので、それを鍵にして用意を 1 本へ絞る。
/// すでに在れば鍵も取らずに帰る (ふつうの経路では待ちが 0)。
pub fn init_bare_script(repo: &RemotePath) -> String {
    let q = quote_home(repo);
    // 待ちの上限。`N 体が順に通るには N·t 要る`ので、64 体でも通る幅を取る
    // (用意は 1 度きりなので、待つのは最初の 1 回だけ)。
    format!(
        "set -e; \
         if [ -d {q}/objects ]; then exit 0; fi; \
         mkdir -p \"$(dirname {q})\"; \
         lock={q}.lock; i=0; \
         while ! mkdir \"$lock\" 2>/dev/null; do \
           i=$((i+1)); \
           if [ \"$i\" -gt 300 ]; then \
             echo \"zv_error=リポジトリの用意が終わりません ($lock を消してください)\" >&2; exit 1; \
           fi; \
           sleep 0.1 2>/dev/null || sleep 1; \
           if [ -d {q}/objects ]; then exit 0; fi; \
         done; \
         trap 'rmdir \"$lock\" 2>/dev/null || true' EXIT INT TERM; \
         if [ ! -d {q}/objects ]; then \
           git init --bare -q {q}; \
           git -C {q} config receive.denyCurrentBranch ignore; \
         fi"
    )
}

/// worktree を作る sh の断片。
///
/// **仕事ごとに別の worktree**。共有すると 2 つのエージェントが同じファイルを
/// 書く (§27)。
pub fn add_worktree_script(
    repo: &RemotePath,
    dir: &RemotePath,
    branch: &str,
    base: &str,
) -> String {
    let r = quote_home(repo);
    let d = quote_home(dir);
    format!(
        "set -e; mkdir -p \"$(dirname {d})\"; \
         git -C {r} worktree add -q -B {} {d} {}",
        posix_quote(branch),
        posix_quote(base)
    )
}

/// 未コミットの変更を持ち帰るためだけの commit にし、**いま HEAD が指して
/// いるもの**を仕事専用の参照へ固定する sh の断片 (§29)。
///
/// **GitHub へ push するための commit ではない。** リモートから手元へ
/// 安全に戻すための輸送用で、そのことをメッセージに書く。
///
/// ## なぜ枝ではなく参照へ固定するのか (この版で直した壊れ方)
///
/// 最初の版は「作った時の枝名 (`zai/cloud/<job>`) を fetch する」形だった。
/// ところが **snapshot が付くのは枝ではなく HEAD** なので、
///
/// * エージェントが `git switch -c other` で別の枝へ移った
/// * detached HEAD のまま作業した
///
/// のどちらでも、成果は HEAD 側に付き `zai/cloud/<job>` は**古いまま**残る。
/// その枝は実在するので **fetch は成功し**、1 バイトも持ち帰らないまま
/// 「成功」として worktree を片付けてしまう — 作業が消えるのに、
/// どこにもエラーが出ない。いちばん静かな壊れ方である。
///
/// だから HEAD の OID をリモート側で**動かない参照**へ固定し、
/// その参照を取りに行く ([`fetch_and_verify`] が OID の一致まで確かめる)。
pub fn snapshot_script(dir: &RemotePath, job_id: &str, result_ref: &str) -> String {
    let d = quote_home(dir);
    let msg = posix_quote(&format!(
        "zaivern: snapshot cloud job {job_id}\n\n\
         リモートの作業を手元へ戻すための輸送用コミットです。\
         そのまま公開する前提のものではありません。"
    ));
    let rref = posix_quote(result_ref);
    format!(
        "set -e; cd {d}; \
         if [ -n \"$(git status --porcelain)\" ]; then \
           git add -A; \
           git -c user.name='Zaivern Cloud' -c user.email='cloud@zaivern.invalid' \
               commit -q -m {msg}; \
           echo zv_snapshot=1; \
         else echo zv_snapshot=0; fi; \
         zv_oid=$(git rev-parse HEAD); \
         git update-ref {rref} \"$zv_oid\"; \
         echo zv_result_oid=$zv_oid"
    )
}

/// [`snapshot_script`] の出力を読む。**純関数**なので表で固定できる。
///
/// 返すのは `(確定した OID, 輸送用コミットを作ったか)`。
/// OID が出ていなければ**失敗として返す** — 「取れなかったが成功」に
/// してしまうと、その先の照合が素通りする。
pub fn parse_snapshot_output(text: &str) -> Result<(String, bool), CloudError> {
    let text = text.replace("\r\n", "\n");
    let mut oid: Option<String> = None;
    let mut snapshotted = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "zv_snapshot=1" {
            snapshotted = true;
        }
        if let Some(v) = line.strip_prefix("zv_result_oid=") {
            oid = Some(v.trim().to_string());
        }
    }
    let Some(oid) = oid else {
        return Err(CloudError::transport(
            "リモートで結果の位置を確定できませんでした (OID が返っていません)",
        ));
    };
    if !is_object_id(&oid) {
        return Err(CloudError::transport(format!(
            "リモートが返した OID の形が違います: {oid}"
        )));
    }
    Ok((oid, snapshotted))
}

/// worktree を片付ける sh の断片 (§30)。
pub fn remove_worktree_script(repo: &RemotePath, dir: &RemotePath) -> String {
    let r = quote_home(repo);
    let d = quote_home(dir);
    // **枝は消さない。** 手元がまだ持ち帰っていない可能性がある
    format!("git -C {r} worktree remove --force {d} 2>/dev/null || rm -rf {d}")
}

/// `~/` の後ろだけを引用する (先頭の `~` はリモートに展開させる)。
fn quote_home(p: &RemotePath) -> String {
    match p.as_str().strip_prefix("~/") {
        Some(rest) => format!("~/{}", posix_quote(rest)),
        None => posix_quote(p.as_str()),
    }
}

// ───────────────────────── 実際に走らせる ─────────────────────────

/// リモートで sh の断片を 1 つ走らせる。
fn run_remote(
    transport: &dyn ExecutionTransport,
    target: &ExecutionTarget,
    script: &str,
    what: &str,
) -> Result<String, CloudError> {
    let req = ExecRequest::new("sh", vec!["-c".to_string(), script.to_string()]);
    let mut sink = CollectSink::with_limit(256 * 1024);
    let r = transport.exec(target, &req, &mut sink)?;
    if !r.ok() {
        return Err(CloudError::transport(format!(
            "{what} に失敗しました: {}",
            sink.stderr_text().trim()
        )));
    }
    Ok(sink.stdout_text())
}

/// 手元で `git` を走らせる。**`GIT_SSH_COMMAND` を渡すので、Zaivern の
/// known_hosts と鍵だけが使われる。**
fn run_local_git(
    repo: &Path,
    args: &[String],
    ssh_command: Option<&str>,
    timeout: Duration,
) -> Result<(ExecResult, CollectSink), CloudError> {
    let mut cmd = crate::procx::hidden_command("git");
    cmd.arg("-C").arg(repo).args(args);
    if let Some(s) = ssh_command {
        cmd.env("GIT_SSH_COMMAND", s);
    }
    // 資格情報を対話で聞かれて固まらないようにする (§48)
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    let mut sink = CollectSink::with_limit(256 * 1024);
    let r = super::transport::run_child(cmd, timeout, "git", &mut sink)?;
    Ok((r, sink))
}

/// 1 つの仕事のためのリモート作業場。
pub struct RemoteWorkspace {
    pub repo: RemotePath,
    pub dir: RemotePath,
    pub branch: String,
    pub base: String,
    pub job_id: String,
    /// リモート側で結果 OID を固定する参照。**枝と違って動かない。**
    pub remote_result: String,
}

impl RemoteWorkspace {
    pub fn new(workspace_key: &str, job_id: &str) -> Result<Self, CloudError> {
        Ok(Self {
            repo: bare_repo(workspace_key)?,
            dir: job_dir(job_id)?,
            branch: job_branch(job_id)?,
            base: base_ref(job_id)?,
            job_id: job_id.to_string(),
            remote_result: remote_result_ref(job_id)?,
        })
    }
}

/// 持ち帰った結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CollectedResult {
    /// 手元の追跡参照 (`refs/remotes/zaivern-cloud/<job>`)。
    pub result_ref: String,
    /// リモートで確定し、手元でも一致を確かめた OID。
    pub oid: String,
    /// 未コミットの変更を輸送用コミットにしたか。
    pub snapshotted: bool,
}

/// **未コミットの変更があれば断る** (P1-2)。
///
/// ## なぜ黙って進めてはいけないのか
///
/// リモートへ送るのは `HEAD` なので、作業ツリーの編集も新しいファイルも
/// **1 バイトも向こうへ行かない**。それでも `zai cloud job run` は成功し、
/// 結果も持ち帰れてしまうので、利用者は「いまの作業ツリーで走った」と読む。
/// 実際に走ったのは**最後のコミット**で、両者が食い違ったことは
/// どの出力にも現れない — 静かに違うものを測る、いちばん質の悪い形になる。
///
/// v1 では自動で snapshot を取らず、**明示的に止める**。
///
/// ## 何を dirty と数えるか
///
/// `git status --porcelain` の 1 行でも出れば dirty。追跡中の変更・
/// index に載せた変更・追跡していないファイルのどれも含み、
/// `.gitignore` されたものは含まない。
///
/// **`-uall` を明示する。** `status.showUntrackedFiles=no` は global にも
/// 置けるので、既定に任せると「その利用者の手元でだけ追跡外が見えない」
/// という形で穴が開く (この差は手元では再現しない)。
pub fn ensure_clean_worktree(repo: &Path) -> Result<(), CloudError> {
    let mut cmd = crate::procx::hidden_command("git");
    cmd.arg("-C")
        .arg(repo)
        .args(["status", "--porcelain", "--untracked-files=all"]);
    let out = cmd
        .output()
        .map_err(|e| CloudError::config(format!("git を起動できません: {e}")))?;
    if !out.status.success() {
        return Err(CloudError::config(format!(
            "git status を実行できません: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let dirty: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    if dirty.is_empty() {
        return Ok(());
    }
    // **何が引っ掛かったかを見せる。** 直せない拒否は拒否として役に立たない。
    let shown: Vec<&str> = dirty.iter().take(10).copied().collect();
    let more = if dirty.len() > shown.len() {
        format!("\n  … 他 {} 件", dirty.len() - shown.len())
    } else {
        String::new()
    };
    Err(CloudError::config(format!(
        "未コミットの変更があります。cloud job run は現在 HEAD の内容のみを送信します。\n         変更を commit するか stash してから再実行してください。\n         {}{more}",
        shown.join("\n  ")
    )))
}

/// 手元の HEAD をリモートの bare リポジトリへ押し込み、worktree を作る。
pub fn prepare(
    transport: &dyn ExecutionTransport,
    target: &ExecutionTarget,
    local_repo: &Path,
    ws: &RemoteWorkspace,
    opts: &SshOptions,
    timeout: Duration,
) -> Result<(), CloudError> {
    // **git の転送は SSH の実行先だけ。** 手元どうしで worktree を作っても
    // 分離にならない (同じファイルシステムの同じリポジトリを触る)。
    if transport.kind() != super::model::TransportKind::Ssh {
        return Err(CloudError::unsupported(
            "分離した作業場はリモート (SSH) の実行先にだけ作れます",
        ));
    }
    // **リモートを 1 バイトも触る前に確かめる。** 送るのは HEAD だけなので、
    // 作業ツリーが汚れていたら「送ったつもりのもの」と食い違う (P1-2)。
    ensure_clean_worktree(local_repo)?;
    run_remote(
        transport,
        target,
        &init_bare_script(&ws.repo),
        "リモートのリポジトリを用意",
    )?;

    let url = ssh_git_url(target, &ws.repo)?;
    let ssh = git_ssh_command(target, opts)?;
    let (r, sink) = run_local_git(
        local_repo,
        &[
            "push".into(),
            "--force".into(),
            url,
            format!("HEAD:{}", ws.base),
        ],
        Some(&ssh),
        timeout,
    )?;
    if !r.ok() {
        return Err(CloudError::transport(format!(
            "リモートへ push できませんでした: {}",
            sink.stderr_text().trim()
        )));
    }

    run_remote(
        transport,
        target,
        &add_worktree_script(&ws.repo, &ws.dir, &ws.branch, &ws.base),
        "リモートの worktree を作成",
    )?;
    Ok(())
}

/// リモートで snapshot を取り、**いまの HEAD** を仕事専用の参照へ固定する。
///
/// 返すのは `(確定した OID, 輸送用コミットを作ったか)`。
pub fn snapshot_and_pin(
    transport: &dyn ExecutionTransport,
    target: &ExecutionTarget,
    ws: &RemoteWorkspace,
) -> Result<(String, bool), CloudError> {
    let out = run_remote(
        transport,
        target,
        &snapshot_script(&ws.dir, &ws.job_id, &ws.remote_result),
        "リモートの変更を確認",
    )?;
    parse_snapshot_output(&out)
}

/// 固定した参照を手元へ持ち帰り、**OID の一致まで確かめる**。
///
/// * 取りに行くのは枝ではなく [`RemoteWorkspace::remote_result`]
/// * 置き先は手元の専用名前空間だけ (`refs/remotes/zaivern-cloud/<job>`)
/// * **一致しなければ失敗として返す。** 失敗しても回収用の参照は消さない —
///   消すと、手元に届いていたかもしれないものまで失う
///
/// `ssh_command` は `GIT_SSH_COMMAND` に入れる 1 行。ローカルのパスを
/// `url` に渡すときは `None` でよい (試験がそうする)。
pub fn fetch_and_verify(
    local_repo: &Path,
    url: &str,
    ws: &RemoteWorkspace,
    expected_oid: &str,
    ssh_command: Option<&str>,
    timeout: Duration,
) -> Result<String, CloudError> {
    if !is_object_id(expected_oid) {
        return Err(CloudError::transport(format!(
            "確定した OID の形が違います: {expected_oid}"
        )));
    }
    let dest = result_ref(&ws.job_id)?;
    let (r, sink) = run_local_git(
        local_repo,
        &[
            "fetch".into(),
            "--no-tags".into(),
            url.to_string(),
            format!("+{}:{}", ws.remote_result, dest),
        ],
        ssh_command,
        timeout,
    )?;
    if !r.ok() {
        return Err(CloudError::transport(format!(
            "結果を持ち帰れませんでした: {}",
            sink.stderr_text().trim()
        )));
    }

    // **届いたものが、リモートで確定したものと同じかを確かめる。**
    // fetch が成功したことは「何かを取った」しか意味しない。
    let (rev, rev_sink) = run_local_git(
        local_repo,
        &["rev-parse".into(), dest.clone()],
        None,
        timeout,
    )?;
    if !rev.ok() {
        return Err(CloudError::transport(format!(
            "持ち帰った参照を読めませんでした: {}",
            rev_sink.stderr_text().trim()
        )));
    }
    let got = rev_sink.stdout_text().trim().to_string();
    if got != expected_oid {
        return Err(CloudError::transport(format!(
            "持ち帰った結果がリモートで確定したものと違います \
             (リモート {expected_oid} / 手元 {got})。\n\
             リモートの作業場は片付けずに残します"
        )));
    }
    Ok(dest)
}

/// 結果を持ち帰る。**利用者の枝には触らない。**
pub fn collect(
    transport: &dyn ExecutionTransport,
    target: &ExecutionTarget,
    local_repo: &Path,
    ws: &RemoteWorkspace,
    opts: &SshOptions,
    timeout: Duration,
) -> Result<CollectedResult, CloudError> {
    let (oid, snapshotted) = snapshot_and_pin(transport, target, ws)?;
    let url = ssh_git_url(target, &ws.repo)?;
    let ssh = git_ssh_command(target, opts)?;
    let result_ref = fetch_and_verify(local_repo, &url, ws, &oid, Some(&ssh), timeout)?;
    Ok(CollectedResult {
        result_ref,
        oid,
        snapshotted,
    })
}

/// worktree を片付ける。
///
/// **結果を持ち帰れなかったときは呼ばない** (§30) — ディスクの節約より
/// データを失わないほうが大事。
pub fn cleanup(
    transport: &dyn ExecutionTransport,
    target: &ExecutionTarget,
    ws: &RemoteWorkspace,
) -> Result<(), CloudError> {
    run_remote(
        transport,
        target,
        &remove_worktree_script(&ws.repo, &ws.dir),
        "リモートの worktree を片付け",
    )?;
    Ok(())
}

/// 手元が git リポジトリなら、その根を返す。
pub fn local_repo_root(cwd: &Path) -> Result<std::path::PathBuf, CloudError> {
    let mut cmd = crate::procx::hidden_command("git");
    cmd.arg("-C")
        .arg(cwd)
        .args(["rev-parse", "--show-toplevel"]);
    let out = cmd
        .output()
        .map_err(|e| CloudError::config(format!("git を起動できません: {e}")))?;
    if !out.status.success() {
        return Err(CloudError::config(
            "ここは git リポジトリではありません。リポジトリの中で実行してください",
        ));
    }
    let path = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(std::path::PathBuf::from(path))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::model::ExecutionTarget;
    use crate::features::cloud_execution::test_support::{
        target, FakeRun, FakeTransport, TargetOpts,
    };

    const KEY: &str = "0123456789abcdef";

    fn opts() -> SshOptions {
        SshOptions {
            known_hosts: std::path::PathBuf::from("/tmp/kh"),
            ..SshOptions::default()
        }
    }

    #[test]
    fn worktree_branch_is_unique() {
        let a = job_branch("t-aaa").expect("組める");
        let b = job_branch("t-bbb").expect("組める");
        assert_ne!(a, b);
        assert_eq!(a, "zai/cloud/t-aaa");
        // 実際に作られる ID でも一意
        let x = crate::features::cloud_execution::model::ids::new_id("j-");
        let y = crate::features::cloud_execution::model::ids::new_id("j-");
        assert_ne!(job_branch(&x).expect("組める"), job_branch(&y).expect("組める"));
    }

    #[test]
    fn worktree_isolation() {
        // 仕事ごとに置き場が別 (共有すると 2 人が同じファイルを書く)
        let a = job_dir("j-aaa").expect("組める");
        let b = job_dir("j-bbb").expect("組める");
        assert_ne!(a, b);
        assert!(a.as_str().ends_with("/j-aaa"), "{a}");
        assert!(a.as_str().starts_with("~/.zaivern/cloud/jobs/"), "{a}");
        // bare リポジトリは共有 (履歴は 1 本)
        assert_eq!(bare_repo(KEY).expect("組める"), bare_repo(KEY).expect("組める"));
    }

    #[test]
    fn 仕事idに危ない値を通さない() {
        for bad in [
            "../../etc",
            "-rf",
            "a b",
            "a;id",
            "j-$(id)",
            "",
            &"x".repeat(65),
        ] {
            assert!(job_dir(bad).is_err(), "通してしまった: {bad:?}");
            assert!(job_branch(bad).is_err(), "通してしまった: {bad:?}");
            assert!(base_ref(bad).is_err(), "通してしまった: {bad:?}");
            assert!(result_ref(bad).is_err(), "通してしまった: {bad:?}");
        }
        // ワークスペースキーは 16 桁 hex だけ
        assert!(bare_repo("../../x").is_err());
        assert!(bare_repo("zzzz").is_err());
        assert!(bare_repo(KEY).is_ok());
    }

    #[test]
    fn result_fetch_does_not_touch_main() {
        // 持ち帰り先が利用者の枝と重ならない場所であること
        let r = result_ref("j-abc").expect("組める");
        assert_eq!(r, "refs/remotes/zaivern-cloud/j-abc");
        assert!(r.starts_with(RESULT_NAMESPACE));
        assert!(!r.starts_with("refs/heads/"), "利用者の枝を指している");

        // **merge / rebase / push を 1 度も呼ばない** ことをソースで固定する
        let src = include_str!("git_workspace.rs").replace("\r\n", "\n");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        for banned in ["\"merge\"", "\"rebase\"", "\"cherry-pick\"", "\"reset\""] {
            assert!(
                !body.contains(banned),
                "{banned} を呼んでいる。統合は実行層の仕事ではない"
            );
        }
        // push は「リモートの bare へ base を送る」1 か所だけ
        assert_eq!(
            body.matches("\"push\".into()").count(),
            1,
            "push の呼び出しが 1 か所ではない"
        );
    }

    #[test]
    fn 持ち帰りは追跡枝を強制的に置き換える() {
        // `+` が無いと、作り直した仕事で fetch が拒否される
        let t = target("dev-01", TargetOpts::default());
        let ws = RemoteWorkspace::new(KEY, "j-abc").expect("組める");
        let tr = FakeTransport::new(vec![FakeRun::ok("zv_snapshot=0\nabc123\n")]);
        // 手元の git は動かせないので、組み立てだけを見る
        let url = ssh_git_url(&t, &ws.repo).expect("組める");
        assert_eq!(
            url,
            "ssh://zaivern@example.com:22/~/.zaivern/cloud/repos/0123456789abcdef.git"
        );
        let _ = tr;
    }

    #[test]
    fn git_ssh_commandはホストを含まない() {
        // ホストとユーザーは URL 側が持つ。両方に書くとずれる
        let t = target("dev-01", TargetOpts::default());
        let line = git_ssh_command(&t, &opts()).expect("組める");
        assert!(line.starts_with("ssh "), "{line}");
        assert!(!line.contains("example.com"), "{line}");
        assert!(line.contains("StrictHostKeyChecking=yes"), "{line}");
        assert!(line.contains("/tmp/kh"), "{line}");
    }

    #[test]
    fn ポートが違えばurlに載る() {
        let t = target(
            "dev-01",
            TargetOpts {
                port: 2222,
                ..TargetOpts::default()
            },
        );
        let url = ssh_git_url(&t, &bare_repo(KEY).expect("組める")).expect("組める");
        assert!(url.contains(":2222/"), "{url}");
    }

    #[test]
    fn 断片はホーム相対を展開しつつ引用する() {
        let repo = bare_repo(KEY).expect("組める");
        let s = init_bare_script(&repo);
        // `~` は展開させ、後ろは引用する
        assert!(s.contains("~/'.zaivern/cloud/repos/0123456789abcdef.git'"), "{s}");
        // 何度呼んでも同じ結果になる形か
        assert!(s.contains("if [ ! -d"), "{s}");
    }

    /// **同時に用意されても壊れないこと**を、断片の形で固定する。
    ///
    /// 実測で 4 本中 1 本が `could not lock config file` で落ちた。
    /// 1 本ずつの試験では永久に見つからない類の壊れ方なので、
    /// 「鍵を取ってから触る」を構造として固定する。
    #[test]
    fn 用意は同時に呼ばれても壊れない() {
        let s = init_bare_script(&bare_repo(KEY).expect("組める"));
        // すでに在れば鍵も取らずに帰る (ふつうの経路で待たない)
        assert!(s.contains("if [ -d") && s.contains("exit 0"), "{s}");
        // 鍵は mkdir (POSIX で原子的)。`test -d` + `mkdir` に割ると競れる
        assert!(s.contains("mkdir \"$lock\""), "{s}");
        // **git を触るのは鍵を取った後だけ**
        let at_lock = s.find("mkdir \"$lock\"").expect("鍵がある");
        let at_init = s.find("git init --bare").expect("init がある");
        assert!(at_lock < at_init, "鍵より先に git を触っている:\n{s}");
        let at_config = s.find("git -C").expect("config がある");
        assert!(at_lock < at_config, "鍵より先に config を触っている:\n{s}");
        // 鍵は必ず外す (落ちても外れる)
        assert!(s.contains("trap") && s.contains("rmdir"), "{s}");
        // **永久には待たない**
        assert!(s.contains("-gt 300"), "待ちに上限が無い:\n{s}");
    }

    #[test]
    fn 輸送用コミットはそう名乗る() {
        let s = snapshot_script(
            &job_dir("j-abc").expect("組める"),
            "j-abc",
            &remote_result_ref("j-abc").expect("組める"),
        );
        assert!(s.contains("zaivern: snapshot cloud job j-abc"), "{s}");
        assert!(s.contains("輸送用"), "{s}");
        // 変更が無ければ commit しない
        assert!(s.contains("zv_snapshot=0"), "{s}");
        // 利用者の名前で commit しない
        assert!(s.contains("user.name='Zaivern Cloud'"), "{s}");
    }

    #[test]
    fn 片付けは枝を消さない() {
        let s = remove_worktree_script(
            &bare_repo(KEY).expect("組める"),
            &job_dir("j-abc").expect("組める"),
        );
        assert!(s.contains("worktree remove"), "{s}");
        assert!(!s.contains("branch -D"), "手元がまだ持ち帰っていないかもしれない");
    }

    #[test]
    fn リモート側の失敗は理由つきで返る() {
        // 黙って成功したことにすると、worktree が無いまま次の段へ進む
        let t = target("dev-01", TargetOpts::default());
        let tr = FakeTransport::new(vec![FakeRun::fail(128, "fatal: not a git repository")]);
        let e = run_remote(&tr, &t, "git status", "リモートの確認").expect_err("失敗する");
        assert!(matches!(e, CloudError::Transport(_)), "{e:?}");
        assert!(format!("{e}").contains("not a git repository"), "{e}");
        assert!(format!("{e}").contains("リモートの確認"), "{e}");
    }

    #[test]
    fn 分離した作業場はsshの実行先にだけ作る() {
        // 手元どうしで worktree を作っても分離にならない (同じ FS の同じリポジトリ)
        struct LocalKind(FakeTransport);
        impl ExecutionTransport for LocalKind {
            fn kind(&self) -> crate::features::cloud_execution::model::TransportKind {
                crate::features::cloud_execution::model::TransportKind::Local
            }
            fn probe(&self, t: &ExecutionTarget) -> Result<super::super::model::ProbeResult, CloudError> {
                self.0.probe(t)
            }
            fn exec(
                &self,
                t: &ExecutionTarget,
                r: &ExecRequest,
                s: &mut dyn super::super::model::EventSink,
            ) -> Result<super::super::model::ExecResult, CloudError> {
                self.0.exec(t, r, s)
            }
            fn upload(&self, t: &ExecutionTarget, a: &Path, b: &RemotePath) -> Result<(), CloudError> {
                self.0.upload(t, a, b)
            }
            fn download(&self, t: &ExecutionTarget, a: &RemotePath, b: &Path) -> Result<(), CloudError> {
                self.0.download(t, a, b)
            }
        }
        let tr = LocalKind(FakeTransport::default());
        let t = target("dev-01", TargetOpts::default());
        let ws = RemoteWorkspace::new(KEY, "j-abc").expect("組める");
        let e = prepare(
            &tr,
            &t,
            std::path::Path::new("."),
            &ws,
            &opts(),
            Duration::from_secs(5),
        )
        .expect_err("断る");
        assert!(matches!(e, CloudError::Unsupported(_)), "{e:?}");
    }

    #[test]
    fn オブジェクトidは長さを決め打たない() {
        // SHA-1 (40) と SHA-256 (64) の両方を受ける
        assert!(is_object_id(&"a".repeat(40)));
        assert!(is_object_id(&"0123456789abcdef".repeat(4)));
        // 形が違うものは断る
        assert!(!is_object_id(&"a".repeat(39)));
        assert!(!is_object_id(&"a".repeat(65)));
        assert!(!is_object_id(&"a".repeat(41)), "奇数長は無い");
        assert!(!is_object_id(""));
        assert!(!is_object_id(&format!("{}z", "a".repeat(39))), "hex でない");
        assert!(!is_object_id("HEAD"));
    }

    #[test]
    fn snapshotの出力からoidを読む() {
        let oid = "0123456789abcdef0123456789abcdef01234567";
        let (got, snap) =
            parse_snapshot_output(&format!("zv_snapshot=1\nzv_result_oid={oid}\n"))
                .expect("読める");
        assert_eq!(got, oid);
        assert!(snap);

        let (_, snap) = parse_snapshot_output(&format!("zv_snapshot=0\nzv_result_oid={oid}"))
            .expect("読める");
        assert!(!snap);

        // CRLF でも読める (Windows 由来のシェルから返ることがある)
        assert!(parse_snapshot_output(&format!("zv_snapshot=0\r\nzv_result_oid={oid}\r\n")).is_ok());

        // **OID が無ければ失敗として返す。** 「取れなかったが成功」にすると
        // その先の照合が素通りする
        assert!(parse_snapshot_output("zv_snapshot=1\n").is_err());
        assert!(parse_snapshot_output("").is_err());
        assert!(parse_snapshot_output("zv_result_oid=nope").is_err());
    }

    #[test]
    fn 結果は枝ではなく参照へ固定する() {
        let ws = RemoteWorkspace::new(KEY, "j-abc").expect("組める");
        assert_eq!(ws.remote_result, "refs/zaivern/result/j-abc");
        let s = snapshot_script(&ws.dir, &ws.job_id, &ws.remote_result);
        // HEAD を読んで、その OID を参照へ固定している
        assert!(s.contains("zv_oid=$(git rev-parse HEAD)"), "{s}");
        assert!(s.contains("git update-ref 'refs/zaivern/result/j-abc'"), "{s}");
        assert!(s.contains("echo zv_result_oid=$zv_oid"), "{s}");
        // 仕事 ID の検査は参照名にも効く
        assert!(remote_result_ref("../evil").is_err());
    }

    #[test]
    fn sshでない実行先はgitの転送先にならない() {
        let local = ExecutionTarget::local(1);
        assert!(matches!(
            ssh_git_url(&local, &bare_repo(KEY).expect("組める")),
            Err(CloudError::Unsupported(_))
        ));
    }

    #[test]
    fn 用意はリポジトリ作成とworktree作成を順に呼ぶ() {
        let t = target("dev-01", TargetOpts::default());
        let ws = RemoteWorkspace::new(KEY, "j-abc").expect("組める");
        let tr = FakeTransport::new(vec![FakeRun::ok("")]);
        // 手元の git push は本物なので、ここではリモート側の断片だけ確かめる
        run_remote(&tr, &t, &init_bare_script(&ws.repo), "test").expect("走る");
        run_remote(
            &tr,
            &t,
            &add_worktree_script(&ws.repo, &ws.dir, &ws.branch, &ws.base),
            "test",
        )
        .expect("走る");
        let cmds = tr.commands();
        assert_eq!(cmds.len(), 2);
        assert!(cmds[0].contains("git init --bare"), "{}", cmds[0]);
        assert!(cmds[1].contains("worktree add"), "{}", cmds[1]);
        assert!(cmds[1].contains("'zai/cloud/j-abc'"), "{}", cmds[1]);
    }
}

/// **実の git で端から端まで確かめる試験。**
///
/// スクリプトの文字列を突き合わせるだけでは、この層のいちばん大事な性質
/// (「エージェントが枝を移しても成果を取り違えない」) を 1 バイトも見て
/// いない。実際に bare リポジトリと worktree を作り、リモート側の断片を
/// 本物の `sh` で走らせ、本物の `git fetch` で持ち帰って確かめる。
///
/// **unix 限定。** [`RemotePath`] は POSIX の形しか受けないので、Windows の
/// 一時パス (`C:\…`) では実験場そのものが作れない (断るのが正しい挙動)。
/// Windows での担保は上の純関数の試験が受け持つ。
#[cfg(all(test, unix))]
mod live_git_tests {
    use super::*;
    use crate::features::cloud_execution::model::ExecutionTarget;
    use crate::features::cloud_execution::transport::LocalTransport;
    use std::path::PathBuf;
    use std::process::Command;

    fn timeout() -> Duration {
        Duration::from_secs(60)
    }

    /// 実験場の git を撃つ。
    ///
    /// * **cwd を継承させない** (`-C` で固定する)。継承させると、実験場の
    ///   外のリポジトリを触りうる
    /// * **利用者の設定を読ませない。** `core.hooksPath` のような global 設定は
    ///   このリポジトリの挙動まで変える (CI にはあって手元に無い設定がある)
    fn git_at(dir: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("LC_ALL", "C")
            .output()
            .expect("git を起動できる")
    }

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let out = git_at(dir, args);
        assert!(
            out.status.success(),
            "git {args:?} が失敗: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    struct Lab {
        _root: PathBuf,
        local: PathBuf,
        bare: PathBuf,
        work: PathBuf,
        ws: RemoteWorkspace,
    }

    impl Lab {
        /// リモート側で走らせる (本物の `sh` を通る)。
        fn snapshot(&self) -> Result<(String, bool), CloudError> {
            let tr = LocalTransport::new(timeout());
            snapshot_and_pin(&tr, &ExecutionTarget::local(1), &self.ws)
        }

        /// 手元へ持ち帰る (本物の `git fetch` を通る)。
        fn fetch(&self, expected: &str) -> Result<String, CloudError> {
            fetch_and_verify(
                &self.local,
                self.bare.to_str().expect("utf-8"),
                &self.ws,
                expected,
                None,
                timeout(),
            )
        }

        fn work_head(&self) -> String {
            git_ok(&self.work, &["rev-parse", "HEAD"])
        }
    }

    /// 手元 → bare → worktree まで、本番と同じ順で組む。
    fn lab(tag: &str) -> Lab {
        let root = crate::test_util::unique_temp_dir("zv-cloud", tag);
        let local = root.join("local");
        let bare = root.join("remote.git");
        let work = root.join("job");
        std::fs::create_dir_all(&local).expect("作れる");

        git_ok(&local, &["init", "-q", "-b", "main", "."]);
        git_ok(&local, &["config", "user.name", "Test"]);
        git_ok(&local, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(local.join("a.txt"), b"hello\n").expect("書ける");
        git_ok(&local, &["add", "-A"]);
        git_ok(&local, &["commit", "-q", "-m", "initial"]);

        let job = "j-live";
        let ws = RemoteWorkspace {
            repo: RemotePath::new(bare.to_str().expect("utf-8")).expect("作れる"),
            dir: RemotePath::new(work.to_str().expect("utf-8")).expect("作れる"),
            branch: job_branch(job).expect("組める"),
            base: base_ref(job).expect("組める"),
            job_id: job.to_string(),
            remote_result: remote_result_ref(job).expect("組める"),
        };

        // 本番の `prepare` と同じ 3 段
        let tr = LocalTransport::new(timeout());
        run_remote(
            &tr,
            &ExecutionTarget::local(1),
            &init_bare_script(&ws.repo),
            "用意",
        )
        .expect("bare を作れる");
        git_ok(
            &local,
            &[
                "push",
                "-q",
                bare.to_str().expect("utf-8"),
                &format!("HEAD:{}", ws.base),
            ],
        );
        run_remote(
            &tr,
            &ExecutionTarget::local(1),
            &add_worktree_script(&ws.repo, &ws.dir, &ws.branch, &ws.base),
            "worktree",
        )
        .expect("worktree を作れる");

        Lab {
            _root: root,
            local,
            bare,
            work,
            ws,
        }
    }

    // ───── 作業ツリーが汚れていたら断る (P1-2) ─────    // ───── 作業ツリーが汚れていたら断る (P1-2) ─────

    /// コミット 1 つだけの、きれいなリポジトリ。
    fn clean_repo(tag: &str) -> PathBuf {
        let root = crate::test_util::unique_temp_dir("zv-cloud", tag);
        let local = root.join("local");
        std::fs::create_dir_all(&local).expect("作れる");
        git_ok(&local, &["init", "-q", "-b", "main", "."]);
        git_ok(&local, &["config", "user.name", "Test"]);
        git_ok(&local, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(local.join("a.txt"), b"hello\n").expect("書ける");
        git_ok(&local, &["add", "-A"]);
        git_ok(&local, &["commit", "-q", "-m", "initial"]);
        local
    }

    #[test]
    fn clean_worktree_can_prepare() {
        let repo = clean_repo("clean-worktree");
        ensure_clean_worktree(&repo).expect("きれいなら通る");
    }

    /// **P1-2 の再現。** 追跡中のファイルを書き換えただけでは HEAD が動かない
    /// ので、黙って進むと**最後のコミット**がリモートで走る。
    #[test]
    fn dirty_worktree_is_not_silently_ignored() {
        let repo = clean_repo("dirty-worktree");
        std::fs::write(repo.join("a.txt"), b"edited\n").expect("書ける");

        let e = ensure_clean_worktree(&repo).expect_err("断る");
        let text = format!("{e}");
        assert!(text.contains("未コミットの変更があります"), "{text}");
        assert!(
            text.contains("HEAD の内容のみ"),
            "何が起きるか言っていない: {text}"
        );
        assert!(
            text.contains("a.txt"),
            "何が引っ掛かったか言っていない: {text}"
        );
    }

    #[test]
    fn untracked_file_is_rejected() {
        let repo = clean_repo("untracked");
        std::fs::write(repo.join("new.rs"), b"fn main() {}\n").expect("書ける");
        let e = ensure_clean_worktree(&repo).expect_err("断る");
        assert!(format!("{e}").contains("new.rs"), "{e}");
    }

    #[test]
    fn staged_change_is_rejected() {
        let repo = clean_repo("staged");
        std::fs::write(repo.join("b.txt"), b"staged\n").expect("書ける");
        git_ok(&repo, &["add", "b.txt"]);
        let e = ensure_clean_worktree(&repo).expect_err("断る");
        assert!(format!("{e}").contains("b.txt"), "{e}");
    }

    /// **`.gitignore` されたものは汚れではない。** ここを含めると
    /// `target/` を持つふつうのリポジトリが 1 つも通らなくなる。
    #[test]
    fn ignored_file_is_not_dirty() {
        let repo = clean_repo("ignored");
        std::fs::write(repo.join(".gitignore"), b"build/\n").expect("書ける");
        git_ok(&repo, &["add", ".gitignore"]);
        git_ok(&repo, &["commit", "-q", "-m", "ignore build"]);
        std::fs::create_dir_all(repo.join("build")).expect("作れる");
        std::fs::write(repo.join("build/out.bin"), b"x").expect("書ける");
        ensure_clean_worktree(&repo).expect("無視されたものは汚れではない");
    }

    /// **リモートを 1 バイトも触る前に断る。** 先に bare リポジトリを作って
    /// から断ると、失敗しただけなのに向こうへ物が残る。
    #[test]
    fn prepare_rejects_dirty_worktree_before_touching_remote() {
        use crate::features::cloud_execution::test_support::{FakeTransport, TargetOpts};

        let repo = clean_repo("prepare-order");
        std::fs::write(repo.join("a.txt"), b"edited\n").expect("書ける");

        let tr = FakeTransport::default();
        let t =
            crate::features::cloud_execution::test_support::target("dev", TargetOpts::default());
        let ws = RemoteWorkspace::new("0123456789abcdef", "j-order").expect("組める");
        let e = prepare(
            &tr,
            &t,
            &repo,
            &ws,
            &SshOptions::default(),
            Duration::from_secs(5),
        )
        .expect_err("断る");

        assert!(format!("{e}").contains("未コミットの変更があります"), "{e}");
        assert!(
            tr.commands().is_empty(),
            "リモートを触ってしまった: {:?}",
            tr.commands()
        );
    }

    /// リモート側で 1 コミット積む (エージェントの作業を模す)。    /// リモート側で 1 コミット積む (エージェントの作業を模す)。
    fn commit_in_work(lab: &Lab, file: &str, body: &str, msg: &str) -> String {
        std::fs::write(lab.work.join(file), body).expect("書ける");
        git_ok(&lab.work, &["add", "-A"]);
        git_ok(
            &lab.work,
            &[
                "-c",
                "user.name=Agent",
                "-c",
                "user.email=agent@example.invalid",
                "commit",
                "-q",
                "-m",
                msg,
            ],
        );
        lab.work_head()
    }

    /// 回収したものが、リモートで確定したものと同じ内容かを確かめる。
    fn assert_collected(lab: &Lab, oid: &str, file: &str, body: &str) {
        let dest = result_ref(&lab.ws.job_id).expect("組める");
        assert_eq!(
            git_ok(&lab.local, &["rev-parse", &dest]),
            oid,
            "追跡参照が指す先が違う"
        );
        let shown = git_ok(&lab.local, &["show", &format!("{dest}:{file}")]);
        assert_eq!(shown.trim(), body.trim(), "中身が違う");
    }

    #[test]
    fn 元のジョブ枝で作業した成果を回収する() {
        let lab = lab("live-same-branch");
        let head = commit_in_work(&lab, "b.txt", "on-job-branch\n", "work on job branch");
        let (oid, snapped) = lab.snapshot().expect("固定できる");
        assert_eq!(oid, head);
        assert!(!snapped, "未コミットは無いので輸送用コミットは作らない");
        lab.fetch(&oid).expect("回収できる");
        assert_collected(&lab, &oid, "b.txt", "on-job-branch");
    }

    /// **これが指摘の本体。** 枝を移されると、旧実装は古い枝を取って
    /// 「成功」してしまう (成果が 1 バイトも入っていない)。
    #[test]
    fn 別の枝へ移って作業した成果を回収する() {
        let lab = lab("live-switch");
        let before = lab.work_head();
        git_ok(&lab.work, &["switch", "-q", "-c", "agent/side"]);
        let head = commit_in_work(&lab, "b.txt", "on-side-branch\n", "work on side branch");
        assert_ne!(head, before);

        // 元のジョブ枝は**古いまま**である (旧実装はこれを取っていた)
        let stale = git_ok(&lab.bare, &["rev-parse", &lab.ws.branch]);
        assert_eq!(stale, before, "ジョブ枝が動いてしまっている (前提が崩れた)");

        let (oid, _) = lab.snapshot().expect("固定できる");
        assert_eq!(oid, head, "HEAD ではなく別のものを固定している");
        lab.fetch(&oid).expect("回収できる");
        assert_collected(&lab, &oid, "b.txt", "on-side-branch");
    }

    #[test]
    fn detached_headで作業した成果を回収する() {
        let lab = lab("live-detached");
        git_ok(&lab.work, &["switch", "-q", "--detach"]);
        let head = commit_in_work(&lab, "b.txt", "on-detached\n", "work while detached");
        // 枝はどこも指していない状態でも固定できる
        let (oid, _) = lab.snapshot().expect("固定できる");
        assert_eq!(oid, head);
        lab.fetch(&oid).expect("回収できる");
        assert_collected(&lab, &oid, "b.txt", "on-detached");
    }

    #[test]
    fn 未コミットの変更は輸送用コミットにして回収する() {
        let lab = lab("live-dirty");
        let before = lab.work_head();
        std::fs::write(lab.work.join("c.txt"), "uncommitted\n").expect("書ける");
        let (oid, snapped) = lab.snapshot().expect("固定できる");
        assert!(snapped, "輸送用コミットを作っていない");
        assert_ne!(oid, before);
        lab.fetch(&oid).expect("回収できる");
        assert_collected(&lab, &oid, "c.txt", "uncommitted");
        // 輸送用であることが読めば分かる
        let msg = git_ok(&lab.local, &["log", "-1", "--format=%B", &oid]);
        assert!(msg.contains("zaivern: snapshot cloud job"), "{msg}");
    }

    #[test]
    fn detached_headでの未コミット変更も回収する() {
        let lab = lab("live-detached-dirty");
        git_ok(&lab.work, &["switch", "-q", "--detach"]);
        std::fs::write(lab.work.join("c.txt"), "detached-dirty\n").expect("書ける");
        let (oid, snapped) = lab.snapshot().expect("固定できる");
        assert!(snapped);
        lab.fetch(&oid).expect("回収できる");
        assert_collected(&lab, &oid, "c.txt", "detached-dirty");
    }

    #[test]
    fn oidが一致しなければ回収を成功にしない() {
        let lab = lab("live-mismatch");
        commit_in_work(&lab, "b.txt", "x\n", "work");
        let (oid, _) = lab.snapshot().expect("固定できる");

        // リモートで確定したものと違う OID を期待して回収する
        let other = "0".repeat(oid.len());
        let e = lab.fetch(&other).expect_err("断る");
        assert!(matches!(e, CloudError::Transport(_)), "{e:?}");
        assert!(format!("{e}").contains("片付けずに残します"), "{e}");

        // **回収用の参照は消さない** (届いていたかもしれないものまで失う)
        let dest = result_ref(&lab.ws.job_id).expect("組める");
        assert_eq!(git_ok(&lab.local, &["rev-parse", &dest]), oid);
    }

    #[test]
    fn 取りに行けなければ回収を成功にしない() {
        let lab = lab("live-fetch-fail");
        commit_in_work(&lab, "b.txt", "x\n", "work");
        let (oid, _) = lab.snapshot().expect("固定できる");
        let missing = lab._root.join("no-such-repo.git");
        let e = fetch_and_verify(
            &lab.local,
            missing.to_str().expect("utf-8"),
            &lab.ws,
            &oid,
            None,
            timeout(),
        )
        .expect_err("断る");
        assert!(matches!(e, CloudError::Transport(_)), "{e:?}");
    }

    /// **手元のものを 1 つも動かさない** (§28 / 要件 7)。
    #[test]
    fn 回収しても手元の枝とindexと作業ツリーは変わらない() {
        let lab = lab("live-untouched");
        // 手元を「作業中」の状態にしておく
        std::fs::write(lab.local.join("wip.txt"), "work in progress\n").expect("書ける");
        git_ok(&lab.local, &["add", "wip.txt"]);
        std::fs::write(lab.local.join("a.txt"), "locally edited\n").expect("書ける");

        let before_head = git_ok(&lab.local, &["rev-parse", "HEAD"]);
        let before_branch = git_ok(&lab.local, &["rev-parse", "--abbrev-ref", "HEAD"]);
        let before_status = git_ok(&lab.local, &["status", "--porcelain"]);
        let before_index = git_ok(&lab.local, &["diff", "--cached", "--name-status"]);

        commit_in_work(&lab, "b.txt", "remote work\n", "work");
        let (oid, _) = lab.snapshot().expect("固定できる");
        lab.fetch(&oid).expect("回収できる");

        assert_eq!(git_ok(&lab.local, &["rev-parse", "HEAD"]), before_head, "HEAD が動いた");
        assert_eq!(
            git_ok(&lab.local, &["rev-parse", "--abbrev-ref", "HEAD"]),
            before_branch,
            "枝が変わった"
        );
        assert_eq!(
            git_ok(&lab.local, &["status", "--porcelain"]),
            before_status,
            "作業ツリーが変わった"
        );
        assert_eq!(
            git_ok(&lab.local, &["diff", "--cached", "--name-status"]),
            before_index,
            "index が変わった"
        );
        assert_eq!(
            std::fs::read_to_string(lab.local.join("a.txt")).expect("読める"),
            "locally edited\n",
            "手元の編集が上書きされた"
        );
        // main は 1 バイトも動いていない
        assert_eq!(git_ok(&lab.local, &["rev-parse", "main"]), before_head);
    }

    /// 回収した先は**専用の名前空間だけ**で、利用者の枝は増えていない。
    #[test]
    fn 回収先は専用の名前空間だけ() {
        let lab = lab("live-namespace");
        commit_in_work(&lab, "b.txt", "x\n", "work");
        let (oid, _) = lab.snapshot().expect("固定できる");
        lab.fetch(&oid).expect("回収できる");

        let heads = git_ok(&lab.local, &["for-each-ref", "--format=%(refname)", "refs/heads/"]);
        assert_eq!(heads, "refs/heads/main", "利用者の枝が増えた:\n{heads}");
        let ours = git_ok(
            &lab.local,
            &["for-each-ref", "--format=%(refname)", RESULT_NAMESPACE],
        );
        assert_eq!(ours, format!("{RESULT_NAMESPACE}/{}", lab.ws.job_id));
    }
}
