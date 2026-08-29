//! `zai team run` の起動要求 — **型付きで渡す。コマンド文字列を IPC しない。**
//!
//! ## なぜ型なのか
//!
//! 「起動中のエディタへ何かをやらせる」経路に未検証の文字列を通すと、
//! そこが任意コマンド実行の穴になる。ここは [`TeamLaunchRequest`] という
//! **決まった形**だけを受け渡しし、受け取り側は中身を必ず検証し直す。
//!
//! ## 受け渡しの実体
//!
//! 新しい IPC は足さない。既存の `~/.zaivern/` を投函箱として使う
//! (`hook` と同じ流儀):
//!
//! ```text
//! ~/.zaivern/team/<ワークスペースキー>/launch.json
//! ```
//!
//! * ワークスペース単位 — 別のフォルダで動いている GUI が拾わない
//! * 拾った側は**必ず消してから**処理する (二重処理の防止)
//! * サイズ上限つき・schema version つき
//! * ここに入るのは検証済みの値だけで、**コマンド文字列は 1 つも入らない**

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 起動要求の版。
pub const LAUNCH_VERSION: u32 = 1;
/// 投函ファイルのバイト上限。
pub const LAUNCH_MAX_BYTES: u64 = 64 * 1024;
/// 投函が古すぎたら拾わない (秒)。前回の実行の残骸で GUI が動き出さないため。
pub const LAUNCH_TTL_SECS: u64 = 600;
/// エージェント数の上限。
pub const MAX_AGENTS: usize = 64;

/// GUI へ渡す起動要求。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TeamLaunchRequest {
    #[serde(default = "default_version")]
    pub version: u32,
    pub workspace_root: PathBuf,
    pub spec_path: PathBuf,
    /// SPEC の中身。**ファイルを二度読ませない** (GUI 側が読む頃には
    /// 内容が変わっているかもしれない)。
    pub spec_text: String,
    pub agent_count: usize,
    /// `--yes` が付いていたか。**Start Team の確認だけ**を省ける。
    pub auto_start: bool,
    pub requested_at: u64,
}

fn default_version() -> u32 {
    LAUNCH_VERSION
}

/// 起動要求を作れなかった理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LaunchError {
    SpecNotFound(PathBuf),
    SpecNotFile(PathBuf),
    SpecTooLarge {
        bytes: u64,
        limit: u64,
    },
    SpecNotUtf8(PathBuf),
    SpecEmpty(PathBuf),
    /// SPEC がワークスペースの外にある。
    OutsideWorkspace {
        spec: PathBuf,
        workspace: PathBuf,
    },
    BadAgentCount(usize),
    Io(String),
}

impl LaunchError {
    pub fn detail(&self) -> String {
        match self {
            LaunchError::SpecNotFound(p) => format!("SPEC が見つかりません: {}", p.display()),
            LaunchError::SpecNotFile(p) => {
                format!("SPEC がファイルではありません: {}", p.display())
            }
            LaunchError::SpecTooLarge { bytes, limit } => {
                format!("SPEC が大きすぎます ({bytes} バイト / 上限 {limit})")
            }
            LaunchError::SpecNotUtf8(p) => {
                format!("SPEC が UTF-8 として読めません: {}", p.display())
            }
            LaunchError::SpecEmpty(p) => format!("SPEC が空です: {}", p.display()),
            LaunchError::OutsideWorkspace { spec, workspace } => format!(
                "SPEC ({}) がワークスペース ({}) の外にあります",
                spec.display(),
                workspace.display()
            ),
            LaunchError::BadAgentCount(n) => {
                format!("エージェント数 {n} は 1〜{MAX_AGENTS} の範囲で指定してください")
            }
            LaunchError::Io(e) => format!("起動要求を渡せません: {e}"),
        }
    }
}

/// SPEC の上限 (バイト)。
pub const SPEC_MAX_BYTES: u64 = 512 * 1024;

