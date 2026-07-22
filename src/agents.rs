use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui;

use crate::config::AgentPreset;
use crate::terminal::{Session, SpawnSpec};

pub enum SessionEvent {
    /// (title) — セッションがユーザーの承認待ちになった
    NeedsApproval(String),
    /// (title, 説明) — 全自動YESモードが承認プロンプトへ自動応答した
    AutoApproved(String, &'static str),
    /// (title, exit code) — セッションが終了した
    Exited(String, u32),
    /// (title, 警告行) — レート制限/使用上限の警告を新たに検知した
    RateLimited(String, String),
}

/// 既定の承認モード (config.approval_mode に対応)。
/// Agent = Agent欄(プリセット)優先: コマンドに書かれたフラグをそのまま使う。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Ask,
    Auto,
    Agent,
}

impl Approval {
    pub fn from_mode(mode: &str) -> Self {
        match mode {
            "auto" => Approval::Auto,
            "agent" => Approval::Agent,
            _ => Approval::Ask,
        }
    }
}

/// 承認モードを自動適用できる CLI 1 件分の定義。
///
/// `bin` は「ターミナルで実際に打つ実行ファイル名」。`codex exec` や `goose run` のような
/// サブコマンド形式でも、先頭トークン(= 実行ファイル名)だけで一致判定する。
///
/// NOTE: UI 表示用メタデータのうち、まだ app.rs 側の配線が入っていないものだけ
/// フィールド単位で dead_code を許可している(構造体まるごとの許可はしない —
/// 到達性はコンパイラに証明させたいため)。
pub struct AgentSpec {
    /// ターミナルで実際に打つ実行ファイル名(サブコマンドは含めない)。
    pub bin: &'static str,
    /// UI 表示名。
    #[allow(dead_code)]
    pub label: &'static str,
    /// UI 用アイコン。
    #[allow(dead_code)]
    pub icon: &'static str,
    /// 一括自動承認フラグ。持たない CLI は "" (その場合は `auto_env` を使う)。
    pub auto_flag: &'static str,
    /// フラグが無い CLI 用の、環境変数による自動承認ルート。
    pub auto_env: &'static [(&'static str, &'static str)],
    /// Ask モードで除去する単独フラグ群(auto_flag とその別名)。
    pub strip: &'static [&'static str],
    /// 非対話(ヘッドレス)実行の指定。サブコマンド型は `bin sub` の形。無ければ ""。
    #[allow(dead_code)]
    pub headless: &'static str,
    /// モデル指定フラグ。設定ファイル専用なら ""。
    #[allow(dead_code)]
    pub model_flag: &'static str,
    /// 未インストール時に案内するインストールコマンド。
    #[allow(dead_code)]
    pub install: &'static str,
    /// UI で出す日本語の注意書き。無ければ ""。
    #[allow(dead_code)]
    pub note: &'static str,
    /// 実行中セッションへ送る「権限モード切替」のキー列。
    ///
    /// **実機で確認できた CLI だけ**を埋めること。生きたセッションへ誤ったキーを
    /// 撃ち込むのは、機能が無いことより有害なので、未確認は "" のままにする
    /// (`switch_keys_bytes()` が None を返し、UI はボタンを出さない)。
    pub switch_keys: &'static str,
    /// 権限モード切替ボタンの説明。`switch_keys` と必ず対で埋める。未確認は ""。
    pub switch_hint: &'static str,
}

impl AgentSpec {
    /// 検証済みの権限モード切替キー列。未検証の CLI では None。
    pub fn switch_keys_bytes(&self) -> Option<&'static [u8]> {
        if self.switch_keys.is_empty() {
            return None;
        }
        Some(self.switch_keys.as_bytes())
    }

    /// 権限モード切替ボタンの説明。未検証の CLI では None。
    pub fn switch_hint_text(&self) -> Option<&'static str> {
        if self.switch_hint.is_empty() {
            return None;
        }
        Some(self.switch_hint)
    }
}

