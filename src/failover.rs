//! レート制限時のアカウント自動フェイルオーバー。
//!
//! 夜間の放置運用で「片方の枠が尽きた瞬間に全部止まる」のを避けるための機構。
//! **既定は無効**で、ユーザーが明示的に有効化したときだけ働く。
//!
//! ## 責務の切り分け
//!
//! | 層 | 置き場所 | 性質 |
//! |---|---|---|
//! | 検知 (使用率・上限イベント) | [`crate::coordinator::quota`] / [`crate::terminal::detect_rate_limit`] | 既存。**複製しない** |
//! | 信号の格付けと候補選び | このモジュールの純関数 ([`classify_signal`] / [`explain_failover`]) | 純粋・テーブルテスト済み |
//! | 段の管理と履歴 | [`Failover`] | 可変。時刻は引数で注入する |
//! | 実際の起動・プロンプト引き継ぎ | `app.rs` (既存の `AgentManager::launch` / `agents::apply_resume`) | 既存経路を再利用 |
//!
//! ## 設計原則 4 — 画面から推測しない
//!
//! 何を根拠に「レート制限だ」と判断したかを [`Signal`] で持ち回り、UI に必ず出す。
//! 段は上から順に降りる: 構造化プロトコル > ベンダー提供フック > 状態ファイル >
//! 画面スクレイプ。最下段 ([`Signal::Screen`]) だけが根拠のときは
//! [`confirm_screen`] の裏取り (単語列一致 + 連続一致 + **出力が進んでいない**) を
//! 通らないと切り替えない。UI 側も「推定」と明示する。
//!
//! ## 状態機械
//!
//! `検知 → 候補選定 → 切替 → 再開 → 検証` の 5 段 ([`Stage`])。
//! [`Stage::step`] が 1..=5 を返すので、UI は「今どの段か」をそのまま描ける。
//!
//! ## 安全側の既定
//!
//! - **現行セッションは殺さない。** 新しいプロファイルで別セッションを起動するだけ。
//!   終了済みセッションへ kill を撃つ経路もここには無い。
//! - 同じ候補を無限に試さない: セッションごとの切替上限 ([`FailoverConfig::max_switches`])、
//!   候補ごとの試行上限 ([`FailoverConfig::max_attempts`])、指数バックオフ ([`backoff`])、
//!   そして「このフェイルオーバー連鎖で既に試したプリセットは二度と選ばない」
//!   ([`FailingSession::tried`]) の 4 重で止める。

use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::i18n::{tr, trf};

/// 保持する切替履歴の上限 (メモリを有界に保つ)。
pub const RECORD_CAP: usize = 32;

// ── 設定 ───────────────────────────────────────────────────────────────

/// `[failover]` セクション。**`enabled` の既定は false**。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct FailoverConfig {
    /// 自動フェイルオーバーを行うか。**既定は無効**。
    pub enabled: bool,
    /// 1 セッションあたりの連鎖切替の上限。
    pub max_switches: u8,
    /// 同じ候補 (枠) を試す回数の上限。
    pub max_attempts: u8,
    /// 失敗した枠を寝かせる基準時間 (秒)。指数で伸びる。
    pub cooldown_secs: u64,
    /// クールダウンの上限 (秒)。
    pub max_cooldown_secs: u64,
    /// 切替後、「動いている」と見なすまでの観察時間 (秒)。
    pub verify_secs: u64,
    /// 画面由来の検知を信じるまでに必要な連続一致回数。
    pub min_screen_hits: u8,
}

impl Default for FailoverConfig {
    fn default() -> Self {
        Self {
            // 自動で別アカウントへ移るのは「勝手に課金先が変わる」ことでもある。
            // 既定は必ず無効 — ユーザーが明示的に入れたときだけ働く。
            enabled: false,
            max_switches: 3,
            max_attempts: 2,
            cooldown_secs: 5 * 60,
            max_cooldown_secs: 60 * 60,
            verify_secs: 90,
            min_screen_hits: 2,
        }
    }
}

// ── 信号 (設計原則 4 の段) ─────────────────────────────────────────────

/// 「レート制限だ」と判断した根拠。上ほど信頼できる。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Signal {
    /// 1 段目: 構造化プロトコル (機械可読な終了理由)。現状これを出す CLI は無い。
    Protocol,
    /// 2 段目: ベンダー提供フック (statusline / hook 経由で渡ってくる値)。
    VendorHook,
    /// 3 段目: ベンダーの状態ファイル ([`crate::coordinator::quota`] の実測値)。
    StateFile,
    /// 4 段目 (最下段): 画面スクレイプ。**推定**。単独では信じない。
    Screen,
}

impl Signal {
    /// 何段目か (1 が最上位)。UI に「今どの段の情報で動いているか」を出すため。
    pub fn rung(self) -> u8 {
        match self {
            Signal::Protocol => 1,
            Signal::VendorHook => 2,
            Signal::StateFile => 3,
            Signal::Screen => 4,
        }
    }

    /// 推定にすぎないか (UI は必ず「推定」と描く)。
    pub fn is_estimate(self) -> bool {
        matches!(self, Signal::Screen)
    }

    /// UI 表示用のラベル。
    pub fn label(self) -> String {
        let name = match self {
            Signal::Protocol => tr("構造化プロトコル"),
            Signal::VendorHook => tr("ベンダー提供フック"),
            Signal::StateFile => tr("状態ファイル (実測)"),
            Signal::Screen => tr("画面スクレイプ (推定)"),
        };
        trf(
            "{rung}段目: {name}",
            &[("rung", self.rung().to_string()), ("name", name)],
        )
    }
}

/// 段を上から順に並べた表。UI に「どの段まで降りたか」を出すために使う
/// (設計原則 4: 最下段はベンダーの CLI 更新のたびに壊れるので、常に見せる)。
pub const LADDER: [Signal; 4] = [
    Signal::Protocol,
    Signal::VendorHook,
    Signal::StateFile,
    Signal::Screen,
];

/// 手元にある信号のうち**いちばん上の段**を選ぶ。
///
/// 設計原則 4 の「優先順位を意識的に降りる」をそのまま関数にしたもの。
/// 候補が空なら `None` (= そもそも検知していない)。
pub fn classify_signal(available: &[Signal]) -> Option<Signal> {
    available.iter().copied().min()
}

