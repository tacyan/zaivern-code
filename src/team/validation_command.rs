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
//!         → 危険度の判定 (名前と引数を見る: graph::classify)
//!         → 承認ゲート (runtime::advance)
//!         → 実行器で危険度と承認を**もう一度**確かめる (launch)
//!         → 実体の解決 (PATH を自分で引き、信用できない場所を弾く)
//!         → その実体 + argv + cwd で起動
//! ```
//!
//! **判定は解決より前に来る。** 判定が見ているのは名前だけなので、
//! 解決が「信用できない場所の実体」を返したらそこで実行を止める
//! ([`resolve_in`])。名前で得た「読むだけ」の評価を、実体側へ持ち越さない。
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
            // **実行体は必ず刈り込む。** 刈り込まないと、判定する側
            // (`classify` は `trim()` してから許可リストを見る) と解決する側
            // (`resolve_in` は文字どおりの名前で PATH を引く) が**別の
            // 文字列**を見る。`{"executable":"cargo\u{a0}"}` は許可リストを
            // `cargo` として通り抜け、実際には `cargo\u{a0}` を探しに行く。
            Wire::Parts { executable, args } => Self::new(executable, args),
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
    /// 実行体と引数から組み立てる (**実行体の前後の空白は刈る**)。
    ///
    /// 組み立ての入口をここ 1 つにして、「判定した文字列」と
    /// 「PATH を引く文字列」がずれないようにする。
    pub fn new(executable: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            executable: executable.into().trim().to_string(),
            args,
        }
    }

    /// **語に割れなかった行**を、そのまま 1 つの実行体として持つ。
    ///
    /// 黙って捨てない — 捨てると、人が SPEC に書いた検証がどこにも
    /// 出ないまま消える。ここへ入れておけば [`super::graph::classify`] が
    /// `Forbidden` にし、`validate_plan` が理由つきで止める。
    pub fn unparsed(line: &str) -> Self {
        Self {
            executable: line.trim().to_string(),
            args: Vec::new(),
        }
    }

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
        Ok(Self::new(executable, args))
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

    /// **旗の位置にある語**だけを並べる (`--check=x` は `--check` として)。
    ///
    /// `takes_value` に載る旗は**次の語を値として食う**ので、食われた語は
    /// 旗として数えない。`--` 以降は位置引数として扱う。
    ///
    /// これを間違えると、判定と実行がずれる。black は Click なので
    /// `--extend-exclude --check .` の `--check` は**旗ではなく値**であり、
    /// black は書き換えモードで動く。位置を見ない照合はこれを
    /// 「`--check` がある = 読むだけ」と読む。
    pub fn flags_in_flag_position(&self, takes_value: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        let mut skip_next = false;
        let mut positional_only = false;
        for a in &self.args {
            if skip_next {
                skip_next = false;
                continue;
            }
            if positional_only {
                continue;
            }
            if a == "--" {
                positional_only = true;
                continue;
            }
            if !a.starts_with('-') || a == "-" {
                continue;
            }
            let (name, has_eq) = match a.split_once('=') {
                Some((n, _)) => (n, true),
                None => (a.as_str(), false),
            };
            out.push(name.to_string());
            if !has_eq && takes_value.contains(&name) {
                skip_next = true;
            }
        }
        out
    }

    /// **旗でも値でもない最初の語** (サブコマンド)。
    ///
    /// `takes_value` を渡すのは、`ruff --config foo.toml check .` の
    /// `foo.toml` をサブコマンドと読まないため。
    pub fn first_positional(&self, takes_value: &[&str]) -> Option<&str> {
        let mut skip_next = false;
        for a in &self.args {
            if skip_next {
                skip_next = false;
                continue;
            }
            if a == "--" {
                continue;
            }
            if a.starts_with('-') && a != "-" {
                if !a.contains('=') && takes_value.contains(&a.as_str()) {
                    skip_next = true;
                }
                continue;
            }
            return Some(a.as_str());
        }
        None
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

/// 名前に補う拡張子を、**試す順**に並べる (純関数)。
///
/// **拡張子つきを先に見る。** Windows の cmd.exe も `PATHEXT` を先に当てる。
/// 素の名前を先にすると、npm / yarn / pnpm のように「拡張子なしの sh
/// スクリプト」と「`.cmd`」が同じディレクトリに並ぶ道具で、
/// **CreateProcess が起こせない sh スクリプト**を選んでしまう
/// (`ERROR_BAD_EXE_FORMAT`)。非 unix では実行権限を見られないぶん、
/// 「ファイルがある」だけで選んでしまうのが効いてくる。
/// 素の名前は最後の受け皿として残す。
///
/// `windows` を引数にしてあるのは、**この並び順を macOS / Linux の
/// テストからも固定するため** (cfg で分けると、Windows の CI でしか
/// 検査されない場所に判断が住む)。
fn candidate_exts(pathext: Option<&str>, windows: bool) -> Vec<String> {
    if !windows {
        return vec![String::new()];
    }
    pathext
        .filter(|s| !s.trim().is_empty())
        .unwrap_or(DEFAULT_PATHEXT)
        .split(';')
        .map(|e| e.trim().to_string())
        .filter(|e| !e.is_empty())
        .chain(std::iter::once(String::new()))
        .collect()
}

/// できるかぎり正規化した絶対パス (できなければそのまま)。
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── 実行体の信用区分 ─────────────────────────────────────────────────

/// 実行体が**どこに置かれているか**の区分。
///
/// **「workspace の外なら信用できる」は成り立たない。** エージェントは
/// Zaivern と同じ利用者権限で動くので、`~/.local/bin` や `~/bin` に
/// 実行体を置ける (`mkdir -p ~/.local/bin && cp evil ~/.local/bin/rustfmt`)。
/// workspace の外にあることは、**書き換えられないことを 1 つも意味しない**。
///
/// **並びは弱い順。** [`Ord`] を導出していて、「置き場所から見た区分」と
/// 「実体から見た区分」の**弱いほう**を [`Ord::min`] で採る。
/// 順番を入れ替えると悲観側の選択が反転する
/// (`validation_command::tests::信用の並びは弱い順` が固定する)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExecTrust {
    /// workspace の中。**エージェントが書いたものそのもの**なので、
    /// 承認があっても自動実行しない。
    Workspace,
    /// どの分類にも当てはまらない場所。**安全側に倒す** (承認が要る)。
    Unknown,
    /// 利用者の権限で書ける場所 (`$HOME` 配下 / `%LOCALAPPDATA%` / `/tmp`)。
    /// エージェントも同じ権限で書けるので、無承認では実行しない。
    UserWritable,
    /// 書き換えに昇格が要る場所 (`/usr/bin`, `C:\Windows\System32`)。
    SystemTrusted,
}

