//! Cloud Execution の**基本データモデル** — Provider にも Transport にも
//! Agent にも依存しない、この層の共通語彙。
//!
//! ## 設計上の約束
//!
//! * **ID に意味を持たせない。** Provider 名や IP を [`TargetId`] にすると、
//!   同じ機械が別 Provider へ移った日に同一性が壊れる (§7)。
//! * **任意のシェル文字列を持たない。** `ssh_opts: String` のような欄を
//!   [`TargetEndpoint`] へ置くと、そこがコマンド注入の入口になる。
//!   接続情報は `host` / `user` / `port` / `identity_file` に**構造化**して持つ (§8)。
//! * **Provider 名で分岐しない。** Scheduler が見るのは [`Capabilities`] と
//!   [`TargetCapacity`] だけ (§9 / §32)。
//! * **秘密を持たない。** この層のどの型にもトークンや秘密鍵の**中身**は
//!   入らない。入るのは環境変数の**名前**とファイルの**パス**だけ (§16 / §40)。

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::redact::redact;

// ───────────────────────────── ID ─────────────────────────────

/// Provider の識別子 (`local` / `static-ssh` / `hetzner` のような**種別**ではなく、
/// 利用者が付けた**プロファイル名**を持つ。例: `hetzner-eu`)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProviderId(pub String);

impl ProviderId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ProviderId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 実行先の識別子。**Provider 名や IP から作らない** (§7)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TargetId(pub String);

impl TargetId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for TargetId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 仕事の識別子。ブランチ名とディレクトリ名になるので、**識別子として安全な
/// 文字だけ**で作る ([`ids::new_id`] が保証する)。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct JobId(pub String);

impl JobId {
    pub fn new(s: impl Into<String>) -> Self {
        Self(s.into())
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 衝突しない ID を作る道具。
///
/// **乱数 crate を足さない。** 要るのは「同じ機械の同じ瞬間でも重複しない」
/// ことと「時刻順に並ぶ」ことだけで、暗号強度は要らない。
/// 時刻 (ミリ秒) + プロセス ID + 単調増加カウンタを Crockford Base32 で畳む。
pub mod ids {
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Crockford Base32 の英数字 (紛らわしい `I` `L` `O` `U` を含まない)。
    /// ブランチ名・ディレクトリ名・URL のどれに置いても意味を変えない字だけ。
    const ALPHABET: &[u8] = b"0123456789abcdefghjkmnpqrstvwxyz";

    fn base32(mut v: u128, len: usize) -> String {
        let mut buf = vec![b'0'; len];
        for slot in buf.iter_mut().rev() {
            *slot = ALPHABET[(v & 0x1f) as usize];
            v >>= 5;
        }
        String::from_utf8(buf).expect("base32 は ASCII")
    }

    /// `<接頭辞>-<時刻><機械><連番>` 形式の ID。時刻順に並ぶ。
    pub fn new_id(prefix: &str) -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let millis = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let pid = u128::from(std::process::id());
        let seq = u128::from(SEQ.fetch_add(1, Ordering::Relaxed));
        format!(
            "{prefix}{}{}{}",
            base32(millis, 9),
            base32(pid, 4),
            base32(seq, 3)
        )
    }
}

// ─────────────────────────── 能力 ───────────────────────────

/// OS の系統。**v1 で実行先として正式に支えるのは Linux だけ**だが、
/// 型としては 3 つとも持つ (Zaivern 自身はどの OS からでも操作できる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsFamily {
    Linux,
    MacOS,
    Windows,
    Unknown,
}

