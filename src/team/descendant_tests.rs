use crate::terminal::{Session, SpawnSpec};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

#[test]
fn descendant_writer_probe() {
    use std::io::{Read, Write};
    let Ok(mode) = std::env::var("ZAI_DESCENDANT_MODE") else {
        return;
    };
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGHUP, libc::SIG_IGN);
    }
    let mut stream =
        std::net::TcpStream::connect(std::env::var("ZAI_DESCENDANT_ADDR").unwrap()).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(15)))
        .unwrap();
    if mode == "parent" {
        let probe = format!(
            "{}::descendant_writer_probe",
            module_path!().split_once("::").unwrap().1
        );
        let child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", &probe, "--nocapture"])
            .env("ZAI_DESCENDANT_MODE", "child")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap();
        stream.write_all(b"P").unwrap();
        let _ = stream.read_exact(&mut [0]);
        drop(child); // Deliberately leave the managed descendant alive.
    } else {
        stream.write_all(b"C").unwrap();
        if stream.read_exact(&mut [0]).is_ok() {
            std::fs::write(std::env::var("ZAI_DESCENDANT_FILE").unwrap(), "late writer").unwrap();
            stream.write_all(b"D").unwrap();
        }
    }
}

pub(super) struct Descendant {
    pub session: Option<Session>,
    parent: TcpStream,
    child: TcpStream,
    pub tree: std::sync::Arc<crate::terminal::writer_tree::Tree>,
}
impl Descendant {
    pub fn spawn(id: u64, work: &Path, file: &str) -> Self {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let probe = format!(
            "{}::descendant_writer_probe",
            module_path!().split_once("::").unwrap().1
        );
        let exe = std::env::current_exe().unwrap();
        #[cfg(unix)]
        let command = format!(
            "exec '{}' --exact {probe} --nocapture",
            exe.to_string_lossy().replace('\'', "'\"'\"'")
        );
        #[cfg(windows)]
        let command = format!("\"{}\" --exact {probe} --nocapture", exe.display());
        let session = Session::spawn(
            id,
            SpawnSpec {
                title: "descendant".into(),
                preset_name: "probe".into(),
                icon: String::new(),
                command,
                cwd: work.to_owned(),
                env: [
                    ("ZAI_DESCENDANT_MODE".into(), "parent".into()),
                    (
                        "ZAI_DESCENDANT_ADDR".into(),
                        listener.local_addr().unwrap().to_string(),
                    ),
                    (
                        "ZAI_DESCENDANT_FILE".into(),
                        work.join(file).to_string_lossy().into_owned(),
                    ),
                ]
                .into(),
                log_path: None,
            },
            eframe::egui::Context::default(),
        )
        .unwrap();
        let tree =
            crate::terminal::writer_tree::Tree::lookup(session.live_process_id().unwrap()).unwrap();
        let mut streams = std::collections::BTreeMap::new();
        let deadline = Instant::now() + Duration::from_secs(10);
        while streams.len() < 2 {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).unwrap();
                    stream
                        .set_read_timeout(Some(Duration::from_secs(10)))
                        .unwrap();
                    let mut kind = [0];
                    stream.read_exact(&mut kind).unwrap();
                    streams.insert(kind[0], stream);
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "descendant handshake timed out");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => panic!("{e}"),
            }
        }
        Self {
            session: Some(session),
            tree,
            parent: streams.remove(&b'P').unwrap(),
            child: streams.remove(&b'C').unwrap(),
        }
    }
    pub fn exit_parent(&mut self) {
        self.parent.write_all(b"x").unwrap();
        wait_until(|| {
            self.session
                .as_ref()
                .unwrap()
                .exited
                .load(Ordering::Acquire)
        });
        assert!(
            !self.tree.finished(),
            "the controlled child is still waiting"
        );
    }
    pub fn finish_child(&mut self) {
        self.child.write_all(b"x").unwrap();
        self.child.read_exact(&mut [0]).unwrap();
        wait_until(|| self.tree.finished());
    }
}
impl Drop for Descendant {
    fn drop(&mut self) {
        self.tree.stop();
        if let Some(session) = self.session.take() {
            crate::terminal::reap(session);
        }
        if !std::thread::panicking() {
            wait_until(|| self.tree.finished());
        }
    }
}
pub(super) fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "writer completion timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}
