//! Task Graph — 依存関係の検証・Ready 化・開発フェーズの算出。
//!
//! ## 何を守るのか
//!
//! 「並列に走らせたら後で衝突が見つかった」を作らないため、**配る前に**
//! 計画そのものを検査する。ここで弾くのは次の 7 つ:
//!
//! 1. 循環依存 (誰も Ready にならず、静かに止まる)
//! 2. 存在しない依存先 (永久に Pending のまま残る)
//! 3. 自己依存 (同上)
//! 4. 重複するタスクキー (依存の解決が先勝ちになる)
//! 5. 空の受入基準 (完了判定が「本人の申告」だけになる)
//! 6. ワークスペース外のファイル (境界を越えた書き込み)
//! 7. 危険な検証コマンド (`rm -rf` / push / deploy など)
//!
//! **どれも「動かしてみれば分かる」類ではない** — 循環依存は
//! 「なぜか誰も動かない」として現れ、原因が Task Graph だと気付くまでに
//! 時間が溶ける。

use std::collections::{BTreeMap, BTreeSet};

use super::model::{TaskId, TeamTask, TeamTaskState};
use super::validation_command::ValidationCommand;

/// 計画の不備 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlanIssue {
    /// 循環依存 (関わっているタスクを ID 順で並べる)。
    Cycle(Vec<TaskId>),
    /// 存在しない依存先。
    MissingDependency { task: TaskId, missing: String },
    /// 自分自身への依存。
    SelfDependency(TaskId),
    /// タスクキーの重複。
    DuplicateKey(String),
    /// タスク ID の重複。
    DuplicateId(TaskId),
    /// 受入基準が空。
    NoAcceptanceCriteria(TaskId),
    /// 検証コマンドが空。
    NoValidationCommand(TaskId),
    /// ワークスペースの外を指すファイル。
    FileOutsideWorkspace { task: TaskId, path: String },
    /// 危険な検証コマンド。
    DangerousCommand { task: TaskId, command: String },
    /// Definition of Done が空。
    NoDefinitionOfDone,
    /// タスクが 1 件も無い。
    NoTasks,
}

impl PlanIssue {
    /// 人へ出す説明。
    pub fn detail(&self) -> String {
        match self {
            PlanIssue::Cycle(ids) => {
                let list: Vec<String> = ids.iter().map(|i| format!("#{i}")).collect();
                format!("依存が循環しています: {}", list.join(" → "))
            }
            PlanIssue::MissingDependency { task, missing } => {
                format!("#{task} が存在しないタスク「{missing}」に依存しています")
            }
            PlanIssue::SelfDependency(t) => format!("#{t} が自分自身に依存しています"),
            PlanIssue::DuplicateKey(k) => format!("タスクキー「{k}」が重複しています"),
            PlanIssue::DuplicateId(i) => format!("タスク ID #{i} が重複しています"),
            PlanIssue::NoAcceptanceCriteria(t) => {
                format!("#{t} に受入基準がありません (完了を機械的に判定できません)")
            }
            PlanIssue::NoValidationCommand(t) => {
                format!("#{t} に検証コマンドがありません (検証なしで完了にはしません)")
            }
            PlanIssue::FileOutsideWorkspace { task, path } => {
                format!("#{task} の担当ファイル「{path}」がワークスペースの外を指しています")
            }
            PlanIssue::DangerousCommand { task, command } => {
                format!("#{task} の検証コマンド「{command}」は自動実行しません")
            }
            PlanIssue::NoDefinitionOfDone => "Definition of Done が空です".to_string(),
            PlanIssue::NoTasks => "タスクが 1 件もありません".to_string(),
        }
    }
}

/// **自動では絶対に走らせない語。**
///
/// MVP の安全条件 (push / merge / deploy / 破壊的操作 / 権限昇格) を
/// 検証コマンドの中身で照合する。判定は語単位で、パス風のトークンは
/// 素通しする — `src/deploy_test.rs` を「deploy」と読むと、まともな
/// テストコマンドまで止まってしまう。
pub const FORBIDDEN_COMMAND_WORDS: &[&str] = &[
    "push",
    "deploy",
    "release",
    "publish",
    "merge",
    "rebase",
    "reset",
    "clean",
    "rm",
    "rmdir",
    "del",
    // **`format` は入れない。** ディスクの初期化 (`format C:`) は実行体の
    // 名前なので、許可リストに `format` が無い時点で通らない。引数として
    // 見ると `ruff format --check .` / `dotnet format` のような**整形の
    // サブコマンド**を巻き込むだけで、守りにならない (書き換えるかどうかは
    // `read_only_mode` が旗で見る)。
    "mkfs",
    "dd",
    "shutdown",
    "reboot",
    "sudo",
    "su",
    "doas",
    "chmod",
    "chown",
    "curl",
    "wget",
    "ssh",
    "scp",
    "kubectl",
    "helm",
    "terraform",
    "aws",
    "gcloud",
    "az",
    "docker",
    "npm-publish",
    "shred",
];

/// シェルのメタ文字。**コマンドは文字列として連結せず、語に分けて扱う**ので、
/// これらが出てきた時点で「素のシェル文字列」と判断して拒否する。
/// シェルのメタ文字。**コマンドは文字列として連結せず、語に分けて扱う**
/// ので、これらが出てきた時点で「素のシェル文字列」と判断して拒否する。
///
/// **Windows の cmd.exe が特別扱いする字も入れる。** `.cmd` / `.bat` の
/// 実行体はどうしても cmd.exe を経由するので (std がそう起こす)、
/// `%VAR%` の展開・`^` の脱出・`!` の遅延展開が効く。判定した文字列と
/// cmd.exe が解釈する文字列が別物になる経路を、最初から塞ぐ。
///
/// **入れるのは「std が安全に逃がせないもの」だけ。** `^` `(` `)` は
/// std のバッチ用の逃がし方が面倒を見るうえ、検証コマンドの引数として
/// ごく普通に出てくる (`pytest -k "not (slow or db)"` /
/// `npm test -- --grep "^auth"`)。ここへ入れると、当たり前の SPEC が
/// `Forbidden` → `NeedsUser` で行き止まる。**fail-closed なら安全、
/// にはならない** — 誰も直せない止まり方をするだけである。
/// `%` は std がバッチの引数として**逃がせずに拒否する**字なので、
/// 分かりやすい理由をつけてこちらで先に断る。
pub const SHELL_METACHARS: &[char] = &[
    ';', '|', '&', '>', '<', '`', '$', '\n', '\r', // unix
    '%', '!', '"', '\'', // windows の cmd.exe
];

