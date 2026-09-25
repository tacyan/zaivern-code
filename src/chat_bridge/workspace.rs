//! A bounded text snapshot, never a host bind mount. Symlinks, hard links,
//! hidden/configuration directories, credential files and special files are denied.
use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{Read, Seek, Write};
use std::path::{Component, Path, PathBuf};

pub(super) const FILE_LIMIT: usize = 1024 * 1024;
pub(super) const SNAPSHOT_LIMIT: usize = 8 * FILE_LIMIT;
const FILE_COUNT: usize = 1024;
// A separate verifier budget, never an Agent context increase. At most one
// quarter of the existing 256 MiB volume; the rest is reserved for build output.
pub(super) const VERIFICATION_LIMIT: usize = 64 * FILE_LIMIT;
const VERIFICATION_FILES: usize = 8192;

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
    allowed_verification(path)
        && !path
            .components()
            .any(|c| c.as_os_str().eq_ignore_ascii_case("vendor"))
}

// Vendor is only reachable through the original Cargo graph, never Agent selection.
pub(super) fn allowed_verification(path: &Path) -> bool {
    // Bound metadata/index and Agent path-list memory independently of file bytes.
    if path.as_os_str().len() > 1024 || path.components().count() > 64 {
        return false;
    }
    path.components().all(|c| {
        let Component::Normal(name) = c else {
            return false;
        };
        let Some(name) = name.to_str() else {
            return false;
        };
        let name = name.to_ascii_lowercase();
        let (stem, extension) = name.rsplit_once('.').unwrap_or((&name, ""));
        let sensitive = matches!(
            stem,
            "credential" | "credentials" | "secret" | "secrets" | "token" | "tokens"
        ) && matches!(
            extension,
            "" | "json" | "yaml" | "yml" | "toml" | "ini" | "txt"
        );
        !name.starts_with('.')
            && !sensitive
            && !name.contains(':')
            && !name.contains('\\')
            && !name.chars().any(char::is_control)
            && !matches!(name.as_str(), "target" | "node_modules" | "config")
            && !["config.", "id_"].iter().any(|s| name.starts_with(s))
            && ![".pem", ".key", ".p12", ".pfx", ".env"]
                .iter()
                .any(|s| name.ends_with(s))
    })
}

pub(super) struct Snapshot {
    pub root: PathBuf,
    directory: File,
    pub files: BTreeMap<PathBuf, Vec<u8>>,
    verification_only: BTreeMap<PathBuf, Vec<u8>>,
    pub omitted: usize,
    unsafe_cargo_source: std::cell::Cell<bool>,
}
pub(super) struct Applied {
    pub changed: Vec<String>,
    pub error: Option<String>,
}
impl Snapshot {
    #[cfg(all(test, unix))]
    pub fn read(root: &Path) -> Result<Self, String> {
        Self::read_for_task(root, "")
    }

