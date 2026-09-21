//! Official full-client installation. Verify before parsing or executing bytes.
use super::private::{self, Result};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::io::{Cursor, Read};
use std::path::Path;
use std::time::Duration;

pub(super) const TESTED_VERSION: &str = "0.0.14";
// Compatibility is established by tests, not inferred from pre-1.0 semver.
pub(super) const SUPPORTED_VERSIONS: &[&str] = &[TESTED_VERSION];
const DOWNLOAD_LIMIT: usize = 256 * 1024 * 1024;
const BINARY_LIMIT: usize = 512 * 1024 * 1024;

pub(super) fn platform(os: &str, arch: &str) -> Result<String> {
    let os = match os {
        "macos" => "darwin",
        "linux" => "linux",
        _ => {
            return Err(
                "ChatGPT Bridge supports macOS and Linux; Windows is not yet supported".into(),
            )
        }
    };
    let arch = match arch {
        "x86_64" => "amd64",
        "aarch64" | "arm64" => "arm64",
        _ => return Err("Unsupported CPU architecture".into()),
    };
    Ok(format!("{os}-{arch}"))
}

pub(super) fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub(super) fn verify_sha(bytes: &[u8], expected: &str) -> Result<()> {
    if expected.len() != 64
        || !expected.bytes().all(|b| b.is_ascii_hexdigit())
        || sha256(bytes) != expected.to_ascii_lowercase()
    {
        return Err("Tunnel client SHA256 mismatch; previous installation was preserved".into());
    }
    Ok(())
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<Asset>,
}
#[derive(Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    digest: Option<String>,
    size: u64,
}

fn select<'a>(release: &'a Release, name: &str) -> Result<&'a Asset> {
    let mut matches = release.assets.iter().filter(|a| a.name == name);
    let asset = matches.next().ok_or("Official release asset not found")?;
    if matches.next().is_some() || asset.browser_download_url != format!("https://github.com/openai/tunnel-client/releases/download/v{TESTED_VERSION}/{name}") {
        return Err("Unexpected or ambiguous release asset".into());
    }
    Ok(asset)
}

fn download(url: &str, limit: usize) -> Result<Vec<u8>> {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(Duration::from_secs(180)))
        .max_redirects(5)
        .user_agent("Zaivern-ChatGPT-Setup")
        .build()
        .into();
    let mut response = agent
        .get(url)
        .call()
        .map_err(|_| "Official download failed; check HTTPS connectivity and GitHub rate limits")?;
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Download interrupted")?;
    if bytes.len() > limit {
        return Err("Release asset exceeds download limit".into());
    }
    Ok(bytes)
}

fn asset_bytes(asset: &Asset, limit: usize) -> Result<Vec<u8>> {
    if asset.size > limit as u64 {
        return Err("Release asset exceeds download limit".into());
    }
    let bytes = download(&asset.browser_download_url, limit)?;
    if bytes.len() as u64 != asset.size {
        return Err("Release asset size mismatch".into());
    }
    if let Some(digest) = &asset.digest {
        verify_sha(
            &bytes,
            digest
                .strip_prefix("sha256:")
                .ok_or("Unsupported release digest")?,
        )?;
    }
    Ok(bytes)
}

fn checksum<'a>(sums: &'a str, name: &str) -> Result<&'a str> {
    let mut found = None;
    for line in sums.lines() {
        let parts: Vec<_> = line.split_whitespace().collect();
        if parts.len() == 2 && parts[1].trim_start_matches('*') == name {
            if found.is_some() {
                return Err("Duplicate checksum entry".into());
            }
            found = Some(parts[0]);
        }
    }
    found.ok_or_else(|| "Missing asset SHA256SUMS entry".into())
}

fn extract(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| "Invalid release ZIP")?;
    if archive.len() > 256 {
        return Err("Too many ZIP entries".into());
    }
    let mut names = std::collections::HashSet::new();
    let mut binary = None;
    let mut total = 0u64;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(|_| "Invalid ZIP entry")?;
        let name = entry.name().to_string();
        if entry.enclosed_name().is_none() || name.contains('\\') || !names.insert(name.clone()) {
            return Err("Unsafe ZIP path or duplicate entry".into());
        }
        if let Some(mode) = entry.unix_mode() {
            let kind = mode & 0o170000;
            if kind != 0 && kind != 0o100000 && kind != 0o040000 {
                return Err("ZIP links/special files are forbidden".into());
            }
        }
        total = total.checked_add(entry.size()).ok_or("ZIP size overflow")?;
        if total > BINARY_LIMIT as u64 {
            return Err("ZIP exceeds expanded size limit".into());
        }
        if !entry.is_dir()
            && Path::new(&name)
                .file_name()
                .is_some_and(|n| n == "tunnel-client")
        {
            if binary.is_some() {
                return Err("Ambiguous tunnel-client binary".into());
            }
            let mut data = Vec::new();
            entry
                .by_ref()
                .take(BINARY_LIMIT as u64 + 1)
                .read_to_end(&mut data)
                .map_err(|_| "ZIP CRC/read failure")?;
            if data.len() > BINARY_LIMIT {
                return Err("Binary exceeds size limit".into());
            }
            binary = Some(data);
        }
    }
    binary.ok_or_else(|| "Full tunnel-client binary missing from release".into())
}

