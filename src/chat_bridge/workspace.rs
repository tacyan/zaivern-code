//! A bounded text snapshot, never a host bind mount. Symlinks, hard links,
//! hidden/configuration directories, credential files and special files are denied.
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

pub(super) const FILE_LIMIT: usize = 1024 * 1024;
pub(super) const SNAPSHOT_LIMIT: usize = 8 * FILE_LIMIT;
const FILE_COUNT: usize = 1024;

pub(super) fn validate_root(root: &Path) -> Result<PathBuf, String> {
    if !root.is_absolute() || root.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("workspace must be absolute without parent traversal".into());
    }
    let root = root
        .canonicalize()
        .map_err(|_| "workspace does not exist")?;
    if !root.is_dir() || root.parent().is_none() {
        return Err("workspace must be a project directory".into());
    }
    Ok(crate::pathx::plain(root))
}

fn allowed(path: &Path) -> bool {
    path.components().all(|c| {
        let Component::Normal(name) = c else {
            return false;
        };
        let Some(name) = name.to_str() else {
            return false;
        };
        let name = name.to_ascii_lowercase();
        !name.starts_with('.')
            && !name.contains(':')
            && !name.contains('\\')
            && !name.chars().any(char::is_control)
            && !matches!(
                name.as_str(),
                "target" | "node_modules" | "vendor" | "config"
            )
            && !["config.", "id_", "credential", "secret", "token"]
                .iter()
                .any(|s| name.starts_with(s))
            && ![".pem", ".key", ".p12", ".pfx", ".env"]
                .iter()
                .any(|s| name.ends_with(s))
    })
}

pub(super) struct Snapshot {
    pub root: PathBuf,
    directory: File,
    pub files: BTreeMap<PathBuf, Vec<u8>>,
}
pub(super) struct Applied {
    pub changed: Vec<String>,
    pub error: Option<String>,
}
impl Snapshot {
    pub fn read(root: &Path) -> Result<Self, String> {
        let directory = open_root(root)?;
        let mut snapshot = Self {
            root: root.to_path_buf(),
            directory,
            files: BTreeMap::new(),
        };
        let mut pending = vec![PathBuf::new()];
        let mut bytes = 0;
        let mut visited = 0;
        while let Some(rel) = pending.pop() {
            let directory = if rel.as_os_str().is_empty() {
                snapshot
                    .directory
                    .try_clone()
                    .map_err(|_| "cannot duplicate workspace handle")?
            } else {
                open_relative(&snapshot.directory, &rel, false)?
            };
            for name in directory_names(&directory)? {
                visited += 1;
                if visited > 16 * FILE_COUNT {
                    return Err("workspace entry limit exceeded".into());
                }
                let path = rel.join(name);
                if !allowed(&path) {
                    continue;
                }
                let mut file = open_relative(&snapshot.directory, &path, false)?;
                let metadata = file
                    .metadata()
                    .map_err(|_| "cannot inspect workspace entry")?;
                if metadata.is_dir() {
                    pending.push(path);
                    continue;
                }
                if !metadata.is_file() {
                    return Err("special file in shared workspace files".into());
                }
                let mut content = Vec::new();
                Read::by_ref(&mut file)
                    .take((FILE_LIMIT + 1) as u64)
                    .read_to_end(&mut content)
                    .map_err(|_| "cannot read workspace file")?;
                if content.len() > FILE_LIMIT {
                    return Err("workspace file exceeds 1 MiB".into());
                }
                if std::str::from_utf8(&content).is_err() || content.contains(&0) {
                    continue;
                }
                // The snapshot is source code, not an authentication/configuration channel.
                if content
                    .windows(b"PRIVATE KEY-----".len())
                    .any(|w| w == b"PRIVATE KEY-----")
                {
                    return Err("private key material in shared source".into());
                }
                bytes += content.len();
                if bytes > SNAPSHOT_LIMIT || snapshot.files.len() >= FILE_COUNT {
                    return Err("workspace snapshot limit exceeded".into());
                }
                snapshot.files.insert(path, content);
            }
        }
        Ok(snapshot)
    }

    pub fn stage(&self, destination: &Path) -> Result<(), String> {
        self.stage_changes(destination, &self.files)
    }

    pub fn stage_changes(
        &self,
        destination: &Path,
        changes: &BTreeMap<PathBuf, Vec<u8>>,
    ) -> Result<(), String> {
        for key in self.files.keys() {
            let bytes = changes.get(key).ok_or("missing shared candidate file")?;
            let path = destination.join(key);
            std::fs::create_dir_all(path.parent().ok_or("invalid snapshot path")?)
                .map_err(|_| "cannot stage snapshot")?;
            std::fs::write(path, bytes).map_err(|_| "cannot stage snapshot")?;
        }
        Ok(())
    }

