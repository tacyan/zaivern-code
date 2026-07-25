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
