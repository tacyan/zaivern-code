//! **どうやってコマンドを届けるか**だけを担う層 (§12)。
//!
//! Provider と混ぜない。Provider は「実行先をどこから持ってくるか」だけを
//! 担い、Transport は「そこでどう走らせるか」だけを担う。分けてあるから、
//! Hetzner で作った VM も自宅の Linux も**同じ [`SshTransport`]** で動く
//! (Provider を 1 つ足しても Transport は 1 行も変わらない)。
//!
//! ## 子プロセスを待つときの約束 (§48)
//!
//! * **永久待ちを作らない。** どの実行にも上限を当てる。
//! * **上限に当たったらプロセス**ツリー**ごと止める。** 直接の子だけを殺すと、
//!   シェルが `exec` せずに起こした孫がパイプを握ったまま残り、読み取りの
//!   join が孫の寿命まで戻らない (= 呼び出し側が固まる)。
//!   後始末は既存の [`crate::procx::kill_tree`] を使う。
//! * **出力を溜めない。** 読んだ端から [`EventSink`] へ流す (設計原則 2)。

pub mod local;
pub mod ssh;

use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use super::model::{
    CloudError, EventSink, ExecRequest, ExecResult, ExecutionTarget, ProbeResult, RemotePath,
    TransportKind,
};

pub use local::LocalTransport;
pub use ssh::SshTransport;

/// 実行先へコマンドを届ける面。
pub trait ExecutionTransport: Send + Sync {
    fn kind(&self) -> TransportKind;

    /// 届くか、そこが何者かを確かめる。
    fn probe(&self, target: &ExecutionTarget) -> Result<ProbeResult, CloudError>;

    /// 1 回実行する。出力は溜めずに `sink` へ流す。
    fn exec(
        &self,
        target: &ExecutionTarget,
        request: &ExecRequest,
        sink: &mut dyn EventSink,
    ) -> Result<ExecResult, CloudError>;

    fn upload(
        &self,
        target: &ExecutionTarget,
        source: &Path,
        destination: &RemotePath,
    ) -> Result<(), CloudError>;

    fn download(
        &self,
        target: &ExecutionTarget,
        source: &RemotePath,
        destination: &Path,
    ) -> Result<(), CloudError>;
}

/// 実行先に合う Transport を返す。**ここが Provider 名で分岐しない唯一の理由**
/// — 見ているのは [`ExecutionTarget::transport`] だけで、その値を誰が作ったか
/// (Hetzner か静的 SSH か) は 1 バイトも見ていない。
pub fn for_target(
    target: &ExecutionTarget,
    timeout: Duration,
) -> Box<dyn ExecutionTransport> {
    match target.transport {
        TransportKind::Local => Box::new(LocalTransport::new(timeout)),
        TransportKind::Ssh => Box::new(SshTransport::new(timeout)),
    }
}

/// 子プロセスの出力の断片。**読み手のスレッドから本体へ渡す**ための箱。
enum Chunk {
    Out(Vec<u8>),
    Err(Vec<u8>),
    OutEof,
    ErrEof,
}