    pub fn apply(&self, changes: &BTreeMap<PathBuf, Vec<u8>>) -> Result<Applied, String> {
        // Open and validate every destination before changing any file. Existing
        // user changes cause rejection; deleted/new files are not imported in MVP.
        let mut destinations = Vec::new();
        for (path, after) in changes {
            let before = self.files.get(path).ok_or("unshared output path")?;
            if before == after {
                continue;
            }
            if after.len() > FILE_LIMIT || std::str::from_utf8(after).is_err() {
                return Err("invalid output file".into());
            }
            if !matches!(
                crate::lease::check_write(&self.root.join(path)),
                crate::lease::Verdict::Allow
            ) {
                return Err("workspace lease denied the edit".into());
            }
            let mut file = open_relative(&self.directory, path, true)?;
            let mut current = Vec::new();
            Read::by_ref(&mut file)
                .take((FILE_LIMIT + 1) as u64)
                .read_to_end(&mut current)
                .map_err(|_| "cannot verify destination")?;
            if &current != before {
                return Err("workspace changed while task ran; no results imported".into());
            }
            destinations.push((path, file, after));
        }
        let mut changed = Vec::new();
        for (path, mut file, after) in destinations {
            changed.push(path.to_string_lossy().into_owned());
            if file
                .rewind()
                .and_then(|()| file.write_all(after))
                .and_then(|()| file.set_len(after.len() as u64))
                .is_err()
            {
                return Ok(Applied { changed, error: Some("Import I/O failure; listed files may contain partial edits. Inspect them locally.".into()) });
            }
        }
        Ok(Applied {
            changed,
            error: None,
        })
    }
}

#[cfg(unix)]
fn open_relative(root: &File, relative: &Path, write: bool) -> Result<File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    if !allowed(relative) {
        return Err("file policy denied access".into());
    }
    let parts: Vec<_> = relative.components().collect();
    if parts.is_empty() {
        return Err("empty file path".into());
    }
    let mut parent = root
        .try_clone()
        .map_err(|_| "cannot duplicate workspace handle")?;
    for (index, part) in parts.iter().enumerate() {
        let name =
            std::ffi::CString::new(part.as_os_str().as_bytes()).map_err(|_| "invalid file name")?;
        let last = index + 1 == parts.len();
        let flags = libc::O_CLOEXEC
            | libc::O_NOFOLLOW
            | libc::O_NONBLOCK
            | if last {
                if write {
                    libc::O_RDWR
                } else {
                    libc::O_RDONLY
                }
            } else {
                libc::O_RDONLY | libc::O_DIRECTORY
            };
        // SAFETY: parent and name live through openat; the returned descriptor is
        // owned exactly once. Every component is opened without following links.
        let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0 {
            return Err("workspace path unavailable or symlink rejected".into());
        }
        let next = unsafe { File::from_raw_fd(fd) };
        if last {
            let meta = next.metadata().map_err(|_| "cannot inspect file")?;
            if !meta.is_dir() && (!meta.is_file() || meta.nlink() != 1) {
                return Err("non-regular or hard-linked file denied".into());
            }
        }
        parent = next;
    }
    Ok(parent)
}
#[cfg(not(unix))]
fn open_relative(_: &File, _: &Path, _: bool) -> Result<File, String> {
    Err("MCP workspace execution requires Unix secure file handles in this MVP".into())
}