/// 検証コマンドの危険度。**「安全か危険か」の 2 値では足りない。**
///
/// 名前だけでは決まらない、が要点:
///
/// * `cargo test` にシェルのメタ文字は 1 つも無いが、`build.rs` /
///   `#[test]` の中身 / `conftest.py` / `Makefile` / `package.json` の
///   `scripts` を通じて**リポジトリ内の任意コードを実行できる**
/// * `black --check .` は読むだけだが、`black .` は**ファイルを書き換える**。
///   同じ実行体で、旗ひとつで意味が変わる
///
/// なので判定は**実行体と引数の両方**を見る。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValidationRisk {
    /// **読むだけ。** リポジトリのコードを実行せず、workspace も書き換えない。
    /// 自動で実行してよい唯一の段。
    ReadOnly,
    /// テスト・ビルド・スクリプトなど、**リポジトリ内のコードを実行しうる**もの。
    /// 隔離 (sandbox) が無い以上、人の承認を通してから実行する。
    RepositoryCodeExecution,
    /// **workspace を書き換えうる。** 整形や自動修正。人の承認を通す。
    WorkspaceMutation,
    /// 実行しない。パス指定・シェル・publish / deploy / push / sudo /
    /// 破壊的操作など。
    Forbidden,
}

impl ValidationRisk {
    /// 人の承認なしで実行してよいか。
    pub fn auto_runnable(self) -> bool {
        self == ValidationRisk::ReadOnly
    }

    /// 承認を通せば実行してよいか (`Forbidden` は承認しても実行しない)。
    pub fn needs_approval(self) -> bool {
        matches!(
            self,
            ValidationRisk::RepositoryCodeExecution | ValidationRisk::WorkspaceMutation
        )
    }

    /// 表示用の安定 ID。
    pub fn key(self) -> &'static str {
        match self {
            ValidationRisk::ReadOnly => "read_only",
            ValidationRisk::RepositoryCodeExecution => "repository_code_execution",
            ValidationRisk::WorkspaceMutation => "workspace_mutation",
            ValidationRisk::Forbidden => "forbidden",
        }
    }
}

/// **読むだけにできる道具**と、そう言える条件。
///
/// ここに載るのは「リポジトリのコードを実行しない」ものだけ。そのうえで
/// **旗しだいで書き換える**ので、読むだけだと言い切れる形を明示する。
///
/// 迷ったら載せない — 設定ファイルから任意のコードを読み込むもの
/// (`eslint` / `prettier` の JS 設定、`mypy` / `pylint` のプラグイン) は、
/// 名前が「検査するだけ」に見えても実行しうる。
/// 「読むだけ」だと言い切れる形。**知っている旗だけで組まれているとき**に限る。
///
/// 危険な旗を数え上げる (deny) 形では守れない。実際に 4 つ漏れた:
///
/// * `rustfmt --check --print-config default out.toml` — `--print-config` は
///   整形モードより**手前**で処理されるので `--check` では止まらず、
///   指定したパスへファイルを書く (workspace の外でも書ける)
/// * `black --extend-exclude --check .` — Click は次の語を値として食うので
///   `--check` は旗ではなく値。black は書き換えモードで動く
/// * `ruff check --fix-only .` / `--add-noqa .` — どちらも書き換えるが
///   `--fix` とは綴りが違う
/// * `shellcheck -x a.sh` — `# shellcheck source=…` を辿って
///   ディスクのどこでも読むようになる
///
/// 数え上げる側を逆にする: **知っている旗の集合**を持ち、そこに無い旗が
/// 1 つでもあれば「読むだけ」とは言わない (承認へ回す)。道具に新しい旗が
/// 増えても、こちらが黙って通すことはない。
struct ReadOnlyTool {
    /// 「読むだけ」になるために**旗の位置で**必要な旗 (いずれか 1 つ)。
    /// 空なら旗なしでも読むだけ。
    requires_any: &'static [&'static str],
    /// 知っている旗 (`requires_any` と `takes_value` も含める)。
    known: &'static [&'static str],
    /// **次の語を値として食う旗。** これを知らないと
    /// `black --extend-exclude --check .` の `--check` を旗と読む。
    takes_value: &'static [&'static str],
}

/// shellcheck — 既定で読むだけ。`-x` / `--source-path` (`-P`) は
/// **ディスクのどこでも読む**ようになるので `known` に入れない。
const SHELLCHECK: ReadOnlyTool = ReadOnlyTool {
    requires_any: &[],
    known: &[
        "-a",
        "--check-sourced",
        "-C",
        "--color",
        "-e",
        "--exclude",
        "-f",
        "--format",
        "-i",
        "--include",
        "-o",
        "--enable",
        "-s",
        "--shell",
        "-S",
        "--severity",
        "--norc",
        "--no-rc",
        "--extended-analysis",
    ],
    takes_value: &[
        "-e",
        "--exclude",
        "-f",
        "--format",
        "-i",
        "--include",
        "-o",
        "--enable",
        "-s",
        "--shell",
        "-S",
        "--severity",
    ],
};

/// rustfmt — `--check` があるときだけ読むだけ。`--print-config` / `--emit` /
/// `--backup` は `--check` があっても書くので `known` に入れない。
const RUSTFMT: ReadOnlyTool = ReadOnlyTool {
    requires_any: &["--check"],
    known: &[
        "--check",
        "--edition",
        "--color",
        "--config",
        "--config-path",
        "--files-with-diff",
        "-l",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--unstable-features",
    ],
    takes_value: &["--edition", "--color", "--config", "--config-path"],
};

/// black — `--check` / `--diff` があるときだけ読むだけ。
/// **値を食う旗を漏らさない** (`--extend-exclude --check .` で `--check` が
/// 値として消え、workspace 全部が黙って書き換わる)。
const BLACK: ReadOnlyTool = ReadOnlyTool {
    requires_any: &["--check", "--diff"],
    known: &[
        "--check",
        "--diff",
        "--color",
        "--no-color",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--fast",
        "--safe",
        "--preview",
        "--skip-string-normalization",
        "-S",
        "--skip-magic-trailing-comma",
        "-C",
        "--line-length",
        "-l",
        "--target-version",
        "-t",
        "--include",
        "--exclude",
        "--extend-exclude",
        "--force-exclude",
        "--config",
        "--workers",
        "-W",
        "--required-version",
    ],
    takes_value: &[
        "--line-length",
        "-l",
        "--target-version",
        "-t",
        "--include",
        "--exclude",
        "--extend-exclude",
        "--force-exclude",
        "--config",
        "--workers",
        "-W",
        "--required-version",
        "--stdin-filename",
        "--code",
        "-c",
    ],
};