impl OsFamily {
    /// `uname -s` の出力から起こす。
    pub fn from_uname(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "linux" => Self::Linux,
            "darwin" => Self::MacOS,
            x if x.starts_with("mingw") || x.starts_with("msys") || x.starts_with("cygwin") => {
                Self::Windows
            }
            _ => Self::Unknown,
        }
    }

    /// いまこの Zaivern が動いている OS。
    pub fn host() -> Self {
        if cfg!(target_os = "linux") {
            Self::Linux
        } else if cfg!(target_os = "macos") {
            Self::MacOS
        } else if cfg!(windows) {
            Self::Windows
        } else {
            Self::Unknown
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::MacOS => "macos",
            Self::Windows => "windows",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "linux" => Some(Self::Linux),
            "macos" | "darwin" => Some(Self::MacOS),
            "windows" | "win" => Some(Self::Windows),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// CPU アーキテクチャ。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Architecture {
    X86_64,
    Aarch64,
    Unknown,
}

impl Architecture {
    /// `uname -m` の出力から起こす。
    pub fn from_uname(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "x86_64" | "amd64" => Self::X86_64,
            "aarch64" | "arm64" => Self::Aarch64,
            _ => Self::Unknown,
        }
    }

    pub fn host() -> Self {
        if cfg!(target_arch = "x86_64") {
            Self::X86_64
        } else if cfg!(target_arch = "aarch64") {
            Self::Aarch64
        } else {
            Self::Unknown
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
            Self::Unknown => "unknown",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "x86_64" | "amd64" => Some(Self::X86_64),
            "aarch64" | "arm64" => Some(Self::Aarch64),
            "unknown" => Some(Self::Unknown),
            _ => None,
        }
    }
}

/// GPU の能力。v1 では**要る / 要らない**の判定にしか使わないが、
/// 型を最初から持たせておく (後で GPU Provider を足すときに型が変わらない)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuCapability {
    pub model: String,
    pub memory_mib: Option<u64>,
    pub count: u16,
}

/// 実行先が「何をできるか」。**Scheduler が見るのはここだけ**で、
/// Provider 名は 1 バイトも見ない (§9)。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capabilities {
    pub os: OsFamily,
    pub arch: Architecture,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_cores: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk_mib: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gpu: Vec<GpuCapability>,
    /// 使える道具 (`git` / `docker` …)。**エージェント名は入れない** —
    /// ここへ入れると、この層がエージェントを知ることになる。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub tools: BTreeSet<String>,
    /// 利用者が付けた自由な札 (`gpu` / `eu` / `big` …)。
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub labels: BTreeSet<String>,
}

impl Default for Capabilities {
    fn default() -> Self {
        Self {
            os: OsFamily::Unknown,
            arch: Architecture::Unknown,
            cpu_cores: None,
            memory_mib: None,
            disk_mib: None,
            gpu: Vec::new(),
            tools: BTreeSet::new(),
            labels: BTreeSet::new(),
        }
    }
}

impl Capabilities {
    /// この Zaivern が動いている機械の能力 (分かる範囲で)。
    pub fn host() -> Self {
        Self {
            os: OsFamily::host(),
            arch: Architecture::host(),
            cpu_cores: std::thread::available_parallelism()
                .ok()
                .map(|n| n.get().min(u16::MAX as usize) as u16),
            ..Self::default()
        }
    }
}

/// 「この仕事にはどんな機械が要るか」。**Provider 名を書く欄は無い** (§10)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExecutionRequirements {
    pub os: Option<OsFamily>,
    pub arch: Option<Architecture>,
    pub min_cpu_cores: Option<u16>,
    pub min_memory_mib: Option<u64>,
    pub requires_gpu: bool,
    pub required_tools: BTreeSet<String>,
    pub labels: BTreeSet<String>,
    /// 利用者が名指しした実行先 (あれば最優先。ただし能力と空きは必ず見る)。
    pub preferred: Option<TargetId>,
    /// 手元とリモートのどちらを先に見るか。**能力と空きで絞った後の
    /// 並べ替えにしか効かない** — 好みで要求を満たさない実行先は選ばれない。
    pub prefer: super::scheduler::Prefer,
}

// ─────────────────────── 実行先 ───────────────────────

/// どうやってコマンドを届けるか。**Provider ではない** (§12)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransportKind {
    Local,
    Ssh,
}

impl TransportKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Ssh => "ssh",
        }
    }
}

/// 接続先。**任意のシェル文字列を持たせない** (§8)。
///
/// `ssh_opts: String` のような欄をここへ置くと、設定ファイルに書かれた文字が
/// そのままコマンド行へ流れる = 注入の入口になる。必要な項目は構造化して持つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum TargetEndpoint {
    Local,
    Ssh {
        host: String,
        user: String,
        port: u16,
        /// 秘密鍵の**パス**だけ。中身は保存しない (§16)。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity_file: Option<PathBuf>,
    },
}

