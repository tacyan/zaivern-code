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

use super::model::{ValidationOutcome, ValidationRun};
use super::validation_command::ValidationCommand;

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

/// パスを可能なかぎり正規化する。
///
/// 実在すれば `canonicalize` (symlink を辿り、`..` を畳み、Windows では
/// ドライブ表記も揃う)。実在しないものは辿れないので、**形だけ**
/// 畳んだものを返す ([`lexical_normalize`])。
///
/// **素のまま返してはいけない。** `a/../b` と `b` が別物のままだと、
/// 「内側か」の判定 (`starts_with`) が形の違いだけで通ったり落ちたりする。
fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| lexical_normalize(p))
}

/// ファイルシステムに触らずにパスを畳む (`.` を捨て、`..` を 1 つ戻す)。
///
/// 実在しないパスにも使えるのが要点。**先頭の `..` は残す** — 畳めない
/// 脱出は「畳めなかった」まま残し、境界の判定で落とす。
fn lexical_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                // 直前が普通の名前のときだけ戻せる。根や `..` の上は戻せない。
                let pop = matches!(
                    out.components().next_back(),
                    Some(Component::Normal(_))
                );
                if pop {
                    out.pop();
                } else {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    if out.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        out
    }
}

/// **投函の中の値は権限を持たない。**
///
/// 起動要求は未信頼データである。`spec_path` が要求の中の `workspace_root`
/// の内側かを見ても意味が無い — `workspace_root` を `/` に書き換えれば
/// どんな絶対パスでも「内側」になる。判定の基準は**いま開いている
/// workspace** (呼び出し側が渡す信頼済みの値) だけ。
///
/// 戻り値は「この要求を、この workspace で受け取ってよいか」。
pub fn request_matches_workspace(req: &TeamLaunchRequest, workspace: &Path) -> bool {
    let current = canon(workspace);
    // 要求は workspace を**宣言**できるが、それが権限になってはいけない。
    // 宣言が現在の workspace と食い違うなら、その要求はここ宛てではない。
    if canon(&req.workspace_root) != current {
        return false;
    }
    // SPEC は**現在の** workspace の内側にあること (symlink の先まで見る)。
    canon(&req.spec_path).starts_with(&current)
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

/// 投函先 = `<根>/team/<ワークスペースキー>/launch.json`。
///
/// **根は必ず呼び出し側が渡す。** 素で `~/.zaivern` を指す入口を残すと、
/// テストが利用者の置き場へ書いてしまう。
pub fn launch_path_in(root: &Path, workspace: &Path) -> PathBuf {
    super::persistence::team_dir_in(root, workspace).join("launch.json")
}

/// 起動要求を投函する (既存の GUI があればそれが拾う)。
pub fn post_in(root: &Path, req: &TeamLaunchRequest) -> Result<PathBuf, LaunchError> {
    let path = launch_path_in(root, &req.workspace_root);
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
pub fn take_in(root: &Path, workspace: &Path, now: u64) -> Option<TeamLaunchRequest> {
    let path = launch_path_in(root, workspace);
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
    // 基準は要求の中の `workspace_root` ではなく、呼び出し側が渡した
    // **いま開いている workspace** ([`request_matches_workspace`])。
    if !request_matches_workspace(&req, workspace) {
        return None;
    }
    if req.spec_text.trim().is_empty() {
        return None;
    }
    Some(req)
}

// ── 検証コマンドの実行 ───────────────────────────────────────────────

// **Windows の `.cmd` / `.bat` は、拡張子を解決してから起こす。**
//
// `npm` / `yarn` / `pnpm` などは Windows では `npm.cmd` なので、名前の
// ままでは見つからない。以前は `cmd /C npm` と**シェルを挟んで**いたが、
// それは cmd.exe にもう一度解釈させることでもあった (`%VAR%` の展開と
// `!` の遅延展開で、判定した文字列と実行される文字列が別物になる)。
//
// いまは `validation_command::resolve_in` が `PATHEXT` を見て `npm.cmd` の
// **実体**を確定し、そのパスをそのまま `Command` へ渡す (`PATHEXT` を
// **素の名前より先に**当てる — 逆にすると拡張子なしの sh スクリプトを
// 選んでしまう)。バッチファイルの引数の逃がし方は std が持っている
// (Rust 1.77.2 以降)。std が逃がせない字 (`%` / 改行) は
// `graph::SHELL_METACHARS` が先に断っている。
//
// **どの項目にも付かない説明なので `///` にしない。** 付けると
// rustdoc の対象が無いまま `unused_doc_comments` が出る。

/// 検証の既定の時間切れ (秒)。
///
/// **無期限に待たない。** `cargo test` / `npm test` / `pytest` は、実装の
/// 不具合ひとつで終わらなくなる。待ち続けると、そのタスクは永久に
/// `Validating` に残り、Team Run 全体が静かに止まる。
pub const VALIDATION_TIMEOUT_SECS: u64 = 600;

/// 実行中の検証を外から止めるための札。
///
/// **スレッドを畳むだけでは子プロセスが残る。** 立てられた札を見た実行器が
/// [`crate::procx::kill_tree`] でプロセスツリーごと終了させる。
pub type CancelFlag = std::sync::Arc<std::sync::atomic::AtomicBool>;

/// 止まっていない札を 1 つ作る。
pub fn new_cancel_flag() -> CancelFlag {
    std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
}

/// **いま走っている子プロセスの PID** (0 = 走っていない)。
///
/// 札を立てるだけでは足りない場面がある — アプリを閉じるときは、札を見る
/// はずの worker スレッドごと消えるので、**誰も木を落とさない**。子は自分の
/// プロセスグループを持っているので親と一緒には死なない (`cargo test` が
/// 残り続ける)。そこで PID を外から見えるところに置き、閉じる側が同期的に
/// 落とせるようにする。
pub type PidSlot = std::sync::Arc<std::sync::atomic::AtomicU32>;

/// 空の PID 置き場を作る。
pub fn new_pid_slot() -> PidSlot {
    std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0))
}

