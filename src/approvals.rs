//! # 統合承認キュー (Unified Approval Queue)
//!
//! **すべてのエージェント CLI の承認要求を 1 本のキューへ集約し**、種別ごとの
//! ポリシーで捌き、判断を追記専用の監査ログへ残す。
//!
//! ## なぜ要るのか (競合との差)
//! 他製品は「各 CLI の bypass フラグを注入する」だけで、承認は **全部 YES か
//! 全部手動かの二択**、しかも何を許可したのか後から辿れない。Zaivern は
//!
//! - 種別 (読み取り / 書き込み / 削除 / シェル / ネットワーク / git /
//!   パッケージ導入 / 権限昇格) ごとに `Ask / 一回だけ許可 / 常に許可 /
//!   常に拒否` を持てる
//! - 適用範囲を `全体 / エージェント別 / セッション別 / パス接頭辞` で絞れる
//! - 判断は 1 行 = 1 JSON の追記ログ (`~/.zaivern/approvals.jsonl`) に残る
//!
//! ## 設計の原則
//! - **エージェント固有の知識はすべてデータ表**に置く ([`CLASS_RULES`])。
//!   分類ロジックには一切リテラルを書かない。CLI 側の文言が変わったら表の
//!   1 行を直せば済む。
//! - **分類は純関数**。表に当たらなければ必ず [`ApprovalKind::Other`] で、
//!   より狭い種別を推測しない (推測は誤った自動許可に直結する)。
//! - **権限昇格は決して自動承認しない**。既存の [`crate::agents::PROMPT_NEVER`]
//!   ガードは、どんなポリシー (`AllowAlways` を含む) よりも強い。
//!
//! ## UI 側の描画契約 (パネルは後日実装)
//! 毎フレーム:
//! 1. `agents.approvals.pending()` — 承認待ちの一覧 (古い順) を取る。
//! 2. 1 行につき `kind.icon()` + `tr(kind.label())` + `summary` を出し、
//!    折りたたみで `detail` / `raw_prompt_excerpt` を見せる。
//! 3. キー割り当ての推奨:
//!    - `Y` … [`Command::Approve`] (この 1 件)
//!    - `A` … [`Command::ApproveAllOfKind`] (保留中の同種すべて)
//!    - `⇧A` … [`Command::ApproveKindForAgentAlways`] (以後ずっと。ポリシー生成)
//!    - `N` … [`Command::Deny`]
//!    - `⇧N` … [`Command::DenyKindForAgentAlways`]
//! 4. 押されたら `approvals.apply(id, cmd)` を呼び、返る [`Resolution`] の
//!    `replies` を `(session_id, action)` の順に PTY へ流し
//!    (`ReplyAction::Approve` → `Session::press_pet_approve_button`、
//!    `ReplyAction::Deny` → `send_text(deny_keys)` + `resolve_attention()`)、
//!    `policy` が `Some` なら config.toml の `[[approval_policies]]` へ追記する。
//! 5. 監査ビューは `read_audit_tail(dir, cap)` で末尾だけ読む (全読みしない)。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

// ══════════════════════════════════════════════════════════════════════
//  上限値 — 「秘密を書かない」ための切り詰め幅もここに集約する
// ══════════════════════════════════════════════════════════════════════

/// 監査ログのファイル名 (`<zaivern_dir>/approvals.jsonl`)。
pub const AUDIT_FILE: &str = "approvals.jsonl";
/// 監査ログのローテート閾値 (バイト)。超えたら `.old` へ寄せる
/// (main.rs の panic.log / session.rs の term ログと同じ流儀)。
pub const AUDIT_MAX_BYTES: u64 = 1_000_000;
/// 監査ログの 1 行 `summary` の上限 (文字)。
///
/// **秘密を書かないための栓**。プロンプト本文には API キーやトークンが
/// 貼られていることがあるため、監査ログへ入るのは「種別 + 対象の頭 160 文字」
/// までで、プロンプト全文は決してディスクへ出さない。
pub const SUMMARY_CAP: usize = 160;
/// メモリ上に持つ `raw_prompt_excerpt` の上限 (文字)。ここも同じ理由で切る。
/// この抜粋は UI の折りたたみ表示用で、**監査ログには書き出さない**。
pub const EXCERPT_CAP: usize = 400;
/// 重複判定用に覚えておくプロンプト指紋の最大件数 (セッション横断)。
const SEEN_CAP: usize = 512;
/// 承認待ちキューの上限。あふれたら古いものから捨てる (UI が詰まらないように)。
const PENDING_CAP: usize = 256;
/// 自動YESの事前ゲート (ポリシー相談) の間引き間隔 (ミリ秒)。
/// `Session::scan_attention` の間引き (900ms) と揃える。
const PRE_GATE_INTERVAL_MS: u128 = 900;

// ══════════════════════════════════════════════════════════════════════
//  種別 (ApprovalKind) と、その分類表
// ══════════════════════════════════════════════════════════════════════

/// 承認要求の種別。
///
/// 表に当たらないものは必ず [`Other`](ApprovalKind::Other)。
/// 「たぶんシェルだろう」のような推測はしない — 推測した狭い種別に
/// `AllowAlways` が付いていると、意図しないものまで自動承認されるため。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum ApprovalKind {
    /// ファイルの読み取り
    FileRead,
    /// ファイルの作成・編集・上書き
    FileWrite,
    /// ファイル・ディレクトリの削除
    FileDelete,
    /// 任意のシェルコマンド実行
    ShellCommand,
    /// 外部ネットワークへのアクセス
    NetworkAccess,
    /// git / gh の操作 (commit / push / rebase …)
    GitOperation,
    /// パッケージの導入 (npm / pip / cargo / brew …)
    PackageInstall,
    /// OS の管理者権限昇格 — **自動承認は決して行わない**
    Privilege,
    /// 分類できなかったもの
    Other,
}

impl ApprovalKind {
    /// 設定ファイル・監査ログで使う安定 ID (ロケール非依存)。
    pub fn as_str(self) -> &'static str {
        match self {
            ApprovalKind::FileRead => "file_read",
            ApprovalKind::FileWrite => "file_write",
            ApprovalKind::FileDelete => "file_delete",
            ApprovalKind::ShellCommand => "shell_command",
            ApprovalKind::NetworkAccess => "network_access",
            ApprovalKind::GitOperation => "git_operation",
            ApprovalKind::PackageInstall => "package_install",
            ApprovalKind::Privilege => "privilege",
            ApprovalKind::Other => "other",
        }
    }

    /// 安定 ID から復元する。未知の文字列は `None`
    /// (= 設定の書き間違いを黙って `Other` に丸めない)。
    pub fn from_id(s: &str) -> Option<Self> {
        ALL_KINDS.iter().copied().find(|k| k.as_str() == s)
    }

    /// UI 表示名 (原文は日本語。`tr()` を通して使う)。
    pub fn label(self) -> &'static str {
        match self {
            ApprovalKind::FileRead => "ファイル読み取り",
            ApprovalKind::FileWrite => "ファイル書き込み",
            ApprovalKind::FileDelete => "ファイル削除",
            ApprovalKind::ShellCommand => "コマンド実行",
            ApprovalKind::NetworkAccess => "ネットワーク接続",
            ApprovalKind::GitOperation => "git 操作",
            ApprovalKind::PackageInstall => "パッケージ導入",
            ApprovalKind::Privilege => "管理者権限の昇格",
            ApprovalKind::Other => "その他の承認",
        }
    }

    /// UI 用アイコン。
    // 承認パネル (UI) 待ち。到達性はパネル実装で証明される。
    #[allow(dead_code)]
    pub fn icon(self) -> &'static str {
        match self {
            ApprovalKind::FileRead => "👁",
            ApprovalKind::FileWrite => "✏",
            ApprovalKind::FileDelete => "🗑",
            ApprovalKind::ShellCommand => "⌘",
            ApprovalKind::NetworkAccess => "🌐",
            ApprovalKind::GitOperation => "🌿",
            ApprovalKind::PackageInstall => "📦",
            ApprovalKind::Privilege => "🛡",
            ApprovalKind::Other => "❓",
        }
    }

    /// この種別は自動承認 (ポリシーによる無人応答) を許すか。
    ///
    /// 権限昇格だけは常に `false`。ポリシーが `AllowAlways` でも覆せない
    /// — 既存の [`crate::agents::PROMPT_NEVER`] ガードと同じ立場を、
    /// ポリシー層にも持ち込む。
    pub fn auto_approvable(self) -> bool {
        self != ApprovalKind::Privilege
    }
}

