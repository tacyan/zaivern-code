//! 検証コマンドを**構造のまま**扱う — 判定したものと、OS が実行するものを一致させる。
//!
//! ## なぜ文字列で持たないのか
//!
//! 「文字列を検査して安全だと決め、あとで別の層がもう一度解釈する」形は、
//! 検査と実行の間に**ずれ**を作る。ずれた分だけが穴になる:
//!
//! * `split_whitespace()` は引用符を知らないので、
//!   `cargo test --package "my package"` は 4 語ではなく 5 語に割れる
//! * Windows の `cmd /C` は `%VAR%` を展開するので、検査した文字列と
//!   cmd.exe が実行する文字列が別物になる
//! * `Command::new("rustfmt")` は PATH を引くので、`PATH` の先頭に
//!   workspace が入っていれば**攻撃者が置いた実行体**が動く
//!
//! そこでこの層は、計画から実行までを 1 本の道にする:
//!
//! ```text
//! Planner → ValidationCommand{executable, args}
//!         → 実体の解決 (PATH を自分で引き、信用できない場所を弾く)
//!         → 危険度の判定 (解決した実体と引数を見る)
//!         → 承認ゲート
//!         → その実体 + argv + cwd で起動
//! ```
//!
//! **文字列へ戻す場所は 1 つだけ** — 画面と台帳の見出し
//! ([`ValidationCommand::display`])。そこから実行経路へは戻らない。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 検証コマンド 1 本 (実行体の名前と引数)。
///
/// `executable` は**PATH から解決される素の名前**だけ。パスを含むものは
/// [`super::graph::classify`] が `Forbidden` にする。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Wire", into = "Wire")]
pub struct ValidationCommand {
    pub executable: String,
    pub args: Vec<String>,
}

/// 保存形式。**旧版は 1 本の文字列**だったので、どちらも読めるようにする。
///
/// 書くときは必ず構造化した形 (`Parts`)。読むときだけ文字列を受け付け、
/// その場で構造へ正規化する — 文字列のまま内側へは 1 バイトも入れない。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(untagged)]
enum Wire {
    Parts {
        executable: String,
        #[serde(default)]
        args: Vec<String>,
    },
    Line(String),
}

impl From<Wire> for ValidationCommand {
    fn from(w: Wire) -> Self {
        match w {
            Wire::Parts { executable, args } => Self { executable, args },
            // 旧形式。壊れていても落とさず、空の実行体として持つ
            // (危険度の判定が `Forbidden` にする)。
            Wire::Line(s) => Self::parse(&s).unwrap_or_else(|_| Self {
                executable: String::new(),
                args: Vec::new(),
            }),
        }
    }
}

impl From<ValidationCommand> for Wire {
    fn from(c: ValidationCommand) -> Self {
        Wire::Parts {
            executable: c.executable,
            args: c.args,
        }
    }
}

/// 1 本のコマンドとして受け付ける長さの上限 (文字)。
pub const COMMAND_MAX_CHARS: usize = 400;
/// 引数の数の上限。
pub const ARGS_MAX: usize = 64;

