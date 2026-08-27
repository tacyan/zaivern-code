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
use super::transport::ssh::{
    posix_quote, remote_cwd_expr, remote_script, ssh_command_line, SshOptions,
};

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

    /// **試験のための組み立て。** 製品側は欄へ直に入れる (`spec.cwd = …`) ので、
    /// ここを出荷ビルドへ入れると「使われていない公開 API」になる。
    #[cfg(test)]
    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    /// **試験のための組み立て** ([`LaunchSpec::with_cwd`] と同じ理由)。
    #[cfg(test)]
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

    /// 画面とログに出す 1 行。**秘密は伏せていない。**
    ///
    /// 残す・見せる用途では [`LaunchSpec::safe_display`] を使うこと。
    pub fn display(&self) -> String {
        self.to_request().display()
    }

    /// **記録・画面・ログへ出す 1 行** (§41)。
    ///
    /// 引数にはトークンが混ざる (`--password=…` / `Authorization: Bearer …`)。
    /// 伏せ方は [`super::redact`] にだけ置き、ここは通すだけ — 2 か所に
    /// 書くと必ずずれて、ずれた側から漏れる。
    pub fn safe_display(&self) -> String {
        super::redact::redact(&self.display())
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
    let script = remote_shell_line(original_command, remote_cwd);
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

/// `--run` で**実際に起動するもの**。
///
/// ## 表示用の 1 行と混ぜない
///
/// [`session_command_line`] / [`launch_command_line`] が返すのは
/// **POSIX シェルへ貼るための文字列**で、これは**プロセス起動そのもの**。
/// 混ぜると、貼るために組んだ引用をローカルのシェルがもう一度読み直す —
/// `cmd.exe` は POSIX の単一引用符を引用として扱わないので、Windows で
/// 引数が壊れる (この版で直した壊れ方)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunPlan {
    /// 起動するプログラムと引数。**このまま `Command` へ渡す。**
    pub argv: Vec<String>,
    /// ローカルのシェルを 1 枚挟むか。
    ///
    /// **挟むのは「利用者が書いた 1 行」を手元で走らせるときだけ。**
    /// こちらが組んだ ssh の引数配列には決して挟まない。
    pub via_local_shell: bool,
    /// 手元で走らせるときの作業ディレクトリ。
    pub cwd: Option<String>,
}

impl RunPlan {
    /// 起動するものを 1 行にしたもの。
    ///
    /// **表示にも起動にも使わない** — 起動は [`RunPlan::argv`] をそのまま
    /// `Command` へ渡し、貼り付け用は [`session_command_line`] が別に組む。
    /// ここは「畳んだ形」と「畳んでいない形」が違うことを試験で示すためだけ。
    #[cfg(test)]
    pub fn display(&self) -> String {
        self.argv.join(" ")
    }
}

/// `--command "<行>"` を実行先で走らせる計画。
///
/// リモートなら ssh の引数配列、手元なら利用者が書いた行をそのまま
/// ローカルのシェルへ渡す (その行を引用したのは利用者自身なので、
/// こちらが読み直す余地は無い)。
pub fn run_plan_for_command(
    target: &ExecutionTarget,
    original_command: &str,
    remote_cwd: Option<&str>,
    opts: &SshOptions,
) -> Result<RunPlan, CloudError> {
    if original_command.trim().is_empty() {
        return Err(CloudError::config("コマンドが空です"));
    }
    if target.transport == TransportKind::Local {
        return Ok(RunPlan {
            argv: vec![original_command.to_string()],
            via_local_shell: true,
            cwd: remote_cwd.map(str::to_string),
        });
    }
    let script = remote_shell_line(original_command, remote_cwd);
    Ok(RunPlan {
        argv: super::transport::ssh::ssh_interactive_argv(target, &script, opts)?,
        via_local_shell: false,
        cwd: None,
    })
}

