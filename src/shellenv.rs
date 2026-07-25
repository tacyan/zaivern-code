//! 子プロセスへ渡す PATH の解決と、OS 非依存の `which`。
//!
//! # なぜ必要か
//!
//! GUI アプリはユーザーのログインシェルの PATH を受け取らない。
//! macOS で `.app` (Dock / Finder / Spotlight) から起動すると PATH は launchd の
//! `/usr/bin:/bin:/usr/sbin:/sbin` だけになり、`claude` も `codex` も見えない。
//!
//! これまでは `$SHELL -lc '<command>'` 越しに起動することで回避していたが、
//! **ログインシェルは対話用の rc ファイルを読まない**:
//!
//! | シェル | `-lc` が読むもの | 読まないもの |
//! |--------|------------------|--------------|
//! | zsh    | `.zshenv` / `.zprofile` / `.zlogin` | **`.zshrc`** |
//! | bash   | `.bash_profile` / `.profile` | **`.bashrc`** |
//!
//! Claude Code のネイティブ版 (`~/.local/bin`)、nvm / mise / asdf の node、
//! `npm -g` 済みの CLI — これらの PATH 追加はほぼ `.zshrc` / `.bashrc` に書かれる。
//! つまり `zsh -lc claude` は **command not found** になる。Mac でエージェントが
//! 起動できなかった原因はこれ。
//!
//! Windows 側にも裏返しの穴がある。`$SHELL` は存在しないので
//! `$SHELL -lc ...` に頼った検出 (エージェントピッカー等) は `/bin/sh` を
//! 起動しようとして必ず失敗し、「1 つもインストールされていない」ように見えていた。
//!
//! # 方針
//!
//! PATH の解決をここ 1 箇所に集め、**両 OS で同じ道**を通す:
//!
//! 1. 自プロセスの PATH (端末から起動したときはこれが正解)
//! 2. ログイン **かつ対話** シェルが持つ PATH (unix のみ。`.zshrc` 等を読ませる)
//! 3. よく使われるインストール先のうち実在するもの (シェルが使えなくても拾えるように)
//!
//! これを [`user_path`] が一度だけ組み立てて使い回す。子プロセスには
//! [`apply_path`] でこの PATH を渡し、存在確認はサブプロセスを起こさない
//! [`which`] で行う (PATH を自前で走査するので Windows でも同じに動く)。

use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// ログインシェルへの問い合わせを諦めるまでの時間 (Windows では使わない)。
///
/// rc ファイルが重いと 1 秒近くかかることがある一方、`-i` 付きのシェルは
/// 設定次第で戻らなくなりうる (プロンプトの外部コマンド待ちなど)。
/// 待ちすぎるとエージェント起動が固まるので、実測より十分長く・体感より短い所で切る。
#[allow(dead_code)] // Windows ではログインシェルへ問い合わせない
const SHELL_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 子プロセスへ渡す PATH。プロセス内で一度だけ解決してキャッシュする。
///
/// 初回の呼び出しだけログインシェルを起こす (最大 [`SHELL_PROBE_TIMEOUT`])。
/// UI スレッドから初めて呼ぶと最大その時間だけ止まるので、起動直後に
/// [`warm_up`] でワーカースレッドから温めておくこと。
pub fn user_path() -> &'static OsStr {
    static CACHE: OnceLock<OsString> = OnceLock::new();
    CACHE.get_or_init(resolve_path).as_os_str()
}

/// PATH の解決をバックグラウンドで済ませておく (結果は [`user_path`] が使い回す)。
pub fn warm_up() {
    std::thread::spawn(|| {
        let _ = user_path();
    });
}

/// 子プロセスに [`user_path`] を渡す。
///
/// ユーザーが PATH を明示している場合まで奪わないよう、**呼び出し側が
/// `cmd.env("PATH", ...)` で上書きできる順序で呼ぶこと** (先に当てる)。
pub fn apply_path(cmd: &mut Command) {
    cmd.env("PATH", user_path());
}