impl ExecTrust {
    /// **承認の証跡なしで起こしてよいか。** `SystemTrusted` だけが true。
    pub fn auto_runnable(self) -> bool {
        self == ExecTrust::SystemTrusted
    }

    /// 承認があっても起こさないか (workspace の中の実行体)。
    pub fn never_runnable(self) -> bool {
        self == ExecTrust::Workspace
    }

    /// 人へ出す理由。
    pub fn why(self) -> &'static str {
        match self {
            ExecTrust::Workspace => "ワークスペースの中にあります",
            ExecTrust::Unknown => "信用できる場所か判断できません",
            ExecTrust::UserWritable => "利用者の権限で書き換えられる場所にあります",
            ExecTrust::SystemTrusted => "システムの場所にあります",
        }
    }
}

/// どこを「利用者が書ける場所」「昇格が要る場所」と見なすかの表。
///
/// **OS を跨いだ規則を、この 1 つの純粋なデータに落とす。** `cfg` で
/// 分岐すると Windows の規則は Windows の CI でしか検査されない場所に住み、
/// 「書いたが 1 度も動かしていない」状態のまま出荷される。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TrustPolicy {
    /// workspace の根。**素の形と解決した形の両方**を持つ (下記)。
    workspace: Vec<String>,
    user_roots: Vec<String>,
    system_roots: Vec<String>,
    windows: bool,
}

impl TrustPolicy {
    /// 表を組み立てる (根はすべて正規化して持つ)。
    ///
    /// **workspace は素の形と `canonicalize` した形の両方を持つ。**
    /// 片方だけだと、リンクを経由した書き方 (macOS の
    /// `/var/folders/…` は実体が `/private/var/folders/…`) を、
    /// 文字列だけを見る [`path_entry_trust`] が取り逃がす。実体側の
    /// 照合は解決してから当たるので実害は出ないが、**どちらの綴りでも
    /// 同じ答えになる**ほうが読み手に嘘がない (実際に macOS の CI が
    /// 「workspace そのものを PATH に書いたのに Workspace ではない」で
    /// 落ちた)。
    pub fn new(workspace: &Path, user_roots: &[&str], system_roots: &[&str], windows: bool) -> Self {
        let n = |s: &str| norm_path(Path::new(s), windows);
        let mut ws = vec![norm_path(workspace, windows)];
        let resolved = norm_path(&canonical(workspace), windows);
        if !ws.contains(&resolved) {
            ws.push(resolved);
        }
        Self {
            workspace: ws.into_iter().filter(|w| !w.is_empty()).collect(),
            user_roots: user_roots.iter().map(|s| n(s)).filter(|s| !s.is_empty()).collect(),
            system_roots: system_roots.iter().map(|s| n(s)).filter(|s| !s.is_empty()).collect(),
            windows,
        }
    }

    /// いまの OS の表 (`workspace` は正規化して持つ)。
    ///
    /// `HOME` が読めないときは [`dirs::home_dir`] へ落ちる。**どちらも
    /// 読めなければ利用者の根が 1 つも無い**ことになるが、そのときは
    /// 未分類 = `Unknown` になるだけで、緩む方向へは倒れない。
    pub fn for_workspace(workspace: &Path) -> Self {
        let env = |k: &str| {
            std::env::var(k)
                .ok()
                .filter(|v| !v.trim().is_empty())
                .or_else(|| match k {
                    "HOME" | "USERPROFILE" => {
                        dirs::home_dir().map(|h| h.display().to_string())
                    }
                    _ => None,
                })
        };
        if cfg!(windows) {
            windows_policy(workspace, env)
        } else {
            unix_policy(workspace, env)
        }
    }
}

/// unix (macOS / Linux) の表。**`env` を引数に取る**ので、
/// 中身は OS に依らず試験から固定できる。
pub fn unix_policy(workspace: &Path, env: impl Fn(&str) -> Option<String>) -> TrustPolicy {
    let home = env("HOME").unwrap_or_default();
    let mut user: Vec<&str> = vec![
        // **どの利用者のホームでも**同じ扱い (`HOME` が読めない場合の受け皿)。
        "/home",
        "/Users",
        "/root",
        // 誰でも書ける場所。
        "/tmp",
        "/var/tmp",
        "/private/tmp",
        "/private/var/tmp",
        // **パッケージ管理が利用者所有で置く場所。**
        //
        // Homebrew は `/opt/homebrew` (Apple Silicon) と `/usr/local`
        // (Intel) を**いまのログインユーザーの所有**にする。MacPorts の
        // `/opt/local` も同じ。つまりエージェントは Zaivern と同じ権限で
        // `/opt/homebrew/bin/rustfmt` を**書き換えられる** — この PR の
        // 脅威モデルそのものなので、昇格が要る場所として扱ってはいけない。
        //
        // `/usr/local` をここへ置くのが効くのは、利用者の場所を
        // システムより**先に**見るため (`classify_path` の順序)。
        // `/usr` に含まれるからといって `SystemTrusted` にはならない。
        "/usr/local",
        "/opt/homebrew",
        "/opt/local",
        "/home/linuxbrew/.linuxbrew",
    ];
    if !home.trim().is_empty() {
        user.push(home.trim());
    }
    TrustPolicy::new(
        workspace,
        &user,
        &[
            // 書き換えに昇格が要る場所。**上の利用者側の一覧が先に効く**
            // ので、`/usr/local` はここに含まれない。
            "/usr",
            "/bin",
            "/sbin",
            "/Library/Developer",
            "/System",
            "/snap",
        ],
        false,
    )
}