/// ruff の値を食う旗。**サブコマンドを探す前にも要る** —
/// `ruff --config x.toml check .` の `x.toml` をサブコマンドと読まないため。
const RUFF_VALUE_FLAGS: &[&str] = &[
    "--select",
    "--ignore",
    "--extend-select",
    "--extend-ignore",
    "--per-file-ignores",
    "--exclude",
    "--extend-exclude",
    "--line-length",
    "--target-version",
    "--output-format",
    "--config",
    "--cache-dir",
    "--stdin-filename",
    "-e",
    "-n",
];

/// `ruff check` — 既定で読むだけ。`--fix` / `--fix-only` / `--add-noqa` /
/// `--unsafe-fixes` は書くので `known` に入れない。
const RUFF_CHECK: ReadOnlyTool = ReadOnlyTool {
    requires_any: &[],
    known: &[
        "--select",
        "--ignore",
        "--extend-select",
        "--extend-ignore",
        "--per-file-ignores",
        "--exclude",
        "--extend-exclude",
        "--line-length",
        "--target-version",
        "--output-format",
        "--config",
        "--cache-dir",
        "--no-cache",
        "--statistics",
        "--show-files",
        "--show-settings",
        "--diff",
        "--quiet",
        "-q",
        "--silent",
        "-s",
        "--verbose",
        "-v",
        "--no-respect-gitignore",
        "--respect-gitignore",
        "--isolated",
        "--preview",
        "--no-preview",
        "--exit-zero",
        "--exit-non-zero-on-fix",
        "-e",
        "-n",
    ],
    takes_value: RUFF_VALUE_FLAGS,
};

/// `ruff format` — `--check` / `--diff` があるときだけ読むだけ。
const RUFF_FORMAT: ReadOnlyTool = ReadOnlyTool {
    requires_any: &["--check", "--diff"],
    known: &[
        "--check",
        "--diff",
        "--exclude",
        "--extend-exclude",
        "--line-length",
        "--target-version",
        "--config",
        "--cache-dir",
        "--no-cache",
        "--quiet",
        "-q",
        "--verbose",
        "-v",
        "--isolated",
        "--preview",
        "--no-preview",
        "--respect-gitignore",
        "--no-respect-gitignore",
    ],
    takes_value: RUFF_VALUE_FLAGS,
};

/// この実行体と引数で使う「読むだけ」の型を選ぶ。
fn read_only_tool(cmd: &ValidationCommand) -> Option<&'static ReadOnlyTool> {
    match cmd.executable.as_str() {
        "shellcheck" => Some(&SHELLCHECK),
        "rustfmt" => Some(&RUSTFMT),
        "black" => Some(&BLACK),
        "ruff" => match cmd.first_positional(RUFF_VALUE_FLAGS) {
            Some("check") => Some(&RUFF_CHECK),
            Some("format") => Some(&RUFF_FORMAT),
            _ => None,
        },
        _ => None,
    }
}

fn read_only_mode(cmd: &ValidationCommand) -> Option<bool> {
    let tool = match read_only_tool(cmd) {
        Some(t) => t,
        // サブコマンド無しの `ruff .` は既定が `check` だった版がある。
        // **どちらとも言い切れないので書き換える側へ倒す。**
        None if cmd.executable == "ruff" => return Some(false),
        None => return None,
    };
    let flags = cmd.flags_in_flag_position(tool.takes_value);
    // **知らない旗が 1 つでもあれば、読むだけとは言わない。**
    if flags.iter().any(|f| !tool.known.contains(&f.as_str())) {
        return Some(false);
    }
    if tool.requires_any.is_empty() {
        return Some(true);
    }
    Some(
        flags
            .iter()
            .any(|f| tool.requires_any.contains(&f.as_str())),
    )
}

/// 自動実行の対象にしてよいコマンド名 (実行ファイルの名前そのもの)。
///
/// **ここに載っていても「安全」ではない** — 危険度は [`classify`] が決める。
const ALLOWED_HEAD: &[&str] = &[
    "cargo",
    "rustc",
    "rustfmt",
    "clippy-driver",
    "make",
    "just",
    "npm",
    "pnpm",
    "yarn",
    "bun",
    "node",
    "deno",
    "python",
    "python3",
    "pytest",
    "go",
    "gradle",
    "mvn",
    "dotnet",
    "swift",
    "ruby",
    "bundle",
    "rake",
    "php",
    "composer",
    "dart",
    "flutter",
    "zig",
    "ctest",
    "cmake",
    "ninja",
    "bazel",
    "tox",
    "mix",
    "sbt",
    "elixir",
    "jest",
    "vitest",
    "eslint",
    "prettier",
    "tsc",
    "biome",
    "ruff",
    "black",
    "mypy",
    "pylint",
    "shellcheck",
    "zai",
];

/// 実行ファイルの指定にパスが混ざっているか。
///
/// **混ざっていたら実行しない。** basename だけで許可を決めると
/// `/tmp/cargo test` が「`cargo` だから許可」になり、実際に起動されるのは
/// `/tmp/cargo` — 攻撃者が置いた任意の実行体になる。PATH から解決される
/// 素の名前だけを通す (その解決も自分で行う:
/// [`super::validation_command::resolve_in`])。
fn head_has_path(head: &str) -> bool {
    if head.contains('/') || head.contains('\\') {
        return true;
    }
    // Windows のドライブ指定 (`C:cargo` は「C: のカレント」からの相対)。
    let b = head.as_bytes();
    if b.len() >= 2 && b[1] == b':' && (b[0] as char).is_ascii_alphabetic() {
        return true;
    }
    // 拡張子つきの実行体指定も、PATH 解決の素の名前ではない。
    head.contains('.')
}

/// 検証コマンドの危険度を決める。**許可されたものだけを通す (allowlist)**。
pub fn classify(cmd: &ValidationCommand) -> ValidationRisk {
    classify_why(cmd).0
}

