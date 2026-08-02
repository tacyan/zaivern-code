//! パス正規化の共通ヘルパー。
//!
//! Windows の [`Path::canonicalize`] は `\\?\C:\...` (verbatim 形式) を返す。
//! アプリ内で持ち回るだけなら無害だが、**子プロセスの作業ディレクトリ**に
//! 渡すと壊れる: `cmd.exe` は verbatim / UNC のカレントディレクトリを受け付けず、
//!
//! ```text
//! '\\?\C:\Users\me\proj'
//! CMD.EXE was started with the above path as the current directory.
//! UNC paths are not supported.  Defaulting to Windows directory.
//! ```
//!
//! と言って `C:\Windows` へ落ちる。つまり `zai .` で開いたフォルダで動くはずの
//! エージェントが `C:\Windows` で起動してしまう (端末は `cmd.exe /C <command>`
//! 経由で起動するため、この経路をすべて通る)。
//!
//! そこでアプリが保持するパスは最初から素の形 (`C:\...`) に揃える。
//! canonicalize の目的 (シンボリックリンク差と `..` の吸収) は接頭辞を外しても
//! 失われない。macOS / Linux では canonicalize と同じ挙動になる。

use std::path::{Path, PathBuf};

/// Windows の canonicalize が付ける `\\?\` 接頭辞を外した素のパス。
/// 接頭辞が無ければそのまま返す (macOS / Linux では常に素通し)。
///
/// - `\\?\C:\a` → `C:\a`
/// - `\\?\UNC\srv\share\a` → `\\srv\share\a` (ネットワークパスの素の形)
/// - `\\?\Volume{…}\a` → そのまま (ドライブ文字が無く、外すと解決できなくなる)
pub fn plain(p: PathBuf) -> PathBuf {
    match plain_str(&p.to_string_lossy()) {
        Some(s) => PathBuf::from(s),
        None => p,
    }
}

/// [`plain`] の文字列版。変換が不要な入力には `None` を返す。
///
/// 文字列処理だけを切り出してあるのは、OS を問わずテストできるようにするため
/// (Windows 以外でも `\\?\` 付きの入力を与えて挙動を確かめられる)。
fn plain_str(s: &str) -> Option<String> {
    let rest = s.strip_prefix(r"\\?\")?;
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    // `C:` で始まるものだけ素に戻す。`Volume{GUID}` 形式は接頭辞込みでしか
    // 解決できないので触らない。
    let b = rest.as_bytes();
    let has_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    has_drive.then(|| rest.to_string())
}

/// canonicalize してから [`plain`] を当てる。
/// 解決できないパス (存在しない等) は入力のまま返す。
pub fn canonical(p: &Path) -> PathBuf {
    match p.canonicalize() {
        Ok(c) => plain(c),
        Err(_) => p.to_path_buf(),
    }
}

/// 子プロセス (PTY / 外部コマンド) の作業ディレクトリとして安全なパス。
///
/// verbatim 接頭辞を外し、ディレクトリとして実在することまで確かめる。
/// 実在しなければホーム → 一時ディレクトリへ落とす: 存在しない cwd は spawn
/// 自体の失敗になり「エージェントが起動しない」形で表に出るため、
/// 起動できる場所へ寄せてから渡す。
pub fn launch_dir(p: &Path) -> PathBuf {
    let plain_p = plain(p.to_path_buf());
    if plain_p.is_dir() {
        return plain_p;
    }
    if let Some(home) = dirs::home_dir().map(plain).filter(|h| h.is_dir()) {
        return home;
    }
    std::env::temp_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_strips_verbatim_drive_prefix() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\C:\Users\me\proj")),
            PathBuf::from(r"C:\Users\me\proj")
        );
        // 小文字ドライブ / ドライブ直下も同じ
        assert_eq!(plain(PathBuf::from(r"\\?\d:\")), PathBuf::from(r"d:\"));
    }

    #[test]
    fn plain_converts_verbatim_unc_to_plain_unc() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\UNC\srv\share\proj")),
            PathBuf::from(r"\\srv\share\proj")
        );
    }

    #[test]
    fn plain_leaves_untouchable_paths_alone() {
        // ドライブ文字を持たない verbatim パスは外すと解決できなくなる
        let vol = PathBuf::from(r"\\?\Volume{9f8a}\proj");
        assert_eq!(plain(vol.clone()), vol);
        // 接頭辞が無いパス (Windows / POSIX どちらも) は素通し
        for p in [r"C:\Users\me", r"\\srv\share", "/home/me/proj", "rel/ative"] {
            assert_eq!(plain(PathBuf::from(p)), PathBuf::from(p), "{p}");
        }
    }

    #[test]
    fn canonical_never_returns_a_verbatim_path() {
        let dir = crate::test_util::unique_temp_dir("zaivern-pathx-test", "canon");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let c = canonical(&dir);
        assert!(
            !c.to_string_lossy().starts_with(r"\\?\"),
            "canonical は素のパスを返す: {}",
            c.display()
        );
        assert!(c.is_dir(), "指しているものは変わらない: {}", c.display());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn canonical_keeps_unresolvable_input_as_is() {
        let ghost = PathBuf::from("no/such/dir-for-zaivern-pathx");
        assert_eq!(canonical(&ghost), ghost);
    }

    #[test]
    fn launch_dir_returns_an_existing_directory() {
        let dir = crate::test_util::unique_temp_dir("zaivern-pathx-test", "launch");
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(launch_dir(&dir), dir, "実在するディレクトリはそのまま");

        // 消えたフォルダを cwd にしようとしても、起動できる場所へ落ちる
        let ghost = dir.join("gone");
        let fallback = launch_dir(&ghost);
        assert!(fallback.is_dir(), "{} は実在すべき", fallback.display());
        assert_ne!(fallback, ghost);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// 本命の回帰: PTY へ渡す cwd に `\\?\` が残っていると cmd.exe が
    /// `C:\Windows` へ落ちるので、実在チェックの前に必ず素へ戻す。
    #[cfg(windows)]
    #[test]
    fn launch_dir_strips_verbatim_prefix_from_real_dir() {
        let dir = crate::test_util::unique_temp_dir("zaivern-pathx-test", "verbatim");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let verbatim = PathBuf::from(format!(r"\\?\{}", plain(canonical(&dir)).display()));
        let got = launch_dir(&verbatim);
        assert!(
            !got.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            got.display()
        );
        assert!(got.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }
}
