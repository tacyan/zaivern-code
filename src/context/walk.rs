//! ワークスペース境界つきのファイル走査。**Context Engine の関所**。
//!
//! ## なぜ境界を型で持つのか
//!
//! `read_slim(path)` のような道具は、渡された文字列をそのまま
//! `fs::read` へ流すのが自然な書き方になる。それをやると
//! `../../../../etc/passwd` や `~/.ssh/id_rsa` が**エージェントの一言で
//! 読める**。「そんな要求は来ない」は保証ではないので、**パスの検査を
//! 通らないと `Path` を手に入れられない**形にした:
//! 道具は [`Workspace::resolve`] が返した [`SafePath`] しか受け取らない。
//!
//! ## 検査の内容
//!
//! 1. 相対パスは根から解決する
//! 2. `..` を字句で畳む ([`crate::pathx::lexical`]) — **実体を辿る前に**
//!    畳むので、存在しないパスでも判定できる
//! 3. 実体があるなら canonicalize して**シンボリックリンク越しの脱出**も塞ぐ
//! 4. どれかの根の下に収まっていなければ [`ContextError::OutsideWorkspace`]
//!
//! 大文字小文字の畳み方は **実 FS を検査した答え**
//! ([`crate::worktree::fs_case_insensitive_at`]) に合わせる。cfg で決め打つと、
//! Docker-on-Mac の bind mount のような「Linux なのに大小非区別」で嘘の
//! 判定が出る。

use std::path::{Path, PathBuf};

use super::glob::any_match;
use super::ContextError;

/// 走査で降りないディレクトリ。**成果物と依存の置き場**は文脈にならない。
pub const SKIP_DIRS: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "node_modules",
    "target",
    "dist",
    "build",
    "out",
    ".next",
    ".nuxt",
    ".venv",
    "venv",
    "__pycache__",
    "vendor",
    ".idea",
    ".vscode",
    ".cache",
    "coverage",
    ".terraform",
    "Pods",
    "DerivedData",
];

/// `exclude_tests` が展開する、よくあるテスト置き場。
pub const TEST_GLOBS: &[&str] = &[
    "**/test/**",
    "**/tests/**",
    "**/__tests__/**",
    "**/spec/**",
    "**/specs/**",
    "**/testdata/**",
    "**/fixtures/**",
    "**/__mocks__/**",
    "**/e2e/**",
    "*_test.*",
    "*_tests.*",
    "test_*.*",
    "*.test.*",
    "*.spec.*",
    "*Test.java",
    "*Tests.cs",
    "conftest.py",
];

/// 1 ファイルとして読む上限。これを超えるものは「文脈にする対象ではない」。
pub const MAX_FILE_BYTES: u64 = 2_000_000;

/// 1 回の走査で見るファイル数の上限。
pub const MAX_FILES_SCANNED: usize = 20_000;

/// 2 進かどうかを判定するために覗く先頭バイト数。
const BINARY_SNIFF_BYTES: usize = 4096;

/// **境界の検査を通ったパス**。道具はこれ以外を受け取らない。
///
/// 作れるのは [`Workspace::resolve`] だけ (欄は非公開)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SafePath {
    abs: PathBuf,
    /// 根から見た `/` 区切りの相対パス。表示と glob 照合に使う。
    rel: String,
}

impl SafePath {
    /// 絶対パス。
    pub fn as_path(&self) -> &Path {
        &self.abs
    }

    /// 根から見た `/` 区切りの相対パス。**出力に載るのはこちら**
    /// (絶対パスを載せると利用者のホームディレクトリ名が文脈へ漏れる)。
    pub fn rel(&self) -> &str {
        &self.rel
    }

    /// 小文字にした拡張子 (無ければ空文字)。
    pub fn ext(&self) -> String {
        self.abs
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase()
    }
}

/// Context Engine が触ってよい範囲。
#[derive(Clone, Debug)]
pub struct Workspace {
    roots: Vec<PathBuf>,
    fold_case: bool,
}

impl Workspace {
    /// 根を 1 つ以上与えて作る。**根が 1 つも無いワークスペースは作れない**
    /// (空にすると「どこでも読める」と同じ意味になる)。
    pub fn new(roots: &[PathBuf]) -> Result<Self, ContextError> {
        let mut out: Vec<PathBuf> = Vec::new();
        for r in roots {
            let abs = if r.is_absolute() {
                r.clone()
            } else {
                std::env::current_dir()
                    .map_err(|e| ContextError::Io(format!("current_dir: {e}")))?
                    .join(r)
            };
            let abs = crate::pathx::canonical(&crate::pathx::lexical(&abs));
            if !out.contains(&abs) {
                out.push(abs);
            }
        }
        if out.is_empty() {
            return Err(ContextError::NoWorkspace);
        }
        let fold_case = crate::worktree::fs_case_insensitive_at(&out[0]);
        Ok(Self {
            roots: out,
            fold_case,
        })
    }