/// 危険度と、`Forbidden` のときの理由。
pub fn classify_why(cmd: &ValidationCommand) -> (ValidationRisk, String) {
    let no = |why: String| (ValidationRisk::Forbidden, why);
    let head = cmd.executable.trim();
    if head.is_empty() {
        return no("空のコマンド".to_string());
    }
    if cmd.args.len() > super::validation_command::ARGS_MAX {
        return no("引数が多すぎます".to_string());
    }
    // **語のどこにもシェルのメタ文字を入れない。** 引用符で割ったあとの
    // 語を見る — 「文字列としては安全に見えるが、どこかの層がもう一度
    // 解釈する」形を残さないため。Windows の cmd.exe が特別扱いする字も
    // 含める (`%VAR%` の展開で、判定した文字列と実行される文字列がずれる)。
    //
    // 判定そのものは [`shell_syntax_reason`] 1 か所にある。Planner は
    // 同じ関数を呼んで**構文の誤り**として扱う (人が直せるように) が、
    // ここでは従来どおり `Forbidden` — 二重の防御であって、**判定の
    // 実装は 1 つ**である。
    if let Some(why) = shell_syntax_reason(cmd) {
        return no(why);
    }
    // 実行するのは実体だけ。`sh -c "..."` のような入れ子は通さない。
    if head_has_path(head) {
        return no(format!(
            "`{head}` はパス指定です。PATH から解決される名前だけを使ってください"
        ));
    }
    if !ALLOWED_HEAD.contains(&head) {
        return no(format!("`{head}` は自動実行を許可していないコマンドです"));
    }
    // 引数側に破壊的な語が混ざっていないか (`cargo publish` など)。
    for w in &cmd.args {
        let w_low = w.trim_start_matches('-').to_ascii_lowercase();
        if FORBIDDEN_COMMAND_WORDS.contains(&w_low.as_str()) {
            return no(format!("`{w}` を含む操作は自動実行しません"));
        }
    }
    match read_only_mode(cmd) {
        Some(true) => (ValidationRisk::ReadOnly, String::new()),
        // 名前は読むだけの道具だが、この旗では書き換える。
        Some(false) => (ValidationRisk::WorkspaceMutation, String::new()),
        None => (ValidationRisk::RepositoryCodeExecution, String::new()),
    }
}

/// シェルとしての再解釈が要る形になっていないか。
///
/// **`None` なら「割れている」だけで、実行してよいという意味ではない。**
/// 危険度は [`classify_why`] が決める。
///
/// ここを [`classify_why`] と Planner の両方が呼ぶ。2 つ持つと、
/// 「Planner は通したのに実行時に断られる」というずれが出る。
pub fn shell_syntax_reason(cmd: &ValidationCommand) -> Option<String> {
    let head = cmd.executable.trim();
    for w in std::iter::once(head).chain(cmd.args.iter().map(|s| s.as_str())) {
        if let Some(c) = w.chars().find(|c| SHELL_METACHARS.contains(c)) {
            return Some(format!("シェルのメタ文字 `{c}` は使えません"));
        }
    }
    None
}

/// 受け取れない理由。**構文の問題と、方針の問題を分ける。**
///
/// 混ぜると利用者が直しようがない。`npm test && npm run lint` は
/// **書き方**を直せば通る (2 行に分ける) が、`git push` は何をしても
/// 通らない — 同じ「拒否」で返すと、前者を直せる人が直さなくなる。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandReject {
    /// 語に割れない / シェルの解釈が要る。**書き方の問題。**
    Syntax(String),
    /// 割れたが、実行を許していない。**方針の問題。**
    Forbidden(String),
}

impl CommandReject {
    pub fn reason(&self) -> &str {
        match self {
            CommandReject::Syntax(s) | CommandReject::Forbidden(s) => s,
        }
    }
}

/// 文字列 1 本を検証コマンドとして受け取る (SPEC / 自動決定の入口)。
///
/// **構造へ直してから判定する。** 文字列のまま判定して、あとで別の層が
/// もう一度割ると、判定したものと実行するものがずれる。
///
/// 断る理由は [`CommandReject`] で**種類を分けて**返す — 呼び出し側が
/// 「書き方を直せば通る」と「何をしても通らない」を区別できるように。
pub fn parse_command(line: &str) -> Result<ValidationCommand, CommandReject> {
    let cmd = ValidationCommand::parse(line).map_err(CommandReject::Syntax)?;
    if let Some(why) = shell_syntax_reason(&cmd) {
        return Err(CommandReject::Syntax(why));
    }
    check_command(&cmd).map_err(CommandReject::Forbidden)?;
    Ok(cmd)
}

/// 検証コマンドとして**そもそも実行してよいか** (`Forbidden` でないか)。
///
/// 返り値が `Err` なら、その文面をそのまま人へ見せる。
/// **`Ok` は「安全」という意味ではない** — 承認が要るものも `Ok` になる。
pub fn check_command(cmd: &ValidationCommand) -> Result<(), String> {
    match classify_why(cmd) {
        (ValidationRisk::Forbidden, why) => Err(why),
        _ => Ok(()),
    }
}

/// 担当ファイルのパターンがワークスペースの内側に収まっているか。
///
/// 実際のファイルシステムには触らない (計画時点では存在しないファイルを
/// 指してよい)。見るのは**形**だけ — 絶対パス・`..` での脱出・
/// Windows のドライブ指定・UNC を弾く。
pub fn path_inside_workspace(pat: &str) -> bool {
    let p = pat.trim();
    if p.is_empty() {
        return false;
    }
    if p.starts_with('/') || p.starts_with('\\') {
        return false;
    }
    // `C:\...` / `C:/...`
    let bytes = p.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' && (bytes[0] as char).is_ascii_alphabetic() {
        return false;
    }
    if p.starts_with("~") {
        return false;
    }
    // `..` を含む区間があれば脱出しうる。深さを数えず、形で断る (fail-closed)。
    p.split(['/', '\\']).all(|seg| seg != "..")
}

/// 計画全体を検証する。**問題が 1 つでもあれば配らない。**
pub fn validate_plan(tasks: &[TeamTask], definition_of_done: &[String]) -> Vec<PlanIssue> {
    let mut issues = Vec::new();
    if definition_of_done.is_empty() {
        issues.push(PlanIssue::NoDefinitionOfDone);
    }
    if tasks.is_empty() {
        issues.push(PlanIssue::NoTasks);
        return issues;
    }
    // **「検証を 1 本も持たない計画」と「1 本だけ抜けている計画」を分ける。**
    // 前者は道具が無いだけ (素の HTML など) なので通す。後者は抜け穴なので
    // 従来どおり断る。
    let any_validation = tasks.iter().any(|t| !t.validation_commands.is_empty());

    let mut seen_keys: BTreeSet<&str> = BTreeSet::new();
    let mut seen_ids: BTreeSet<TaskId> = BTreeSet::new();
    for t in tasks {
        if !seen_keys.insert(t.key.as_str()) {
            issues.push(PlanIssue::DuplicateKey(t.key.clone()));
        }
        if !seen_ids.insert(t.id) {
            issues.push(PlanIssue::DuplicateId(t.id));
        }
    }

    let ids: BTreeSet<TaskId> = tasks.iter().map(|t| t.id).collect();
    for t in tasks {
        for d in &t.dependencies {
            if *d == t.id {
                issues.push(PlanIssue::SelfDependency(t.id));
            } else if !ids.contains(d) {
                issues.push(PlanIssue::MissingDependency {
                    task: t.id,
                    missing: format!("#{d}"),
                });
            }
        }
        // レビュータスクは実装タスクの受入基準を引き継ぐので、自前の
        // 受入基準を要求しない (要求すると計画が二重に膨らむ)。
        if t.review_of.is_none() {
            if t.acceptance_criteria.is_empty() {
                issues.push(PlanIssue::NoAcceptanceCriteria(t.id));
            }
            // **検証コマンドが 1 本も無い計画そのものは通す。**
            //
            // 素の HTML やデザインだけのフォルダには、走らせられる検証が
            // 存在しない。そこで断ると Team がその手の仕事に使えなくなる。
            // 完了は**レビュー承認だけ**で決まる状態になるので、盤面が
            // それを出す (`TeamSnapshot::unvalidated`)。
            //
            // **道具がある計画では、従来どおり全タスクに要求する。**
            // 1 本でも検証を持つ計画で「このタスクだけ検証なし」を許すと、
            // 検証の抜け穴を 1 タスク作るだけで完了を素通りさせられる。
            if any_validation && t.validation_commands.is_empty() {
                issues.push(PlanIssue::NoValidationCommand(t.id));
            }
        }
        for f in &t.files {
            if !path_inside_workspace(f) {
                issues.push(PlanIssue::FileOutsideWorkspace {
                    task: t.id,
                    path: f.clone(),
                });
            }
        }
        for c in &t.validation_commands {
            if check_command(c).is_err() {
                issues.push(PlanIssue::DangerousCommand {
                    task: t.id,
                    command: c.display(),
                });
            }
        }
    }

    if let Some(cycle) = find_cycle(tasks) {
        issues.push(PlanIssue::Cycle(cycle));
    }
    issues
}