/// パスを可能なかぎり正規化する。実在しない場合は `canonicalize` できないので
/// 素のまま返す (検証は形でも行う)。
fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// 起動要求を組み立てる。**ここで全部検証する。**
pub fn build(
    workspace_root: &Path,
    spec_path: &Path,
    agent_count: usize,
    auto_start: bool,
) -> Result<TeamLaunchRequest, LaunchError> {
    if agent_count == 0 || agent_count > MAX_AGENTS {
        return Err(LaunchError::BadAgentCount(agent_count));
    }
    let ws = canon(workspace_root);
    let spec = canon(spec_path);
    if !spec.exists() {
        return Err(LaunchError::SpecNotFound(spec));
    }
    let meta = std::fs::metadata(&spec).map_err(|e| LaunchError::Io(e.to_string()))?;
    if !meta.is_file() {
        return Err(LaunchError::SpecNotFile(spec));
    }
    if meta.len() > SPEC_MAX_BYTES {
        return Err(LaunchError::SpecTooLarge {
            bytes: meta.len(),
            limit: SPEC_MAX_BYTES,
        });
    }
    // **ワークスペース境界。** 外のファイルを読ませない。
    if !spec.starts_with(&ws) {
        return Err(LaunchError::OutsideWorkspace {
            spec,
            workspace: ws,
        });
    }
    let raw = std::fs::read(&spec).map_err(|e| LaunchError::Io(e.to_string()))?;
    let text = String::from_utf8(raw).map_err(|_| LaunchError::SpecNotUtf8(spec.clone()))?;
    if text.trim().is_empty() {
        return Err(LaunchError::SpecEmpty(spec));
    }
    Ok(TeamLaunchRequest {
        version: LAUNCH_VERSION,
        workspace_root: ws,
        spec_path: spec,
        spec_text: text,
        agent_count,
        auto_start,
        requested_at: super::model::now_secs(),
    })
}

/// 投函先。
pub fn launch_path(workspace: &Path) -> PathBuf {
    super::persistence::team_dir(workspace).join("launch.json")
}

/// 起動要求を投函する (既存の GUI があればそれが拾う)。
pub fn post(req: &TeamLaunchRequest) -> Result<PathBuf, LaunchError> {
    let path = launch_path(&req.workspace_root);
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| LaunchError::Io(e.to_string()))?;
    let body = serde_json::to_string_pretty(req).map_err(|e| LaunchError::Io(e.to_string()))?;
    let tmp = dir.join(format!(".launch.{}.tmp", std::process::id()));
    std::fs::write(&tmp, body).map_err(|e| LaunchError::Io(e.to_string()))?;
    std::fs::rename(&tmp, &path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        LaunchError::Io(e.to_string())
    })?;
    Ok(path)
}

/// 投函を 1 件だけ取り出す。**取り出したら必ず消す** (二重処理の防止)。
///
/// 古すぎる投函・大きすぎる投函・版違いは拾わずに消す。
pub fn take(workspace: &Path, now: u64) -> Option<TeamLaunchRequest> {
    let path = launch_path(workspace);
    let meta = std::fs::metadata(&path).ok()?;
    if meta.len() > LAUNCH_MAX_BYTES {
        let _ = std::fs::remove_file(&path);
        return None;
    }
    let raw = std::fs::read_to_string(&path).ok();
    // **読めても読めなくても、まず消す。** 残すと次のフレームでまた拾う。
    let _ = std::fs::remove_file(&path);
    let raw = raw?;
    let req: TeamLaunchRequest = serde_json::from_str(&raw).ok()?;
    if req.version != LAUNCH_VERSION {
        return None;
    }
    if now.saturating_sub(req.requested_at) > LAUNCH_TTL_SECS {
        return None;
    }
    if req.agent_count == 0 || req.agent_count > MAX_AGENTS {
        return None;
    }
    // **受け取り側でも境界を確かめ直す。** 投函箱を書き換えられても通さない。
    if !req.spec_path.starts_with(&req.workspace_root) {
        return None;
    }
    if req.spec_text.trim().is_empty() {
        return None;
    }
    Some(req)
}

// ── 検証コマンドの実行 ───────────────────────────────────────────────