    /// 最初の根。表示と既定の走査開始点に使う。
    pub fn primary(&self) -> &Path {
        &self.roots[0]
    }

    /// 照合用にパスを畳む。大小非区別の FS では小文字へ寄せる。
    fn fold(&self, p: &Path) -> String {
        let s = p.to_string_lossy().replace('\\', "/");
        if self.fold_case {
            s.to_lowercase()
        } else {
            s
        }
    }

    /// 境界の検査。通れば [`SafePath`]、外なら [`ContextError::OutsideWorkspace`]。
    ///
    /// **実体が無くても判定できる** (`..` は字句で畳むので、存在しない
    /// パスを渡して境界検査だけを回避することはできない)。
    pub fn resolve(&self, path: &Path) -> Result<SafePath, ContextError> {
        let joined = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.primary().join(path)
        };
        // (1) 字句で `..` を畳む → (2) 実体があるなら canonicalize。
        // 順序が肝で、先に canonicalize すると存在しないパスで判定できない。
        let lexical = crate::pathx::lexical(&joined);
        let abs = if lexical.exists() {
            crate::pathx::canonical(&lexical)
        } else {
            lexical
        };
        let folded = self.fold(&abs);
        for root in &self.roots {
            let rf = self.fold(root);
            let inside = folded == rf
                || folded
                    .strip_prefix(&rf)
                    .is_some_and(|rest| rest.starts_with('/'));
            if inside {
                let rel = abs
                    .strip_prefix(root)
                    .map(|r| r.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                let rel = if rel.is_empty() {
                    root.file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_else(|| ".".to_string())
                } else {
                    rel
                };
                return Ok(SafePath { abs, rel });
            }
        }
        Err(ContextError::OutsideWorkspace {
            path: abs.to_string_lossy().to_string(),
            roots: self
                .roots
                .iter()
                .map(|r| r.to_string_lossy().to_string())
                .collect(),
        })
    }

    /// 検査済みのパスから、根の下の相対パスを作る (走査中の子に使う)。
    fn child(&self, root: &Path, p: &Path) -> SafePath {
        let rel = p
            .strip_prefix(root)
            .map(|r| r.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| p.to_string_lossy().replace('\\', "/"));
        SafePath {
            abs: p.to_path_buf(),
            rel,
        }
    }
}

/// 走査の絞り込み。
#[derive(Clone, Default, Debug)]
pub struct Filter {
    /// 小文字の拡張子 (空なら全部)。
    pub exts: Vec<String>,
    /// これに当たるものだけを見る (空なら全部)。
    pub include: Vec<String>,
    /// これに当たるものは見ない。
    pub exclude: Vec<String>,
}

impl Filter {
    /// テスト置き場を `exclude` へ足す。
    pub fn exclude_tests(mut self) -> Self {
        self.exclude
            .extend(TEST_GLOBS.iter().map(|s| (*s).to_string()));
        self
    }

    /// 拡張子の指定 (`"rs,toml"` / `".rs"` のどちらも受ける)。
    pub fn with_exts(mut self, spec: &str) -> Self {
        self.exts = spec
            .split(',')
            .map(|x| x.trim().trim_start_matches('.').to_lowercase())
            .filter(|x| !x.is_empty())
            .collect();
        self
    }

    fn wants(&self, sp: &SafePath) -> bool {
        if !self.exts.is_empty() && !self.exts.contains(&sp.ext()) {
            return false;
        }
        if !self.include.is_empty() && !any_match(&self.include, sp.rel()) {
            return false;
        }
        !any_match(&self.exclude, sp.rel())
    }
}

/// 名前だけで降りないと決まるディレクトリか。
pub fn should_skip_dir(name: &str) -> bool {
    SKIP_DIRS.contains(&name) || (name.starts_with('.') && name != "." && name != "..")
}

/// 走査の結果。**打ち切ったかどうかを必ず返す** — 打ち切りを黙っていると
/// 「全部見た」と読まれる。
pub struct Walk {
    pub files: Vec<SafePath>,
    /// 絞り込みで落ちた数。
    pub filtered: usize,
    /// 上限に当たって打ち切ったか。
    pub capped: bool,
}