/// 承認モードを自動適用できる CLI カタログ。
///
/// `claude` の `--permission-mode bypassPermissions` と `devin` の
/// `--permission-mode bypass` は 2 トークン形式のため `apply_approval` 側で別処理する。
///
/// 【意図的に除外している CLI】
/// - Codebuff: ヘッドレス実行モードが無く、一括自動承認の仕組みも一切無い。
///   プリセットとして登録しても承認モード(Auto/Ask)を適用できず壊れた項目にしかならないため、
///   「親切心で」追加しないこと。
pub const AGENT_CATALOG: &[AgentSpec] = &[
    AgentSpec {
        bin: "claude",
        label: "Claude Code",
        icon: "👾",
        auto_flag: "--dangerously-skip-permissions",
        auto_env: &[],
        strip: &["--dangerously-skip-permissions"],
        headless: "-p",
        model_flag: "--model",
        install: "curl -fsSL https://claude.ai/install.sh | bash",
        note: "",
        switch_keys: "\x1b[Z",
        switch_hint: "権限モード切替 (Shift+Tab)",
    },
    AgentSpec {
        bin: "codex",
        label: "Codex",
        icon: "💡",
        auto_flag: "--dangerously-bypass-approvals-and-sandbox",
        auto_env: &[],
        strip: &[
            "--dangerously-bypass-approvals-and-sandbox",
            "--yolo",
            "--full-auto",
        ],
        headless: "codex exec",
        model_flag: "-m",
        install: "curl -fsSL https://chatgpt.com/codex/install.sh | sh",
        note: "`-p` は `--print` ではなく `--profile`。非対話実行は `codex exec` を使う",
        switch_keys: "/permissions\r",
        switch_hint: "権限モード切替 (/permissions)",
    },
    AgentSpec {
        bin: "grok",
        label: "Grok",
        icon: "📡",
        auto_flag: "--always-approve",
        auto_env: &[],
        strip: &["--always-approve", "--yolo"],
        headless: "-p",
        model_flag: "-m",
        install: "npm i -g @xai-official/grok",
        note: "同名バイナリの別製品が存在し、名前では判別できない",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "cursor-agent",
        label: "Cursor",
        icon: "🖱",
        auto_flag: "-f",
        auto_env: &[],
        strip: &["-f"],
        headless: "-p",
        model_flag: "--model",
        install: "curl https://cursor.com/install -fsS | bash",
        note: "全自動は `-f` のみ。`--yolo` は受け付けない",
        switch_keys: "\x1b[Z",
        switch_hint: "権限モード切替 (Shift+Tab)",
    },
    AgentSpec {
        bin: "copilot",
        label: "GitHub Copilot",
        icon: "🐙",
        auto_flag: "--allow-all-tools",
        auto_env: &[],
        strip: &["--allow-all-tools"],
        headless: "-p",
        model_flag: "--model",
        install: "npm i -g @github/copilot",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "opencode",
        label: "OpenCode",
        icon: "📦",
        auto_flag: "--auto",
        auto_env: &[],
        strip: &["--auto"],
        headless: "opencode run",
        model_flag: "-m",
        install: "curl -fsSL https://opencode.ai/install | bash",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "mimo",
        label: "MiMo Code",
        icon: "🍚",
        auto_flag: "--dangerously-skip-permissions",
        auto_env: &[],
        strip: &["--dangerously-skip-permissions"],
        headless: "mimo run",
        model_flag: "-m",
        install: "curl -fsSL https://mimo.xiaomi.com/install | bash",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "amp",
        label: "Amp",
        icon: "⚡",
        auto_flag: "--dangerously-allow-all",
        auto_env: &[],
        strip: &["--dangerously-allow-all"],
        headless: "-x",
        model_flag: "",
        install: "npm i -g @sourcegraph/amp",
        note: "モデル指定フラグは無い(設定側で指定)",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "openclaude",
        label: "OpenClaude",
        icon: "🌀",
        auto_flag: "--dangerously-skip-permissions",
        auto_env: &[],
        strip: &["--dangerously-skip-permissions"],
        headless: "-p",
        model_flag: "--model",
        install: "npm i -g @gitlawb/openclaude@latest",
        note: "スコープ無しの npm パッケージ `openclaude` は別物",
        switch_keys: "",
        switch_hint: "",
    },
    // Antigravity CLI (Google)。全自動フラグは claude と同名。
    AgentSpec {
        bin: "agy",
        label: "Antigravity",
        icon: "🚀",
        auto_flag: "--dangerously-skip-permissions",
        auto_env: &[],
        strip: &["--dangerously-skip-permissions", "--yolo"],
        headless: "-p",
        model_flag: "--model",
        install: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
        note: "",
        switch_keys: "\x1b[Z",
        switch_hint: "権限モード切替 (Shift+Tab)",
    },
    AgentSpec {
        bin: "pi",
        label: "Pi",
        icon: "🔷",
        auto_flag: "-a",
        auto_env: &[],
        strip: &["-a"],
        headless: "-p",
        model_flag: "--model",
        install: "npm i -g --ignore-scripts @earendil-works/pi-coding-agent",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "omp",
        label: "oh-my-pi",
        icon: "🔶",
        auto_flag: "--auto-approve",
        auto_env: &[],
        strip: &["--auto-approve", "--yolo"],
        headless: "-p",
        model_flag: "--model",
        install: "npm i -g @oh-my-pi/pi-coding-agent",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "hermes",
        label: "Hermes",
        icon: "🕊",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo"],
        headless: "-z",
        model_flag: "-m",
        install: "curl -fsSL https://hermes-agent.nousresearch.com/install.sh | bash",
        note: "非対話実行は `-p` ではなく `-z`",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "devin",
        label: "Devin",
        icon: "👷",
        auto_flag: "--permission-mode bypass",
        auto_env: &[],
        strip: &[],
        headless: "-p",
        model_flag: "--model",
        install: "brew install --cask devin-cli",
        note: "全自動は 2 トークン形式の `--permission-mode bypass`",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "goose",
        label: "Goose",
        icon: "🐦",
        auto_flag: "",
        auto_env: &[("GOOSE_MODE", "auto")],
        strip: &[],
        headless: "goose run -t",
        model_flag: "--model",
        install: "brew install block-goose-cli",
        note: "一括自動承認フラグが無く、環境変数 `GOOSE_MODE=auto` や設定ファイル側で指定する",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "auggie",
        label: "Auggie",
        icon: "🅰",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "--print",
        model_flag: "--model",
        install: "npm i -g @augmentcode/auggie@latest",
        note: "一括自動承認フラグが無く、ツール単位の許可を設定ファイル側で指定する",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "autohand",
        label: "Autohand",
        icon: "✋",
        auto_flag: "--unrestricted",
        auto_env: &[],
        strip: &["--unrestricted"],
        headless: "-p",
        model_flag: "--model",
        install: "curl -fsSL https://autohand.ai/install.sh | sh",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "crush",
        label: "Crush",
        icon: "🌊",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "crush run",
        model_flag: "-m",
        install: "brew install charmbracelet/tap/crush",
        note: "`crush run` は既定で自動承認し `--yolo` を受け付けない",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "cline",
        label: "Cline",
        icon: "🔗",
        auto_flag: "--auto-approve",
        auto_env: &[],
        strip: &["--auto-approve", "--yolo"],
        headless: "--print",
        model_flag: "-m",
        install: "npm i -g cline",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "cmd",
        label: "Command Code",
        icon: "⌘",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo", "--auto-accept", "--dangerously-skip-permissions"],
        headless: "-p",
        model_flag: "-m",
        install: "npm i -g command-code",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "cn",
        label: "Continue",
        icon: "➡",
        auto_flag: "--auto",
        auto_env: &[],
        strip: &["--auto"],
        headless: "-p",
        model_flag: "--model",
        install: "npm i -g @continuedev/cli",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "droid",
        label: "Droid",
        icon: "👾",
        auto_flag: "--skip-permissions-unsafe",
        auto_env: &[],
        strip: &["--skip-permissions-unsafe"],
        headless: "droid exec",
        model_flag: "-m",
        install: "curl -fsSL https://app.factory.ai/cli | sh",
        note: "`--auto` は値が必須(`low|medium|high`)。全自動は `--skip-permissions-unsafe`",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "kilo",
        label: "Kilo Code",
        icon: "🔩",
        auto_flag: "--auto",
        auto_env: &[],
        strip: &["--auto"],
        headless: "kilo run",
        model_flag: "--model",
        install: "npm i -g @kilocode/cli",
        note: "",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "kimi",
        label: "Kimi",
        icon: "🌙",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo"],
        headless: "-p",
        model_flag: "-m",
        install: "npm i -g @moonshot-ai/kimi-code",
        note: "スコープ無しの npm パッケージ `kimi` は別物",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "kiro-cli",
        label: "Kiro",
        icon: "🎏",
        auto_flag: "--trust-all-tools",
        auto_env: &[],
        strip: &["--trust-all-tools"],
        headless: "--no-interactive",
        model_flag: "",
        install: "curl -fsSL https://cli.kiro.dev/install | bash",
        note: "`kiro` は IDE 本体、エージェントは `kiro-cli`",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "vibe",
        label: "Mistral Vibe",
        icon: "🎐",
        auto_flag: "--auto-approve",
        auto_env: &[],
        strip: &["--auto-approve", "--yolo"],
        headless: "-p",
        model_flag: "",
        install: "uv tool install mistral-vibe",
        note: "モデルは設定ファイル専用でフラグ指定できない",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "qwen",
        label: "Qwen Code",
        icon: "🐉",
        auto_flag: "--approval-mode=yolo",
        auto_env: &[],
        strip: &["--approval-mode=yolo", "--yolo"],
        headless: "-p",
        model_flag: "-m",
        install: "npm i -g @qwen-code/qwen-code@latest",
        note: "旧版は `--yolo`、現行は `--approval-mode=yolo`",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "acli",
        label: "Rovo Dev",
        icon: "💠",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo"],
        headless: "acli rovodev run",
        model_flag: "",
        install: "Atlassian `acli` を入れて `acli rovodev auth login`",
        note: "エージェントは `acli rovodev run` サブコマンド",
        switch_keys: "",
        switch_hint: "",
    },
    AgentSpec {
        bin: "aider",
        label: "Aider",
        icon: "🛠",
        auto_flag: "--yes-always",
        auto_env: &[("AIDER_YES_ALWAYS", "1")],
        strip: &["--yes-always"],
        headless: "-m",
        model_flag: "--model",
        install: "python -m pip install aider-install && aider-install",
        note: "`-m` は model ではなく message",
        switch_keys: "",
        switch_hint: "",
    },
];