    pub fn read_for_task(root: &Path, instruction: &str) -> Result<Self, String> {
        let mut snapshot = Self {
            root: root.to_path_buf(),
            directory: open_root(root)?,
            files: BTreeMap::new(),
            verification_only: BTreeMap::new(),
            omitted: 0,
            unsafe_cargo_source: std::cell::Cell::new(false),
        };
        // This closure is frozen before the Agent starts. Candidate manifests
        // never authorize additional host reads.
        let graph = super::cargo_graph::discover(&snapshot)?;
        let dependency_roots: BTreeSet<_> = graph
            .packages
            .keys()
            .filter(|p| !p.as_os_str().is_empty())
            .cloned()
            .collect();
        let mut paths = snapshot.walk(Path::new(""), false, &graph.dependencies)?;
        let hints: Vec<_> = instruction
            .split(|c: char| !(c.is_alphanumeric() || "_./-".contains(c)))
            .filter(|s| !s.is_empty())
            .collect();
        paths.sort_by_cached_key(|path| (source_priority(path, &hints), path.clone()));
        let mut bytes = 0;
        for path in paths {
            if snapshot.files.len() == FILE_COUNT || bytes == SNAPSHOT_LIMIT {
                snapshot.omitted += 1;
                continue;
            }
            let Some(content) = snapshot.read_text(&path)? else {
                snapshot.omitted += 1;
                continue;
            };
            if bytes + content.len() > SNAPSHOT_LIMIT {
                snapshot.omitted += 1;
                continue;
            }
            bytes += content.len();
            snapshot.files.insert(path, content);
        }
        // Original manifests are authoritative even if source selection would
        // omit one. Root Cargo.toml is priority zero and cannot be oversized.
        if let Some(original) = graph.manifests.get(Path::new("Cargo.toml")) {
            if snapshot.files.get(Path::new("Cargo.toml")) != Some(original) {
                return Err("root manifest changed during snapshot or cannot be shared".into());
            }
        }
        let mut verification_paths = BTreeSet::new();
        for (package, manifest) in &graph.packages {
            verification_paths.extend(super::cargo_graph::inputs(
                &snapshot,
                package,
                manifest,
                &dependency_roots,
            )?);
            if verification_paths.len() > VERIFICATION_FILES {
                return Err("Cargo verification file limit exceeded".into());
            }
        }
        let mut verification_bytes = bytes;
        for path in verification_paths {
            if snapshot.files.contains_key(&path) {
                continue;
            }
            let Some(content) = snapshot.read_text(&path)? else {
                if path.extension().is_some_and(|e| e == "rs") {
                    snapshot.unsafe_cargo_source.set(true);
                }
                continue;
            };
            verification_bytes += content.len();
            if verification_bytes > VERIFICATION_LIMIT
                || snapshot.files.len() + snapshot.verification_only.len() >= VERIFICATION_FILES
            {
                return Err("Cargo verification snapshot limit exceeded".into());
            }
            snapshot.verification_only.insert(path, content);
        }
        for (path, original) in graph.manifests {
            if snapshot
                .files
                .get(&path)
                .or_else(|| snapshot.verification_only.get(&path))
                != Some(&original)
            {
                return Err("Cargo manifest changed during snapshot".into());
            }
        }
        Ok(snapshot)
    }

    pub(super) fn names(&self, path: &Path) -> Result<Vec<std::ffi::OsString>, String> {
        let dir = if path.as_os_str().is_empty() {
            self.directory
                .try_clone()
                .map_err(|_| "cannot duplicate workspace handle")?
        } else {
            open_relative(&self.directory, path, false)?
        };
        directory_names(&dir)
    }

    pub(super) fn is_file(&self, path: &Path) -> Result<bool, String> {
        Ok(open_relative(&self.directory, path, false)?
            .metadata()
            .map_err(|_| "cannot inspect input")?
            .is_file())
    }

    pub(super) fn exists(&self, path: &Path) -> Result<bool, String> {
        let parent = path.parent().ok_or("invalid input path")?;
        let name = path.file_name().ok_or("invalid input path")?;
        Ok(self.names(parent)?.iter().any(|n| n == name))
    }

    pub(super) fn read_text(&self, path: &Path) -> Result<Option<Vec<u8>>, String> {
        let mut file = open_relative(&self.directory, path, false)?;
        if !file
            .metadata()
            .map_err(|_| "cannot inspect input")?
            .is_file()
        {
            return Err("expected regular source file".into());
        }
        let mut content = Vec::new();
        Read::by_ref(&mut file)
            .take((FILE_LIMIT + 1) as u64)
            .read_to_end(&mut content)
            .map_err(|_| "cannot read workspace file")?;
        // Never share an oversized, binary or private-key-bearing file, even
        // when explicitly named in an instruction or Cargo manifest.
        if content.len() > FILE_LIMIT
            || std::str::from_utf8(&content).is_err()
            || content.contains(&0)
            || content
                .windows(b"PRIVATE KEY-----".len())
                .any(|w| w == b"PRIVATE KEY-----")
        {
            return Ok(None);
        }
        Ok(Some(content))
    }