/// Windows の表。**`env` を引数に取る**ので、macOS / Linux の CI からも
/// この規則そのものを試験できる (Windows でしか通らない判断を残さない)。
pub fn windows_policy(workspace: &Path, env: impl Fn(&str) -> Option<String>) -> TrustPolicy {
    let sys_root = env("SystemRoot").unwrap_or_else(|| r"C:\Windows".to_string());
    let drive = env("SystemDrive").unwrap_or_else(|| "C:".to_string());
    let mut user: Vec<String> = Vec::new();
    for k in [
        "USERPROFILE",
        "LOCALAPPDATA",
        "APPDATA",
        "TEMP",
        "TMP",
        "PUBLIC",
        // 既定の ACL で認証済みの利用者が作成できる。
        // (chocolatey などがここへ入る — 無承認では実行しない)
        "ProgramData",
    ] {
        if let Some(v) = env(k) {
            user.push(v);
        }
    }
    user.push(format!(r"{drive}\Users"));
    user.push(r"C:\Users".to_string());
    let mut system: Vec<String> = vec![
        sys_root.clone(),
        format!(r"{sys_root}\System32"),
        format!(r"{drive}\Windows"),
        r"C:\Windows".to_string(),
        r"C:\Program Files".to_string(),
        r"C:\Program Files (x86)".to_string(),
    ];
    for k in ["ProgramFiles", "ProgramFiles(x86)", "ProgramW6432"] {
        if let Some(v) = env(k) {
            system.push(v);
        }
    }
    let u: Vec<&str> = user.iter().map(|s| s.as_str()).collect();
    let sy: Vec<&str> = system.iter().map(|s| s.as_str()).collect();
    TrustPolicy::new(workspace, &u, &sy, true)
}

/// パスを比較できる形へ揃える (純関数)。
///
/// Windows 側は `\` を `/` へ寄せ、大小を畳み、`canonicalize` が付ける
/// 拡張長プレフィクス (`\\?\`) を落とす。**`Path::starts_with` を使わない**
/// のは、Windows 形式の文字列が unix ではただの 1 要素になり、
/// macOS / Linux の CI から Windows の規則を試験できなくなるため。
fn norm_path(p: &Path, windows: bool) -> String {
    let mut s = p.to_string_lossy().into_owned();
    if windows {
        s = s.replace('\\', "/");
        if let Some(rest) = s.strip_prefix("//?/") {
            s = match rest.strip_prefix("UNC/").or_else(|| rest.strip_prefix("unc/")) {
                Some(unc) => format!("//{unc}"),
                None => rest.to_string(),
            };
        }
        s = s.to_ascii_lowercase();
    }
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    s
}

/// 正規化済みの絶対パスらしいか (`/usr/bin` / `c:/windows` / `//server/share`)。
fn looks_absolute(s: &str) -> bool {
    if s.starts_with('/') {
        return true;
    }
    let b = s.as_bytes();
    b.len() >= 3 && b[0].is_ascii_alphabetic() && b[1] == b':' && b[2] == b'/'
}

/// `child` が `root` の内側 (または同じ) か。
fn under(child: &str, root: &str) -> bool {
    if root.is_empty() || !child.starts_with(root) {
        return false;
    }
    let rest = &child[root.len()..];
    rest.is_empty() || rest.starts_with('/') || root.ends_with('/')
}

/// パス 1 つの信用区分 (**純関数**、I/O をしない)。
///
/// 判定の順序が保証そのもの:
/// workspace → 利用者が書ける場所 → 昇格が要る場所 → それ以外。
/// 利用者の根を先に見るので、入れ子になっていても**緩い側へは倒れない**。
pub fn classify_path(p: &Path, policy: &TrustPolicy) -> ExecTrust {
    let s = norm_path(p, policy.windows);
    if !looks_absolute(&s) {
        // 相対の指定は「起動時のカレント」= workspace を指す。
        return ExecTrust::Workspace;
    }
    if s.split('/').any(|seg| seg == "..") {
        // 正規化していない `..` は、どの根の内側かを文字列では決められない。
        return ExecTrust::Unknown;
    }
    if policy.workspace.iter().any(|w| under(&s, w)) {
        return ExecTrust::Workspace;
    }
    if policy.user_roots.iter().any(|r| under(&s, r)) {
        return ExecTrust::UserWritable;
    }
    if policy.system_roots.iter().any(|r| under(&s, r)) {
        return ExecTrust::SystemTrusted;
    }
    ExecTrust::Unknown
}

/// **実測して降格する** — 表が「システム」と言っても、実際に自分の権限で
/// 書き換えられるなら信用しない。
///
/// 表 (`TrustPolicy`) は綴りしか見ないので、次の環境を取りこぼす:
///
/// * Homebrew が `/usr/local` をログインユーザー所有にしている
/// * 誰かが `/usr/bin` を世界書き込み可にしてしまっている
/// * 独自ビルドのシステム領域が自分の uid で作られている
///
/// **上げる方向には絶対に効かない。** ここが返すのは「そのままか、弱いか」
/// だけ (`min`)。上げてしまうと、綴りで断ったものを実測で通すことになる。
///
/// `my_uid` は「いまのプロセスの利用者」。std だけでは取れないので、
/// **ホームディレクトリの所有者**を使う (自分のホームは自分のもの)。
/// 取れなければ所有者の比較はせず、世界書き込み可だけを見る。
#[cfg(unix)]
fn measured_trust(path: &Path, trust: ExecTrust, my_uid: Option<u32>) -> ExecTrust {
    use std::os::unix::fs::MetadataExt;
    if trust != ExecTrust::SystemTrusted {
        return trust;
    }
    // **実体と、そこへ至る置き場を全部見る。** 置き場に書ければ実体を
    // 差し替えられる (消して置き直せる) し、その 1 つ上に書ければ置き場ごと
    // 挿げ替えられる。`path` は正規化済みなので、辿る先はリンクの解けた形。
    for t in std::iter::once(path).chain(path.ancestors().skip(1)) {
        let Ok(meta) = std::fs::metadata(t) else {
            // 見られないものは判断しない。**綴りだけで通さない。**
            return ExecTrust::Unknown;
        };
        let mode = meta.mode();
        let world_writable = mode & 0o002 != 0;
        // **uid 0 では所有者の比較をしない。** root はどこへでも書けるので、
        // 所有者で見ると「システムの場所」が 1 つも無くなる — そこまで倒すと
        // 全部が承認必須になり、人は中身を読まずに承認するようになる
        // (守りとしては逆効果)。root で動かしているなら、そもそも区分は
        // 守りにならない (残存する既知の制限として文書化してある)。
        // 世界書き込み可だけは root でも見る。
        let mine = my_uid.is_some_and(|u| u != 0 && meta.uid() == u) && mode & 0o200 != 0;
        if world_writable || mine {
            return ExecTrust::UserWritable;
        }
    }
    trust
}

