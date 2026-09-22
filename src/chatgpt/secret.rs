//! Runtime authentication only. Secrets never implement Debug/Display.
use super::private::{self, Result};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::path::Path;
use zeroize::Zeroizing;

pub(super) struct Secret(pub(super) Zeroizing<Vec<u8>>);
impl Secret {
    pub(super) fn from_bytes(bytes: Vec<u8>) -> Result<Self> {
        let bytes = Zeroizing::new(bytes);
        if bytes.len() < 16 || bytes.len() > 4096 || !bytes.iter().all(|b| (33..=126).contains(b)) {
            return Err(
                "Invalid Runtime API Key format (expected 16–4096 printable ASCII characters)"
                    .into(),
            );
        }
        Ok(Self(bytes))
    }
    pub(super) fn text(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or_default()
    }
}

pub(super) fn prompt() -> Result<Secret> {
    super::process::disable_core_dumps()?;
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|_| {
            "Runtime API Key requires an interactive terminal; never pass it as an argument"
        })?;
    let fd = tty.as_raw_fd();
    let mut old = std::mem::MaybeUninit::<libc::termios>::uninit();
    if unsafe { libc::tcgetattr(fd, old.as_mut_ptr()) } != 0 {
        return Err("Cannot read terminal mode".into());
    }
    let old = unsafe { old.assume_init() };
    struct Restore(i32, libc::termios);
    impl Drop for Restore {
        fn drop(&mut self) {
            unsafe {
                libc::tcsetattr(self.0, libc::TCSAFLUSH, &self.1);
            }
        }
    }
    let mut mode = old;
    // Handle Ctrl-C ourselves so normal cancellation also restores echo.
    mode.c_lflag &= !(libc::ECHO | libc::ECHONL | libc::ICANON | libc::ISIG);
    mode.c_cc[libc::VMIN] = 1;
    mode.c_cc[libc::VTIME] = 0;
    tty.write_all(crate::i18n::tr("chatgpt.secret_prompt").as_bytes())
        .map_err(|_| "Cannot write terminal")?;
    if unsafe { libc::tcsetattr(fd, libc::TCSAFLUSH, &mode) } != 0 {
        return Err("Cannot disable terminal echo".into());
    }
    let _restore = Restore(fd, old);
    let mut bytes = Zeroizing::new(Vec::new());
    loop {
        let mut byte = [0u8];
        tty.read_exact(&mut byte)
            .map_err(|_| "Secret input interrupted")?;
        match byte[0] {
            b'\n' | b'\r' => break,
            3 | 4 => {
                let _ = tty.write_all(b"\n");
                return Err("Cancelled".into());
            }
            8 | 127 => {
                bytes.pop();
            }
            b if bytes.len() < 4096 => bytes.push(b),
            _ => return Err("Runtime key exceeds input limit".into()),
        }
    }
    let _ = tty.write_all(b"\n");
    Secret::from_bytes(std::mem::take(&mut *bytes))
}

fn account(root: &Path) -> String {
    super::install::sha256(root.as_os_str().as_encoded_bytes())
}

#[cfg(not(target_os = "macos"))]
fn service_executable() -> Result<std::path::PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let path = Path::new("/usr/bin/secret-tool")
        .canonicalize()
        .map_err(|_| "System Secret Service tool unavailable")?;
    // Do not let PATH or an Agent-edited helper receive the runtime key.
    let metadata =
        std::fs::symlink_metadata(&path).map_err(|_| "Cannot inspect Secret Service executable")?;
    if !metadata.is_file()
        || metadata.uid() != 0
        || metadata.nlink() != 1
        || metadata.mode() & 0o022 != 0
        || metadata.mode() & 0o111 == 0
    {
        return Err("Secret Service executable must be a root-owned, non-shared executable".into());
    }
    Ok(path)
}

pub(super) fn save(
    root: &Path,
    secret: &Secret,
    approve_file: impl FnOnce() -> Result<bool>,
) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let _ = approve_file;
        remember_store(root, "keychain")?;
        security_framework::passwords::set_generic_password(
            "org.zaivern.chatgpt.runtime",
            &account(root),
            &secret.0,
        )
        .map_err(|_| {
            "macOS Keychain denied access; unlock Keychain and retry zai chatgpt setup --reauth"
        })?;
        Ok("keychain".into())
    }
    #[cfg(not(target_os = "macos"))]
    {
        save_with_service(
            root,
            secret,
            service_executable().ok().as_deref(),
            approve_file,
        )
    }
}