impl TargetEndpoint {
    /// 画面と一覧に出す 1 行 (**秘密は含まない**)。
    pub fn summary(&self) -> String {
        match self {
            Self::Local => "local".to_string(),
            Self::Ssh {
                host, user, port, ..
            } => {
                if *port == 22 {
                    format!("{user}@{host}")
                } else {
                    format!("{user}@{host}:{port}")
                }
            }
        }
    }
}

/// 実行先の生死。**Scheduler へ渡してよいのは [`TargetLifecycle::Ready`] だけ** (§50)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TargetLifecycle {
    /// まだ一度も確かめていない。
    Unknown,
    /// Provider が作っている最中 (SSH はまだ開いていない)。
    Provisioning,
    /// 接続できることを確かめた。
    Ready,
    /// 新しい仕事を載せない (走っているものは終わらせる)。
    Draining,
    /// 止まっている / 消えた。
    Stopped,
    /// 壊れている (理由付き)。
    Failed,
}

impl TargetLifecycle {
    pub fn id(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Provisioning => "provisioning",
            Self::Ready => "ready",
            Self::Draining => "draining",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// 同時に何本まで載せるか。**1 台 = 1 エージェントに固定しない** (§31)。
///
/// v1 では `max_jobs` を利用者が決める。CPU / RAM からの自動推論を**強制しない**
/// (推論は後で Scheduler 側へ足せる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetCapacity {
    pub max_jobs: u16,
    #[serde(default)]
    pub active_jobs: u16,
}

impl Default for TargetCapacity {
    fn default() -> Self {
        Self {
            max_jobs: 1,
            active_jobs: 0,
        }
    }
}

impl TargetCapacity {
    pub fn free(&self) -> u16 {
        self.max_jobs.saturating_sub(self.active_jobs)
    }
    pub fn has_room(&self) -> bool {
        self.free() > 0
    }
}

/// 課金の形。**Provider ごとの価格文字列を Zaivern が理解しないようにする** (§34)。
///
/// 価格表をコードへ埋め込まない。API から取れなければ [`BillingModel::Unknown`]。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BillingModel {
    Free,
    FixedMonthly {
        monthly_minor: u64,
        currency: String,
    },
    HourlyWithMonthlyCap {
        hourly_minor: u64,
        monthly_cap_minor: u64,
        currency: String,
    },
    UsageBased,
    Unknown,
}

impl Default for BillingModel {
    fn default() -> Self {
        Self::Unknown
    }
}

impl BillingModel {
    /// 並べ替えのための費用ヒント (小さいほど安い)。**分からないものは真ん中**に置く
    /// — 0 にすると「不明 = ただ」になり、いちばん高いものを選びかねない。
    pub fn cost_hint(&self) -> u64 {
        match self {
            Self::Free => 0,
            Self::FixedMonthly { monthly_minor, .. } => *monthly_minor,
            Self::HourlyWithMonthlyCap {
                monthly_cap_minor, ..
            } => *monthly_cap_minor,
            Self::UsageBased | Self::Unknown => u64::MAX / 2,
        }
    }

    /// 画面に出す 1 行。単位は**マイナー単位** (セント / 銭) で持っているので
    /// ここで戻す。分からないものは分からないと書く。
    pub fn summary(&self) -> String {
        let money = |minor: u64, cur: &str| format!("{}.{:02} {cur}", minor / 100, minor % 100);
        match self {
            Self::Free => "free".to_string(),
            Self::FixedMonthly {
                monthly_minor,
                currency,
            } => format!("{}/mo", money(*monthly_minor, currency)),
            Self::HourlyWithMonthlyCap {
                hourly_minor,
                monthly_cap_minor,
                currency,
            } => format!(
                "{}/h (cap {})",
                money(*hourly_minor, currency),
                money(*monthly_cap_minor, currency)
            ),
            Self::UsageBased => "usage-based".to_string(),
            Self::Unknown => "unknown".to_string(),
        }
    }
}