/// **Windows で `.cmd` / `.bat` として配られる実行体。**
///
/// `npm` / `yarn` / `pnpm` などは Windows では `npm.cmd` であり、
/// `Command::new("npm")` は `NotFound` で落ちる (実体が `npm` という名前で
/// 存在しないため)。**「見つからない」で検証が全部 127 になる**ので、
/// この一覧に載っているものは `cmd /C` 越しに起こし直す。
///
/// **コマンド全体を 1 引数へ押し込まない。** Rust の `Command` は引数ごとに
/// Windows の規則で引用するので、cmd 側の再解析とずれて失敗する
/// (CLAUDE.md の既知の罠)。語に分けたまま渡す。
pub const WINDOWS_SHIM_BINS: &[&str] = &[
    "npm", "npx", "yarn", "pnpm", "bun", "tsc", "eslint", "prettier", "jest", "vitest", "biome",
    "just", "gradle", "mvn", "flutter", "composer", "rake", "bundle", "tox",
];

/// この語は Windows で `cmd /C` 越しに起こす必要があるか (純関数)。
///
/// **判定を切り出してあるのは、Windows のビルドが手元で回らない環境でも
/// 表で固定できるようにするため。** `#[cfg(windows)]` の中に埋めると、
/// macOS / Linux では 1 度もコンパイルされない (CLAUDE.md の実測)。
pub fn needs_windows_shim(head: &str) -> bool {
    let base = head.rsplit(['/', '\\']).next().unwrap_or(head);
    // 拡張子つきで書かれていたら、そのまま起こせる
    if base.contains('.') {
        return false;
    }
    WINDOWS_SHIM_BINS.contains(&base)
}

