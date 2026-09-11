//! 同梱の更新処理を Windows PowerShell へ UTF-8 の標準入力で渡す。
//! 公開ブランチのスクリプトが古くても、このバイナリの修正を使う。

use std::io::{self, Write};
use std::path::Path;
use std::process::{Command, ExitStatus, Output, Stdio};

fn command(exe: &Path) -> Command {
    let mut command = Command::new("powershell");
    command
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "$ErrorActionPreference = 'Stop'; \
             [Console]::InputEncoding = New-Object System.Text.UTF8Encoding; \
             [Console]::OutputEncoding = New-Object System.Text.UTF8Encoding; \
             & ([ScriptBlock]::Create([Console]::In.ReadToEnd()))",
        ])
        .stdin(Stdio::piped())
        // PowerShell 7 のモジュールパスを 5.1 へ引き継ぐと、Get-FileHash や
        // Expand-Archive を読み込めない。子の 5.1 に標準パスを組み立てさせる。
        .env_remove("PSModulePath")
        .env("ZAI_UPDATE_ONLY", "1");
    if let Some(dir) = exe.parent() {
        command.env("ZAI_INSTALL_DIR", crate::pathx::plain(dir.to_path_buf()));
    }
    command
}

fn execute(mut command: Command, script: &str) -> io::Result<Output> {
    let mut child = command.spawn()?;
    let write_result = child
        .stdin
        .take()
        .expect("installer stdin is piped")
        .write_all(script.trim_start_matches('\u{feff}').as_bytes());
    // 標準入力を閉じてから待つ。書き込み失敗時も子を回収する。
    let output = child.wait_with_output()?;
    write_result?;
    Ok(output)
}

pub(crate) fn run(exe: &Path, script: &str) -> io::Result<ExitStatus> {
    execute(command(exe), script).map(|output| output.status)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_preserves_unicode_destination_and_failure_status() {
        let destination = std::env::temp_dir().join("zai ' 日本語");
        let mut command = command(&destination.join("zai.exe"));
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let output = execute(
            command,
            "Write-Output $env:ZAI_INSTALL_DIR; throw '更新失敗'",
        )
        .expect("run Windows PowerShell");
        assert!(!output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap().trim(),
            destination.to_str().unwrap()
        );
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("更新失敗"));
    }

    #[test]
    fn successful_installer_exits_zero() {
        let output = execute(
            command(Path::new("zai.exe")),
            "Get-Command Get-FileHash,Expand-Archive -ErrorAction Stop | Out-Null; exit 0",
        )
        .expect("run Windows PowerShell with standard modules");
        assert!(output.status.success());
    }
}