/// 1 つの実行先。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionTarget {
    pub id: TargetId,
    pub name: String,
    pub provider: ProviderId,
    pub transport: TransportKind,
    pub endpoint: TargetEndpoint,
    #[serde(default)]
    pub capabilities: Capabilities,
    #[serde(default)]
    pub capacity: TargetCapacity,
    #[serde(default = "lifecycle_unknown")]
    pub lifecycle: TargetLifecycle,
    /// **Zaivern が作った実行先か。** `false` のものは決して破棄しない (§22 / §51)。
    #[serde(default)]
    pub managed: bool,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub labels: BTreeMap<String, String>,
    /// 費用の目安。取れなければ [`BillingModel::Unknown`] (§21)。
    #[serde(default)]
    pub billing: BillingModel,
    /// Provider 側の識別子 (Hetzner のサーバー ID 等)。**破棄の照合に使う**。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_ref: Option<String>,
    /// 最後の確認で分かったこと (画面と `doctor` 用。秘密は入らない)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note: String,
}

fn lifecycle_unknown() -> TargetLifecycle {
    TargetLifecycle::Unknown
}

impl ExecutionTarget {
    /// この Zaivern が動いている機械そのもの。**常に 1 つだけ在る実行先**。
    pub fn local(max_jobs: u16) -> Self {
        Self {
            id: TargetId::new("local"),
            name: "local".to_string(),
            provider: ProviderId::new("local"),
            transport: TransportKind::Local,
            endpoint: TargetEndpoint::Local,
            capabilities: Capabilities::host(),
            capacity: TargetCapacity {
                max_jobs,
                active_jobs: 0,
            },
            lifecycle: TargetLifecycle::Ready,
            managed: false,
            labels: BTreeMap::new(),
            billing: BillingModel::Free,
            provider_ref: None,
            note: String::new(),
        }
    }

    /// 遠隔か (画面の並べ替えと `prefer` の判定に使う)。
    pub fn is_remote(&self) -> bool {
        self.transport != TransportKind::Local
    }
}

// ─────────────────────── 実行の要求と結果 ───────────────────────

/// リモート側のパス。**POSIX の絶対パスか、`~/` から始まる home 相対**だけ。
///
/// 型で包むのは、ローカルの [`PathBuf`] (Windows なら `\`) がそのまま
/// リモートへ流れる事故を防ぐため。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RemotePath(String);

impl RemotePath {
    /// 検査つきで作る。制御文字・`\` ・空を弾く。
    pub fn new(s: impl Into<String>) -> Result<Self, CloudError> {
        let s = s.into();
        if s.is_empty() {
            return Err(CloudError::config("リモートのパスが空です"));
        }
        if s.chars().any(|c| c.is_control()) {
            return Err(CloudError::security(
                "リモートのパスに制御文字が入っています",
            ));
        }
        if s.contains('\\') {
            return Err(CloudError::config(
                "リモートのパスは POSIX 形式 (/ 区切り) で指定してください",
            ));
        }
        if !(s.starts_with('/') || s.starts_with("~/") || s == "~") {
            return Err(CloudError::config(
                "リモートのパスは / か ~/ から始めてください",
            ));
        }
        Ok(Self(s))
    }

    /// `~/` を **home 相対**として組み立てる (この形しか使わないので入口を 1 つにする)。
    pub fn home(rel: impl AsRef<str>) -> Result<Self, CloudError> {
        Self::new(format!("~/{}", rel.as_ref().trim_start_matches('/')))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

}

impl fmt::Display for RemotePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 1 回の実行の要求。**シェル文字列を丸ごと受け取る欄は無い** (§13)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub program: String,
    pub args: Vec<String>,
    /// 実行する場所。`None` ならリモートの既定 (ログインディレクトリ)。
    pub cwd: Option<String>,
    pub env: BTreeMap<String, String>,
    /// 擬似端末を割り当てるか (対話が要るときだけ)。
    pub tty: bool,
    /// 上限。**`None` を許すが、Transport 側が必ず既定の上限を当てる** (§48)。
    pub timeout: Option<Duration>,
}

impl ExecRequest {
    pub fn new(program: impl Into<String>, args: impl IntoIterator<Item = String>) -> Self {
        Self {
            program: program.into(),
            args: args.into_iter().collect(),
            cwd: None,
            env: BTreeMap::new(),
            tty: false,
            timeout: None,
        }
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_env(mut self, k: impl Into<String>, v: impl Into<String>) -> Self {
        self.env.insert(k.into(), v.into());
        self
    }

    /// 画面とログに出す 1 行 (実行そのものには使わない)。
    pub fn display(&self) -> String {
        let mut s = self.program.clone();
        for a in &self.args {
            s.push(' ');
            s.push_str(a);
        }
        s
    }
}

/// 実行の結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecResult {
    /// 終了コード。シグナルで死んだ場合は `None`。
    pub exit_code: Option<i32>,
    pub duration_ms: u64,
}