/// 画面由来の検知を信じてよいか (最下段の裏取り)。
///
/// 3 つ全部を満たしたときだけ true:
/// 1. パスらしいトークンを落とした行が [`crate::terminal::detect_rate_limit`] に
///    当たる (検出器は**再利用**し、パターンを複製しない)
/// 2. 同じ警告が `min_hits` 回以上続けて見えている
/// 3. **生の出力が進んでいない** (スピナーの再描画を進捗と見なさない前提の値を渡すこと)
pub fn confirm_screen(line: &str, hits: u8, output_advanced: bool, min_hits: u8) -> bool {
    if output_advanced || hits < min_hits.max(1) {
        return false;
    }
    crate::terminal::detect_rate_limit(&strip_pathlike(line)).is_some()
}

/// パスらしいトークンを落とす。`Read(src/rate_limit_reached.rs)` のような
/// **ファイル名**を上限警告と読み違えないための前処理。
fn strip_pathlike(line: &str) -> String {
    line.split_whitespace()
        .filter(|t| {
            let t = t.trim_matches(|c: char| !c.is_alphanumeric() && c != '/' && c != '\\' && c != '.');
            // 区切り記号を含む = パス。拡張子付きの単独トークンもパス扱いにする。
            if t.contains('/') || t.contains('\\') {
                return false;
            }
            !matches!(t.rsplit_once('.'), Some((head, ext))
                if !head.is_empty()
                    && !ext.is_empty()
                    && ext.chars().all(|c| c.is_ascii_alphanumeric()))
        })
        .collect::<Vec<_>>()
        .join(" ")
}

// ── 候補 ───────────────────────────────────────────────────────────────

/// 切替先の候補 1 件。
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// プリセット名 (`config::AgentPreset::name`)。起動はこの名前で引き直す。
    pub preset: String,
    /// CLI の実行ファイル名 (`agents.rs` のカタログと同じキー)。
    pub bin: String,
    /// 枠の共有鍵 ([`account_key`])。**同じ鍵は同じ枠を食う** = 同時に枯れる。
    pub account: String,
    /// この枠が使えない期限。`None` なら今すぐ使える。
    pub cooldown_until: Option<Instant>,
    /// この枠で既に失敗した回数。
    pub attempts: u8,
}

/// レート制限に当たったセッションの情報。
///
/// `coordinator::SessionInfo` (タスク割り当て用の id/state/caps) とは別物。
/// こちらは「どの CLI のどの枠が枯れたか」だけを持つ。
#[derive(Clone, Debug, PartialEq)]
pub struct FailingSession {
    pub session_id: u64,
    /// いま動いていたプリセット名。
    pub preset: String,
    /// いま動いていた CLI の bin 名。
    pub bin: String,
    /// 枯れた枠の共有鍵。
    pub account: String,
    /// 何を根拠に「枯れた」と判断したか。
    pub signal: Signal,
    /// このセッションで既に行った切替の回数。
    pub switches: u8,
    /// この連鎖で既に試したプリセット名。**二度と選ばない**。
    pub tried: Vec<String>,
}

/// 候補を選んだ理由 (UI の説明文になる)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickReason {
    /// 同じ CLI の別プロファイル (別アカウント)。いちばん素直な移行先。
    SameAgentOtherAccount,
    /// 別の CLI。
    OtherAgent,
}

impl PickReason {
    pub fn label(self) -> String {
        match self {
            PickReason::SameAgentOtherAccount => tr("同じ CLI の別プロファイル"),
            PickReason::OtherAgent => tr("別の CLI"),
        }
    }
}

/// 切替計画。
#[derive(Clone, Debug, PartialEq)]
pub struct FailoverPlan {
    /// `candidates` の添字。
    pub candidate: usize,
    pub preset: String,
    pub bin: String,
    pub account: String,
    pub reason: PickReason,
}

/// 切り替えなかった理由。UI に「なぜ動かないか」を必ず出すために型で持つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refusal {
    /// 設定で無効。
    Disabled,
    /// 候補が 1 つも無い。
    NoCandidates,
    /// 同じ枠を食う候補しかない (一緒に枯れているので移っても無意味)。
    SameAccountOnly,
    /// この連鎖で全部試し終えた。
    AllTried,
    /// 候補ごとの試行上限に達した。
    AttemptsExhausted { max: u8 },
    /// 空いている候補が全部クールダウン中。
    AllCoolingDown,
    /// このセッションの切替上限に達した。
    SwitchesExhausted { max: u8 },
    /// 切替先も上限に当たった。連鎖させず、ここで人へ渡す。
    TargetAlsoLimited,
    /// 切替先が立ち上がらなかった / すぐ落ちた。
    TargetFailed,
}

impl Refusal {
    pub fn label(self) -> String {
        match self {
            Refusal::Disabled => tr("自動フェイルオーバーは無効です"),
            Refusal::NoCandidates => tr("切替先の候補がありません"),
            Refusal::SameAccountOnly => tr("同じ枠を使う候補しかありません (一緒に枯れています)"),
            Refusal::AllTried => tr("この連鎖で候補を全て試し終えました"),
            Refusal::AttemptsExhausted { max } => trf(
                "候補ごとの試行上限 ({max} 回) に到達しました",
                &[("max", max.to_string())],
            ),
            Refusal::AllCoolingDown => tr("候補は全てクールダウン中です"),
            Refusal::SwitchesExhausted { max } => trf(
                "このセッションの切替上限 ({max} 回) に到達しました",
                &[("max", max.to_string())],
            ),
            Refusal::TargetAlsoLimited => tr("切替先も上限に当たりました (ここで人へ渡します)"),
            Refusal::TargetFailed => tr("切替先が立ち上がりませんでした"),
        }
    }
}

/// 切替先を選ぶ**純関数**。既定の [`FailoverConfig`] の上限で判定する。
///
/// 上限を設定から変えたいときは [`explain_failover`] を使う (理由も返る)。
pub fn pick_failover(
    current: &FailingSession,
    candidates: &[Candidate],
    now: Instant,
) -> Option<FailoverPlan> {
    explain_failover(current, candidates, now, &FailoverConfig::default()).ok()
}

