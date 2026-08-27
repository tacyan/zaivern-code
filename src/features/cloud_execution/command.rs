//! **何を起動するかは、外から渡ってくる** (§37)。
//!
//! この層はエージェントのカタログを持たない。持つと、エージェントが 1 つ
//! 増えるたびに Cloud 側も増えることになり、「どのエージェントでも使える
//! 実行層」ではなくなる。
//!
//! ```text
//! Agent Provider (crate::agents のカタログ)
//!        ↓ LaunchSpec を作る
//! Cloud Execution (ここ)
//!        ↓ 実行先の上で走らせる
//! Local / SSH
//! ```
//!
//! ## 既存の PTY / Supervisor をそのまま使う (§36)
//!
//! 手元のエージェント起動は
//! 「1 本のコマンド行 + cwd + env」を [`crate::terminal::Session`] へ渡す形で、
//! Supervisor はその PTY を読んでいる。リモートでも**同じ経路**を通すために、
//! ここがするのは
//!
//! ```text
//! <元のコマンド行>  →  ssh <実行先> '<元のコマンド行>'
//! ```
//!
//! という**コマンド行の書き換えだけ**。Supervisor から見れば、どちらも
//! ただのセッションで、Cloud 専用の端末パーサも状態機械も要らない。

use std::collections::BTreeMap;

use super::model::{CloudError, ExecRequest, ExecutionTarget, TransportKind};
use super::transport::ssh::{posix_quote, remote_script, ssh_command_line, SshOptions};

/// 起動するもの。**上位 (Agent Provider) が組んで渡す。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaunchSpec {
    pub program: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// 実行する場所 (リモートのパス)。
    pub cwd: Option<String>,
    /// 画面に出す名前。実行そのものには使わない。
    pub title: String,
}

impl LaunchSpec {
    pub fn new(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            program: program.into(),
            args,
            env: BTreeMap::new(),
            cwd: None,
            title: String::new(),
        }
    }

    /// `-- <コマンド…>` の並びから組む (CLI が使う)。
    pub fn from_argv(argv: &[String]) -> Result<Self, CloudError> {
        let mut it = argv.iter();
        let program = it
            .next()
            .ok_or_else(|| CloudError::config("実行するコマンドがありません"))?;
        Ok(Self::new(program.clone(), it.cloned().collect()))
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    /// 実行の要求へ写す。
    pub fn to_request(&self) -> ExecRequest {
        let mut req = ExecRequest::new(self.program.clone(), self.args.clone());
        req.cwd = self.cwd.clone();
        // **秘密らしい環境変数は運ばない** (§24)。手元のトークンが
        // リモートへ流れる経路をここで塞ぐ。
        req.env = super::transport::ssh::forwardable_env(&self.env);
        req
    }

    /// 画面とログに出す 1 行。
    pub fn display(&self) -> String {
        self.to_request().display()
    }
}

/// **既存のセッション起動経路へ渡すコマンド行**を作る。
///
/// 手元の実行先ならそのまま (書き換えない)。リモートなら `ssh …` で包む。
/// **エージェントの名前も種類も見ない** — 受け取った行をそのまま運ぶだけ。
pub fn session_command_line(
    target: &ExecutionTarget,
    original_command: &str,
    remote_cwd: Option<&str>,
    opts: &SshOptions,
) -> Result<String, CloudError> {
    if original_command.trim().is_empty() {
        return Err(CloudError::config("コマンドが空です"));
    }
    if target.transport == TransportKind::Local {
        // 手元では 1 バイトも変えない。**ここを書き換えると、手元の
        // エージェント起動がリモート対応の副作用で壊れる。**
        return Ok(original_command.to_string());
    }

    // 対話するので擬似端末を割り当てる (エージェントは端末を前提にしている)
    let mut opts = opts.clone();
    opts.tty = true;
    opts.batch = false;
    let ssh = ssh_command_line(target, &opts)?;

    // リモートで走らせる中身。**元のコマンド行はシェルの文字列として
    // そのまま渡す** — これは利用者 (と既存カタログ) が組んだ行であって、
    // こちらが解釈してよいものではない。
    let script = match remote_cwd {
        Some(dir) if !dir.is_empty() => {
            let cd = if let Some(rest) = dir.strip_prefix("~/") {
                format!("~/{}", posix_quote(rest))
            } else {
                posix_quote(dir)
            };
            format!("cd {cd} && {original_command}")
        }
        _ => original_command.to_string(),
    };
    Ok(format!("{ssh} {}", posix_quote(&script)))
}