impl ExecResult {
    pub fn ok(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// 実行中の出力の受け口。**呼び出し側を止めない**ために、Transport は
/// 出力を溜め込まずここへ流す (設計原則 2)。
pub trait EventSink {
    fn on_stdout(&mut self, chunk: &[u8]);
    fn on_stderr(&mut self, chunk: &[u8]);
}

/// 溜める受け口 (CLI とテストが使う)。**上限を持つ** — リモートが暴走しても
/// 手元のメモリを食い尽くさない。
pub struct CollectSink {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub truncated: bool,
    limit: usize,
}

impl Default for CollectSink {
    fn default() -> Self {
        Self::with_limit(8 * 1024 * 1024)
    }
}

impl CollectSink {
    pub fn with_limit(limit: usize) -> Self {
        Self {
            stdout: Vec::new(),
            stderr: Vec::new(),
            truncated: false,
            limit,
        }
    }

    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }

    pub fn stderr_text(&self) -> String {
        String::from_utf8_lossy(&self.stderr).into_owned()
    }

    fn push(buf: &mut Vec<u8>, chunk: &[u8], limit: usize, truncated: &mut bool) {
        if buf.len() >= limit {
            *truncated = true;
            return;
        }
        let room = limit - buf.len();
        if chunk.len() > room {
            buf.extend_from_slice(&chunk[..room]);
            *truncated = true;
        } else {
            buf.extend_from_slice(chunk);
        }
    }
}

impl EventSink for CollectSink {
    fn on_stdout(&mut self, chunk: &[u8]) {
        Self::push(&mut self.stdout, chunk, self.limit, &mut self.truncated);
    }
    fn on_stderr(&mut self, chunk: &[u8]) {
        Self::push(&mut self.stderr, chunk, self.limit, &mut self.truncated);
    }
}

/// 実行先を確かめた結果 (§42 の `zai cloud target probe`)。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProbeResult {
    pub reachable: bool,
    pub latency_ms: u64,
    pub capabilities: Capabilities,
    pub shell: String,
    pub kernel: String,
    /// 失敗の理由 (**秘密は入らない**)。
    pub error: String,
}

// ─────────────────────── 仕事 ───────────────────────