impl ValidationCommand {
    /// 文字列から組み立てる (**引用符を尊重する**)。
    ///
    /// `cargo test --package "my package"` は 3 引数になる。
    /// `split_whitespace()` だと 4 引数に割れて、別のパッケージを指す。
    ///
    /// **展開はしない。** `~` も `$VAR` も `*` も、ただの文字として扱う
    /// (シェルを通さないので展開する主体が居ない)。閉じていない引用符は
    /// 誤りとして断る — 黙って閉じると、書いた人の意図と違うものが動く。
    pub fn parse(line: &str) -> Result<Self, String> {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            return Err("空のコマンド".to_string());
        }
        if trimmed.chars().count() > COMMAND_MAX_CHARS {
            return Err("コマンドが長すぎます".to_string());
        }
        let mut words: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut started = false;
        let mut quote: Option<char> = None;
        for c in trimmed.chars() {
            match quote {
                Some(q) if c == q => quote = None,
                Some(_) => cur.push(c),
                None if c == '"' || c == '\'' => {
                    quote = Some(c);
                    started = true;
                }
                None if c.is_whitespace() => {
                    if started {
                        words.push(std::mem::take(&mut cur));
                        started = false;
                    }
                }
                None => {
                    cur.push(c);
                    started = true;
                }
            }
        }
        if quote.is_some() {
            return Err("引用符が閉じていません".to_string());
        }
        if started {
            words.push(cur);
        }
        let mut it = words.into_iter();
        let executable = it.next().ok_or_else(|| "空のコマンド".to_string())?;
        let args: Vec<String> = it.collect();
        if args.len() > ARGS_MAX {
            return Err("引数が多すぎます".to_string());
        }
        Ok(Self { executable, args })
    }

    /// 画面と台帳に出す見出し。**ここから実行経路へは戻らない。**
    ///
    /// 空白を含む引数は引用して、読んだ人が元の形を復元できるようにする。
    pub fn display(&self) -> String {
        let mut s = self.executable.clone();
        for a in &self.args {
            s.push(' ');
            if a.is_empty() || a.chars().any(char::is_whitespace) {
                s.push('"');
                s.push_str(a);
                s.push('"');
            } else {
                s.push_str(a);
            }
        }
        s
    }

    /// 引数に指定の旗が含まれるか (`--check` / `--check=x` の両方)。
    pub fn has_flag(&self, flag: &str) -> bool {
        self.args
            .iter()
            .any(|a| a == flag || a.starts_with(&format!("{flag}=")))
    }

    /// 旗ではない最初の引数 (サブコマンド)。
    pub fn subcommand(&self) -> Option<&str> {
        self.args
            .iter()
            .find(|a| !a.starts_with('-'))
            .map(|s| s.as_str())
    }
}

impl std::fmt::Display for ValidationCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.display())
    }
}

// ── 実体の解決 ───────────────────────────────────────────────────────

/// 実体を解決できなかった理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ResolveError {
    /// PATH のどこにも無い。
    NotFound(String),
    /// **信用できない場所にあった。** workspace の内側など。
    Untrusted { name: String, found: PathBuf },
    /// PATH が読めない。
    NoPath,
}

impl ResolveError {
    pub fn detail(&self) -> String {
        match self {
            ResolveError::NotFound(n) => format!("`{n}` が PATH に見つかりません"),
            ResolveError::Untrusted { name, found } => format!(
                "`{name}` の実体 ({}) が信用できない場所にあります。\
                 ワークスペースの中の実行ファイルは自動実行しません",
                found.display()
            ),
            ResolveError::NoPath => "PATH が読めません".to_string(),
        }
    }
}

/// PATH の区切り (`:` / `;`)。
fn path_sep() -> char {
    if cfg!(windows) {
        ';'
    } else {
        ':'
    }
}

/// Windows で補う拡張子 (`PATHEXT` が読めないときの既定)。
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// そのパスは「実行できるファイル」か。
fn is_executable_file(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        return meta.permissions().mode() & 0o111 != 0;
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `dir` が `root` の内側か (両方できるかぎり正規化してから比べる)。
fn inside(dir: &Path, root: &Path) -> bool {
    let a = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.to_path_buf());
    let b = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    a.starts_with(&b)
}

/// PATH の 1 要素は、実行体を探してよい場所か (純関数)。
///
/// **相対パスと空の要素は信用しない。** 空の要素は「カレント」を意味する
/// ので、`PATH=:/usr/bin` で `.` が探索対象になる。
pub fn path_entry_is_trusted(entry: &str, workspace: &Path) -> bool {
    let e = entry.trim();
    if e.is_empty() {
        return false;
    }
    let p = Path::new(e);
    if !p.is_absolute() {
        return false;
    }
    !inside(p, workspace)
}