/// 子プロセスを走らせ、出力を流しながら上限つきで待つ。
///
/// `label` は失敗のメッセージに出る短い名前 (`ssh` / `git` 等)。
pub(crate) fn run_child(
    mut cmd: Command,
    timeout: Duration,
    label: &str,
    sink: &mut dyn EventSink,
) -> Result<ExecResult, CloudError> {
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let started = Instant::now();
    let mut child = cmd.spawn().map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CloudError::config(format!(
                "{label} が見つかりません。導入してから再実行してください"
            ))
        } else {
            CloudError::io(format!("{label} を起動できません: {e}"))
        }
    })?;

    let (tx, rx) = mpsc::channel::<Chunk>();
    spawn_reader(child.stdout.take(), tx.clone(), true);
    spawn_reader(child.stderr.take(), tx.clone(), false);
    // **本体の分を落としておく。** 残しておくと、読み手が両方終わっても
    // チャネルが閉じず、`recv_timeout` が上限まで空回りする。
    drop(tx);

    let mut open = 2;
    loop {
        if started.elapsed() >= timeout {
            return Err(kill_and_timeout(child, label, timeout));
        }
        // 上限までの残りと、様子を見る間隔の短いほう
        let slice = timeout
            .saturating_sub(started.elapsed())
            .min(Duration::from_millis(50));
        match rx.recv_timeout(slice) {
            Ok(Chunk::Out(b)) => sink.on_stdout(&b),
            Ok(Chunk::Err(b)) => sink.on_stderr(&b),
            Ok(Chunk::OutEof) | Ok(Chunk::ErrEof) => {
                open -= 1;
                if open == 0 {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            // 両方の読み手が落ちた (相手が消えた)。終了状態の確認へ進む。
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // 出力が閉じたあと、終了するまでを待つ。ここにも上限を当てる。
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let duration_ms = started.elapsed().as_millis() as u64;
                return Ok(ExecResult {
                    exit_code: status.code(),
                    duration_ms,
                })
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    return Err(kill_and_timeout(child, label, timeout));
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => return Err(CloudError::io(format!("{label} の終了を待てません: {e}"))),
        }
    }
}

fn kill_and_timeout(child: Child, label: &str, timeout: Duration) -> CloudError {
    // **ツリーごと止める。** 直接の子だけでは孫がパイプを握ったまま残る。
    crate::procx::kill_tree(child.id());
    let mut child = child;
    let _ = child.kill();
    let _ = child.wait();
    CloudError::timeout(format!(
        "{label} が {} 秒で終わりませんでした",
        timeout.as_secs()
    ))
}

fn spawn_reader<R: std::io::Read + Send + 'static>(
    stream: Option<R>,
    tx: mpsc::Sender<Chunk>,
    is_stdout: bool,
) {
    let Some(mut stream) = stream else {
        let _ = tx.send(if is_stdout { Chunk::OutEof } else { Chunk::ErrEof });
        return;
    };
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = buf[..n].to_vec();
                    let sent = tx.send(if is_stdout {
                        Chunk::Out(chunk)
                    } else {
                        Chunk::Err(chunk)
                    });
                    if sent.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = tx.send(if is_stdout { Chunk::OutEof } else { Chunk::ErrEof });
    });
}

/// リモートから返る 1 行 1 組の `key=value` を能力へ写す。
///
/// **Transport をまたいで同じ形**にしてある — ローカルも SSH も同じ問い合わせを
/// 出し、同じ関数で読む。2 か所に書くとずれる。
pub(crate) fn parse_probe_output(text: &str) -> ProbeResult {
    use super::model::{Architecture, Capabilities, OsFamily};
    let mut os = String::new();
    let mut arch = String::new();
    let mut cores: Option<u16> = None;
    let mut mem_mib: Option<u64> = None;
    let mut disk_mib: Option<u64> = None;
    let mut shell = String::new();
    let mut kernel = String::new();
    let mut tools = std::collections::BTreeSet::new();

    for line in text.replace("\r\n", "\n").lines() {
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        let v = v.trim();
        if v.is_empty() {
            continue;
        }
        match k.trim() {
            "zv_os" => os = v.to_string(),
            "zv_arch" => arch = v.to_string(),
            "zv_cores" => cores = v.parse().ok(),
            // Linux の /proc/meminfo は kB、macOS の hw.memsize はバイト。
            // **単位を送る側で MiB へ揃える** (受け側で当てると必ず外す)。
            "zv_mem_mib" => mem_mib = v.parse().ok(),
            "zv_disk_mib" => disk_mib = v.parse().ok(),
            "zv_shell" => shell = v.to_string(),
            "zv_kernel" => kernel = v.to_string(),
            "zv_tool" => {
                tools.insert(v.to_string());
            }
            _ => {}
        }
    }

    ProbeResult {
        reachable: !os.is_empty(),
        latency_ms: 0,
        capabilities: Capabilities {
            os: OsFamily::from_uname(&os),
            arch: Architecture::from_uname(&arch),
            cpu_cores: cores,
            memory_mib: mem_mib,
            disk_mib,
            tools,
            ..Capabilities::default()
        },
        shell,
        kernel,
        error: String::new(),
    }
}

