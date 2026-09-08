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
        if std::env::var("ZAI_DESCENDANT_EARLY_EXIT").as_deref() != Ok("1") {
            let _ = stream.read_exact(&mut [0]);
        }
        drop(child); // Deliberately leave the managed descendant alive.
    } else {
        #[cfg(unix)]
        unsafe {
            match std::env::var("ZAI_DESCENDANT_GROUP").as_deref() {
                Ok("setpgid") => assert_eq!(libc::setpgid(0, 0), 0),
                Ok("setsid") => assert!(libc::setsid() >= 0),
                _ => {}
            }
        }
        // This acknowledgement happens only after the requested group separation.
        stream.write_all(b"C").unwrap();
        let mut command = [0];
        while stream.read_exact(&mut command).is_ok() {
            std::fs::write(std::env::var("ZAI_DESCENDANT_FILE").unwrap(), "late writer").unwrap();
            stream.write_all(b"D").unwrap();
            if command[0] == b'x' {
                break;
            }
        }
    }
}

pub(crate) struct Descendant {
    pub session: Option<Session>,
    parent: TcpStream,
    child: TcpStream,
    pub tree: std::sync::Arc<crate::terminal::writer_tree::Tree>,
}
impl Descendant {
    pub fn spawn(id: u64, work: &Path, file: &str) -> Self {
        Self::spawn_group_with_env(id, work, file, "group", Default::default())
    }
    pub fn spawn_group(id: u64, work: &Path, file: &str, group: &str) -> Self {
        if group == "group" {
            Self::spawn(id, work, file)
        } else {
            Self::spawn_group_with_env(id, work, file, group, Default::default())
        }
    }
    pub fn spawn_group_with_env(
        id: u64,
        work: &Path,
        file: &str,
        group: &str,
        extra: std::collections::BTreeMap<String, String>,
    ) -> Self {
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
                env: {
                    let mut env: std::collections::HashMap<String, String> = [
                        ("ZAI_DESCENDANT_MODE".into(), "parent".into()),
                        ("ZAI_DESCENDANT_GROUP".into(), group.into()),
                        (
                            "ZAI_DESCENDANT_ADDR".into(),
                            listener.local_addr().unwrap().to_string(),
                        ),
                        (
                            "ZAI_DESCENDANT_FILE".into(),
                            work.join(file).to_string_lossy().into_owned(),
                        ),
                    ]
                    .into();
                    env.extend(extra);
                    env
                },
                log_path: None,
            },
            eframe::egui::Context::default(),
        )
        .unwrap();
        let tree = session.writer_identity().unwrap().tree.unwrap();
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
        self.wait_for_observation();
        self.write_child();
        assert!(
            !self.tree.finished(),
            "the controlled child is still waiting"
        );
    }
    fn wait_for_observation(&self) {
        #[cfg(unix)]
        {
            let epoch = self.tree.observation_epoch.load(Ordering::Acquire);
            wait_until(|| {
                self.tree.finished() || self.tree.observation_epoch.load(Ordering::Acquire) != epoch
            });
        }
    }
    pub fn write_child(&mut self) {
        self.child.write_all(b"w").unwrap();
        let mut acknowledgement = [0];
        self.child.read_exact(&mut acknowledgement).unwrap();
        assert_eq!(acknowledgement, *b"D");
    }
    pub fn finish_child(&mut self) {
        self.child.write_all(b"x").unwrap();
        self.child.read_exact(&mut [0]).unwrap();
        wait_until(|| self.tree.finished());
    }
}
impl Drop for Descendant {
    fn drop(&mut self) {
        // EOF also terminates a detached child when the product tracker is broken.
        // Do this before stopping the tree, including during assertion unwinding.
        let _ = self.parent.shutdown(std::net::Shutdown::Write);
        let _ = self.child.shutdown(std::net::Shutdown::Write);
        let _ = self.child.read_to_end(&mut Vec::new());
        self.tree.stop();
        if let Some(session) = self.session.take() {
            crate::terminal::reap(session);
        }
        if !std::thread::panicking() {
            wait_until(|| self.tree.finished());
        }
    }
}
pub(crate) fn wait_until(mut ready: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready() {
        assert!(Instant::now() < deadline, "writer completion timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(unix)]
fn separated_writer_case(group: &str, stop: bool) {
    let work = crate::test_util::unique_temp_dir("zai-descendant-group", group);
    std::fs::create_dir_all(&work).unwrap();
    static NEXT_SESSION: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(90_000_000);
    let id = NEXT_SESSION.fetch_add(1, Ordering::Relaxed);
    let mut writer = if group == "group" {
        Descendant::spawn(id, &work, "late.txt")
    } else {
        Descendant::spawn_group(id, &work, "late.txt", group)
    };
    writer.parent.write_all(b"x").unwrap();
    wait_until(|| {
        writer
            .session
            .as_ref()
            .unwrap()
            .exited
            .load(Ordering::Acquire)
    });
    writer.wait_for_observation();
    writer.write_child();
    assert_eq!(
        std::fs::read_to_string(work.join("late.txt")).unwrap(),
        "late writer"
    );
    assert!(
        !writer.tree.finished(),
        "{group}: child acknowledged a write after parent exit but tree was finished"
    );
    if stop {
        writer.session.as_mut().unwrap().kill();
        let handle = crate::terminal::reap_tracked(writer.session.take().unwrap());
        wait_until(|| handle.is_finished());
        let mut response = [0];
        assert_eq!(
            writer.child.read(&mut response).unwrap(),
            0,
            "Stop completed with a live writer socket"
        );
    } else {
        writer.finish_child();
        let handle = crate::terminal::reap_tracked(writer.session.take().unwrap());
        wait_until(|| handle.is_finished());
    }
    assert!(writer.tree.finished());
    drop(writer);
    std::fs::remove_dir_all(work).unwrap();
}

#[cfg(unix)]
#[test]
fn session_tracks_same_group_writer_after_parent_exit() {
    separated_writer_case("group", false);
}
#[cfg(unix)]
#[test]
fn session_tracks_setpgid_writer_after_parent_exit() {
    separated_writer_case("setpgid", false);
}
#[cfg(unix)]
#[test]
fn session_tracks_setsid_writer_after_parent_exit() {
    separated_writer_case("setsid", false);
}
#[cfg(unix)]
#[test]
fn session_stop_reaps_setpgid_writer() {
    separated_writer_case("setpgid", true);
}
#[cfg(unix)]
#[test]
fn session_stop_reaps_setsid_writer() {
    separated_writer_case("setsid", true);
}

#[cfg(unix)]
fn early_orphan_case(group: &str) {
    let work = crate::test_util::unique_temp_dir("zai-descendant-early", group);
    std::fs::create_dir_all(&work).unwrap();
    static NEXT_SESSION: std::sync::atomic::AtomicU64 =
        std::sync::atomic::AtomicU64::new(90_100_000);
    let mut writer = Descendant::spawn_group_with_env(
        NEXT_SESSION.fetch_add(1, Ordering::Relaxed),
        &work,
        "late.txt",
        group,
        [("ZAI_DESCENDANT_EARLY_EXIT".into(), "1".into())].into(),
    );
    wait_until(|| {
        writer
            .session
            .as_ref()
            .unwrap()
            .exited
            .load(Ordering::Acquire)
    });
    writer.wait_for_observation();
    writer.write_child();
    assert_eq!(
        std::fs::read_to_string(work.join("late.txt")).unwrap(),
        "late writer"
    );
    assert!(
        !writer.tree.finished(),
        "an orphaned writer was lost during admission"
    );
    writer.finish_child();
    let handle = crate::terminal::reap_tracked(writer.session.take().unwrap());
    wait_until(|| handle.is_finished());
    drop(writer);
    std::fs::remove_dir_all(work).unwrap();
}

#[cfg(unix)]
#[test]
fn session_tracks_setpgid_child_when_parent_exits_before_handshake() {
    early_orphan_case("setpgid");
}
#[cfg(unix)]
#[test]
fn session_tracks_setsid_child_when_parent_exits_before_handshake() {
    early_orphan_case("setsid");
}