/// **PATH を自分で引いて、実体を確定する。**
///
/// `Command::new("rustfmt")` に任せると、OS がもう一度 PATH を引く。
/// そのとき先頭に workspace が入っていれば、**判定したのとは別の実行体**が
/// 動く (`PATH=<workspace>/bin:$PATH` で `rustfmt` を置くだけでよい)。
///
/// ここで確定した絶対パスをそのまま `Command::new` へ渡すので、
/// 「判定したもの」と「OS が実行するもの」が一致する。
pub fn resolve_in(
    name: &str,
    workspace: &Path,
    path_var: Option<&str>,
    pathext: Option<&str>,
) -> Result<PathBuf, ResolveError> {
    // パス指定は解決しない (呼ぶ前に `Forbidden` で弾かれている)。
    if name.contains('/') || name.contains('\\') {
        return Err(ResolveError::Untrusted {
            name: name.to_string(),
            found: PathBuf::from(name),
        });
    }
    let Some(path_var) = path_var else {
        return Err(ResolveError::NoPath);
    };
    let exts: Vec<String> = if cfg!(windows) {
        let raw = pathext.unwrap_or(DEFAULT_PATHEXT);
        std::iter::once(String::new())
            .chain(
                raw.split(';')
                    .map(|e| e.trim().to_string())
                    .filter(|e| !e.is_empty()),
            )
            .collect()
    } else {
        vec![String::new()]
    };
    let mut untrusted: Option<PathBuf> = None;
    for entry in path_var.split(path_sep()) {
        let trusted = path_entry_is_trusted(entry, workspace);
        let dir = Path::new(entry.trim());
        for ext in &exts {
            let cand = dir.join(format!("{name}{ext}"));
            if !is_executable_file(&cand) {
                continue;
            }
            if trusted {
                return Ok(cand);
            }
            // **信用できない場所で見つけた。** ここで「次を探す」と、
            // OS は先頭のこれを実行するのに、こちらは後ろの別物を判定した
            // ことになる (判定と実行がずれる)。見つけた時点で断る。
            untrusted.get_or_insert(cand);
        }
        if untrusted.is_some() {
            break;
        }
    }
    match untrusted {
        Some(found) => Err(ResolveError::Untrusted {
            name: name.to_string(),
            found,
        }),
        None => Err(ResolveError::NotFound(name.to_string())),
    }
}