/// PATH 上の実行ファイルを探す (`which` / `where` 相当)。
///
/// サブプロセスを起こさないので毎フレーム呼んでも安全。Windows では
/// `PATHEXT` の拡張子 (`.CMD` / `.EXE` …) も試すため、`npm -g` が置く
/// `claude.cmd` のようなラッパーも見つかる。
pub fn which(bin: &str) -> Option<PathBuf> {
    if bin.is_empty() {
        return None;
    }
    // パスを直接渡された場合はそのまま扱う (PATH 走査の対象にしない)
    if bin.contains('/') || (cfg!(windows) && bin.contains('\\')) {
        let p = PathBuf::from(bin);
        return is_executable(&p).then_some(p);
    }
    lookup(bin, &path_dirs(user_path()), &exe_exts())
}

/// PATH 上にあるか (パスが要らない場合の薄い包み)。
pub fn has(bin: &str) -> bool {
    which(bin).is_some()
}

/// シェル経由でコマンド行を実行する [`Command`] を組む。
///
/// unix は `$SHELL -lc <script>`、Windows は `%COMSPEC% /C <script>`。
/// PATH は [`user_path`] を渡すので、ログインシェルが `.zshrc` を読まなくても
/// ユーザーの CLI が見つかる。
pub fn shell_command(script: &str) -> Command {
    let mut c = crate::procx::hidden_command(shell_program());
    for a in shell_args(script) {
        c.arg(a);
    }
    c
}

/// [`shell_command`] が使うシェルの実行ファイル。
pub fn shell_program() -> OsString {
    #[cfg(windows)]
    {
        std::env::var_os("COMSPEC").unwrap_or_else(|| OsString::from("cmd.exe"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("SHELL").unwrap_or_else(|| OsString::from("/bin/sh"))
    }
}

/// [`shell_command`] が渡す引数 (OS ごとのシェル呼び出し規約)。
fn shell_args(script: &str) -> Vec<String> {
    #[cfg(windows)]
    {
        vec!["/C".to_string(), script.to_string()]
    }
    #[cfg(not(windows))]
    {
        vec!["-lc".to_string(), script.to_string()]
    }
}

// ───────────────────────── PATH の組み立て ─────────────────────────

/// 実際に PATH を解決する ([`user_path`] の中身)。
fn resolve_path() -> OsString {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut push = |d: PathBuf, dirs: &mut Vec<PathBuf>| {
        if !d.as_os_str().is_empty() && seen.insert(d.clone()) {
            dirs.push(d);
        }
    };

    // 1. 自プロセスの PATH (端末から起動したときはこれが完全)
    if let Some(p) = std::env::var_os("PATH") {
        for d in path_dirs(&p) {
            push(d, &mut dirs);
        }
    }
    // 2. ログイン + 対話シェルの PATH (.zshrc / .bashrc の追加を拾う)
    for d in login_shell_path() {
        push(d, &mut dirs);
    }
    // 3. よくあるインストール先で、実在するもの
    for d in well_known_dirs() {
        if d.is_dir() {
            push(d, &mut dirs);
        }
    }
    join_dirs(dirs)
}

/// ディレクトリ列を PATH 文字列へ。区切り文字を含むディレクトリは
/// 結合できないので、そこだけ落として残りを活かす
/// (1 つの変なパスで PATH 全体を失わないため)。
fn join_dirs(dirs: Vec<PathBuf>) -> OsString {
    if let Ok(joined) = std::env::join_paths(&dirs) {
        return joined;
    }
    let safe: Vec<PathBuf> = dirs
        .into_iter()
        .filter(|d| std::env::join_paths(std::iter::once(d)).is_ok())
        .collect();
    std::env::join_paths(safe).unwrap_or_else(|_| std::env::var_os("PATH").unwrap_or_default())
}

/// PATH 文字列をディレクトリへ割る。
fn path_dirs(path: &OsStr) -> Vec<PathBuf> {
    std::env::split_paths(path).collect()
}

/// ログイン + 対話シェルが持つ PATH を取り出す (unix のみ。失敗したら空)。
#[cfg(not(windows))]
fn login_shell_path() -> Vec<PathBuf> {
    let shell = shell_program();
    // `-i` まで付けるのが要点: zsh の `.zshrc` / bash の `.bashrc` は対話シェルでしか
    // 読まれず、nvm・mise・Claude Code のネイティブ版の PATH 追加はそこに書かれる。
    //
    // 取り出しに `/usr/bin/env` を使うのは、シェルによる違いを踏まないため。
    // fish では `"$PATH"` が空白区切りに展開されてしまい `printf` では壊れる。
    // env の出力ならどのシェルでも「PATH=<コロン区切り>」の 1 行になる。
    let mut cmd = crate::procx::hidden_command_raw(&shell);
    cmd.arg("-i").arg("-l").arg("-c").arg("/usr/bin/env");
    let out = match capture_stdout(cmd, SHELL_PROBE_TIMEOUT) {
        Some(s) => s,
        // 対話シェルは設定次第で失敗しうる。ログインシェルだけでもう一度試す。
        None => {
            let mut cmd = crate::procx::hidden_command_raw(&shell);
            cmd.arg("-lc").arg("/usr/bin/env");
            capture_stdout(cmd, SHELL_PROBE_TIMEOUT).unwrap_or_default()
        }
    };
    parse_env_path(&out)
        .map(|p| path_dirs(OsStr::new(&p)))
        .unwrap_or_default()
}

/// Windows にログインシェルの概念は無い (`$SHELL` も無い)。
#[cfg(windows)]
fn login_shell_path() -> Vec<PathBuf> {
    Vec::new()
}

/// `env` の出力から PATH の値を取り出す。
///
/// rc ファイルが何か表示しても、`PATH=` で始まる行だけを見るので巻き込まれない。
#[allow(dead_code)] // 呼ぶのは unix の login_shell_path (と、両 OS のテスト)
fn parse_env_path(env_out: &str) -> Option<String> {
    env_out
        .lines()
        .find_map(|l| l.strip_prefix("PATH="))
        .map(|v| v.trim_end_matches('\r').to_string())
        .filter(|v| !v.is_empty())
}

/// 子プロセスの stdout を、時間を区切って読む。時間切れなら kill して `None`。
///
/// 読み取りを別スレッドへ出すのは、パイプが詰まったまま `wait` すると
/// 相手が終われず永久に待つため (`output()` が使えない理由)。
#[allow(dead_code)] // 呼ぶのは unix の login_shell_path
fn capture_stdout(mut cmd: Command, timeout: std::time::Duration) -> Option<String> {
    use std::io::Read;
    use std::process::Stdio;
    use std::sync::mpsc;

    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut s = String::new();
        let _ = stdout.read_to_string(&mut s);
        let _ = tx.send(s);
    });
    match rx.recv_timeout(timeout) {
        Ok(s) => {
            let _ = child.wait();
            Some(s)
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            None
        }
    }
}