/// [`pick_failover`] の理由つき版。**有効/無効はここでは見ない**
/// (設定ゲートは [`Failover::plan`] の役目。純関数は順序付けだけを担う)。
///
/// 順序: 同一 CLI の別プロファイル → 別 CLI。同順位は試行回数の少ない順、
/// それも同じなら候補配列の並び順 (= プリセットの並び) で決める。
pub fn explain_failover(
    current: &FailingSession,
    candidates: &[Candidate],
    now: Instant,
    cfg: &FailoverConfig,
) -> Result<FailoverPlan, Refusal> {
    if current.switches >= cfg.max_switches {
        return Err(Refusal::SwitchesExhausted {
            max: cfg.max_switches,
        });
    }
    // 自分自身は候補にしない (同じ枠を掴み直すだけ)。
    let others: Vec<(usize, &Candidate)> = candidates
        .iter()
        .enumerate()
        .filter(|(_, c)| c.preset != current.preset)
        .collect();
    if others.is_empty() {
        return Err(Refusal::NoCandidates);
    }
    // 同じ枠は一緒に枯れているので移っても意味がない。
    let other_account: Vec<(usize, &Candidate)> = others
        .into_iter()
        .filter(|(_, c)| c.account != current.account)
        .collect();
    if other_account.is_empty() {
        return Err(Refusal::SameAccountOnly);
    }
    // この連鎖で試したものは二度と選ばない (無限ループの元栓)。
    let untried: Vec<(usize, &Candidate)> = other_account
        .into_iter()
        .filter(|(_, c)| !current.tried.iter().any(|t| t == &c.preset))
        .collect();
    if untried.is_empty() {
        return Err(Refusal::AllTried);
    }
    let under_cap: Vec<(usize, &Candidate)> = untried
        .into_iter()
        .filter(|(_, c)| c.attempts < cfg.max_attempts)
        .collect();
    if under_cap.is_empty() {
        return Err(Refusal::AttemptsExhausted {
            max: cfg.max_attempts,
        });
    }
    // クールダウン期限を過ぎた候補はここで復活する (`> now` だけを落とす)。
    let mut usable: Vec<(usize, &Candidate)> = under_cap
        .into_iter()
        .filter(|(_, c)| c.cooldown_until.map(|t| t <= now).unwrap_or(true))
        .collect();
    if usable.is_empty() {
        return Err(Refusal::AllCoolingDown);
    }
    usable.sort_by_key(|(i, c)| (u8::from(c.bin != current.bin), c.attempts, *i));
    let (idx, c) = usable[0];
    Ok(FailoverPlan {
        candidate: idx,
        preset: c.preset.clone(),
        bin: c.bin.clone(),
        account: c.account.clone(),
        reason: if c.bin == current.bin {
            PickReason::SameAgentOtherAccount
        } else {
            PickReason::OtherAgent
        },
    })
}

/// 失敗回数に対するクールダウン (指数バックオフ、上限つき)。
pub fn backoff(attempts: u8, cfg: &FailoverConfig) -> Duration {
    let base = cfg.cooldown_secs.max(1);
    let cap = cfg.max_cooldown_secs.max(base);
    let secs = base.saturating_mul(1u64 << attempts.min(20));
    Duration::from_secs(secs.min(cap))
}

/// プリセットが使う「枠」の鍵。同じ鍵なら同じプランを食い合う。
///
/// ベンダー名 ([`crate::coordinator::quota`] の記述子) と、プリセットが指定している
/// アカウント系環境変数 ([`crate::agents::account_env_keys`]) の指紋を繋いだもの。
/// **値そのもの (API キー等) は決して持ち回らない** — ハッシュだけを載せる。
pub fn account_key(bin: &str, env: &HashMap<String, String>) -> String {
    let vendor = crate::coordinator::quota::descriptor(bin)
        .map(|d| d.account)
        .unwrap_or(bin);
    let mut parts: Vec<String> = crate::agents::account_env_keys(bin)
        .iter()
        .filter_map(|k| {
            let v = env.get(*k)?.trim();
            (!v.is_empty()).then(|| format!("{k}={v}"))
        })
        .collect();
    if parts.is_empty() {
        return format!("{vendor}:default");
    }
    parts.sort();
    let mut h = DefaultHasher::new();
    parts.join("\u{1f}").hash(&mut h);
    format!("{vendor}:{:08x}", (h.finish() >> 32) as u32)
}

/// 切替先で「前の会話の続き」を再開できるか。
///
/// できるのは **同じ CLI** で、かつ会話の保存先を引っ越す環境変数
/// ([`crate::agents::session_store_env_keys`]) が一致しているときだけ。
/// 別 CLI や別の設定ディレクトリには過去の会話が無いので、再開指定を付けると
/// 「空のセッションを再開しようとして失敗する」だけになる。
pub fn can_resume(
    from_bin: &str,
    from_env: &HashMap<String, String>,
    to_bin: &str,
    to_env: &HashMap<String, String>,
) -> bool {
    if from_bin != to_bin {
        return false;
    }
    crate::agents::session_store_env_keys(from_bin)
        .iter()
        .all(|k| from_env.get(*k).map(|s| s.trim()) == to_env.get(*k).map(|s| s.trim()))
}

/// プリセット一覧から候補を組み立てる。
///
/// 素のシェル (コマンドが空) と、カタログに無い CLI は枠を食わないので候補にしない。
pub fn candidates_from_presets(
    presets: &[crate::config::AgentPreset],
    cooldowns: &HashMap<String, Instant>,
    attempts: &HashMap<String, u8>,
    now: Instant,
) -> Vec<Candidate> {
    let mut out = Vec::new();
    for p in presets {
        if p.command.trim().is_empty() {
            continue;
        }
        let Some(spec) = crate::agents::spec_for_command(&p.command) else {
            continue;
        };
        let account = account_key(spec.bin, &p.env);
        out.push(Candidate {
            preset: p.name.clone(),
            bin: spec.bin.to_string(),
            cooldown_until: cooldowns.get(&account).copied().filter(|t| *t > now),
            attempts: attempts.get(&account).copied().unwrap_or(0),
            account,
        });
    }
    out
}

// ── 状態機械 ───────────────────────────────────────────────────────────