#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 引用符を尊重して語に割る() {
        let c = ValidationCommand::parse("cargo test --package \"my package\"").unwrap();
        assert_eq!(c.executable, "cargo");
        assert_eq!(c.args, vec!["test", "--package", "my package"]);

        let p = ValidationCommand::parse("pytest \"tests/my test.py\"").unwrap();
        assert_eq!(p.args, vec!["tests/my test.py"]);

        // 単引用符も同じ
        let s = ValidationCommand::parse("pytest 'tests/a b.py'").unwrap();
        assert_eq!(s.args, vec!["tests/a b.py"]);
    }

    #[test]
    fn 閉じていない引用符は断る() {
        // 黙って閉じると、書いた人の意図と違うものが動く。
        assert!(ValidationCommand::parse("cargo test \"unclosed").is_err());
        assert!(ValidationCommand::parse("").is_err());
    }

    #[test]
    fn 展開はしない() {
        // シェルを通さないので、展開する主体が居ない。ただの文字。
        let c = ValidationCommand::parse("pytest tests/*.py").unwrap();
        assert_eq!(c.args, vec!["tests/*.py"]);
        let d = ValidationCommand::parse("pytest $HOME/x.py").unwrap();
        assert_eq!(d.args, vec!["$HOME/x.py"]);
    }

    #[test]
    fn 見出しは元の形へ戻せる() {
        let c = ValidationCommand::parse("cargo test --package \"my package\"").unwrap();
        assert_eq!(c.display(), "cargo test --package \"my package\"");
        // 見出しから作り直しても同じ構造になる (台帳の照合が壊れない)。
        assert_eq!(ValidationCommand::parse(&c.display()).unwrap(), c);
    }

    #[test]
    fn 旧形式の文字列も読める() {
        // 版 3 までは 1 本の文字列だった。読めなくなると、保存済みの
        // Run が丸ごと「壊れている」になる。
        let v: Vec<ValidationCommand> =
            serde_json::from_str(r#"["cargo test auth", "black --check ."]"#).unwrap();
        assert_eq!(v[0].executable, "cargo");
        assert_eq!(v[0].args, vec!["test", "auth"]);
        assert_eq!(v[1].args, vec!["--check", "."]);
        // 書くときは必ず構造化した形。
        let json = serde_json::to_string(&v[1]).unwrap();
        assert!(json.contains("\"executable\""), "{json}");
        assert_eq!(
            serde_json::from_str::<ValidationCommand>(&json).unwrap(),
            v[1]
        );
    }

    #[test]
    fn 旗とサブコマンドを読む() {
        let c = ValidationCommand::parse("ruff check --fix .").unwrap();
        assert_eq!(c.subcommand(), Some("check"));
        assert!(c.has_flag("--fix"));
        assert!(!c.has_flag("--check"));
        let d = ValidationCommand::parse("black --check=x .").unwrap();
        assert!(d.has_flag("--check"));
    }

    fn tmp(name: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-team-resolve", name)
    }

    #[cfg(unix)]
    fn put_exe(dir: &Path, name: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[cfg(unix)]
    #[test]
    fn workspace内の偽の実行体は使わない() {
        // **PATH の先頭に workspace が入っているだけで乗っ取れる**、を防ぐ。
        let ws = tmp("hijack");
        let bin = ws.join("bin");
        let fake = put_exe(&bin, "rustfmt");
        let real_dir = tmp("hijack-real");
        let real = put_exe(&real_dir, "rustfmt");

        let path = format!("{}:{}", bin.display(), real_dir.display());
        let got = resolve_in("rustfmt", &ws, Some(&path), None);
        assert_eq!(
            got,
            Err(ResolveError::Untrusted {
                name: "rustfmt".into(),
                found: fake.clone()
            }),
            "workspace の中の実行体を使おうとした"
        );
        // **後ろの本物へ黙って落ちない。** OS は先頭を実行するので、
        // そこで「後ろの本物を判定した」ことにすると判定と実行がずれる。
        assert_ne!(got, Ok(real.clone()));

        // workspace の外だけなら解決できる。
        assert_eq!(
            resolve_in("rustfmt", &ws, Some(&real_dir.display().to_string()), None),
            Ok(real)
        );
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&real_dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 相対pathと空の要素は信用しない() {
        let ws = tmp("relpath");
        std::fs::create_dir_all(&ws).unwrap();
        assert!(!path_entry_is_trusted("", &ws), "空 = カレントを信用した");
        assert!(!path_entry_is_trusted(".", &ws));
        assert!(!path_entry_is_trusted("bin", &ws));
        assert!(!path_entry_is_trusted("./bin", &ws));
        assert!(!path_entry_is_trusted(&ws.display().to_string(), &ws));
        assert!(path_entry_is_trusted("/usr/bin", &ws));
        std::fs::remove_dir_all(&ws).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 見つからなければ見つからないと言う() {
        let ws = tmp("notfound");
        std::fs::create_dir_all(&ws).unwrap();
        let d = tmp("notfound-bin");
        std::fs::create_dir_all(&d).unwrap();
        assert_eq!(
            resolve_in("zzz-no-such", &ws, Some(&d.display().to_string()), None),
            Err(ResolveError::NotFound("zzz-no-such".into()))
        );
        assert_eq!(
            resolve_in("rustfmt", &ws, None, None),
            Err(ResolveError::NoPath)
        );
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn パス指定は解決しない() {
        let ws = tmp("qualified");
        for bad in ["/tmp/cargo", "./cargo", "tools\\cargo"] {
            assert!(matches!(
                resolve_in(bad, &ws, Some("/usr/bin"), None),
                Err(ResolveError::Untrusted { .. })
            ));
        }
    }
}