/// 依存の循環を 1 つ見つける (無ければ `None`)。
///
/// 決定的にするため、走査は ID の昇順で行う。
pub fn find_cycle(tasks: &[TeamTask]) -> Option<Vec<TaskId>> {
    let ids: BTreeSet<TaskId> = tasks.iter().map(|t| t.id).collect();
    let deps: BTreeMap<TaskId, Vec<TaskId>> = tasks
        .iter()
        .map(|t| {
            let mut d: Vec<TaskId> = t
                .dependencies
                .iter()
                .copied()
                .filter(|x| ids.contains(x) && *x != t.id)
                .collect();
            d.sort_unstable();
            (t.id, d)
        })
        .collect();

    // 色塗り DFS。0=未訪問 1=訪問中 2=完了
    let mut color: BTreeMap<TaskId, u8> = ids.iter().map(|i| (*i, 0u8)).collect();
    let mut stack: Vec<TaskId> = Vec::new();

    fn dfs(
        node: TaskId,
        deps: &BTreeMap<TaskId, Vec<TaskId>>,
        color: &mut BTreeMap<TaskId, u8>,
        stack: &mut Vec<TaskId>,
    ) -> Option<Vec<TaskId>> {
        color.insert(node, 1);
        stack.push(node);
        for next in deps.get(&node).map(|v| v.as_slice()).unwrap_or(&[]) {
            match color.get(next).copied().unwrap_or(0) {
                0 => {
                    if let Some(c) = dfs(*next, deps, color, stack) {
                        return Some(c);
                    }
                }
                1 => {
                    // stack の中に next が居るので、そこから先が循環。
                    let at = stack.iter().position(|x| x == next).unwrap_or(0);
                    let mut cyc: Vec<TaskId> = stack[at..].to_vec();
                    cyc.push(*next);
                    return Some(cyc);
                }
                _ => {}
            }
        }
        stack.pop();
        color.insert(node, 2);
        None
    }

    for id in &ids {
        if color.get(id).copied().unwrap_or(0) == 0 {
            if let Some(c) = dfs(*id, &deps, &mut color, &mut stack) {
                return Some(c);
            }
            stack.clear();
        }
    }
    None
}

/// 依存が全部 [`TeamTaskState::Completed`] のタスクを列挙する
/// (いま `Pending` のものだけ)。**Ready 化はここでしか起きない。**
pub fn newly_ready(tasks: &[TeamTask]) -> Vec<TaskId> {
    let done: BTreeSet<TaskId> = tasks
        .iter()
        .filter(|t| t.state == TeamTaskState::Completed)
        .map(|t| t.id)
        .collect();
    let mut out: Vec<TaskId> = tasks
        .iter()
        .filter(|t| t.state == TeamTaskState::Pending)
        .filter(|t| t.dependencies.iter().all(|d| done.contains(d)))
        .map(|t| t.id)
        .collect();
    out.sort_unstable();
    out
}

/// クリティカルパス上のタスク (そこから続く仕事がいちばん長いもの) の
/// 「深さ」。スケジューラの優先順位に使う。値が大きいほど先に着手すべき。
pub fn critical_depth(tasks: &[TeamTask]) -> BTreeMap<TaskId, u32> {
    let mut children: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    let ids: BTreeSet<TaskId> = tasks.iter().map(|t| t.id).collect();
    for t in tasks {
        for d in &t.dependencies {
            if ids.contains(d) {
                children.entry(*d).or_default().push(t.id);
            }
        }
    }
    let mut depth: BTreeMap<TaskId, u32> = ids.iter().map(|i| (*i, 0u32)).collect();
    // 循環があると収束しないので、上限つきで回す (検証済みなら 1 周で足りる)。
    let limit = tasks.len().saturating_add(1);
    for _ in 0..limit {
        let mut changed = false;
        for id in ids.iter().rev() {
            let best = children
                .get(id)
                .map(|cs| {
                    cs.iter()
                        .map(|c| depth.get(c).copied().unwrap_or(0) + 1)
                        .max()
                        .unwrap_or(0)
                })
                .unwrap_or(0);
            if depth.get(id).copied().unwrap_or(0) < best {
                depth.insert(*id, best);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    depth
}

/// 開発フェーズ。**Task Graph から計算する。手動の状態を別に持たない。**
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    GoalAnalysis,
    Architecture,
    Implementation,
    Review,
    Integration,
    FinalValidation,
}

impl Phase {
    pub fn key(self) -> &'static str {
        match self {
            Phase::GoalAnalysis => "goal_analysis",
            Phase::Architecture => "architecture",
            Phase::Implementation => "implementation",
            Phase::Review => "review",
            Phase::Integration => "integration",
            Phase::FinalValidation => "final_validation",
        }
    }

    pub const ALL: [Phase; 6] = [
        Phase::GoalAnalysis,
        Phase::Architecture,
        Phase::Implementation,
        Phase::Review,
        Phase::Integration,
        Phase::FinalValidation,
    ];
}

/// フェーズの進み具合。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PhaseStatus {
    Waiting,
    Running,
    Done,
}

impl PhaseStatus {
    pub fn key(self) -> &'static str {
        match self {
            PhaseStatus::Waiting => "waiting",
            PhaseStatus::Running => "running",
            PhaseStatus::Done => "done",
        }
    }
}