/// 生きている子を待つときの刻み。短すぎると空回りで CPU を食い、長すぎると
/// 停止の反応が鈍る。
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// 検証コマンド 1 本を実行する。
///
/// **シェルを挟まない。** 実行体と引数は分かれたまま OS へ渡す — 文字列へ
/// 連結し直すと、`sh -c` や `cmd /C` がもう一度解釈して、判定したものと
/// 実行されるものがずれる。
///
/// **実体は自分で解決する。** `Command::new("rustfmt")` に任せると OS が
/// もう一度 PATH を引くので、`PATH=<workspace>/bin:$PATH` に偽の実行体を
/// 置くだけで乗っ取られる。ここで確定した絶対パスをそのまま渡すので、
/// 判定した実体と OS が実行する実体が同じであることが構造的に決まる。
///
/// **必ず決着する。** 時間切れ・停止・起動失敗のどれでも、対応する
/// [`ValidationOutcome`] を持った結果を返す (返さない経路は無い)。
pub fn run_validation_command(
    cmd: &ValidationCommand,
    approved: &[ValidationCommand],
    cwd: &Path,
    timeout: std::time::Duration,
    cancel: &CancelFlag,
    pid_slot: &PidSlot,
) -> ValidationRun {
    let path = std::env::var("PATH").ok();
    let pathext = std::env::var("PATHEXT").ok();
    run_validation_command_in(
        cmd,
        approved,
        cwd,
        timeout,
        cancel,
        pid_slot,
        path.as_deref(),
        pathext.as_deref(),
    )
}

/// PATH を明示して実行する。
///
/// **探索の入力を引数にしてある。** 環境変数を差し替えるテストは並列に
/// 走る他のテストへ漏れるので、`PATH` の先頭に workspace が入っている
/// 状況は**この入口から**再現する (`team_dir_in` / `post_in` と同じ流儀)。
///
/// `approved` は**人が承認したコマンドの一覧**。読むだけ (`ReadOnly`)
/// 以外は、ここに載っていなければ実行しない。
#[allow(clippy::too_many_arguments)]
pub fn run_validation_command_in(
    cmd: &ValidationCommand,
    approved: &[ValidationCommand],
    cwd: &Path,
    timeout: std::time::Duration,
    cancel: &CancelFlag,
    pid_slot: &PidSlot,
    path_var: Option<&str>,
    pathext: Option<&str>,
) -> ValidationRun {
    let label = cmd.display();
    // **呼ぶ側の判定を信じ切らない。** ここは実行の直前なので、
    // 危険度と承認の両方をもう一度見る。`Forbidden` だけを見ていると、
    // 承認ゲートを通らずに実行器へ届いた `black .` が何の抵抗もなく走る
    // — ゲートが 1 か所にしか無い状態は、そこを迂回されたときに
    // 何も残らないということでもある。
    let risk = super::graph::classify(cmd);
    if risk == super::graph::ValidationRisk::Forbidden {
        return ValidationRun::new(label, 126, ValidationOutcome::SpawnFailed);
    }
    if !risk.auto_runnable() && !approved.contains(cmd) {
        return ValidationRun::new(
            format!("{label}  (承認の証跡がありません)"),
            126,
            ValidationOutcome::SpawnFailed,
        );
    }
    // **実体を確定する。** 信用できない場所にあれば実行しない。
    let program =
        match super::validation_command::resolve_in(&cmd.executable, cwd, path_var, pathext) {
            Ok(p) => p,
            Err(e) => {
                // **理由を残す。** 「見つからない」と「信用できない場所に
                // あった」は直し方がまるで違う (入れるか、PATH を直すか)。
                return ValidationRun::new(
                    format!("{label}  ({})", e.detail()),
                    127,
                    ValidationOutcome::SpawnFailed,
                );
            }
        };
    let args: Vec<&str> = cmd.args.iter().map(|s| s.as_str()).collect();
    let out = run_resolved(&program, &args, cwd, timeout, cancel, pid_slot);
    ValidationRun::new(label, out.0, out.1)
}