    pub(super) fn walk(
        &self,
        start: &Path,
        verification: bool,
        excluded: &BTreeSet<PathBuf>,
    ) -> Result<Vec<PathBuf>, String> {
        let mut pending = vec![start.to_path_buf()];
        let mut files = Vec::new();
        let mut visited = 0;
        while let Some(rel) = pending.pop() {
            for name in self.names(&rel)? {
                visited += 1;
                if visited > 16 * FILE_COUNT {
                    return Err("workspace entry limit exceeded".into());
                }
                let path = rel.join(&name);
                let admitted = if verification {
                    allowed_verification(&path)
                } else {
                    allowed(&path)
                };
                let vendor = name.eq_ignore_ascii_case("vendor");
                if !admitted || vendor {
                    // Cargo discovers integration tests/targets by directory.
                    // Silently dropping even a forbidden source name could turn
                    // a failing suite green. Do not open it; reject verification.
                    if verification
                        && rel.components().any(|c| {
                            matches!(
                                c.as_os_str().to_str(),
                                Some("src" | "tests" | "examples" | "benches")
                            )
                        })
                    {
                        self.unsafe_cargo_source.set(true);
                    }
                    continue;
                }
                if excluded.contains(&path) {
                    continue;
                }
                let file = open_relative(&self.directory, &path, false)?;
                let meta = file
                    .metadata()
                    .map_err(|_| "cannot inspect workspace entry")?;
                if meta.is_dir() {
                    pending.push(path);
                } else if meta.is_file() {
                    files.push(path);
                } else {
                    return Err("special file in shared workspace files".into());
                }
            }
        }
        Ok(files)
    }

    pub(super) fn validate_local_directory(&self, path: &Path) -> Result<(), String> {
        if !path.as_os_str().is_empty() {
            let file = open_relative(&self.directory, path, false)?;
            if !file
                .metadata()
                .map_err(|_| "cannot inspect local dependency")?
                .is_dir()
            {
                return Err("Cargo local dependency must be a directory".into());
            }
        }
        let canonical = self
            .root
            .join(path)
            .canonicalize()
            .map_err(|_| "local dependency unavailable")?;
        if !crate::pathx::plain(canonical).starts_with(&self.root) {
            return Err("Cargo local dependency escapes workspace".into());
        }
        Ok(())
    }

    pub fn stage_verification(
        &self,
        destination: &Path,
        changes: &BTreeMap<PathBuf, Vec<u8>>,
    ) -> Result<(), String> {
        // A future caller must not accidentally materialize a hidden-input
        // execution tree by bypassing the executor's NotVerified decision.
        if self.has_verification_only() {
            return Err("unshared Cargo inputs cannot be staged for candidate execution".into());
        }
        if self.unsafe_cargo_source.get() {
            return Err("Cargo source excluded by safety policy; changes were not imported".into());
        }
        if changes.len() != self.files.len() || changes.keys().any(|p| !self.files.contains_key(p))
        {
            return Err("candidate must contain exactly the shared files".into());
        }
        let total: usize = changes.values().map(Vec::len).sum();
        if total > VERIFICATION_LIMIT || changes.values().any(|b| b.len() > FILE_LIMIT) {
            return Err("Cargo verification snapshot limit exceeded".into());
        }
        self.stage_changes(destination, changes)?;
        Ok(())
    }

    #[cfg(all(test, unix))]
    pub(super) fn has_frozen_input(&self, path: &Path) -> bool {
        self.verification_only.contains_key(path)
    }

    pub(super) fn has_verification_only(&self) -> bool {
        !self.verification_only.is_empty()
    }