/// UI のフィルタ列や設定検証で使う全種別 (安定順)。
pub const ALL_KINDS: &[ApprovalKind] = &[
    ApprovalKind::FileRead,
    ApprovalKind::FileWrite,
    ApprovalKind::FileDelete,
    ApprovalKind::ShellCommand,
    ApprovalKind::NetworkAccess,
    ApprovalKind::GitOperation,
    ApprovalKind::PackageInstall,
    ApprovalKind::Privilege,
    ApprovalKind::Other,
];

/// 分類ルール 1 件。**ここだけがエージェント固有の知識**。
///
/// 判定は「画面テキストを小文字化したもの」に対して行う
/// (日本語は小文字化の影響を受けない)。そのため `all` / `any` / `avoid` の
/// ASCII 部分は**すべて小文字で書く** — [`class_rules_are_lowercase`] が検査する。
#[derive(Clone, Copy)]
pub struct ClassRule {
    /// 判定結果の種別。
    pub kind: ApprovalKind,
    /// 対象エージェント (カタログの `bin` 名)。`""` は全エージェント共通。
    pub agent: &'static str,
    /// **すべて**含まれていたら一致 (AND)。空なら AND 条件なし。
    pub all: &'static [&'static str],
    /// **どれか 1 つ**含まれていたら一致 (OR)。空なら OR 条件なし。
    pub any: &'static [&'static str],
    /// 1 つでも含まれていたら不一致にする除外語。
    pub avoid: &'static [&'static str],
}

/// 分類表。**上から順に**評価し、最初に一致した行の種別を採用する。
///
/// 並び順 = 具体性の順。危険側 (権限昇格 → 削除 → 導入 → git →
/// ネットワーク) を先に置き、汎用のシェル実行を最後にしている。
/// 例: `Bash command / rm -rf build` は「シェル実行」ではなく
/// **「ファイル削除」** に落ちる — 承認する人が見るべき危険はそちらだから。
///
/// 収録した文言の出どころ:
/// - Antigravity (`agy`) … `crate::agents::PROMPT_RULES` と同じ実測文字列
/// - Claude Code … `Bash command` / `Do you want to make this edit to` など
/// - Codex … `Allow command` / `Run command` 系
/// - 日本語 UI の CLI 全般 … 「〜しますか」「〜を許可」
pub static CLASS_RULES: &[ClassRule] = &[
    // ── 権限昇格 (最優先。ここに落ちたら自動承認は一切しない) ──────
    ClassRule {
        kind: ApprovalKind::Privilege,
        agent: "",
        all: &[],
        any: &[
            // agy の実測文言 (PROMPT_NEVER と対応)
            "one-time admin escalation",
            "administrator privileges are required",
            "run as administrator",
            "elevated privileges",
            "requires sudo",
            "sudo ",
            "管理者権限",
            "権限昇格",
            "昇格が必要",
        ],
        avoid: &[],
    },
    // ── 削除 ───────────────────────────────────────────────
    ClassRule {
        kind: ApprovalKind::FileDelete,
        agent: "",
        all: &[],
        any: &[
            "rm -rf",
            "rm -r ",
            "rm -f ",
            "remove-item",
            "delete this file",
            "delete file",
            "delete the file",
            "allow deletion",
            "yes, allow deletion",
            "を削除しますか",
            "ファイルを削除",
            "削除を許可",
        ],
        avoid: &[],
    },
    // ── パッケージ導入 ────────────────────────────────────
    ClassRule {
        kind: ApprovalKind::PackageInstall,
        agent: "",
        all: &[],
        any: &[
            "npm install",
            "npm i ",
            "pnpm add",
            "yarn add",
            "pip install",
            "pip3 install",
            "uv add",
            "cargo add",
            "cargo install",
            "brew install",
            "apt install",
            "apt-get install",
            "gem install",
            "go install",
            "go get ",
            "パッケージをインストール",
            "依存関係を追加",
        ],
        avoid: &[],
    },
    // ── git / gh ──────────────────────────────────────────
    ClassRule {
        kind: ApprovalKind::GitOperation,
        agent: "",
        all: &[],
        any: &[
            "git push",
            "git commit",
            "git add",
            "git rebase",
            "git merge",
            "git reset",
            "git checkout",
            "git switch",
            "git stash",
            "git tag",
            "gh pr ",
            "gh release",
            "コミットしますか",
            "プッシュしますか",
        ],
        avoid: &[],
    },
    // ── ネットワーク ──────────────────────────────────────
    ClassRule {
        kind: ApprovalKind::NetworkAccess,
        agent: "",
        all: &[],
        any: &[
            "webfetch",
            "websearch",
            "curl ",
            "wget ",
            "fetch this url",
            "access the internet",
            "network access",
            "ネットワークへ接続",
            "外部サイトへアクセス",
            "インターネットへ接続",
        ],
        avoid: &[],
    },
    // ── ファイル書き込み (作成 / 編集 / 上書き) ──────────────
    ClassRule {
        kind: ApprovalKind::FileWrite,
        agent: "",
        all: &[],
        any: &[
            // agy 実測
            "allow creation of this file?",
            "yes, allow creation",
            "accept this file edit?",
            "yes, accept this change",
            // Claude Code
            "do you want to make this edit to",
            "do you want to create",
            "edit file",
            "write file",
            "create file",
            // 汎用 / 日本語
            "overwrite",
            "上書きしますか",
            "ファイルを作成",
            "編集を適用",
            "書き込みを許可",
        ],
        avoid: &[],
    },
    // ── ファイル読み取り ──────────────────────────────────
    ClassRule {
        kind: ApprovalKind::FileRead,
        agent: "",
        all: &[],
        any: &[
            // agy 実測
            "allow access to this file?",
            "yes, allow access",
            // Claude Code / 汎用
            "do you want to read",
            "read file",
            "ファイルを読み取",
            "読み取りを許可",
        ],
        avoid: &[],
    },
    // ── シェル実行 (総取り。上のどれにも当たらなかったコマンド系) ──
    ClassRule {
        kind: ApprovalKind::ShellCommand,
        agent: "",
        all: &[],
        any: &[
            "bash command",
            "shell command",
            "run this command",
            "allow command",
            "run command",
            "execute command",
            "do you want to run",
            "コマンドを実行",
            "実行を許可",
        ],
        avoid: &[],
    },
];

/// 画面テキストを種別へ分類し、根拠になった行 (detail) も返す。
///
/// **純関数** — グローバル状態も時刻も触らない。表に当たらなければ
/// `(Other, 末尾の非空行)` を返す。
pub fn classify_detail(text: &str, agent: Option<&str>) -> (ApprovalKind, String) {
    let hay = text.to_lowercase();
    for rule in CLASS_RULES {
        if !rule.agent.is_empty() {
            if let Some(a) = agent {
                if a != rule.agent {
                    continue;
                }
            }
        }
        if rule.avoid.iter().any(|n| hay.contains(n)) {
            continue;
        }
        if !rule.all.is_empty() && !rule.all.iter().all(|n| hay.contains(n)) {
            continue;
        }
        // AND だけのルール (any が空) は all を満たした時点で一致。
        let hit = if rule.any.is_empty() {
            if rule.all.is_empty() {
                None // 何も条件が無い行は一致させない (書きかけの表で暴発させない)
            } else {
                rule.all.first().copied()
            }
        } else {
            rule.any.iter().copied().find(|n| hay.contains(n))
        };
        let Some(needle) = hit else { continue };
        return (rule.kind, evidence_line(text, needle));
    }
    (ApprovalKind::Other, last_content_line(text))
}

/// 種別だけが要るとき用。
// 承認パネル (UI) のフィルタ用。本体の投入経路は classify_detail を使う。
#[allow(dead_code)]
pub fn classify(text: &str, agent: Option<&str>) -> ApprovalKind {
    classify_detail(text, agent).0
}

/// `needle` (小文字化済み) を含む行を、原文のまま取り出す。
fn evidence_line(text: &str, needle: &str) -> String {
    text.lines()
        .find(|l| l.to_lowercase().contains(needle))
        .map(|l| trim_cap(l.trim(), SUMMARY_CAP))
        .unwrap_or_default()
}

/// 末尾の「中身のある行」。分類できなかったときの detail。
fn last_content_line(text: &str) -> String {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .map(|l| trim_cap(l, SUMMARY_CAP))
        .unwrap_or_default()
}

/// 文字数 (char) で切り詰める。UTF-8 の途中で割らない。
pub fn trim_cap(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    s.chars().take(cap).collect::<String>() + "…"
}

// ══════════════════════════════════════════════════════════════════════
//  ポリシー
// ══════════════════════════════════════════════════════════════════════

/// ポリシーの適用範囲。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Scope {
    /// すべてのエージェント・すべてのセッション
    Global,
    /// 実行ファイル名 (`claude` / `agy` …) 単位
    Agent(String),
    /// セッション ID 単位 (アプリを再起動すると ID は変わる)
    Session(u64),
    /// 対象パスの接頭辞単位
    PathPrefix(PathBuf),
}

