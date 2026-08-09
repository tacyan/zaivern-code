use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui;

use crate::config::AgentPreset;
use crate::terminal::{Session, SpawnSpec};

// 統合承認キュー (全エージェント横断の承認待ち + ポリシー + 監査ログ)。
//
// `#[path]` でこのモジュールの子として取り込む。coordinator.rs の `quota`
// と同じ流儀で、**main.rs へ `mod approvals;` を足す必要は無い**
// (足すなら下の 1 行を消し、`approvals::` を `crate::approvals::` へ
// 書き換えること。二重登録は型が別物になる)。
// 外からは `crate::agents::approvals::…` で参照する。

/// 承認要求の分類・ポリシー・監査ログ (詳細はモジュール doc)。
#[path = "approvals.rs"]
pub mod approvals;

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
    /// 前回の会話を再開して起動するための指定。フラグ型 (`claude --continue`) と
    /// サブコマンド型 (`codex resume --last` — bin の直後に挟む) の両方がある。
    /// 再開機能を実機確認できていない CLI は "" (復元時は素の再起動になる)。
    pub resume_flag: &'static str,
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

    /// この CLI の「過去セッション保存先」。列挙できない CLI は `SessionStore::None`。
    pub fn session_store(&self) -> SessionStore {
        session_entry(self.bin)
            .map(|e| e.1)
            .unwrap_or(SessionStore::None)
    }

    /// セッション ID を指定して再開するための指定 (`--resume` / `resume`)。
    /// 未対応なら ""。`apply_resume_id()` から使う。
    pub fn resume_id_flag(&self) -> &'static str {
        session_entry(self.bin).map(|e| e.2).unwrap_or("")
    }

    /// ID 指定再開はできるのに保存先を列挙できない理由。該当しなければ ""。
    #[allow(dead_code)] // 一覧が空のときの説明文として UI へ出す予定
    pub fn no_store_reason(&self) -> &'static str {
        session_entry(self.bin).map(|e| e.3).unwrap_or("")
    }

    /// 実行ファイル名の直後に必ず要る引数 (`kiro-cli chat --tui` の `chat --tui` 等)。
    /// 不要な CLI では ""。
    pub fn launch_args(&self) -> &'static str {
        launch_args_for(self.bin)
    }

    /// そのまま端末へ打てる起動コマンド。`launch_args` が無ければ `bin` と同じ。
    pub fn launch_command(&self) -> String {
        let args = self.launch_args();
        if args.is_empty() {
            self.bin.to_string()
        } else {
            format!("{} {args}", self.bin)
        }
    }
}

/// 「フラグ + 値」の 2 トークンで自動承認になる指定の表。
///
/// `--permission-mode bypassPermissions` のように値まで見ないと bypass か判定できない
/// ものをここへ集める。`--flag=値` の 1 トークン形も同じ表から導く
/// (ロジック側にフラグ名や値のリテラルを一切置かないため)。
const TWO_TOKEN_BYPASS: &[(&str, &[&str])] = &[
    // claude / devin / ante (`--permission-mode yolo`) / (orca 版) grok
    (
        "--permission-mode",
        &["bypassPermissions", "bypass", "yolo"],
    ),
    // gemini / qwen — `--approval-mode yolo` (実機 `gemini --help` で確認)
    ("--approval-mode", &["yolo"]),
];

/// 過去セッションの保存先の種類。列挙器 (session_picker.rs) がこの値で分岐する。
///
/// gemini は `~/.gemini/tmp/<sha256>/chats/` が空のことが多く実機で当てにならないため、
/// 意図的に `None` (= 一覧は出さず、従来どおり `resume_flag` での再開に落とす)。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)] // UI 配線は後続ウェーブ (session_picker.rs 経由で使う)
pub enum SessionStore {
    /// 過去セッションを列挙できない。
    None,
    /// `~/.claude/projects/<エンコード済み cwd>/<uuid>.jsonl` — プロジェクト単位。
    ClaudeProjects,
    /// `~/.codex/sessions/YYYY/MM/DD/rollout-<ts>-<uuid>.jsonl` — 全プロジェクト混在。
    CodexRollouts,
}

/// bin → (保存先, ID 指定再開の指定) のカタログ。
///
/// **AgentSpec のフィールドとして持たない理由**: `AgentSpec` の構造体リテラルは
/// 他モジュール (diagnostician.rs のテスト用フィクスチャ) にも存在し、フィールドを
/// 増やすとそれらが一斉にコンパイルエラーになる。ここでは bin をキーにした別テーブルで
/// 保持し、参照は必ず `AgentSpec::session_store()` / `resume_id_flag()` 経由にする
/// (エージェント固有の知識をコード中へ直書きしないという原則は保たれる)。
/// 4 つ目の要素は「保存先を列挙できないのに ID 指定再開だけ持つ理由」。
/// `SessionStore::None` と ID 指定再開が同居するときは **必ず** 埋める
/// (テスト `resume_id_without_store_has_a_reason` が空を落とす)。
const SESSION_STORES: &[(&str, SessionStore, &str, &str)] = &[
    // claude: フラグ型。`claude --resume <id>` — 実機 `claude --help` で確認
    ("claude", SessionStore::ClaudeProjects, "--resume", ""),
    // codex: サブコマンド型。`codex resume <id>` (bin の直後に挟む) — 実機 `codex resume --help` で確認
    ("codex", SessionStore::CodexRollouts, "resume", ""),
    // agy (Antigravity): `agy --conversation <ID>` — 実機 `agy --help` で確認。
    ("agy", SessionStore::None, "--conversation", "会話は Antigravity のプロジェクト側に保存され、ローカルの一覧可能なファイル群として公開されていない"),
    // droid: `droid --resume [sessionId]` — 実機 `droid --help` で確認。
    ("droid", SessionStore::None, "--resume", "セッションは `droid search` 経由でしか引けず、一覧可能な transcript ディレクトリが公開されていない"),
    // cursor-agent: `cursor-agent --resume [chatId]` — 実機 `cursor-agent --help` で確認。
    ("cursor-agent", SessionStore::None, "--resume", "チャットはクラウド側に保存され、ローカルに一覧可能な transcript が無い"),
    // ante: `ante --resume <SESSION_ID>` — 出典 https://ante.run/reference/cli-reference
    ("ante", SessionStore::None, "--resume", "保存先は ~/.ante/sessions/<id>/ だが、session_picker がまだこの形式を読めない"),
    // acli (Rovo Dev): `acli rovodev run --restore <UUID>`
    // 出典 https://support.atlassian.com/rovo/docs/manage-sessions-in-rovo-dev-cli/
    ("acli", SessionStore::None, "--restore", "保存先は ~/.rovodev/sessions/ だが、session_picker がまだこの形式を読めない"),
    // codebuff: `codebuff --continue [conversation-id]`
    // 出典 CodebuffAI/codebuff cli/src/cli-args.ts
    ("codebuff", SessionStore::None, "--continue", "保存先は ~/.config/manicode/projects/ 配下で公式ドキュメントに記載が無く、形式が保証されない"),
];

fn session_entry(
    bin: &str,
) -> Option<&'static (&'static str, SessionStore, &'static str, &'static str)> {
    SESSION_STORES.iter().find(|e| e.0 == bin)
}

/// bin → 「どのアカウント/プロファイルで動くか」を決める環境変数のカタログ。
///
/// プリセットの `env` にこれらのどれかが入っていれば、**同じ CLI でも別の枠**を
/// 食う (= レート制限のフェイルオーバー先になれる)。値そのものは秘密になり得るので
/// 呼び出し側 (`crate::failover::account_key`) がハッシュ化してから持ち回る。
///
/// `AgentSpec` のフィールドにしない理由は [`SESSION_STORES`] と同じ
/// (他モジュールに `AgentSpec` の構造体リテラルがあり、増やすと一斉に壊れる)。
/// 3 つ目の要素は 2 つ目の**部分集合**で、「会話の保存先そのものを引っ越す」変数。
/// これが違う切替先では過去の会話を再開できない (`--continue` を付けても中身が無い)。
const ACCOUNT_ENVS: &[(&str, &[&str], &[&str])] = &[
    // claude: 設定ディレクトリを分けると別ログイン + 会話も別置き場になる。
    (
        "claude",
        &[
            "CLAUDE_CONFIG_DIR",
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_AUTH_TOKEN",
            "ANTHROPIC_BASE_URL",
        ],
        &["CLAUDE_CONFIG_DIR"],
    ),
    // codex: `CODEX_HOME` が認証情報と rollout の置き場所。
    (
        "codex",
        &["CODEX_HOME", "OPENAI_API_KEY", "OPENAI_BASE_URL"],
        &["CODEX_HOME"],
    ),
    (
        "gemini",
        &["GEMINI_API_KEY", "GOOGLE_API_KEY", "GOOGLE_CLOUD_PROJECT"],
        &[],
    ),
    (
        "qwen",
        &["DASHSCOPE_API_KEY", "OPENAI_API_KEY", "OPENAI_BASE_URL"],
        &[],
    ),
    ("cursor-agent", &["CURSOR_API_KEY"], &[]),
    (
        "opencode",
        &["OPENCODE_CONFIG", "ANTHROPIC_API_KEY", "OPENAI_API_KEY"],
        &["OPENCODE_CONFIG"],
    ),
    ("crush", &["ANTHROPIC_API_KEY", "OPENAI_API_KEY"], &[]),
    (
        "aider",
        &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "AIDER_MODEL"],
        &[],
    ),
    (
        "goose",
        &["ANTHROPIC_API_KEY", "OPENAI_API_KEY", "GOOSE_PROVIDER"],
        &[],
    ),
    ("grok", &["XAI_API_KEY"], &[]),
    ("droid", &["FACTORY_API_KEY"], &[]),
];

/// この CLI でアカウント (プロファイル) を分ける環境変数名。未知の bin では空。
///
/// 空を返す CLI は「プリセットの env ではアカウントを分けられない」という意味で、
/// フェイルオーバー先としては**別 CLI** としてしか扱われない。
pub fn account_env_keys(bin: &str) -> &'static [&'static str] {
    ACCOUNT_ENVS
        .iter()
        .find(|e| e.0 == bin)
        .map(|e| e.1)
        .unwrap_or(&[])
}

/// 会話の保存先そのものを引っ越す環境変数名 ([`account_env_keys`] の部分集合)。
/// これが一致しない切替先では、再開指定 (`--continue` 等) を付けてはいけない。
pub fn session_store_env_keys(bin: &str) -> &'static [&'static str] {
    ACCOUNT_ENVS
        .iter()
        .find(|e| e.0 == bin)
        .map(|e| e.2)
        .unwrap_or(&[])
}

/// 実行ファイル名だけでは起動できない CLI の「bin の直後に必ず要る引数」。
///
/// orca (`src/shared/tui-agent-config.ts` の `launchCmd`) と同じ考え方で、
/// エージェント固有の起動形をここのデータだけに置く。`AgentSpec` のフィールドに
/// しないのは `SESSION_STORES` と同じ理由 (他モジュールに構造体リテラルがある)。
const LAUNCH_ARGS: &[(&str, &str)] = &[
    // kiro-cli: 素の `kiro-cli` は CLI シェル。エージェント TUI は `chat --tui`
    // (`--trust-all-tools` も `chat` 側に付く)。出典: orca tui-agent-config.ts
    ("kiro-cli", "chat --tui"),
    // hermes: 素の `hermes` は旧 REPL。全画面エージェント UI は `--tui`。
    // 出典: orca tui-agent-config.ts
    ("hermes", "--tui"),
    // acli: Atlassian CLI 本体。Rovo Dev エージェントは `rovodev run` サブコマンド。
    ("acli", "rovodev run"),
];

/// `bin` → 起動時に必ず添える引数。無ければ ""。
pub fn launch_args_for(bin: &str) -> &'static str {
    LAUNCH_ARGS
        .iter()
        .find(|e| e.0 == bin)
        .map(|e| e.1)
        .unwrap_or("")
}

/// 初回起動時に config.toml へ書き出す「おすすめプリセット」の並び (bin 名)。
///
/// ここに無い CLI が使えないわけではない — エージェントピッカーは
/// [`AGENT_CATALOG`] 全件を出すので、いつでもプリセットに追加できる。
/// 既定を短く保つのは、初回のプルダウンが 60 行になるのを避けるため。
/// **この機で `--help` を実行して全項目を確認できた CLI だけ**を並べている。
pub const DEFAULT_PRESET_BINS: &[&str] =
    &["claude", "codex", "gemini", "agy", "cursor-agent", "droid"];

/// 実行ファイル名の別名表。
///
/// orca の `TuiAgent` ID / `detectCmdAliases` と、実際に PATH へ入る実行ファイル名の
/// ズレを吸収する。**ロジック側に別名を直書きしない**ための表。
///
/// `windows_safe = false` の別名は Windows では解決しない —
/// `cmd` は Windows 組み込みシェル (cmd.exe) と衝突するため
/// (出典: orca tui-agent-config.ts の command-code コメント)。
///
/// 【意図的に入れていない別名】
/// - `kiro` → `kiro-cli`: `kiro` は IDE 本体の実行ファイルで、エージェントではない。
/// - `continue` → `cn`: `continue` は bash/zsh の予約語で、実行ファイルとして解決しない
///   (出典: orca tui-agent-config.ts の continue コメント)。
const AGENT_ALIASES: &[(&str, &str, bool)] = &[
    ("antigravity", "agy", true),
    ("antigravity-cli", "agy", true),
    ("claude-code", "claude", true),
    ("gemini-cli", "gemini", true),
    ("mimo-code", "mimo", true),      // orca の TuiAgent ID
    ("qwen-code", "qwen", true),      // orca の TuiAgent ID
    ("mistral-vibe", "vibe", true),   // orca detectCmdAliases
    ("cursor", "cursor-agent", true), // orca の TuiAgent ID
    ("aug", "auggie", true),          // orca の TuiAgent ID
    ("cb", "codebuff", true),         // npm codebuff が同梱する短縮 bin
    // orca は `rovo` という単独実行ファイルを想定しているが、実在が確認できない
    // (npm/PyPI/Homebrew いずれにも無い)。実体の `acli rovodev run` へ寄せる。
    ("rovo", "acli", true),
    ("rovodev", "acli", true),
    ("cmd", "command-code", false), // Windows の cmd.exe と衝突するため除外
];

