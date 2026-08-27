//! 手元の機械で走らせる Transport。
//!
//! **「ローカルも実行先の 1 つ」にしておくのが要**。特別扱いすると、
//! 上位の層に `if local { … } else { … }` が生えて、そこがすべての分岐の
//! 置き場になる。ここが [`ExecutionTransport`] を満たしていれば、
//! Scheduler も Runner も CLI も**ローカルとリモートを区別しない**。

use std::path::Path;
use std::time::{Duration, Instant};

use crate::features::cloud_execution::model::{
    CloudError, CollectSink, EventSink, ExecRequest, ExecResult, ExecutionTarget, ProbeResult,
    RemotePath, TargetEndpoint, TransportKind,
};

use super::{parse_probe_output, run_child, ExecutionTransport, PROBE_SCRIPT};

/// 手元で走らせる Transport。
pub struct LocalTransport {
    timeout: Duration,
}

impl LocalTransport {
    pub fn new(timeout: Duration) -> Self {
        Self { timeout }
    }

    fn check_local(target: &ExecutionTarget) -> Result<(), CloudError> {
        match target.endpoint {
            TargetEndpoint::Local => Ok(()),
            _ => Err(CloudError::unsupported(
                "この実行先はリモートなので、手元では動かせません",
            )),
        }
    }

    fn build(&self, request: &ExecRequest) -> Result<std::process::Command, CloudError> {
        if request.program.is_empty() {
            return Err(CloudError::config("実行するコマンドがありません"));
        }
        // **シェルを挟まない。** 引数はそのまま渡す (SSH 側と同じ約束)。
        let mut cmd = crate::procx::hidden_command(&request.program);
        cmd.args(&request.args);
        if let Some(cwd) = &request.cwd {
            cmd.current_dir(expand_home(cwd));
        }
        for (k, v) in &request.env {
            cmd.env(k, v);
        }
        Ok(cmd)
    }
}

/// `~` を手元の home へ広げる。**リモート用の [`RemotePath`] とは別物**で、
/// こちらはローカルのファイルシステムを指す。
fn expand_home(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::path::PathBuf::from(path)
}

impl ExecutionTransport for LocalTransport {
    fn kind(&self) -> TransportKind {
        TransportKind::Local
    }

    fn probe(&self, target: &ExecutionTarget) -> Result<ProbeResult, CloudError> {
        Self::check_local(target)?;
        let started = Instant::now();

        // **Windows には POSIX sh が無い。** 探りの script は sh 前提なので、
        // 手元が Windows のときは自分自身のことを直接答える
        // (自分の OS を「知らない」と言うのは、いちばん馬鹿げた失敗)。
        if cfg!(windows) {
            return Ok(ProbeResult {
                reachable: true,
                latency_ms: started.elapsed().as_millis() as u64,
                capabilities: local_capabilities(),
                shell: std::env::var("ComSpec").unwrap_or_else(|_| "cmd.exe".into()),
                kernel: String::new(),
                error: String::new(),
            });
        }

        let req = ExecRequest::new("sh", ["-c".to_string(), PROBE_SCRIPT.to_string()]);
        let mut sink = CollectSink::with_limit(64 * 1024);
        let cmd = self.build(&req)?;
        let r = run_child(cmd, self.timeout, "sh", &mut sink)?;
        let mut p = parse_probe_output(&sink.stdout_text());
        p.latency_ms = started.elapsed().as_millis() as u64;
        if !r.ok() {
            p.error = crate::features::cloud_execution::redact::redact(sink.stderr_text().trim());
        }
        // 手元のことは探りより確かに分かる欄がある (取り違えない)
        if p.capabilities.cpu_cores.is_none() {
            p.capabilities.cpu_cores = local_capabilities().cpu_cores;
        }
        Ok(p)
    }

    fn exec(
        &self,
        target: &ExecutionTarget,
        request: &ExecRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ExecResult, CloudError> {
        Self::check_local(target)?;
        let cmd = self.build(request)?;
        let timeout = request.timeout.unwrap_or(self.timeout);
        run_child(cmd, timeout, &request.program, sink)
    }

    /// 手元から手元への「送信」は複製。**上位が分岐しないために実装する。**
    fn upload(
        &self,
        target: &ExecutionTarget,
        source: &Path,
        destination: &RemotePath,
    ) -> Result<(), CloudError> {
        Self::check_local(target)?;
        copy_local(source, &expand_home(destination.as_str()))
    }

    fn download(
        &self,
        target: &ExecutionTarget,
        source: &RemotePath,
        destination: &Path,
    ) -> Result<(), CloudError> {
        Self::check_local(target)?;
        copy_local(&expand_home(source.as_str()), destination)
    }
}

fn copy_local(from: &Path, to: &Path) -> Result<(), CloudError> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)
            .map_err(|e| CloudError::io(format!("{} を作れません: {e}", dir.display())))?;
    }
    std::fs::copy(from, to).map_err(|e| {
        CloudError::io(format!(
            "{} を {} へ複製できません: {e}",
            from.display(),
            to.display()
        ))
    })?;
    Ok(())
}