pub(super) fn verify_arch(bytes: &[u8], target: &str) -> Result<()> {
    let good = match target {
        "darwin-amd64" => bytes.get(..8) == Some(&[0xcf, 0xfa, 0xed, 0xfe, 7, 0, 0, 1]),
        "darwin-arm64" => bytes.get(..8) == Some(&[0xcf, 0xfa, 0xed, 0xfe, 12, 0, 0, 1]),
        "linux-amd64" | "linux-arm64" => {
            bytes.get(..6) == Some(&[0x7f, b'E', b'L', b'F', 2, 1])
                && bytes.get(18..20)
                    == Some(if target == "linux-amd64" {
                        &[62, 0]
                    } else {
                        &[183, 0]
                    })
        }
        _ => false,
    };
    if good {
        Ok(())
    } else {
        Err("Tunnel client binary OS/architecture mismatch".into())
    }
}

pub(super) fn verify_installed(root: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let file = private::open(&root.join("tunnel-client"), true)?;
    if file
        .metadata()
        .map_err(|_| "Cannot inspect tunnel-client permissions")?
        .permissions()
        .mode()
        & 0o100
        == 0
    {
        return Err("Tunnel client is not executable; run zai chatgpt repair".into());
    }
    let mut bytes = Vec::new();
    file.take(BINARY_LIMIT as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "Cannot read installed tunnel-client")?;
    if bytes.len() > BINARY_LIMIT {
        return Err("Installed binary exceeds size limit".into());
    }
    verify_arch(
        &bytes,
        &platform(std::env::consts::OS, std::env::consts::ARCH)?,
    )?;
    let expected = private::read(&root.join("tunnel-client.sha256"), 128)?;
    verify_sha(
        &bytes,
        std::str::from_utf8(&expected).map_err(|_| "Invalid installed checksum")?,
    )
}

pub(super) fn install(root: &Path) -> Result<()> {
    if verify_installed(root).is_ok() {
        return Ok(());
    }
    let target = platform(std::env::consts::OS, std::env::consts::ARCH)?;
    let raw = download(
        &format!(
            "https://api.github.com/repos/openai/tunnel-client/releases/tags/v{TESTED_VERSION}"
        ),
        2 * 1024 * 1024,
    )?;
    let release: Release =
        serde_json::from_slice(&raw).map_err(|_| "Invalid official release metadata")?;
    if release.tag_name != format!("v{TESTED_VERSION}") || release.draft || release.prerelease {
        return Err("Unexpected release version/state".into());
    }
    let name = format!("tunnel-client-v{TESTED_VERSION}-{target}.zip");
    let sums = asset_bytes(select(&release, "SHA256SUMS.txt")?, 1024 * 1024)?;
    let sums = std::str::from_utf8(&sums).map_err(|_| "Invalid SHA256SUMS encoding")?;
    let archive = asset_bytes(select(&release, &name)?, DOWNLOAD_LIMIT)?;
    verify_sha(&archive, checksum(sums, &name)?)?;
    let bytes = extract(&archive)?;
    verify_arch(&bytes, &target)?;
    private::write(&root.join("tunnel-client"), &bytes, true)?;
    private::write(
        &root.join("tunnel-client.sha256"),
        sha256(&bytes).as_bytes(),
        false,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn zip_rejects_traversal_symlinks_and_ambiguous_binaries() {
        use std::io::Write;
        use zip::write::SimpleFileOptions;
        for names in [
            vec!["../tunnel-client"],
            vec!["/tunnel-client"],
            vec!["a/tunnel-client", "b/tunnel-client"],
            vec!["a\\tunnel-client"],
        ] {
            let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for name in names {
                writer
                    .start_file(name, SimpleFileOptions::default())
                    .unwrap();
                writer.write_all(b"binary").unwrap();
            }
            assert!(extract(&writer.finish().unwrap().into_inner()).is_err());
        }
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .add_symlink("tunnel-client", "/etc/passwd", SimpleFileOptions::default())
            .unwrap();
        assert!(extract(&writer.finish().unwrap().into_inner()).is_err());
        let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
        writer
            .start_file("release/tunnel-client", SimpleFileOptions::default())
            .unwrap();
        writer.write_all(b"verified binary").unwrap();
        assert_eq!(
            extract(&writer.finish().unwrap().into_inner()).unwrap(),
            b"verified binary"
        );
    }
    #[test]
    fn platforms_and_runtime_asset_rejection() {
        for (os, arch, expected) in [
            ("macos", "x86_64", "darwin-amd64"),
            ("macos", "aarch64", "darwin-arm64"),
            ("linux", "arm64", "linux-arm64"),
            ("linux", "x86_64", "linux-amd64"),
        ] {
            assert_eq!(platform(os, arch).unwrap(), expected);
        }
        assert!(platform("windows", "x86_64").is_err());
        assert!(platform("linux", "riscv64").is_err());
        let r = Release {
            tag_name: "v0.0.14".into(),
            draft: false,
            prerelease: false,
            assets: vec![Asset {
                name: "tunnel-client-runtime-cloudflared-v0.0.14-darwin-amd64.zip".into(),
                browser_download_url: String::new(),
                digest: None,
                size: 0,
            }],
        };
        assert!(select(&r, "tunnel-client-v0.0.14-darwin-amd64.zip").is_err());
    }
    #[test]
    fn checksum_and_arch_fail_closed() {
        let digest = sha256(b"verified");
        verify_sha(b"verified", &digest).unwrap();
        assert!(verify_sha(b"tampered", &digest).is_err());
        assert!(checksum("abcd  a.zip\nabcd  a.zip", "a.zip").is_err());
        assert!(checksum("abcd  b.zip", "a.zip").is_err());
        verify_arch(&[0xcf, 0xfa, 0xed, 0xfe, 7, 0, 0, 1], "darwin-amd64").unwrap();
        assert!(verify_arch(&[0xcf, 0xfa, 0xed, 0xfe, 7, 0, 0, 1], "darwin-arm64").is_err());
        assert!(verify_arch(b"#!/bin/sh", "linux-amd64").is_err());
    }
}