/// タスクがどのフェーズに属するか (役割で決まる)。
fn phase_of(t: &TeamTask) -> Phase {
    use super::model::TeamRole as R;
    match t.role {
        R::Planner | R::TeamLead => Phase::GoalAnalysis,
        R::Architect => Phase::Architecture,
        R::Implementer => Phase::Implementation,
        R::Tester => Phase::Review,
        R::Reviewer => Phase::Review,
        R::Integrator => Phase::Integration,
    }
}

/// フェーズ一覧と、それぞれの進み具合を Task Graph から算出する。
///
/// 最終検証 (`FinalValidation`) はタスクを持たない集約フェーズで、
/// 「全タスク完了 + Goal の DoD 判定」がそのまま状態になる。
pub fn phases(tasks: &[TeamTask], goal_completed: bool) -> Vec<(Phase, PhaseStatus)> {
    let mut out = Vec::new();
    for p in Phase::ALL {
        if p == Phase::FinalValidation {
            let all_done =
                !tasks.is_empty() && tasks.iter().all(|t| t.state == TeamTaskState::Completed);
            let st = if goal_completed {
                PhaseStatus::Done
            } else if all_done {
                PhaseStatus::Running
            } else {
                PhaseStatus::Waiting
            };
            out.push((p, st));
            continue;
        }
        let mine: Vec<&TeamTask> = tasks.iter().filter(|t| phase_of(t) == p).collect();
        let st = if mine.is_empty() {
            // タスクが無いフェーズは「通過済み」として扱う。
            // 空のまま Waiting にすると、永久に待っているように見える。
            PhaseStatus::Done
        } else if mine.iter().all(|t| t.state == TeamTaskState::Completed) {
            PhaseStatus::Done
        } else if mine.iter().any(|t| t.state != TeamTaskState::Pending) {
            PhaseStatus::Running
        } else {
            PhaseStatus::Waiting
        };
        out.push((p, st));
    }
    out
}

/// いま進行中のフェーズ (先頭の Running、無ければ最初の Waiting)。
pub fn current_phase(tasks: &[TeamTask], goal_completed: bool) -> Phase {
    let ps = phases(tasks, goal_completed);
    ps.iter()
        .find(|(_, s)| *s == PhaseStatus::Running)
        .or_else(|| ps.iter().find(|(_, s)| *s == PhaseStatus::Waiting))
        .map(|(p, _)| *p)
        .unwrap_or(Phase::FinalValidation)
}

/// Goal を完了にしてよいか。**「エージェントが完了と言った」では通らない。**
///
/// 条件:
/// 1. タスクが 1 件以上ある
/// 2. 全タスクが `Completed`
/// 3. 全タスクの検証が要求どおり成功している
/// 4. 全タスクのレビューが承認されている (レビュー不要なタスクを除く)
/// 5. Definition of Done が空でない
pub fn goal_done(tasks: &[TeamTask], definition_of_done: &[String], review_required: bool) -> bool {
    if tasks.is_empty() || definition_of_done.is_empty() {
        return false;
    }
    tasks.iter().all(|t| {
        if t.state != TeamTaskState::Completed {
            return false;
        }
        // レビュータスク自身は「対象を見た」ことが仕事なので、自前の検証も
        // レビューも要求しない (要求すると入れ子の無限後退になる)。
        if t.review_of.is_some() {
            return true;
        }
        t.validation.passed(&t.validation_commands) && (!review_required || t.review.approved())
    })
}

/// 進捗率 (0.0〜1.0)。完了タスク数 / 全タスク数。
pub fn progress(tasks: &[TeamTask]) -> f32 {
    if tasks.is_empty() {
        return 0.0;
    }
    let done = tasks
        .iter()
        .filter(|t| t.state == TeamTaskState::Completed)
        .count();
    done as f32 / tasks.len() as f32
}

#[cfg(test)]
mod tests {
    use super::super::testkit::task;
    use super::*;

    #[test]
    fn 依存が終わるまでreadyにならない() {
        let a = task(1, "a", &[]);
        let mut b = task(2, "b", &[1]);
        b.state = TeamTaskState::Pending;
        let tasks = vec![a, b];
        assert_eq!(newly_ready(&tasks), vec![1], "b はまだ Ready にならない");
    }

    #[test]
    fn 依存が完了したらreadyになる() {
        let mut a = task(1, "a", &[]);
        a.state = TeamTaskState::Completed;
        let b = task(2, "b", &[1]);
        assert_eq!(newly_ready(&[a, b]), vec![2]);
    }

    #[test]
    fn 循環依存を見つける() {
        let a = task(1, "a", &[2]);
        let b = task(2, "b", &[1]);
        let c = find_cycle(&[a.clone(), b.clone()]).expect("循環を見つけるべき");
        assert!(c.contains(&1) && c.contains(&2), "{c:?}");
        let issues = validate_plan(&[a, b], &["done".into()]);
        assert!(issues.iter().any(|i| matches!(i, PlanIssue::Cycle(_))));
    }

    #[test]
    fn 三つ巴の循環も見つける() {
        let t = vec![task(1, "a", &[3]), task(2, "b", &[1]), task(3, "c", &[2])];
        assert!(find_cycle(&t).is_some());
    }