/// よくあるインストール先の候補 (実在確認は呼び出し側)。
///
/// ログインシェルへの問い合わせが使えない・失敗した環境でも、代表的な
/// パッケージマネージャで入れた CLI は拾えるようにするための保険。
fn well_known_dirs() -> Vec<PathBuf> {
    let home = dirs::home_dir();
    let mut out: Vec<PathBuf> = Vec::new();
    // `/` 区切りで書いた相対パスをホーム基準の実パスへ (Windows では `\` になる)
    let at_home = |rel: &str, out: &mut Vec<PathBuf>| {
        if let Some(h) = &home {
            out.push(rel.split('/').fold(h.clone(), |p, seg| p.join(seg)));
        }
    };

    // どの OS でも同じ場所に入るもの (公式インストーラ / rustup / bun / deno …)
    for rel in [
        ".local/bin",   // Claude Code ネイティブ版 / uv / pipx
        ".claude/local", // Claude Code のローカルインストール
        ".cargo/bin",
        ".bun/bin",
        ".deno/bin",
        ".volta/bin",
        "go/bin",
    ] {
        at_home(rel, &mut out);
    }

    #[cfg(not(windows))]
    {
        for rel in [
            ".npm-global/bin",
            ".npm-packages/bin",
            ".yarn/bin",
            ".config/yarn/global/node_modules/.bin",
            "Library/pnpm",           // macOS の pnpm
            ".local/share/pnpm",      // Linux の pnpm
            ".asdf/shims",
            ".local/share/mise/shims",
        ] {
            at_home(rel, &mut out);
        }
        out.extend(
            [
                "/opt/homebrew/bin", // Apple Silicon の Homebrew
                "/opt/homebrew/sbin",
                "/usr/local/bin", // Intel Mac の Homebrew / npm -g
                "/usr/local/sbin",
                "/home/linuxbrew/.linuxbrew/bin",
                "/snap/bin",
                "/usr/bin",
                "/bin",
                "/usr/sbin",
                "/sbin",
            ]
            .iter()
            .map(PathBuf::from),
        );
        // nvm は「今どのバージョンか」を .zshrc 側で決めるため、ここでは
        // 入っているバージョンを全部足しておく (which が実体のある方を拾う)。
        if let Some(h) = &home {
            out.extend(node_version_bins(&h.join(".nvm/versions/node")));
        }
    }

    #[cfg(windows)]
    {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            out.push(PathBuf::from(&appdata).join("npm")); // npm -g のラッパー (.cmd)
        }
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let local = PathBuf::from(&local);
            out.push(local.join(r"Microsoft\WindowsApps"));
            out.push(local.join(r"Microsoft\WinGet\Links"));
        }
        at_home("scoop/shims", &mut out);
        for k in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Some(pf) = std::env::var_os(k) {
                let pf = PathBuf::from(&pf);
                out.push(pf.join("nodejs"));
                out.push(pf.join("Git").join("cmd"));
                out.push(pf.join("GitHub CLI"));
            }
        }
    }
    out
}

