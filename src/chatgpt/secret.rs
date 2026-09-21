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

pub(super) fn save(root: &Path, secret: &Secret, allow_file: bool) -> Result<String> {
    #[cfg(target_os = "macos")]
    {
        let _ = allow_file;
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
        if let Some(bin) = crate::shellenv::which("secret-tool") {
            remember_store(root, "secret-service")?;
            let mut command = super::process::command(&bin);
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
            let mut child = super::process::OwnedChild::spawn(&mut command)?;
            let mut input = child
                .child
                .stdin
                .take()
                .ok_or("Secret Service input unavailable")?;
            input
                .write_all(&secret.0)
                .and_then(|_| input.write_all(b"\n"))
                .map_err(|_| "Secret Service write failed")?;
            drop(input);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if let Some(status) = child.exited()? {
                    return if status.success() {
                        Ok("secret-service".into())
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
        if !allow_file {
            return Err("Secret Service unavailable. Explicitly approve the private 0600 file fallback in setup".into());
        }
        remember_store(root, "private-file")?;
        private::write(&root.join("runtime-key"), &secret.0, false)?;
        Ok("private-file".into())
    }
}

pub(super) fn load(root: &Path, backend: &str) -> Result<Secret> {
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
            let bin = crate::shellenv::which("secret-tool").ok_or("Secret Service unavailable")?;
            let mut command = super::process::command(&bin);
            command.args([
                "lookup",
                "application",
                "zaivern-chatgpt",
                "account",
                &account(root),
            ]);
            let mut bytes =
                super::process::capture(command, std::time::Duration::from_secs(60), 4097)?;
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
            let bin = crate::shellenv::which("secret-tool").ok_or("Secret Service unavailable")?;
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
    if !root.join("secret-stores.json").exists() {
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