/// 承認モードを自動適用できる CLI カタログ。
///
/// `claude` の `--permission-mode bypassPermissions` と `devin` の
/// `--permission-mode bypass` は 2 トークン形式のため `apply_approval` 側で別処理する。
///
/// 一括自動承認フラグを持たない CLI (goose / auggie / crush / codebuff …) も
/// 載せてよい。`auto_flag` を空にしておけば「全自動」プリセットは作られず、
/// `apply_approval` も書き換えないので壊れた項目にはならない。
/// **やってはいけないのはフラグの捏造** — 出典の無いフラグは空のままにする。
///
/// 【意図的に除外している起動形】
/// - orca の `claude-agent-teams` (= `orca claude-teams`): orca 本体が提供する
///   ラッパーであって独立した CLI ではないため、Zaivern からは扱わない。
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
        resume_flag: "--continue",
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
        resume_flag: "resume --last",
    },
    // Gemini CLI (Google)。**この機の `gemini --help` 実行結果から全項目を確認済み**。
    // `--approval-mode yolo` (2 トークン) は TWO_TOKEN_BYPASS 表で除去される。
    // `--resume` は「"latest" か番号」を取り、セッション ID は取らない (だから
    // resume_id_flag は空 — orca は `gemini --resume <id>` を使うが、この機の
    // gemini 0.52 の help とは食い違うため採用しない)。
    AgentSpec {
        bin: "gemini",
        label: "Gemini CLI",
        icon: "✨",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo", "-y", "--approval-mode=yolo"],
        headless: "-p",
        model_flag: "-m",
        install: "npm i -g @google/gemini-cli",
        note: "`--approval-mode` は default|auto_edit|yolo|plan。全自動は `--yolo`",
        switch_keys: "",
        switch_hint: "",
        resume_flag: "--resume latest",
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
        resume_flag: "",
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
        // 実機 help の `--resume [chatId]` は ID 省略時の挙動が不明なため、
        // 直前再開 (resume_flag) には使わず、ID 指定再開 (SESSION_STORES) だけに使う。
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
    },
    // Antigravity CLI (Google)。全自動フラグは claude と同名。自動承認環境変数も完全サポート。
    AgentSpec {
        bin: "agy",
        label: "Antigravity",
        icon: "🚀",
        auto_flag: "--dangerously-skip-permissions",
        auto_env: &[("ANTIGRAVITY_AUTO_APPROVE", "1"), ("AGY_AUTO_APPROVE", "1")],
        strip: &[
            "--dangerously-skip-permissions",
            "--yolo",
            "--auto-approve",
            "--yes",
            "-y",
        ],
        headless: "-p",
        model_flag: "--model",
        install: "curl -fsSL https://antigravity.google/cli/install.sh | bash",
        note: "",
        switch_keys: "\x1b[Z",
        switch_hint: "権限モード切替 (Shift+Tab)",
        // 実機 `agy --help`: `--continue` (`-c`) = 直前の会話を継続。
        resume_flag: "--continue",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
    },
    // bin は `cmd` ではなく `command-code`。`cmd` は Windows 組み込みシェル
    // (cmd.exe) と衝突し、`cmd /c ...` を Command Code と誤認するため
    // (出典: orca src/shared/tui-agent-config.ts の command-code コメント)。
    // 非 Windows では別名 `cmd` として引き続き解決する (AGENT_ALIASES)。
    AgentSpec {
        bin: "command-code",
        label: "Command Code",
        icon: "⌘",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo", "--auto-accept", "--dangerously-skip-permissions"],
        headless: "-p",
        model_flag: "-m",
        install: "npm i -g command-code",
        note: "短縮名 `cmd` は Windows の cmd.exe と衝突するため `command-code` で起動する",
        switch_keys: "",
        switch_hint: "",
        resume_flag: "",
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
        resume_flag: "",
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
        // 実機 `droid --help`: `-r, --resume [sessionId]` は ID 省略で直近セッションを再開。
        resume_flag: "--resume",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
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
        resume_flag: "",
    },
    // Rovo Dev (Atlassian)。orca は `rovo` という単独実行ファイルを起動しようとするが、
    // npm / PyPI / Homebrew のいずれにも `rovo` は存在せず (2026-07 時点で確認)、
    // 実体は ACLI 拡張の `acli rovodev run`。こちらを正とする。
    // 出典: https://support.atlassian.com/rovo/docs/rovo-dev-cli-commands/
    //       https://developer.atlassian.com/cloud/acli/guides/install-macos/
    AgentSpec {
        bin: "acli",
        label: "Rovo Dev",
        icon: "💠",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo"],
        headless: "acli rovodev run",
        model_flag: "",
        install: "brew tap atlassian/homebrew-acli && brew install acli",
        note: "エージェントは `acli rovodev run`。初回は `acli rovodev auth login` が要る。モデルは ~/.rovodev/config.yml 側",
        switch_keys: "",
        switch_hint: "",
        // `--restore` は値なしで直近セッションを復元 (公式ドキュメント)
        resume_flag: "--restore",
    },
    // Ante (Antigma Labs)。単独バイナリ配布 (npm の `ante` は無関係の空パッケージ)。
    // 出典: https://ante.run/reference/cli-reference / https://ante.run/start/quickstart
    AgentSpec {
        bin: "ante",
        label: "Ante",
        icon: "🃏",
        auto_flag: "--yolo",
        auto_env: &[],
        strip: &["--yolo", "--permission-mode=yolo"],
        headless: "-p",
        model_flag: "-m",
        install: "curl -fsSL https://ante.run/install.sh | bash",
        note: "`--permission-mode` は strict|auto|yolo。`-p` は 1 回実行して終了する",
        switch_keys: "",
        switch_hint: "",
        // 直前セッションの再開は TUI の `/resume` だけで、起動フラグは未確認 (だから空)。
        // ID 指定再開 `--resume <SESSION_ID>` は SESSION_STORES 側にある。
        resume_flag: "",
    },
    // OpenClaw (OpenClaw Foundation)。チャットアプリとエージェントをつなぐゲートウェイで、
    // 起動フラグ形式の一括自動承認は無い (`openclaw exec-policy preset yolo` で設定する)。
    // 出典: https://github.com/openclaw/openclaw/blob/main/docs/cli/approvals.md
    //       https://github.com/openclaw/openclaw/blob/main/docs/cli/agent.md
    //       https://registry.npmjs.org/openclaw/latest (bin: openclaw)
    AgentSpec {
        bin: "openclaw",
        label: "OpenClaw",
        icon: "🐾",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "openclaw agent exec",
        model_flag: "",
        install: "npm i -g openclaw@latest",
        note: "自動承認は起動フラグではなく `openclaw exec-policy preset yolo`。`--model` は `agent exec` サブコマンド側",
        switch_keys: "",
        switch_hint: "",
        resume_flag: "",
    },
    // ── ここから下は orca (stablyai/orca) が起動対象にしている CLI のうち、
    //    Zaivern に無かったもの。出典は各エントリのコメント参照。
    //
    // Codebuff: 一括自動承認フラグを orca も持っていない
    // (orca src/shared/tui-agent-permissions.ts の YOLO_TUI_AGENT_ARGS に無い)。
    // フラグを捏造しないので auto_flag は空 = 「全自動」プリセットは作られない。
    // bin / インストール先は npm registry (registry.npmjs.org/codebuff, bin: codebuff|cb) で確認。
    AgentSpec {
        bin: "codebuff",
        label: "Codebuff",
        icon: "🔨",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "",
        model_flag: "",
        install: "npm i -g codebuff",
        note: "一括自動承認フラグもヘッドレス実行フラグも無い。モデルではなく `--agent <id>` で切り替える",
        switch_keys: "",
        switch_hint: "",
        // `--continue [conversation-id]` — 出典: CodebuffAI/codebuff cli/src/cli-args.ts
        resume_flag: "--continue",
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
        resume_flag: "",
    },
];

// ══════════════════════════════════════════════════════════════════════
//  自動YES: プロンプト応答表 (データ)
// ══════════════════════════════════════════════════════════════════════
//
// 「画面にこの文言が出ていたら、この キー列 を PTY へ送る」という対応表。
// **ここだけがエージェント固有の知識**で、判定ロジック (terminal.rs の
// auto_yes_reply) は表を上から順に見るだけ。CLI 側が文言を変えても
// 表の 1 行を直せば済む(ロジックには一切リテラルを置かない)。
//
// ユーザーは config.toml の `[[auto_yes_rules]]` で自分のルールを足せる
// (再コンパイル不要)。ユーザールールは常にこの組み込み表より先に評価される。

/// 自動YESの応答ルール 1 件。
#[derive(Clone, Copy)]
pub struct PromptRule {
    /// 対象エージェント(カタログの `bin` 名)。`""` は全エージェント共通。
    /// セッションのエージェントが判っているときだけ絞り込みに使う。
    pub agent: &'static str,
    /// 画面に**すべて**含まれていたら一致(AND 条件)。
    /// 単語 1 個で決めず、「見出し」+「肯定選択肢」のように 2 つ以上を
    /// 組み合わせることで、本文中にたまたま "yes" が出ただけの行で
    /// 誤爆しないようにする。
    pub needles: &'static [&'static str],
    /// 1 つでも画面に含まれていたら**不一致**にする除外語。
    pub avoid: &'static [&'static str],
    /// 一致したとき PTY へ送るキー列。
    pub reply: &'static [u8],
    /// UI 通知に出す説明。
    pub desc: &'static str,
}

/// Antigravity CLI (`agy`) の選択 UI が必ず出すフッタ。
/// この 1 行があれば「矢印キーで選ぶ承認メニューが開いている」と断定できる。
/// 実機バイナリに埋め込まれた文言をそのまま使っている。
const AGY_SELECT_HINT: &str = "[Use arrow keys to navigate, Enter to select]";

/// 組み込みのプロンプト応答表。**上から順に**評価し、最初に一致したものを使う。
///
/// ## Antigravity (`agy`) の実測メモ
/// 文言と UI の形は、インストール済み `agy` 本体 (Go バイナリ) に埋め込まれた
/// 文字列から採取した実物である。判ったこと:
///
/// - 承認 UI は **矢印キー + Enter** 方式 (`[Use arrow keys to navigate, Enter to select]`)。
///   番号キーによる選択でも `(y/n)` でもないため、Zaivern が他 CLI 向けに持っていた
///   「1. Yes」「(y/n)」パターンはどれも一致せず、**自動YESが素通りしていた**。
///   これがユーザー報告「`.gemini` の設定を入れないと自動YESが動かない」の正体。
/// - 肯定側の選択肢は必ず `Yes, ...` で始まり、リストの**先頭(既定選択)**にある。
///   よって確定キーは Enter (`\r`)。CLI 自身が "Enter to select" と案内している。
/// - `--dangerously-skip-permissions` は本物のフラグ (`agy --help` で確認)。
///   バイナリにも "dangerously-skip-permissions set, auto-approving all tool
///   permissions" の文字列がある。ただしこれが効くのはツール権限だけで、
///   フォルダ信頼確認などの起動時プロンプトは別途出るため、この表が要る。
pub static PROMPT_RULES: &[PromptRule] = &[
    // ── Antigravity CLI (agy) ───────────────────────────────────────
    // ファイル読み取り許可。見出し + 肯定選択肢の両方が出ていることを要求する。
    PromptRule {
        agent: "agy",
        needles: &["Allow access to this file?", "Yes, allow access"],
        avoid: &[],
        reply: b"\r",
        desc: "Antigravity のファイル参照許可に Enter",
    },
    // ファイル新規作成の許可。
    PromptRule {
        agent: "agy",
        needles: &["Allow creation of this file?", "Yes, allow creation"],
        avoid: &[],
        reply: b"\r",
        desc: "Antigravity のファイル作成許可に Enter",
    },
    // 編集内容のレビュー (差分の受け入れ)。
    PromptRule {
        agent: "agy",
        needles: &["Accept this file edit?", "Yes, accept this change"],
        avoid: &[],
        reply: b"\r",
        desc: "Antigravity の編集受け入れに Enter",
    },
    // フォルダ信頼確認 (起動直後)。肯定が既定選択。
    PromptRule {
        agent: "agy",
        needles: &["Yes, I trust this folder"],
        avoid: &[],
        reply: b"\r",
        desc: "Antigravity のフォルダ信頼確認に Enter",
    },
    // コマンド実行などの汎用権限要求。`Persist to settings.json` 付きの
    // 「常に許可」ではなく、先頭の一回だけ許可を選ぶ。
    PromptRule {
        agent: "agy",
        needles: &["Requesting permission", "Yes, grant permission for"],
        avoid: &[],
        reply: b"\r",
        desc: "Antigravity の権限要求に Enter",
    },
    PromptRule {
        agent: "agy",
        needles: &["Requesting permission", "Yes, approve"],
        avoid: &[],
        reply: b"\r",
        desc: "Antigravity の承認要求に Enter",
    },
    // 上のどれにも当たらない **将来の** 承認メニュー用の総取りルール。
    // 選択 UI のフッタが出ていて、肯定選択肢 (`Yes, `) が画面にあるときだけ。
    // 文言が変わってもフッタは選択ウィジェット共通なので、この 1 行が効き続ける。
    PromptRule {
        agent: "agy",
        needles: &[AGY_SELECT_HINT, "Yes, "],
        avoid: &[],
        reply: b"\r",
        desc: "Antigravity の選択メニューに Enter",
    },
];

/// **絶対に自動応答しない**画面の目印。
///
/// 自動YESは「エージェントのツール承認を代わりに押す」機能であって、
/// OS の管理者権限昇格まで肩代わりするものではない。ここに載る語が画面に
/// あるときは、組み込み表もユーザールールも汎用ヒューリスティックも
/// すべて黙り、ユーザー本人の判断に委ねる。
///
/// NOTE: 既存の汎用ヒューリスティック側には元々この種のガードが無く、
/// 「Overwrite? (y/n)」や「削除しますか」にも YES を返す設計だった
/// (= 全自動YESはユーザーが明示的にオンにする「全部はい」モード)。
/// その挙動は変えていない — ここで止めるのは管理者権限昇格だけ。
pub static PROMPT_NEVER: &[&str] = &[
    // agy: sudo によるサンドボックス初期設定。実測文言。
    "one-time admin escalation",
    "Administrator privileges are required",
];

// ══════════════════════════════════════════════════════════════════════
//  番号入力メニュー(アンケート/選択式プロンプト)の語彙表 (データ)
// ══════════════════════════════════════════════════════════════════════
//
// 「1. Yes / 2. No … 番号を入力してください」のように **数字 + Enter** を
// 要求してくる画面用。ユーザー報告「アンケートを数字で入力しないと進まなく
// なっていた」の対策で、CLI が出す番号メニューに自動で答えるために使う。
//
// 判定ロジック (terminal.rs の `numbered_menu_reply`) はこの表を引くだけで、
// CLI 固有の文言をロジック側に一切持たない。照合は **小文字化した部分一致**
// (日本語はそのまま)。表に 1 行足せば再コンパイルだけで新しい CLI に効く。

/// 「番号を打て」と言っている行の目印。
/// **これが画面に無ければ番号メニューとみなさない** — 出力中のただの箇条書き
/// (手順書の「1. …」など) へ数字を撃ち込まないための最重要ガード。
pub static MENU_NUMBER_HINTS: &[&str] = &[
    "enter a number",
    "enter the number",
    "enter a choice",
    "enter your choice",
    "enter selection",
    "enter 1",
    "type a number",
    "type the number",
    "select an option",
    "select a number",
    "select one",
    "select 1",
    "choose an option",
    "choose a number",
    "choose one",
    "choose 1",
    "pick a number",
    "pick an option",
    "your choice",
    "your selection",
    "番号を入力",
    "番号でお答え",
    "番号でご回答",
    "番号をお選び",
    "番号を選ん",
    "数字を入力",
    "数字でお答え",
    "いずれかの番号",
];

/// `(1-5)` `[1-3]` のような**範囲の書き方**。開始記号の直後が数字のときだけ
/// 範囲とみなす (本文中の "1-2 秒" のような書きぶりでは発火しない)。
pub static MENU_RANGE_OPENERS: &[&str] = &["(1-", "[1-", "(1〜", "[1〜", "(1～", "[1～"];

/// **矢印キーで選ぶ** UI の目印。これがある画面へは数字を送らない
/// (Antigravity のように Enter で確定する UI に数字を撃つと入力欄が汚れる)。
pub static MENU_ARROW_HINTS: &[&str] = &[
    // AGY_SELECT_HINT の中核部分。小文字化して照合する。
    "arrow keys to navigate",
    "use arrow keys",
    "use the arrow keys",
    "↑/↓",
    "↑↓",
    "j/k to move",
    "矢印キー",
    "カーソルキー",
    "上下キー",
];

/// 選択肢が「承認・肯定」を表す語 (部分一致)。
pub static MENU_AFFIRM: &[&str] = &[
    "yes",
    "allow",
    "approve",
    "accept",
    "continue",
    "proceed",
    "grant",
    "permit",
    "はい",
    "許可",
    "続行",
    "承認",
    "受け入れ",
    "実行する",
];

/// 肯定語を含んでいても**打ち消す**語。`Don't continue` / `No, exit` 対策。
pub static MENU_NEGATIONS: &[&str] = &[
    "don't",
    "do not",
    "never",
    "no,",
    "no ",
    "not ",
    "cancel",
    "exit",
    "quit",
    "abort",
    "reject",
    "deny",
    "しない",
    "やめ",
    "中止",
    "キャンセル",
    "終了",
    "拒否",
];

/// 選択肢が「見送り・スキップ」を表す語 (部分一致)。
/// アンケートや意見を聞く画面では、意見を代筆せずここを選ぶ。
pub static MENU_SKIP: &[&str] = &[
    "skip",
    "not now",
    "maybe later",
    "ask me later",
    "remind me later",
    "no thanks",
    "no, thanks",
    "dismiss",
    "don't ask",
    "do not ask",
    "not interested",
    "スキップ",
    "あとで",
    "後で",
    "今はしない",
    "回答しない",
    "答えない",
    "聞かない",
    "興味がない",
];

/// 短すぎて部分一致だと誤爆する見送り語。**完全一致**でだけ採用する
/// (`no` を部分一致にすると `not now` 以外の無関係な語まで拾う)。
pub static MENU_SKIP_EXACT: &[&str] = &["no", "n", "later", "いいえ", "不要", "パス"];

/// 評価尺度の**最も肯定的な端**を表す語。スキップ肢も肯定肢も無い
/// 「1〜5 の評点しか無い」アンケートで、どれを選ぶかを決めるのに使う。
/// 尺度が昇順でも降順でもここが当たった選択肢を選ぶので端を取り違えない。
/// 打ち消し語 (`dissatisfied` / `not ...`) を含む語はここに入れないこと。
pub static MENU_RATING_BEST: &[&str] = &[
    "very satisfied",
    "extremely satisfied",
    "completely satisfied",
    "highly satisfied",
    "very likely",
    "extremely likely",
    "very good",
    "excellent",
    "strongly agree",
    "very helpful",
    "very useful",
    "love it",
    "とても満足",
    "非常に満足",
    "大変満足",
    "大変良い",
    "とても良い",
    "非常に良い",
    "とても役立",
    "そう思う",
    "最高",
];