impl Scope {
    /// 具体性スコア。**大きいほど具体的**で、解決時に優先される。
    ///
    /// `Session` > `PathPrefix` > `Agent` > `Global`。
    /// `PathPrefix` 同士は「パス要素数が多い方 = 深い方」が勝つので、
    /// `/repo/src/secret` は `/repo` を上書きできる。
    pub fn specificity(&self) -> u32 {
        match self {
            Scope::Global => 0,
            Scope::Agent(_) => 1_000,
            Scope::PathPrefix(p) => 2_000 + p.components().count() as u32,
            Scope::Session(_) => 900_000,
        }
    }

    /// 設定ファイル用の (種別名, 対象値)。
    // ポリシーを config.toml へ書き戻す UI 待ち (読み側の from_toml は配線済み)。
    #[allow(dead_code)]
    pub fn to_toml(&self) -> (&'static str, String) {
        match self {
            Scope::Global => ("global", String::new()),
            Scope::Agent(a) => ("agent", a.clone()),
            Scope::Session(id) => ("session", id.to_string()),
            Scope::PathPrefix(p) => ("path", p.display().to_string()),
        }
    }

    /// 設定ファイルから復元する。未知の種別は `None` (行ごと無視される)。
    pub fn from_toml(scope: &str, target: &str) -> Option<Self> {
        match scope {
            "global" | "" => Some(Scope::Global),
            "agent" if !target.is_empty() => Some(Scope::Agent(target.to_string())),
            "session" => target.parse().ok().map(Scope::Session),
            "path" if !target.is_empty() => Some(Scope::PathPrefix(PathBuf::from(target))),
            _ => None,
        }
    }
}

/// ポリシーの判断。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Decision {
    /// 既定。人間に聞く (キューへ積む)
    Ask,
    /// 今回だけ許可する。使うと**そのポリシーは消費される**
    AllowOnce,
    /// 以後ずっと許可する
    AllowAlways,
    /// 以後ずっと拒否する
    DenyAlways,
}

impl Decision {
    /// 設定ファイル・監査ログで使う安定 ID。
    pub fn as_str(self) -> &'static str {
        match self {
            Decision::Ask => "ask",
            Decision::AllowOnce => "allow_once",
            Decision::AllowAlways => "allow_always",
            Decision::DenyAlways => "deny_always",
        }
    }

    /// 安定 ID から復元する。未知の文字列は `None`。
    pub fn from_id(s: &str) -> Option<Self> {
        [
            Decision::Ask,
            Decision::AllowOnce,
            Decision::AllowAlways,
            Decision::DenyAlways,
        ]
        .into_iter()
        .find(|d| d.as_str() == s)
    }

    /// 許可側か。
    pub fn allows(self) -> bool {
        matches!(self, Decision::AllowOnce | Decision::AllowAlways)
    }
}

/// ポリシー 1 件。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Policy {
    pub kind: ApprovalKind,
    pub scope: Scope,
    pub decision: Decision,
}

impl Policy {
    /// この要求に当てはまるか (種別 + 範囲の両方が一致すること)。
    pub fn matches(&self, req: &ApprovalRequest) -> bool {
        if self.kind != req.kind {
            return false;
        }
        match &self.scope {
            Scope::Global => true,
            Scope::Agent(a) => req.agent_bin == *a,
            Scope::Session(id) => req.agent_session_id == *id,
            Scope::PathPrefix(p) => req.path.as_ref().is_some_and(|rp| rp.starts_with(p)),
        }
    }
}

/// ポリシー表から判断を引く (**純関数**)。
///
/// 解決順は **具体性の高いものが先**。同点なら**後ろに書いた方が勝つ**
/// (= あとから足したポリシーで上書きできる)。当たらなければ [`Decision::Ask`]。
///
/// **権限昇格は例外**: `req.never_auto` が立っている要求 (種別が
/// [`ApprovalKind::Privilege`]、または画面が [`crate::agents::PROMPT_NEVER`]
/// に当たる) に対しては、どんなポリシーでも常に `Ask` を返す。
pub fn resolve_with(policies: &[Policy], req: &ApprovalRequest) -> Decision {
    if req.never_auto {
        return Decision::Ask;
    }
    let mut best: Option<(u32, Decision)> = None;
    for p in policies {
        if !p.matches(req) {
            continue;
        }
        let s = p.scope.specificity();
        // `>=` なので、同じ具体性なら後勝ち。
        if best.map(|(bs, _)| s >= bs).unwrap_or(true) {
            best = Some((s, p.decision));
        }
    }
    best.map(|(_, d)| d).unwrap_or(Decision::Ask)
}

/// [`resolve_with`] と同じだが、勝った [`Decision::AllowOnce`] を消費して
/// 表から取り除く。キューが実運用で使う入口。
fn resolve_and_consume(policies: &mut Vec<Policy>, req: &ApprovalRequest) -> Decision {
    let d = resolve_with(policies, req);
    if d == Decision::AllowOnce {
        // 勝った 1 件だけを消す (同じ具体性なら後勝ちなので後ろから探す)。
        if let Some(i) = policies
            .iter()
            .rposition(|p| p.matches(req) && p.decision == Decision::AllowOnce)
        {
            policies.remove(i);
        }
    }
    d
}

// ── config.toml から配られるポリシーの受け口 ────────────────────────
//
// `crate::agents::set_user_prompt_rules` と同じ流儀。設定を読み直すたびに
// ここへ配り、キュー側は世代番号が変わったときだけ取り込む。

static POLICY_CELL: std::sync::OnceLock<std::sync::RwLock<(u64, Vec<Policy>)>> =
    std::sync::OnceLock::new();

fn policy_cell() -> &'static std::sync::RwLock<(u64, Vec<Policy>)> {
    POLICY_CELL.get_or_init(|| std::sync::RwLock::new((0, Vec::new())))
}

/// config.toml の `[[approval_policies]]` を取り込む (設定読み込みのたびに呼ぶ)。
pub fn set_policies(policies: Vec<Policy>) {
    if let Ok(mut g) = policy_cell().write() {
        if g.1 == policies {
            return;
        }
        g.0 = g.0.wrapping_add(1);
        g.1 = policies;
    }
}

/// いま配られているポリシー (世代番号つき)。
pub fn published_policies() -> (u64, Vec<Policy>) {
    policy_cell()
        .read()
        .map(|g| (g.0, g.1.clone()))
        .unwrap_or((0, Vec::new()))
}

// ── 承認/拒否キーの受け口 ──────────────────────────────────────────
//
// `pet_approve_keys` / `pet_deny_keys` は config.rs 側の値。ポリシーによる
// 無人応答は `poll_events` の中で起きるため、設定を引数で持ち回れない。
// ここも設定読み込み時に配る。

static KEYS_CELL: std::sync::OnceLock<std::sync::RwLock<(String, String)>> =
    std::sync::OnceLock::new();

fn keys_cell() -> &'static std::sync::RwLock<(String, String)> {
    KEYS_CELL.get_or_init(|| std::sync::RwLock::new(("\r".into(), "\u{1b}".into())))
}

/// 承認/拒否キーを配る (設定読み込みのたびに呼ぶ)。空文字は既定値のまま。
pub fn set_reply_keys(approve: &str, deny: &str) {
    if let Ok(mut g) = keys_cell().write() {
        if !approve.is_empty() {
            g.0 = approve.to_string();
        }
        if !deny.is_empty() {
            g.1 = deny.to_string();
        }
    }
}

/// いまの (承認キー, 拒否キー)。
pub fn reply_keys() -> (String, String) {
    keys_cell()
        .read()
        .map(|g| g.clone())
        .unwrap_or_else(|_| ("\r".into(), "\u{1b}".into()))
}

// ══════════════════════════════════════════════════════════════════════
//  承認要求
// ══════════════════════════════════════════════════════════════════════

/// キューに積まれる承認要求 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApprovalRequest {
    /// キュー内で一意な連番。UI のキー割り当てはこの ID を使う。
    pub id: u64,
    /// 要求元セッションの ID (`crate::terminal::Session::id`)。
    pub agent_session_id: u64,
    /// 要求元の実行ファイル名 (`claude` / `agy` …)。不明なら空文字。
    pub agent_bin: String,
    /// 分類結果。
    pub kind: ApprovalKind,
    /// UI の 1 行表示 (`種別ラベル — 根拠行`)。
    pub summary: String,
    /// 根拠になった画面の 1 行。
    pub detail: String,
    /// プロンプト本文の抜粋。[`EXCERPT_CAP`] 文字で切ってある。
    /// **監査ログには書き出さない** (秘密が混ざり得るため)。
    pub raw_prompt_excerpt: String,
    /// 検知時刻 (UNIX 秒)。
    pub created_at: u64,
    /// 検出できた対象パス ([`Scope::PathPrefix`] の突き合わせ先)。
    pub path: Option<PathBuf>,
    /// **自動応答してはいけない**要求か。
    /// 権限昇格、または [`crate::agents::PROMPT_NEVER`] に当たる画面で立つ。
    /// 一度立つとポリシー (`AllowAlways` 含む) では覆せない。
    pub never_auto: bool,
    /// 重複判定に使ったプロンプト指紋。
    pub signature: u64,
}