/// 非 unix では所有権を同じ形で読めないので、表の判断のまま。
#[cfg(not(unix))]
fn measured_trust(_path: &Path, trust: ExecTrust, _my_uid: Option<u32>) -> ExecTrust {
    trust
}

/// いまの利用者の uid (**自分のホームの所有者**として読む)。
///
/// `libc` を足さずに取るための形。読めなければ `None` で、そのときは
/// 世界書き込み可だけを見る (判断を諦めるのではなく、材料を減らす)。
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let home = std::env::var("HOME").ok().filter(|h| !h.trim().is_empty())?;
    std::fs::metadata(home).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

/// PATH の 1 要素の信用区分 (**純関数**)。
///
/// **空の要素と相対パスは workspace 扱い。** 空は「カレント」を意味する
/// ので `PATH=:/usr/bin` で作業フォルダが探索対象になる。
pub fn path_entry_trust(entry: &str, policy: &TrustPolicy) -> ExecTrust {
    classify_path(Path::new(entry.trim()), policy)
}

/// 解決した実体と、その信用区分。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub path: PathBuf,
    pub trust: ExecTrust,
}

/// **PATH を自分で引いて、実体を確定する。**
///
/// `Command::new("rustfmt")` に任せると、OS がもう一度 PATH を引く。
/// そのとき先頭に workspace が入っていれば、**判定したのとは別の実行体**が
/// 動く (`PATH=<workspace>/bin:$PATH` で `rustfmt` を置くだけでよい)。
///
/// ここで確定した絶対パスをそのまま `Command::new` へ渡すので、
/// 「判定したもの」と「OS が実行するもの」が一致する。
///
/// **後ろへ落ちない。** PATH の順に見て**最初に見つかった実行体**が答え。
/// 前方の信用できないものを飛ばして後方の信用できるものを採ると、
/// OS が実行するのは前方のほうなので判定と実行がずれる。
pub fn resolve_in(
    name: &str,
    workspace: &Path,
    path_var: Option<&str>,
    pathext: Option<&str>,
) -> Result<Resolved, ResolveError> {
    resolve_with(name, &TrustPolicy::for_workspace(workspace), path_var, pathext)
}