/// 評価尺度で**最も否定的な端**を表す語。`MENU_RATING_BEST` が当たらない
/// ときの向き推定に使う (否定端が先頭なら肯定端は末尾、という判断)。
pub static MENU_RATING_WORST: &[&str] = &[
    "very dissatisfied",
    "extremely dissatisfied",
    "not at all",
    "very unlikely",
    "very poor",
    "poor",
    "terrible",
    "strongly disagree",
    "not satisfied",
    "very bad",
    "とても不満",
    "非常に不満",
    "大変不満",
    "全く",
    "まったく",
    "とても悪い",
    "非常に悪い",
    "最低",
];

/// 画面が**アンケート/評価/感想**を聞いていると判る目印。
/// これが出ている画面では、まず見送り肢を選ぶ
/// (= ユーザーに成り代わって意見を書かない)。見送り肢が無いときだけ
/// `MENU_RATING_BEST` / `MENU_RATING_WORST` で肯定側の端を選んで先へ進める
/// — 止まったままにする方が害が大きい、というユーザーの判断による。
pub static MENU_SURVEY_MARKS: &[&str] = &[
    "survey",
    "questionnaire",
    "feedback",
    "rate ",
    "rating",
    "how would you",
    "how likely",
    "how satisfied",
    "satisfaction",
    "satisfied",
    "recommend",
    "opinion",
    "アンケート",
    "評価",
    "満足度",
    "ご意見",
    "ご感想",
    "おすすめ度",
];

/// ユーザー定義ルール (config.toml の `[[auto_yes_rules]]`)。
///
/// 登録時に `&'static` へリークして持つ。応答キーは `&'static [u8]` で
/// 扱えた方が呼び出し側 (承認バブルの `approve_reply` など) が単純になるうえ、
/// ルール数はユーザーが手で書く数しかなく、同じ内容なら再登録もしないため
/// リーク量は実質的に固定である。
static USER_RULES: std::sync::OnceLock<std::sync::RwLock<&'static [PromptRule]>> =
    std::sync::OnceLock::new();

fn user_rules_cell() -> &'static std::sync::RwLock<&'static [PromptRule]> {
    USER_RULES.get_or_init(|| std::sync::RwLock::new(&[]))
}

/// いま有効なユーザー定義ルール。
pub fn user_prompt_rules() -> &'static [PromptRule] {
    user_rules_cell().read().map(|g| *g).unwrap_or(&[])
}

/// config.toml のユーザー定義ルールを取り込む(設定読み込みのたびに呼ぶ)。
///
/// `(pattern, reply, agent)` の 3 つ組で受け取る。`agent` が空なら全エージェント。
/// `pattern` か `reply` が空の行は無視する(書きかけの設定で暴発させない)。
/// 中身が前回と同じなら何もしない — 設定の読み直しでリークを積み増さないため。
pub fn set_user_prompt_rules(rules: &[(String, String, String)]) {
    let cell = user_rules_cell();
    let cur = cell.read().map(|g| *g).unwrap_or(&[]);
    let wanted: Vec<(&str, &str, &str)> = rules
        .iter()
        .map(|(p, r, a)| (p.as_str(), r.as_str(), a.as_str()))
        .filter(|(p, r, _)| !p.is_empty() && !r.is_empty())
        .collect();
    let same = cur.len() == wanted.len()
        && cur.iter().zip(&wanted).all(|(c, w)| {
            c.needles.first().copied() == Some(w.0) && c.reply == w.1.as_bytes() && c.agent == w.2
        });
    if same {
        return;
    }
    let built: Vec<PromptRule> = wanted
        .iter()
        .map(|(p, r, a)| PromptRule {
            agent: Box::leak(a.to_string().into_boxed_str()),
            needles: Box::leak(
                vec![&*Box::leak(p.to_string().into_boxed_str())].into_boxed_slice(),
            ),
            avoid: &[],
            reply: Box::leak(r.to_string().into_boxed_str()).as_bytes(),
            desc: Box::leak(format!("ユーザー定義ルール「{p}」").into_boxed_str()),
        })
        .collect();
    if let Ok(mut g) = cell.write() {
        *g = Box::leak(built.into_boxed_slice());
    }
}

/// 画面が「自動応答してはいけない」種類か。
pub fn prompt_never_answer(text: &str) -> bool {
    PROMPT_NEVER.iter().any(|m| text.contains(m))
}

/// 1 件のルールが画面に一致するか。
fn rule_matches(rule: &PromptRule, text: &str, agent: Option<&str>) -> bool {
    // エージェント絞り込み: セッションのエージェントが判っていて、かつ
    // ルールが別のエージェント専用なら対象外。判らないときは全ルールを見る
    // (カタログ外から呼ばれる分類ヘルパ用。needles が具体的なので誤爆しない)。
    if !rule.agent.is_empty() {
        if let Some(a) = agent {
            if a != rule.agent {
                return false;
            }
        }
    }
    if rule.needles.is_empty() {
        return false;
    }
    rule.needles.iter().all(|n| text.contains(n)) && !rule.avoid.iter().any(|n| text.contains(n))
}

/// 応答表から、画面に合う応答キーと説明を引く。
/// ユーザー定義ルールを先に見るので、組み込みの判断を上書きできる。
pub fn prompt_rule_reply(text: &str, agent: Option<&str>) -> Option<(&'static [u8], &'static str)> {
    prompt_rule_reply_with(user_prompt_rules(), text, agent)
}

/// [`prompt_rule_reply`] の本体。ユーザー定義ルールを引数で受け取る。
///
/// プロセス全体で共有するレジストリを触らずに済むので、テストが並列に
/// 走っても互いのルールが混ざらない (レジストリ経由だと 1 本のテストが
/// 登録したルールが同時実行中の別テストの画面に一致してしまう)。
pub fn prompt_rule_reply_with(
    user: &[PromptRule],
    text: &str,
    agent: Option<&str>,
) -> Option<(&'static [u8], &'static str)> {
    if prompt_never_answer(text) {
        return None;
    }
    user.iter()
        .chain(PROMPT_RULES.iter())
        .find(|r| rule_matches(r, text, agent))
        .map(|r| (r.reply, r.desc))
}

/// プロンプト指紋 (terminal.rs) が「承認プロンプトの行」を見分けるための目印。
/// 応答表の needles をそのまま流用する — 表に足せば指紋にも自動で効く。
pub fn prompt_sig_marks() -> impl Iterator<Item = &'static str> {
    user_prompt_rules()
        .iter()
        .chain(PROMPT_RULES.iter())
        .flat_map(|r| r.needles.iter().copied())
}

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
/// `antigravity` や `antigravity-cli` などのエイリアス名も正しく吸収する。
pub fn spec_for_bin(bin: &str) -> Option<&'static AgentSpec> {
    if let Some(spec) = AGENT_CATALOG.iter().find(|s| s.bin == bin) {
        return Some(spec);
    }
    let normalized = AGENT_ALIASES
        .iter()
        .find(|(alias, _, windows_safe)| *alias == bin && (*windows_safe || !cfg!(windows)))
        .map(|(_, target, _)| *target)
        .unwrap_or(bin);
    AGENT_CATALOG.iter().find(|s| s.bin == normalized)
}