impl ApprovalRequest {
    /// 画面テキストから 1 件組み立てる (**純関数**。時刻だけ引数で受ける)。
    pub fn from_prompt(
        id: u64,
        session_id: u64,
        agent_bin: Option<&str>,
        text: &str,
        signature: u64,
        created_at: u64,
    ) -> Self {
        let (kind, detail) = classify_detail(text, agent_bin);
        let never_auto = !kind.auto_approvable() || crate::agents::prompt_never_answer(text);
        let summary = trim_cap(
            &format!("{} — {}", crate::i18n::tr(kind.label()), detail),
            SUMMARY_CAP,
        );
        ApprovalRequest {
            id,
            agent_session_id: session_id,
            agent_bin: agent_bin.unwrap_or_default().to_string(),
            kind,
            summary,
            detail: detail.clone(),
            raw_prompt_excerpt: trim_cap(text.trim(), EXCERPT_CAP),
            created_at,
            path: extract_path(&detail),
            never_auto,
            signature,
        }
    }
}

/// 根拠行から「それらしい絶対/相対パス」を 1 個拾う。
///
/// 見つからなければ `None` — 見つからないことは正常で、その場合
/// [`Scope::PathPrefix`] のポリシーはこの要求に当たらない (安全側)。
fn extract_path(detail: &str) -> Option<PathBuf> {
    detail
        .split_whitespace()
        .map(|t| t.trim_matches(|c: char| "\"'`(),:;?".contains(c)))
        .find(|t| {
            // 区切り文字を含み、URL ではないトークンをパスとみなす。
            t.len() > 1 && (t.contains('/') || t.contains('\\')) && !t.contains("://")
        })
        .map(PathBuf::from)
}

// ══════════════════════════════════════════════════════════════════════
//  監査ログ (JSONL・追記専用・サイズ上限つき)
// ══════════════════════════════════════════════════════════════════════

/// 判断の出どころ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Source {
    /// 人間がキューで押した
    // 生成元の ApprovalQueue::apply が承認パネル (UI) 待ちのため、
    // いまはテストからのみ構築される。
    #[allow(dead_code)]
    Manual,
    /// ポリシーが無人で決めた
    Policy,
    /// 従来の全自動YES (`pet_auto_yes`) が応答した
    AutoYes,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Manual => "manual",
            Source::Policy => "policy",
            Source::AutoYes => "auto_yes",
        }
    }
}

/// 監査ログの 1 行 (= 1 JSON)。
///
/// **プロンプト本文は入らない**。入るのは `summary` だけで、しかも
/// [`SUMMARY_CAP`] 文字で切ってある。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    /// UNIX 秒
    pub ts: u64,
    /// 実行ファイル名 (不明なら空文字)
    pub agent: String,
    /// [`ApprovalKind::as_str`]
    pub kind: String,
    /// [`Decision::as_str`]
    pub decision: String,
    /// [`Source::as_str`]
    pub source: String,
    /// 種別ラベル + 根拠行 (最大 [`SUMMARY_CAP`] 文字)
    pub summary: String,
}

/// 監査ログのパス。
pub fn audit_path(dir: &Path) -> PathBuf {
    dir.join(AUDIT_FILE)
}

/// 1 行追記する。肥大していたら先に `.old` へローテートする。
///
/// 書けなかったとき (権限なし等) は黙って諦める — 承認の流れを
/// ログの都合で止めない。
pub fn append_audit(dir: &Path, entry: &AuditEntry) {
    let Ok(line) = serde_json::to_string(entry) else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let path = audit_path(dir);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > AUDIT_MAX_BYTES) {
        let _ = std::fs::rename(&path, path.with_extension("jsonl.old"));
    }
    use std::io::Write as _;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{line}");
    }
}

/// 末尾だけ読む (UI の監査ビュー用)。`cap` バイトを超える分は捨てる。
///
/// `session::read_term_log_tail` と同じ流儀で `.old` → 本体の順に繋ぎ、
/// 途中で切れた先頭行は落とす。壊れた行 (書き込み中の切れ端など) は
/// 読み飛ばす — 監査ビューがログ 1 行のせいで真っ白にならないように。
// 監査ビュー (UI) 待ち。
#[allow(dead_code)]
pub fn read_audit_tail(dir: &Path, cap: usize) -> Vec<AuditEntry> {
    let path = audit_path(dir);
    let mut raw = std::fs::read(path.with_extension("jsonl.old")).unwrap_or_default();
    if let Ok(cur) = std::fs::read(&path) {
        raw.extend_from_slice(&cur);
    }
    if raw.len() > cap {
        let from = raw.len() - cap;
        let cut = raw[from..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| from + i + 1)
            .unwrap_or(from);
        raw.drain(..cut);
    }
    String::from_utf8_lossy(&raw)
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<AuditEntry>(l).ok())
        .collect()
}

/// いまの UNIX 秒。
fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ══════════════════════════════════════════════════════════════════════
//  キュー本体
// ══════════════════════════════════════════════════════════════════════

/// UI が PTY へ送るべき応答。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ReplyAction {
    /// 何も送らない
    // 生成元が承認パネル (UI) 待ち。
    #[allow(dead_code)]
    None,
    /// 承認キーを送る (`Session::press_pet_approve_button(Some(approve_keys))`)
    Approve,
    /// 拒否キーを送る (`send_text(deny_keys)` + `resolve_attention()`)
    Deny,
}

/// [`ApprovalQueue::intake`] の結果。
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// 同じセッションの同じプロンプトを既に見ている。何もしない。
    Duplicate,
    /// キューへ積んだ。UI が承認待ちとして見せる。
    Queued { id: u64 },
    /// ポリシーが即断した。`reply` をその場で PTY へ送る。
    Decided {
        id: u64,
        decision: Decision,
        reply: ReplyAction,
        /// トーストに出す説明 (`SessionEvent::AutoApproved` へ渡せるよう `'static`)。
        note: &'static str,
    },
}

/// UI から飛んでくる操作 (1 キーストローク = 1 コマンド)。
// 承認パネル (UI) 待ち。モジュール doc の「描画契約」がキー割り当てを定める。
#[allow(dead_code)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Command {
    /// この 1 件だけ承認する
    Approve,
    /// 保留中の**同じ種別**をまとめて承認する
    ApproveAllOfKind,
    /// 以後この種別 × このエージェントを常に許可する (ポリシーを作る)。
    /// 保留中の同種も同時に承認する。
    ApproveKindForAgentAlways,
    /// この 1 件だけ拒否する
    Deny,
    /// 以後この種別 × このエージェントを常に拒否する (ポリシーを作る)。
    /// 保留中の同種も同時に拒否する。
    DenyKindForAgentAlways,
}

/// [`ApprovalQueue::apply`] の結果。UI はこれを見て PTY と設定を更新する。
// 承認パネル (UI) 待ち。
#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Resolution {
    /// `(セッション ID, 送る応答)` を先頭から順に実行する。
    pub replies: Vec<(u64, ReplyAction)>,
    /// 生成されたポリシー。`Some` なら config.toml の
    /// `[[approval_policies]]` へ追記して永続化する。
    pub policy: Option<Policy>,
    /// 監査ログへ書いた件数。
    pub logged: usize,
    /// 権限昇格のため「常に許可」を作れなかった場合に立つ。
    /// UI はこのとき「この 1 件だけ承認しました」と伝える。
    pub refused_always: bool,
}

/// 全エージェント共通の承認キュー。[`crate::agents::AgentManager`] が 1 個持つ。
pub struct ApprovalQueue {
    /// 承認待ち (古い順)。
    pending: VecDeque<ApprovalRequest>,
    /// いま効いているポリシー。config.toml から配られたものを取り込む。
    pub policies: Vec<Policy>,
    /// 取り込み済みのポリシー世代。
    policy_gen: u64,
    /// 重複判定: `(セッション ID, プロンプト指紋)`。
    seen: HashSet<(u64, u64)>,
    /// `seen` の追い出し順 (FIFO)。
    seen_order: VecDeque<(u64, u64)>,
    /// 次に払い出す要求 ID。
    next_id: u64,
    /// 監査ログの置き場。`None` なら `~/.zaivern`。
    log_dir: Option<PathBuf>,
    /// 自動YES 事前ゲートの間引き用: セッション ID → (最終判定時刻, 遮断するか)。
    gate_cache: HashMap<u64, (Instant, bool)>,
}