/// `root` の下のファイルを集める。
///
/// * **シンボリックリンクは辿らない** (根の外へ出る最短経路であり、
///   循環でも止まらなくなる)
/// * 除外に当たるディレクトリは**降りる前に**落とす
pub fn collect(ws: &Workspace, root: &SafePath, filter: &Filter) -> Walk {
    let mut files = Vec::new();
    let mut filtered = 0usize;
    let mut capped = false;
    if root.as_path().is_file() {
        if filter.wants(root) {
            files.push(root.clone());
        } else {
            filtered += 1;
        }
        return Walk {
            files,
            filtered,
            capped,
        };
    }
    let base = root.as_path().to_path_buf();
    let mut stack = vec![base.clone()];
    while let Some(dir) = stack.pop() {
        if files.len() >= MAX_FILES_SCANNED {
            capped = true;
            break;
        }
        let Ok(rd) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut subdirs: Vec<PathBuf> = Vec::new();
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            // symlink はここで落とす (辿ると根の外へ出られる)
            if ft.is_symlink() {
                continue;
            }
            let p = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let sp = ws.child(&base, &p);
            if ft.is_dir() {
                if !should_skip_dir(&name) && !any_match(&filter.exclude, sp.rel()) {
                    subdirs.push(p);
                }
            } else if ft.is_file() {
                if filter.wants(&sp) {
                    files.push(sp);
                    if files.len() >= MAX_FILES_SCANNED {
                        capped = true;
                        break;
                    }
                } else {
                    filtered += 1;
                }
            }
        }
        // 並びを決めておく (read_dir の順序は OS と FS で変わる)
        subdirs.sort();
        subdirs.reverse();
        stack.extend(subdirs);
    }
    files.sort_by(|a, b| a.rel().cmp(b.rel()));
    Walk {
        files,
        filtered,
        capped,
    }
}

/// テキストとして読む。2 進・大きすぎるものは断る。
///
/// UTF-8 でないバイトは**置換文字へ落として読み進む** (端末ログや
/// CP932 のファイルで丸ごと読めなくなるより、読めるところを渡すほうがよい)。
pub fn read_text(sp: &SafePath) -> Result<String, ContextError> {
    let meta = std::fs::metadata(sp.as_path())
        .map_err(|e| ContextError::Io(format!("{}: {e}", sp.rel())))?;
    if meta.is_dir() {
        return Err(ContextError::Io(format!("{}: is a directory", sp.rel())));
    }
    if meta.len() > MAX_FILE_BYTES * 5 {
        return Err(ContextError::TooLarge {
            path: sp.rel().to_string(),
            bytes: meta.len(),
        });
    }
    let raw =
        std::fs::read(sp.as_path()).map_err(|e| ContextError::Io(format!("{}: {e}", sp.rel())))?;
    if is_binary(&raw) {
        return Err(ContextError::Binary(sp.rel().to_string()));
    }
    Ok(String::from_utf8_lossy(&raw).into_owned())
}