/// [`resolve_in`] の本体 (表を差し替えられる形)。
pub fn resolve_with(
    name: &str,
    policy: &TrustPolicy,
    path_var: Option<&str>,
    pathext: Option<&str>,
) -> Result<Resolved, ResolveError> {
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
    let exts = candidate_exts(pathext, cfg!(windows));
    for entry in path_var.split(path_sep()) {
        let entry_trust = path_entry_trust(entry, policy);
        let dir = Path::new(entry.trim());
        for ext in &exts {
            let cand = dir.join(format!("{name}{ext}"));
            if !is_executable_file(&cand) {
                continue;
            }
            // **置き場所と実体の、弱いほうを採る。** 置き場所だけでは
            // 足りない — 信用できる場所に置かれた**シンボリックリンク**が
            // workspace の中を指していれば、エージェントが書いたコードが
            // 動く (`ln -s $WS/bin/evil ~/.local/bin/ruff`)。
            // `std::fs::metadata` はリンクを辿るので「そこにファイルが
            // ある」だけでは判定にならない。`canonical` はリンクを解くので、
            // たどった先で判定できる。
            let trust = entry_trust.min(classify_path(&canonical(&cand), policy));
            // **綴りで「システム」でも、実際に書き換えられるなら信用しない。**
            // 上げる方向には効かない (`measured_trust` は同じか弱いかだけ)。
            let trust = trust.min(measured_trust(&canonical(&cand), trust, current_uid()));
            if trust.never_runnable() {
                return Err(ResolveError::Untrusted {
                    name: name.to_string(),
                    found: cand,
                });
            }
            // **信用の区分を持って返す。** ここで「信用できない場所だから
            // 次を探す」としてはいけない (OS は先頭のこれを実行する)。
            // 起こしてよいかどうかは、承認と突き合わせる呼び出し側が決める。
            return Ok(Resolved { path: cand, trust });
        }
    }
    Err(ResolveError::NotFound(name.to_string()))
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
        assert_eq!(c.first_positional(&[]), Some("check"));
        assert_eq!(c.flags_in_flag_position(&[]), vec!["--fix"]);
        let d = ValidationCommand::parse("black --check=x .").unwrap();
        assert_eq!(d.flags_in_flag_position(&[]), vec!["--check"]);
    }

    #[test]
    fn 値として食われた語を旗と読まない() {
        // **これを読み違えると workspace が黙って書き換わる。**
        // black は Click なので `--extend-exclude` は次の語を値として食う。
        // `--check` は旗ではなく値になり、black は書き換えモードで動く。
        let c = ValidationCommand::parse("black --extend-exclude --check .").unwrap();
        assert!(
            c.args.iter().any(|a| a == "--check"),
            "位置を見ない照合では見つかる (だから危ない)"
        );
        assert_eq!(
            c.flags_in_flag_position(&["--extend-exclude"]),
            vec!["--extend-exclude"],
            "食われた `--check` を旗として数えた"
        );
        // `=` で書いたときは次の語を食わない。
        let d = ValidationCommand::parse("black --extend-exclude=x --check .").unwrap();
        assert_eq!(
            d.flags_in_flag_position(&["--extend-exclude"]),
            vec!["--extend-exclude", "--check"]
        );
        // `--` 以降は位置引数。
        let e = ValidationCommand::parse("black -- --check").unwrap();
        assert!(e.flags_in_flag_position(&[]).is_empty());
        // サブコマンド探しも値を飛ばす。
        let f = ValidationCommand::parse("ruff --config x.toml check .").unwrap();
        assert_eq!(f.first_positional(&["--config"]), Some("check"));
        assert_eq!(
            f.first_positional(&[]),
            Some("x.toml"),
            "値を知らないと値をサブコマンドと読む"
        );
    }

    #[test]
    fn 実行体の前後の空白は組み立てで刈る() {
        // **判定する側と解決する側で別の文字列を見ない。**
        // `classify` は `trim()` してから許可リストを見るので、刈らずに
        // 持つと `cargo\u{a0}` が `cargo` として許可を通り、PATH は
        // `cargo\u{a0}` を探しに行く (= 判定と実行がずれる)。
        let v: ValidationCommand =
            serde_json::from_str(r#"{"executable":"cargo\u00a0","args":["test"]}"#).unwrap();
        assert_eq!(v.executable, "cargo");
        assert_eq!(ValidationCommand::new(" black ", vec![]).executable, "black");
    }

    #[test]
    fn windowsでは拡張子つきを先に試す() {
        // **素の名前を先に試すと npm が動かない。** npm / yarn / pnpm は
        // 同じディレクトリに拡張子なしの sh スクリプトと `.cmd` を並べる。
        // 非 unix では実行権限を見られないので、素の名前を先にすると
        // sh スクリプトを選び、CreateProcess が `ERROR_BAD_EXE_FORMAT` で
        // 落ちる (以前の `cmd /C npm` はここを PATHEXT で正しく解いていた)。
        let e = candidate_exts(Some(".COM;.EXE;.BAT;.CMD"), true);
        assert_eq!(e.first().map(String::as_str), Some(".COM"));
        assert_eq!(
            e.last().map(String::as_str),
            Some(""),
            "素の名前は最後の受け皿"
        );
        assert!(e.iter().position(|x| x == ".CMD").unwrap() < e.len() - 1);
        // PATHEXT が読めない / 空のときは既定へ。
        assert_eq!(candidate_exts(None, true), candidate_exts(Some(""), true));
        // unix は拡張子を補わない。
        assert_eq!(candidate_exts(Some(".EXE"), false), vec![String::new()]);
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
        assert!(got.is_err());

        // workspace の外だけなら解決できる。**ただし一時フォルダは
        // 利用者が書ける場所**なので、無承認で走ってよい区分にはしない。
        let ok = resolve_in("rustfmt", &ws, Some(&real_dir.display().to_string()), None)
            .expect("解決できるはず");
        assert_eq!(ok.path, real);
        assert!(
            !ok.trust.auto_runnable(),
            "一時フォルダの実行体を無承認で走ってよいと判定した: {ok:?}"
        );
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&real_dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 信用できる場所からworkspaceへ張られたリンクも使わない() {
        // **置き場所だけを見ても足りない。** `~/.local/bin` のような
        // 普通の PATH 要素に、workspace の中を指すシンボリックリンクを
        // 1 本張るだけで、エージェントが書いたコードが「読むだけの
        // 検証」として動いてしまう。`std::fs::metadata` はリンクを辿るので、
        // 「そこにファイルがある」だけでは判定にならない。
        let ws = tmp("symlink-ws");
        let evil = put_exe(&ws.join("bin"), "evil");
        let good_dir = tmp("symlink-bin");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::os::unix::fs::symlink(&evil, good_dir.join("ruff")).unwrap();

        let got = resolve_in("ruff", &ws, Some(&good_dir.display().to_string()), None);
        assert!(
            matches!(got, Err(ResolveError::Untrusted { .. })),
            "workspace の中を指すリンクを実行対象にした: {got:?}"
        );
        // 同じ場所の、workspace を指さない実体は使える (「常に断る」に
        // なっていないこと)。
        put_exe(&good_dir, "ruff2");
        assert!(
            resolve_in("ruff2", &ws, Some(&good_dir.display().to_string()), None).is_ok(),
            "普通の実行体まで断った"
        );
        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&good_dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 相対pathと空の要素は信用しない() {
        let ws = tmp("relpath");
        std::fs::create_dir_all(&ws).unwrap();
        let pol = TrustPolicy::for_workspace(&ws);
        // 空と相対は「起動時のカレント」= workspace を指す。
        for e in ["", ".", "bin", "./bin"] {
            assert_eq!(
                path_entry_trust(e, &pol),
                ExecTrust::Workspace,
                "{e:?} を workspace の外と見た"
            );
        }
        assert_eq!(
            path_entry_trust(&ws.display().to_string(), &pol),
            ExecTrust::Workspace
        );
        assert_eq!(path_entry_trust("/usr/bin", &pol), ExecTrust::SystemTrusted);

        // **リンク越しに渡された workspace も、渡された綴りのまま分かる。**
        // macOS の一時フォルダは `/var/folders/…` で実体が
        // `/private/var/folders/…`。解決した形しか持たない表だと
        // 「workspace そのものを PATH に書いたのに Workspace ではない」に
        // なる (実際に macOS の CI が落ちた)。
        let link = tmp("relpath-link").join("as-link");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&ws, &link).unwrap();
        let via_link = TrustPolicy::for_workspace(&link);
        assert_eq!(
            path_entry_trust(&link.display().to_string(), &via_link),
            ExecTrust::Workspace,
            "{} を workspace と見ていない",
            link.display()
        );
        std::fs::remove_dir_all(link.parent().unwrap()).ok();
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **綴りが違っても、workspace の中の実行体は弾く。**
    ///
    /// PATH の要素だけを文字どおりに見る [`path_entry_trust`] は I/O を
    /// しないので、`/var/folders/…` と `/private/var/folders/…` のような
    /// 「同じ場所の別の綴り」までは分からない。そこを埋めているのが
    /// **見つけた実体を解決してから分類する**ほう ([`resolve_with`]) で、
    /// 保証はそちらが持っている。ここはその保証そのものを見る。
    #[cfg(unix)]
    #[test]
    fn workspaceの別の綴りから見つけた実行体も弾く() {
        let ws = tmp("spelling-ws");
        let bin = ws.join("bin");
        put_exe(&bin, "rustfmt");
        // workspace を指すリンクを作り、**PATH にはリンク越しの綴り**を渡す。
        let link = tmp("spelling-link").join("as-link");
        std::fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&ws, &link).unwrap();

        let via_link = link.join("bin").display().to_string();
        let got = resolve_in("rustfmt", &ws, Some(&via_link), None);
        assert!(
            matches!(got, Err(ResolveError::Untrusted { .. })),
            "別の綴りで書かれた workspace の実行体を使おうとした: {got:?}"
        );
        // 逆向き (表はリンク越しの綴りで組み、PATH には実体の綴り) も同じ。
        let real = ws.join("bin").display().to_string();
        let got = resolve_in("rustfmt", &link, Some(&real), None);
        assert!(
            matches!(got, Err(ResolveError::Untrusted { .. })),
            "実体の綴りで書かれた workspace の実行体を使おうとした: {got:?}"
        );
        std::fs::remove_dir_all(link.parent().unwrap()).ok();
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
    fn 信用の並びは弱い順() {
        // `min` で「置き場所」と「実体」の悲観側を採るので、この並びが
        // 保証そのもの。入れ替えると、workspace のリンクが
        // `SystemTrusted` として通る。
        assert!(ExecTrust::Workspace < ExecTrust::Unknown);
        assert!(ExecTrust::Unknown < ExecTrust::UserWritable);
        assert!(ExecTrust::UserWritable < ExecTrust::SystemTrusted);
        assert_eq!(
            ExecTrust::SystemTrusted.min(ExecTrust::Workspace),
            ExecTrust::Workspace
        );
        // **無承認で走ってよいのは 1 つだけ。**
        for t in [
            ExecTrust::Workspace,
            ExecTrust::Unknown,
            ExecTrust::UserWritable,
        ] {
            assert!(!t.auto_runnable(), "{t:?} を無承認で走らせる判定にした");
            assert!(!t.why().is_empty());
        }
        assert!(ExecTrust::SystemTrusted.auto_runnable());
        // **承認があっても走らせないのは workspace だけ。**
        assert!(ExecTrust::Workspace.never_runnable());
        assert!(!ExecTrust::UserWritable.never_runnable());
    }

    /// 利用者が書ける場所の表 (unix)。**`$HOME` 配下は信用しない。**
    #[test]
    fn 利用者が書ける場所をシステムと同じに扱わない() {
        // `~/.local/bin` / `~/bin` は、エージェントが Zaivern と同じ権限で
        // 書ける (`mkdir -p ~/.local/bin && cp evil ~/.local/bin/rustfmt`)。
        // ここを「workspace の外だから信用できる」と読むのが、直した穴。
        let ws = Path::new("/work/repo");
        // **ホームは既定の一覧の外**に置く。ここが `UserWritable` になるのは
        // `$HOME` を見ているからで、`/home` や `/tmp` の一覧のおかげではない。
        let home = "/opt/custom-home";
        let pol = unix_policy(ws, |k| (k == "HOME").then(|| home.to_string()));
        for p in [
            "/opt/custom-home/.local/bin/rustfmt",
            "/opt/custom-home/bin/rustfmt",
            "/opt/custom-home/.cargo/bin/cargo",
            "/home/alice/.local/bin/ruff",
            "/Users/alice/bin/ruff",
            "/root/bin/ruff",
            "/tmp/evil/rustfmt",
            "/var/tmp/evil/rustfmt",
        ] {
            assert_eq!(
                classify_path(Path::new(p), &pol),
                ExecTrust::UserWritable,
                "{p} を利用者が書ける場所と見ていない"
            );
        }
        // ホームが読めないときでも、**システム扱いにはならない**。
        let blind = unix_policy(ws, |_| None);
        assert_eq!(
            classify_path(Path::new("/opt/custom-home/.local/bin/rustfmt"), &blind),
            ExecTrust::Unknown,
            "分類できない場所をシステム扱いにした"
        );
        assert!(!classify_path(Path::new("/opt/custom-home/.local/bin/rustfmt"), &blind).auto_runnable());
        // 昇格が要る場所は、これまでどおり通す。
        for p in ["/usr/bin/rustfmt", "/bin/sh", "/usr/sbin/foo", "/snap/bin/x"] {
            assert_eq!(
                classify_path(Path::new(p), &pol),
                ExecTrust::SystemTrusted,
                "{p} まで断った"
            );
        }
        // **パッケージ管理が利用者所有で置く場所は、システム扱いにしない。**
        //
        // Homebrew は `/opt/homebrew` (Apple Silicon) と `/usr/local`
        // (Intel) をログインユーザーの所有にする。つまりエージェントは
        // Zaivern と同じ権限でそこの実行体を書き換えられる — この PR の
        // 脅威モデルそのもの。`/usr` に含まれるからといって通してはいけない。
        for p in [
            "/usr/local/bin/ruff",
            "/opt/homebrew/bin/rustfmt",
            "/opt/local/bin/black",
            "/home/linuxbrew/.linuxbrew/bin/ruff",
        ] {
            let got = classify_path(Path::new(p), &pol);
            assert_ne!(
                got,
                ExecTrust::SystemTrusted,
                "{p} を無条件にシステム扱いした"
            );
            assert!(!got.auto_runnable(), "{p} を無承認で走らせる判定にした");
        }
        // workspace が最優先 (システムの下に置かれていても)。
        let nested = unix_policy(Path::new("/usr/local/src/repo"), |_| None);
        assert_eq!(
            classify_path(Path::new("/usr/local/src/repo/bin/rustfmt"), &nested),
            ExecTrust::Workspace
        );
        // 正規化していない `..` は、文字列では根を決められない。
        assert_eq!(
            classify_path(Path::new("/usr/bin/../../tmp/evil/rustfmt"), &pol),
            ExecTrust::Unknown
        );
    }

    /// Windows の規則を **macOS / Linux の CI から**固定する。
    ///
    /// `cfg(windows)` で書くと、この表は Windows のランナーでしか
    /// 動かない (= 手元では 1 度も検査されない)。
    #[test]
    fn windowsでも利用者が書ける場所をシステムと同じに扱わない() {
        let env = |k: &str| {
            Some(match k {
                "USERPROFILE" => r"C:\Users\alice",
                "LOCALAPPDATA" => r"C:\Users\alice\AppData\Local",
                "APPDATA" => r"C:\Users\alice\AppData\Roaming",
                "TEMP" | "TMP" => r"C:\Users\alice\AppData\Local\Temp",
                "ProgramData" => r"C:\ProgramData",
                "SystemRoot" => r"C:\Windows",
                "SystemDrive" => "C:",
                "ProgramFiles" => r"C:\Program Files",
                "ProgramFiles(x86)" => r"C:\Program Files (x86)",
                _ => return None,
            }
            .to_string())
        };
        let pol = windows_policy(Path::new(r"C:\work\repo"), env);
        let table: &[(&str, ExecTrust)] = &[
            // 利用者が書ける場所 — **エージェントもここへ置ける**。
            (r"C:\Users\alice\bin\rustfmt.exe", ExecTrust::UserWritable),
            (
                r"C:\Users\alice\AppData\Local\Microsoft\WindowsApps\ruff.exe",
                ExecTrust::UserWritable,
            ),
            (
                r"C:\Users\alice\AppData\Roaming\npm\npm.cmd",
                ExecTrust::UserWritable,
            ),
            (r"C:\Users\alice\.cargo\bin\cargo.exe", ExecTrust::UserWritable),
            (r"C:\ProgramData\chocolatey\bin\ruff.exe", ExecTrust::UserWritable),
            (r"C:\Users\bob\bin\ruff.exe", ExecTrust::UserWritable),
            // 昇格が要る場所。
            (r"C:\Windows\System32\where.exe", ExecTrust::SystemTrusted),
            (r"C:\Windows\py.exe", ExecTrust::SystemTrusted),
            (r"C:\Program Files\Git\cmd\git.exe", ExecTrust::SystemTrusted),
            (
                r"C:\Program Files (x86)\tool\t.exe",
                ExecTrust::SystemTrusted,
            ),
            // workspace の中。
            (r"C:\work\repo\bin\rustfmt.exe", ExecTrust::Workspace),
            (r"C:\work\repo\.venv\Scripts\black.exe", ExecTrust::Workspace),
            // **`PATHEXT` で拾う綴りも同じ扱い。** `.cmd` / `.bat` は
            // `CreateProcess` が cmd.exe 越しに起こす経路なので、ここが
            // 緩むと workspace の中のバッチが「読むだけの検証」として動く。
            (r"C:\work\repo\bin\rustfmt.cmd", ExecTrust::Workspace),
            (r"C:\work\repo\tools\check.bat", ExecTrust::Workspace),
            (
                r"C:\Users\alice\AppData\Roaming\npm\npx.cmd",
                ExecTrust::UserWritable,
            ),
            // どちらとも言えない場所は、**システム扱いにしない**。
            (r"D:\misc\tool.exe", ExecTrust::Unknown),
            (r"\\server\share\tool.exe", ExecTrust::Unknown),
        ];
        for (p, want) in table {
            assert_eq!(
                classify_path(Path::new(p), &pol),
                *want,
                "{p} の区分が違う"
            );
        }
        // **大小は畳む** (`C:\USERS\...` で抜けられない)。
        assert_eq!(
            classify_path(Path::new(r"C:\USERS\Alice\BIN\rustfmt.exe"), &pol),
            ExecTrust::UserWritable
        );
        assert_eq!(
            classify_path(Path::new(r"c:/windows/system32/where.exe"), &pol),
            ExecTrust::SystemTrusted
        );
        // `canonicalize` が付ける拡張長プレフィクスでも同じ判定になる。
        assert_eq!(
            classify_path(Path::new(r"\\?\C:\work\repo\bin\rustfmt.exe"), &pol),
            ExecTrust::Workspace
        );
        // **名前が接頭辞として似ているだけの場所を巻き込まない。**
        assert_eq!(
            classify_path(Path::new(r"C:\work\repo-other\bin\x.exe"), &pol),
            ExecTrust::Unknown
        );
    }

    #[cfg(unix)]
    #[test]
    fn 普通のシステムpathの道具はこれまでどおり無承認で走れる() {
        // **締めすぎていないこと。** `/usr/bin` の道具まで承認待ちになると、
        // 誰も承認しなくなって関門そのものが形骸化する。
        let ws = tmp("system-path");
        std::fs::create_dir_all(&ws).unwrap();
        let mut checked = 0;
        for (dir, name) in [("/usr/bin", "env"), ("/bin", "sh"), ("/usr/bin", "cat")] {
            if !Path::new(dir).join(name).exists() {
                continue;
            }
            let got = resolve_in(name, &ws, Some(dir), None).expect("解決できるはず");
            assert_eq!(got.path, Path::new(dir).join(name));
            assert_eq!(
                got.trust,
                ExecTrust::SystemTrusted,
                "{dir}/{name} を無承認で走れない区分にした"
            );
            assert!(got.trust.auto_runnable());
            checked += 1;
        }
        if checked == 0 {
            eprintln!("[skip] 普通のシステムpathの道具 — /usr/bin も /bin も無い");
        }
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **綴りが「システム」でも、実際に書き換えられるなら信用しない。**
    ///
    /// 表は綴りしか見ない。Homebrew が `/usr/local` を利用者所有にする、
    /// 誰かが `/usr/bin` を世界書き込み可にしてしまう、といった環境では
    /// 綴りだけの判断が嘘になる。実測は**降格にだけ**効く。
    #[cfg(unix)]
    #[test]
    fn 書き換えられるシステム領域は実測で降格する() {
        use std::os::unix::fs::PermissionsExt;

        let ws = tmp("measured-ws");
        std::fs::create_dir_all(&ws).unwrap();
        // 「システムの場所」として表に載せた実験場。**実際には自分のもの。**
        let sys = tmp("measured-sys");
        let exe = put_exe(&sys, "zzz-measured");
        let canon = |p: &Path| {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .display()
                .to_string()
        };
        let sys_s = canon(&sys);
        let pol = TrustPolicy::new(&ws, &[], &[&sys_s], false);
        // 綴りだけの判断は「システム」。**解決した綴りで見る** — macOS の
        // 一時フォルダは `/var/folders/…` で実体が `/private/var/folders/…`
        // なので、素の綴りのままだと表と照合できない (実際に CI が落ちた)。
        assert_eq!(
            classify_path(Path::new(&canon(&exe)), &pol),
            ExecTrust::SystemTrusted,
            "前提: 表の上ではシステム"
        );

        // **世界書き込み可にすると、実測が降格させる。**
        // (uid での比較は root では行わないので、どの環境でも効くこちらで見る)
        let mut perm = std::fs::metadata(&exe).unwrap().permissions();
        perm.set_mode(0o777);
        std::fs::set_permissions(&exe, perm).unwrap();
        let got = resolve_with("zzz-measured", &pol, Some(&sys_s), None).expect("見つかるはず");
        assert_eq!(
            got.trust,
            ExecTrust::UserWritable,
            "誰でも書ける実行体をシステム扱いのまま通した"
        );
        assert!(!got.trust.auto_runnable(), "無承認で走らせる判定にした");

        // **置き場に書ければ、実体を差し替えられる** (消して置き直せる)。
        let mut perm = std::fs::metadata(&exe).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&exe, perm).unwrap();
        let mut dperm = std::fs::metadata(&sys).unwrap().permissions();
        dperm.set_mode(0o777);
        std::fs::set_permissions(&sys, dperm).unwrap();
        let got = resolve_with("zzz-measured", &pol, Some(&sys_s), None).expect("見つかるはず");
        assert_eq!(
            got.trust,
            ExecTrust::UserWritable,
            "誰でも書ける置き場をシステム扱いのまま通した"
        );

        // **上げる方向には効かない。** 実測が何を言おうと、workspace の
        // 中は workspace のまま (綴りの判断より緩くならない)。
        let inside = put_exe(&ws.join("bin"), "zzz-measured");
        let mut perm = std::fs::metadata(&inside).unwrap().permissions();
        perm.set_mode(0o555);
        std::fs::set_permissions(&inside, perm).unwrap();
        let ws_bin = canon(&ws.join("bin"));
        let got = resolve_with("zzz-measured", &pol, Some(&ws_bin), None);
        assert!(
            matches!(got, Err(ResolveError::Untrusted { .. })),
            "実測で workspace の実行体が通ってしまった: {got:?}"
        );

        std::fs::remove_dir_all(&ws).ok();
        std::fs::remove_dir_all(&sys).ok();
    }

    #[cfg(unix)]
    #[test]
    fn 前方の信用できない実体から後方の信用できる実体へ落ちない() {
        // **PATH の順がすべて。** 前方の `~/.local/bin/rustfmt` を飛ばして
        // 後方の `/usr/bin/rustfmt` を「判定した実体」にすると、
        // 判定と実行がずれる (利用者がシェルで打てば前方が動く)。
        // ここでは *昇格が要る場所* を試験用の表で作って、その順序だけを見る。
        let ws = tmp("no-fallback-ws");
        std::fs::create_dir_all(&ws).unwrap();
        let user_dir = tmp("no-fallback-user");
        let sys_dir = tmp("no-fallback-sys");
        put_exe(&user_dir, "zzz-probe");
        put_exe(&sys_dir, "zzz-probe");
        // **根は正規化して渡す。** macOS の一時フォルダは
        // `/var/folders/…` → `/private/var/folders/…` へ解決されるので、
        // 素のパスのままだと実体側の照合が外れて Unknown になる
        // (製品は正しいのに、テストだけが OS で落ちる形)。
        let canon = |p: &Path| {
            std::fs::canonicalize(p)
                .unwrap_or_else(|_| p.to_path_buf())
                .display()
                .to_string()
        };
        let (user_s, sys_s) = (canon(&user_dir), canon(&sys_dir));
        let pol = TrustPolicy::new(&ws, &[&user_s], &[&sys_s], false);

        // 信用できない側が前 → **そちらが答え**。後ろへ落ちない。
        let front = format!("{user_s}:{sys_s}");
        let got = resolve_with("zzz-probe", &pol, Some(&front), None).expect("見つかるはず");
        assert_eq!(
            got.path,
            Path::new(&user_s).join("zzz-probe"),
            "後ろの信用できる実体へ落ちた"
        );
        assert_eq!(got.trust, ExecTrust::UserWritable);
        assert!(!got.trust.auto_runnable(), "無承認で走らせる判定にした");

        // 逆順なら前方が答え — **「常に断る」になっていない**ことの対照。
        //
        // **区分そのものは実測で降格しうる** (実験場は動かしている人の
        // 所有なので、表がシステムだと言っても書き換えられる)。ここで
        // 見たいのは「どちらを選んだか」なので、選んだ実体で確かめる。
        let back = format!("{sys_s}:{user_s}");
        let got = resolve_with("zzz-probe", &pol, Some(&back), None).expect("見つかるはず");
        assert_eq!(got.path, Path::new(&sys_s).join("zzz-probe"));

        for d in [&ws, &user_dir, &sys_dir] {
            std::fs::remove_dir_all(d).ok();
        }
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