/// **検証コマンドの並びを順に実行する。**
///
/// 並べ方の決まりごと (どこで打ち切るか) は実行器の側に置く — GUI の
/// 橋渡し層に書くと、テストから 1 度も通らない場所に判断が住むことになる。
///
/// * 1 本落ちたら残りは走らせない (判定は変わらず、時間と資源を使うだけ)
/// * **1 本ごとに停止の札を見る。** 見ないと、止めた後に始まった次の
///   コマンドが「誰も知らない子」として走り出す (自分のプロセスグループ
///   を持つので、アプリが終わっても死なない)
pub fn run_validation_list(
    commands: &[ValidationCommand],
    approved: &[ValidationCommand],
    cwd: &Path,
    timeout: std::time::Duration,
    cancel: &CancelFlag,
    pid_slot: &PidSlot,
) -> Vec<ValidationRun> {
    let mut runs = Vec::with_capacity(commands.len());
    for c in commands {
        let r = run_validation_command(c, approved, cwd, timeout, cancel, pid_slot);
        let stop = !r.ok();
        runs.push(r);
        if stop {
            break;
        }
    }
    runs
}

/// **解決済みの実体**を起こす (終了コードと終わり方を返す)。
///
/// `Command::new` へ渡すのは絶対パス。**名前を渡さない** — 渡すと OS が
/// PATH を引き直し、こちらが判定したのとは別の実体が動きうる。
///
/// Windows の `.cmd` / `.bat` は std が cmd.exe 越しに起こす (引数の
/// 逃がし方も std が持っている)。**自分で `cmd /C` を組み立てない** —
/// 組み立てると `%VAR%` の展開や `^` の脱出で、判定した文字列と
/// cmd.exe が解釈する文字列が別物になる。語に入りうるメタ文字は
/// [`super::graph::SHELL_METACHARS`] が先に断っている。
pub fn run_resolved(
    program: &Path,
    args: &[&str],
    cwd: &Path,
    timeout: std::time::Duration,
    cancel: &CancelFlag,
    pid_slot: &PidSlot,
) -> (i32, ValidationOutcome) {
    use std::sync::atomic::Ordering;

    let mut command = crate::procx::hidden_command(program);
    command
        .args(args)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // **unix では自分のプロセスグループを持たせる。** そうしないと
    // `kill_tree` (killpg) が Zaivern 自身を巻き込む。孫まで届くのも
    // グループがあってこそ。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    // **起こす前に札を見る。** 見ないと、停止を頼まれた後に始まった
    // コマンドが「誰も知らない子」として走り出す。
    if cancel.load(Ordering::Relaxed) {
        return (130, ValidationOutcome::Cancelled);
    }
    // 127 = コマンドが見つからない (シェルの慣習に合わせる)
    let Ok(mut child) = command.spawn() else {
        return (127, ValidationOutcome::SpawnFailed);
    };
    let pid = child.id();
    // **外から落とせるようにする。** 閉じる側 (`on_exit`) は worker を
    // 待てないので、PID を見て自分で木を落とす。
    pid_slot.store(pid, Ordering::Relaxed);
    let _clear_pid = PidGuard(pid_slot);
    let start = std::time::Instant::now();
    loop {
        match child.try_wait() {
            // **終わったと分かった瞬間に PID を伏せる。** 回収済みの PID へ
            // 撃つと、OS が同じ番号を別のプロセスへ再利用していたときに
            // 巻き添えにする (CLAUDE.md の「終了済みへ kill を撃たない」)。
            // `PidGuard` は関数を抜けるときにも伏せるが、こちらのほうが早い。
            Ok(Some(st)) => {
                pid_slot.store(0, Ordering::Relaxed);
                return (st.code().unwrap_or(1), from_status(st));
            }
            // 待っている相手が居ない = 取りこぼし。待ち続けない。
            Err(_) => {
                pid_slot.store(0, Ordering::Relaxed);
                return (1, ValidationOutcome::RunnerDisconnected);
            }
            Ok(None) => {}
        }
        let stop = if cancel.load(Ordering::Relaxed) {
            Some(ValidationOutcome::Cancelled)
        } else if start.elapsed() >= timeout {
            Some(ValidationOutcome::TimedOut)
        } else {
            None
        };
        if let Some(why) = stop {
            // **木ごと落とす。** 直接の子だけを殺すと、孫が cwd を握ったまま
            // 残る (既存の `procx::kill_tree` を使う — Team 専用の第 2 の
            // プロセス管理を作らない)。
            crate::procx::kill_tree(pid);
            let _ = child.wait();
            pid_slot.store(0, Ordering::Relaxed);
            return (
                if why == ValidationOutcome::TimedOut {
                    124
                } else {
                    130
                },
                why,
            );
        }
        std::thread::sleep(POLL);
    }
}