/// `<nvm>/versions/node/*/bin` を新しいバージョンから順に並べる。
#[cfg(not(windows))]
fn node_version_bins(versions_dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(versions_dir) else {
        return Vec::new();
    };
    let mut names: Vec<PathBuf> = rd.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    names.sort();
    names.reverse();
    names.into_iter().map(|p| p.join("bin")).collect()
}

// ───────────────────────── which の中身 ─────────────────────────

/// 実行ファイルとして試す拡張子。unix は拡張子なしのみ。
///
/// Windows では **PATHEXT の拡張子を先に試し、拡張子なしは最後**にする。
/// `npm -g` は `claude` (sh スクリプト) と `claude.cmd` (Windows 用ラッパー) を
/// 同じフォルダへ置くので、拡張子なしを先に拾うと Windows では起動できない方を
/// 掴んでしまう。
fn exe_exts() -> Vec<String> {
    #[cfg(windows)]
    {
        let raw = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into());
        let mut v: Vec<String> = raw
            .split(';')
            .map(|e| e.trim().to_string())
            .filter(|e| !e.is_empty())
            .collect();
        // 拡張子込みで指定された場合 (`foo.exe`) の受け皿
        v.push(String::new());
        v
    }
    #[cfg(not(windows))]
    {
        vec![String::new()]
    }
}

/// `dirs` を順に見て、最初に見つかった実行ファイルを返す。
fn lookup(bin: &str, dirs: &[PathBuf], exts: &[String]) -> Option<PathBuf> {
    for d in dirs {
        for ext in exts {
            let cand = d.join(format!("{bin}{ext}"));
            if is_executable(&cand) {
                return Some(cand);
            }
        }
    }
    None
}