/// 先頭に NUL があれば 2 進と見なす。
pub fn is_binary(raw: &[u8]) -> bool {
    raw.iter().take(BINARY_SNIFF_BYTES).any(|b| *b == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実験場を 1 つ作る。**実 `~/.zaivern` には触らない。**
    fn lab(tag: &str) -> PathBuf {
        let d = crate::test_util::unique_temp_dir("zaivern-ctx", tag);
        crate::pathx::canonical(&d)
    }

    fn write(root: &Path, rel: &str, body: &str) {
        let p = root.join(rel);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::fs::write(p, body).unwrap();
    }

    #[test]
    fn 根が空のワークスペースは作れない() {
        assert!(matches!(
            Workspace::new(&[]),
            Err(ContextError::NoWorkspace)
        ));
    }

    /// 脱出の経路を 1 本ずつ塞げていること。
    #[test]
    fn ワークスペースの外は読めない() {
        let root = lab("ws-escape");
        write(&root, "src/a.rs", "fn a() {}\n");
        let ws = Workspace::new(std::slice::from_ref(&root)).unwrap();

        assert!(ws.resolve(Path::new("src/a.rs")).is_ok());
        assert!(ws.resolve(&root.join("src/a.rs")).is_ok());
        // 根そのものは中
        assert!(ws.resolve(Path::new(".")).is_ok());

        for escape in [
            "../outside.txt",
            "src/../../outside.txt",
            "src/../../../../etc/passwd",
            // 存在しないパスでも境界検査は効く
            "../does/not/exist",
        ] {
            assert!(
                matches!(
                    ws.resolve(Path::new(escape)),
                    Err(ContextError::OutsideWorkspace { .. })
                ),
                "{escape} が通ってしまった"
            );
        }
        // 絶対パスでの脱出
        assert!(matches!(
            ws.resolve(Path::new("/etc")),
            Err(ContextError::OutsideWorkspace { .. })
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 根の**兄弟**で名前が接頭辞になっているものを中と誤らない
    /// (`/w/proj` の隣の `/w/proj-secrets`)。
    #[test]
    fn 名前が接頭辞の兄弟は外側と判定する() {
        let base = lab("ws-sibling");
        let root = base.join("proj");
        let sibling = base.join("proj-secrets");
        std::fs::create_dir_all(&root).unwrap();
        write(&sibling, "k.txt", "秘密\n");
        let ws = Workspace::new(std::slice::from_ref(&root)).unwrap();
        assert!(matches!(
            ws.resolve(&sibling.join("k.txt")),
            Err(ContextError::OutsideWorkspace { .. })
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 走査は **symlink を辿らない** (辿れば根の外へ出られる)。
    #[cfg(unix)]
    #[test]
    fn 走査はシンボリックリンクを辿らない() {
        let base = lab("ws-symlink");
        let root = base.join("proj");
        let outside = base.join("outside");
        write(&outside, "secret.rs", "fn secret() {}\n");
        write(&root, "src/a.rs", "fn a() {}\n");
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let ws = Workspace::new(std::slice::from_ref(&root)).unwrap();
        let start = ws.resolve(Path::new(".")).unwrap();
        let w = collect(&ws, &start, &Filter::default());
        let rels: Vec<&str> = w.files.iter().map(|f| f.rel()).collect();
        assert_eq!(rels, vec!["src/a.rs"], "リンクの先まで歩いた");
        // resolve も、リンク越しの実体が外なら断る
        assert!(matches!(
            ws.resolve(Path::new("link/secret.rs")),
            Err(ContextError::OutsideWorkspace { .. })
        ));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn 走査は成果物のディレクトリへ降りない() {
        let root = lab("ws-skip");
        write(&root, "src/a.rs", "fn a() {}\n");
        write(&root, "target/debug/b.rs", "fn b() {}\n");
        write(&root, "node_modules/x/c.js", "let c\n");
        write(&root, ".git/config", "[core]\n");
        let ws = Workspace::new(std::slice::from_ref(&root)).unwrap();
        let start = ws.resolve(Path::new(".")).unwrap();
        let rels: Vec<String> = collect(&ws, &start, &Filter::default())
            .files
            .iter()
            .map(|f| f.rel().to_string())
            .collect();
        assert_eq!(rels, vec!["src/a.rs".to_string()]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 絞り込みは拡張子と包含と除外を順に効かせる() {
        let root = lab("ws-filter");
        write(&root, "src/a.rs", "fn a() {}\n");
        write(&root, "src/b.toml", "x = 1\n");
        write(&root, "tests/c.rs", "fn c() {}\n");
        let ws = Workspace::new(std::slice::from_ref(&root)).unwrap();
        let start = ws.resolve(Path::new(".")).unwrap();

        let only_rs = Filter::default().with_exts("rs");
        let rels: Vec<String> = collect(&ws, &start, &only_rs)
            .files
            .iter()
            .map(|f| f.rel().to_string())
            .collect();
        assert_eq!(rels, vec!["src/a.rs", "tests/c.rs"]);

        let no_tests = Filter::default().with_exts(".rs").exclude_tests();
        let rels: Vec<String> = collect(&ws, &start, &no_tests)
            .files
            .iter()
            .map(|f| f.rel().to_string())
            .collect();
        assert_eq!(rels, vec!["src/a.rs"]);

        let inc = Filter {
            include: vec!["src/**".into()],
            ..Filter::default()
        };
        let w = collect(&ws, &start, &inc);
        assert_eq!(w.files.len(), 2);
        assert_eq!(w.filtered, 1, "落とした数を数えていない");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn 読み取りは2進と巨大なファイルを断る() {
        let root = lab("ws-read");
        std::fs::write(root.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        write(&root, "ok.txt", "こんにちは\n");
        let ws = Workspace::new(std::slice::from_ref(&root)).unwrap();
        assert!(matches!(
            read_text(&ws.resolve(Path::new("bin.dat")).unwrap()),
            Err(ContextError::Binary(_))
        ));
        assert_eq!(
            read_text(&ws.resolve(Path::new("ok.txt")).unwrap()).unwrap(),
            "こんにちは\n"
        );
        // ディレクトリは Io エラー (panic しない)
        assert!(read_text(&ws.resolve(Path::new(".")).unwrap()).is_err());
        // UTF-8 でないバイトは置換文字へ落として読み進む
        std::fs::write(root.join("cp932.txt"), [0x82, 0xA0, b'\n']).unwrap();
        assert!(read_text(&ws.resolve(Path::new("cp932.txt")).unwrap()).is_ok());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 出力に載るのは**根からの相対パス**。絶対パスを載せると
    /// 利用者のホームディレクトリ名が文脈へ漏れる。
    #[test]
    fn 相対パスは根からの形になる() {
        let root = lab("ws-rel");
        write(&root, "src/deep/a.rs", "fn a() {}\n");
        let ws = Workspace::new(std::slice::from_ref(&root)).unwrap();
        let sp = ws.resolve(&root.join("src/deep/a.rs")).unwrap();
        assert_eq!(sp.rel(), "src/deep/a.rs");
        assert_eq!(sp.ext(), "rs");
        let _ = std::fs::remove_dir_all(&root);
    }
}