/// 抜けるときに PID 置き場を必ず空にする (`?` でも panic でも通る)。
struct PidGuard<'a>(&'a PidSlot);

impl Drop for PidGuard<'_> {
    fn drop(&mut self) {
        self.0.store(0, std::sync::atomic::Ordering::Relaxed);
    }
}

fn from_status(st: std::process::ExitStatus) -> ValidationOutcome {
    if st.success() {
        ValidationOutcome::Passed
    } else {
        ValidationOutcome::Failed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-team-launch", name)
    }

    /// **実 `~/.zaivern` に 1 バイトも触らない**ための置き場。
    fn test_home(dir: &Path) -> PathBuf {
        dir.join(".zaivern-test-home")
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
        let home = test_home(&dir);
        post_in(&home, &req).unwrap();
        let got = take_in(&home, &req.workspace_root, req.requested_at).expect("拾えるべき");
        assert_eq!(got.agent_count, 2);
        assert!(got.auto_start);
        // 2 回目は無い (再描画のたびに再実行しない)
        assert_eq!(take_in(&home, &req.workspace_root, req.requested_at), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 古い投函は拾わない() {
        let dir = ws("stale");
        let spec = write_spec(&dir, "# x\n- y\n");
        let req = build(&dir, &spec, 2, false).unwrap();
        let home = test_home(&dir);
        post_in(&home, &req).unwrap();
        let later = req.requested_at + LAUNCH_TTL_SECS + 1;
        assert_eq!(take_in(&home, &req.workspace_root, later), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 版違いの投函は拾わない() {
        let dir = ws("version");
        let spec = write_spec(&dir, "# x\n- y\n");
        let mut req = build(&dir, &spec, 2, false).unwrap();
        req.version = LAUNCH_VERSION + 1;
        let home = test_home(&dir);
        post_in(&home, &req).unwrap();
        assert_eq!(take_in(&home, &req.workspace_root, req.requested_at), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 境界を偽った投函は受け取り側でも弾く() {
        let dir = ws("forged");
        let spec = write_spec(&dir, "# x\n- y\n");
        let mut req = build(&dir, &spec, 2, false).unwrap();
        // 投函箱を書き換えて「ワークスペース外の SPEC」を渡そうとする
        req.spec_path = PathBuf::from("/etc/passwd");
        let home = test_home(&dir);
        post_in(&home, &req).unwrap();
        assert_eq!(take_in(&home, &req.workspace_root, req.requested_at), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 投函箱を書き換えたうえで、**現在開いている workspace** へ持ち込もうと
    /// する筋書き。判定の基準は要求の中の `workspace_root` ではなく、
    /// 呼び出し側から渡された現在の workspace でなければならない。
    fn forged(dir: &Path, mutate: impl FnOnce(&mut TeamLaunchRequest)) -> Option<TeamLaunchRequest> {
        let spec = write_spec(dir, "# x\n- y\n");
        let mut req = build(dir, &spec, 2, false).unwrap();
        let home = test_home(dir);
        mutate(&mut req);
        // 投函先は**現在の workspace の箱**にする (攻撃者は自分の GUI が
        // 見ている箱へ書ける)。
        let path = launch_path_in(&home, dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&req).unwrap()).unwrap();
        let at = req.requested_at;
        take_in(&home, dir, at)
    }

    #[test]
    fn 同じworkspaceの正しい要求は通る() {
        let dir = ws("ws-ok");
        assert!(forged(&dir, |_| {}).is_some(), "正しい要求を弾いた");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 要求の中のworkspaceは権限を持たない() {
        // **`workspace_root` を書き換えれば境界を広げられる**、が成り立たない
        // ことを見る。`/` にすれば `spec_path.starts_with(workspace_root)` は
        // 必ず通るので、要求の中の値を基準にしてはいけない。
        let dir = ws("ws-root");
        let evil = dir.join("outside-spec.md");
        std::fs::write(&evil, "# evil\n- y\n").unwrap();
        for (name, mutate) in [
            (
                "root",
                Box::new(|r: &mut TeamLaunchRequest| {
                    r.workspace_root = PathBuf::from(if cfg!(windows) { "C:\\" } else { "/" });
                    r.spec_path = PathBuf::from(if cfg!(windows) {
                        "C:\\Windows\\evil.md"
                    } else {
                        "/etc/passwd"
                    });
                }) as Box<dyn FnOnce(&mut TeamLaunchRequest)>,
            ),
            (
                "別の workspace",
                Box::new(|r: &mut TeamLaunchRequest| {
                    r.workspace_root = PathBuf::from(if cfg!(windows) {
                        "C:\\other\\place"
                    } else {
                        "/other/place"
                    });
                    r.spec_path = r.workspace_root.join("SPEC.md");
                }),
            ),
            (
                "Windows のドライブ差し替え",
                Box::new(|r: &mut TeamLaunchRequest| {
                    r.workspace_root = PathBuf::from("D:\\elsewhere");
                    r.spec_path = PathBuf::from("D:\\elsewhere\\SPEC.md");
                }),
            ),
            (
                "UNC",
                Box::new(|r: &mut TeamLaunchRequest| {
                    r.workspace_root = PathBuf::from("\\\\host\\share");
                    r.spec_path = PathBuf::from("\\\\host\\share\\SPEC.md");
                }),
            ),
        ] {
            let got = forged(&dir, mutate);
            assert!(got.is_none(), "{name} を通してしまった: {got:?}");
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn workspace外のspecは拒否する() {
        let dir = ws("ws-escape");
        // `..` で外へ出る / 別の絶対パス / 現在 workspace の外の実在ファイル
        let outside = dir.parent().map(|p| p.join("outside.md"));
        if let Some(o) = &outside {
            std::fs::write(o, "# x\n- y\n").ok();
        }
        let up = dir.join("..").join("outside.md");
        for (name, path) in [
            ("..", up),
            (
                "絶対パス",
                PathBuf::from(if cfg!(windows) {
                    "C:\\Windows\\evil.md"
                } else {
                    "/etc/passwd"
                }),
            ),
        ] {
            let got = forged(&dir, |r| r.spec_path = path);
            assert!(got.is_none(), "{name} の SPEC を通してしまった: {got:?}");
        }
        if let Some(o) = outside {
            std::fs::remove_file(o).ok();
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    /// symlink で workspace の外を指す SPEC。**実体の位置で判定する。**
    #[cfg(unix)]
    #[test]
    fn symlink越しにworkspace外を指すspecは拒否する() {
        let dir = ws("ws-symlink");
        std::fs::create_dir_all(&dir).unwrap();
        let outside_dir = dir.join("..").join("zaivern-outside-symlink");
        std::fs::create_dir_all(&outside_dir).ok();
        let real = outside_dir.join("evil.md");
        std::fs::write(&real, "# evil\n- y\n").unwrap();
        let link = dir.join("linked-spec.md");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let got = forged(&dir, |r| r.spec_path = link);
        assert!(got.is_none(), "symlink 越しに外を指す SPEC を通した: {got:?}");
        std::fs::remove_dir_all(&outside_dir).ok();
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 別のworkspaceが開いているときは拾わない() {
        // 投函は workspace ごとの箱に入るが、**箱の場所も未信頼**なので、
        // 中身の `workspace_root` が現在の workspace と一致することまで見る。
        let a = ws("ws-a");
        let b = ws("ws-b");
        let spec = write_spec(&a, "# x\n- y\n");
        let req = build(&a, &spec, 2, false).unwrap();
        let home = test_home(&a);
        // A 宛ての要求を、B の箱へ置く。
        let path = launch_path_in(&home, &b);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string(&req).unwrap()).unwrap();
        assert_eq!(
            take_in(&home, &b, req.requested_at),
            None,
            "別の workspace 宛ての要求を拾った"
        );
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }


    /// テスト用: 名前を PATH から素直に引いて `run_resolved` を呼ぶ。
    ///
    /// **本番はここを通らない** — 本番は `resolve` が信用できない場所を
    /// 弾いてから同じ `run_resolved` を呼ぶ。ここで確かめたいのは
    /// 「起こしてから畳むまで」の振る舞いだけなので、解決だけ素朴にする。
    fn run_words(
        head: &str,
        args: &[&str],
        cwd: &Path,
        timeout: std::time::Duration,
        cancel: &CancelFlag,
        pid: &PidSlot,
    ) -> (i32, ValidationOutcome) {
        let program = which_for_test(head);
        run_resolved(&program, args, cwd, timeout, cancel, pid)
    }

    /// PATH から素朴に引く (テスト専用)。見つからなければ名前のまま返す
    /// (`spawn` が失敗して `SpawnFailed` になる、という筋書きに使う)。
    fn which_for_test(name: &str) -> PathBuf {
        let Ok(path) = std::env::var("PATH") else {
            return PathBuf::from(name);
        };
        let sep = if cfg!(windows) { ';' } else { ':' };
        for dir in path.split(sep) {
            for ext in if cfg!(windows) {
                vec!["", ".exe", ".cmd", ".bat"]
            } else {
                vec![""]
            } {
                let p = Path::new(dir).join(format!("{name}{ext}"));
                if p.is_file() {
                    return p;
                }
            }
        }
        PathBuf::from(name)
    }

    /// 終わらないコマンド (OS ごとに実体が違う)。
    fn forever() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            ("ping", vec!["-n", "60", "127.0.0.1"])
        } else {
            ("sleep", vec!["60"])
        }
    }

    #[test]
    fn 正常終了と異常終了を見分ける() {
        let dir = ws("exec-status");
        std::fs::create_dir_all(&dir).unwrap();
        let t = std::time::Duration::from_secs(30);
        let (ok_head, ok_args) = if cfg!(windows) {
            ("cmd", vec!["/C", "exit", "0"])
        } else {
            ("true", vec![])
        };
        let (ng_head, ng_args) = if cfg!(windows) {
            ("cmd", vec!["/C", "exit", "3"])
        } else {
            ("false", vec![])
        };
        assert_eq!(
            run_words(ok_head, &ok_args, &dir, t, &new_cancel_flag(), &new_pid_slot()),
            (0, ValidationOutcome::Passed)
        );
        let (code, out) = run_words(ng_head, &ng_args, &dir, t, &new_cancel_flag(), &new_pid_slot());
        assert_eq!(out, ValidationOutcome::Failed);
        assert_ne!(code, 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 起動できないコマンドはspawn_failedになる() {
        let dir = ws("exec-nospawn");
        std::fs::create_dir_all(&dir).unwrap();
        let (code, out) = run_words(
            "zaivern-no-such-binary-9f3a",
            &[],
            &dir,
            std::time::Duration::from_secs(5),
            &new_cancel_flag(),
            &new_pid_slot(),
        );
        assert_eq!(out, ValidationOutcome::SpawnFailed);
        assert_eq!(code, 127);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 終わらない検証は時間切れで打ち切る() {
        // **無期限に待たない。** 待つと、そのタスクは永久に `Validating`
        // に残り、Team Run 全体が静かに止まる。
        let dir = ws("exec-timeout");
        std::fs::create_dir_all(&dir).unwrap();
        let (head, args) = forever();
        let start = std::time::Instant::now();
        let (code, out) = run_words(
            head,
            &args,
            &dir,
            std::time::Duration::from_millis(300),
            &new_cancel_flag(),
            &new_pid_slot(),
        );
        assert_eq!(out, ValidationOutcome::TimedOut, "時間切れにならない");
        assert_eq!(code, 124);
        // 「10 分待つテスト」にしない — 時限は注入する。
        assert!(
            start.elapsed() < std::time::Duration::from_secs(20),
            "打ち切りに時間がかかりすぎ: {:?}",
            start.elapsed()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 既に止まっているなら起こさない() {
        // **起こす前に札を見る。** 見ないと、停止を頼まれた後に始まった
        // コマンドが「誰も知らない子」として走り出す (自分のプロセス
        // グループを持つので、アプリが終わっても死なない)。
        let dir = ws("exec-precancel");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("ran");
        let cancel = new_cancel_flag();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        // **痕跡の残るコマンドを使う。** 起こしてから札を見て畳んでも
        // 戻り値は同じ `Cancelled` になるので、戻り値だけでは
        // 「起こさなかった」ことを確かめられない。
        let touch = format!(": > {}", marker.display());
        let win = format!("type nul > {}", marker.display());
        let (head, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("cmd", vec!["/C", win.as_str()])
        } else {
            ("sh", vec!["-c", touch.as_str()])
        };
        let (code, out) = run_words(
            head,
            &args,
            &dir,
            std::time::Duration::from_secs(30),
            &cancel,
            &new_pid_slot(),
        );
        assert_eq!(out, ValidationOutcome::Cancelled, "止まっているのに起こした");
        assert_eq!(code, 130);
        assert!(
            !marker.exists(),
            "止まっているのにコマンドが走った: {}",
            marker.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 並びは落ちたところで止める() {
        // 落ちた後のコマンドを走らせても判定は変わらず、時間と資源を
        // 使うだけ。**打ち切りの決まりごとは実行器が持つ** (GUI の橋渡し
        // 層に置くと、テストから 1 度も通らない場所に判断が住む)。
        let dir = ws("exec-list-stop");
        std::fs::create_dir_all(&dir).unwrap();
        let cmds = [
            ValidationCommand::parse("rustc --zzz-not-a-real-flag").unwrap(),
            ValidationCommand::parse("cargo --version").unwrap(),
        ];
        let runs = run_validation_list(
            &cmds,
            // 承認済みとして渡す — 見たいのは**打ち切り方**であって、
            // 承認ゲートで落ちて走らなかった、では確かめたことにならない。
            &cmds,
            &dir,
            std::time::Duration::from_secs(60),
            &new_cancel_flag(),
            &new_pid_slot(),
        );
        assert_eq!(runs.len(), 1, "落ちたのに次を走らせた: {runs:?}");
        assert!(!runs[0].ok());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 止まっているなら並びを一本も走らせない() {
        // 停止のあとに始まったコマンドは「誰も知らない子」になる
        // (自分のプロセスグループを持つので、アプリが終わっても死なない)。
        let dir = ws("exec-list-cancel");
        std::fs::create_dir_all(&dir).unwrap();
        let cancel = new_cancel_flag();
        cancel.store(true, std::sync::atomic::Ordering::Relaxed);
        let cmds = [
            ValidationCommand::parse("cargo --version").unwrap(),
            ValidationCommand::parse("cargo --version").unwrap(),
        ];
        let runs = run_validation_list(
            &cmds,
            &cmds,
            &dir,
            std::time::Duration::from_secs(60),
            &cancel,
            &new_pid_slot(),
        );
        assert_eq!(runs.len(), 1, "止まっているのに何本も走らせた: {runs:?}");
        assert_eq!(runs[0].outcome(), ValidationOutcome::Cancelled);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 停止の札を立てると打ち切る() {
        let dir = ws("exec-cancel");
        std::fs::create_dir_all(&dir).unwrap();
        let (head, args) = forever();
        let cancel = new_cancel_flag();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(200));
            c2.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let start = std::time::Instant::now();
        let (code, out) = run_words(
            head,
            &args,
            &dir,
            std::time::Duration::from_secs(60),
            &cancel,
            &new_pid_slot(),
        );
        assert_eq!(out, ValidationOutcome::Cancelled);
        assert_eq!(code, 130);
        assert!(start.elapsed() < std::time::Duration::from_secs(20));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **孫まで落ちることを、痕跡で確かめる。**
    ///
    /// 直接の子だけを殺すと孫が残り、「スレッドは畳んだが `cargo test` が
    /// 走り続けている」になる。孫に「1 秒後にファイルを作る」仕事をさせて、
    /// 打ち切った後もそのファイルが**現れないこと**を見る。
    #[cfg(unix)]
    #[test]
    fn 打ち切ると孫プロセスも残らない() {
        let dir = ws("exec-tree");
        std::fs::create_dir_all(&dir).unwrap();
        let marker = dir.join("grandchild-was-alive");
        let script = format!(
            "sh -c 'sleep 1; : > {}' & sleep 60",
            marker.display()
        );
        let cancel = new_cancel_flag();
        let c2 = cancel.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            c2.store(true, std::sync::atomic::Ordering::Relaxed);
        });
        let (_, out) = run_words(
            "sh",
            &["-c", &script],
            &dir,
            std::time::Duration::from_secs(60),
            &cancel,
            &new_pid_slot(),
        );
        assert_eq!(out, ValidationOutcome::Cancelled);
        // 孫が生きていれば、この間にファイルを作る。
        std::thread::sleep(std::time::Duration::from_millis(1800));
        assert!(
            !marker.exists(),
            "孫プロセスが生き残ってファイルを作った: {}",
            marker.display()
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    /// workspace の中に偽の実行体を置き、PATH の先頭に差し込む。
    #[cfg(unix)]
    fn plant_fake(ws: &Path, name: &str, marker: &Path) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let bin = ws.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let p = bin.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n: > {}\nexit 0\n", marker.display())).unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    /// **workspace 内の偽物を、名前を偽装しても実行しない。**
    ///
    /// `PATH=<workspace>/bin:$PATH` に `rustfmt` を置くだけで、
    /// 「読むだけ」と判定されたコマンドが攻撃者の実行体になる。
    #[cfg(unix)]
    #[test]
    fn workspace内の偽の実行体は自動実行しない() {
        let ws = ws("hijack-exec");
        std::fs::create_dir_all(&ws).unwrap();
        let marker = ws.join("hijacked");
        let orig = std::env::var("PATH").unwrap_or_default();
        for name in ["rustfmt", "black", "ruff", "shellcheck"] {
            plant_fake(&ws, name, &marker);
        }
        // このテストだけ PATH を差し替える。**プロセス共通なので、
        // 実際に PATH を変える代わりに解決関数へ直接渡す**
        // (環境変数の差し替えは並列に走る他のテストへ漏れる)。
        let path = format!("{}:{}", ws.join("bin").display(), orig);
        for name in ["rustfmt", "black", "ruff", "shellcheck"] {
            let got = super::super::validation_command::resolve_in(name, &ws, Some(&path), None);
            assert!(
                matches!(
                    got,
                    Err(super::super::validation_command::ResolveError::Untrusted { .. })
                ),
                "{name} が workspace 内の偽物へ解決された: {got:?}"
            );
        }
        // **実行器も、偽物の PATH を渡されても起こさない。**
        let r = run_validation_command_in(
            &ValidationCommand::parse("rustfmt --check src/a.rs").unwrap(),
            &[],
            &ws,
            std::time::Duration::from_secs(10),
            &new_cancel_flag(),
            &new_pid_slot(),
            Some(&path),
            None,
        );
        // **起動できなかった**こと自体が結果。実体を解決せずに OS へ
        // 名前を渡していたら、本物の `rustfmt` が動いて `Failed` になる
        // (この違いが「解決している / していない」の分かれ目)。
        assert_eq!(
            r.outcome(),
            ValidationOutcome::SpawnFailed,
            "実体を解決せずに起こした: {r:?}"
        );
        assert!(
            !marker.exists(),
            "workspace 内の偽の実行体が動いた: {}",
            marker.display()
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **読むだけと判定した検証は、workspace を 1 バイトも変えない。**
    #[cfg(unix)]
    #[test]
    fn 読むだけの検証はworkspaceを変えない() {
        let ws = ws("readonly-invariant");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::write(ws.join("a.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(ws.join("b.txt"), "hello\n").unwrap();
        // **道具に仕事をさせる。** 整形の対象が 1 つも無い実験場では
        // 書き換える版でも書き換えないので、この検査は空回りする
        // (前の版は `.py` も `.rs` も 1 つも無いまま緑だった)。
        std::fs::write(ws.join("a.py"), "x=1\ny  =  2\n").unwrap();
        std::fs::write(ws.join("b.py"), "import os,sys\nz   =3\n").unwrap();
        std::fs::write(ws.join("a.rs"), "fn main(){let  x=1;}\n").unwrap();
        let before = snapshot_dir(&ws);
        // 許可リストにあり、この環境に実在しうる読むだけのもの。
        // 実体が無ければ `SpawnFailed` になるが、**どちらでも workspace は
        // 変わらない**ことが見たいもの。
        for line in [
            "shellcheck a.sh",
            "black --check .",
            "black --diff .",
            "ruff check .",
            "ruff format --check .",
            "rustfmt --check a.rs",
        ] {
            let cmd = ValidationCommand::parse(line).unwrap();
            assert_eq!(
                super::super::graph::classify(&cmd),
                super::super::graph::ValidationRisk::ReadOnly,
                "{line} が読むだけになっていない"
            );
            let _ = run_validation_command(
                &cmd,
                &[],
                &ws,
                std::time::Duration::from_secs(30),
                &new_cancel_flag(),
                &new_pid_slot(),
            );
        }
        assert_eq!(
            snapshot_dir(&ws),
            before,
            "読むだけの検証が workspace を変えた"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// ディレクトリの中身 (相対パスと内容) を並べたもの。
    ///
    /// **道具が作る隠しキャッシュは数えない。** `ruff check .` は
    /// `.ruff_cache/` を、mypy は `.mypy_cache/` を作る。ここで守るのは
    /// 「**人のファイルを書き換えない**」であって「1 バイトも増えない」
    /// ではない (増えないことにするには `--no-cache` を強制するしかなく、
    /// それは人が書いたコマンドを勝手に変えることになる)。
    fn snapshot_dir(root: &Path) -> Vec<(String, Vec<u8>)> {
        fn walk(dir: &Path, root: &Path, out: &mut Vec<(String, Vec<u8>)>) {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return;
            };
            for e in rd.flatten() {
                let p = e.path();
                if p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with('.'))
                {
                    continue;
                }
                if p.is_dir() {
                    walk(&p, root, out);
                } else if let Ok(b) = std::fs::read(&p) {
                    let rel = p
                        .strip_prefix(root)
                        .unwrap_or(&p)
                        .display()
                        .to_string();
                    out.push((rel, b));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    #[test]
    fn 承認の証跡がなければ実行器が断る() {
        // **ゲートを 1 か所にしか置かないと、そこを通らない経路が
        // 何の抵抗もなく走る。** 実行の直前でもう一度、危険度と承認を見る。
        let dir = ws("exec-unapproved");
        std::fs::create_dir_all(&dir).unwrap();
        let cmd = ValidationCommand::parse("cargo --version").unwrap();
        assert!(
            super::super::graph::classify(&cmd).needs_approval(),
            "前提: 承認が要るコマンド"
        );
        let t = std::time::Duration::from_secs(60);
        let r = run_validation_command(&cmd, &[], &dir, t, &new_cancel_flag(), &new_pid_slot());
        assert_eq!(r.exit_code, 126, "承認なしで走らせた: {r:?}");
        assert_eq!(r.outcome(), ValidationOutcome::SpawnFailed);
        assert!(r.command.contains("承認"), "断った理由が残っていない: {r:?}");
        // 承認済みとして渡せば走る (断り方が「常に断る」になっていない)。
        let ok = run_validation_command(
            &cmd,
            std::slice::from_ref(&cmd),
            &dir,
            t,
            &new_cancel_flag(),
            &new_pid_slot(),
        );
        assert_eq!(ok.outcome(), ValidationOutcome::Passed, "{ok:?}");
        // **別のコマンドの承認を流用させない。**
        let other = ValidationCommand::parse("cargo --locked --version").unwrap();
        let r2 = run_validation_command(
            &cmd,
            std::slice::from_ref(&other),
            &dir,
            t,
            &new_cancel_flag(),
            &new_pid_slot(),
        );
        assert_eq!(r2.exit_code, 126, "他のコマンドの承認で走らせた: {r2:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 許可されていないコマンドは実行しない() {
        let dir = ws("exec");
        std::fs::create_dir_all(&dir).unwrap();
        let r = run_validation_command(
            &ValidationCommand::parse("rm -rf /").unwrap(),
            &[],
            &dir,
            std::time::Duration::from_secs(5),
            &new_cancel_flag(),
            &new_pid_slot(),
        );
        assert_eq!(r.exit_code, 126, "危険なコマンドを実行してしまった");
        assert_eq!(r.outcome(), ValidationOutcome::SpawnFailed);
        std::fs::remove_dir_all(&dir).ok();
    }
}