/// パス付きでも実行ファイル名だけを取り出す(`/usr/local/bin/claude` → `claude`)。
fn basename(token: &str) -> &str {
    token.rsplit(['/', '\\']).next().unwrap_or(token)
}

/// コマンド文字列(先頭トークン)からカタログ定義を引く。
/// `codex exec` / `goose run` のようなサブコマンド形式でも先頭トークンだけで一致する。
pub fn spec_for_command(command: &str) -> Option<&'static AgentSpec> {
    let head = basename(command.split_whitespace().next()?);
    spec_for_bin(head)
}

/// 実行ファイル名(パス無し)からカタログ定義を引く。
pub fn spec_for_bin(bin: &str) -> Option<&'static AgentSpec> {
    AGENT_CATALOG.iter().find(|s| s.bin == bin)
}

/// コマンドの先頭トークンが承認モード対応 CLI なら (auto フラグ, 除去対象) を返す。
fn known_agent(command: &str) -> Option<(&'static str, &'static [&'static str])> {
    spec_for_command(command).map(|s| (s.auto_flag, s.strip))
}

/// `--permission-mode <値>` の値が bypass 系か。
fn is_bypass_permission_mode(value: &str) -> bool {
    value.eq_ignore_ascii_case("bypassPermissions") || value.eq_ignore_ascii_case("bypass")
}

/// カタログ由来の bypass フラグ集合にトークンが含まれるか。
/// `-f` / `-a` のような 2 文字フラグは誤検知を避けるため、そのフラグを持つ CLI
/// のコマンドである場合だけ bypass 扱いにする。
fn token_is_bypass_flag(token: &str, head_spec: Option<&'static AgentSpec>) -> bool {
    let short = token.len() <= 2;
    AGENT_CATALOG.iter().any(|s| {
        if short && head_spec.map(|h| h.bin) != Some(s.bin) {
            return false;
        }
        s.auto_flag
            .split_whitespace()
            .any(|f| f.eq_ignore_ascii_case(token))
            || s.strip.iter().any(|f| f.eq_ignore_ascii_case(token))
    })
}

/// コマンド文字列が bypass 権限フラグを含むか(表示用の判定)。
/// 判定はカタログの `auto_flag` / `strip` の和集合から導出し、加えて
/// `--permission-mode bypassPermissions` / `--permission-mode bypass` を特別扱いする。
pub fn command_is_bypass(command: &str) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let head_spec = spec_for_command(command);
    for (i, tok) in tokens.iter().enumerate() {
        // `--permission-mode bypassPermissions` / `--permission-mode bypass`
        if *tok == "--permission-mode" {
            if tokens.get(i + 1).map(|v| is_bypass_permission_mode(v)) == Some(true) {
                return true;
            }
            continue;
        }
        // `--permission-mode=bypassPermissions`
        if let Some(v) = tok.strip_prefix("--permission-mode=") {
            if is_bypass_permission_mode(v) {
                return true;
            }
            continue;
        }
        // `--permission-mode` を伴わない bypassPermissions 表記の保険
        if tok.to_lowercase().contains("bypasspermissions") {
            return true;
        }
        if i > 0 && token_is_bypass_flag(tok, head_spec) {
            return true;
        }
    }
    false
}