    #[test]
    fn 存在しない依存を拒否する() {
        let a = task(1, "a", &[99]);
        let issues = validate_plan(&[a], &["done".into()]);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, PlanIssue::MissingDependency { .. })),
            "{issues:?}"
        );
    }

    #[test]
    fn 自己依存を拒否する() {
        let a = task(1, "a", &[1]);
        let issues = validate_plan(&[a], &["done".into()]);
        assert!(issues
            .iter()
            .any(|i| matches!(i, PlanIssue::SelfDependency(1))));
    }

    #[test]
    fn 重複キーを拒否する() {
        let a = task(1, "same", &[]);
        let b = task(2, "same", &[]);
        let issues = validate_plan(&[a, b], &["done".into()]);
        assert!(issues
            .iter()
            .any(|i| matches!(i, PlanIssue::DuplicateKey(_))));
    }

    #[test]
    fn 受入基準が空なら拒否する() {
        let mut a = task(1, "a", &[]);
        a.acceptance_criteria.clear();
        let issues = validate_plan(&[a], &["done".into()]);
        assert!(issues
            .iter()
            .any(|i| matches!(i, PlanIssue::NoAcceptanceCriteria(1))));
    }

    #[test]
    fn ワークスペース外のファイルを拒否する() {
        for bad in [
            "/etc/passwd",
            "../outside/x.rs",
            "C:\\Windows\\x.rs",
            "~/secrets",
            "a/../../b",
        ] {
            assert!(!path_inside_workspace(bad), "{bad} を通してしまった");
        }
        for ok in ["src/auth/**", "src/a.rs", "docs/x.md", "./src/b.rs"] {
            assert!(path_inside_workspace(ok), "{ok} を弾いてしまった");
        }
    }

    /// 文字列 1 本の危険度 (テストを読みやすくするための包み)。
    fn risk(line: &str) -> ValidationRisk {
        match ValidationCommand::parse(line) {
            Ok(c) => classify(&c),
            // 引用符が閉じていない等は、そもそも受け付けない。
            Err(_) => ValidationRisk::Forbidden,
        }
    }

    #[test]
    fn 危険なコマンドを拒否する() {
        for bad in [
            "rm -rf /",
            "git push origin main",
            "cargo test && rm -rf x",
            "sh -c 'echo hi'",
            "cargo publish",
            "kubectl apply -f x",
            "cargo test; echo done",
            "cargo test | head",
        ] {
            assert_eq!(risk(bad), ValidationRisk::Forbidden, "{bad} を通した");
        }
        for ok in ["cargo test auth", "npm test", "cargo fmt --check", "just ci"] {
            assert_ne!(risk(ok), ValidationRisk::Forbidden, "{ok} を弾いた");
        }
    }

    #[test]
    fn パス付きの実行ファイルは拒否する() {
        // **basename だけを見てはいけない。** `/tmp/cargo` は basename が
        // `cargo` なので許可され、実際に起動されるのは `/tmp/cargo` だった。
        for bad in [
            "/tmp/cargo test",
            "./cargo test",
            "tools/python script.py",
            "C:\\tools\\cargo.exe test",
            "..\\bin\\cargo test",
            ".\\cargo.exe test",
            "C:cargo test",
        ] {
            assert_eq!(risk(bad), ValidationRisk::Forbidden, "{bad} を通した");
        }
    }

    #[test]
    fn リポジトリのコードを実行しうるものは自動実行しない() {
        // ビルド・テスト・スクリプト実行系は、シェルのメタ文字が 1 つも
        // 無くても **リポジトリ内の任意コードを実行できる**
        // (build.rs / conftest.py / Makefile / package.json の scripts …)。
        for c in [
            "cargo test auth",
            "cargo build",
            "npm test",
            "pnpm test",
            "yarn test",
            "bun test",
            "node malicious.js",
            "deno test",
            "python arbitrary.py",
            "python3 arbitrary.py",
            "pytest",
            "make dangerous-target",
            "just ci",
            "gradle test",
            "mvn verify",
            "dotnet test",
            "go test ./...",
        ] {
            let r = risk(c);
            assert_eq!(r, ValidationRisk::RepositoryCodeExecution, "{c}");
            assert!(!r.auto_runnable(), "{c} を人の承認なしで実行する");
            assert!(r.needs_approval(), "{c} が承認を通らない");
        }
    }

    #[test]
    fn 書き換えるかどうかは旗で決まる() {
        // **名前だけでは決まらない。** `black --check .` は読むだけだが
        // `black .` はファイルをその場で書き換える。同じ実行体・同じ
        // 許可リスト・シェルのメタ文字ゼロで、意味だけが違う。
        let read_only = [
            "shellcheck file.sh",
            "black --check .",
            "black --diff .",
            "ruff check .",
            "ruff format --check .",
            "rustfmt --check src/main.rs",
        ];
        let mutating = [
            "black .",
            "ruff check --fix .",
            "ruff check --unsafe-fixes .",
            "ruff format .",
            "ruff .",
            "rustfmt src/main.rs",
        ];
        for c in read_only {
            let r = risk(c);
            assert_eq!(r, ValidationRisk::ReadOnly, "{c} を書き換える側にした");
            assert!(r.auto_runnable(), "{c} を自動実行しない");
        }
        for c in mutating {
            let r = risk(c);
            assert_eq!(r, ValidationRisk::WorkspaceMutation, "{c} を読むだけにした");
            assert!(!r.auto_runnable(), "{c} を人の承認なしで実行する");
            assert!(r.needs_approval(), "{c} が承認を通らない");
        }
    }

    #[test]
    fn 危険な旗を数え上げる形では守れない() {
        // **どれも「`--check` がある = 読むだけ」を通り抜けて書く。**
        // 旗を deny で数え上げていた版は、これを全部 `ReadOnly` にしていた。
        for c in [
            // `--print-config` は整形モードより手前で処理されるので
            // `--check` では止まらない。指定したパスへファイルを書く
            // (workspace の外でも書ける)。
            "rustfmt --check --print-config default out.toml",
            "rustfmt --check --emit files src/a.rs",
            // black は Click。`--extend-exclude` が次の語を値として食うので
            // `--check` は旗ではなく値になり、black は書き換えモードで動く。
            "black --extend-exclude --check .",
            "black --exclude --check .",
            "black --config --check .",
            // `--fix` とは綴りが違うが、どちらも書き換える。
            "ruff check --fix-only .",
            "ruff check --add-noqa .",
            // `-x` は `# shellcheck source=…` を辿って
            // ディスクのどこでも読むようになる。
            "shellcheck -x a.sh",
            "shellcheck --external-sources a.sh",
            // **知らない旗**。道具に旗が増えても黙って通さない。
            "black --check --zzz-new-flag .",
            "ruff check --zzz-new-flag .",
            "rustfmt --check --zzz-new-flag a.rs",
            "shellcheck --zzz-new-flag a.sh",
        ] {
            let r = risk(c);
            assert_ne!(r, ValidationRisk::ReadOnly, "{c} を読むだけにした");
            assert!(!r.auto_runnable(), "{c} を人の承認なしで実行する");
        }
    }

    #[test]
    fn サブコマンド探しも値を飛ばす() {
        // `--config` は次の語を食う。飛ばさないと `x.toml` を
        // サブコマンドと読み、`ruff check` の型を選べずに
        // `WorkspaceMutation` へ落ちる (行き止まりではないが、
        // 読むだけのコマンドが毎回承認を求めるようになる)。
        assert_eq!(
            risk("ruff --config x.toml check ."),
            ValidationRisk::ReadOnly
        );
        // `--` 以降は位置引数なので、`--check` は旗として数えない。
        assert_eq!(risk("black -- --check ."), ValidationRisk::WorkspaceMutation);
    }

    #[test]
    fn 自動実行してよいのは読むだけのものに限る() {
        // **不変条件**: 自動実行 = 読むだけ。ここが崩れると、AI が書いた
        // 計画が人の承認なしにファイルを書き換えられる。
        for c in [
            "cargo test",
            "black .",
            "rustfmt src/a.rs",
            "ruff check --fix .",
            "npm test",
            "make",
        ] {
            assert!(
                !risk(c).auto_runnable(),
                "{c} を人の承認なしで実行してしまう"
            );
        }
    }

    #[test]
    fn 禁止されたコマンドはforbiddenになる() {
        for bad in [
            "cargo publish",
            "git push origin main",
            "npm publish",
            "sudo make install",
            "rm -rf /",
            "kubectl apply -f x",
            "cargo test && rm -rf x",
            "sh -c 'echo hi'",
            "terraform apply",
        ] {
            assert_eq!(risk(bad), ValidationRisk::Forbidden, "{bad} を通した");
        }
    }

    #[test]
    fn windowsのシェル特殊文字も断る() {
        // `.cmd` / `.bat` の実行体は std が cmd.exe 越しに起こすので、
        // **cmd.exe の解釈が効く**。`%VAR%` の展開と `!` の遅延展開は
        // std が逃がせないので、判定した文字列と実行される文字列が
        // 別物になる余地を残さないよう先に断る。
        for bad in [
            "npm run %PATH%",
            "npm run %COMSPEC%",
            "npm run a!b!",
            "cargo test a&b",
            "cargo test a|b",
            "cargo test a>b",
            "cargo test a<b",
        ] {
            assert_eq!(risk(bad), ValidationRisk::Forbidden, "{bad} を通した");
        }
    }

    #[test]
    fn 当たり前の検証コマンドを行き止まりにしない() {
        // **入れすぎた禁止は「fail-closed だから安全」では済まない。**
        // `validate_plan` が `DangerousCommand` を出し、そのタスクは
        // `NeedsUser` で止まる。ごく普通に書かれた SPEC の 1 行が、
        // 誰も直せないまま Team Run を止める。
        //
        // `^` `(` `)` は std のバッチ用の逃がし方が面倒を見るので、
        // ここで断る必要が無い (`%` と `!` は std が逃がせないので残す)。
        for ok in [
            "pytest -k \"not (slow or db)\"",
            "jest --testPathPattern \"src/(auth)\"",
            "npm test -- --grep \"^auth\"",
            "cargo test -- --skip \"a::b (x)\"",
        ] {
            assert_ne!(
                risk(ok),
                ValidationRisk::Forbidden,
                "{ok} を行き止まりにした"
            );
        }
    }

    #[test]
    fn フェーズはタスクグラフから決まる() {
        let mut impl_t = task(1, "impl", &[]);
        impl_t.role = super::super::model::TeamRole::Implementer;
        let mut rev = task(2, "rev", &[1]);
        rev.role = super::super::model::TeamRole::Reviewer;
        let tasks = vec![impl_t, rev];
        let ps = phases(&tasks, false);
        assert_eq!(ps.len(), 6);
        // 実装が Pending なので Implementation は Waiting、Review も Waiting
        assert_eq!(ps[2], (Phase::Implementation, PhaseStatus::Waiting));
        assert_eq!(ps[5].1, PhaseStatus::Waiting);
        // 実装が走り出すと Implementation が Running
        let mut tasks2 = tasks.clone();
        tasks2[0].state = TeamTaskState::Running;
        assert_eq!(
            phases(&tasks2, false)[2],
            (Phase::Implementation, PhaseStatus::Running)
        );
        assert_eq!(current_phase(&tasks2, false), Phase::Implementation);
    }

    #[test]
    fn 全部完了して初めて最終検証が動く() {
        let mut a = task(1, "a", &[]);
        a.state = TeamTaskState::Completed;
        let ps = phases(&[a], false);
        assert_eq!(ps[5], (Phase::FinalValidation, PhaseStatus::Running));
        let mut b = task(1, "a", &[]);
        b.state = TeamTaskState::Completed;
        assert_eq!(phases(&[b], true)[5].1, PhaseStatus::Done);
    }

    #[test]
    fn goalはレビュー承認まで完了しない() {
        let mut a = task(1, "a", &[]);
        a.state = TeamTaskState::Completed;
        a.validation
            .runs
            .push(super::super::model::ValidationRun::passed(
                a.validation_commands[0].display(),
            ));
        assert!(
            !goal_done(&[a.clone()], &["done".into()], true),
            "未承認で完了させない"
        );
        a.review.verdict = Some(super::super::model::ReviewVerdict::Approve);
        assert!(goal_done(&[a], &["done".into()], true));
    }

    #[test]
    fn goalは検証未実行では完了しない() {
        let mut a = task(1, "a", &[]);
        a.state = TeamTaskState::Completed;
        a.review.verdict = Some(super::super::model::ReviewVerdict::Approve);
        assert!(!goal_done(&[a], &["done".into()], true));
    }

    #[test]
    fn クリティカルパスの深さ() {
        // 1 → 2 → 3, 1 → 4
        let t = vec![
            task(1, "a", &[]),
            task(2, "b", &[1]),
            task(3, "c", &[2]),
            task(4, "d", &[1]),
        ];
        let d = critical_depth(&t);
        assert_eq!(d[&1], 2);
        assert_eq!(d[&2], 1);
        assert_eq!(d[&3], 0);
        assert_eq!(d[&4], 0);
    }

    #[test]
    fn 進捗率() {
        assert_eq!(progress(&[]), 0.0);
        let mut a = task(1, "a", &[]);
        let b = task(2, "b", &[]);
        a.state = TeamTaskState::Completed;
        assert!((progress(&[a, b]) - 0.5).abs() < 1e-6);
    }

    /// **検証コマンドが 1 本も無い計画は通す。**
    ///
    /// 素の HTML やデザインだけのフォルダには、走らせられる検証が存在しない。
    /// そこで断ると Team がその手の仕事にまったく使えなくなる
    /// (実際に「綺麗な美容室の HTML を作って」で行き止まりになった)。
    /// 完了は**レビュー承認だけ**で決まる状態になり、盤面がそれを出す。
    #[test]
    fn 検証が1本も無い計画は通す() {
        let mut a = super::super::testkit::task(1, "a", &[]);
        let mut b = super::super::testkit::task(2, "b", &[]);
        a.validation_commands.clear();
        b.validation_commands.clear();
        let issues = validate_plan(&[a, b], &["動く".to_string()]);
        assert!(
            !issues
                .iter()
                .any(|i| matches!(i, PlanIssue::NoValidationCommand(_))),
            "道具が無いだけで断った: {issues:?}"
        );
    }

    /// **1 本でも検証を持つ計画で「このタスクだけ検証なし」は断る。**
    ///
    /// 抜け穴を 1 タスク作るだけで完了を素通りさせられるため。
    #[test]
    fn 検証を持つ計画で抜けているタスクは断る() {
        let a = super::super::testkit::task(1, "a", &[]);
        let mut b = super::super::testkit::task(2, "b", &[]);
        b.validation_commands.clear();
        let issues = validate_plan(&[a, b], &["動く".to_string()]);
        assert!(
            issues
                .iter()
                .any(|i| matches!(i, PlanIssue::NoValidationCommand(2))),
            "抜け穴を通してしまった: {issues:?}"
        );
    }
}