    #[cfg(all(test, unix))]
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
            stage_file(destination, key, bytes)?;
        }
        Ok(())
    }

    // The held directory FD pins the original identity (and prevents inode reuse).
    // Reopen every path component without following symlinks before using leases.
    #[cfg(unix)]
    fn ensure_root_unchanged(&self) -> Result<(), String> {
        use std::os::unix::fs::MetadataExt;
        let error = "workspace root changed while task ran; no results imported";
        let original = self.directory.metadata().map_err(|_| error)?;
        let current = open_root(&self.root).map_err(|_| error)?;
        let current = current.metadata().map_err(|_| error)?;
        if original.dev() != current.dev() || original.ino() != current.ino() {
            return Err(error.into());
        }
        Ok(())
    }

    #[cfg(not(unix))]
    fn ensure_root_unchanged(&self) -> Result<(), String> {
        Err("secure workspace handles unsupported".into())
    }

    pub fn apply(&self, changes: &BTreeMap<PathBuf, Vec<u8>>) -> Result<Applied, String> {
        self.ensure_root_unchanged()?;
        // Open and validate every destination before changing any file. Existing
        // user changes cause rejection; deleted/new files are not imported in MVP.
        let mut destinations = Vec::new();
        if changes.len() != self.files.len()
            || changes.keys().any(|path| !self.files.contains_key(path))
        {
            return Err("candidate must contain exactly the shared files".into());
        }
        // Hidden inputs were not executed, so never reread/compare them here:
        // their contents or concurrent edits must not decide import success.
        // Keep original manifests authoritative across subsequent tasks too.
        // Removing a local dependency must not reclassify its hidden sources
        // as editable on the next snapshot.
        if self.has_verification_only()
            && changes.iter().any(|(path, after)| {
                path.file_name().is_some_and(|name| name == "Cargo.toml")
                    && self.files.get(path) != Some(after)
            })
        {
            return Err("Cargo manifests defining unshared input scope cannot be imported".into());
        }
        for (path, after) in changes {
            let before = self.files.get(path).ok_or("unshared output path")?;
            if after.len() > FILE_LIMIT || std::str::from_utf8(after).is_err() {
                return Err("invalid output file".into());
            }
            if before != after
                && !matches!(
                    crate::lease::check_write(&self.root.join(path)),
                    crate::lease::Verdict::Allow
                )
            {
                return Err("workspace lease denied the edit".into());
            }
            let mut file = open_relative(&self.directory, path, before != after)?;
            let mut current = Vec::new();
            Read::by_ref(&mut file)
                .take((FILE_LIMIT + 1) as u64)
                .read_to_end(&mut current)
                .map_err(|_| "cannot verify destination")?;
            if &current != before {
                return Err("workspace changed while task ran; no results imported".into());
            }
            if before != after {
                destinations.push((path, file, after));
            }
        }
        // Validation can take time; reject a root swap during that pass too.
        // This remains inside Control::import's cancellation/import gate.
        #[cfg(all(test, unix))]
        tests::before_write_hook();
        self.ensure_root_unchanged()?;
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

fn stage_file(destination: &Path, relative: &Path, bytes: &[u8]) -> Result<(), String> {
    let path = destination.join(relative);
    std::fs::create_dir_all(path.parent().ok_or("invalid snapshot path")?)
        .map_err(|_| "cannot stage snapshot")?;
    std::fs::write(path, bytes).map_err(|_| "cannot stage snapshot".into())
}

fn source_priority(path: &Path, hints: &[&str]) -> u8 {
    if path == Path::new("Cargo.toml") {
        return 0;
    }
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
    if hints.iter().any(|hint| {
        let hint = Path::new(hint);
        hint.components().all(|c| matches!(c, Component::Normal(_)))
            && (path.starts_with(hint) || hint == Path::new(name))
    }) {
        return 1;
    }
    if matches!(
        name,
        "lib.rs" | "main.rs" | "mod.rs" | "build.rs" | "Cargo.lock" | "README.md"
    ) {
        return 2;
    }
    if path.extension().is_some_and(|e| e == "rs") {
        return 3;
    }
    4
}

#[cfg(unix)]
fn open_relative(root: &File, relative: &Path, write: bool) -> Result<File, String> {
    use std::os::fd::{AsRawFd, FromRawFd};
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;
    if !allowed_verification(relative) {
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
    use std::os::fd::AsRawFd;
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
    // A dup/try_clone shares the directory cursor with the original handle.
    // Reopen "." relative to the pinned descriptor for an independent cursor.
    // SAFETY: the directory descriptor and NUL-terminated name remain valid.
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            c".".as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err("cannot reopen workspace directory".into());
    }
    // SAFETY: on success closedir owns fd; on failure we close it below.
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
    #[cfg(unix)]
    #[test]
    fn external_change_to_unedited_input_prevents_all_import() {
        let root = crate::test_util::unique_temp_dir("bridge", "unchanged-conflict");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.rs"), "original").unwrap();
        std::fs::write(root.join("input.txt"), "verified input").unwrap();
        let root = validate_root(&root).unwrap();
        let snapshot = Snapshot::read(&root).unwrap();
        let mut changes = snapshot.files.clone();
        changes.insert(PathBuf::from("lib.rs"), b"candidate".to_vec());
        std::fs::write(root.join("input.txt"), "external edit").unwrap();
        assert!(snapshot.apply(&changes).is_err());
        assert_eq!(
            std::fs::read_to_string(root.join("lib.rs")).unwrap(),
            "original"
        );
        assert_eq!(
            std::fs::read_to_string(root.join("input.txt")).unwrap(),
            "external edit"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn validation_and_dangerous_paths() {
        assert!(validate_root(Path::new("../project")).is_err());
        assert!(validate_root(&std::env::temp_dir().join("../project")).is_err());
        for path in [
            "../x",
            ".git/config",
            ".env",
            ".env.local",
            "private.key",
            "credentials.json",
            "token.json",
            "tokens.json",
            "secret.json",
            "secrets.json",
            "credentials",
            "credentials.yaml",
            "credentials.toml",
            "secrets.yml",
            "token.txt",
            "secret.ini",
            "id_rsa",
            "id_ed25519",
            "private.p12",
            "private.pfx",
            ".ssh/id_rsa",
            "secret.pem",
            "a/../../x",
            "config/token",
            "C:\\x",
        ] {
            assert!(!allowed(Path::new(path)), "{path}");
        }
        assert!(allowed(Path::new("src/main.rs")));
        for path in [
            "src/tokenizer.rs",
            "src/tokens.rs",
            "src/secretary.rs",
            "src/credential_manager.rs",
        ] {
            assert!(allowed(Path::new(path)), "{path}");
        }
    }
    #[cfg(unix)]
    #[test]
    fn snapshot_includes_tokenizer_but_not_credentials() {
        let root = crate::test_util::unique_temp_dir("bridge", "tokenizer");
        std::fs::create_dir_all(root.join("src")).unwrap();
        let root = validate_root(&root).unwrap();
        std::fs::write(root.join("src/lib.rs"), "mod tokenizer;\n").unwrap();
        std::fs::write(root.join("src/tokenizer.rs"), "pub fn tokenize() {}\n").unwrap();
        std::fs::write(root.join("credentials.json"), "private fixture").unwrap();
        let snapshot = Snapshot::read(&root).unwrap();
        assert_eq!(snapshot.files.len(), 2);
        assert!(snapshot.files.contains_key(Path::new("src/tokenizer.rs")));
        assert!(!snapshot.files.contains_key(Path::new("credentials.json")));
        std::fs::remove_dir_all(root).unwrap();
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
        for replacement in ["symlink", "same_inode_symlink", "directory", "missing"] {
            let base = crate::test_util::unique_temp_dir("bridge", "root-swap");
            std::fs::create_dir_all(base.join("root")).unwrap();
            std::fs::create_dir_all(base.join("outside")).unwrap();
            let base = base.canonicalize().unwrap();
            std::fs::write(base.join("root/file.rs"), "before").unwrap();
            std::fs::write(base.join("outside/file.rs"), "private").unwrap();
            let snapshot = Snapshot::read(&base.join("root")).unwrap();
            std::fs::rename(base.join("root"), base.join("moved")).unwrap();
            match replacement {
                "symlink" | "same_inode_symlink" => {
                    let target = if replacement == "symlink" {
                        "outside"
                    } else {
                        "moved"
                    };
                    std::os::unix::fs::symlink(base.join(target), base.join("root")).unwrap();
                    assert!(Snapshot::read(&base.join("root")).is_err());
                }
                "directory" => {
                    std::fs::create_dir(base.join("root")).unwrap();
                    // Identical bytes must not hide a different directory inode.
                    std::fs::write(base.join("root/file.rs"), "before").unwrap();
                }
                _ => {}
            }
            let result = snapshot.apply(&BTreeMap::from([(
                PathBuf::from("file.rs"),
                b"after".to_vec(),
            )]));
            assert_eq!(
                result.err().unwrap(),
                "workspace root changed while task ran; no results imported"
            );
            assert_eq!(
                std::fs::read(base.join("outside/file.rs")).unwrap(),
                b"private"
            );
            assert_eq!(
                std::fs::read(base.join("moved/file.rs")).unwrap(),
                b"before"
            );
            if replacement == "directory" {
                assert_eq!(std::fs::read(base.join("root/file.rs")).unwrap(), b"before");
            }
            std::fs::remove_dir_all(base).unwrap();
        }
    }
    #[cfg(unix)]
    #[test]
    fn external_file_edit_rejects_all_imports_before_first_write() {
        let base = crate::test_util::unique_temp_dir("bridge", "external-conflict");
        std::fs::create_dir_all(&base).unwrap();
        let root = validate_root(&base).unwrap();
        for name in ["a.rs", "z.rs"] {
            std::fs::write(root.join(name), "before").unwrap();
        }
        let snapshot = Snapshot::read(&root).unwrap();
        std::fs::write(root.join("z.rs"), "external edit").unwrap();
        let changes = BTreeMap::from([
            (PathBuf::from("a.rs"), b"candidate".to_vec()),
            (PathBuf::from("z.rs"), b"candidate".to_vec()),
        ]);
        assert!(snapshot.apply(&changes).is_err());
        assert_eq!(std::fs::read(root.join("a.rs")).unwrap(), b"before");
        assert_eq!(std::fs::read(root.join("z.rs")).unwrap(), b"external edit");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[cfg(unix)]
    std::thread_local! {
        static BEFORE_WRITE: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {
            std::cell::RefCell::new(None)
        };
    }

    #[cfg(unix)]
    pub(super) fn before_write_hook() {
        let hook = BEFORE_WRITE.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook();
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_swap_after_validation_still_imports_zero_files() {
        let base = crate::test_util::unique_temp_dir("bridge", "late-root-swap");
        std::fs::create_dir_all(base.join("root")).unwrap();
        let root = validate_root(&base.join("root")).unwrap();
        for name in ["a.rs", "z.rs"] {
            std::fs::write(root.join(name), "before").unwrap();
        }
        let snapshot = Snapshot::read(&root).unwrap();
        let moved = root.with_file_name("moved");
        let swap_root = root.clone();
        let swap_moved = moved.clone();
        BEFORE_WRITE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                std::fs::rename(&swap_root, swap_moved).unwrap();
                std::fs::create_dir(&swap_root).unwrap();
                for name in ["a.rs", "z.rs"] {
                    std::fs::write(swap_root.join(name), "replacement").unwrap();
                }
            }));
        });
        let changes = BTreeMap::from([
            (PathBuf::from("a.rs"), b"candidate".to_vec()),
            (PathBuf::from("z.rs"), b"candidate".to_vec()),
        ]);
        assert_eq!(
            snapshot.apply(&changes).err().unwrap(),
            "workspace root changed while task ran; no results imported"
        );
        for name in ["a.rs", "z.rs"] {
            assert_eq!(std::fs::read(moved.join(name)).unwrap(), b"before");
            assert_eq!(std::fs::read(root.join(name)).unwrap(), b"replacement");
        }
        std::fs::remove_dir_all(base).unwrap();
    }
}