/// フェイルオーバーの段。`検知 → 候補選定 → 切替 → 再開 → 検証`。
#[derive(Clone, Debug, PartialEq)]
pub enum Stage {
    /// ① 検知 — 何を根拠に気づいたか。
    Detected { signal: Signal, evidence: String },
    /// ② 候補選定。
    Picking { signal: Signal },
    /// ③ 切替 — 新しいプロファイルでセッションを起動する。
    Switching { to: String },
    /// ④ 再開 — プロンプトを引き継いで走らせる。
    Resuming { to: String, session: u64 },
    /// ⑤ 検証 — 新しい側が本当に進んでいるか見届ける。
    Verifying { to: String, session: u64 },
    /// 完了。
    Done { to: String },
    /// 打ち切り (理由つき)。
    GaveUp { reason: Refusal },
}

impl Stage {
    /// 5 段のうち何段目か。完了/打ち切りは 5 (= 最後まで行った) を返す。
    pub fn step(&self) -> u8 {
        match self {
            Stage::Detected { .. } => 1,
            Stage::Picking { .. } => 2,
            Stage::Switching { .. } => 3,
            Stage::Resuming { .. } => 4,
            Stage::Verifying { .. } | Stage::Done { .. } | Stage::GaveUp { .. } => 5,
        }
    }

    /// これ以上進まない段か (UI はここで色を落とす)。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Stage::Done { .. } | Stage::GaveUp { .. })
    }

    /// UI に出す 1 行。「今どの段にいるか」と根拠を必ず含める。
    pub fn label(&self) -> String {
        match self {
            Stage::Detected { signal, evidence } => trf(
                "①検知 — {signal} / {evidence}",
                &[
                    ("signal", signal.label()),
                    ("evidence", crate::notify::truncate_chars(evidence, 60)),
                ],
            ),
            Stage::Picking { signal } => trf(
                "②候補選定 — {signal}",
                &[("signal", signal.label())],
            ),
            Stage::Switching { to } => {
                trf("③切替 — {to} を起動中", &[("to", to.clone())])
            }
            Stage::Resuming { to, .. } => {
                trf("④再開 — {to} へプロンプトを引き継ぎ中", &[("to", to.clone())])
            }
            Stage::Verifying { to, .. } => {
                trf("⑤検証 — {to} が進んでいるか確認中", &[("to", to.clone())])
            }
            Stage::Done { to } => trf("✅ 切替完了 — {to}", &[("to", to.clone())]),
            Stage::GaveUp { reason } => {
                trf("⏹ 打ち切り — {why}", &[("why", reason.label())])
            }
        }
    }
}

/// 段 + いつその段に入ったか。
#[derive(Clone, Debug)]
pub struct StageAt {
    pub stage: Stage,
    pub at: Instant,
}

/// 切替 1 件の記録 (UI の履歴表示用)。
#[derive(Clone, Debug)]
pub struct FailoverRecord {
    pub at: Instant,
    pub from: String,
    pub to: String,
    pub signal: Signal,
    pub reason: PickReason,
}

impl FailoverRecord {
    /// この切替からの経過時間 (UI の「◯分前」表示用)。
    pub fn ago(&self, now: Instant) -> Duration {
        now.saturating_duration_since(self.at)
    }

    /// 通知・トーストに出す 1 行の説明。
    pub fn line(&self) -> String {
        trf(
            "{from} がレート制限 → {to} へ切替 ({why} / 根拠 {signal})",
            &[
                ("from", self.from.clone()),
                ("to", self.to.clone()),
                ("why", self.reason.label()),
                ("signal", self.signal.label()),
            ],
        )
    }
}

/// フェイルオーバーの実行状態。時刻は必ず引数で受け取る (テスト可能に保つ)。
#[derive(Default)]
pub struct Failover {
    cfg: FailoverConfig,
    /// 枠 → クールダウン期限。
    cooldowns: HashMap<String, Instant>,
    /// 枠 → 失敗回数。
    attempts: HashMap<String, u8>,
    /// 元セッション → 現在の段。
    stages: HashMap<u64, StageAt>,
    /// 元セッション → 切替回数。
    switches: HashMap<u64, u8>,
    /// 元セッション → この連鎖で試したプリセット名。
    tried: HashMap<u64, Vec<String>>,
    log: Vec<FailoverRecord>,
}

impl Failover {
    pub fn new(cfg: FailoverConfig) -> Self {
        Self {
            cfg,
            ..Default::default()
        }
    }

    pub fn config(&self) -> &FailoverConfig {
        &self.cfg
    }