/// 手元の能力 (OS / アーキテクチャ / 論理コア数)。
fn local_capabilities() -> crate::features::cloud_execution::model::Capabilities {
    crate::features::cloud_execution::model::Capabilities::host()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::model::{Capabilities, OsFamily};
    use crate::features::cloud_execution::test_support::{ssh_target, TargetOpts};

    fn transport() -> LocalTransport {
        LocalTransport::new(Duration::from_secs(30))
    }

    #[test]
    fn 手元で走って終了コードが返る() {
        let t = ExecutionTarget::local(1);
        let mut sink = CollectSink::default();
        let req = if cfg!(windows) {
            ExecRequest::new("cmd", vec!["/C".into(), "echo hi".into()])
        } else {
            ExecRequest::new("echo", vec!["hi".into()])
        };
        let r = transport().exec(&t, &req, &mut sink).expect("走る");
        assert!(r.ok(), "{r:?}");
        assert!(sink.stdout_text().contains("hi"));
    }

    #[test]
    fn 引数はシェルを通さない() {
        // シェルを挟んでいたら `;` が区切りとして効いてしまう
        let t = ExecutionTarget::local(1);
        let mut sink = CollectSink::default();
        let req = if cfg!(windows) {
            ExecRequest::new("cmd", vec!["/C".into(), "echo a;id".into()])
        } else {
            ExecRequest::new("echo", vec!["a;id".into()])
        };
        transport().exec(&t, &req, &mut sink).expect("走る");
        assert!(sink.stdout_text().contains("a;id"), "{}", sink.stdout_text());
        assert!(!sink.stdout_text().contains("uid="), "id が走ってしまった");
    }

    #[test]
    fn リモートの実行先は手元で動かさない() {
        let t = ssh_target("example.com", TargetOpts::default());
        let mut sink = CollectSink::default();
        let e = transport()
            .exec(&t, &ExecRequest::new("echo", vec![]), &mut sink)
            .expect_err("断る");
        assert!(matches!(e, CloudError::Unsupported(_)), "{e:?}");
    }

    #[test]
    fn 手元を探ると自分のosが分かる() {
        let t = ExecutionTarget::local(1);
        let p = transport().probe(&t).expect("探れる");
        assert!(p.reachable, "手元に届かないはずがない: {}", p.error);
        // **自分の OS を「知らない」と言わない**
        assert_eq!(p.capabilities.os, OsFamily::host());
        assert_ne!(p.capabilities.os, OsFamily::Unknown);
        assert!(p.capabilities.cpu_cores.unwrap_or(0) >= 1);
    }

    #[test]
    fn 手元の能力は自分のosとアーキテクチャ() {
        let c: Capabilities = local_capabilities();
        assert_eq!(c.os, OsFamily::host());
        assert_ne!(c.arch, crate::features::cloud_execution::model::Architecture::Unknown);
    }

    #[test]
    fn 手元どうしの送受信は複製になる() {
        let dir = crate::test_util::unique_temp_dir("zv-cloud", "local-copy");
        let src = dir.join("a.txt");
        std::fs::write(&src, b"hello").expect("書ける");
        let dst = dir.join("sub").join("b.txt");
        let t = ExecutionTarget::local(1);

        // [`RemotePath`] は POSIX の形しか受けない。Windows の一時パスは
        // `C:\…` なのでその形にならず、**断られるのが正しい挙動**である
        // (リモートへ `C:\…` を送ろうとするのを型で止めるための欄なので)。
        //
        // **`expect` を条件分岐より先に書かないこと。** 最初の版はそうなって
        // いて、`cfg!(windows)` で飛ばすつもりの行が飛ばす前に panic した
        // — macOS / Linux では永久に緑で、**Windows の CI だけが赤**になった。
        let Ok(remote) = RemotePath::new(dst.to_string_lossy().replace('\\', "/")) else {
            assert!(
                cfg!(windows),
                "POSIX のパスが断られた: {}",
                dst.display()
            );
            // Windows でも「断ること」は確かめる (空振りする試験にしない)
            assert!(RemotePath::new("C:/work/x.txt").is_err());
            assert!(RemotePath::new("C:\\work\\x.txt").is_err());
            return;
        };

        transport().upload(&t, &src, &remote).expect("送れる");
        assert_eq!(std::fs::read(&dst).expect("読める"), b"hello");

        // 受け取る側も同じ道を通る
        let back = dir.join("back.txt");
        transport().download(&t, &remote, &back).expect("受け取れる");
        assert_eq!(std::fs::read(&back).expect("読める"), b"hello");
    }
}