/// **Cloud 層が持ってよい状態は「基盤の仕事」だけ** (§35)。
///
/// エージェントが考え中か・編集中か・承認待ちかは**この層の関知するところ
/// ではない**。第 2 の Agent 状態機械をここに作らない
/// (既存の [`crate::supervisor`] が唯一の持ち主)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionJobState {
    Queued,
    Preparing,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl ExecutionJobState {
    pub fn id(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Preparing => "preparing",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// もう動いていないか (枠を返してよいか)。
    pub fn is_final(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// 1 つの仕事の記録。`~/.zaivern/cloud/jobs.json` に載る。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionJob {
    pub id: JobId,
    pub target: TargetId,
    pub state: ExecutionJobState,
    /// 何を走らせたか (画面用の 1 行。**秘密は入らない**)。
    #[serde(default)]
    pub command: String,
    /// リモートの作業ディレクトリ。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    /// 結果を持ち帰った枝 (`refs/remotes/zaivern-cloud/<job>`)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub result_ref: String,
    #[serde(default)]
    pub started_unix: u64,
    #[serde(default)]
    pub ended_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub message: String,
}

// ─────────────────────── 失敗 ───────────────────────

/// この層の失敗。**Provider 固有の型を無制限に増やさない** (§47)。
///
/// メッセージは**作る時点で** [`redact`] を通す。`Debug` / `Display` の
/// どちらから出しても秘密が出ないことを [`tests::秘密はどの出口からも出ない`]
/// が確かめる。
#[derive(Clone, PartialEq, Eq)]
pub enum CloudError {
    Config(String),
    Auth(String),
    Io(String),
    Transport(String),
    Provider {
        provider: String,
        status: Option<u16>,
        message: String,
    },
    Timeout(String),
    Security(String),
    NoCapacity(String),
    Unsupported(String),
}

impl CloudError {
    pub fn config(m: impl Into<String>) -> Self {
        Self::Config(redact(&m.into()))
    }
    pub fn auth(m: impl Into<String>) -> Self {
        Self::Auth(redact(&m.into()))
    }
    pub fn io(m: impl Into<String>) -> Self {
        Self::Io(redact(&m.into()))
    }
    pub fn transport(m: impl Into<String>) -> Self {
        Self::Transport(redact(&m.into()))
    }
    pub fn timeout(m: impl Into<String>) -> Self {
        Self::Timeout(redact(&m.into()))
    }
    pub fn security(m: impl Into<String>) -> Self {
        Self::Security(redact(&m.into()))
    }
    pub fn no_capacity(m: impl Into<String>) -> Self {
        Self::NoCapacity(redact(&m.into()))
    }
    pub fn unsupported(m: impl Into<String>) -> Self {
        Self::Unsupported(redact(&m.into()))
    }

    /// Provider の失敗。**本文は要約だけ**に切り詰める (§47) — 生の応答を
    /// そのまま抱えると、そこに載っていた値まで一緒に持ち回ることになる。
    pub fn provider(provider: impl Into<String>, status: Option<u16>, body: &str) -> Self {
        Self::Provider {
            provider: provider.into(),
            status,
            message: redact(&summarize(body)),
        }
    }

    /// 終了コードの対応表 (§43)。
    pub fn exit_code(&self) -> i32 {
        match self {
            // 設定と認証は「直すのは利用者の手元」なので 3
            Self::Config(_) | Self::Auth(_) => 3,
            Self::NoCapacity(_) => 4,
            _ => 1,
        }
    }

    /// 再試行してよい失敗か (§49)。
    pub fn retryable(&self) -> bool {
        match self {
            Self::Provider { status, .. } => {
                matches!(status, Some(429) | Some(500..=599))
            }
            Self::Timeout(_) => true,
            _ => false,
        }
    }
}

/// 応答本文を 1 行の要約へ畳む。長さを切り、改行を潰す。
fn summarize(body: &str) -> String {
    const MAX: usize = 200;
    let one = body
        .replace(['\n', '\r'], " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    if one.chars().count() <= MAX {
        return one;
    }
    let cut: String = one.chars().take(MAX).collect();
    format!("{cut}…")
}

impl fmt::Display for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(m) => write!(f, "設定: {m}"),
            Self::Auth(m) => write!(f, "認証: {m}"),
            Self::Io(m) => write!(f, "入出力: {m}"),
            Self::Transport(m) => write!(f, "接続: {m}"),
            Self::Provider {
                provider,
                status,
                message,
            } => match status {
                Some(s) => write!(f, "{provider} ({s}): {message}"),
                None => write!(f, "{provider}: {message}"),
            },
            Self::Timeout(m) => write!(f, "時間切れ: {m}"),
            Self::Security(m) => write!(f, "安全のため中止: {m}"),
            Self::NoCapacity(m) => write!(f, "空きがありません: {m}"),
            Self::Unsupported(m) => write!(f, "未対応: {m}"),
        }
    }
}

/// **`Debug` を導出しない。** 導出すると欄の中身が生で出るので、将来
/// 誰かが秘密を持つ欄を足した日に静かに漏れる。表示は [`fmt::Display`] と
/// 同じ経路 (= 必ず畳んだ後の文字列) だけを通す。
impl fmt::Debug for CloudError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CloudError({self})")
    }
}

impl std::error::Error for CloudError {}