/// `-- <program> <args…>` を実行先で走らせる計画。
///
/// 手元なら**シェルを 1 枚も挟まずに**そのまま起動する。
pub fn run_plan_for_spec(
    target: &ExecutionTarget,
    spec: &LaunchSpec,
    opts: &SshOptions,
) -> Result<RunPlan, CloudError> {
    let req = spec.to_request();
    if req.program.is_empty() {
        return Err(CloudError::config("実行するコマンドがありません"));
    }
    if target.transport == TransportKind::Local {
        let mut argv = vec![req.program.clone()];
        argv.extend(req.args.clone());
        return Ok(RunPlan {
            argv,
            via_local_shell: false,
            cwd: req.cwd.clone(),
        });
    }
    Ok(RunPlan {
        argv: super::transport::ssh::ssh_interactive_argv(target, &remote_script(&req)?, opts)?,
        via_local_shell: false,
        cwd: None,
    })
}

/// リモートのシェルが受け取る 1 行 (`cd … && <利用者の行>`)。
///
/// `cd` の作り方は [`remote_cwd_expr`] が唯一の出所 — ここで組み直すと、
/// 構造化された経路 ([`remote_script`]) と食い違う (実際に `~` の特別扱いが
/// 片方だけ抜けていた)。
fn remote_shell_line(original_command: &str, remote_cwd: Option<&str>) -> String {
    match remote_cwd {
        Some(dir) if !dir.is_empty() => {
            format!("cd {} && {original_command}", remote_cwd_expr(dir))
        }
        _ => original_command.to_string(),
    }
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

    // ───────── リモートの作業ディレクトリ (cwd) の表し方 ─────────

    #[test]
    fn remote_cwd_home_expands_tilde() {
        let line = remote_shell_line("pwd", Some("~"));
        assert_eq!(line, "cd ~ && pwd");
    }

    /// **表で固定する。** `~` だけ特別扱いを忘れる、が実際に起きた壊れ方
    /// (片方の経路にはあって、もう片方に無かった)。
    #[test]
    fn リモートの作業ディレクトリの表し方() {
        const CASES: &[(&str, &str)] = &[
            // `~` はリモートに展開させる (引用すると別の場所になる)
            ("~", "cd ~ && pwd"),
            // `~/` の後ろは引用する (展開されるのは `~` だけ)
            ("~/repo", "cd ~/'repo' && pwd"),
            ("~/my work/repo", "cd ~/'my work/repo' && pwd"),
            // 絶対パスは丸ごと引用する
            ("/tmp/test", "cd '/tmp/test' && pwd"),
            ("/tmp/a b", "cd '/tmp/a b' && pwd"),
            // 単一引用符を含んでも壊れない
            ("/tmp/it's", r"cd '/tmp/it'\''s' && pwd"),
            // 危ない文字も字義どおり
            ("/tmp/a;id", "cd '/tmp/a;id' && pwd"),
            ("/tmp/$(id)", "cd '/tmp/$(id)' && pwd"),
            ("~/a;id", "cd ~/'a;id' && pwd"),
        ];
        for (dir, want) in CASES {
            assert_eq!(&remote_shell_line("pwd", Some(dir)), want, "cwd = {dir:?}");
        }
        // 指定が無ければ `cd` を足さない
        assert_eq!(remote_shell_line("pwd", None), "pwd");
        assert_eq!(remote_shell_line("pwd", Some("")), "pwd");
    }

    /// **2 つの経路が同じ答えを出す。**
    ///
    /// `--command` 形式 ([`remote_shell_line`]) と構造化形式
    /// ([`remote_script`]) は別の関数だが、`cd` の作り方が食い違うと
    /// 「片方だけ直っている」状態になる (実際にそうなっていた)。
    #[test]
    fn 二つの経路のcdの作り方が一致する() {
        for dir in ["~", "~/repo", "~/my work", "/tmp/test", "/tmp/a b", "/tmp/it's"] {
            let via_line = remote_shell_line("x", Some(dir));
            let mut req = ExecRequest::new("x", vec![]);
            req.cwd = Some(dir.to_string());
            let via_script = super::super::transport::ssh::remote_script(&req).expect("組める");
            let cd_of = |s: &str| s.split(" && ").next().unwrap_or_default().to_string();
            assert_eq!(cd_of(&via_line), cd_of(&via_script), "cwd = {dir:?}");
        }
    }

    /// **実の `sh` で、本当にホームへ移動することを確かめる。**
    ///
    /// 実験場の中に `~` という名前のディレクトリを置く。引用してしまうと
    /// **そちら**へ入ってしまう — それがこの不具合の実害である。
    #[cfg(unix)]
    #[test]
    fn 実際のシェルでホームへ移動する() {
        let lab = crate::test_util::unique_temp_dir("zv-cloud", "cwd-tilde");
        let trap = lab.join("~");
        std::fs::create_dir_all(&trap).expect("罠を作れる");

        let run = |script: &str| -> String {
            let out = std::process::Command::new("sh")
                .arg("-c")
                .arg(script)
                .current_dir(&lab)
                .env("LC_ALL", "C")
                .output()
                .expect("sh を起動できる");
            assert!(
                out.status.success(),
                "{script} が失敗: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        let home = run("cd ~ && pwd");
        let got = run(&remote_shell_line("pwd", Some("~")));
        assert_eq!(got, home, "ホームへ移動していない");

        // 罠のディレクトリ (`<実験場>/~`) へ入っていないこと
        let trap_pwd = run("cd './~' && pwd");
        assert_ne!(got, trap_pwd, "`~` という名前のディレクトリへ入っている");

        // `~/` 形式と絶対パスも、実の sh で意味が合う
        std::fs::create_dir_all(lab.join("a b")).expect("作れる");
        let want = run(&format!("cd {} && pwd", posix_quote(&lab.join("a b").to_string_lossy())));
        let got = run(&remote_shell_line(
            "pwd",
            Some(&lab.join("a b").to_string_lossy()),
        ));
        assert_eq!(got, want);
    }

    // ───────── ローカル起動とリモート引用の分離 (指摘 4) ─────────

    /// 実行先が SSH で、鍵と known_hosts のパスに空白が入っている状況。
    fn spacey_opts() -> SshOptions {
        SshOptions {
            known_hosts: std::path::PathBuf::from("/home/my user/.zaivern/cloud/known hosts"),
            ..SshOptions::default()
        }
    }

    fn spacey_target() -> ExecutionTarget {
        target(
            "dev-01",
            TargetOpts {
                identity_file: Some(std::path::PathBuf::from("/home/my user/.ssh/id ed25519")),
                port: 2222,
                ..TargetOpts::default()
            },
        )
    }

    /// **ssh へ渡す各引数に、余計な引用符が 1 つも残っていないこと。**
    ///
    /// 引数配列で起動するなら引用は要らない。残っていたら、それは
    /// 「文字列に畳んでから割り直した」証拠で、Windows で壊れる形である。
    fn assert_no_stray_quotes(argv: &[String]) {
        for (i, a) in argv.iter().enumerate() {
            // 末尾のリモートスクリプトだけは、リモートのシェル向けの引用を持つ
            if i + 1 == argv.len() {
                continue;
            }
            assert!(
                !a.starts_with('\'') && !a.ends_with('\''),
                "argv[{i}] に引用符が残っている: {a:?}\n全体: {argv:?}"
            );
            assert!(!a.contains("'\\''"), "argv[{i}] が二重に引用されている: {a:?}");
        }
    }

    #[test]
    fn 起動計画はsshを引数配列で組む() {
        let t = spacey_target();
        let spec = LaunchSpec::new("worker", vec!["--msg".into(), "hello world".into()]);
        let plan = run_plan_for_spec(&t, &spec, &spacey_opts()).expect("組める");

        assert!(!plan.via_local_shell, "ローカルのシェルを挟んでいる");
        assert_eq!(plan.argv[0], "ssh");
        assert_no_stray_quotes(&plan.argv);

        // **空白を含むパスは 1 要素として渡る** (引用符では包まない)
        assert!(
            plan.argv.contains(&"/home/my user/.ssh/id ed25519".to_string()),
            "{:?}",
            plan.argv
        );
        assert!(
            plan.argv
                .contains(&"UserKnownHostsFile=/home/my user/.zaivern/cloud/known hosts".to_string()),
            "{:?}",
            plan.argv
        );
        // 接続の作法は既存の組み立てをそのまま使っている
        assert!(plan.argv.contains(&"StrictHostKeyChecking=yes".to_string()));
        assert!(plan.argv.contains(&"-p".to_string()) && plan.argv.contains(&"2222".to_string()));
        // 対話できるよう擬似端末を割り当てている
        assert!(plan.argv.contains(&"-tt".to_string()), "{:?}", plan.argv);

        // 末尾がリモートで走る 1 行。**ここだけが POSIX 引用**
        let script = plan.argv.last().expect("ある");
        assert_eq!(script, "'worker' '--msg' 'hello world'");
    }

    /// 危ない文字を含む引数が、**リモートまで字義どおり**届く形で載っていること。
    #[test]
    fn 危ない文字を含む引数も一要素として載る() {
        const CASES: &[&str] = &[
            "a b",
            "it's",
            "say \"hi\"",
            "a;id",
            "a&&id",
            "a|id",
            "$(id)",
            "`id`",
            "a\nb",
            "-rf",
            "*",
            "~/secret",
            "🌏",
        ];
        for c in CASES {
            let spec = LaunchSpec::new("worker", vec![(*c).to_string()]);
            let plan =
                run_plan_for_spec(&spacey_target(), &spec, &spacey_opts()).expect("組める");
            assert_no_stray_quotes(&plan.argv);
            let script = plan.argv.last().expect("ある");
            // リモートのシェルが読むのは「引用された 1 語」
            assert_eq!(
                script,
                &format!("'worker' {}", super::super::transport::ssh::posix_quote(c)),
                "{c:?}"
            );
            // ローカルのシェルは 1 枚も挟まない
            assert!(!plan.via_local_shell, "{c:?}");
        }
    }

    /// `--command` 形式でも、リモートなら引数配列で起動する。
    #[test]
    fn コマンド行形式でもローカルシェルを挟まない() {
        let t = spacey_target();
        let plan = run_plan_for_command(&t, "some-agent --flag 'a b'", Some("~/my work"), &spacey_opts())
            .expect("組める");
        assert!(!plan.via_local_shell);
        assert_eq!(plan.argv[0], "ssh");
        assert_no_stray_quotes(&plan.argv);
        let script = plan.argv.last().expect("ある");
        // 利用者の行はそのまま、cd だけ足す
        assert_eq!(script, "cd ~/'my work' && some-agent --flag 'a b'");
    }

    /// **手元の実行先では ssh を挟まず、そのまま起動する。**
    #[test]
    fn 手元の実行先はシェルを挟まず直に起動する() {
        let local = ExecutionTarget::local(1);
        let spec = LaunchSpec::new("echo", vec!["a;id".into(), "b c".into()]);
        let plan = run_plan_for_spec(&local, &spec, &spacey_opts()).expect("組める");
        assert!(!plan.via_local_shell, "手元でもシェルを挟まない");
        assert_eq!(plan.argv, vec!["echo", "a;id", "b c"]);

        // 利用者が書いた 1 行だけは、手元のシェルへ渡す (引用したのは利用者自身)
        let plan = run_plan_for_command(&local, "echo hi && echo bye", None, &spacey_opts())
            .expect("組める");
        assert!(plan.via_local_shell);
        assert_eq!(plan.argv, vec!["echo hi && echo bye"]);
    }

    #[test]
    fn 空の指定は起動計画にならない() {
        let t = spacey_target();
        assert!(run_plan_for_command(&t, "   ", None, &spacey_opts()).is_err());
        let empty = LaunchSpec::new("", vec![]);
        assert!(run_plan_for_spec(&t, &empty, &spacey_opts()).is_err());
    }

    /// **表示用の 1 行と、起動する argv は別物。**
    ///
    /// 表示用は POSIX シェルへ貼るための引用を持つ。起動用は持たない。
    /// 混ぜると Windows で壊れる。
    #[test]
    fn 表示用の行と起動用のargvを混ぜない() {
        let t = spacey_target();
        let spec = LaunchSpec::new("worker", vec!["a b".into()]);
        let line = launch_command_line(&t, &spec, &spacey_opts()).expect("組める");
        let plan = run_plan_for_spec(&t, &spec, &spacey_opts()).expect("組める");

        // 表示用は「1 本の文字列」で、要素が引用されている
        assert!(line.contains("'-tt'"), "{line}");
        // 起動用は引用していない
        assert!(plan.argv.contains(&"-tt".to_string()), "{:?}", plan.argv);
        assert!(!plan.argv.contains(&"'-tt'".to_string()), "{:?}", plan.argv);
        assert_ne!(line, plan.display());
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