impl Default for ApprovalQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl ApprovalQueue {
    // NOTE: 以下の `#[allow(dead_code)]` は、いずれも**承認パネル (UI) の配線待ち**。
    // パネルが入った時点で外すこと (モジュール doc の「描画契約」がそのまま呼び出し表)。
    pub fn new() -> Self {
        ApprovalQueue {
            pending: VecDeque::new(),
            policies: Vec::new(),
            policy_gen: 0,
            seen: HashSet::new(),
            seen_order: VecDeque::new(),
            next_id: 1,
            log_dir: None,
            gate_cache: HashMap::new(),
        }
    }

    /// 監査ログの置き場を指定して作る (テスト用 / 将来の per-workspace ログ用)。
    #[allow(dead_code)]
    pub fn in_dir(dir: impl Into<PathBuf>) -> Self {
        let mut q = Self::new();
        q.log_dir = Some(dir.into());
        q
    }

    /// 監査ログの置き場。
    pub fn log_dir(&self) -> PathBuf {
        self.log_dir
            .clone()
            .unwrap_or_else(crate::config::zaivern_dir)
    }

    /// 承認待ちの一覧 (古い順)。UI が毎フレーム呼ぶ。
    #[allow(dead_code)]
    pub fn pending(&self) -> impl Iterator<Item = &ApprovalRequest> {
        self.pending.iter()
    }

    /// 承認待ちの件数 (バッジ表示用)。
    #[allow(dead_code)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// 種別ごとの承認待ち件数 (「同種をまとめて承認」ボタンの表示用)。
    #[allow(dead_code)]
    pub fn pending_of_kind(&self, kind: ApprovalKind) -> usize {
        self.pending.iter().filter(|r| r.kind == kind).count()
    }

    /// ID から 1 件引く。
    #[allow(dead_code)]
    pub fn get(&self, id: u64) -> Option<&ApprovalRequest> {
        self.pending.iter().find(|r| r.id == id)
    }

    /// config.toml から配られたポリシーを取り込む (世代が変わったときだけ)。
    pub fn sync_policies(&mut self) {
        let (gen, list) = published_policies();
        if gen != self.policy_gen {
            self.policy_gen = gen;
            self.policies = list;
        }
    }

    /// 「常に拒否」のポリシーを 1 件でも持っているか。
    ///
    /// 自動YES の事前ゲートを回すかどうかの早期判定に使う。
    /// ポリシーを 1 件も書いていない既定構成では、画面テキストの取得すら
    /// 走らない (追加コスト 0)。
    pub fn has_deny_policy(&self) -> bool {
        self.policies
            .iter()
            .any(|p| p.decision == Decision::DenyAlways)
    }

    /// 画面テキストを 1 件投入する。
    ///
    /// - 同じセッションの同じ指紋なら [`Verdict::Duplicate`] (二重に積まない)
    /// - ポリシーが決着させたら [`Verdict::Decided`] (監査ログへ `policy` で記録)
    /// - それ以外は [`Verdict::Queued`]
    pub fn intake(
        &mut self,
        session_id: u64,
        agent_bin: Option<&str>,
        text: &str,
        signature: u64,
    ) -> Verdict {
        self.sync_policies();
        let key = (session_id, signature);
        if self.seen.contains(&key) {
            return Verdict::Duplicate;
        }
        self.remember(key);
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1).max(1);
        let req =
            ApprovalRequest::from_prompt(id, session_id, agent_bin, text, signature, now_secs());
        // ポリシー相談は**従来の自動YESより先**。ここで決着したら PTY へ即答する。
        let decision = resolve_and_consume(&mut self.policies, &req);
        if decision.allows() {
            self.log(&req, decision, Source::Policy);
            return Verdict::Decided {
                id,
                decision,
                reply: ReplyAction::Approve,
                note: NOTE_POLICY_ALLOW,
            };
        }
        if decision == Decision::DenyAlways {
            self.log(&req, decision, Source::Policy);
            return Verdict::Decided {
                id,
                decision,
                reply: ReplyAction::Deny,
                note: NOTE_POLICY_DENY,
            };
        }
        self.pending.push_back(req);
        while self.pending.len() > PENDING_CAP {
            self.pending.pop_front();
        }
        Verdict::Queued { id }
    }

    /// 従来の全自動YES (`pet_auto_yes`) を**このプロンプトに限って止める**か。
    ///
    /// 「常に拒否」ポリシーは自動YESより強くなければ意味がない。ところが
    /// `Session::scan_attention(true)` は検知と同時に YES を撃ってしまうため、
    /// 撃たせる前にここで相談する。画面テキストの取得は `text` クロージャ経由の
    /// 遅延評価で、ポリシーを持たない構成・間引き中は一切呼ばれない。
    pub fn auto_yes_blocked(
        &mut self,
        session_id: u64,
        agent_bin: Option<&str>,
        text: impl FnOnce() -> String,
    ) -> bool {
        self.sync_policies();
        if !self.has_deny_policy() {
            return false;
        }
        if let Some((at, blocked)) = self.gate_cache.get(&session_id) {
            if at.elapsed().as_millis() < PRE_GATE_INTERVAL_MS {
                return *blocked;
            }
        }
        let body = text();
        // 判定専用なのでキューへは積まない (ID も払い出さない)。
        let probe = ApprovalRequest::from_prompt(0, session_id, agent_bin, &body, 0, 0);
        let blocked = resolve_with(&self.policies, &probe) == Decision::DenyAlways;
        self.gate_cache
            .insert(session_id, (Instant::now(), blocked));
        blocked
    }

    /// 従来の全自動YES が応答した事実を監査ログへ残す (`source: auto_yes`)。
    pub fn log_auto_yes(&self, session_id: u64, agent_bin: Option<&str>, text: &str) {
        let req = ApprovalRequest::from_prompt(0, session_id, agent_bin, text, 0, now_secs());
        self.log(&req, Decision::AllowOnce, Source::AutoYes);
    }

    /// UI の 1 キーストロークを処理する。
    ///
    /// 対象 ID が既に無ければ空の [`Resolution`] を返す (二重押し対策)。
    #[allow(dead_code)]
    pub fn apply(&mut self, id: u64, cmd: Command) -> Resolution {
        let Some(pos) = self.pending.iter().position(|r| r.id == id) else {
            return Resolution::default();
        };
        let target = self.pending[pos].clone();
        let mut out = Resolution::default();
        // 「常に」系は権限昇格へは絶対に作らない。ここが PROMPT_NEVER と
        // 同じ立場をポリシー生成側に持ち込む栓。
        let want_always = matches!(
            cmd,
            Command::ApproveKindForAgentAlways | Command::DenyKindForAgentAlways
        );
        let always_ok = !target.never_auto;
        if want_always && !always_ok {
            out.refused_always = true;
        }
        // 何件を対象にするか。
        let bulk = matches!(
            cmd,
            Command::ApproveAllOfKind
                | Command::ApproveKindForAgentAlways
                | Command::DenyKindForAgentAlways
        );
        let approve = matches!(
            cmd,
            Command::Approve | Command::ApproveAllOfKind | Command::ApproveKindForAgentAlways
        );
        let action = if approve {
            ReplyAction::Approve
        } else {
            ReplyAction::Deny
        };
        let decision = if approve {
            Decision::AllowOnce
        } else {
            Decision::Ask
        };
        let mut taken: Vec<ApprovalRequest> = Vec::new();
        if bulk {
            // 同じ種別を一括処理する。「常に」系はエージェントも揃える
            // (別エージェントの同種まで巻き込まない)。
            let same_agent_only = want_always;
            let kind = target.kind;
            let agent = target.agent_bin.clone();
            let mut keep = VecDeque::with_capacity(self.pending.len());
            while let Some(r) = self.pending.pop_front() {
                let hit = r.kind == kind && (!same_agent_only || r.agent_bin == agent);
                if hit {
                    taken.push(r);
                } else {
                    keep.push_back(r);
                }
            }
            self.pending = keep;
        } else {
            taken.push(self.pending.remove(pos).expect("index checked above"));
        }
        for r in &taken {
            self.log(r, decision, Source::Manual);
            out.logged += 1;
            out.replies.push((r.agent_session_id, action));
        }
        if want_always && always_ok {
            let p = Policy {
                kind: target.kind,
                scope: Scope::Agent(target.agent_bin.clone()),
                decision: if approve {
                    Decision::AllowAlways
                } else {
                    Decision::DenyAlways
                },
            };
            self.policies.push(p.clone());
            out.policy = Some(p);
        }
        out
    }

    /// セッションが閉じたときに呼ぶ。そのセッションの承認待ち・重複記録を捨てる。
    pub fn forget_session(&mut self, session_id: u64) {
        self.pending.retain(|r| r.agent_session_id != session_id);
        self.seen.retain(|(sid, _)| *sid != session_id);
        self.seen_order.retain(|(sid, _)| *sid != session_id);
        self.gate_cache.remove(&session_id);
    }

    /// 監査ログへ 1 行書く。
    fn log(&self, req: &ApprovalRequest, decision: Decision, source: Source) {
        append_audit(
            &self.log_dir(),
            &AuditEntry {
                ts: if req.created_at > 0 {
                    req.created_at
                } else {
                    now_secs()
                },
                agent: req.agent_bin.clone(),
                kind: req.kind.as_str().to_string(),
                decision: decision.as_str().to_string(),
                source: source.as_str().to_string(),
                // 本文は入れない。summary は既に SUMMARY_CAP で切ってある。
                summary: trim_cap(&req.summary, SUMMARY_CAP),
            },
        );
    }

    /// 重複判定キーを覚える (上限つき FIFO)。
    fn remember(&mut self, key: (u64, u64)) {
        if self.seen.insert(key) {
            self.seen_order.push_back(key);
            while self.seen_order.len() > SEEN_CAP {
                if let Some(old) = self.seen_order.pop_front() {
                    self.seen.remove(&old);
                }
            }
        }
    }
}