impl From<std::io::Error> for CloudError {
    fn from(e: std::io::Error) -> Self {
        Self::io(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 実行先idは時刻順に並び重複しない() {
        let a = ids::new_id("t-");
        let b = ids::new_id("t-");
        assert_ne!(a, b, "同じ瞬間でも重複しない");
        assert!(a < b, "時刻順に並ぶ: {a} < {b}");
        // ブランチ名・ディレクトリ名として安全な字だけ
        for id in [&a, &b] {
            assert!(
                id.chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{id} に使えない字がある"
            );
        }
    }

    #[test]
    fn unameから能力を起こす() {
        assert_eq!(OsFamily::from_uname("Linux\n"), OsFamily::Linux);
        assert_eq!(OsFamily::from_uname("Darwin"), OsFamily::MacOS);
        assert_eq!(OsFamily::from_uname("MINGW64_NT-10.0"), OsFamily::Windows);
        assert_eq!(OsFamily::from_uname("Plan9"), OsFamily::Unknown);
        assert_eq!(Architecture::from_uname("x86_64"), Architecture::X86_64);
        assert_eq!(Architecture::from_uname("aarch64"), Architecture::Aarch64);
        assert_eq!(Architecture::from_uname("arm64"), Architecture::Aarch64);
        assert_eq!(Architecture::from_uname("riscv64"), Architecture::Unknown);
    }

    #[test]
    fn リモートパスは形を強制する() {
        assert!(RemotePath::new("/srv/work").is_ok());
        assert!(RemotePath::new("~/work").is_ok());
        assert!(RemotePath::new("work").is_err(), "相対パスは受けない");
        assert!(RemotePath::new("C:\\work").is_err(), "Windows 形式は受けない");
        assert!(RemotePath::new("/srv/wo\nrk").is_err(), "制御文字は受けない");
        assert!(RemotePath::new("").is_err());
        assert_eq!(
            RemotePath::home(".zaivern/cloud").expect("作れる").as_str(),
            "~/.zaivern/cloud"
        );
    }

    /// **仕込んだ秘密が Debug / Display のどちらからも出ない** (§57)。
    #[test]
    fn 秘密はどの出口からも出ない() {
        const TOKEN: &str = "super-secret-test-token";
        let body = format!("{{\"error\":\"bad\",\"authorization\":\"Bearer {TOKEN}\"}}");
        let e = CloudError::provider("hetzner", Some(401), &body);
        assert!(!format!("{e}").contains(TOKEN), "Display から漏れた: {e}");
        assert!(!format!("{e:?}").contains(TOKEN), "Debug から漏れた: {e:?}");
        // 分類そのものは残る (伏せても診断できること)
        assert!(format!("{e}").contains("401"));
    }

    #[test]
    fn 応答本文は要約へ畳む() {
        let long = "x".repeat(1000);
        let s = summarize(&long);
        assert!(s.chars().count() <= 201, "長さが切れていない: {}", s.len());
        assert_eq!(summarize("a\nb\r\nc"), "a b c", "改行は潰す");
    }

    #[test]
    fn 終了コードは分類ごとに決まる() {
        assert_eq!(CloudError::config("x").exit_code(), 3);
        assert_eq!(CloudError::auth("x").exit_code(), 3);
        assert_eq!(CloudError::no_capacity("x").exit_code(), 4);
        assert_eq!(CloudError::transport("x").exit_code(), 1);
        assert_eq!(CloudError::security("x").exit_code(), 1);
    }

    #[test]
    fn 再試行の可否は状態で決まる() {
        assert!(CloudError::provider("p", Some(429), "").retryable());
        assert!(CloudError::provider("p", Some(503), "").retryable());
        assert!(CloudError::timeout("x").retryable());
        assert!(!CloudError::provider("p", Some(401), "").retryable());
        assert!(!CloudError::provider("p", Some(403), "").retryable());
        assert!(!CloudError::provider("p", Some(404), "").retryable());
        assert!(!CloudError::security("host key mismatch").retryable());
    }

    #[test]
    fn 空き枠は飽和で数える() {
        let c = TargetCapacity {
            max_jobs: 2,
            active_jobs: 5,
        };
        assert_eq!(c.free(), 0, "引き算で溢れない");
        assert!(!c.has_room());
    }

    #[test]
    fn 費用の不明はただではない() {
        // 「不明 = 0」にすると、いちばん高いものを最安として選びかねない
        assert!(BillingModel::Unknown.cost_hint() > BillingModel::Free.cost_hint());
        assert_eq!(
            BillingModel::FixedMonthly {
                monthly_minor: 849,
                currency: "EUR".into()
            }
            .summary(),
            "8.49 EUR/mo"
        );
    }

    #[test]
    fn 溜める受け口は上限で切る() {
        let mut s = CollectSink::with_limit(4);
        s.on_stdout(b"abcdef");
        assert_eq!(s.stdout_text(), "abcd");
        assert!(s.truncated);
    }
}