/// コマンドの先頭トークンが承認モード対応 CLI なら (auto フラグ, 除去対象) を返す。
fn known_agent(command: &str) -> Option<(&'static str, &'static [&'static str])> {
    spec_for_command(command).map(|s| (s.auto_flag, s.strip))
}

/// `--permission-mode <値>` のような 2 トークン指定が bypass 系か。
/// 判定は `TWO_TOKEN_BYPASS` の表からのみ導く。
fn is_bypass_two_token(flag: &str, value: &str) -> bool {
    TWO_TOKEN_BYPASS
        .iter()
        .any(|(f, values)| *f == flag && values.iter().any(|v| v.eq_ignore_ascii_case(value)))
}

/// `--permission-mode=bypassPermissions` のような `=` 区切り 1 トークン形が bypass 系か。
fn is_bypass_joined(token: &str) -> bool {
    TWO_TOKEN_BYPASS.iter().any(|(flag, values)| {
        token
            .strip_prefix(flag)
            .and_then(|rest| rest.strip_prefix('='))
            .map(|v| values.iter().any(|allowed| allowed.eq_ignore_ascii_case(v)))
            .unwrap_or(false)
    })
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
/// 判定はカタログの `auto_flag` / `strip` の和集合と `TWO_TOKEN_BYPASS` の表から
/// のみ導出する(フラグ名をロジックへ直書きしない)。
pub fn command_is_bypass(command: &str) -> bool {
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let head_spec = spec_for_command(command);
    for (i, tok) in tokens.iter().enumerate() {
        // `--permission-mode bypassPermissions` / `--approval-mode yolo` など
        if TWO_TOKEN_BYPASS.iter().any(|(f, _)| f == tok) {
            if tokens.get(i + 1).map(|v| is_bypass_two_token(tok, v)) == Some(true) {
                return true;
            }
            continue;
        }
        // `--permission-mode=bypassPermissions` など `=` 区切り 1 トークン形
        if TWO_TOKEN_BYPASS.iter().any(|(f, _)| {
            tok.starts_with(f) && tok.len() > f.len() && tok.as_bytes()[f.len()] == b'='
        }) {
            if is_bypass_joined(tok) {
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
        // `--permission-mode bypassPermissions` / `--approval-mode yolo`
        // (スペース区切り2トークン)を除去。`--permission-mode plan` などは残す。
        if TWO_TOKEN_BYPASS.iter().any(|(f, _)| *f == tok)
            && tokens
                .get(i + 1)
                .map(|v| is_bypass_two_token(tok, v))
                .unwrap_or(false)
        {
            i += 2;
            continue;
        }
        // `--permission-mode=bypassPermissions`(= 区切り1トークン)を除去
        if is_bypass_joined(tok) {
            i += 1;
            continue;
        }
        parts.push(tok);
        i += 1;
    }
    if approval == Approval::Auto && !auto_flag.is_empty() {
        parts.push(auto_flag);
    }
    parts.join(" ")
}

/// セッション復元時、コマンドへ「前回の会話を再開する」指定を足す。
///
/// claude はフラグ型 (`claude --continue`)、codex はサブコマンド型
/// (`codex resume --last` — 実行ファイル名の直後に挟まないとサブコマンドとして
/// 解釈されない)。再開機能を確認できていない CLI は `resume_flag` が "" なので
/// 素のまま返す (誤ったフラグで起動に失敗するより、会話が新規になる方がまし)。
/// 復元経路でのみ使うこと — 通常の起動に付けると意図しない再開になる。
pub fn apply_resume(command: &str, spec: &AgentSpec) -> String {
    if spec.resume_flag.is_empty() {
        return command.to_string();
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.is_empty() {
        return command.to_string();
    }
    let resume: Vec<&str> = spec.resume_flag.split_whitespace().collect();
    // 既に再開指定が入っているなら二重に付けない
    if tokens.iter().any(|t| *t == resume[0]) {
        return command.to_string();
    }
    if resume[0].starts_with('-') {
        // フラグ型: 末尾に付けるだけでよい
        return format!("{} {}", command.trim(), spec.resume_flag);
    }
    // サブコマンド型: 実行ファイル名の直後に挟む (末尾では引数扱いされてしまう)
    let mut out: Vec<&str> = Vec::with_capacity(tokens.len() + resume.len());
    out.push(tokens[0]);
    out.extend(resume.iter());
    out.extend(tokens[1..].iter());
    out.join(" ")
}

/// 「この過去セッションを開く」用に、コマンドへ **セッション ID 指定の再開** を足す。
///
/// `apply_resume` (= 直前の会話を再開) の ID 指定版。フラグ型 (`claude --resume <id>`)
/// とサブコマンド型 (`codex resume <id>` — 実行ファイル名の直後) の扱いは
/// `apply_resume` と同じ規則。ID 指定再開に未対応の CLI、および ID が空/不正
/// (空白やシェルのメタ文字を含む) なら **何もせず素のコマンドを返す** —
/// 壊れた引数で起動に失敗するより、新規会話になる方がまし。
///
/// 二重付与ガード: 既に ID 指定 (`--resume` / `resume`) または直前再開指定
/// (`--continue` など `resume_flag` の先頭トークン) が入っているコマンドは触らない。
#[allow(dead_code)] // UI 配線は後続ウェーブ (session_picker.rs の一覧から呼ぶ)
pub fn apply_resume_id(command: &str, spec: &AgentSpec, id: &str) -> String {
    let flag = spec.resume_id_flag();
    let id = id.trim();
    if flag.is_empty() || id.is_empty() || !is_safe_session_id(id) {
        return command.to_string();
    }
    let tokens: Vec<&str> = command.split_whitespace().collect();
    if tokens.is_empty() {
        return command.to_string();
    }
    let resume: Vec<&str> = flag.split_whitespace().collect();
    // 既に再開指定 (ID 指定・直前再開のどちらでも) があるなら二重に付けない
    let already = spec.resume_flag.split_whitespace().next().unwrap_or("");
    if tokens
        .iter()
        .any(|t| *t == resume[0] || (!already.is_empty() && *t == already))
    {
        return command.to_string();
    }
    if resume[0].starts_with('-') {
        // フラグ型: 末尾に `--resume <id>` を足す
        return format!("{} {flag} {id}", command.trim());
    }
    // サブコマンド型: `bin resume <id> ...` の順で実行ファイル名の直後に挟む
    let mut out: Vec<&str> = Vec::with_capacity(tokens.len() + resume.len() + 1);
    out.push(tokens[0]);
    out.extend(resume.iter());
    out.push(id);
    out.extend(tokens[1..].iter());
    out.join(" ")
}

/// セッション ID としてコマンド行へ素で置いて安全か。
/// 実体はファイル名由来の UUID なので、英数と `-` `_` `.` だけを許す
/// (空白・引用符・シェルのメタ文字が入る余地を残さない)。
fn is_safe_session_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
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

/// Claude Code などのエージェントが同一マシン・他プロセス・他セッションと競合を起こさないよう、
/// セッション ID ごとに環境変数を安全に分離・アイソレートしたマップを生成する。
/// `CLAUDE_CONFIG_DIR` が未指定の場合、`~/.claude-sessions/session-{session_id}` を自動割り当てし、
/// 複数 Claude インスタンスの同時起動時のファイルロック衝突を回避する。
#[allow(dead_code)]
pub fn isolated_env_for_session(
    command: &str,
    session_id: u64,
    approval: Approval,
    preset_env: &HashMap<String, String>,
) -> HashMap<String, String> {
    let mut env = merged_env(command, approval, preset_env);
    if let Some(spec) = spec_for_command(command) {
        if spec.bin == "claude" && !env.contains_key("CLAUDE_CONFIG_DIR") {
            let isolated_path = format!("~/.claude-sessions/session-{}", session_id);
            let expanded = expand_home(&isolated_path).to_string_lossy().into_owned();
            env.insert("CLAUDE_CONFIG_DIR".to_string(), expanded);
        }
    }
    env
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
    /// 全エージェント横断の統合承認キュー。`poll_events` が投入し、
    /// 承認パネル (UI) がこれを描く。詳細は `approvals` モジュール doc。
    pub approvals: approvals::ApprovalQueue,
    next_id: u64,
}

/// 承認要求の分類に使う画面テキストの取得範囲。
/// 承認プロンプトは画面下部にしか出ないので、末尾の数十行で足りる
/// (画面全体の contents() を毎フレーム作らないための上限)。
const APPROVAL_SCAN_ROWS: usize = 40;
/// 同上、1 行あたりの文字数上限。
const APPROVAL_SCAN_COLS: usize = 400;

/// 承認判定用に、セッション画面の末尾テキストを取り出す。
fn approval_scan_text(s: &Session) -> String {
    s.screen_tail_lines(APPROVAL_SCAN_ROWS, APPROVAL_SCAN_COLS)
        .join("\n")
}

/// 承認キューが決めた応答を実際に PTY へ送る。送れたら true。
///
/// 承認側は `press_pet_approve_button` を通す — 画面に合った承認キー
/// (「1. No, exit」が既定選択のプロンプトでは番号キー等) を選び、
/// 送信後に同じプロンプトを応答済みにしてくれるため。
fn apply_reply_action(s: &mut Session, action: approvals::ReplyAction) -> bool {
    let (approve_keys, deny_keys) = approvals::reply_keys();
    match action {
        approvals::ReplyAction::None => false,
        approvals::ReplyAction::Approve => s.press_pet_approve_button(Some(&approve_keys)),
        approvals::ReplyAction::Deny => {
            let ok = s.send_text(&deny_keys);
            if ok {
                s.resolve_attention();
            }
            ok
        }
    }
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
            approvals: approvals::ApprovalQueue::new(),
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
        // **エージェント別の履歴へ 1 行積む。**
        // 過去の会話一覧はベンダー側の保存物 (Claude / Codex / Antigravity)
        // しか読めず、それ以外のエージェントは一覧にすら出なかった。
        // 起動は全部この関数を通るので、ここ 1 箇所で記録すれば漏れない。
        // 失敗しても起動は続ける (履歴が書けないことで作業を止めない)。
        let s = self
            .sessions
            .last()
            .expect("push した直後なので必ずある");
        let _ = crate::history::append(&crate::history::Entry {
            id: s.id,
            agent_bin: spec_for_command(&s.command)
                .map(|sp| sp.bin.to_string())
                .unwrap_or_default(),
            preset_name: s.preset_name.clone(),
            title: s.title.clone(),
            icon: s.icon.clone(),
            command: s.command.clone(),
            cwd: s.cwd.to_string_lossy().into_owned(),
            log_file: s
                .log_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            started: crate::history::now_unix(),
            ended: 0,
            brief: String::new(),
            vendor_id: String::new(),
        });
        self.active = self.sessions.len() - 1;
        self.panel_open = true;
        Ok(())
    }

    /// 保存済みセッション記録 (チャット履歴のフォルダ別保存) からの復元起動。
    ///
    /// `launch` と違い、タイトル・コマンド・ログの書き出し先は呼び出し側
    /// (セッション記録) が決める — 特にログは**前回と同じファイル**へ追記させ、
    /// 再起動をまたいで 1 本の履歴になるようにする。`replay` (前回ログの末尾) を
    /// 先に vt100 へ流し込み、旧スクロールバックが見える状態にしてから
    /// エージェントの新しい出力を受ける。
    pub fn launch_restored(
        &mut self,
        spec: SpawnSpec,
        replay: &[u8],
        ctx: &egui::Context,
    ) -> Result<(), String> {
        let id = self.next_id;
        self.next_id += 1;
        let session = Session::spawn(id, spec, ctx.clone())?;
        session.preload_scrollback(replay);
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
        self.panel_open = true;
        Ok(())
    }

    pub fn restart(&mut self, i: usize, ctx: &egui::Context) -> Result<(), String> {
        let Some(old) = self.sessions.get_mut(i) else {
            return Ok(());
        };
        // ここで old.kill() を呼んではいけない: kill() のスレッドが先に根 (シェル)
        // を落とすと、後段 reap の taskkill /T が木を辿れず孫が生き残り、
        // ConPTY を閉じる drop が reap スレッドごと永久に固まる (remove と同じ罠)。
        // 木→根→drop の正しい順序は reap が一手に引き受ける。
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
        // 代入で古いセッションをその場で drop すると、UI スレッドが ConPTY の
        // 後始末で止まる。取り出して reap へ渡す (crate::terminal::reap を参照)。
        let old = std::mem::replace(&mut self.sessions[i], session);
        crate::terminal::reap(old);
        self.active = i;
        Ok(())
    }

    pub fn remove(&mut self, i: usize) {
        if i >= self.sessions.len() {
            return;
        }
        // 閉じたセッションの承認待ち・重複記録を捨てる。PID と同じく
        // セッション ID は再利用され得るので、残すと別セッションの
        // プロンプトを「見たことがある」と誤判定してしまう。
        if let Some(s) = self.sessions.get(i) {
            self.approvals.forget_session(s.id);
        }
        // 取り除いたセッションは**この場で drop しない**。ConPTY を閉じる
        // Drop は UI スレッドを止め得るので、後始末ごと reap に預ける
        // (crate::terminal::reap の説明を参照)。
        crate::terminal::reap(self.sessions.remove(i));
        // active より左を閉じたら、フォーカス中セッションが左へ1つ詰まるので
        // active も詰める(でないとキーボード/リモート入力が隣のセッションへ流れる)。
        // i == active が最右のときは下のクランプが左隣へ寄せる(従来挙動のまま)。
        if i < self.active {
            self.active -= 1;
        }
        if self.active >= self.sessions.len() && !self.sessions.is_empty() {
            self.active = self.sessions.len() - 1;
        }
    }

    /// 稼働中のセッションを**全部**止める (タブは残す)。戻り値は止めに行った本数。
    ///
    /// - [`Session::kill`] を通すので、**プロセスツリーごと**落ちる
    ///   (`procx::kill_tree` → unix はプロセスグループ、Windows は `taskkill /T`)。
    ///   直接の子だけを撃つと、シェルが `exec` せずに起こした孫がパイプを
    ///   握ったまま残り、読み取りの join が戻らず UI が固まる。
    /// - **終了済みには撃たない**。wait 済みの PID は OS に返却されており、
    ///   無関係なプロセス (グループ) に再利用され得るため
    ///   (`Session::kill` 側にも同じガードがあり、ここは二重の栓)。
    /// - kill は別スレッドへ投げるだけなので、UI スレッドから呼んでよい。
    pub fn stop_all(&mut self) -> usize {
        let mut n = 0;
        for s in self.sessions.iter_mut() {
            if s.running() {
                s.kill();
                n += 1;
            }
        }
        n
    }

    pub fn active_session(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active)
    }

    pub fn running_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.running()).count()
    }

    /// 各セッションの状態変化(承認待ち・自動承認・終了)を検知して返す。毎フレーム呼んで良い。
    /// 自動YES (`pet_auto_yes`) がオンなら、カタログ既知の CLI の承認プロンプトへ
    /// 自動応答する。起動時の承認モード (Ask/bypass) には依存しない。
    ///
    /// `allow_auto_yes` が false のときは自動応答せず、承認待ち (NeedsApproval)
    /// として報告するだけに留める。勝手にYESを送らずユーザーの承認を待つための栓。
    pub fn poll_events(&mut self, allow_auto_yes: bool) -> Vec<SessionEvent> {
        use crate::terminal::Attention;
        let mut events = Vec::new();
        // approvals と sessions を同時に可変で借りるため、フィールドを分解する。
        let Self {
            sessions,
            approvals,
            ..
        } = self;
        for s in sessions.iter_mut() {
            if s.running() {
                // ── ① ポリシー相談は従来の自動YESより先 ──────────────
                // 「常に拒否」ポリシーは全自動YESより強くなければ意味がない。
                // ところが scan_attention(true) は検知と同時に YES を撃つので、
                // 撃たせる前にここで栓をする。拒否ポリシーが 1 件も無い既定
                // 構成では画面テキストの取得すら走らない (追加コスト 0)。
                let mut auto = s.auto_yes_target(allow_auto_yes);
                if auto && approvals.auto_yes_blocked(s.id, s.agent_bin(), || approval_scan_text(s))
                {
                    auto = false;
                }
                match s.scan_attention(auto) {
                    Some(Attention::NeedsApproval) => {
                        // ── ② 統合承認キューへ投入。ポリシーが決着させたら即答する ──
                        let text = approval_scan_text(s);
                        let sig = crate::terminal::prompt_signature(&text);
                        match approvals.intake(s.id, s.agent_bin(), &text, sig) {
                            approvals::Verdict::Decided { reply, note, .. } => {
                                if apply_reply_action(s, reply) {
                                    // 既存の SessionEvent を増やさずに済むよう、
                                    // 無人応答は許可/拒否とも AutoApproved で伝える
                                    // (note に「自動承認/自動拒否」が入る)。
                                    events.push(SessionEvent::AutoApproved(s.title.clone(), note));
                                } else {
                                    events.push(SessionEvent::NeedsApproval(s.title.clone()));
                                }
                            }
                            // Duplicate は「同じプロンプトの再検出」。従来どおり
                            // 承認待ちとして報告する (トーストは app.rs 側で間引く)。
                            _ => events.push(SessionEvent::NeedsApproval(s.title.clone())),
                        }
                    }
                    Some(Attention::AutoReplied(desc)) => {
                        // 従来の全自動YES が撃った分も監査ログへ残す (source: auto_yes)。
                        approvals.log_auto_yes(s.id, s.agent_bin(), &approval_scan_text(s));
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
                let code = crate::lockx::lock_ok(&s.exit_code).unwrap_or(0);
                events.push(SessionEvent::Exited(s.title.clone(), code));
            }
        }
        events
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

// ═══════════════════════════════════════════════════════════════════════════
//  状態ラダー上位 2 段のカタログ (CLAUDE.md 設計原則 #4)
//
//  構造化プロトコル > ベンダー提供フック > 状態ファイル > 画面スクレイプ。
//  上 2 段の**エージェント固有値**(フラグ・イベント名・ツール名・設定ファイルの
//  場所) はすべてここにデータとして持つ。機構は `supervisor::protocol` /
//  `supervisor::hooks` にあり、リテラルを 1 つも持たない。
//
//  **実機で確認できたものだけを書く。** 憶測で 1 行足すと、そのエージェントは
//  「画面より確かな判定」として嘘を配ることになる (最下段より有害)。
// ═══════════════════════════════════════════════════════════════════════════

use crate::supervisor::hooks::HookTarget;
use crate::supervisor::protocol::{EventRule, ProtoState, StreamDialect};

/// Claude Code のツール名 → 状態。名前は実機 `stream-json` の
/// `{"type":"system","subtype":"init","tools":[…]}` から採取した実在の値。
///
/// ここに無いツールは規則側の状態 (= ツール実行 → 実行中) のままにする。
const CLAUDE_TOOLS: &[(&str, ProtoState)] = &[
    ("Edit", ProtoState::Editing),
    ("Write", ProtoState::Editing),
    ("NotebookEdit", ProtoState::Editing),
];

/// 構造化出力を持つと**実機で確認できた**エージェントの方言表。
///
/// ## 確認方法と結果 (2026-08)
/// - `claude` 2.1.226: `claude --help` に
///   `--output-format <format> … "stream-json" (realtime streaming)` があり、
///   `claude -p --output-format stream-json --verbose` を実行して JSONL を採取。
///   観測した `type`: `system`(subtype=`init`/`hook_started`/`hook_response`/
///   `thinking_tokens`) / `assistant` / `rate_limit_event` / `result`(subtype=`success`)。
///   **注意**: `--output-format` は `--print` (非対話) 専用。対話 PTY セッションでは
///   出ないので、この段が効くのはヘッドレス実行のときだけ。対話セッションは
///   フック段 ([`HOOK_TARGETS`]) が受け持つ。
/// - `codex` 0.147.0: `codex exec --help` に `--json  Print events to stdout as JSONL`。
///   `codex exec --json` を実行して JSONL を採取。観測した `type`:
///   `thread.started` / `turn.started` / `item.started` / `item.completed`
///   (`item.type` = `agent_message` / `command_execution`) / `turn.completed`。
/// - `gemini` 0.51.0: `gemini --help` に `-o, --output-format … "stream-json"` は
///   **在る**。しかし実行がアカウント制限 (IneligibleTierError) で通らず、
///   イベントの語彙を 1 件も観測できなかった → **意図的に表へ入れていない**。
///   観測できたら 1 エントリ足すだけで有効になる。
pub static STREAM_DIALECTS: &[StreamDialect] = &[
    StreamDialect {
        bin: "claude",
        args: "--print --verbose --output-format stream-json",
        kind_path: "type",
        rules: &[
            EventRule {
                kind: "system",
                sub_path: "subtype",
                sub_value: "init",
                state: ProtoState::Starting,
                detail_path: "",
            },
            // 失敗を先に見る (result は成功も失敗も同じ種別で来る)。
            EventRule {
                kind: "result",
                sub_path: "is_error",
                sub_value: "true",
                state: ProtoState::Failed,
                detail_path: "",
            },
            EventRule {
                kind: "result",
                sub_path: "subtype",
                sub_value: "success",
                state: ProtoState::Done,
                detail_path: "",
            },
            // content[] の中に tool_use ブロックが在れば「ツールを使っている」。
            EventRule {
                kind: "assistant",
                sub_path: "message.content[].type",
                sub_value: "tool_use",
                state: ProtoState::Running,
                detail_path: "message.content[].name",
            },
            EventRule {
                kind: "assistant",
                sub_path: "",
                sub_value: "",
                state: ProtoState::Thinking,
                detail_path: "",
            },
        ],
        tools: CLAUDE_TOOLS,
        verified: "claude 2.1.226 — --help の choices + `claude -p --output-format stream-json --verbose` の実出力",
    },
    StreamDialect {
        bin: "codex",
        args: "exec --json",
        kind_path: "type",
        rules: &[
            EventRule {
                kind: "thread.started",
                sub_path: "",
                sub_value: "",
                state: ProtoState::Starting,
                detail_path: "",
            },
            EventRule {
                kind: "item.started",
                sub_path: "item.type",
                sub_value: "command_execution",
                state: ProtoState::Running,
                detail_path: "item.command",
            },
            // コマンドが終われば手番はモデルへ戻る。
            EventRule {
                kind: "item.completed",
                sub_path: "item.type",
                sub_value: "command_execution",
                state: ProtoState::Thinking,
                detail_path: "item.command",
            },
            EventRule {
                kind: "item.completed",
                sub_path: "item.type",
                sub_value: "agent_message",
                state: ProtoState::Thinking,
                detail_path: "",
            },
            EventRule {
                kind: "turn.started",
                sub_path: "",
                sub_value: "",
                state: ProtoState::Thinking,
                detail_path: "",
            },
            EventRule {
                kind: "turn.completed",
                sub_path: "",
                sub_value: "",
                state: ProtoState::Idle,
                detail_path: "",
            },
        ],
        // codex の item は `command_execution` しか観測できていない。
        // ファイル編集の item 種別は未観測なので表を空にしておく (憶測を書かない)。
        tools: &[],
        verified: "codex-cli 0.147.0 — `codex exec --help` の --json + `codex exec --json` の実出力",
    },
];

/// 構造化出力の方言。持たないエージェントでは `None`。
pub fn stream_dialect(bin: &str) -> Option<&'static StreamDialect> {
    STREAM_DIALECTS.iter().find(|d| d.bin == bin)
}

/// **このコマンドは構造化出力つきで起動されているか**。
///
/// カタログの `args` に並んだトークンが**すべて**コマンド行に在るときだけ
/// `Some`。素の `claude` を「構造化段が使える」と誤認しないための関門であり、
/// フラグ名のリテラルはここにも一切置かない (表から引くだけ)。
pub fn stream_dialect_for_command(command: &str) -> Option<&'static StreamDialect> {
    let spec = spec_for_command(command)?;
    let d = stream_dialect(spec.bin)?;
    let tokens: Vec<&str> = command.split_whitespace().collect();
    d.args
        .split_whitespace()
        .all(|need| tokens.contains(&need))
        .then_some(d)
}

/// フックを仕掛けられると**実機で確認できた**エージェント。
///
/// ## 確認方法 (2026-08)
/// `claude` 2.1.226 の `--output-format stream-json` に
/// `{"type":"system","subtype":"hook_started","hook_event":"SessionStart",
///   "hook_name":"SessionStart:startup"}` と、続く `hook_response` が流れた
/// = フックが実際に発火している。設定の形と有効なイベント名は実在する
/// `~/.claude/settings.json` から採取した (`hooks.<Event>[].hooks[].command`)。
///
/// 表に入れているのは、その中で**名前から意味が一意に決まるもの**だけ。
/// `Notification` / `Elicitation` などは「何を待っているか」がペイロード次第
/// なので、当てずっぽうで承認待ちに落とさない。
pub static HOOK_TARGETS: &[HookTarget] = &[HookTarget {
    bin: "claude",
    settings_rel: ".claude/settings.json",
    events: &[
        ("SessionStart", ProtoState::Starting, false),
        ("UserPromptSubmit", ProtoState::Thinking, false),
        // ツールを使う直前 = そのツール名で状態を細分できる唯一の点。
        ("PreToolUse", ProtoState::Running, true),
        ("PostToolUse", ProtoState::Thinking, false),
        ("PermissionRequest", ProtoState::Approval, false),
        ("Stop", ProtoState::Idle, false),
        ("SessionEnd", ProtoState::Done, false),
    ],
    tools: CLAUDE_TOOLS,
    verified: "claude 2.1.226 — stream-json に hook_started/hook_response を観測 + 実在の ~/.claude/settings.json の hooks スキーマ",
}];

/// フック設定の対象。持たないエージェントでは `None`。
pub fn hook_target(bin: &str) -> Option<&'static HookTarget> {
    HOOK_TARGETS.iter().find(|t| t.bin == bin)
}

/// フックイベント名 → (状態, ツール名で細分してよいか)。カタログに無ければ `None`。
pub fn hook_event_state(bin: &str, event: &str) -> Option<(ProtoState, bool)> {
    hook_target(bin)?
        .events
        .iter()
        .find(|(e, _, _)| *e == event)
        .map(|(_, s, refine)| (*s, *refine))
}

/// ツール名 → 状態 (フック段の細分)。表に無ければ `None`。
pub fn hook_tool_state(bin: &str, tool: &str) -> Option<ProtoState> {
    if tool.is_empty() {
        return None;
    }
    hook_target(bin)?
        .tools
        .iter()
        .find(|(n, _)| *n == tool)
        .map(|(_, s)| *s)
}

// ═══════════════════════════════════════════════════════════════════════════
//  非対話 (ヘッドレス) の一発実行カタログ — セッションの自動命名に使う
//
//  「そのエージェント自身の CLI に、自分の作業へ短い題名を付けさせる」ための表。
//  **コマンド名もフラグもここにしか無い** (機構側は `naming` モジュールにあり、
//  リテラルを 1 つも持たない)。`STREAM_DIALECTS` / `HOOK_TARGETS` と同じ流儀。
//
//  supervisor の診断を外部 CLI へ投げない方針とは無関係 — あちらは「見張りの
//  判断」、こちらは「本人に自分の作業を名付けさせる」もの。**別のエージェントへ
//  投げてはいけない**という一線だけは共通で、`title_generator_for_command` が
//  そのセッション自身の bin しか引けないようにして構造で守っている。
// ═══════════════════════════════════════════════════════════════════════════

/// 非対話の一発実行 (ヘッドレス) ができると**実機で確認できた**エージェント。
///
/// プロンプトは常に**最後の 1 引数**として渡す。`args` に並べたトークンを
/// bin の直後へ置くだけで済むよう、そう揃えてある
/// (`claude -p <prompt>` / `codex exec … <prompt>` / `agy -p <prompt>`)。
pub struct TitleGen {
    /// 実行ファイル名 ([`AgentSpec::bin`] と同じ値)。
    pub bin: &'static str,
    /// bin の直後に置く引数列 (空白区切り)。プロンプトはこの後ろへ 1 引数で足す。
    pub args: &'static str,
    /// 何をどう確かめたか。**実機で撃った証拠だけ**を書くこと。
    pub verified: &'static str,
}

/// セッションの自動命名を任せられる CLI の表。
///
/// ## 確認方法と結果 (2026-08、実機 macOS)
/// - `claude` 2.1.226: `claude --help` に `-p, --print  Print response and exit`。
///   `claude -p "…" --model haiku` を実行し、**標準出力に本文だけ**が出ることを確認。
/// - `codex` 0.147.0: `codex exec --help` に `Run Codex non-interactively` と
///   `[PROMPT]`。`codex exec --skip-git-repo-check --color never -s read-only "…"`
///   を実行し、**進行ログは stderr・最終メッセージだけが stdout** に出ることを確認。
///   `--skip-git-repo-check` はリポジトリ外 (命名は一時ディレクトリで走らせる) の
///   ため、`-s read-only` は命名のついでにファイルを触らせないための栓。
/// - `agy` (Antigravity): `agy --help` に
///   `--print  Run a single prompt non-interactively and print the response`。
///   `agy -p "…"` を実行し、標準出力に本文だけが出ることを確認。
///
/// ## **意図的に入れていない** CLI (この機では実行を確認できなかった)
/// - `gemini` 0.51.0: `--help` に `-p, --prompt … non-interactive (headless) mode`
///   は在るが、実行が `IneligibleTierError` + 「trusted directory ではない」で
///   通らず、出力を 1 度も観測できなかった (`STREAM_DIALECTS` の gemini と同じ理由)。
/// - `cursor-agent`: `--help` に `-p, --print` は在るが、未サインインで
///   「Press any key to sign in...」の画面しか返らなかった。
/// - `droid`: `--help` に `exec … Run non-interactively` は在るが、
///   `Authentication failed` で本文を観測できなかった。
///
/// いずれも**観測できたら 1 エントリ足すだけ**で有効になる。憶測で先に書くと、
/// 「対応と宣言したのに毎ターン無駄なプロセスを起こす」だけの表になる。
pub static TITLE_GENERATORS: &[TitleGen] = &[
    TitleGen {
        bin: "claude",
        args: "-p",
        verified: "claude 2.1.226 — --help の `-p, --print` + `claude -p …` の実出力 (stdout に本文のみ)",
    },
    TitleGen {
        bin: "codex",
        args: "exec --skip-git-repo-check --color never -s read-only",
        verified: "codex-cli 0.147.0 — `codex exec --help` の [PROMPT] + 実行して stdout が最終メッセージのみと確認",
    },
    TitleGen {
        bin: "agy",
        args: "-p",
        verified: "agy — --help の `--print` + `agy -p …` の実出力 (stdout に本文のみ)",
    },
];

/// 非対話の一発実行ができる CLI か。できなければ `None`。
pub fn title_generator(bin: &str) -> Option<&'static TitleGen> {
    TITLE_GENERATORS.iter().find(|g| g.bin == bin)
}

/// **このセッションを起動したコマンド自身**の命名器。
///
/// 引くのはコマンド行の先頭トークンから解決した bin だけ — 別のエージェントへ
/// 投げる経路をそもそも作らない (方針「別のエージェントへ投げない」の構造的な栓)。
pub fn title_generator_for_command(command: &str) -> Option<&'static TitleGen> {
    title_generator(spec_for_command(command)?.bin)
}