/// ポリシーが許可したときにトーストへ出す文言 (`'static` が要る)。
pub const NOTE_POLICY_ALLOW: &str = "承認ポリシーにより自動承認";
/// ポリシーが拒否したときにトーストへ出す文言。
pub const NOTE_POLICY_DENY: &str = "承認ポリシーにより自動拒否";

// ══════════════════════════════════════════════════════════════════════
//  テスト
// ══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn req(kind: ApprovalKind, session: u64, agent: &str, path: Option<&str>) -> ApprovalRequest {
        ApprovalRequest {
            id: 1,
            agent_session_id: session,
            agent_bin: agent.into(),
            kind,
            summary: String::new(),
            detail: String::new(),
            raw_prompt_excerpt: String::new(),
            created_at: 0,
            path: path.map(PathBuf::from),
            never_auto: kind == ApprovalKind::Privilege,
            signature: 0,
        }
    }

    // ── 分類表 ────────────────────────────────────────────

    #[test]
    fn class_rules_are_lowercase() {
        // 判定は小文字化した画面テキストに対して行うので、表の ASCII は
        // 必ず小文字。大文字を書くと永久に一致しない。
        for r in CLASS_RULES {
            for n in r.all.iter().chain(r.any).chain(r.avoid) {
                assert_eq!(
                    *n,
                    n.to_lowercase(),
                    "分類表の needle は小文字で書くこと: {n}"
                );
            }
        }
    }

    #[test]
    fn classification_table() {
        // (画面テキスト, エージェント, 期待する種別)
        let cases: &[(&str, Option<&str>, ApprovalKind)] = &[
            // ── Antigravity (agy) の実測プロンプト ──
            (
                "Allow access to this file?\n> Yes, allow access\n  No",
                Some("agy"),
                ApprovalKind::FileRead,
            ),
            (
                "Allow creation of this file?\n> Yes, allow creation",
                Some("agy"),
                ApprovalKind::FileWrite,
            ),
            (
                "Accept this file edit?\n> Yes, accept this change",
                Some("agy"),
                ApprovalKind::FileWrite,
            ),
            (
                "Requesting permission\n> Yes, grant permission for\n[Use arrow keys to navigate, Enter to select]",
                Some("agy"),
                ApprovalKind::Other,
            ),
            (
                "Yes, I trust this folder",
                Some("agy"),
                ApprovalKind::Other,
            ),
            (
                "Zaivern needs a one-time admin escalation to configure the sandbox",
                Some("agy"),
                ApprovalKind::Privilege,
            ),
            (
                "Administrator privileges are required to continue",
                Some("agy"),
                ApprovalKind::Privilege,
            ),
            // ── Claude Code ──
            (
                "Bash command\n  cargo test --all\nDo you want to proceed?\n❯ 1. Yes",
                Some("claude"),
                ApprovalKind::ShellCommand,
            ),
            (
                "Do you want to make this edit to src/app.rs?\n❯ 1. Yes",
                Some("claude"),
                ApprovalKind::FileWrite,
            ),
            (
                "Bash command\n  rm -rf target/debug\nDo you want to proceed?",
                Some("claude"),
                // シェルではなく削除。承認者が見るべき危険はこちら。
                ApprovalKind::FileDelete,
            ),
            (
                "Bash command\n  git push origin main\nDo you want to proceed?",
                Some("claude"),
                ApprovalKind::GitOperation,
            ),
            (
                "Bash command\n  npm install left-pad\nDo you want to proceed?",
                Some("claude"),
                ApprovalKind::PackageInstall,
            ),
            (
                "WebFetch(https://example.com)\nDo you want to proceed?",
                Some("claude"),
                ApprovalKind::NetworkAccess,
            ),
            (
                "Bash command\n  sudo launchctl load /Library/LaunchDaemons/x.plist",
                Some("claude"),
                ApprovalKind::Privilege,
            ),
            // ── Codex ──
            (
                "Allow command: ls -la (y/n)",
                Some("codex"),
                ApprovalKind::ShellCommand,
            ),
            // ── 日本語 UI ──
            (
                "src/main.rs を削除しますか? [y/N]",
                None,
                ApprovalKind::FileDelete,
            ),
            (
                "設定ファイルを上書きしますか? (y/n)",
                None,
                ApprovalKind::FileWrite,
            ),
            (
                "このファイルを読み取ることを許可しますか",
                None,
                ApprovalKind::FileRead,
            ),
            (
                "次のコマンドを実行しますか: ls",
                None,
                ApprovalKind::ShellCommand,
            ),
            (
                "管理者権限が必要です。続行しますか",
                None,
                ApprovalKind::Privilege,
            ),
            // ── 表に無いものは必ず Other (狭い種別を推測しない) ──
            ("Would you like to proceed?", None, ApprovalKind::Other),
            ("", None, ApprovalKind::Other),
            ("なにかのメッセージ", None, ApprovalKind::Other),
        ];
        for (text, agent, want) in cases {
            assert_eq!(
                classify(text, *agent),
                *want,
                "分類が想定と違う: {:?}",
                text.lines().next().unwrap_or("")
            );
        }
    }

    #[test]
    fn classification_keeps_evidence_line() {
        let (kind, detail) = classify_detail(
            "Bash command\n  git commit -m wip\nDo you want to proceed?",
            Some("claude"),
        );
        assert_eq!(kind, ApprovalKind::GitOperation);
        assert!(detail.contains("git commit"), "根拠行が取れていない: {detail}");
    }

    #[test]
    fn every_kind_has_stable_id_roundtrip() {
        for k in ALL_KINDS {
            assert_eq!(ApprovalKind::from_id(k.as_str()), Some(*k));
        }
        assert_eq!(ApprovalKind::from_id("no_such_kind"), None);
    }

    // ── ポリシー解決 ──────────────────────────────────────

    #[test]
    fn policy_precedence_table() {
        let r = req(ApprovalKind::FileWrite, 7, "claude", Some("/repo/src/a.rs"));
        let global = Policy {
            kind: ApprovalKind::FileWrite,
            scope: Scope::Global,
            decision: Decision::AllowAlways,
        };
        let agent = Policy {
            kind: ApprovalKind::FileWrite,
            scope: Scope::Agent("claude".into()),
            decision: Decision::DenyAlways,
        };
        let path = Policy {
            kind: ApprovalKind::FileWrite,
            scope: Scope::PathPrefix("/repo".into()),
            decision: Decision::AllowAlways,
        };
        let deep = Policy {
            kind: ApprovalKind::FileWrite,
            scope: Scope::PathPrefix("/repo/src".into()),
            decision: Decision::DenyAlways,
        };
        let session = Policy {
            kind: ApprovalKind::FileWrite,
            scope: Scope::Session(7),
            decision: Decision::AllowOnce,
        };
        let other_session = Policy {
            kind: ApprovalKind::FileWrite,
            scope: Scope::Session(99),
            decision: Decision::DenyAlways,
        };
        let other_kind = Policy {
            kind: ApprovalKind::FileRead,
            scope: Scope::Session(7),
            decision: Decision::DenyAlways,
        };

        // 何も無ければ Ask
        assert_eq!(resolve_with(&[], &r), Decision::Ask);
        // 単独
        assert_eq!(
            resolve_with(std::slice::from_ref(&global), &r),
            Decision::AllowAlways
        );
        // Agent は Global より具体的
        assert_eq!(
            resolve_with(&[global.clone(), agent.clone()], &r),
            Decision::DenyAlways
        );
        // 並び順を逆にしても具体性で決まる
        assert_eq!(
            resolve_with(&[agent.clone(), global.clone()], &r),
            Decision::DenyAlways
        );
        // PathPrefix は Agent より具体的
        assert_eq!(
            resolve_with(&[agent.clone(), path.clone()], &r),
            Decision::AllowAlways
        );
        // 深いパスが浅いパスに勝つ
        assert_eq!(
            resolve_with(&[deep.clone(), path.clone()], &r),
            Decision::DenyAlways
        );
        // Session が最強
        assert_eq!(
            resolve_with(&[deep.clone(), path.clone(), session.clone()], &r),
            Decision::AllowOnce
        );
        // 範囲が当たらないポリシーは無視 (別セッション / 別種別)
        assert_eq!(
            resolve_with(&[other_session.clone(), other_kind.clone()], &r),
            Decision::Ask
        );
        // 同じ具体性が 2 件 → 後勝ち (あとから足した方で上書きできる)
        let g_deny = Policy {
            decision: Decision::DenyAlways,
            ..global.clone()
        };
        assert_eq!(
            resolve_with(&[global.clone(), g_deny.clone()], &r),
            Decision::DenyAlways
        );
        assert_eq!(
            resolve_with(&[g_deny, global.clone()], &r),
            Decision::AllowAlways
        );
        // パスが取れなかった要求には PathPrefix は当たらない (安全側)
        let no_path = req(ApprovalKind::FileWrite, 7, "claude", None);
        assert_eq!(
            resolve_with(&[deep, path], &no_path),
            Decision::Ask,
            "パス不明の要求へ PathPrefix ポリシーを当ててはいけない"
        );
        // Agent 範囲は別エージェントには当たらない
        let other_agent = req(ApprovalKind::FileWrite, 7, "agy", None);
        assert_eq!(resolve_with(&[agent], &other_agent), Decision::Ask);
    }

    #[test]
    fn allow_once_policy_is_consumed() {
        let r = req(ApprovalKind::FileRead, 1, "claude", None);
        let mut ps = vec![Policy {
            kind: ApprovalKind::FileRead,
            scope: Scope::Global,
            decision: Decision::AllowOnce,
        }];
        assert_eq!(resolve_and_consume(&mut ps, &r), Decision::AllowOnce);
        assert!(ps.is_empty(), "AllowOnce は使ったら消える");
        assert_eq!(resolve_and_consume(&mut ps, &r), Decision::Ask);
    }

    #[test]
    fn scope_toml_roundtrip() {
        let scopes = [
            Scope::Global,
            Scope::Agent("claude".into()),
            Scope::Session(42),
            Scope::PathPrefix(PathBuf::from("/repo/src")),
        ];
        for s in scopes {
            let (name, target) = s.to_toml();
            assert_eq!(Scope::from_toml(name, &target), Some(s.clone()), "{name}");
        }
        assert_eq!(Scope::from_toml("nope", "x"), None);
        assert_eq!(Scope::from_toml("agent", ""), None, "対象名なしは無効");
        for d in [
            Decision::Ask,
            Decision::AllowOnce,
            Decision::AllowAlways,
            Decision::DenyAlways,
        ] {
            assert_eq!(Decision::from_id(d.as_str()), Some(d));
        }
        assert_eq!(Decision::from_id("maybe"), None);
    }

    // ── 権限昇格は決して自動承認しない ────────────────────

    #[test]
    fn privilege_never_auto_beats_allow_always() {
        let dir = unique_temp_dir("zaivern-approvals-test", "privilege");
        let mut q = ApprovalQueue::in_dir(&dir);
        // 「全種別を常に許可」で塗りつぶす。権限昇格も含めて。
        q.policies = ALL_KINDS
            .iter()
            .map(|k| Policy {
                kind: *k,
                scope: Scope::Global,
                decision: Decision::AllowAlways,
            })
            .collect();
        // PROMPT_NEVER に載っている実測文言。
        let text = "Zaivern requires a one-time admin escalation to configure the sandbox\n> Yes, continue";
        let v = q.intake(3, Some("agy"), text, 111);
        assert_eq!(
            v,
            Verdict::Queued { id: 1 },
            "権限昇格はポリシーで自動承認してはいけない"
        );
        let r = q.get(1).expect("キューに積まれていない");
        assert_eq!(r.kind, ApprovalKind::Privilege);
        assert!(r.never_auto, "never_auto が立っていない");
        // 純関数レベルでも AllowAlways を跳ね返すこと。
        assert_eq!(
            resolve_with(&q.policies, r),
            Decision::Ask,
            "resolve_with が never_auto を無視している"
        );
        // 「以後ずっと許可」も作らせない — ただしこの 1 件は人手で通せる。
        let out = q.apply(1, Command::ApproveKindForAgentAlways);
        assert!(out.refused_always, "権限昇格へポリシーを作ってしまった");
        assert!(out.policy.is_none(), "権限昇格のポリシーが生成された");
        assert_eq!(out.replies, vec![(3, ReplyAction::Approve)]);
        // 事前ゲート (自動YES の抑止) 側でも許可へ倒れないこと。
        q.policies.push(Policy {
            kind: ApprovalKind::Privilege,
            scope: Scope::Global,
            decision: Decision::DenyAlways,
        });
        assert!(
            !q.auto_yes_blocked(3, Some("agy"), || text.to_string()),
            "never_auto の要求は常に Ask なので、遮断判定も false になる"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn prompt_never_marker_beats_policy_even_when_kind_is_narrow() {
        // 種別としては FileWrite に見えるが、画面に PROMPT_NEVER の目印が
        // あるので never_auto。ポリシーは効かない。
        let text = "Accept this file edit?\nAdministrator privileges are required";
        let r = ApprovalRequest::from_prompt(1, 1, Some("agy"), text, 0, 0);
        assert!(r.never_auto);
        let ps = vec![Policy {
            kind: r.kind,
            scope: Scope::Global,
            decision: Decision::AllowAlways,
        }];
        assert_eq!(resolve_with(&ps, &r), Decision::Ask);
    }

    // ── キュー ────────────────────────────────────────────

    #[test]
    fn queue_dedups_on_session_and_signature() {
        let dir = unique_temp_dir("zaivern-approvals-test", "dedup");
        let mut q = ApprovalQueue::in_dir(&dir);
        let text = "Allow access to this file?\n> Yes, allow access";
        assert_eq!(q.intake(1, Some("agy"), text, 77), Verdict::Queued { id: 1 });
        // 同じセッション・同じ指紋 → 二重に積まない
        assert_eq!(q.intake(1, Some("agy"), text, 77), Verdict::Duplicate);
        assert_eq!(q.pending_len(), 1);
        // 別セッションなら別物
        assert_eq!(q.intake(2, Some("agy"), text, 77), Verdict::Queued { id: 2 });
        // 同じセッションでも指紋が違えば別物 (連続承認の 2 個目)
        assert_eq!(q.intake(1, Some("agy"), text, 78), Verdict::Queued { id: 3 });
        assert_eq!(q.pending_len(), 3);
        // セッションを閉じたら、その分だけ消える
        q.forget_session(1);
        assert_eq!(q.pending_len(), 1);
        // 閉じた後は同じ指紋でもまた積める
        assert_eq!(q.intake(1, Some("agy"), text, 77), Verdict::Queued { id: 4 });
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn one_keystroke_commands() {
        let dir = unique_temp_dir("zaivern-approvals-test", "keystroke");
        let mut q = ApprovalQueue::in_dir(&dir);
        let read = "Allow access to this file?\n> Yes, allow access";
        let write = "Accept this file edit?\n> Yes, accept this change";
        q.intake(1, Some("agy"), read, 1);
        q.intake(2, Some("agy"), read, 2);
        q.intake(3, Some("claude"), read, 3);
        q.intake(4, Some("agy"), write, 4);
        assert_eq!(q.pending_len(), 4);
        assert_eq!(q.pending_of_kind(ApprovalKind::FileRead), 3);

        // 1 件だけ承認
        let out = q.apply(1, Command::Approve);
        assert_eq!(out.replies, vec![(1, ReplyAction::Approve)]);
        assert!(out.policy.is_none());
        assert_eq!(q.pending_len(), 3);
        // 消えた ID をもう一度押しても何も起きない
        assert_eq!(q.apply(1, Command::Approve), Resolution::default());

        // 同種を一括承認 (エージェントは問わない → agy と claude の両方)
        let out = q.apply(2, Command::ApproveAllOfKind);
        assert_eq!(out.replies.len(), 2);
        assert!(out.policy.is_none());
        assert_eq!(q.pending_len(), 1, "FileWrite の 1 件だけ残る");

        // 以後ずっと許可 → ポリシーが生える
        let out = q.apply(4, Command::ApproveKindForAgentAlways);
        assert!(!out.refused_always);
        let p = out.policy.expect("ポリシーが作られていない");
        assert_eq!(p.kind, ApprovalKind::FileWrite);
        assert_eq!(p.scope, Scope::Agent("agy".into()));
        assert_eq!(p.decision, Decision::AllowAlways);
        assert!(q.policies.contains(&p), "キュー側にも反映されていない");
        assert_eq!(q.pending_len(), 0);

        // 以後は同じ種別が自動で通る
        let v = q.intake(9, Some("agy"), write, 9);
        assert!(
            matches!(
                v,
                Verdict::Decided {
                    decision: Decision::AllowAlways,
                    reply: ReplyAction::Approve,
                    ..
                }
            ),
            "ポリシーが効いていない: {v:?}"
        );
        // 別エージェントには効かない
        assert!(matches!(
            q.intake(10, Some("claude"), write, 10),
            Verdict::Queued { .. }
        ));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn deny_always_command_creates_policy_and_denies() {
        let dir = unique_temp_dir("zaivern-approvals-test", "deny-always");
        let mut q = ApprovalQueue::in_dir(&dir);
        let text = "Bash command\n  curl https://example.com\nDo you want to proceed?";
        q.intake(1, Some("claude"), text, 1);
        let out = q.apply(1, Command::DenyKindForAgentAlways);
        assert_eq!(out.replies, vec![(1, ReplyAction::Deny)]);
        let p = out.policy.expect("ポリシーが無い");
        assert_eq!(p.kind, ApprovalKind::NetworkAccess);
        assert_eq!(p.decision, Decision::DenyAlways);
        // 次は即拒否される
        assert!(matches!(
            q.intake(1, Some("claude"), text, 2),
            Verdict::Decided {
                reply: ReplyAction::Deny,
                ..
            }
        ));
        // 自動YES の事前ゲートも遮断へ倒れる
        assert!(q.has_deny_policy());
        assert!(q.auto_yes_blocked(1, Some("claude"), || text.to_string()));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn auto_yes_gate_is_free_without_deny_policies() {
        let mut q = ApprovalQueue::in_dir(unique_temp_dir("zaivern-approvals-test", "gate"));
        // 拒否ポリシーが 1 件も無ければ画面テキストの取得すら走らない。
        let called = std::cell::Cell::new(false);
        let blocked = q.auto_yes_blocked(1, Some("claude"), || {
            called.set(true);
            String::new()
        });
        assert!(!blocked);
        assert!(!called.get(), "ポリシー無しなのに画面を読んでいる");
    }

    // ── 監査ログ ──────────────────────────────────────────

    #[test]
    fn audit_append_rotate_and_tail_roundtrip() {
        let dir = unique_temp_dir("zaivern-approvals-test", "audit");
        let e = |n: u64| AuditEntry {
            ts: 1_700_000_000 + n,
            agent: "claude".into(),
            kind: ApprovalKind::FileWrite.as_str().into(),
            decision: Decision::AllowAlways.as_str().into(),
            source: Source::Policy.as_str().into(),
            summary: format!("ファイル書き込み — src/a{n}.rs"),
        };
        append_audit(&dir, &e(1));
        append_audit(&dir, &e(2));
        let back = read_audit_tail(&dir, 64 * 1024);
        assert_eq!(back, vec![e(1), e(2)], "追記した順に読み戻せない");

        // ローテート: 閾値を超えたら .old へ寄り、tail は .old → 本体の順に繋ぐ。
        // 埋め草も行指向にする (実物のログは 1 行 = 1 JSON なので)。
        let path = audit_path(&dir);
        let filler = "x\n".repeat(AUDIT_MAX_BYTES as usize / 2 + 1);
        std::fs::write(&path, filler).unwrap();
        append_audit(&dir, &e(3));
        assert!(
            path.with_extension("jsonl.old").exists(),
            "ローテートされていない"
        );
        assert!(
            std::fs::metadata(&path).unwrap().len() < AUDIT_MAX_BYTES,
            "ローテート後の本体が肥大したまま"
        );
        // 壊れた行 (filler) は読み飛ばし、正しい行だけ返る。
        let back = read_audit_tail(&dir, 64 * 1024);
        assert_eq!(back, vec![e(3)]);

        // cap を小さくすると末尾だけ読む (先頭の欠けた行は落ちる)。
        std::fs::remove_file(path.with_extension("jsonl.old")).unwrap();
        std::fs::remove_file(&path).unwrap();
        for n in 0..50 {
            append_audit(&dir, &e(n));
        }
        let tail = read_audit_tail(&dir, 400);
        assert!(!tail.is_empty() && tail.len() < 50, "末尾読みが効いていない");
        assert_eq!(
            tail.last().map(|x| x.ts),
            Some(1_700_000_000 + 49),
            "末尾が最新行ではない"
        );
        // 無いディレクトリでも落ちない
        let ghost = dir.join("no-such-dir");
        assert!(read_audit_tail(&ghost, 1024).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn audit_never_records_prompt_body() {
        let dir = unique_temp_dir("zaivern-approvals-test", "no-secret");
        let mut q = ApprovalQueue::in_dir(&dir);
        // プロンプト本文に秘密が貼られている想定。
        let secret = "sk-live-DO-NOT-LEAK-0123456789";
        let text = format!(
            "Accept this file edit?\n  .env\n  API_KEY={secret}\n> Yes, accept this change"
        );
        q.intake(1, Some("agy"), &text, 1);
        q.apply(1, Command::Approve);
        let raw = std::fs::read_to_string(audit_path(&dir)).unwrap();
        assert!(
            !raw.contains(secret),
            "監査ログにプロンプト本文が漏れている: {raw}"
        );
        let back = read_audit_tail(&dir, 64 * 1024);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].source, "manual");
        assert!(back[0].summary.chars().count() <= SUMMARY_CAP + 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn audit_sources_are_distinguished() {
        let dir = unique_temp_dir("zaivern-approvals-test", "sources");
        let mut q = ApprovalQueue::in_dir(&dir);
        let text = "Allow access to this file?\n> Yes, allow access";
        // manual
        q.intake(1, Some("agy"), text, 1);
        q.apply(1, Command::Approve);
        // policy
        q.policies.push(Policy {
            kind: ApprovalKind::FileRead,
            scope: Scope::Global,
            decision: Decision::AllowAlways,
        });
        q.intake(1, Some("agy"), text, 2);
        // auto_yes (従来の全自動YES が撃った分)
        q.log_auto_yes(1, Some("agy"), text);
        let got: Vec<String> = read_audit_tail(&dir, 64 * 1024)
            .into_iter()
            .map(|e| e.source)
            .collect();
        assert_eq!(got, vec!["manual", "policy", "auto_yes"]);
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 要求の組み立て ────────────────────────────────────

    #[test]
    fn request_truncates_excerpt_and_finds_path() {
        let long = "あ".repeat(EXCERPT_CAP * 2);
        let text = format!("Do you want to make this edit to /repo/src/app.rs?\n{long}");
        let r = ApprovalRequest::from_prompt(5, 9, Some("claude"), &text, 123, 42);
        assert_eq!(r.id, 5);
        assert_eq!(r.agent_session_id, 9);
        assert_eq!(r.agent_bin, "claude");
        assert_eq!(r.kind, ApprovalKind::FileWrite);
        assert_eq!(r.created_at, 42);
        assert_eq!(r.signature, 123);
        assert!(!r.never_auto);
        assert!(
            r.raw_prompt_excerpt.chars().count() <= EXCERPT_CAP + 1,
            "抜粋が切られていない"
        );
        assert_eq!(r.path.as_deref(), Some(Path::new("/repo/src/app.rs")));
        // URL はパスとして拾わない
        let r2 = ApprovalRequest::from_prompt(
            1,
            1,
            Some("claude"),
            "WebFetch(https://example.com/a/b)",
            0,
            0,
        );
        assert_eq!(r2.kind, ApprovalKind::NetworkAccess);
        assert_eq!(r2.path, None, "URL をパス扱いしてはいけない");
    }

    #[test]
    fn trim_cap_is_char_safe() {
        assert_eq!(trim_cap("あいうえお", 3), "あいう…");
        assert_eq!(trim_cap("abc", 10), "abc");
        assert_eq!(trim_cap("", 3), "");
    }
}
