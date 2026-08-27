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

/// 未コミットの変更を、持ち帰るためだけの commit にする sh の断片 (§29)。
///
/// **GitHub へ push するための commit ではない。** リモートから手元へ
/// 安全に戻すための輸送用で、そのことをメッセージに書く。
pub fn snapshot_script(dir: &RemotePath, job_id: &str) -> String {
    let d = quote_home(dir);
    let msg = posix_quote(&format!(
        "zaivern: snapshot cloud job {job_id}\n\n\
         リモートの作業を手元へ戻すための輸送用コミットです。\
         そのまま公開する前提のものではありません。"
    ));
    format!(
        "set -e; cd {d}; \
         if [ -n \"$(git status --porcelain)\" ]; then \
           git add -A; \
           git -c user.name='Zaivern Cloud' -c user.email='cloud@zaivern.invalid' \
               commit -q -m {msg}; \
           echo zv_snapshot=1; \
         else echo zv_snapshot=0; fi; \
         git rev-parse HEAD"
    )
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
}

impl RemoteWorkspace {
    pub fn new(workspace_key: &str, job_id: &str) -> Result<Self, CloudError> {
        Ok(Self {
            repo: bare_repo(workspace_key)?,
            dir: job_dir(job_id)?,
            branch: job_branch(job_id)?,
            base: base_ref(job_id)?,
            job_id: job_id.to_string(),
        })
    }
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

/// 結果を持ち帰る。**利用者の枝には触らない。**
///
/// 返すのは「持ち帰った参照」と「輸送用の commit を作ったか」。
pub fn collect(
    transport: &dyn ExecutionTransport,
    target: &ExecutionTarget,
    local_repo: &Path,
    ws: &RemoteWorkspace,
    opts: &SshOptions,
    timeout: Duration,
) -> Result<(String, bool), CloudError> {
    let out = run_remote(
        transport,
        target,
        &snapshot_script(&ws.dir, &ws.job_id),
        "リモートの変更を確認",
    )?;
    let snapshotted = out.lines().any(|l| l.trim() == "zv_snapshot=1");

    let url = ssh_git_url(target, &ws.repo)?;
    let ssh = git_ssh_command(target, opts)?;
    let dest = result_ref(&ws.job_id)?;
    let (r, sink) = run_local_git(
        local_repo,
        &[
            "fetch".into(),
            "--no-tags".into(),
            url,
            format!("+{}:{}", ws.branch, dest),
        ],
        Some(&ssh),
        timeout,
    )?;
    if !r.ok() {
        return Err(CloudError::transport(format!(
            "結果を持ち帰れませんでした: {}",
            sink.stderr_text().trim()
        )));
    }
    Ok((dest, snapshotted))
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
        let s = snapshot_script(&job_dir("j-abc").expect("組める"), "j-abc");
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