    /// 設定を差し替える (config 再読み込み時)。
    pub fn set_config(&mut self, cfg: FailoverConfig) {
        self.cfg = cfg;
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// 有効/無効を切り替えて、切り替えた後の値を返す。
    pub fn set_enabled(&mut self, on: bool) -> bool {
        self.cfg.enabled = on;
        on
    }

    /// ① 検知を記録する。既に切替の途中 (終端でない段) なら二重に始めない。
    /// 新しく検知の段へ入ったら true。
    pub fn note_detected(
        &mut self,
        session: u64,
        signal: Signal,
        evidence: &str,
        now: Instant,
    ) -> bool {
        if self
            .stages
            .get(&session)
            .is_some_and(|s| !s.stage.is_terminal())
        {
            return false;
        }
        self.stages.insert(
            session,
            StageAt {
                stage: Stage::Detected {
                    signal,
                    evidence: evidence.trim().to_string(),
                },
                at: now,
            },
        );
        true
    }

    /// ② 候補選定 → ③ 切替。無効なら [`Refusal::Disabled`] で終わる。
    ///
    /// 成功しても**ここでは何も起動しない**。呼び出し側 (app.rs) が既存の
    /// 起動経路を使い、結果を [`Failover::note_switched`] で戻す。
    pub fn plan(
        &mut self,
        current: &FailingSession,
        candidates: &[Candidate],
        now: Instant,
    ) -> Result<FailoverPlan, Refusal> {
        if !self.cfg.enabled {
            return Err(Refusal::Disabled);
        }
        self.set_stage(
            current.session_id,
            Stage::Picking {
                signal: current.signal,
            },
            now,
        );
        match explain_failover(current, candidates, now, &self.cfg) {
            Ok(plan) => {
                self.set_stage(
                    current.session_id,
                    Stage::Switching {
                        to: plan.preset.clone(),
                    },
                    now,
                );
                Ok(plan)
            }
            Err(reason) => {
                self.set_stage(current.session_id, Stage::GaveUp { reason }, now);
                Err(reason)
            }
        }
    }

    /// ④ 切替が済んだ (新セッションが立った)。履歴へ 1 行残す。
    pub fn note_switched(
        &mut self,
        from_session: u64,
        from: &str,
        plan: &FailoverPlan,
        new_session: u64,
        signal: Signal,
        now: Instant,
    ) {
        *self.switches.entry(from_session).or_insert(0) += 1;
        self.tried
            .entry(from_session)
            .or_default()
            .push(plan.preset.clone());
        self.set_stage(
            from_session,
            Stage::Resuming {
                to: plan.preset.clone(),
                session: new_session,
            },
            now,
        );
        self.log.push(FailoverRecord {
            at: now,
            from: from.to_string(),
            to: plan.preset.clone(),
            signal,
            reason: plan.reason,
        });
        if self.log.len() > RECORD_CAP {
            let cut = self.log.len() - RECORD_CAP;
            self.log.drain(..cut);
        }
    }

    /// ⑤ 検証へ進む (プロンプトを渡し終えた)。
    pub fn note_resumed(&mut self, from_session: u64, now: Instant) {
        let next = match self.stages.get(&from_session).map(|s| &s.stage) {
            Some(Stage::Resuming { to, session }) => Stage::Verifying {
                to: to.clone(),
                session: *session,
            },
            _ => return,
        };
        self.set_stage(from_session, next, now);
    }

    /// 検証に合格した (切替先が動いている)。
    pub fn note_verified(&mut self, from_session: u64, now: Instant) {
        let next = match self.stages.get(&from_session).map(|s| &s.stage) {
            Some(Stage::Verifying { to, .. }) => Stage::Done { to: to.clone() },
            _ => return,
        };
        self.set_stage(from_session, next, now);
    }

    /// 打ち切る (理由つき)。連鎖の途中で「これ以上は人が決めること」になった場合。
    pub fn note_gave_up(&mut self, from_session: u64, reason: Refusal, now: Instant) {
        self.set_stage(from_session, Stage::GaveUp { reason }, now);
    }

    /// 切替先も枯れていた / 起動に失敗した。枠を寝かせて次回の候補から外す。
    pub fn note_failed(&mut self, account: &str, now: Instant) {
        let n = self.attempts.entry(account.to_string()).or_insert(0);
        *n = n.saturating_add(1);
        let wait = backoff(n.saturating_sub(1), &self.cfg);
        self.cooldowns.insert(account.to_string(), now + wait);
    }

    /// この段に入ってからの経過が検証時間を超えたか (⑤ の合否判定に使う)。
    pub fn verify_elapsed(&self, from_session: u64, now: Instant) -> bool {
        self.stages
            .get(&from_session)
            .map(|s| now.duration_since(s.at) >= Duration::from_secs(self.cfg.verify_secs))
            .unwrap_or(false)
    }

    pub fn stage_of(&self, session: u64) -> Option<&Stage> {
        self.stages.get(&session).map(|s| &s.stage)
    }

    /// 進行中 (終端でない) の段のうち、いちばん新しいもの。ステータス表示用。
    pub fn active(&self) -> Option<(u64, &Stage)> {
        self.stages
            .iter()
            .filter(|(_, s)| !s.stage.is_terminal())
            .max_by_key(|(_, s)| s.at)
            .map(|(id, s)| (*id, &s.stage))
    }

    /// 進行中のセッション ID 一覧 (毎フレームの駆動用。順序は不定)。
    pub fn in_flight(&self) -> Vec<u64> {
        self.stages
            .iter()
            .filter(|(_, s)| !s.stage.is_terminal())
            .map(|(id, _)| *id)
            .collect()
    }

    pub fn records(&self) -> &[FailoverRecord] {
        &self.log
    }

    /// このセッションで既に試したプリセット名。
    pub fn tried_for(&self, session: u64) -> &[String] {
        self.tried.get(&session).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn switches_for(&self, session: u64) -> u8 {
        self.switches.get(&session).copied().unwrap_or(0)
    }

    /// 枠のクールダウン表 (候補組み立てに渡す)。
    pub fn cooldowns(&self) -> &HashMap<String, Instant> {
        &self.cooldowns
    }

    /// 枠ごとの失敗回数 (候補組み立てに渡す)。
    pub fn attempt_counts(&self) -> &HashMap<String, u8> {
        &self.attempts
    }

    /// セッションが閉じられたら忘れる (ID は再利用され得るので残さない)。
    pub fn forget_session(&mut self, session: u64) {
        self.stages.remove(&session);
        self.switches.remove(&session);
        self.tried.remove(&session);
    }

    fn set_stage(&mut self, session: u64, stage: Stage, at: Instant) {
        self.stages.insert(session, StageAt { stage, at });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(preset: &str, bin: &str, account: &str) -> Candidate {
        Candidate {
            preset: preset.into(),
            bin: bin.into(),
            account: account.into(),
            cooldown_until: None,
            attempts: 0,
        }
    }

    fn failing(preset: &str, bin: &str, account: &str) -> FailingSession {
        FailingSession {
            session_id: 1,
            preset: preset.into(),
            bin: bin.into(),
            account: account.into(),
            signal: Signal::StateFile,
            switches: 0,
            tried: Vec::new(),
        }
    }

    // ── pick_failover のテーブルテスト ──────────────────────────────

    #[test]
    fn pick_failover_decision_table() {
        let now = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");

        // (説明, 候補, 期待する結果)
        let cases: Vec<(&str, Vec<Candidate>, Result<(&str, PickReason), Refusal>)> = vec![
            ("候補ゼロ", vec![], Err(Refusal::NoCandidates)),
            (
                "自分だけ = 候補ゼロ扱い",
                vec![cand("Claude Code", "claude", "anthropic:default")],
                Err(Refusal::NoCandidates),
            ),
            (
                "同じ枠しかない (一緒に枯れている)",
                vec![cand("Claude Code 全自動", "claude", "anthropic:default")],
                Err(Refusal::SameAccountOnly),
            ),
            (
                "1 つだけ空き",
                vec![cand("Claude 仕事用", "claude", "anthropic:aa11bb22")],
                Ok(("Claude 仕事用", PickReason::SameAgentOtherAccount)),
            ),
            (
                "同一 CLI の別プロファイルが別 CLI より先",
                vec![
                    cand("Codex", "codex", "openai:default"),
                    cand("Claude 仕事用", "claude", "anthropic:aa11bb22"),
                ],
                Ok(("Claude 仕事用", PickReason::SameAgentOtherAccount)),
            ),
            (
                "同一 CLI が無ければ別 CLI へ",
                vec![cand("Codex", "codex", "openai:default")],
                Ok(("Codex", PickReason::OtherAgent)),
            ),
            (
                "別 CLI が複数ならプリセットの並び順",
                vec![
                    cand("Codex", "codex", "openai:default"),
                    cand("Gemini", "gemini", "google:default"),
                ],
                Ok(("Codex", PickReason::OtherAgent)),
            ),
        ];

        for (name, candidates, want) in cases {
            let got = explain_failover(&cur, &candidates, now, &FailoverConfig::default());
            match (want, got) {
                (Ok((preset, reason)), Ok(plan)) => {
                    assert_eq!(plan.preset, preset, "{name}");
                    assert_eq!(plan.reason, reason, "{name}");
                    assert_eq!(candidates[plan.candidate].preset, preset, "{name}: 添字");
                }
                (Err(want), Err(got)) => assert_eq!(want, got, "{name}"),
                (w, g) => panic!("{name}: 期待 {w:?} / 実際 {g:?}"),
            }
        }
    }

    #[test]
    fn pick_failover_all_cooling_down() {
        let now = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");
        let mut c = vec![
            cand("Codex", "codex", "openai:default"),
            cand("Claude 仕事用", "claude", "anthropic:aa11"),
        ];
        for x in &mut c {
            x.cooldown_until = Some(now + Duration::from_secs(60));
        }
        assert_eq!(
            explain_failover(&cur, &c, now, &FailoverConfig::default()),
            Err(Refusal::AllCoolingDown)
        );

        // クールダウン中でないものが 1 つでもあればそれが選ばれる。
        c[0].cooldown_until = None;
        let plan = explain_failover(&cur, &c, now, &FailoverConfig::default()).expect("選べる");
        assert_eq!(plan.preset, "Codex");
    }

    #[test]
    fn cooldown_expires_and_candidate_comes_back() {
        let now = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");
        let mut c = vec![cand("Codex", "codex", "openai:default")];
        c[0].cooldown_until = Some(now + Duration::from_secs(300));

        assert_eq!(
            explain_failover(&cur, &c, now, &FailoverConfig::default()),
            Err(Refusal::AllCoolingDown),
            "リセット前は選ばない"
        );
        // リセット時刻ちょうど / 過ぎた後は復活する。
        let at_reset = now + Duration::from_secs(300);
        assert!(explain_failover(&cur, &c, at_reset, &FailoverConfig::default()).is_ok());
        let after = now + Duration::from_secs(301);
        assert_eq!(
            explain_failover(&cur, &c, after, &FailoverConfig::default())
                .expect("復活")
                .preset,
            "Codex"
        );
    }

    #[test]
    fn already_tried_candidate_is_never_retried() {
        let now = Instant::now();
        let mut cur = failing("Claude Code", "claude", "anthropic:default");
        let c = vec![
            cand("Codex", "codex", "openai:default"),
            cand("Gemini", "gemini", "google:default"),
        ];
        let first = explain_failover(&cur, &c, now, &FailoverConfig::default()).expect("1 回目");
        assert_eq!(first.preset, "Codex");

        // 直前に失敗した候補は tried に入る → 二度と選ばれない。
        cur.tried.push(first.preset.clone());
        cur.switches = 1;
        let second = explain_failover(&cur, &c, now, &FailoverConfig::default()).expect("2 回目");
        assert_eq!(second.preset, "Gemini", "直前に失敗した候補を再試行しない");

        cur.tried.push(second.preset.clone());
        cur.switches = 2;
        assert_eq!(
            explain_failover(&cur, &c, now, &FailoverConfig::default()),
            Err(Refusal::AllTried),
            "全部試したら止まる (無限ループしない)"
        );
    }

    #[test]
    fn switch_and_attempt_caps_stop_the_loop() {
        let now = Instant::now();
        let cfg = FailoverConfig::default();
        let mut cur = failing("Claude Code", "claude", "anthropic:default");
        let mut c = vec![cand("Codex", "codex", "openai:default")];

        cur.switches = cfg.max_switches;
        assert_eq!(
            explain_failover(&cur, &c, now, &cfg),
            Err(Refusal::SwitchesExhausted {
                max: cfg.max_switches
            })
        );

        cur.switches = 0;
        c[0].attempts = cfg.max_attempts;
        assert_eq!(
            explain_failover(&cur, &c, now, &cfg),
            Err(Refusal::AttemptsExhausted {
                max: cfg.max_attempts
            })
        );
    }

    #[test]
    fn fewer_attempts_wins_within_the_same_tier() {
        let now = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");
        let mut c = vec![
            cand("Codex A", "codex", "openai:a"),
            cand("Codex B", "codex", "openai:b"),
        ];
        c[0].attempts = 1;
        let plan = explain_failover(&cur, &c, now, &FailoverConfig::default()).expect("選べる");
        assert_eq!(plan.preset, "Codex B", "失敗の少ない枠を先に試す");
    }

    #[test]
    fn pick_failover_matches_explain_with_defaults() {
        let now = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");
        let c = vec![cand("Codex", "codex", "openai:default")];
        assert_eq!(
            pick_failover(&cur, &c, now).map(|p| p.preset),
            Some("Codex".to_string())
        );
        assert!(pick_failover(&cur, &[], now).is_none());
    }

    // ── 信号の格付け ────────────────────────────────────────────────

    #[test]
    fn classify_signal_descends_the_ladder() {
        let cases: Vec<(Vec<Signal>, Option<Signal>)> = vec![
            (vec![], None),
            (vec![Signal::Screen], Some(Signal::Screen)),
            (
                vec![Signal::Screen, Signal::StateFile],
                Some(Signal::StateFile),
            ),
            (
                vec![Signal::Screen, Signal::StateFile, Signal::VendorHook],
                Some(Signal::VendorHook),
            ),
            (
                vec![Signal::Screen, Signal::Protocol, Signal::StateFile],
                Some(Signal::Protocol),
            ),
        ];
        for (have, want) in cases {
            assert_eq!(classify_signal(&have), want, "{have:?}");
        }
        assert!(Signal::Screen.is_estimate());
        assert!(!Signal::StateFile.is_estimate());
        assert_eq!(Signal::Protocol.rung(), 1);
        assert_eq!(Signal::Screen.rung(), 4);
    }

    #[test]
    fn confirm_screen_needs_corroboration() {
        let line = "5-hour limit reached ∙ resets 3am";
        // (説明, 行, 連続一致, 出力が進んだか, 期待)
        let cases: [(&str, &str, u8, bool, bool); 6] = [
            ("裏取り済み", line, 2, false, true),
            ("1 回だけ = 信じない", line, 1, false, false),
            ("出力が進んでいる = 別の理由", line, 5, true, false),
            ("上限警告ではない行", "普通のビルド出力です", 5, false, false),
            (
                "パスらしいトークンだけ",
                "Read(src/usage_limit_reached.rs)",
                5,
                false,
                false,
            ),
            (
                "パス混じりでも本文が上限警告なら拾う",
                "src/foo.rs · usage limit reached",
                2,
                false,
                true,
            ),
        ];
        for (name, l, hits, advanced, want) in cases {
            assert_eq!(confirm_screen(l, hits, advanced, 2), want, "{name}");
        }
    }

    // ── バックオフ ──────────────────────────────────────────────────

    #[test]
    fn backoff_grows_and_is_capped() {
        let cfg = FailoverConfig {
            cooldown_secs: 60,
            max_cooldown_secs: 600,
            ..Default::default()
        };
        let want = [60u64, 120, 240, 480, 600, 600, 600];
        for (n, w) in want.iter().enumerate() {
            assert_eq!(backoff(n as u8, &cfg).as_secs(), *w, "attempts={n}");
        }
        // 極端な値でも溢れない。
        assert_eq!(backoff(u8::MAX, &cfg).as_secs(), 600);
    }

    // ── 枠の鍵 ──────────────────────────────────────────────────────

    #[test]
    fn account_key_separates_profiles_without_leaking_values() {
        let plain: HashMap<String, String> = HashMap::new();
        let mut work = HashMap::new();
        work.insert("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-work".to_string());
        let mut secret = HashMap::new();
        secret.insert("ANTHROPIC_API_KEY".to_string(), "sk-super-secret".to_string());

        let a = account_key("claude", &plain);
        let b = account_key("claude", &work);
        let c = account_key("claude", &secret);
        assert_eq!(a, "anthropic:default");
        assert_ne!(a, b, "プロファイルを分けたら別の枠");
        assert_ne!(b, c);
        assert!(!c.contains("sk-super-secret"), "秘密の値を鍵へ載せない: {c}");
        // 同じ内容なら安定 (毎回同じ鍵になる)。
        assert_eq!(b, account_key("claude", &work));
        // 関係ない環境変数は枠を分けない。
        let mut noise = work.clone();
        noise.insert("PATH".to_string(), "/nowhere".to_string());
        assert_eq!(b, account_key("claude", &noise));
        // カタログに無い CLI でもベンダー名 (= bin) で成立する。
        assert_eq!(account_key("no-such-cli", &plain), "no-such-cli:default");
    }

    #[test]
    fn resume_only_when_the_conversation_store_is_the_same() {
        let plain: HashMap<String, String> = HashMap::new();
        let mut work = HashMap::new();
        work.insert("CLAUDE_CONFIG_DIR".to_string(), "~/.claude-work".to_string());
        let mut key_only = HashMap::new();
        key_only.insert("ANTHROPIC_API_KEY".to_string(), "sk-x".to_string());

        // 同じ CLI・同じ保存先 (API キーだけ違う) → 会話は続けられる。
        assert!(can_resume("claude", &plain, "claude", &key_only));
        // 設定ディレクトリが違う → 過去の会話は向こうに無い。
        assert!(!can_resume("claude", &plain, "claude", &work));
        assert!(!can_resume("claude", &work, "claude", &plain));
        // 別 CLI は常に不可。
        assert!(!can_resume("claude", &plain, "codex", &plain));
        // 保存先を引っ越す変数が無い CLI は、同じ CLI なら再開できる扱い。
        assert!(can_resume("gemini", &plain, "gemini", &key_only));
    }

    #[test]
    fn candidates_skip_shell_and_unknown_clis() {
        use crate::config::AgentPreset;
        let presets = vec![
            AgentPreset {
                name: "Claude Code".into(),
                command: "claude".into(),
                ..Default::default()
            },
            AgentPreset {
                name: "Shell".into(),
                command: String::new(),
                ..Default::default()
            },
            AgentPreset {
                name: "謎".into(),
                command: "definitely-not-an-agent-cli".into(),
                ..Default::default()
            },
        ];
        let now = Instant::now();
        let got = candidates_from_presets(&presets, &HashMap::new(), &HashMap::new(), now);
        assert_eq!(got.len(), 1, "素のシェルと未知の CLI は候補にしない");
        assert_eq!(got[0].preset, "Claude Code");
        assert_eq!(got[0].bin, "claude");
        assert_eq!(got[0].attempts, 0);
        assert!(got[0].cooldown_until.is_none());

        // クールダウン/試行回数は枠の鍵で引かれる。
        let mut cd = HashMap::new();
        cd.insert(got[0].account.clone(), now + Duration::from_secs(30));
        let mut at = HashMap::new();
        at.insert(got[0].account.clone(), 2u8);
        let got = candidates_from_presets(&presets, &cd, &at, now);
        assert_eq!(got[0].attempts, 2);
        assert!(got[0].cooldown_until.is_some());
        // 期限を過ぎていれば None に畳まれる。
        let later = now + Duration::from_secs(31);
        let got = candidates_from_presets(&presets, &cd, &at, later);
        assert!(got[0].cooldown_until.is_none());
    }

    // ── 状態機械 ────────────────────────────────────────────────────

    #[test]
    fn disabled_by_default_and_refuses_to_plan() {
        let mut f = Failover::default();
        assert!(!f.enabled(), "既定は必ず無効");
        let now = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");
        let c = vec![cand("Codex", "codex", "openai:default")];
        assert_eq!(f.plan(&cur, &c, now), Err(Refusal::Disabled));
        assert!(f.stage_of(cur.session_id).is_none(), "無効なら段も作らない");

        assert!(f.set_enabled(true));
        assert!(f.plan(&cur, &c, now).is_ok());
    }

    #[test]
    fn stage_machine_walks_all_five_steps() {
        let mut f = Failover::new(FailoverConfig {
            enabled: true,
            verify_secs: 10,
            ..Default::default()
        });
        let t0 = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");
        let c = vec![cand("Codex", "codex", "openai:default")];

        assert!(f.note_detected(1, Signal::Screen, "usage limit reached", t0));
        assert_eq!(f.stage_of(1).map(Stage::step), Some(1));
        assert!(
            !f.note_detected(1, Signal::Screen, "again", t0),
            "進行中は二重に始めない"
        );

        let plan = f.plan(&cur, &c, t0).expect("候補あり");
        assert_eq!(f.stage_of(1).map(Stage::step), Some(3));

        f.note_switched(1, "Claude Code", &plan, 9, Signal::Screen, t0);
        assert_eq!(f.stage_of(1).map(Stage::step), Some(4));
        assert_eq!(f.switches_for(1), 1);
        assert_eq!(f.tried_for(1).len(), 1);
        assert_eq!(f.tried_for(1)[0], "Codex");
        assert_eq!(f.records().len(), 1);
        assert!(f.records()[0].line().contains("Codex"));

        f.note_resumed(1, t0);
        assert!(matches!(f.stage_of(1), Some(Stage::Verifying { .. })));
        assert!(!f.verify_elapsed(1, t0));
        assert!(f.verify_elapsed(1, t0 + Duration::from_secs(10)));

        f.note_verified(1, t0 + Duration::from_secs(10));
        assert!(matches!(f.stage_of(1), Some(Stage::Done { .. })));
        assert!(f.stage_of(1).unwrap().is_terminal());
        assert!(f.active().is_none(), "終わった段は進行中に数えない");
        assert!(f.in_flight().is_empty());

        f.forget_session(1);
        assert!(f.stage_of(1).is_none());
        assert_eq!(f.switches_for(1), 0);
    }

    #[test]
    fn refusal_lands_in_gave_up_stage() {
        let mut f = Failover::new(FailoverConfig {
            enabled: true,
            ..Default::default()
        });
        let now = Instant::now();
        let cur = failing("Claude Code", "claude", "anthropic:default");
        assert_eq!(f.plan(&cur, &[], now), Err(Refusal::NoCandidates));
        match f.stage_of(1) {
            Some(Stage::GaveUp { reason }) => assert_eq!(*reason, Refusal::NoCandidates),
            other => panic!("打ち切りの段になるはず: {other:?}"),
        }
        assert!(f.active().is_none());
    }

    #[test]
    fn failed_account_gets_exponential_cooldown() {
        let mut f = Failover::new(FailoverConfig {
            enabled: true,
            cooldown_secs: 60,
            max_cooldown_secs: 600,
            ..Default::default()
        });
        let t0 = Instant::now();
        f.note_failed("openai:default", t0);
        assert_eq!(f.attempt_counts().get("openai:default"), Some(&1u8));
        assert_eq!(
            f.cooldowns().get("openai:default").copied(),
            Some(t0 + Duration::from_secs(60))
        );
        f.note_failed("openai:default", t0);
        assert_eq!(
            f.cooldowns().get("openai:default").copied(),
            Some(t0 + Duration::from_secs(120)),
            "2 回目は倍待つ"
        );
    }

    #[test]
    fn records_are_bounded() {
        let mut f = Failover::new(FailoverConfig {
            enabled: true,
            max_switches: u8::MAX,
            ..Default::default()
        });
        let t0 = Instant::now();
        let plan = FailoverPlan {
            candidate: 0,
            preset: "Codex".into(),
            bin: "codex".into(),
            account: "openai:default".into(),
            reason: PickReason::OtherAgent,
        };
        for i in 0..(RECORD_CAP as u64 + 10) {
            f.note_switched(i, "Claude Code", &plan, 100 + i, Signal::StateFile, t0);
        }
        assert_eq!(f.records().len(), RECORD_CAP);
    }

    #[test]
    fn active_reports_the_newest_running_stage() {
        let mut f = Failover::new(FailoverConfig {
            enabled: true,
            ..Default::default()
        });
        let t0 = Instant::now();
        f.note_detected(1, Signal::StateFile, "a", t0);
        f.note_detected(2, Signal::Screen, "b", t0 + Duration::from_secs(5));
        assert_eq!(f.active().map(|(id, _)| id), Some(2));
        assert_eq!(f.in_flight().len(), 2);
    }

    #[test]
    fn stage_labels_carry_the_rung() {
        let s = Stage::Detected {
            signal: Signal::Screen,
            evidence: "usage limit reached".into(),
        };
        let l = s.label();
        assert!(l.contains('①'), "{l}");
        assert!(l.contains("推定"), "画面由来なら推定と明示する: {l}");
        assert!(l.contains("4段目"), "{l}");

        let m = Stage::Detected {
            signal: Signal::StateFile,
            evidence: "codex rollout".into(),
        }
        .label();
        assert!(m.contains("実測"), "{m}");
        assert!(!m.contains("推定"), "{m}");
    }

    #[test]
    fn every_refusal_has_a_message() {
        for r in [
            Refusal::Disabled,
            Refusal::NoCandidates,
            Refusal::SameAccountOnly,
            Refusal::AllTried,
            Refusal::AttemptsExhausted { max: 2 },
            Refusal::AllCoolingDown,
            Refusal::SwitchesExhausted { max: 3 },
            Refusal::TargetAlsoLimited,
            Refusal::TargetFailed,
        ] {
            assert!(!r.label().trim().is_empty(), "{r:?}");
        }
        for s in [
            Signal::Protocol,
            Signal::VendorHook,
            Signal::StateFile,
            Signal::Screen,
        ] {
            assert!(!s.label().trim().is_empty(), "{s:?}");
        }
        for p in [PickReason::SameAgentOtherAccount, PickReason::OtherAgent] {
            assert!(!p.label().trim().is_empty(), "{p:?}");
        }
    }
}