/// セッションの自動命名 (cmux 由来) — ターン境界の検出 / 題名の検疫 / 実行。
///
/// **既定はオフ**。有効なときだけ、ターンが終わった瞬間に 1 回だけ走る。
/// アイドル時は 1 プロセスも起こさない (設計原則 3)。
pub mod naming {
    use std::collections::HashMap;
    use std::io::Read;
    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::mpsc::{channel, Receiver, Sender};
    use std::time::{Duration, Instant};

    /// 題名として受け入れる最大文字数 (Unicode スカラー値の数)。
    /// サイドバーの 1 行に収まる長さ。超えた分は切り詰める。
    pub const MAX_TITLE_CHARS: usize = 32;
    /// これより長い 1 行は「題名」ではなく地の文なので**捨てる**。
    pub const REJECT_LINE_CHARS: usize = 200;
    /// 命名の材料として送るユーザー指示の最大文字数。
    pub const MAX_BRIEF_CHARS: usize = 300;
    /// 命名プロセスの上限時間。超えたらプロセスツリーごと畳んで諦める。
    pub const TIMEOUT: Duration = Duration::from_secs(45);
    /// 出力の保持上限 (超えた分は読み捨てる。読むのはやめない)。
    const MAX_OUTPUT_BYTES: usize = 64 * 1024;
    /// 同じ bin で連続して失敗したら、このセッション中はもう起こさない。
    pub const GIVE_UP_AFTER: u32 = 3;

    /// ターン境界の検出器。
    ///
    /// 「出力が動いた → 静かになった」を **1 回だけ** true にする。時計は
    /// 引数で受け取るので、テストから実時間なしで回せる。状態は 1 セッション
    /// あたり 16 バイト程度で、動いていないセッションでは何も起こさない。
    ///
    /// これは状態ラダーの最下段 (画面) に依る判定だが、使い道が
    /// 「題名を付け直す時点」だけなので、外しても害が無い
    /// (誤検知 = 題名が 1 回多く付く / 取りこぼし = 従来名のまま)。
    /// エージェントの**状態**の判定には使わないこと。
    #[derive(Default)]
    pub struct TurnWatcher {
        per: HashMap<u64, TurnState>,
    }

    #[derive(Clone, Copy)]
    struct TurnState {
        last_advance_ms: u64,
        armed: bool,
    }

    impl TurnWatcher {
        /// `quiet_ms`: 出力が止まってからターン終了と見なすまでの静穏時間。
        pub fn observe(&mut self, id: u64, advanced: bool, now_ms: u64, quiet_ms: u64) -> bool {
            let e = self.per.entry(id).or_insert(TurnState {
                last_advance_ms: now_ms,
                armed: false,
            });
            if advanced {
                e.last_advance_ms = now_ms;
                e.armed = true;
                return false;
            }
            if e.armed && now_ms.saturating_sub(e.last_advance_ms) >= quiet_ms {
                e.armed = false;
                return true;
            }
            false
        }

        /// セッションが消えたら忘れる (ID は再利用され得るので残さない)。
        pub fn forget(&mut self, id: u64) {
            self.per.remove(&id);
        }

        #[cfg(test)]
        pub fn tracked(&self) -> usize {
            self.per.len()
        }
    }

    /// 命名器へ送る「材料」を最小化する。
    ///
    /// **送るのはユーザー自身が打った指示文の冒頭だけ** — エージェントの出力も、
    /// 画面の中身も、ファイルの内容も 1 バイトも入れない。制御文字と改行は
    /// 潰し、長さも切り詰める。
    pub fn brief(user_prompt: &str) -> String {
        let mut out = String::new();
        let mut space = false;
        for c in user_prompt.chars() {
            if c.is_control() || c.is_whitespace() {
                space = !out.is_empty();
                continue;
            }
            if space {
                out.push(' ');
                space = false;
            }
            if out.chars().count() >= MAX_BRIEF_CHARS {
                break;
            }
            out.push(c);
        }
        out
    }

    /// 題名を作らせるプロンプト。材料は [`brief`] を通したものだけ。
    pub fn naming_prompt(brief: &str) -> String {
        format!(
            "Give a title for this task in 2-5 words. \
             Reply with the title only: no quotes, no punctuation at the end, \
             no explanation, one line. Use the same language as the task.\n\
             Task: {brief}"
        )
    }

    /// 生成結果を**そのまま信用しない**ための検疫。
    ///
    /// 受け取るのは他所のプロセスの標準出力なので、改行だらけ・空・制御文字混じり・
    /// 何 KB もある、が普通に起こる。通すのは「1 行の短い題名」だけ。
    /// CJK と絵文字は文字数で数えるので途中で壊れない (バイト境界で切らない)。
    pub fn sanitize_title(raw: &str) -> Option<String> {
        // ① 最初の「中身のある行」を取る。前置きの空行や飾り線は捨てる。
        let line = raw
            .lines()
            .map(str::trim)
            .find(|l| l.chars().any(|c| !c.is_control() && !c.is_whitespace()))?;
        // ② 地の文の長さなら題名ではない。捨てる (切り詰めると意味が壊れる)。
        if line.chars().count() > REJECT_LINE_CHARS {
            return None;
        }
        // ③ 制御文字を落とし、連続空白を 1 つに畳む。
        let mut s = String::new();
        let mut space = false;
        for c in line.chars() {
            if c.is_control() {
                continue;
            }
            if c.is_whitespace() {
                space = !s.is_empty();
                continue;
            }
            if space {
                s.push(' ');
                space = false;
            }
            s.push(c);
        }
        // ④ 見出し記号・引用符・箇条書きの飾りを剥がす。
        let s = s
            .trim_matches(|c: char| {
                matches!(
                    c,
                    '"' | '\''
                        | '`'
                        | '#'
                        | '*'
                        | '-'
                        | '_'
                        | '“'
                        | '”'
                        | '「'
                        | '」'
                        | '『'
                        | '』'
                        | '【'
                        | '】'
                        | ':'
                        | '：'
                        | '.'
                        | '。'
                        | ' '
                )
            })
            .to_string();
        if s.is_empty() {
            return None;
        }
        // ⑤ 長すぎるものは切り詰める。**文字単位**で切り、結合文字や
        //    ZWJ で終わらないところまで戻す (絵文字の連結を割らない)。
        let mut t: String = s.chars().take(MAX_TITLE_CHARS).collect();
        if t.chars().count() < s.chars().count() {
            while t
                .chars()
                .next_back()
                .is_some_and(|c| c == '\u{200d}' || is_modifier_char(c))
            {
                t.pop();
            }
            t = t.trim_end().to_string();
            if t.is_empty() {
                return None;
            }
            t.push('…');
        }
        Some(t)
    }

    /// 単独では意味を持たない結合系の文字 (異体字セレクタ・肌色・結合記号)。
    fn is_modifier_char(c: char) -> bool {
        matches!(c as u32,
            0x0300..=0x036F      // 結合分音記号
            | 0xFE00..=0xFE0F    // 異体字セレクタ
            | 0x1F3FB..=0x1F3FF  // 肌の色
            | 0xE0100..=0xE01EF) // 異体字セレクタ補助
    }

    /// 1 件の命名結果。`title` が None なら**黙って従来名のまま**にする。
    pub struct Named {
        pub session_id: u64,
        pub bin: &'static str,
        pub title: Option<String>,
    }

    /// 命名の実行係。要求ごとに 1 スレッドを起こし、結果をチャネルで返す。
    ///
    /// UI スレッドは `poll()` の `try_recv` を舐めるだけなので、走っていない
    /// ときのコストは 0 (設計原則 3)。
    pub struct Namer {
        tx: Sender<Named>,
        rx: Receiver<Named>,
        inflight: HashMap<u64, ()>,
        failures: HashMap<&'static str, u32>,
    }

    impl Default for Namer {
        fn default() -> Self {
            let (tx, rx) = channel();
            Self {
                tx,
                rx,
                inflight: HashMap::new(),
                failures: HashMap::new(),
            }
        }
    }

    impl Namer {
        /// このセッションの命名がまだ走っているか。
        pub fn busy(&self, session_id: u64) -> bool {
            self.inflight.contains_key(&session_id)
        }

        /// この CLI は連続失敗で諦め済みか。
        pub fn given_up(&self, bin: &str) -> bool {
            self.failures.get(bin).is_some_and(|n| *n >= GIVE_UP_AFTER)
        }

        /// 命名を 1 件依頼する。走らせない条件に当たったら false。
        pub fn request(
            &mut self,
            session_id: u64,
            gen: &'static super::TitleGen,
            brief: String,
            ctx: egui::Context,
        ) -> bool {
            if self.busy(session_id) || self.given_up(gen.bin) || brief.is_empty() {
                return false;
            }
            self.inflight.insert(session_id, ());
            let tx = self.tx.clone();
            let prompt = naming_prompt(&brief);
            let ok = std::thread::Builder::new()
                .name("zv-name".into())
                .spawn(move || {
                    let title = run_title(gen, &prompt)
                        .ok()
                        .and_then(|s| sanitize_title(&s));
                    let _ = tx.send(Named {
                        session_id,
                        bin: gen.bin,
                        title,
                    });
                    // 結果が届いたことを UI へ知らせる (1 フレームだけ起こす)。
                    ctx.request_repaint();
                })
                .is_ok();
            if !ok {
                self.inflight.remove(&session_id);
            }
            ok
        }

        /// 届いた結果を取り出す。毎フレーム呼んでよい (待たない)。
        pub fn poll(&mut self) -> Vec<Named> {
            let mut out = Vec::new();
            while let Ok(n) = self.rx.try_recv() {
                self.inflight.remove(&n.session_id);
                match n.title.is_some() {
                    true => {
                        self.failures.remove(n.bin);
                    }
                    false => *self.failures.entry(n.bin).or_insert(0) += 1,
                }
                out.push(n);
            }
            out
        }

        /// セッションが消えたら在庫も忘れる。
        pub fn forget(&mut self, session_id: u64) {
            self.inflight.remove(&session_id);
        }

        #[cfg(test)]
        pub fn note_failure(&mut self, bin: &'static str) {
            *self.failures.entry(bin).or_insert(0) += 1;
        }
    }

    /// 命名を走らせる作業ディレクトリ。
    ///
    /// **プロジェクトフォルダでは走らせない** — CLI がそこの `CLAUDE.md` や
    /// リポジトリの中身を読み込んでしまうため。OS の一時ディレクトリ配下に
    /// 空のフォルダを 1 つだけ作って、そこを cwd にする
    /// (パスの直書きをしないので Windows / Linux / macOS のどれでも通る)。
    pub fn scratch_dir() -> PathBuf {
        let dir = std::env::temp_dir().join("zaivern-naming");
        let _ = std::fs::create_dir_all(&dir);
        dir
    }

    /// 子プロセスを起こしてプロンプトを渡し、標準出力を返す。
    ///
    /// `diagnostician::run` と同じ作法: stdin は null、stdout/stderr は
    /// 読み取りスレッドで読み切り、期限を過ぎたら**プロセスツリーごと**畳む。
    fn run_title(gen: &'static super::TitleGen, prompt: &str) -> Result<String, String> {
        let mut cmd = crate::procx::hidden_command(gen.bin);
        for a in gen.args.split_whitespace() {
            cmd.arg(a);
        }
        cmd.arg(prompt);
        cmd.current_dir(scratch_dir());
        cmd.env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // 子を独立したプロセスグループへ。こうしないと kill_tree が
            // 孫 (CLI が起こす node / ラッパー) を取り逃がす。
            unsafe {
                cmd.pre_exec(|| {
                    libc::setsid();
                    Ok(())
                });
            }
        }
        let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", gen.bin))?;
        let out_rx = child.stdout.take().map(spawn_capped_reader);
        let err_rx = child.stderr.take().map(spawn_capped_reader);