/// 検証コマンド 1 本を実行する。
///
/// **シェルを挟まない。** 語に分けて実体を直に起こす — `sh -c` を通すと
/// 「コマンド、引数、cwd を分離して扱う」という約束が崩れる。
/// 許可リスト ([`super::graph::check_command`]) を通っていないものは
/// 実行せず、終了コード 126 (実行不可) を返す。
///
/// Windows で `.cmd` 配布の実行体だけは `cmd /C` を挟むが、そこでも
/// **引数は分けたまま**渡す ([`needs_windows_shim`])。
pub fn run_validation_command(cmd: &str, cwd: &Path) -> super::model::ValidationRun {
    let fail = |code: i32| super::model::ValidationRun {
        command: cmd.to_string(),
        exit_code: code,
    };
    if super::graph::check_command(cmd).is_err() {
        return fail(126);
    }
    let mut words = cmd.split_whitespace();
    let Some(head) = words.next() else {
        return fail(126);
    };
    let args: Vec<&str> = words.collect();

    let mut command = if cfg!(windows) && needs_windows_shim(head) {
        let mut c = std::process::Command::new("cmd");
        c.arg("/C").arg(head);
        c
    } else {
        std::process::Command::new(head)
    };
    let out = command
        .args(&args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    match out {
        // 127 = コマンドが見つからない (シェルの慣習に合わせる)
        Err(_) => fail(127),
        Ok(s) => super::model::ValidationRun {
            command: cmd.to_string(),
            exit_code: s.code().unwrap_or(1),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-team-launch", name)
    }

    fn write_spec(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("SPEC.md");
        std::fs::write(&p, body).unwrap();
        p
    }

    #[test]
    fn 正しい要求を組み立てる() {
        let dir = ws("build");
        let spec = write_spec(&dir, "# a\n## 要件\n- x\n");
        let r = build(&dir, &spec, 4, false).expect("組み立てられるべき");
        assert_eq!(r.agent_count, 4);
        assert!(r.spec_text.contains("要件"));
        assert!(r.spec_path.starts_with(&r.workspace_root));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 存在しないspecを拒否する() {
        let dir = ws("missing");
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            build(&dir, &dir.join("nope.md"), 4, false),
            Err(LaunchError::SpecNotFound(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 空のspecを拒否する() {
        let dir = ws("empty");
        let spec = write_spec(&dir, "   \n\n");
        assert!(matches!(
            build(&dir, &spec, 4, false),
            Err(LaunchError::SpecEmpty(_))
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ワークスペース外のspecを拒否する() {
        let a = ws("outside-a");
        let b = ws("outside-b");
        std::fs::create_dir_all(&a).unwrap();
        let spec = write_spec(&b, "# x\n- y\n");
        assert!(matches!(
            build(&a, &spec, 4, false),
            Err(LaunchError::OutsideWorkspace { .. })
        ));
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn エージェント数の範囲を守る() {
        let dir = ws("agents");
        let spec = write_spec(&dir, "# x\n- y\n");
        assert!(matches!(
            build(&dir, &spec, 0, false),
            Err(LaunchError::BadAgentCount(0))
        ));
        assert!(matches!(
            build(&dir, &spec, MAX_AGENTS + 1, false),
            Err(LaunchError::BadAgentCount(_))
        ));
        assert!(build(&dir, &spec, MAX_AGENTS, false).is_ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 投函は一度しか拾わない() {
        let dir = ws("post-take");
        let spec = write_spec(&dir, "# x\n- y\n");
        let req = build(&dir, &spec, 2, true).unwrap();
        post(&req).unwrap();
        let got = take(&req.workspace_root, req.requested_at).expect("拾えるべき");
        assert_eq!(got.agent_count, 2);
        assert!(got.auto_start);
        // 2 回目は無い (再描画のたびに再実行しない)
        assert_eq!(take(&req.workspace_root, req.requested_at), None);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(super::super::persistence::team_dir(&req.workspace_root)).ok();
    }

    #[test]
    fn 古い投函は拾わない() {
        let dir = ws("stale");
        let spec = write_spec(&dir, "# x\n- y\n");
        let req = build(&dir, &spec, 2, false).unwrap();
        post(&req).unwrap();
        let later = req.requested_at + LAUNCH_TTL_SECS + 1;
        assert_eq!(take(&req.workspace_root, later), None);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(super::super::persistence::team_dir(&req.workspace_root)).ok();
    }

    #[test]
    fn 版違いの投函は拾わない() {
        let dir = ws("version");
        let spec = write_spec(&dir, "# x\n- y\n");
        let mut req = build(&dir, &spec, 2, false).unwrap();
        req.version = LAUNCH_VERSION + 1;
        post(&req).unwrap();
        assert_eq!(take(&req.workspace_root, req.requested_at), None);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(super::super::persistence::team_dir(&req.workspace_root)).ok();
    }

    #[test]
    fn 境界を偽った投函は受け取り側でも弾く() {
        let dir = ws("forged");
        let spec = write_spec(&dir, "# x\n- y\n");
        let mut req = build(&dir, &spec, 2, false).unwrap();
        // 投函箱を書き換えて「ワークスペース外の SPEC」を渡そうとする
        req.spec_path = PathBuf::from("/etc/passwd");
        post(&req).unwrap();
        assert_eq!(take(&req.workspace_root, req.requested_at), None);
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(super::super::persistence::team_dir(&req.workspace_root)).ok();
    }

    #[test]
    fn windowsで_cmd越しに起こす語を表で固定する() {
        // **どの OS でも同じ表を検査する。** `#[cfg(windows)]` の中に判定を
        // 埋めると macOS / Linux では 1 度もコンパイルされず、Windows の CI
        // まで誰も気付かない (CLAUDE.md の実測)。
        for yes in ["npm", "yarn", "pnpm", "tsc", "jest", "C:/tools/npm", "bin/npm"] {
            assert!(needs_windows_shim(yes), "{yes} を素で起こしてしまう");
        }
        for no in ["cargo", "go", "python3", "npm.cmd", "npm.exe", "make"] {
            assert!(!needs_windows_shim(no), "{no} に余計な cmd を挟む");
        }
    }

    #[test]
    fn 許可されていないコマンドは実行しない() {
        let dir = ws("exec");
        std::fs::create_dir_all(&dir).unwrap();
        let r = run_validation_command("rm -rf /", &dir);
        assert_eq!(r.exit_code, 126, "危険なコマンドを実行してしまった");
        std::fs::remove_dir_all(&dir).ok();
    }
}