/// 構造化された [`LaunchSpec`] から、リモートで走らせる 1 行を作る。
///
/// [`session_command_line`] と違い、**プログラムと引数を構造のまま**受け取る
/// ので、空白や引用符が混ざっていても割れない。
pub fn launch_command_line(
    target: &ExecutionTarget,
    spec: &LaunchSpec,
    opts: &SshOptions,
) -> Result<String, CloudError> {
    let req = spec.to_request();
    if target.transport == TransportKind::Local {
        // 手元でも、組み立て方をリモートと揃えておく (2 通りにしない)
        return remote_script(&req);
    }
    let mut opts = opts.clone();
    opts.tty = true;
    opts.batch = false;
    let ssh = ssh_command_line(target, &opts)?;
    Ok(format!("{ssh} {}", posix_quote(&remote_script(&req)?)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::model::ExecutionTarget;
    use crate::features::cloud_execution::test_support::{target, TargetOpts};

    fn opts() -> SshOptions {
        SshOptions {
            known_hosts: std::path::PathBuf::from("/tmp/kh"),
            ..SshOptions::default()
        }
    }

    #[test]
    fn 手元のコマンド行は一バイトも変えない() {
        let local = ExecutionTarget::local(1);
        let line = session_command_line(&local, "some-agent --flag 'a b'", None, &opts())
            .expect("組める");
        assert_eq!(line, "some-agent --flag 'a b'");
    }

    #[test]
    fn リモートはsshで包むだけ() {
        let t = target("dev-01", TargetOpts::default());
        let line = session_command_line(&t, "some-agent --flag", Some("~/work"), &opts())
            .expect("組める");
        assert!(line.starts_with("ssh "), "{line}");
        // 元のコマンド行がそのまま入っている (解釈していない)
        assert!(line.contains("some-agent --flag"), "{line}");
        // 対話できるよう擬似端末を割り当てている
        assert!(line.contains("'-tt'"), "{line}");
        // host key の確認を外していない
        assert!(line.contains("StrictHostKeyChecking=yes"), "{line}");
    }

    #[test]
    fn 作業ディレクトリの空白で割れない() {
        let t = target("dev-01", TargetOpts::default());
        let line = session_command_line(&t, "run", Some("~/my work/repo"), &opts()).expect("組める");
        // リモートで走るのは `cd ~/'my work/repo' && run`。
        // それが ssh へ**1 つの引数**として渡るよう、もう一段引用されている。
        assert!(
            line.ends_with(r"'cd ~/'\''my work/repo'\'' && run'"),
            "{line}"
        );
    }

    #[test]
    fn 空のコマンドは断る() {
        let t = target("dev-01", TargetOpts::default());
        assert!(session_command_line(&t, "   ", None, &opts()).is_err());
    }

    #[test]
    fn 構造化された起動は引数が割れない() {
        let t = target("dev-01", TargetOpts::default());
        let spec = LaunchSpec::new("worker", vec!["--msg".into(), "hello world".into()])
            .with_cwd("~/jobs/a");
        let line = launch_command_line(&t, &spec, &opts()).expect("組める");
        assert!(line.contains("hello world"), "{line}");
        // 手元でも同じ組み立てを通る
        let local = launch_command_line(&ExecutionTarget::local(1), &spec, &opts())
            .expect("組める");
        assert_eq!(local, "cd ~/'jobs/a' && 'worker' '--msg' 'hello world'");
    }

    #[test]
    fn 秘密らしい環境変数は要求へ入らない() {
        let spec = LaunchSpec::new("worker", vec![])
            .with_env("ZV_JOB", "1")
            .with_env("HCLOUD_TOKEN", "super-secret-test-token");
        let req = spec.to_request();
        assert!(req.env.contains_key("ZV_JOB"));
        assert!(
            !req.env.contains_key("HCLOUD_TOKEN"),
            "トークンを運ぼうとしている"
        );
    }

    #[test]
    fn 並びからコマンドを組む() {
        let spec = LaunchSpec::from_argv(&["cargo".into(), "test".into(), "--workspace".into()])
            .expect("組める");
        assert_eq!(spec.program, "cargo");
        assert_eq!(spec.args, vec!["test", "--workspace"]);
        assert_eq!(spec.display(), "cargo test --workspace");
        assert!(LaunchSpec::from_argv(&[]).is_err());
    }
}