        let deadline = Instant::now() + TIMEOUT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        // **まだ生きている**ことを try_wait で確かめた上で撃つ。
                        // wait 済みの PID へ撃つと無関係なプロセスを巻き添えにする。
                        crate::procx::kill_tree(child.id());
                        let _ = child.wait();
                        return Err(format!("{}: 命名が時間内に終わらなかった", gen.bin));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("{}: {e}", gen.bin)),
            }
        };
        // kill 済みでもパイプが閉じるので join は必ず戻る。
        let stdout = out_rx.and_then(|h| h.join().ok()).unwrap_or_default();
        let _ = err_rx.and_then(|h| h.join().ok());
        if !status.success() {
            return Err(format!("{}: code={:?}", gen.bin, status.code()));
        }
        Ok(stdout)
    }

    /// 上限付きで読み切るリーダースレッド。**保持は有界、読み取りは EOF まで**
    /// (途中でやめるとパイプが詰まって相手が write でブロックする)。
    fn spawn_capped_reader<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            let mut buf: Vec<u8> = Vec::with_capacity(4096);
            let mut chunk = [0u8; 4096];
            loop {
                match r.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if buf.len() < MAX_OUTPUT_BYTES {
                            let room = MAX_OUTPUT_BYTES - buf.len();
                            buf.extend_from_slice(&chunk[..n.min(room)]);
                        }
                    }
                    Err(_) => break,
                }
            }
            String::from_utf8_lossy(&buf).into_owned()
        })
    }
}

/// 状態ラダー上位 2 段のカタログ整合 (CLAUDE.md 原則 #4 の番人)。
///
/// **「構造化出力を持つ」と宣言したのに引かせる表が無い**状態は、画面推定より
/// 強い段位で嘘を配ることになる。ここで落とす。
#[cfg(test)]
mod ladder_catalog_tests {
    use super::*;

    #[test]
    fn 構造化出力を宣言したプリセットはフラグと確認方法を持つ() {
        assert!(
            !STREAM_DIALECTS.is_empty(),
            "上位段が空では原則 #4 を満たさない"
        );
        for d in STREAM_DIALECTS {
            assert!(
                spec_for_bin(d.bin).is_some(),
                "{}: カタログに居ないエージェントの方言",
                d.bin
            );
            assert!(
                !d.args.is_empty(),
                "{}: 構造化出力を有効にする引数が空",
                d.bin
            );
            assert!(!d.kind_path.is_empty(), "{}: 種別フィールドが空", d.bin);
            assert!(!d.rules.is_empty(), "{}: 規則表が空 (引けない)", d.bin);
            assert!(
                !d.verified.is_empty(),
                "{}: 実機での確認方法が空 — 憶測で書かれた疑いがある",
                d.bin
            );
            for r in d.rules {
                assert!(
                    !(r.kind.is_empty() && r.sub_path.is_empty()),
                    "{}: 何にでも当たる規則は書かない",
                    d.bin
                );
                assert!(
                    !(r.sub_path.is_empty() && !r.sub_value.is_empty()),
                    "{}: 絞り込み先の無い値が指定されている",
                    d.bin
                );
            }
            // 方言の引数を足したコマンドは、必ずその方言として引けること。
            let cmd = format!("{} {}", d.bin, d.args);
            assert_eq!(
                stream_dialect_for_command(&cmd).map(|x| x.bin),
                Some(d.bin),
                "{}: 宣言した引数で構造化段に入れない",
                d.bin
            );
        }
    }

    #[test]
    fn フック対象は設定ファイルの場所とイベントを持つ() {
        for t in HOOK_TARGETS {
            assert!(
                spec_for_bin(t.bin).is_some(),
                "{}: カタログに居ないエージェント",
                t.bin
            );
            assert!(
                !t.settings_rel.is_empty(),
                "{}: 設定ファイルの場所が空",
                t.bin
            );
            assert!(
                !t.settings_rel.contains('\\'),
                "{}: 相対パスは / で書く (OS ごとの解決は Path::join に任せる)",
                t.bin
            );
            assert!(!t.events.is_empty(), "{}: 仕掛けるイベントが無い", t.bin);
            assert!(
                !t.verified.is_empty(),
                "{}: 実機での確認方法が空 — 憶測で書かれた疑いがある",
                t.bin
            );
            for (ev, _, _) in t.events {
                assert!(!ev.is_empty(), "{}: 空のイベント名", t.bin);
                assert!(
                    hook_event_state(t.bin, ev).is_some(),
                    "{}: {ev} を引けない",
                    t.bin
                );
            }
            // ツール名で細分してよいイベントが 1 つも無いのに表だけ在る、は無駄。
            if !t.tools.is_empty() {
                assert!(
                    t.events.iter().any(|(_, _, refine)| *refine),
                    "{}: ツール表が使われない",
                    t.bin
                );
            }
        }
    }

    #[test]
    fn 未知のエージェントは上位段を名乗れない() {
        assert!(stream_dialect("そんなCLIは無い").is_none());
        assert!(hook_target("そんなCLIは無い").is_none());
        assert!(hook_event_state("claude", "そんなイベントは無い").is_none());
        assert!(hook_tool_state("claude", "").is_none());
    }

    /// **自動命名に対応と宣言したプリセットは、コマンドとフラグを持つ。**
    ///
    /// 「対応」と書いてあるのに引数が空 / カタログに居ない bin、は毎ターン
    /// 無駄なプロセスを起こすだけになる。
    #[test]
    fn 自動命名に対応と宣言したプリセットはコマンドとフラグを持つ() {
        assert!(!TITLE_GENERATORS.is_empty(), "命名器の表が空");
        let mut seen: Vec<&str> = Vec::new();
        for g in TITLE_GENERATORS {
            assert!(
                !seen.contains(&g.bin),
                "{}: 命名器の表が重複している",
                g.bin
            );
            seen.push(g.bin);
            let spec = spec_for_bin(g.bin)
                .unwrap_or_else(|| panic!("{}: カタログに居ないエージェント", g.bin));
            assert_eq!(spec.bin, g.bin, "{}: 別名で登録されている", g.bin);
            assert!(!g.args.is_empty(), "{}: 非対話実行の引数が空", g.bin);
            assert!(
                !g.verified.is_empty(),
                "{}: 実機での確認方法が空 — 憶測で書かれた疑いがある",
                g.bin
            );
            // カタログの headless 指定と食い違っていないこと
            // (`-p` と `codex exec` はどちらも headless 欄に在る形)。
            let head = g.args.split_whitespace().next().unwrap_or("");
            assert!(
                spec.headless.split_whitespace().any(|t| t == head),
                "{}: headless 欄 ({}) と命名器の引数 ({head}) が食い違う",
                g.bin,
                spec.headless
            );
            // 素のコマンドから、そのエージェント自身の命名器が引けること。
            assert_eq!(
                title_generator_for_command(g.bin).map(|x| x.bin),
                Some(g.bin),
                "{}: 自分自身の命名器を引けない",
                g.bin
            );
        }
    }

    #[test]
    fn 命名器を持たないcliは引けない() {
        assert!(title_generator("そんなCLIは無い").is_none());
        assert!(title_generator_for_command("bash -lc ls").is_none());
        // 実行を観測できていない CLI は**意図的に**表へ入れていない。
        for bin in ["gemini", "cursor-agent", "droid"] {
            assert!(
                title_generator(bin).is_none(),
                "{bin}: 実行を確認できていないのに命名器として宣言されている"
            );
        }
    }
}

/// セッション自動命名 (ターン境界の検出 / 題名の検疫)。
#[cfg(test)]
mod naming_tests {
    use super::naming::*;

    // ── ターン境界 ───────────────────────────────────────────────
    #[test]
    fn ターン終了は出力が止まったとき一度だけ立つ() {
        let mut w = TurnWatcher::default();
        // 出力が動いている間は立たない
        assert!(!w.observe(1, true, 0, 1500));
        assert!(!w.observe(1, true, 500, 1500));
        // 静穏時間に満たないうちも立たない
        assert!(!w.observe(1, false, 1000, 1500));
        // 静穏時間を超えて 1 回だけ
        assert!(w.observe(1, false, 2100, 1500));
        assert!(!w.observe(1, false, 9000, 1500));
        assert!(!w.observe(1, false, 99000, 1500));
        // 次のターンが始まって終われば、また 1 回だけ
        assert!(!w.observe(1, true, 100_000, 1500));
        assert!(w.observe(1, false, 102_000, 1500));
    }

    #[test]
    fn 一度も出力していないセッションではターンが終わらない() {
        let mut w = TurnWatcher::default();
        for t in [0u64, 5_000, 50_000, 500_000] {
            assert!(!w.observe(7, false, t, 1500), "t={t} で誤検知");
        }
    }

    #[test]
    fn セッションを忘れると追跡もやめる() {
        let mut w = TurnWatcher::default();
        w.observe(1, true, 0, 1500);
        w.observe(2, true, 0, 1500);
        assert_eq!(w.tracked(), 2);
        w.forget(1);
        assert_eq!(w.tracked(), 1);
    }

    // ── 題名の検疫 ───────────────────────────────────────────────
    #[test]
    fn まともな題名はそのまま通る() {
        assert_eq!(
            sanitize_title("Fix login redirect"),
            Some("Fix login redirect".into())
        );
        assert_eq!(
            sanitize_title("  Fix login redirect \n"),
            Some("Fix login redirect".into())
        );
    }

    #[test]
    fn 空と空白だけは捨てる() {
        assert_eq!(sanitize_title(""), None);
        assert_eq!(sanitize_title("   "), None);
        assert_eq!(sanitize_title("\n\n\t \n"), None);
        assert_eq!(sanitize_title("\"\""), None);
        assert_eq!(sanitize_title("---"), None);
    }

    #[test]
    fn 改行入りは最初の中身のある行だけを使う() {
        assert_eq!(
            sanitize_title("\n\nRefactor parser\nand then some explanation\nmore"),
            Some("Refactor parser".into())
        );
    }

    #[test]
    fn 制御文字は落とす() {
        let raw = "Fix\u{7} pars\u{1b}er\u{0}\nignored";
        let t = sanitize_title(raw).expect("題名が取れる");
        assert_eq!(t, "Fix parser");
        assert!(
            !t.chars().any(char::is_control),
            "制御文字が残っている: {t:?}"
        );
    }

    #[test]
    fn 極端に長い一行は題名ではないので捨てる() {
        let long = "a".repeat(REJECT_LINE_CHARS + 1);
        assert_eq!(sanitize_title(&long), None);
    }

    #[test]
    fn 長すぎる題名は切り詰める() {
        let src = "abcdefghij".repeat(6); // 60 文字
        let t = sanitize_title(&src).expect("切り詰めて通る");
        assert!(t.chars().count() <= MAX_TITLE_CHARS + 1, "{t:?}");
        assert!(t.ends_with('…'), "切り詰めた印が無い: {t:?}");
    }

    #[test]
    fn cjkと絵文字は文字単位で扱う() {
        // CJK: バイト境界で切ると壊れる長さ
        let jp = "認証まわりのリファクタリングと動作確認".repeat(3);
        let t = sanitize_title(&jp).expect("CJK でも題名になる");
        assert!(t.chars().count() <= MAX_TITLE_CHARS + 1, "{t:?}");
        assert!(t.is_char_boundary(t.len()));
        // 絵文字 (肌色・ZWJ・異体字セレクタ) の途中で終わらない
        let emoji = "👨‍👩‍👧‍👦🎉👍🏽".repeat(10);
        let t = sanitize_title(&emoji).expect("絵文字でも題名になる");
        assert!(
            !t.trim_end_matches('…').ends_with('\u{200d}'),
            "ZWJ で終わっている: {t:?}"
        );
        // 短い絵文字混じりはそのまま通る
        assert_eq!(
            sanitize_title("🎉 リリース準備"),
            Some("🎉 リリース準備".into())
        );
    }

    #[test]
    fn 飾りと引用符は剥がす() {
        assert_eq!(sanitize_title("\"Fix login\""), Some("Fix login".into()));
        assert_eq!(sanitize_title("**Fix login**"), Some("Fix login".into()));
        assert_eq!(sanitize_title("# Fix login"), Some("Fix login".into()));
        assert_eq!(
            sanitize_title("「ログイン修正」"),
            Some("ログイン修正".into())
        );
        assert_eq!(sanitize_title("- Fix login."), Some("Fix login".into()));
    }

    // ── 送る材料 ─────────────────────────────────────────────────
    #[test]
    fn 送る材料はユーザーの指示だけで長さも切り詰める() {
        let b = brief("  ログイン\nの\tリダイレクトを直して  ");
        assert_eq!(b, "ログイン の リダイレクトを直して");
        assert!(!b.chars().any(char::is_control));
        let long = brief(&"あ".repeat(MAX_BRIEF_CHARS * 3));
        assert_eq!(long.chars().count(), MAX_BRIEF_CHARS);
    }

    #[test]
    fn 命名プロンプトは材料以外を含まない() {
        let p = naming_prompt("ログイン修正");
        assert!(p.contains("ログイン修正"));
        assert_eq!(p.lines().count(), 2, "1 行の指示 + Task 行だけ: {p:?}");
    }

    // ── 実行係 ───────────────────────────────────────────────────
    #[test]
    fn 材料が空なら一度も起動しない() {
        let mut n = Namer::default();
        let g = super::title_generator("claude").expect("claude は命名器を持つ");
        let ctx = egui::Context::default();
        assert!(!n.request(1, g, String::new(), ctx));
    }

    #[test]
    fn 連続失敗したcliは諦める() {
        let mut n = Namer::default();
        assert!(!n.given_up("claude"));
        for _ in 0..GIVE_UP_AFTER {
            n.note_failure("claude");
        }
        assert!(n.given_up("claude"));
        let g = super::title_generator("claude").expect("claude は命名器を持つ");
        let ctx = egui::Context::default();
        assert!(
            !n.request(1, g, "テスト".into(), ctx),
            "諦めた後に起動している"
        );
    }

    #[test]
    fn 命名は一時ディレクトリで走らせる() {
        // プロジェクトフォルダを cwd にすると CLI がそこの内容を読み込む。
        let d = scratch_dir();
        assert!(d.starts_with(std::env::temp_dir()), "{d:?}");
        assert!(d.is_dir(), "作業ディレクトリを用意できていない: {d:?}");
    }
}