/// カタログ対応 CLI のコマンドに承認モードを適用する。
/// Auto = 全自動YES (CLI ごとの bypass フラグを付与)、
/// Ask = 毎回ユーザー承認 (bypass 系フラグを全て除去し CLI 標準の確認に任せる)、
/// Agent = Agent欄優先 (プリセットのコマンドを一切書き換えない)。
///
/// Ask のときは、プリセットのコマンドに bypass フラグが直書きされていても
/// 確実に取り除く(これがユーザー報告「全自動じゃなくても bypass になる」対策)。
pub fn apply_approval(command: &str, approval: Approval) -> String {
    if approval == Approval::Agent {
        return command.to_string();
    }
    let Some((auto_flag, strip_flags)) = known_agent(command) else {
        return command.to_string();
    };
    // 一括自動承認フラグを持たない CLI (goose / auggie / crush) は Auto でも書き換えない。
    // 自動承認は spec.auto_env の環境変数か、CLI 側の設定ファイルで行う。
    if approval == Approval::Auto && auto_flag.is_empty() {
        return command.to_string();
    }
    // bypass 系フラグを一旦すべて除去する。claude は
    // `--permission-mode bypassPermissions`(スペース区切り)も、
    // `--permission-mode=bypassPermissions`(= 区切り)も両方消す。
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut parts: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if strip_flags.contains(&tok) {
            i += 1;
            continue;
        }
        // `--permission-mode bypassPermissions` / `--permission-mode bypass`
        // (スペース区切り2トークン)を除去。`--permission-mode plan` などは残す。
        if tok == "--permission-mode"
            && tokens
                .get(i + 1)
                .map(|v| is_bypass_permission_mode(v))
                .unwrap_or(false)
        {
            i += 2;
            continue;
        }
        // `--permission-mode=bypassPermissions`(= 区切り1トークン)を除去
        if let Some(v) = tok.strip_prefix("--permission-mode=") {
            if is_bypass_permission_mode(v) {
                i += 1;
                continue;
            }
        }
        parts.push(tok);
        i += 1;
    }
    if approval == Approval::Auto && !auto_flag.is_empty() {
        parts.push(auto_flag);
    }
    parts.join(" ")
}

/// 起動時にプロセスへ渡す環境変数を組み立てる。
///
/// goose / aider のように「一括自動承認フラグを持たない」CLI は、環境変数でしか
/// 全自動にできない。そこで **Auto モードのときだけ** `spec.auto_env` を混ぜる。
/// Ask / Agent では一切足さない(Ask で勝手に自動承認になるのが最悪の事故なので)。
///
/// 競合したらプリセット側の値が勝つ。ユーザーが明示的に `GOOSE_MODE=approve` などを
/// 書いていたら、それはユーザーの意思なので上書きしない。
pub fn merged_env(
    command: &str,
    approval: Approval,
    preset_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut out: HashMap<String, String> = HashMap::new();
    if approval == Approval::Auto {
        if let Some(spec) = spec_for_command(command) {
            for (k, v) in spec.auto_env {
                out.insert((*k).to_string(), (*v).to_string());
            }
        }
    }
    // プリセット優先: 後から入れて上書きする。
    // 値の先頭 `~/` はホームへ展開する (env は $SHELL を経由せず
    // CommandBuilder へ直接渡るため、シェルの ~ 展開が効かない。
    // CLAUDE_CONFIG_DIR = "~/.claude-work" のようなパス指定を動かすため)。
    for (k, v) in preset_env {
        let v = if v.starts_with("~/") {
            expand_home(v).to_string_lossy().into_owned()
        } else {
            v.clone()
        };
        out.insert(k.clone(), v);
    }
    out
}

