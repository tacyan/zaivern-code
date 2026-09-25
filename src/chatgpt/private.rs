//! Private, bounded, link-resistant storage. Never adopt or chmod unsafe files.
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

pub(super) type Result<T> = std::result::Result<T, String>;

#[cfg(test)]
thread_local! {
    static METADATA_FAILURE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

fn metadata(path: &Path) -> std::io::Result<fs::Metadata> {
    #[cfg(test)]
    if METADATA_FAILURE.with(|slot| slot.borrow().as_deref() == Some(path)) {
        return Err(std::io::Error::from_raw_os_error(libc::EIO));
    }
    fs::symlink_metadata(path)
}

/// A link is present; only ENOENT proves absence. IO/permission failures cannot
/// authorize replacing a journal or treating an old generation as clean.
pub(super) fn exists_checked(path: &Path) -> Result<bool> {
    match metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(_) => Err("Cannot inspect managed state; existing evidence preserved".into()),
    }
}

#[cfg(test)]
pub(super) fn with_metadata_failure<T>(path: &Path, action: impl FnOnce() -> T) -> T {
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            METADATA_FAILURE.with(|slot| *slot.borrow_mut() = self.0.take());
        }
    }
    let previous = METADATA_FAILURE.with(|slot| slot.replace(Some(path.into())));
    let _restore = Restore(previous);
    action()
}

pub(super) fn directory(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err("ChatGPT state directory must be absolute".into());
    }
    if !exists_checked(path)? {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(path)
            .map_err(|_| "Cannot create private ChatGPT directory")?;
    }
    let m = fs::symlink_metadata(path).map_err(|_| "Cannot inspect ChatGPT directory")?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o077 != 0 {
        return Err("ChatGPT directory must be owned by you, mode 0700, and not a symlink".into());
    }
    Ok(())
}

fn validate(file: &File, executable: bool) -> Result<()> {
    let m = file.metadata().map_err(|_| "Cannot inspect private file")?;
    let forbidden = if executable { 0o022 } else { 0o077 };
    if !m.is_file()
        || m.nlink() != 1
        || m.uid() != unsafe { libc::geteuid() }
        || m.mode() & forbidden != 0
    {
        return Err(
            "Unsafe file: expected an owned regular file without links or shared write access"
                .into(),
        );
    }
    Ok(())
}

pub(super) fn open(path: &Path, executable: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .map_err(|_| "Cannot open private file (missing or symlink)")?;
    validate(&file, executable)?;
    Ok(file)
}

pub(super) fn read(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    open(path, false)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read private file")?;
    if bytes.len() > limit {
        return Err("Private file exceeds size limit".into());
    }
    Ok(bytes)
}

pub(super) fn remove(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => {
            open(path, false)?;
            fs::remove_file(path).map_err(|_| "Cannot remove managed private file".into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err("Cannot inspect managed file before removal".into()),
    }
}

pub(super) fn write(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    directory(path.parent().ok_or("Missing private parent")?)?;
    if exists_checked(path)? {
        open(path, executable)?;
    }
    let tmp = path.with_extension(format!("{}.tmp", nonce()?));
    let outcome = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable { 0o700 } else { 0o600 })
            .open(&tmp)
            .map_err(|_| "Cannot create private temporary file")?;
        file.write_all(bytes)
            .and_then(|_| file.sync_all())
            .map_err(|_| "Cannot write private file")?;
        fs::rename(&tmp, path).map_err(|_| "Cannot install private file")?;
        File::open(path.parent().ok_or("Missing private parent")?)
            .and_then(|file| file.sync_all())
            .map_err(|_| "Cannot persist private directory update")?;
        Ok(())
    })();
    if outcome.is_err() {
        let _ = fs::remove_file(tmp);
    }
    outcome
}

pub(super) fn nonce() -> Result<String> {
    let mut bytes = [0u8; 32];
    File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut bytes))
        .map_err(|_| "OS randomness unavailable")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

pub(super) struct Lock(File);
impl Lock {
    /// Observe an existing lock without adopting an unsafe file or conflating
    /// permission/IO errors with contention. This grants no lifecycle authority.
    pub(super) fn held(root: &Path, name: &str) -> Result<bool> {
        directory(root)?;
        let file = open(&root.join(name), false)?;
        let fd = std::os::fd::AsRawFd::as_raw_fd(&file);
        if unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            // File close releases this briefly acquired observational lock.
            return Ok(false);
        }
        if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
            Ok(true)
        } else {
            Err("Cannot inspect ChatGPT runtime lock".into())
        }
    }

    pub(super) fn acquire(root: &Path, name: &str) -> Result<Self> {
        directory(root)?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root.join(name))
            .map_err(|_| "Cannot open ChatGPT lock")?;
        validate(&file, false)?;
        if unsafe {
            libc::flock(
                std::os::fd::AsRawFd::as_raw_fd(&file),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        } != 0
        {
            return Err("ChatGPT operation already running; use zai chatgpt status".into());
        }
        Ok(Self(file))
    }
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(std::os::fd::AsRawFd::as_raw_fd(&self.0), libc::LOCK_UN);
        }
    }
}

pub(super) fn root() -> Result<PathBuf> {
    let base = crate::config::zaivern_dir();
    if !base.is_absolute() {
        return Err("ZAIVERN_HOME must be absolute".into());
    }
    if !exists_checked(&base)? {
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&base)
            .map_err(|_| "Cannot create Zaivern directory")?;
    }
    let m = fs::symlink_metadata(&base).map_err(|_| "Cannot inspect Zaivern directory")?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o022 != 0 {
        return Err(
            "Zaivern directory must be owned by you and not writable by others or a symlink".into(),
        );
    }
    let root = base
        .canonicalize()
        .map_err(|_| "Cannot resolve Zaivern directory")?
        .join("chatgpt");
    directory(&root)?;
    Ok(root)
}

#[cfg(test)]
mod metadata_tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn metadata_errors_never_prove_clean_state_or_authorize_overwrite() {
        let root = crate::test_util::unique_temp_dir("chatgpt", "metadata-failure");
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(super::super::daemon::ensure_clean(&root).is_ok());
        for name in [
            "cleanup-prepared.json",
            "cleanup-pending.json",
            "active-generation",
        ] {
            with_metadata_failure(&root.join(name), || {
                assert!(super::super::daemon::ensure_clean(&root).is_err(), "{name}");
            });
            assert!(!root.join("mcp.done").exists());
            assert!(!root.join("shutdown.json").exists());
        }
        let journal = root.join("secret-stores.json");
        write(&journal, b"[\"private-file\"]", false).unwrap();
        with_metadata_failure(&journal, || {
            assert!(super::super::secret::stores(&root).is_err());
            assert!(super::super::secret::remember_store(&root, "secret-service").is_err());
            assert!(write(&journal, b"replacement", false).is_err());
        });
        assert_eq!(read(&journal, 1024).unwrap(), b"[\"private-file\"]");
        let dangling = root.join("dangling");
        symlink(root.join("absent"), &dangling).unwrap();
        assert!(exists_checked(&dangling).unwrap());
        assert!(write(&dangling, b"must not follow", false).is_err());
        assert!(!exists_checked(&root.join("absent")).unwrap());
        fs::remove_dir_all(root).unwrap();
    }
}