/// 実行先を確かめるために走らせる POSIX sh の断片。
///
/// **これは定数であって、利用者の入力は 1 バイトも混ざらない。**
/// 混ぜたくなったら、混ぜずに済む形 (引数で渡す) を先に考えること。
///
/// 単位は送り出す側で MiB へ揃える。`/proc/meminfo` (Linux, kB) と
/// `hw.memsize` (macOS, バイト) の両方を見る。
pub(crate) const PROBE_SCRIPT: &str = r#"
printf 'zv_os=%s\n' "$(uname -s 2>/dev/null)"
printf 'zv_arch=%s\n' "$(uname -m 2>/dev/null)"
printf 'zv_kernel=%s\n' "$(uname -r 2>/dev/null)"
printf 'zv_shell=%s\n' "${SHELL:-}"
c=$(getconf _NPROCESSORS_ONLN 2>/dev/null || nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null)
[ -n "$c" ] && printf 'zv_cores=%s\n' "$c"
m=$(awk '/^MemTotal:/ {printf "%d", $2/1024; exit}' /proc/meminfo 2>/dev/null)
[ -z "$m" ] && m=$(sysctl -n hw.memsize 2>/dev/null | awk '{printf "%d", $1/1048576}')
[ -n "$m" ] && printf 'zv_mem_mib=%s\n' "$m"
d=$(df -Pk "$HOME" 2>/dev/null | awk 'NR==2 {printf "%d", $2/1024}')
[ -n "$d" ] && printf 'zv_disk_mib=%s\n' "$d"
for t in git tar rsync docker; do
  command -v "$t" >/dev/null 2>&1 && printf 'zv_tool=%s\n' "$t"
done
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::model::{Architecture, CollectSink, OsFamily};

    #[test]
    fn 探りの出力を能力へ写す() {
        let text = "zv_os=Linux\nzv_arch=x86_64\nzv_cores=8\nzv_mem_mib=15900\n\
                    zv_disk_mib=76000\nzv_shell=/bin/bash\nzv_kernel=6.8.0\n\
                    zv_tool=git\nzv_tool=tar\n";
        let p = parse_probe_output(text);
        assert!(p.reachable);
        assert_eq!(p.capabilities.os, OsFamily::Linux);
        assert_eq!(p.capabilities.arch, Architecture::X86_64);
        assert_eq!(p.capabilities.cpu_cores, Some(8));
        assert_eq!(p.capabilities.memory_mib, Some(15900));
        assert!(p.capabilities.tools.contains("git"));
        assert_eq!(p.shell, "/bin/bash");
    }

    #[test]
    fn 探りの出力はcrlfでも読める() {
        // Windows 由来のシェルから返ることがある
        let p = parse_probe_output("zv_os=Linux\r\nzv_arch=aarch64\r\n");
        assert_eq!(p.capabilities.arch, Architecture::Aarch64);
    }

    #[test]
    fn 何も返らなければ届いていないと見なす() {
        let p = parse_probe_output("");
        assert!(!p.reachable);
        let p = parse_probe_output("bash: uname: command not found\n");
        assert!(!p.reachable, "関係の無い出力を成功と読まない");
    }

    /// **上限に当たったら止まること**を、実際に止まらないコマンドで確かめる。
    #[test]
    fn 終わらない子は上限で止める() {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "ping -n 60 127.0.0.1 >NUL"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "sleep 60"]);
            c
        };
        cmd.env("LC_ALL", "C");
        let mut sink = CollectSink::default();
        let started = Instant::now();
        let e = run_child(cmd, Duration::from_millis(400), "test", &mut sink)
            .expect_err("上限で止まる");
        assert!(matches!(e, CloudError::Timeout(_)), "{e:?}");
        // 上限のすぐ後に戻ること (60 秒待たされていない)
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "戻るのに {:?} かかった",
            started.elapsed()
        );
    }

    #[test]
    fn 出力は溜めずに流れて終了コードが返る() {
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", "echo hello& exit 3"]);
            c
        } else {
            let mut c = Command::new("sh");
            c.args(["-c", "echo hello; exit 3"]);
            c
        };
        cmd.env("LC_ALL", "C");
        let mut sink = CollectSink::default();
        let r = run_child(cmd, Duration::from_secs(30), "test", &mut sink).expect("走る");
        assert_eq!(r.exit_code, Some(3));
        assert!(sink.stdout_text().contains("hello"), "{}", sink.stdout_text());
    }

    #[test]
    fn 無い道具は設定の誤りとして返る() {
        let cmd = Command::new("zaivern-no-such-program-xyz");
        let mut sink = CollectSink::default();
        let e = run_child(cmd, Duration::from_secs(5), "zaivern-no-such-program-xyz", &mut sink)
            .expect_err("失敗する");
        // 「実行時エラー」ではなく「設定の誤り」= 終了コード 3
        assert_eq!(e.exit_code(), 3, "{e:?}");
    }
}