/// 環境変数だけで全自動になっている CLI か(auto_flag を持たない goose / aider 用)。
///
/// `command_is_bypass` はコマンド文字列しか見ないので、フラグを持たない CLI では
/// 常に false になる。その結果 Auto で起動しても全自動YESが働かなかった。
/// `spec.auto_env` の値が**すべて一致**して環境に入っているときだけ true を返す。
/// (ユーザーが別の値へ上書きしていたら全自動扱いにしない)
pub fn env_enables_auto(command: &str, env: &HashMap<String, String>) -> bool {
    let Some(spec) = spec_for_command(command) else {
        return false;
    };
    if spec.auto_env.is_empty() {
        return false;
    }
    spec.auto_env
        .iter()
        .all(|(k, v)| env.get(*k).map(|got| got == v).unwrap_or(false))
}

pub struct AgentManager {
    pub sessions: Vec<Session>,
    pub active: usize,
    pub panel_open: bool,
    next_id: u64,
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active: 0,
            panel_open: false,
            next_id: 1,
        }
    }

    pub fn launch(
        &mut self,
        preset: &AgentPreset,
        workspace: &Path,
        approval: Approval,
        ctx: &egui::Context,
    ) -> Result<(), String> {
        let same = self
            .sessions
            .iter()
            .filter(|s| s.preset_name == preset.name)
            .count();
        let title = if same > 0 {
            format!("{} #{}", preset.name, same + 1)
        } else {
            preset.name.clone()
        };
        let cwd = preset
            .cwd
            .as_deref()
            .map(expand_home)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| workspace.to_path_buf());

        let id = self.next_id;
        self.next_id += 1;
        let log_path = Some(crate::session::term_log_path(workspace, id, &title));
        let session = Session::spawn(
            id,
            SpawnSpec {
                title,
                preset_name: preset.name.clone(),
                icon: preset.icon.clone(),
                command: apply_approval(&preset.command, approval),
                cwd,
                env: merged_env(&preset.command, approval, &preset.env),
                log_path,
            },
            ctx.clone(),
        )?;
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
        self.panel_open = true;
        Ok(())
    }

    pub fn restart(&mut self, i: usize, ctx: &egui::Context) -> Result<(), String> {
        let Some(old) = self.sessions.get_mut(i) else {
            return Ok(());
        };
        old.kill();
        let id = self.next_id;
        self.next_id += 1;
        let session = Session::spawn(
            id,
            SpawnSpec {
                title: old.title.clone(),
                preset_name: old.preset_name.clone(),
                icon: old.icon.clone(),
                command: old.command.clone(),
                cwd: old.cwd.clone(),
                env: old.env.clone(),
                // 同じログへ追記する (ヘッダ行で起動の区切りが分かる)
                log_path: old.log_path.clone(),
            },
            ctx.clone(),
        )?;
        self.sessions[i] = session;
        self.active = i;
        Ok(())
    }

    pub fn remove(&mut self, i: usize) {
        if i >= self.sessions.len() {
            return;
        }
        self.sessions.remove(i);
        if self.active >= self.sessions.len() && !self.sessions.is_empty() {
            self.active = self.sessions.len() - 1;
        }
    }

    pub fn active_session(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active)
    }

    pub fn running_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.running()).count()
    }

    /// 各セッションの状態変化(承認待ち・自動承認・終了)を検知して返す。毎フレーム呼んで良い。
    /// 全自動YESで起動した対応 CLI は承認プロンプトへ自動応答する。
    pub fn poll_events(&mut self) -> Vec<SessionEvent> {
        use crate::terminal::Attention;
        let mut events = Vec::new();
        for s in self.sessions.iter_mut() {
            if s.running() {
                match s.scan_attention(s.auto_yes()) {
                    Some(Attention::NeedsApproval) => {
                        events.push(SessionEvent::NeedsApproval(s.title.clone()));
                    }
                    Some(Attention::AutoReplied(desc)) => {
                        events.push(SessionEvent::AutoApproved(s.title.clone(), desc));
                    }
                    Some(Attention::RateLimited(line)) => {
                        events.push(SessionEvent::RateLimited(s.title.clone(), line));
                    }
                    None => {}
                }
            } else if !s.notified_exit {
                s.notified_exit = true;
                s.attention = false;
                let code = s.exit_code.lock().unwrap().unwrap_or(0);
                events.push(SessionEvent::Exited(s.title.clone(), code));
            }
        }
        events
    }

    /// Send text to every running session (cockpit broadcast).
    pub fn broadcast(&mut self, text: &str) {
        let payload = format!("{text}\r");
        for s in &mut self.sessions {
            if s.running() {
                s.write_bytes(payload.as_bytes());
            }
        }
    }

    /// 指定セッションの権限モード切替 UI を開く/切り替える。
    /// Claude/Antigravity は Shift+Tab、Codex は `/permissions` を送る。
    pub fn cycle_permission(&mut self, i: usize) -> Option<&'static str> {
        let s = self.sessions.get_mut(i)?;
        if !s.running() {
            return None;
        }
        let keys = s.permission_switch_keys()?;
        let hint = s.permission_switch_hint()?;
        s.write_bytes(keys);
        Some(hint)
    }

    /// 実行中の対応 CLI セッションへ、それぞれの権限モード切替入力を送る。送った件数を返す。
    pub fn cycle_permission_all(&mut self) -> usize {
        let mut n = 0;
        for s in &mut self.sessions {
            if !s.running() {
                continue;
            }
            if let Some(keys) = s.permission_switch_keys() {
                s.write_bytes(keys);
                n += 1;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_approval, env_enables_auto, merged_env, Approval};
    use std::collections::HashMap;

    #[test]
    fn claude_auto_appends_bypass() {
        assert_eq!(
            apply_approval("claude", Approval::Auto),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn claude_ask_strips_dangerous_flag() {
        assert_eq!(
            apply_approval("claude --dangerously-skip-permissions", Approval::Ask),
            "claude"
        );
    }

    #[test]
    fn claude_ask_strips_permission_mode_space() {
        assert_eq!(
            apply_approval("claude --permission-mode bypassPermissions", Approval::Ask),
            "claude"
        );
    }

    #[test]
    fn claude_ask_strips_permission_mode_equals() {
        assert_eq!(
            apply_approval(
                "claude --permission-mode=bypassPermissions --model x",
                Approval::Ask
            ),
            "claude --model x"
        );
    }

    #[test]
    fn non_known_command_untouched() {
        assert_eq!(
            apply_approval("gemini --dangerously-skip-permissions", Approval::Auto),
            "gemini --dangerously-skip-permissions"
        );
    }

    #[test]
    fn ask_does_not_double_add() {
        // ask では付与しない
        assert_eq!(apply_approval("claude", Approval::Ask), "claude");
        // auto でも二重に付与しない
        assert_eq!(
            apply_approval("claude --dangerously-skip-permissions", Approval::Auto),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn ask_keeps_non_bypass_permission_mode() {
        // plan など bypass 以外の権限モードは残す
        assert_eq!(
            apply_approval("claude --permission-mode plan", Approval::Ask),
            "claude --permission-mode plan"
        );
    }

    #[test]
    fn agent_mode_keeps_preset_command_verbatim() {
        // Agent欄優先: 既定が何であれプリセットのコマンドを書き換えない
        assert_eq!(
            apply_approval("claude --dangerously-skip-permissions", Approval::Agent),
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(apply_approval("claude", Approval::Agent), "claude");
        assert_eq!(
            apply_approval("codex --dangerously-bypass-approvals-and-sandbox", Approval::Agent),
            "codex --dangerously-bypass-approvals-and-sandbox"
        );
    }

    #[test]
    fn codex_auto_appends_bypass() {
        assert_eq!(
            apply_approval("codex", Approval::Auto),
            "codex --dangerously-bypass-approvals-and-sandbox"
        );
    }

    #[test]
    fn codex_ask_strips_auto_flags() {
        assert_eq!(
            apply_approval("codex --dangerously-bypass-approvals-and-sandbox", Approval::Ask),
            "codex"
        );
        assert_eq!(apply_approval("codex --yolo", Approval::Ask), "codex");
        assert_eq!(apply_approval("codex --full-auto", Approval::Ask), "codex");
    }

    #[test]
    fn agy_auto_and_ask() {
        assert_eq!(
            apply_approval("agy", Approval::Auto),
            "agy --dangerously-skip-permissions"
        );
        assert_eq!(
            apply_approval("agy --dangerously-skip-permissions", Approval::Ask),
            "agy"
        );
    }

    // ---- カタログ (AgentSpec) ----
    use super::{command_is_bypass, spec_for_bin, spec_for_command, AGENT_CATALOG};

    #[test]
    fn catalog_lookup_by_bare_name() {
        assert_eq!(spec_for_command("claude").unwrap().label, "Claude Code");
        assert_eq!(spec_for_bin("kiro-cli").unwrap().label, "Kiro");
    }

    #[test]
    fn catalog_lookup_by_absolute_path() {
        assert_eq!(
            spec_for_command("/usr/local/bin/claude --model x")
                .unwrap()
                .bin,
            "claude"
        );
        assert_eq!(
            spec_for_command("/opt/homebrew/bin/goose run -t hi").unwrap().bin,
            "goose"
        );
    }

    #[test]
    fn subcommand_forms_resolve_to_right_spec() {
        for (cmd, bin) in [
            ("codex exec 'do it'", "codex"),
            ("goose run -t hi", "goose"),
            ("crush run", "crush"),
            ("kilo run", "kilo"),
            ("opencode run", "opencode"),
            ("droid exec", "droid"),
            ("acli rovodev run", "acli"),
        ] {
            assert_eq!(spec_for_command(cmd).unwrap().bin, bin, "cmd={cmd}");
        }
    }

    #[test]
    fn auto_appends_flag_for_new_agents() {
        assert_eq!(
            apply_approval("cursor-agent", Approval::Auto),
            "cursor-agent -f"
        );
        assert_eq!(
            apply_approval("copilot", Approval::Auto),
            "copilot --allow-all-tools"
        );
        assert_eq!(
            apply_approval("qwen", Approval::Auto),
            "qwen --approval-mode=yolo"
        );
        assert_eq!(
            apply_approval("devin", Approval::Auto),
            "devin --permission-mode bypass"
        );
        assert_eq!(
            apply_approval("aider --model gpt", Approval::Auto),
            "aider --model gpt --yes-always"
        );
        assert_eq!(
            apply_approval("droid exec", Approval::Auto),
            "droid exec --skip-permissions-unsafe"
        );
    }

    #[test]
    fn ask_strips_aliases() {
        assert_eq!(apply_approval("cline --yolo", Approval::Ask), "cline");
        assert_eq!(apply_approval("vibe --yolo", Approval::Ask), "vibe");
        assert_eq!(apply_approval("qwen --yolo", Approval::Ask), "qwen");
        assert_eq!(apply_approval("omp --auto-approve", Approval::Ask), "omp");
        assert_eq!(
            apply_approval("cmd --dangerously-skip-permissions", Approval::Ask),
            "cmd"
        );
        assert_eq!(
            apply_approval("devin --permission-mode bypass -p hi", Approval::Ask),
            "devin -p hi"
        );
    }

    #[test]
    fn agents_without_auto_flag_untouched_in_auto() {
        for cmd in ["goose run -t hi", "auggie --print", "crush run"] {
            assert_eq!(apply_approval(cmd, Approval::Auto), cmd, "cmd={cmd}");
        }
        // 代わりに auto_env 側で自動承認する
        assert_eq!(
            spec_for_bin("goose").unwrap().auto_env,
            &[("GOOSE_MODE", "auto")]
        );
        assert!(spec_for_bin("crush").unwrap().auto_flag.is_empty());
    }

    #[test]
    fn command_is_bypass_covers_every_catalog_flag() {
        for spec in AGENT_CATALOG {
            if !spec.auto_flag.is_empty() {
                let cmd = format!("{} {}", spec.bin, spec.auto_flag);
                assert!(command_is_bypass(&cmd), "auto_flag not detected: {cmd}");
            }
            for alias in spec.strip {
                let cmd = format!("{} {alias}", spec.bin);
                assert!(command_is_bypass(&cmd), "alias not detected: {cmd}");
            }
        }
        // 特別扱いの 2 トークン / = 区切り表記
        assert!(command_is_bypass(
            "claude --permission-mode bypassPermissions"
        ));
        assert!(command_is_bypass("claude --permission-mode=bypassPermissions"));
        // bypass ではないものは false
        assert!(!command_is_bypass("claude --permission-mode plan"));
        assert!(!command_is_bypass("claude"));
        // 短いフラグは持ち主の CLI のときだけ bypass 扱い(誤検知防止)
        assert!(command_is_bypass("cursor-agent -f"));
        assert!(!command_is_bypass("grep -f patterns.txt"));
        assert!(!command_is_bypass("codex --full-auto-ish"));
    }

    #[test]
    fn codebuff_is_absent_from_catalog() {
        // ヘッドレス実行も一括自動承認も無いため意図的に除外している
        assert!(AGENT_CATALOG.iter().all(|s| s.bin != "codebuff"));
    }

    #[test]
    fn catalog_bins_are_unique_and_populated() {
        assert!(AGENT_CATALOG.len() >= 28);
        for (i, s) in AGENT_CATALOG.iter().enumerate() {
            assert!(!s.bin.is_empty() && !s.label.is_empty() && !s.icon.is_empty());
            assert!(!s.install.is_empty(), "no install hint for {}", s.bin);
            assert!(
                !s.bin.contains(char::is_whitespace),
                "bin must be a single token: {}",
                s.bin
            );
            assert!(
                AGENT_CATALOG[..i].iter().all(|o| o.bin != s.bin),
                "duplicate bin: {}",
                s.bin
            );
        }
    }

    // ── 権限モード切替キー ────────────────────────────────────────────

    /// 切替キーと説明は必ず対で埋める。片方だけだと UI がボタンを出しておいて
    /// 何も送らない(またはその逆)という中途半端な状態になる。
    #[test]
    fn switch_keys_and_hint_are_populated_together() {
        for s in AGENT_CATALOG {
            assert_eq!(
                s.switch_keys.is_empty(),
                s.switch_hint.is_empty(),
                "switch_keys と switch_hint が片方だけ埋まっている: {}",
                s.bin
            );
            assert_eq!(
                s.switch_keys_bytes().is_some(),
                s.switch_hint_text().is_some(),
                "{}",
                s.bin
            );
        }
    }

    /// 実機で確認できた CLI **だけ**が切替キーを持つ。
    /// ここに勝手に足さないこと — 未確認のキーは生きたセッションへの誤爆になる。
    #[test]
    fn only_verified_agents_have_switch_keys() {
        let with_keys: Vec<&str> = AGENT_CATALOG
            .iter()
            .filter(|s| s.switch_keys_bytes().is_some())
            .map(|s| s.bin)
            .collect();
        assert_eq!(with_keys, vec!["claude", "codex", "cursor-agent", "agy"]);
    }

    #[test]
    fn verified_switch_keys_have_expected_bytes() {
        // Shift+Tab = CSI Z (逆タブ)
        for bin in ["claude", "cursor-agent", "agy"] {
            assert_eq!(
                spec_for_bin(bin).unwrap().switch_keys_bytes(),
                Some(&b"\x1b[Z"[..]),
                "{}",
                bin
            );
        }
        assert_eq!(
            spec_for_bin("codex").unwrap().switch_keys_bytes(),
            Some(&b"/permissions\r"[..])
        );
    }

    /// 未確認の CLI は None を返す(既定値を当て推量で入れない)。
    #[test]
    fn unverified_agents_return_none_for_switch_keys() {
        for bin in ["opencode", "goose", "aider", "copilot", "amp"] {
            let s = spec_for_bin(bin).unwrap();
            assert_eq!(s.switch_keys_bytes(), None, "{}", bin);
            assert_eq!(s.switch_hint_text(), None, "{}", bin);
        }
    }

    // ── auto_env のマージ ─────────────────────────────────────────────

    #[test]
    fn auto_env_merged_only_in_auto_mode() {
        let empty = HashMap::new();
        // Auto: auto_env が入る
        let e = merged_env("goose", Approval::Auto, &empty);
        assert_eq!(e.get("GOOSE_MODE").map(String::as_str), Some("auto"));
        let e = merged_env("aider", Approval::Auto, &empty);
        assert_eq!(e.get("AIDER_YES_ALWAYS").map(String::as_str), Some("1"));
        // Ask / Agent: 絶対に入れない(Ask で自動承認になるのが最悪の事故)
        for mode in [Approval::Ask, Approval::Agent] {
            assert!(merged_env("goose", mode, &empty).is_empty());
            assert!(merged_env("aider", mode, &empty).is_empty());
        }
    }

    #[test]
    fn preset_env_tilde_expands_to_home() {
        // CLAUDE_CONFIG_DIR = "~/.claude-work" のようなプロファイル切替を
        // 動かすため、値先頭の ~/ はホームへ展開される (env はシェルを経由しない)
        let mut preset = HashMap::new();
        preset.insert("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-work".to_string());
        preset.insert("PLAIN".to_string(), "no~tilde/inside".to_string());
        let e = merged_env("claude", Approval::Ask, &preset);
        let home = dirs::home_dir().unwrap();
        assert_eq!(
            e.get("CLAUDE_CONFIG_DIR").map(String::as_str),
            Some(home.join(".claude-work").to_str().unwrap())
        );
        // 先頭以外の ~ はそのまま
        assert_eq!(e.get("PLAIN").map(String::as_str), Some("no~tilde/inside"));
    }

    #[test]
    fn preset_env_wins_over_auto_env() {
        let mut preset = HashMap::new();
        preset.insert("GOOSE_MODE".to_string(), "approve".to_string());
        preset.insert("MY_VAR".to_string(), "x".to_string());
        let e = merged_env("goose run", Approval::Auto, &preset);
        // ユーザーが明示した値は上書きしない
        assert_eq!(e.get("GOOSE_MODE").map(String::as_str), Some("approve"));
        assert_eq!(e.get("MY_VAR").map(String::as_str), Some("x"));
    }

    #[test]
    fn merged_env_untouched_for_agents_without_auto_env() {
        let empty = HashMap::new();
        // auto_flag 型の CLI には環境変数を足さない
        assert!(merged_env("claude", Approval::Auto, &empty).is_empty());
        // カタログ外のコマンドも同様
        assert!(merged_env("mycmd --x", Approval::Auto, &empty).is_empty());
    }

    #[test]
    fn env_enables_auto_requires_exact_values() {
        let mut env = HashMap::new();
        assert!(!env_enables_auto("goose", &env));
        env.insert("GOOSE_MODE".to_string(), "auto".to_string());
        assert!(env_enables_auto("goose", &env));
        assert!(env_enables_auto("/opt/bin/goose run", &env));
        // 値が違えば全自動扱いにしない
        env.insert("GOOSE_MODE".to_string(), "approve".to_string());
        assert!(!env_enables_auto("goose", &env));
        // auto_env を持たない CLI は常に false
        let mut c = HashMap::new();
        c.insert("GOOSE_MODE".to_string(), "auto".to_string());
        assert!(!env_enables_auto("claude", &c));
        assert!(!env_enables_auto("mycmd", &c));
    }

    /// launch と同じ経路: Auto なら環境変数が入り、それを Session 側が
    /// 「全自動起動」と認識できること (goose / aider の Auto を実際に機能させる鍵)。
    #[test]
    fn auto_mode_env_round_trips_into_env_enables_auto() {
        let empty = HashMap::new();
        for bin in ["goose", "aider"] {
            let auto = merged_env(bin, Approval::Auto, &empty);
            assert!(env_enables_auto(bin, &auto), "{} は Auto で全自動になるべき", bin);
            let ask = merged_env(bin, Approval::Ask, &empty);
            assert!(!env_enables_auto(bin, &ask), "{} は Ask で全自動になってはいけない", bin);
        }
    }
}