/// 実行できるファイルか。unix では実行ビットまで見る
/// (`~/.local/bin` に置かれた読み取り専用のゴミを拾わないため)。
fn is_executable(p: &Path) -> bool {
    let Ok(md) = p.metadata() else {
        return false;
    };
    if !md.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        md.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_env_path_picks_only_the_path_line() {
        let out = "SHELL=/bin/zsh\nPATH=/usr/bin:/opt/homebrew/bin\nHOME=/Users/me\n";
        assert_eq!(
            parse_env_path(out).as_deref(),
            Some("/usr/bin:/opt/homebrew/bin")
        );
        // rc ファイルがバナーを出しても巻き込まれない
        let noisy = "ようこそ!\n\nPATH=/a:/b\n";
        assert_eq!(parse_env_path(noisy).as_deref(), Some("/a:/b"));
        // PATH が無い / 空なら None (呼び出し側は無視して続ける)
        assert_eq!(parse_env_path("HOME=/Users/me\n"), None);
        assert_eq!(parse_env_path("PATH=\n"), None);
        // CRLF の端末から来ても値に \r を混ぜない
        assert_eq!(parse_env_path("PATH=/a:/b\r\n").as_deref(), Some("/a:/b"));
    }

    /// DoD: 解決した PATH には、実在するよく使うインストール先が含まれる。
    /// ここが空だと Mac の `.app` 起動でエージェントが command not found になる。
    #[test]
    fn user_path_includes_existing_well_known_dirs() {
        let dirs = path_dirs(user_path());
        assert!(!dirs.is_empty(), "PATH が空になっている");
        for d in well_known_dirs().into_iter().filter(|d| d.is_dir()) {
            assert!(dirs.contains(&d), "{} が PATH に入っていない", d.display());
        }
        // 自プロセスの PATH は必ず残す (端末から起動したときの正解を捨てない)
        if let Some(p) = std::env::var_os("PATH") {
            for d in path_dirs(&p).into_iter().filter(|d| !d.as_os_str().is_empty()) {
                assert!(dirs.contains(&d), "{} が落ちている", d.display());
            }
        }
    }

    /// DoD: 同じディレクトリを二度並べない (PATH が起動のたびに伸びない)。
    #[test]
    fn user_path_has_no_duplicates() {
        let dirs = path_dirs(user_path());
        let uniq: HashSet<&PathBuf> = dirs.iter().collect();
        assert_eq!(uniq.len(), dirs.len(), "PATH に重複がある: {dirs:?}");
    }

    /// DoD: which はサブプロセス無しで、どの OS でも実体を見つける。
    #[test]
    fn which_finds_a_real_executable() {
        // どの OS にもある実行ファイルで確認する
        #[cfg(windows)]
        let (present, absent) = ("cmd", "zaivern-no-such-binary");
        #[cfg(not(windows))]
        let (present, absent) = ("sh", "zaivern-no-such-binary");
        let found = which(present).unwrap_or_else(|| panic!("{present} が見つからない"));
        assert!(found.is_file(), "{} は実体を指す", found.display());
        assert!(which(absent).is_none(), "無いものを見つけてはいけない");
        assert!(which("").is_none(), "空文字は常に None");
        assert!(has(present));
    }

    /// Windows の `npm -g` は `claude.cmd` のようなラッパーを置くので、
    /// PATHEXT の拡張子まで試さないと「未インストール」に見えてしまう。
    #[test]
    fn lookup_tries_pathext_on_windows() {
        let dir = crate::test_util::unique_temp_dir("zaivern-shellenv-test", "pathext");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let wrapper = dir.join(if cfg!(windows) { "faux.cmd" } else { "faux" });
        std::fs::write(&wrapper, "echo hi").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper, std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        // npm -g と同じ配置: 拡張子なし (sh スクリプト) と .cmd が同居する。
        // Windows では .cmd を選ばないと起動できない。
        std::fs::write(dir.join("faux"), "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir.join("faux"), std::fs::Permissions::from_mode(0o755))
                .expect("chmod");
        }
        let exts = exe_exts();
        let got = lookup("faux", std::slice::from_ref(&dir), &exts).expect("見つからない");
        // Windows の PATHEXT は大文字 (`.CMD`) なので、拡張子の大小は問わない
        // (ファイルシステムが大小を区別しないため、そのまま起動できる)。
        assert!(
            got.to_string_lossy()
                .eq_ignore_ascii_case(&wrapper.to_string_lossy()),
            "{} を指していない: {}",
            wrapper.display(),
            got.display()
        );
        assert_eq!(lookup("faux2", std::slice::from_ref(&dir), &exts), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// パスを直接渡されたら PATH 走査はしない (相対パスのプラグイン等)。
    #[test]
    fn which_accepts_an_explicit_path() {
        let dir = crate::test_util::unique_temp_dir("zaivern-shellenv-test", "explicit");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let f = dir.join("tool");
        std::fs::write(&f, "#!/bin/sh\n").expect("write");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&f, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        }
        let spec = f.to_string_lossy().replace('\\', "/");
        assert_eq!(which(&spec), Some(PathBuf::from(&spec)));
        assert!(which(&format!("{spec}-gone")).is_none());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// DoD: シェル呼び出しの規約が OS ごとに正しい
    /// (Windows で `-lc` を渡すと cmd.exe が引数として飲み込んで何も実行しない)。
    #[test]
    fn shell_args_match_the_platform() {
        let a = shell_args("echo hi");
        assert_eq!(a.len(), 2);
        if cfg!(windows) {
            assert_eq!(a[0], "/C");
        } else {
            assert_eq!(a[0], "-lc");
        }
        assert_eq!(a[1], "echo hi");
    }

    /// DoD: shell_command は両 OS で実際にコマンドを走らせられる。
    #[test]
    fn shell_command_runs_on_this_platform() {
        let out = shell_command("echo zaivern-ok").output().expect("起動できる");
        assert!(String::from_utf8_lossy(&out.stdout).contains("zaivern-ok"));
    }
}