#[cfg(test)]
mod tests {
    use super::{
        apply_approval, apply_resume, apply_resume_id, env_enables_auto, is_bypass_two_token,
        isolated_env_for_session, merged_env, Approval, SessionStore, AGENT_ALIASES, LAUNCH_ARGS,
        SESSION_STORES,
    };
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
        // カタログ外のコマンドは Auto でも Ask でも一切書き換えない
        assert_eq!(
            apply_approval(
                "some-unknown-cli --dangerously-skip-permissions",
                Approval::Auto
            ),
            "some-unknown-cli --dangerously-skip-permissions"
        );
        assert_eq!(
            apply_approval(
                "some-unknown-cli --dangerously-skip-permissions",
                Approval::Ask
            ),
            "some-unknown-cli --dangerously-skip-permissions"
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

    // ── apply_resume (セッション復元時の会話再開) ──────────────────

    #[test]
    fn resume_claude_appends_continue_flag() {
        let spec = spec_for_bin("claude").expect("claude はカタログにある");
        assert_eq!(apply_resume("claude", spec), "claude --continue");
        // 引数付きでも末尾に足すだけでよい
        assert_eq!(
            apply_resume("claude --dangerously-skip-permissions", spec),
            "claude --dangerously-skip-permissions --continue"
        );
    }

    #[test]
    fn resume_codex_inserts_subcommand_after_bin() {
        let spec = spec_for_bin("codex").expect("codex はカタログにある");
        assert_eq!(apply_resume("codex", spec), "codex resume --last");
        // サブコマンドは bin の直後 — 末尾に付けると resume の引数にならない
        assert_eq!(
            apply_resume("codex --dangerously-bypass-approvals-and-sandbox", spec),
            "codex resume --last --dangerously-bypass-approvals-and-sandbox"
        );
    }

    #[test]
    fn resume_unsupported_cli_returns_command_unchanged() {
        // 再開機能を確認できていない CLI (resume_flag = "") は素のまま
        for bin in ["goose", "qwen", "aider"] {
            let spec = spec_for_bin(bin).expect("カタログにある");
            assert_eq!(spec.resume_flag, "", "{bin} は再開未確認のはず");
            assert_eq!(apply_resume(bin, spec), bin);
        }
    }

    #[test]
    fn resume_does_not_double_add() {
        let claude = spec_for_bin("claude").unwrap();
        assert_eq!(
            apply_resume("claude --continue", claude),
            "claude --continue"
        );
        let codex = spec_for_bin("codex").unwrap();
        assert_eq!(
            apply_resume("codex resume --last", codex),
            "codex resume --last"
        );
    }

    // ── apply_resume_id (ID 指定でこの過去セッションを開く) ─────────

    #[test]
    fn resume_id_table() {
        // (bin, コマンド, id, 期待値)
        let table: &[(&str, &str, &str, &str)] = &[
            // フラグ型: 末尾に `--resume <id>`
            ("claude", "claude", "abc-123", "claude --resume abc-123"),
            (
                "claude",
                "claude --dangerously-skip-permissions",
                "abc-123",
                "claude --dangerously-skip-permissions --resume abc-123",
            ),
            // サブコマンド型: bin の直後に `resume <id>`
            ("codex", "codex", "9f-77", "codex resume 9f-77"),
            (
                "codex",
                "codex --dangerously-bypass-approvals-and-sandbox",
                "9f-77",
                "codex resume 9f-77 --dangerously-bypass-approvals-and-sandbox",
            ),
            // パス付き実行ファイルでもカタログは引ける (bin は basename 一致)
            (
                "claude",
                "/usr/local/bin/claude --model opus",
                "id1",
                "/usr/local/bin/claude --model opus --resume id1",
            ),
            // 空 ID / 空白だけの ID は何もしない
            ("claude", "claude", "", "claude"),
            ("claude", "claude", "   ", "claude"),
            // 危険な文字を含む ID は付けない (シェルへ素で置くため)
            ("claude", "claude", "a b", "claude"),
            ("claude", "claude", "x;rm -rf /", "claude"),
            ("codex", "codex", "$(id)", "codex"),
        ];
        for (bin, cmd, id, want) in table {
            let spec = spec_for_bin(bin).expect("カタログにある");
            assert_eq!(
                apply_resume_id(cmd, spec, id),
                *want,
                "{bin} / {cmd} / {id}"
            );
        }
    }

    #[test]
    fn resume_id_unsupported_cli_returns_command_unchanged() {
        for bin in ["goose", "qwen", "aider", "grok"] {
            let spec = spec_for_bin(bin).expect("カタログにある");
            assert_eq!(
                spec.resume_id_flag(),
                "",
                "{bin} は ID 指定再開 未対応のはず"
            );
            assert_eq!(spec.session_store(), SessionStore::None);
            assert_eq!(apply_resume_id(bin, spec, "abc"), bin);
        }
    }

    #[test]
    fn resume_id_does_not_double_add() {
        let claude = spec_for_bin("claude").unwrap();
        // 既に ID 指定がある
        assert_eq!(
            apply_resume_id("claude --resume old", claude, "new"),
            "claude --resume old"
        );
        // 直前再開 (--continue) と併用させない
        assert_eq!(
            apply_resume_id("claude --continue", claude, "new"),
            "claude --continue"
        );
        let codex = spec_for_bin("codex").unwrap();
        assert_eq!(
            apply_resume_id("codex resume --last", codex, "new"),
            "codex resume --last"
        );
    }

    #[test]
    fn session_store_catalog_matches_resume_id_support() {
        assert_eq!(
            spec_for_bin("claude").unwrap().session_store(),
            SessionStore::ClaudeProjects
        );
        assert_eq!(
            spec_for_bin("codex").unwrap().session_store(),
            SessionStore::CodexRollouts
        );
        // 保存先を宣言しているなら ID 指定再開は必ずできる (逆は成り立たない —
        // 一覧はできないが ID 指定再開だけできる CLI があるため)。
        for spec in AGENT_CATALOG {
            let has_store = spec.session_store() != SessionStore::None;
            if has_store {
                assert!(
                    !spec.resume_id_flag().is_empty(),
                    "{} は保存先だけあって ID 指定再開が無い",
                    spec.bin
                );
                assert!(
                    !spec.resume_flag.is_empty(),
                    "{} は再開指定も要る",
                    spec.bin
                );
            }
        }
    }

    /// 一覧できないのに ID 指定再開だけ持つ CLI は、**必ず理由を書く**。
    /// (書き忘れると「一覧が空なのはバグか仕様か」が誰にも分からなくなる)
    #[test]
    fn resume_id_without_store_has_a_reason() {
        for spec in AGENT_CATALOG {
            if spec.resume_id_flag().is_empty() {
                assert_eq!(
                    spec.no_store_reason(),
                    "",
                    "{} は ID 指定再開が無いのに理由だけある",
                    spec.bin
                );
                continue;
            }
            if spec.session_store() == SessionStore::None {
                assert!(
                    !spec.no_store_reason().is_empty(),
                    "{} は一覧できないのに理由が空",
                    spec.bin
                );
            }
        }
    }

    /// SESSION_STORES / LAUNCH_ARGS の bin は、必ずカタログに実在すること。
    /// (bin をリネームしたときに表だけ取り残されるのを防ぐ)
    #[test]
    fn side_tables_only_reference_real_bins() {
        for (bin, ..) in SESSION_STORES {
            assert!(
                AGENT_CATALOG.iter().any(|s| s.bin == *bin),
                "SESSION_STORES の {bin} がカタログに無い"
            );
        }
        for (bin, args) in LAUNCH_ARGS {
            assert!(
                AGENT_CATALOG.iter().any(|s| s.bin == *bin),
                "LAUNCH_ARGS の {bin} がカタログに無い"
            );
            assert!(!args.trim().is_empty(), "{bin} の起動引数が空");
        }
    }

    /// 起動コマンドは必ず bin で始まり、余計な空白を含まない。
    #[test]
    fn launch_command_starts_with_bin() {
        for spec in AGENT_CATALOG {
            let cmd = spec.launch_command();
            assert!(cmd.starts_with(spec.bin), "{}: {cmd}", spec.bin);
            assert_eq!(cmd.trim(), cmd, "{}: 前後に空白がある", spec.bin);
            // 起動コマンドの先頭トークンからカタログを引き直せること
            assert_eq!(
                spec_for_command(&cmd).map(|s| s.bin),
                Some(spec.bin),
                "{} の起動形からカタログを引けない",
                spec.bin
            );
        }
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
            apply_approval(
                "codex --dangerously-bypass-approvals-and-sandbox",
                Approval::Agent
            ),
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
            apply_approval(
                "codex --dangerously-bypass-approvals-and-sandbox",
                Approval::Ask
            ),
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

    /// Antigravity は別名で書いても同じ定義に解決され、Auto ではどの起動経路でも
    /// 自動承認フラグと `auto_env` が実際にプロセスへ渡ること。
    ///
    /// 経路は 2 つある:
    /// - 新規起動 `AgentManager::launch` … `apply_approval` + `merged_env`
    /// - 復元起動 `AgentManager::launch_restored` … セッション記録に残った
    ///   「起動時のコマンド」(= すでに `apply_approval` 済み) に `apply_resume`
    ///   を足し、env は `merged_env` で引き直す (app.rs のフォルダ復元処理)。
    ///   復元で承認フラグが落ちると、再開したタブだけ自動YESが効かなくなる。
    #[test]
    fn antigravity_aliases_share_one_spec_and_keep_auto_flag_on_every_launch_path() {
        use std::collections::HashMap;

        let canonical = super::spec_for_bin("agy").unwrap();
        for name in ["agy", "antigravity", "antigravity-cli"] {
            let spec = super::spec_for_bin(name)
                .unwrap_or_else(|| panic!("{name} がカタログに解決されない"));
            assert!(
                std::ptr::eq(spec, canonical),
                "{name} が agy と同じ定義に解決されない"
            );
            // パス付き / 引数付きでも同じ (実際のプリセットの書き方)。
            let cmd = format!("/opt/bin/{name} --model gemini-3-pro");
            assert!(
                std::ptr::eq(super::spec_for_command(&cmd).unwrap(), canonical),
                "{cmd} が解決されない"
            );

            // ① 新規起動: フラグが付き、auto_env が入る。
            let launched = apply_approval(name, Approval::Auto);
            assert_eq!(
                launched,
                format!("{name} --dangerously-skip-permissions"),
                "{name}: Auto で自動承認フラグが付かない"
            );
            let env = merged_env(name, Approval::Auto, &HashMap::new());
            for (k, v) in canonical.auto_env {
                assert_eq!(env.get(*k).map(String::as_str), Some(*v), "{name}: {k}");
            }
            assert!(
                super::env_enables_auto(name, &env),
                "{name}: env だけでも全自動と判定できること"
            );

            // ② 復元起動: 記録された起動時コマンド (フラグ入り) から組み立て直す。
            let restored_cmd = match super::spec_for_command(&launched) {
                Some(s) => super::apply_resume(&launched, s),
                None => launched.clone(),
            };
            assert!(
                restored_cmd.contains("--dangerously-skip-permissions"),
                "{name}: 復元経路で自動承認フラグが落ちている ({restored_cmd})"
            );
            let restored_env = merged_env(&restored_cmd, Approval::Auto, &HashMap::new());
            for (k, v) in canonical.auto_env {
                assert_eq!(
                    restored_env.get(*k).map(String::as_str),
                    Some(*v),
                    "{name}: 復元経路で {k} が渡っていない"
                );
            }

            // Ask では逆に、フラグも env も一切足さない (事故防止)。
            assert_eq!(apply_approval(&launched, Approval::Ask), name.to_string());
            assert!(merged_env(name, Approval::Ask, &HashMap::new()).is_empty());
        }
    }

    /// 応答表そのものの健全性 — データを足すときの事故よけ。
    #[test]
    fn prompt_rules_are_well_formed_and_target_known_agents() {
        for r in super::PROMPT_RULES {
            assert!(!r.needles.is_empty(), "needles が空: {}", r.desc);
            assert!(!r.reply.is_empty(), "reply が空: {}", r.desc);
            assert!(!r.desc.is_empty(), "desc が空");
            assert!(
                r.agent.is_empty() || super::spec_for_bin(r.agent).is_some(),
                "カタログに無いエージェント名: {}",
                r.agent
            );
        }
    }

    #[test]
    fn 番号メニューの語彙表は小文字で書かれている() {
        // 照合側は画面を小文字化してから contains する。表に大文字が
        // 紛れ込むと**そのルールが永久に一致しない**ので機械的に止める。
        let tables: &[(&str, &[&str])] = &[
            ("MENU_NUMBER_HINTS", super::MENU_NUMBER_HINTS),
            ("MENU_ARROW_HINTS", super::MENU_ARROW_HINTS),
            ("MENU_AFFIRM", super::MENU_AFFIRM),
            ("MENU_NEGATIONS", super::MENU_NEGATIONS),
            ("MENU_SKIP", super::MENU_SKIP),
            ("MENU_SKIP_EXACT", super::MENU_SKIP_EXACT),
            ("MENU_SURVEY_MARKS", super::MENU_SURVEY_MARKS),
            ("MENU_RATING_BEST", super::MENU_RATING_BEST),
            ("MENU_RATING_WORST", super::MENU_RATING_WORST),
        ];
        for (name, table) in tables {
            for w in *table {
                assert!(!w.is_empty(), "{name} に空の語がある");
                assert_eq!(*w, w.to_lowercase(), "{name} に大文字が混ざっている: {w}");
            }
        }
    }

    #[test]
    fn 評価の肯定端と否定端が食い違わない() {
        // 「very dissatisfied」が肯定端に化けると最悪の評点を送ってしまう。
        for best in super::MENU_RATING_BEST {
            for worst in super::MENU_RATING_WORST {
                assert!(
                    !worst.contains(best),
                    "否定端「{worst}」が肯定端「{best}」を含む (取り違える)"
                );
            }
        }
    }

    // ---- カタログ (AgentSpec) ----
    use super::{command_is_bypass, spec_for_bin, spec_for_command, AGENT_CATALOG};

    #[test]
    fn catalog_lookup_by_bare_name() {
        assert_eq!(spec_for_command("claude").unwrap().label, "Claude Code");
        assert_eq!(spec_for_bin("kiro-cli").unwrap().label, "Kiro");
        assert_eq!(
            spec_for_command("antigravity").unwrap().label,
            "Antigravity"
        );
        assert_eq!(
            spec_for_command("antigravity-cli").unwrap().label,
            "Antigravity"
        );
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
            spec_for_command("/opt/homebrew/bin/goose run -t hi")
                .unwrap()
                .bin,
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
        // `cmd` は Windows のシェルと衝突するため、その環境ではエイリアスを
        // 張らない (= エージェントとして解決しないので素通し)。
        #[cfg(not(windows))]
        assert_eq!(
            apply_approval("cmd --dangerously-skip-permissions", Approval::Ask),
            "cmd"
        );
        #[cfg(windows)]
        assert_eq!(
            apply_approval("cmd --dangerously-skip-permissions", Approval::Ask),
            "cmd --dangerously-skip-permissions",
            "Windows では cmd をエージェント扱いしないので引数を触らない"
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
        assert!(command_is_bypass(
            "claude --permission-mode=bypassPermissions"
        ));
        // bypass ではないものは false
        assert!(!command_is_bypass("claude --permission-mode plan"));
        assert!(!command_is_bypass("claude"));
        // 短いフラグは持ち主の CLI のときだけ bypass 扱い(誤検知防止)
        assert!(command_is_bypass("cursor-agent -f"));
        assert!(!command_is_bypass("grep -f patterns.txt"));
        assert!(!command_is_bypass("codex --full-auto-ish"));
    }

    #[test]
    fn codebuff_is_present_but_without_a_fabricated_auto_flag() {
        // 一括自動承認フラグを持たない CLI も、フラグを捏造しなければ
        // 「起動はできる項目」として並べてよい (auggie / goose と同じ扱い)。
        let s = spec_for_bin("codebuff").expect("codebuff はカタログにある");
        assert_eq!(
            s.auto_flag, "",
            "codebuff に自動承認フラグを捏造してはいけない"
        );
        assert!(s.auto_env.is_empty());
        // 「全自動」プリセットは作られない = 壊れた項目が UI に出ない
        assert!(crate::agent_picker::auto_preset(s).is_none());
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

    // ── カタログ整合性 (カタログ全件を回すので、追加分は自動で対象になる) ──

    /// Ask で確実に自動承認を解除できること = 自分の auto フラグを
    /// 自分で剥がせること。単独トークンのフラグは `strip` に、
    /// 「フラグ + 値」の 2 トークン形は `TWO_TOKEN_BYPASS` に載っていなければならない。
    #[test]
    fn every_auto_flag_is_strippable_by_its_own_spec() {
        for s in AGENT_CATALOG {
            if s.auto_flag.is_empty() {
                continue;
            }
            let tokens: Vec<&str> = s.auto_flag.split_whitespace().collect();
            if tokens.len() == 1 {
                assert!(
                    s.strip.contains(&s.auto_flag),
                    "{} の strip に自分の auto フラグ {} が無い",
                    s.bin,
                    s.auto_flag
                );
            } else {
                assert_eq!(tokens.len(), 2, "{} の auto フラグが 3 トークン以上", s.bin);
                assert!(
                    is_bypass_two_token(tokens[0], tokens[1]),
                    "{} の 2 トークン auto フラグが TWO_TOKEN_BYPASS に無い: {}",
                    s.bin,
                    s.auto_flag
                );
            }
        }
    }

    /// カタログ全件で Auto → Ask → Auto が期待どおり動くこと。
    /// 個別に書かないので、CLI を足しても自動で検証対象になる。
    #[test]
    fn apply_approval_round_trips_for_every_catalog_entry() {
        for s in AGENT_CATALOG {
            let plain = s.launch_command();
            let auto = apply_approval(&plain, Approval::Auto);
            if s.auto_flag.is_empty() {
                // フラグを持たない CLI は Auto でも書き換えない(捏造しない)
                assert_eq!(auto, plain, "{}: auto フラグが無いのに書き換えた", s.bin);
            } else {
                assert_eq!(auto, format!("{plain} {}", s.auto_flag), "{}", s.bin);
                assert!(command_is_bypass(&auto), "{}: bypass と判定されない", s.bin);
                // 二重付与しない
                assert_eq!(apply_approval(&auto, Approval::Auto), auto, "{}", s.bin);
            }
            // Ask は必ず素のコマンドへ戻す
            assert_eq!(apply_approval(&auto, Approval::Ask), plain, "{}", s.bin);
            assert!(
                !command_is_bypass(&apply_approval(&auto, Approval::Ask)),
                "{}",
                s.bin
            );
            // Agent はどんなコマンドでも一切触らない
            assert_eq!(apply_approval(&auto, Approval::Agent), auto, "{}", s.bin);
        }
    }

    /// カタログ全件で「直前の会話を再開」と「ID 指定で再開」が壊れないこと。
    #[test]
    fn apply_resume_and_resume_id_are_sane_for_every_catalog_entry() {
        for s in AGENT_CATALOG {
            let plain = s.launch_command();

            let resumed = apply_resume(&plain, s);
            if s.resume_flag.is_empty() {
                assert_eq!(resumed, plain, "{}: 再開未対応なのに書き換えた", s.bin);
            } else {
                assert!(resumed.starts_with(s.bin), "{}: {resumed}", s.bin);
                assert!(resumed.contains(s.resume_flag), "{}: {resumed}", s.bin);
                // 二重付与しない
                assert_eq!(apply_resume(&resumed, s), resumed, "{}", s.bin);
                // 先頭トークンからカタログを引き直せる (サブコマンド型でも壊れない)
                assert_eq!(spec_for_command(&resumed).map(|x| x.bin), Some(s.bin));
            }

            let by_id = apply_resume_id(&plain, s, "abc-123");
            if s.resume_id_flag().is_empty() {
                assert_eq!(by_id, plain, "{}: ID 再開未対応なのに書き換えた", s.bin);
            } else {
                assert!(by_id.contains("abc-123"), "{}: {by_id}", s.bin);
                assert!(by_id.starts_with(s.bin), "{}: {by_id}", s.bin);
                assert_eq!(apply_resume_id(&by_id, s, "zzz"), by_id, "{}", s.bin);
                assert_eq!(spec_for_command(&by_id).map(|x| x.bin), Some(s.bin));
            }
            // 危険な ID は絶対にコマンドへ載せない
            for bad in ["", "  ", "a b", "x;rm -rf /", "$(id)"] {
                assert_eq!(apply_resume_id(&plain, s, bad), plain, "{}: {bad}", s.bin);
            }
        }
    }

    /// 別名は必ずただ 1 つの spec に解決し、実在の bin と衝突しないこと。
    #[test]
    fn every_alias_resolves_to_exactly_one_spec() {
        for (alias, target, windows_safe) in AGENT_ALIASES {
            assert!(
                AGENT_CATALOG.iter().any(|s| s.bin == *target),
                "別名 {alias} の解決先 {target} がカタログに無い"
            );
            assert_ne!(alias, target, "自分自身への別名は無意味: {alias}");
            assert!(
                AGENT_CATALOG.iter().all(|s| s.bin != *alias),
                "{alias} は実在の bin なので別名にしてはいけない"
            );
            assert!(
                AGENT_ALIASES.iter().filter(|(a, ..)| a == alias).count() == 1,
                "別名 {alias} が重複している"
            );
            // Windows で解決してよい別名だけが Windows でも引ける
            let resolved = spec_for_bin(alias).map(|s| s.bin);
            let expect_resolvable = *windows_safe || !cfg!(windows);
            assert_eq!(
                resolved,
                expect_resolvable.then_some(*target),
                "別名 {alias} の解決結果が想定と違う"
            );
        }
    }

    /// 別名から引いた spec は、本名から引いたものと完全に同一であること。
    #[test]
    fn alias_lookup_table() {
        let table: &[(&str, &str)] = &[
            ("claude-code", "claude"),
            ("gemini-cli", "gemini"),
            ("antigravity", "agy"),
            ("antigravity-cli", "agy"),
            ("mimo-code", "mimo"),
            ("qwen-code", "qwen"),
            ("mistral-vibe", "vibe"),
            ("cursor", "cursor-agent"),
            ("aug", "auggie"),
        ];
        for (alias, bin) in table {
            let via_alias = spec_for_bin(alias).unwrap_or_else(|| panic!("{alias} が引けない"));
            let via_bin = spec_for_bin(bin).unwrap_or_else(|| panic!("{bin} が引けない"));
            assert!(
                std::ptr::eq(via_alias, via_bin),
                "{alias} と {bin} が別実体"
            );
            // コマンド行 (引数つき) からも同じ spec に届くこと
            assert_eq!(
                spec_for_command(&format!("/opt/bin/{alias} --model x")).map(|s| s.bin),
                Some(*bin)
            );
        }
        // `cmd` は Windows では解決してはいけない (cmd.exe と衝突するため)
        #[cfg(windows)]
        assert!(spec_for_bin("cmd").is_none());
        #[cfg(not(windows))]
        assert_eq!(spec_for_bin("cmd").map(|s| s.bin), Some("command-code"));
        // 定義していない名前は解決しない
        assert!(spec_for_bin("kiro").is_none(), "kiro は IDE 本体で別物");
        assert!(
            spec_for_bin("continue").is_none(),
            "continue はシェル予約語"
        );
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
        preset.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "~/.claude-work".to_string(),
        );
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

    #[test]
    fn isolated_env_assigns_session_config_dir_for_claude() {
        let empty = HashMap::new();
        let env = isolated_env_for_session("claude", 42, Approval::Ask, &empty);
        let home = dirs::home_dir().unwrap();
        let expected = home
            .join(".claude-sessions/session-42")
            .to_str()
            .unwrap()
            .to_string();
        assert_eq!(env.get("CLAUDE_CONFIG_DIR"), Some(&expected));

        // 明示的に指定されていれば上書きしない
        let mut preset = HashMap::new();
        preset.insert(
            "CLAUDE_CONFIG_DIR".to_string(),
            "~/custom-claude".to_string(),
        );
        let env_custom = isolated_env_for_session("claude", 42, Approval::Ask, &preset);
        let expected_custom = home.join("custom-claude").to_str().unwrap().to_string();
        assert_eq!(env_custom.get("CLAUDE_CONFIG_DIR"), Some(&expected_custom));
    }

    /// launch と同じ経路: Auto なら環境変数が入り、それを Session 側が
    /// 「全自動起動」と認識できること (goose / aider の Auto を実際に機能させる鍵)。
    #[test]
    fn auto_mode_env_round_trips_into_env_enables_auto() {
        let empty = HashMap::new();
        for bin in ["goose", "aider"] {
            let auto = merged_env(bin, Approval::Auto, &empty);
            assert!(
                env_enables_auto(bin, &auto),
                "{} は Auto で全自動になるべき",
                bin
            );
            let ask = merged_env(bin, Approval::Ask, &empty);
            assert!(
                !env_enables_auto(bin, &ask),
                "{} は Ask で全自動になってはいけない",
                bin
            );
        }
    }

    // ---- stop_all(): 全エージェント一括停止 ----
    //
    // 実 PTY を使うので unix 限定 (remove_active と同じ理由)。
    // **長い sleep を書かない** — 取りこぼしたときにプロセスが残り続けるため、
    // 子は数秒で自然終了する長さにしておく。
    #[cfg(unix)]
    mod stop_all_tests {
        use crate::agents::AgentManager;
        use crate::terminal::{Session, SpawnSpec};
        use eframe::egui;
        use std::collections::HashMap;
        use std::time::{Duration, Instant};

        fn mgr(n: usize) -> AgentManager {
            let mut m = AgentManager::new();
            for i in 0..n {
                let spec = SpawnSpec {
                    title: format!("s{}", i + 1),
                    preset_name: "t".into(),
                    icon: "t".into(),
                    // 取りこぼしても 8 秒で自然に消える長さ。
                    command: "/bin/sleep 8".into(),
                    cwd: std::env::temp_dir(),
                    env: HashMap::new(),
                    log_path: None,
                };
                let s = Session::spawn(i as u64 + 1, spec, egui::Context::default())
                    .expect("テスト用セッションの起動に失敗");
                m.sessions.push(s);
            }
            m
        }

        /// 全部が終了するまで待つ (上限つき)。返り値は「全部止まったか」。
        fn wait_all_exited(m: &AgentManager, limit: Duration) -> bool {
            let start = Instant::now();
            while start.elapsed() < limit {
                if m.running_count() == 0 {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            m.running_count() == 0
        }

        #[test]
        fn 一括停止は稼働中を全部止めて本数を返す() {
            let mut m = mgr(3);
            assert_eq!(m.running_count(), 3, "3 本起動しているはず");
            assert_eq!(m.stop_all(), 3, "止めに行った本数");
            assert!(
                wait_all_exited(&m, Duration::from_secs(6)),
                "一括停止で全部止まらなかった (孤児が残っている疑い)"
            );
            // タブ自体は残る (あとから ⟳ で起動し直せる)。
            assert_eq!(m.sessions.len(), 3);
            for s in &mut m.sessions {
                s.kill();
            }
        }

        #[test]
        fn 終了済みへは撃たない() {
            let mut m = mgr(2);
            assert_eq!(m.stop_all(), 2);
            assert!(wait_all_exited(&m, Duration::from_secs(6)), "止まらない");
            // 2 度目は 1 本も撃たない — wait 済みの PID は再利用され得るので、
            // ここで撃つと無関係なプロセス (グループ) を巻き添えにする。
            assert_eq!(m.stop_all(), 0, "終了済みへ kill を撃っている");
        }

        #[test]
        fn セッションが無いときは何もしない() {
            let mut m = AgentManager::new();
            assert_eq!(m.stop_all(), 0);
        }
    }

    // ---- remove() の active 保存 ----
    // Session は PTY 上の実プロセスが要るため、terminal.rs の pty_tests と同じく
    // Session::spawn で /bin/sleep を起動して組み立てる (unix 限定)。
    #[cfg(unix)]
    mod remove_active {
        use crate::agents::AgentManager;
        use crate::terminal::{Session, SpawnSpec};
        use eframe::egui;
        use std::collections::HashMap;

        /// n 本の実セッション (id は 1..=n) を持つマネージャを作る。
        fn mgr(n: usize) -> AgentManager {
            let mut m = AgentManager::new();
            for i in 0..n {
                let spec = SpawnSpec {
                    title: format!("s{}", i + 1),
                    preset_name: "t".into(),
                    icon: "t".into(),
                    command: "/bin/sleep 30".into(),
                    cwd: std::env::temp_dir(),
                    env: HashMap::new(),
                    log_path: None,
                };
                let s = Session::spawn(i as u64 + 1, spec, egui::Context::default())
                    .expect("テスト用セッションの起動に失敗");
                m.sessions.push(s);
            }
            m
        }

        /// app.rs の閉じる経路と同じ。`remove` が終了と後始末まで持っていくので
        /// (crate::terminal::reap)、呼び出し側は index を渡すだけでよい。
        fn close(m: &mut AgentManager, i: usize) {
            m.remove(i);
        }

        fn kill_all(m: &mut AgentManager) {
            for s in m.sessions.iter_mut() {
                s.kill();
            }
        }

        #[test]
        fn closing_left_of_active_keeps_same_session_focused() {
            // [1,2,3] active=2(id3) → 左端を閉じても id3 を指し続ける
            let mut m = mgr(3);
            m.active = 2;
            close(&mut m, 0);
            let (active, id) = (m.active, m.sessions[m.active].id);
            kill_all(&mut m);
            assert_eq!(active, 1, "active は1つ左へ詰まる");
            assert_eq!(id, 3, "フォーカス中のセッションがすり替わってはいけない");
        }

        #[test]
        fn closing_active_middle_moves_to_right_neighbor() {
            // [1,2,3] active=1(id2) → 自分を閉じたら据え置きで右隣 id3 へ
            let mut m = mgr(3);
            m.active = 1;
            close(&mut m, 1);
            let (active, id) = (m.active, m.sessions[m.active].id);
            kill_all(&mut m);
            assert_eq!(active, 1);
            assert_eq!(id, 3, "中間の active を閉じたら右隣へ移る");
        }

        #[test]
        fn closing_active_rightmost_clamps_to_left_neighbor() {
            // [1,2,3] active=2(id3) → 最右の自分を閉じたら左隣 id2 へクランプ
            let mut m = mgr(3);
            m.active = 2;
            close(&mut m, 2);
            let (active, id) = (m.active, m.sessions[m.active].id);
            kill_all(&mut m);
            assert_eq!(active, 1);
            assert_eq!(id, 2, "最右の active を閉じたら左隣へ移る");
        }

        #[test]
        fn closing_right_of_active_leaves_active_untouched() {
            // [1,2,3] active=0(id1) → 右を閉じても active 不変
            let mut m = mgr(3);
            m.active = 0;
            close(&mut m, 2);
            let (active, id) = (m.active, m.sessions[m.active].id);
            kill_all(&mut m);
            assert_eq!(active, 0);
            assert_eq!(id, 1);
        }

        #[test]
        fn closing_everything_never_leaves_active_out_of_bounds() {
            // 先頭から全部閉じても active が範囲外を指さない
            let mut m = mgr(3);
            m.active = 2;
            while !m.sessions.is_empty() {
                close(&mut m, 0);
                assert!(
                    m.sessions.is_empty() || m.active < m.sessions.len(),
                    "active={} len={}",
                    m.active,
                    m.sessions.len()
                );
            }
            assert!(
                m.active_session().is_none(),
                "空なら active_session は None"
            );
        }
    }

    // ---- poll_events(): 承認待ちのまま子が exit した場合 ----
    // remove_active と同じく実PTYで Session を起こす (unix 限定)。
    #[cfg(unix)]
    mod poll_events_exit {
        use crate::agents::{AgentManager, SessionEvent};
        use crate::terminal::{Session, SpawnSpec};
        use eframe::egui;
        use std::collections::HashMap;
        use std::time::Duration;

        /// 指定コマンドの実セッションを 1 本だけ持つマネージャを作る。
        fn manager_with(cmd: &str, id: u64) -> AgentManager {
            let spec = SpawnSpec {
                title: "poll-e2e".into(),
                preset_name: "test".into(),
                icon: "🧪".into(),
                command: cmd.into(),
                cwd: std::env::temp_dir(),
                env: HashMap::new(),
                log_path: None,
            };
            let s = Session::spawn(id, spec, egui::Context::default())
                .expect("テスト用セッションの起動に失敗");
            let mut m = AgentManager::new();
            m.sessions.push(s);
            m
        }

        #[test]
        fn exit_while_awaiting_approval_emits_exited_once_and_clears_attention() {
            // 承認プロンプトを出し、応答を待ったまま 3 秒で子が勝手に終了する。
            let cmd = r#"stty -echo; printf 'Do you want to proceed? (y/n) '; sleep 3"#;
            let mut m = manager_with(cmd, 9901);

            // 1) 生きている間は承認待ちとして報告される
            let mut needs = false;
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                for ev in m.poll_events(false) {
                    if matches!(ev, SessionEvent::NeedsApproval(_)) {
                        needs = true;
                    }
                }
                if needs {
                    break;
                }
            }
            assert!(needs, "終了前の承認プロンプトが検知されなかった");
            assert!(m.sessions[0].attention);

            // 2) 承認待ちのまま子が exit → Exited が一回だけ出て、承認待ちは
            //    解除される。自動YESを許可していても、死んだセッションへ
            //    NeedsApproval / AutoApproved が誤発火しない。
            let mut exited = 0u32;
            let mut stray = 0u32;
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                for ev in m.poll_events(true) {
                    match ev {
                        SessionEvent::Exited(..) => exited += 1,
                        SessionEvent::NeedsApproval(_) | SessionEvent::AutoApproved(..) => {
                            stray += 1
                        }
                        SessionEvent::RateLimited(..) => {}
                    }
                }
                if exited > 0 {
                    break;
                }
            }
            assert_eq!(exited, 1, "Exited が一回だけ届かなかった");
            assert!(!m.sessions[0].attention, "子の終了後も承認待ちが残った");
            assert_eq!(stray, 0, "死んだセッションへ承認イベントが誤発火した");

            // 3) 以後なんどポーリングしても Exited は増えない (多重通知の防止)
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(100));
                for ev in m.poll_events(true) {
                    match ev {
                        SessionEvent::Exited(..) => exited += 1,
                        SessionEvent::NeedsApproval(_) | SessionEvent::AutoApproved(..) => {
                            stray += 1
                        }
                        SessionEvent::RateLimited(..) => {}
                    }
                }
            }
            assert_eq!(exited, 1, "Exited が多重通知された");
            assert_eq!(stray, 0);
        }

        #[test]
        fn prompt_then_immediate_exit_only_reports_exited() {
            // プロンプトを表示した直後に子が終了する。初回スキャンの機会
            // (起動から 900ms) より先に死ぬため、承認系イベントは一切出ずに
            // Exited だけが届くのが現挙動。
            let cmd = r#"printf 'Do you want to proceed? (y/n) '; exit 0"#;
            let mut m = manager_with(cmd, 9902);

            let mut exited = 0u32;
            let mut stray = 0u32;
            for _ in 0..100 {
                std::thread::sleep(Duration::from_millis(100));
                for ev in m.poll_events(true) {
                    match ev {
                        SessionEvent::Exited(..) => exited += 1,
                        SessionEvent::NeedsApproval(_) | SessionEvent::AutoApproved(..) => {
                            stray += 1
                        }
                        SessionEvent::RateLimited(..) => {}
                    }
                }
                if exited > 0 {
                    break;
                }
            }
            assert_eq!(exited, 1, "Exited が届かなかった");
            assert_eq!(
                stray, 0,
                "プロンプト直後に死んだセッションへ承認イベントが誤発火した"
            );
            assert!(!m.sessions[0].attention);

            // 画面にはプロンプトが残っているが、死んだセッションはスキャン対象外
            let screen = m.sessions[0].parser.lock().unwrap().screen().contents();
            assert!(
                screen.contains("(y/n)"),
                "前提: プロンプトは画面に残っている"
            );
            for _ in 0..10 {
                std::thread::sleep(Duration::from_millis(100));
                assert!(
                    m.poll_events(true).is_empty(),
                    "死んだセッションへイベントが出た"
                );
            }
        }
    }
}
