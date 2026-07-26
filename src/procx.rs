//! 外部コマンド起動の共通ヘルパー。
//!
//! Windows では本アプリが GUI サブシステム (`windows_subsystem = "windows"`)
//! でビルドされるため、`git` / `gh` / `powershell` などのコンソールアプリを
//! そのまま spawn すると **毎回新しいコンソール窓が開いて点滅する**。
//! (git status のポーリングは 2 秒毎なので、放置すると窓が延々と湧き続ける)
//!
//! ここで `CREATE_NO_WINDOW` を一括で付けた [`std::process::Command`] を作り、
//! アプリ内の外部コマンド実行はすべてこのヘルパーを経由させる。
//! macOS / Linux では単なる `Command::new` と同じ。
//!
//! 注意: ユーザーに見せる目的のプロセス (自分自身の新ウィンドウ起動や
//! ブラウザ起動など GUI アプリ) には不要だが、付けても無害。
//!
//! あわせて子プロセスの `PATH` を [`crate::shellenv::user_path`] へ差し替える。
//! GUI アプリ (macOS の `.app` / Windows のショートカット) はユーザーのシェルの
//! PATH を継承しないため、素の `Command` では `gh` も `claude` も見つからない。

use std::ffi::OsStr;
use std::process::Command;

/// コンソール窓を出さず、ユーザーの PATH が通った `Command` を作る。
/// 以後は普通の `Command` として `arg` / `output` / `spawn` すればよい。
pub fn hidden_command(program: impl AsRef<OsStr>) -> Command {
    let mut c = hidden_command_raw(program);
    crate::shellenv::apply_path(&mut c);
    c
}

/// PATH を差し替えない版。**PATH の解決そのもの** (`shellenv`) から使う。
/// 通常のコマンド実行では [`hidden_command`] を使うこと。
pub fn hidden_command_raw(program: impl AsRef<OsStr>) -> Command {
    #[allow(unused_mut)]
    let mut c = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        /// <https://learn.microsoft.com/windows/win32/procthread/process-creation-flags>
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        c.creation_flags(CREATE_NO_WINDOW);
    }
    c
}

/// プロセス**ツリー**を丸ごと落とす (孫まで)。呼び出し側を長く待たせない
/// (unix の猶予は最大 ~80ms、消え次第すぐ返る)。
///
/// **呼び出し側の責務**: 子がまだ生きている (wait 済みでない) ことを確認して
/// から呼ぶこと。wait 済みの PID は OS に返却されており、無関係なプロセス
/// (グループ) に再利用され得る — そこへ撃つとユーザーの別ジョブを巻き添えに
/// する (terminal.rs の「終了済みセッションへ kill を撃たない」ガードと同じ理由)。
///
/// - unix: portable-pty は子を setsid するので pgid == pid。まず SIGHUP で
///   シェルに後始末の機会を与え、短い猶予の後 SIGKILL をグループへ送る。
///   ログインシェルの子 (`bash -lc '…; sleep N'`) や孫も同じグループにいる
///   (非対話シェルはジョブ制御が無く、バックグラウンドジョブも同グループ)。
/// - Windows: `taskkill /T /F` が木を辿って孫まで落とす (コンソール窓は出さない)。
pub fn kill_tree(pid: u32) {
    #[cfg(unix)]
    {
        let pgid = pid as libc::pid_t;
        // SIGHUP: 行儀のよい終了の機会。既にグループごと消えていれば (ESRCH)
        // ここで即返る — 撃つ相手がいない。
        if unsafe { libc::killpg(pgid, libc::SIGHUP) } != 0 {
            return;
        }
        // 猶予: 最大 ~80ms。グループが消えたら待たずに抜ける。
        for _ in 0..8 {
            std::thread::sleep(std::time::Duration::from_millis(10));
            if unsafe { libc::killpg(pgid, 0) } != 0 {
                return;
            }
        }
        // HUP を無視した生き残りへ、グループごと強制終了。
        unsafe { libc::killpg(pgid, libc::SIGKILL) };
    }
    #[cfg(windows)]
    {
        // /T = 子孫ごと、/F = 強制。System32 にあるので PATH 解決は不要。
        // 終了報告はどこへも出さない (ターミナル起動時に出力が混ざるため)。
        let mut c = hidden_command_raw("taskkill");
        c.args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        if let Ok(mut child) = c.spawn() {
            let _ = child.wait();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hidden_command_runs_like_a_normal_command() {
        // どの OS にもある無害なコマンドで、普通に実行できることだけ確認する。
        #[cfg(windows)]
        let out = hidden_command("cmd").args(["/C", "echo hi"]).output();
        #[cfg(not(windows))]
        let out = hidden_command("echo").arg("hi").output();
        let out = out.expect("コマンドが起動できる");
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hi"));
    }
}