#[cfg(unix)]
fn open_root(root: &Path) -> Result<File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    let mut directory = File::open("/").map_err(|_| "cannot open filesystem root")?;
    for part in root.components() {
        let Component::Normal(name) = part else {
            if matches!(part, Component::RootDir) {
                continue;
            }
            return Err("invalid workspace root".into());
        };
        let name = std::ffi::CString::new(name.as_bytes()).map_err(|_| "invalid workspace root")?;
        // SAFETY: all ancestors are directory handles, never symlink paths.
        let fd = unsafe {
            libc::openat(
                directory.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err("workspace root changed or cannot be opened securely".into());
        }
        directory = unsafe { File::from_raw_fd(fd) };
    }
    Ok(directory)
}
#[cfg(not(unix))]
fn open_root(_: &Path) -> Result<File, String> {
    Err("secure workspace handles unsupported".into())
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn directory_names(directory: &File) -> Result<Vec<std::ffi::OsString>, String> {
    use std::os::fd::IntoRawFd;
    use std::os::unix::ffi::OsStringExt;
    struct Directory(*mut libc::DIR);
    impl Drop for Directory {
        fn drop(&mut self) {
            // SAFETY: fdopendir transferred ownership of this stream to us.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let fd = directory
        .try_clone()
        .map_err(|_| "cannot duplicate directory")?
        .into_raw_fd();
    // SAFETY: fd is an owned open directory descriptor. On failure ownership
    // remains ours; on success closedir owns it.
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        unsafe {
            libc::close(fd);
        }
        return Err("cannot enumerate workspace handle".into());
    }
    let stream = Directory(stream);
    let mut names = Vec::new();
    loop {
        // SAFETY: errno is thread-local; stream is exclusively owned, and the
        // returned dirent is copied before the next readdir call.
        unsafe {
            #[cfg(target_os = "macos")]
            let errno = libc::__error();
            #[cfg(target_os = "linux")]
            let errno = libc::__errno_location();
            *errno = 0;
            let entry = libc::readdir(stream.0);
            if entry.is_null() {
                if *errno != 0 {
                    return Err("cannot enumerate workspace handle".into());
                }
                break;
            }
            let bytes = std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()).to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            names.push(std::ffi::OsString::from_vec(bytes.to_vec()));
        }
        if names.len() > 16 * FILE_COUNT {
            return Err("workspace entry limit exceeded".into());
        }
    }
    Ok(names)
}
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn directory_names(_: &File) -> Result<Vec<std::ffi::OsString>, String> {
    Err("secure workspace handles unsupported".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validation_and_dangerous_paths() {
        assert!(validate_root(Path::new("../project")).is_err());
        assert!(validate_root(&std::env::temp_dir().join("../project")).is_err());
        for path in [
            "../x",
            ".git/config",
            ".env",
            ".ssh/id_rsa",
            "secret.pem",
            "a/../../x",
            "config/token",
            "C:\\x",
        ] {
            assert!(!allowed(Path::new(path)), "{path}");
        }
        assert!(allowed(Path::new("src/main.rs")));
    }
    #[cfg(unix)]
    #[test]
    fn symlink_and_hardlink_escape_are_denied() {
        let root = crate::test_util::unique_temp_dir("bridge", "links");
        std::fs::create_dir_all(&root).unwrap();
        let root = validate_root(&root).unwrap();
        std::fs::write(root.join("source"), "text").unwrap();
        std::os::unix::fs::symlink(std::env::temp_dir(), root.join("escape")).unwrap();
        let directory = open_root(&root).unwrap();
        assert!(open_relative(&directory, Path::new("escape/file"), false).is_err());
        assert!(Snapshot::read(&root).is_err());
        std::fs::hard_link(root.join("source"), root.join("alias")).unwrap();
        assert!(open_relative(&directory, Path::new("alias"), false).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[cfg(unix)]
    #[test]
    fn snapshot_edits_and_conflict() {
        let root = crate::test_util::unique_temp_dir("bridge", "edit");
        std::fs::create_dir_all(&root).unwrap();
        let root = validate_root(&root).unwrap();
        std::fs::write(root.join("file.rs"), "before\r\n").unwrap();
        let snapshot = Snapshot::read(&root).unwrap();
        let changes = BTreeMap::from([(PathBuf::from("file.rs"), b"after\r\n".to_vec())]);
        assert_eq!(snapshot.apply(&changes).unwrap().changed, ["file.rs"]);
        assert_eq!(std::fs::read(root.join("file.rs")).unwrap(), b"after\r\n");
        assert!(snapshot.apply(&changes).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn root_replacement_cannot_redirect_existing_snapshot() {
        let base = crate::test_util::unique_temp_dir("bridge", "root-swap");
        std::fs::create_dir_all(base.join("root")).unwrap();
        std::fs::create_dir_all(base.join("outside")).unwrap();
        let base = base.canonicalize().unwrap();
        std::fs::write(base.join("root/file.rs"), "before").unwrap();
        std::fs::write(base.join("outside/file.rs"), "private").unwrap();
        let snapshot = Snapshot::read(&base.join("root")).unwrap();
        std::fs::rename(base.join("root"), base.join("moved")).unwrap();
        std::os::unix::fs::symlink(base.join("outside"), base.join("root")).unwrap();
        assert!(Snapshot::read(&base.join("root")).is_err());
        let result = snapshot
            .apply(&BTreeMap::from([(
                PathBuf::from("file.rs"),
                b"after".to_vec(),
            )]))
            .unwrap();
        assert!(result.error.is_none());
        assert_eq!(
            std::fs::read(base.join("outside/file.rs")).unwrap(),
            b"private"
        );
        assert_eq!(std::fs::read(base.join("moved/file.rs")).unwrap(), b"after");
        std::fs::remove_dir_all(base).unwrap();
    }
}