// Inject the executable, not global PATH/DBus state, in deterministic tests.
#[cfg(any(not(target_os = "macos"), test))]
fn save_with_service(
    root: &Path,
    secret: &Secret,
    executable: Option<&Path>,
    approve_file: impl FnOnce() -> Result<bool>,
) -> Result<String> {
    if let Some(bin) = executable {
        // Keep this entry even on failure: the service may have committed the
        // key before its reply was lost. Reset must retain that recovery debt.
        if try_save_service(root, secret, bin).is_ok() {
            return Ok("secret-service".into());
        }
    }
    if !approve_file()? {
        return Err(
            "Secret Service unavailable; private-file fallback declined. Setup cancelled.".into(),
        );
    }
    remember_store(root, "private-file")?;
    private::write(&root.join("runtime-key"), &secret.0, false)?;
    Ok("private-file".into())
}

#[cfg(any(not(target_os = "macos"), test))]
fn try_save_service(root: &Path, secret: &Secret, bin: &Path) -> Result<()> {
    remember_store(root, "secret-service")?;
    let mut command = super::process::command(bin);
    command
        .args([
            "store",
            "--label=Zaivern ChatGPT Runtime",
            "application",
            "zaivern-chatgpt",
            "account",
            &account(root),
        ])
        .stdin(std::process::Stdio::piped());
    let mut child = super::process::OwnedProcessGroup::spawn(&mut command)?;
    let mut input = child.take_lease()?;
    input
        .write_all(&secret.0)
        .and_then(|_| input.write_all(b"\n"))
        .map_err(|_| "Secret Service write failed")?;
    drop(input);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if child.exited()? {
            let status = child.reclaim()?;
            return if status.success() {
                Ok(())
            } else {
                Err("Secret Service denied access; unlock your keyring and retry".into())
            };
        }
        if std::time::Instant::now() >= deadline {
            return Err("Secret Service timed out".into());
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
}

pub(super) fn load(root: &Path, backend: &str) -> Result<Secret> {
    load_scoped(root, backend, None)
}

pub(super) fn load_guarded(
    root: &Path,
    backend: &str,
    scope: &super::process::GuardianScope,
) -> Result<Secret> {
    load_scoped(root, backend, Some(scope))
}

fn load_scoped(
    root: &Path,
    backend: &str,
    _scope: Option<&super::process::GuardianScope>,
) -> Result<Secret> {
    super::process::disable_core_dumps()?;
    let bytes = match backend {
        #[cfg(target_os = "macos")]
        "keychain" => security_framework::passwords::get_generic_password(
            "org.zaivern.chatgpt.runtime",
            &account(root),
        )
        .map_err(|_| "Runtime key unavailable in Keychain; run zai chatgpt setup --reauth")?,
        #[cfg(not(target_os = "macos"))]
        "secret-service" => {
            let bin = service_executable()?;
            let mut command = super::process::command(&bin);
            command.args([
                "lookup",
                "application",
                "zaivern-chatgpt",
                "account",
                &account(root),
            ]);
            let mut bytes = match _scope {
                Some(scope) => super::process::capture_guarded(
                    scope,
                    command,
                    std::time::Duration::from_secs(60),
                    4097,
                )?,
                None => super::process::capture(command, std::time::Duration::from_secs(60), 4097)?,
            };
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            bytes
        }
        "private-file" => private::read(&root.join("runtime-key"), 4096)?,
        _ => return Err("Unsupported secret store; run zai chatgpt setup --reauth".into()),
    };
    Secret::from_bytes(bytes)
}

pub(super) fn remove(root: &Path, backend: &str) -> Result<()> {
    match backend {
        #[cfg(target_os = "macos")]
        "keychain" => match security_framework::passwords::delete_generic_password(
            "org.zaivern.chatgpt.runtime",
            &account(root),
        ) {
            Ok(()) => {}
            Err(error) if error.code() == -25300 => {} // errSecItemNotFound: idempotent deletion
            Err(_) => return Err("Cannot remove Runtime key from Keychain".into()),
        },
        #[cfg(not(target_os = "macos"))]
        "secret-service" => {
            let bin = service_executable()?;
            let mut cmd = super::process::command(&bin);
            cmd.args([
                "clear",
                "application",
                "zaivern-chatgpt",
                "account",
                &account(root),
            ]);
            super::process::capture(cmd, std::time::Duration::from_secs(30), 1024)?;
        }
        "private-file" => {
            private::remove(&root.join("runtime-key"))?;
        }
        _ => return Err("Unknown secret store".into()),
    }
    Ok(())
}

pub(super) fn stores(root: &Path) -> Result<Vec<String>> {
    if !private::exists_checked(&root.join("secret-stores.json"))? {
        return Ok(Vec::new());
    }
    let stores: Vec<String> =
        serde_json::from_slice(&private::read(&root.join("secret-stores.json"), 1024)?)
            .map_err(|_| "Invalid secret store recovery journal")?;
    if stores.len() > 3
        || stores
            .iter()
            .any(|s| !["keychain", "secret-service", "private-file"].contains(&s.as_str()))
    {
        return Err("Invalid secret store recovery journal".into());
    }
    Ok(stores)
}

pub(super) fn remember_store(root: &Path, backend: &str) -> Result<()> {
    if !["keychain", "secret-service", "private-file"].contains(&backend) {
        return Err("Unknown secret store".into());
    }
    let mut stores = stores(root)?;
    if !stores.iter().any(|s| s == backend) {
        stores.push(backend.into());
    }
    private::write(
        &root.join("secret-stores.json"),
        &serde_json::to_vec(&stores).map_err(|_| "Cannot encode secret store journal")?,
        false,
    )
}

pub(super) fn forget_store(root: &Path, backend: &str) -> Result<()> {
    let mut stores = stores(root)?;
    stores.retain(|s| s != backend);
    private::write(
        &root.join("secret-stores.json"),
        &serde_json::to_vec(&stores).map_err(|_| "Cannot encode secret store journal")?,
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn exercise(success: bool, approve: bool) {
        let root = crate::test_util::unique_temp_dir("chatgpt", "secret-fallback");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let bin = root.join("secret-tool");
        private::write(&bin, format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$0.args\"\nIFS= read -r key\nprintf '%s' \"$key\" >&2\nprintf '%s' \"$key\"\nexit {}\n",
            if success { 0 } else { 1 }).as_bytes(), true).unwrap();
        let sentinel = "sk-runtime-fallback-sentinel-123456789";
        let secret = Secret::from_bytes(sentinel.as_bytes().to_vec()).unwrap();
        let asked = std::cell::Cell::new(false);
        let result = save_with_service(&root, &secret, Some(&bin), || {
            asked.set(true);
            Ok(approve)
        });
        assert_eq!(asked.get(), !success);
        if success {
            assert_eq!(result.unwrap(), "secret-service");
            assert_eq!(stores(&root).unwrap(), ["secret-service"]);
            assert!(!root.join("runtime-key").exists());
        } else if approve {
            assert_eq!(result.unwrap(), "private-file");
            assert_eq!(stores(&root).unwrap(), ["secret-service", "private-file"]);
            let path = root.join("runtime-key");
            assert_eq!(private::read(&path, 4096).unwrap(), sentinel.as_bytes());
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        } else {
            assert!(!result.unwrap_err().contains(sentinel));
            assert!(!root.join("runtime-key").exists());
            assert_eq!(stores(&root).unwrap(), ["secret-service"]);
        }
        for file in ["secret-tool.args", "secret-stores.json"] {
            assert!(!std::fs::read_to_string(root.join(file))
                .unwrap()
                .contains(sentinel));
        }
        assert!(!root.join("config.json").exists());
        assert!(!root.join("profile.yaml").exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn secret_service_unavailable_accepted_private_file() {
        exercise(false, true);
    }
    #[test]
    fn secret_service_unavailable_declined_does_not_save_file() {
        exercise(false, false);
    }
    #[test]
    fn secret_service_success_never_prompts_for_fallback() {
        exercise(true, false);
    }
}
