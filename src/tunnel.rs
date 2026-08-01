//! SSH リバーストンネル — スマホが同じ Wi-Fi にいなくても繋ぐための経路。
//!
//! LAN モードのスマホリモート ([`crate::remote`]) は「同じ Wi-Fi」が前提で、
//! 外出先からは届かない。ここでは **ユーザーが既に SSH で入れるホスト**
//! (VPS / 自宅サーバ / 会社の踏み台) を中継点にして、
//!
//! ```text
//! スマホ ──HTTP──▶ 踏み台:8899 ──SSHトンネル──▶ PC:127.0.0.1:8899
//! ```
//!
//! という経路を作る。`ssh -N -R <公開ポート>:127.0.0.1:<ローカルポート> <接続先>`
//! を子プロセスとして持つだけなので、**スマホ側には SSH クライアントが要らない**
//! (ブラウザで URL を開くだけ)。
//!
//! ## 設計の約束
//!
//! - **認証は OS の `ssh` に丸投げする。** パスワードは受け取らないし保存もしない。
//!   `BatchMode=yes` を付けるので、鍵 (ssh-agent / `~/.ssh/config`) が無ければ
//!   入力待ちで固まらずに即失敗し、理由を返す。
//! - **踏み台側の bind 先はアプリが決めない。** `-R` に bind アドレスを書かない
//!   ので OpenSSH の既定 (= 踏み台の loopback のみ) が効く。インターネットへ
//!   晒すかは踏み台の `sshd_config` の `GatewayPorts` でユーザーが選ぶ。
//! - **生の stderr は画面に出さない。** [`classify`] が要点だけの [`Failure`] に
//!   畳んでから UI へ渡す (純粋関数 + テーブルテスト)。
//! - **アイドルのコストを増やさない。** 監視は接続中だけ。待ちは `Condvar` で、
//!   切断操作は即座にループを起こす。UI の再描画は**状態が変わったときだけ**頼む。
//!
//! 設計原則 5 (ハンドラは1面、トランスポートは多数) の実装: HTTP ハンドラは
//! [`crate::remote`] のまま 1 面で、LAN と SSH は **待ち受け先の差** でしかない。

use std::io::{BufRead, BufReader};
use std::net::Ipv6Addr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::lockx::lock_ok;

// ─── 接続先のパース ──────────────────────────────────────────────────

/// `user@host:port` を分解したもの。`user` と `port` は省略可
/// (省略時は `ssh` が `~/.ssh/config` と現在のユーザー名を使う)。
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Target {
    pub user: Option<String>,
    pub host: String,
    pub port: Option<u16>,
}

impl Target {
    /// `ssh` に渡す接続先引数 (`user@host` / `host`)。ポートは `-p` で別に渡す。
    ///
    /// IPv6 は角括弧を**付けない**。角括弧付きを受け付けない古い OpenSSH が
    /// あるのに対し、素の形はどのバージョンでも通る。
    pub fn dest(&self) -> String {
        match &self.user {
            Some(u) => format!("{u}@{}", self.host),
            None => self.host.clone(),
        }
    }

    /// URL の host 部 (IPv6 リテラルは RFC 3986 に従って角括弧で包む)。
    pub fn url_host(&self) -> String {
        if self.host.parse::<Ipv6Addr>().is_ok() {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        }
    }

    /// 画面に出す短い表記 (入力欄へ戻せる形)。
    pub fn display(&self) -> String {
        let base = self.dest();
        match self.port {
            Some(p) if self.host.parse::<Ipv6Addr>().is_ok() => {
                // 角括弧なしだと `::1:2222` が曖昧になる
                match &self.user {
                    Some(u) => format!("{u}@[{}]:{p}", self.host),
                    None => format!("[{}]:{p}", self.host),
                }
            }
            Some(p) => format!("{base}:{p}"),
            None => base,
        }
    }
}

/// 接続先の入力が受け付けられない理由。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetError {
    /// 何も入力されていない
    Empty,
    /// `@` の左が空 / `@` が複数
    BadUser,
    /// `@` の右が空
    MissingHost,
    /// ホスト名に使えない文字 / 壊れた IPv6 リテラル
    BadHost,
    /// ポートが数値でない / 範囲外
    BadPort,
}

impl TargetError {
    /// UI にそのまま出せる 1 行 (呼び出し側で `tr()` を通すこと)。
    pub fn msg(&self) -> &'static str {
        match self {
            TargetError::Empty => "接続先を入力してください (例: user@example.com)",
            TargetError::BadUser => "ユーザー名が空です (例: user@example.com)",
            TargetError::MissingHost => "ホスト名が空です (例: user@example.com)",
            TargetError::BadHost => "ホスト名に使えない文字があります",
            TargetError::BadPort => "ポート番号は 1〜65535 の数値で指定してください",
        }
    }
}

/// ホスト名に使ってよい文字か (IPv6 の `:` とゾーン ID の `%` を含む)。
fn host_char_ok(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | ':' | '%')
}

fn parse_port(s: &str) -> Result<u16, TargetError> {
    s.parse::<u16>()
        .ok()
        .filter(|p| *p > 0)
        .ok_or(TargetError::BadPort)
}

/// `[user@]host[:port]` を分解する。**純粋関数** (テーブルテストで固定)。
///
/// 受け付ける形:
/// - `example.com` / `user@example.com` / `user@example.com:2222`
/// - `user@[::1]:2222` (角括弧つき IPv6 + ポート)
/// - `::1` / `user@::1` (角括弧なし IPv6。**ポートは書けない** — 曖昧なので)
pub fn parse_target(raw: &str) -> Result<Target, TargetError> {
    let s = raw.trim();
    if s.is_empty() {
        return Err(TargetError::Empty);
    }
    if s.chars().any(|c| c.is_whitespace()) {
        return Err(TargetError::BadHost);
    }

    let (user, rest) = match s.rsplit_once('@') {
        Some((u, _)) if u.is_empty() || u.contains('@') => return Err(TargetError::BadUser),
        Some((u, r)) => (Some(u.to_string()), r),
        None => (None, s),
    };
    if rest.is_empty() {
        return Err(TargetError::MissingHost);
    }

    let (host, port) = if let Some(inner) = rest.strip_prefix('[') {
        let (h, tail) = inner.split_once(']').ok_or(TargetError::BadHost)?;
        if h.parse::<Ipv6Addr>().is_err() {
            return Err(TargetError::BadHost);
        }
        let port = match tail {
            "" => None,
            t => Some(parse_port(t.strip_prefix(':').ok_or(TargetError::BadPort)?)?),
        };
        (h.to_string(), port)
    } else if rest.matches(':').count() >= 2 {
        // 角括弧なしの IPv6。`::1:22` を「ポート付き」と読むと本物の
        // アドレスを壊すので、まるごとホストとして扱う。
        if rest.parse::<Ipv6Addr>().is_err() {
            return Err(TargetError::BadHost);
        }
        (rest.to_string(), None)
    } else if let Some((h, p)) = rest.split_once(':') {
        (h.to_string(), Some(parse_port(p)?))
    } else {
        (rest.to_string(), None)
    };

    if host.is_empty() {
        return Err(TargetError::MissingHost);
    }
    if !host.chars().all(host_char_ok) {
        return Err(TargetError::BadHost);
    }
    Ok(Target { user, host, port })
}

// ─── 失敗の分類 ──────────────────────────────────────────────────────

/// ssh が失敗した理由。生の stderr の代わりに、これだけを UI へ渡す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// `ssh` が PATH に無い
    NoSsh,
    /// 鍵で拒否された (publickey)
    AuthDenied,
    /// パスワード / 対話認証が必要 — BatchMode では入力できない
    NeedsInteractive,
    /// ホスト名を解決できない
    UnknownHost,
    /// 接続を拒否された
    Refused,
    /// 接続がタイムアウトした / 応答が途切れた
    Timeout,
    /// ホスト鍵が未登録・変化した
    HostKey,
    /// 踏み台側の公開ポートが埋まっている
    PortInUse,
    /// 経路が無い
    Unreachable,
    /// 上のどれでもない (詳細は出さない)
    Other,
}

impl Failure {
    /// 1 行の見出し (呼び出し側で `tr()` を通すこと)。
    pub fn headline(&self) -> &'static str {
        match self {
            Failure::NoSsh => "OpenSSH クライアントが見つかりません",
            Failure::AuthDenied => "鍵認証が拒否されました",
            Failure::NeedsInteractive => "パスワード入力が必要な設定です",
            Failure::UnknownHost => "ホスト名を解決できません",
            Failure::Refused => "接続を拒否されました",
            Failure::Timeout => "接続がタイムアウトしました",
            Failure::HostKey => "ホスト鍵を確認できません",
            Failure::PortInUse => "踏み台側のポートが使用中です",
            Failure::Unreachable => "ネットワーク経路がありません",
            Failure::Other => "SSH 接続に失敗しました",
        }
    }

    /// 「では何をすればよいか」の 1 行 (呼び出し側で `tr()` を通すこと)。
    pub fn hint(&self) -> &'static str {
        match self {
            Failure::NoSsh => install_hint(),
            Failure::AuthDenied => {
                "公開鍵を踏み台の ~/.ssh/authorized_keys へ登録してください (パスワードは使いません)"
            }
            Failure::NeedsInteractive => {
                "鍵認証を設定してください — パスワードはこのアプリでは扱いません"
            }
            Failure::UnknownHost => "ホスト名の綴りと DNS / VPN の状態を確認してください",
            Failure::Refused => "ポート番号と、踏み台で sshd が動いているかを確認してください",
            Failure::Timeout => "踏み台に届いていません (ファイアウォール / 回線を確認)",
            Failure::HostKey => {
                "一度ターミナルで ssh <接続先> を実行し、ホスト鍵を登録してください"
            }
            Failure::PortInUse => {
                "踏み台で同じポートを使う別のトンネルが残っています (数十秒で解放されます)"
            }
            Failure::Unreachable => "VPN / ネットワーク接続を確認してください",
            Failure::Other => "ターミナルで ssh <接続先> を実行して原因を確認してください",
        }
    }

    /// 時間が解決し得る失敗か。**時間では直らないものを再試行しない**
    /// (鍵が無いのに 5 回叩いても鍵は生えない)。
    pub fn retryable(&self) -> bool {
        matches!(
            self,
            Failure::Refused | Failure::Timeout | Failure::PortInUse | Failure::Unreachable
        )
    }
}

/// `ssh` の stderr から失敗理由を 1 つ選ぶ。**純粋関数** (テーブルテストで固定)。
///
/// 判定は具体的なものから先に見る。`Permission denied` は最後 —
/// 「ポート転送に失敗した」等の具体的な行が同じ出力に混ざっていることがある。
pub fn classify(stderr: &str) -> Failure {
    let s = stderr.to_ascii_lowercase();
    let has = |p: &str| s.contains(p);

    if has("remote port forwarding failed") || has("cannot listen to port") {
        return Failure::PortInUse;
    }
    if has("host key verification failed")
        || has("remote host identification has changed")
        || (has("host key for") && has("has changed"))
    {
        return Failure::HostKey;
    }
    if has("could not resolve hostname")
        || has("name or service not known")
        || has("nodename nor servname")
        || has("no address associated with hostname")
    {
        return Failure::UnknownHost;
    }
    if has("connection refused") {
        return Failure::Refused;
    }
    if has("network is unreachable") || has("no route to host") {
        return Failure::Unreachable;
    }
    if has("timed out") || has("timeout, server") || has("connection closed by remote host") {
        return Failure::Timeout;
    }
    if has("permission denied") && (has("password") || has("keyboard-interactive")) {
        return Failure::NeedsInteractive;
    }
    if has("permission denied") || has("too many authentication failures") {
        return Failure::AuthDenied;
    }
    Failure::Other
}

/// `ssh -v` の 1 行が「リバース転送が確立した」ことを示すか。
/// **純粋関数** (テーブルテストで固定)。
///
/// 設計原則 4 の順位づけ: これは「ベンダー提供の構造化された合図」に相当する
/// 最上位の手掛かり。取りこぼしても [`GRACE`] 経過で段は進むので、
/// OpenSSH の文言が変わっても「接続済みにならない」壊れ方はしない。
pub fn forward_established(line: &str) -> bool {
    let l = line.to_ascii_lowercase();
    l.contains("remote forward success")
        || l.contains("all remote forwarding requests processed")
        || (l.contains("allocated port") && l.contains("for remote forward"))
}

// ─── 再接続のバックオフ ──────────────────────────────────────────────

/// 再試行の上限。無限に叩き続けない (踏み台に迷惑をかけない)。
pub const MAX_RETRIES: u32 = 5;

/// `attempt` 回目 (1 始まり) の再接続までの待ち時間。上限を超えたら `None`。
/// **純粋関数** (テーブルテストで固定)。2 秒から倍々、30 秒で頭打ち。
pub fn backoff(attempt: u32) -> Option<Duration> {
    if attempt == 0 || attempt > MAX_RETRIES {
        return None;
    }
    let secs = 2u64
        .saturating_pow(attempt)
        .min(30);
    Some(Duration::from_secs(secs))
}

// ─── ssh の起動引数 ──────────────────────────────────────────────────

/// リバーストンネルの `ssh` 引数。**純粋関数** (テーブルテストで固定)。
///
/// - `-N -T`: コマンドを実行せず端末も要求しない (転送だけ)
/// - `BatchMode=yes`: パスワード入力待ちで固まらない (鍵が無ければ即失敗)
/// - `ExitOnForwardFailure=yes`: 転送に失敗したまま生き残らせない
/// - `ServerAliveInterval/CountMax`: 無反応の死んだトンネルを残さない
/// - `-R <公開>:127.0.0.1:<ローカル>`: **bind アドレスを書かない** —
///   踏み台側を loopback に留めるか公開するかは `GatewayPorts` でユーザーが決める
pub fn ssh_args(t: &Target, public_port: u16, local_port: u16) -> Vec<String> {
    let mut a: Vec<String> = [
        "-v", "-N", "-T",
        "-o", "BatchMode=yes",
        "-o", "ExitOnForwardFailure=yes",
        "-o", "ServerAliveInterval=30",
        "-o", "ServerAliveCountMax=3",
        "-o", "ConnectTimeout=10",
    ]
    .iter()
    .map(|s| (*s).to_string())
    .collect();
    a.push("-R".into());
    a.push(format!("{public_port}:127.0.0.1:{local_port}"));
    if let Some(p) = t.port {
        a.push("-p".into());
        a.push(p.to_string());
    }
    a.push(t.dest());
    a
}

/// SSH クライアントを持っている人向けのローカル転送コマンド (表示・コピー用)。
/// 実行はしない。**純粋関数** (テーブルテストで固定)。
pub fn ssh_l_command(t: &Target, public_port: u16, local_port: u16) -> String {
    let port = match t.port {
        Some(p) => format!(" -p {p}"),
        None => String::new(),
    };
    format!(
        "ssh -N -L {local_port}:127.0.0.1:{public_port}{port} {}",
        t.dest()
    )
}

/// `ssh` の実体を PATH から探す (`which` コマンドには依存しない)。
pub fn ssh_path() -> Option<PathBuf> {
    crate::shellenv::which("ssh")
}

// ─── 画面に出す長文 ──────────────────────────────────────────────────
//
// UI (app.rs) から使う長い説明は、**辞書キーそのもの**なので const にして
// 1 箇所に置く。app.rs へ直書きすると、辞書側の文字 (全角空白など) が
// 1 文字ずれただけで英語モードのときだけ日本語が残る — 気付けない壊れ方をする。
// const にしておけば下のテストが辞書との一致をバイト単位で見張れる。

/// 接続先入力欄のホバー説明。
pub const HOST_HINT: &str = "SSH で入れるホスト (VPS / 自宅サーバ / 踏み台)。\n\
     ポートを変えているときは user@host:2222 と書きます。\n\
     認証は OS の ssh と ~/.ssh/config / ssh-agent に任せます —\n\
     このアプリはパスワードも鍵も保存しません。";

/// `ssh -L` コピーボタンのホバー説明。
pub const SSH_L_HINT: &str = "SSH クライアントを持っている PC 向け。実行後、\n\
     ブラウザで http://127.0.0.1:<ポート>/ を開きます\n\
     (このアプリからは実行しません)";

/// 踏み台側の公開範囲の説明 (1 段落)。
pub const GATEWAY_NOTE: &str =
    "※ 踏み台側の公開ポートは既定で 127.0.0.1 にだけ開きます。\n\u{3000}\
     インターネットから直接開くには、踏み台の sshd_config で\n\u{3000}\
     GatewayPorts を有効にしてください。";

/// `ssh` が無いときの、OS 別の入れ方 1 行。
pub fn install_hint() -> &'static str {
    if cfg!(windows) {
        "Windows 10 以降は「設定 → システム → オプション機能」で OpenSSH クライアントを追加できます"
    } else if cfg!(target_os = "macos") {
        "macOS には標準で入っています — PATH が通っているかを確認してください"
    } else {
        "openssh-client パッケージを入れてください (例: sudo apt install openssh-client)"
    }
}

// ─── 状態機械 ────────────────────────────────────────────────────────

/// いまトンネルがどの段にいるか。**UI には必ずこの段を出す**。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// 切断 (何も起動していない)
    Disconnected,
    /// 接続中 (ssh 起動〜転送確立まで。再試行の待ち時間もここ)
    Connecting,
    /// 接続済み (リバース転送が確立している)
    Connected,
    /// 失敗 (理由つき。再試行は打ち切った)
    Failed(Failure),
}

impl Stage {
    /// 段の見出し (呼び出し側で `tr()` を通すこと)。
    ///
    /// 「切断」「失敗」ではなく「未接続」「接続失敗」にしてあるのは、
    /// 前者が**ボタンのラベル / 他画面の語**と衝突するため
    /// (辞書キーは日本語原文そのものなので、同じ語は同じ訳になってしまう)。
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Disconnected => "未接続",
            Stage::Connecting => "接続中",
            Stage::Connected => "接続済み",
            Stage::Failed(_) => "接続失敗",
        }
    }
}

/// UI が読む、トンネルのすべて。
#[derive(Clone, Debug)]
pub struct State {
    pub stage: Stage,
    /// 何回目の再試行中か (0 = 初回)
    pub attempt: u32,
    /// 接続中/接続済みの相手
    pub target: Option<Target>,
    /// 踏み台側の公開ポート (= PC 側のローカルポート)
    pub port: u16,
    /// 直前の失敗 (再試行中も理由を出せるように保持する)
    pub last_failure: Option<Failure>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            stage: Stage::Disconnected,
            attempt: 0,
            target: None,
            port: 0,
            last_failure: None,
        }
    }
}

impl State {
    /// スマホで開く URL (トークン付き)。接続済みのときだけ。
    pub fn phone_url(&self, token: &str) -> Option<String> {
        match (self.stage, &self.target) {
            (Stage::Connected, Some(t)) => {
                Some(format!("http://{}:{}/?t={token}", t.url_host(), self.port))
            }
            _ => None,
        }
    }
}

/// 転送確立の合図を取りこぼしても段を進めるための猶予。
/// これだけ生き延びていれば `ExitOnForwardFailure=yes` を通過している。
const GRACE: Duration = Duration::from_secs(3);

/// 監視の刻み。接続中だけ回る (切断時はスレッドごと存在しない)。
const TICK: Duration = Duration::from_millis(700);

/// stderr の保持量 (分類に必要な直近だけ。生ログは画面に出さない)。
const TAIL_MAX: usize = 4096;

struct Shared {
    state: Mutex<State>,
    stop: AtomicBool,
    gate: Mutex<()>,
    cv: Condvar,
}

impl Shared {
    /// 状態を書き換え、**変わったときだけ** 再描画を頼む
    /// (毎フレーム起こすと「アイドルのコストはゼロ」が崩れる)。
    fn set(&self, ctx: &egui::Context, f: impl FnOnce(&mut State)) {
        let changed = {
            let mut st = lock_ok(&self.state);
            let was = (st.stage, st.attempt);
            f(&mut st);
            was != (st.stage, st.attempt)
        };
        if changed {
            ctx.request_repaint();
        }
    }

    /// `d` だけ待つ。切断されたら即座に起きる。戻り値 = 停止したか。
    fn wait(&self, d: Duration) -> bool {
        let g = lock_ok(&self.gate);
        let _ = self
            .cv
            .wait_timeout(g, d)
            .unwrap_or_else(|e| e.into_inner());
        self.stop.load(Ordering::SeqCst)
    }

    fn stop_now(&self) {
        self.stop.store(true, Ordering::SeqCst);
        let _g = lock_ok(&self.gate);
        self.cv.notify_all();
    }
}

/// SSH リバーストンネル 1 本。`Drop` で必ず畳む。
pub struct Tunnel {
    ctx: egui::Context,
    shared: Arc<Shared>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Tunnel {
    pub fn new(ctx: egui::Context) -> Self {
        Self {
            ctx,
            shared: Arc::new(Shared {
                state: Mutex::new(State::default()),
                stop: AtomicBool::new(true),
                gate: Mutex::new(()),
                cv: Condvar::new(),
            }),
            worker: None,
        }
    }

    /// UI から毎フレーム読んでよい (Mutex 1 回 + clone)。
    pub fn state(&self) -> State {
        lock_ok(&self.shared.state).clone()
    }

    /// トンネルを張る。既に張っていれば畳んでから張り直す。
    ///
    /// `local_port` は [`crate::remote::RemoteServer`] が待ち受けているポート。
    /// 公開ポートは同じ番号にする (URL が予測でき、入力欄が 1 つで済む)。
    pub fn connect(&mut self, target: Target, local_port: u16) {
        self.shutdown();

        let Some(exe) = ssh_path() else {
            {
                let mut st = lock_ok(&self.shared.state);
                st.stage = Stage::Failed(Failure::NoSsh);
                st.target = Some(target);
                st.port = local_port;
                st.last_failure = Some(Failure::NoSsh);
            }
            self.ctx.request_repaint();
            return;
        };

        // 新しい世代を始めるので、停止フラグと状態を作り直す
        self.shared = Arc::new(Shared {
            state: Mutex::new(State {
                stage: Stage::Connecting,
                attempt: 0,
                target: Some(target.clone()),
                port: local_port,
                last_failure: None,
            }),
            stop: AtomicBool::new(false),
            gate: Mutex::new(()),
            cv: Condvar::new(),
        });
        self.ctx.request_repaint();

        let shared = Arc::clone(&self.shared);
        let ctx = self.ctx.clone();
        self.worker = std::thread::Builder::new()
            .name("zv-ssh-tunnel".into())
            .spawn(move || supervise(shared, ctx, exe, target, local_port))
            .ok();
        if self.worker.is_none() {
            lock_ok(&self.shared.state).stage = Stage::Failed(Failure::Other);
            self.ctx.request_repaint();
        }
    }

    /// トンネルを畳む (ssh をプロセスツリーごと落として待つ)。
    pub fn disconnect(&mut self) {
        self.shutdown();
        let mut st = lock_ok(&self.shared.state);
        *st = State::default();
        drop(st);
        self.ctx.request_repaint();
    }

    /// 監視スレッドを止めて join する。`connect` / `disconnect` / `Drop` の共通部分。
    fn shutdown(&mut self) {
        self.shared.stop_now();
        if let Some(h) = self.worker.take() {
            let _ = h.join();
        }
    }
}

impl Drop for Tunnel {
    /// アプリ終了時に必ず畳む — 置き去りの ssh は踏み台のポートを掴んだまま残る。
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// 監視ループ本体 (専用スレッド)。落ちたら指数バックオフで張り直す。
fn supervise(
    shared: Arc<Shared>,
    ctx: egui::Context,
    exe: PathBuf,
    target: Target,
    port: u16,
) {
    let args = ssh_args(&target, port, port);
    let mut attempt = 0u32;

    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        shared.set(&ctx, |s| {
            s.stage = Stage::Connecting;
            s.attempt = attempt;
        });

        let failure = match run_once(&shared, &ctx, &exe, &args) {
            Ok(f) => f,
            Err(_) => Failure::NoSsh,
        };
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }

        // 一度でも確立していたら再試行の回数を数え直す
        // (安定して繋がっていた回線が一瞬切れただけ、を「打ち切り」にしない)
        if lock_ok(&shared.state).stage == Stage::Connected {
            attempt = 0;
        }

        shared.set(&ctx, |s| s.last_failure = Some(failure));
        if !failure.retryable() {
            shared.set(&ctx, |s| s.stage = Stage::Failed(failure));
            return;
        }
        attempt += 1;
        let Some(delay) = backoff(attempt) else {
            shared.set(&ctx, |s| {
                s.stage = Stage::Failed(failure);
                s.attempt = attempt.saturating_sub(1);
            });
            return;
        };
        shared.set(&ctx, |s| {
            s.stage = Stage::Connecting;
            s.attempt = attempt;
        });
        if shared.wait(delay) {
            return;
        }
    }
}

/// ssh を 1 回起動して、終わるまで面倒を見る。戻り値 = 終わった理由。
fn run_once(
    shared: &Arc<Shared>,
    ctx: &egui::Context,
    exe: &std::path::Path,
    args: &[String],
) -> std::io::Result<Failure> {
    let mut cmd = crate::procx::hidden_command(exe);
    cmd.args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    // unix: 独立したプロセスグループへ。こうしないと kill_tree が
    // 自分のプロセスグループ (= Zaivern 本体) を撃つ。
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let mut child = cmd.spawn()?;
    let pid = child.id();

    // stderr は別スレッドで読む。読まないとパイプが詰まって ssh が止まる
    // (設計原則 2: 見ていない出力のために生産者を止めない)。
    let tail = Arc::new(Mutex::new(String::new()));
    let mut reader: Option<std::thread::JoinHandle<()>> = child.stderr.take().and_then(|err| {
        let tail = Arc::clone(&tail);
        let shared = Arc::clone(shared);
        let ctx = ctx.clone();
        std::thread::Builder::new()
            .name("zv-ssh-stderr".into())
            .spawn(move || {
                let mut r = BufReader::new(err);
                let mut buf = Vec::new();
                while r.read_until(b'\n', &mut buf).unwrap_or(0) > 0 {
                    let line = String::from_utf8_lossy(&buf).to_string();
                    buf.clear();
                    if forward_established(&line) {
                        shared.set(&ctx, |s| {
                            if s.stage == Stage::Connecting {
                                s.stage = Stage::Connected;
                                s.attempt = 0;
                            }
                        });
                    }
                    let mut t = lock_ok(&tail);
                    t.push_str(&line);
                    if t.len() > TAIL_MAX {
                        let cut = t.len() - TAIL_MAX;
                        *t = t[cut..].to_string();
                    }
                }
            })
            .ok()
    });

    // 監視: 切断されるか ssh が終わるまで。ポーリングは接続中だけ・0.7 秒刻み。
    let started = Instant::now();
    loop {
        if shared.wait(TICK) {
            // ユーザーが切断した — **ツリーごと**落とす。直接の子だけだと
            // ssh の子孫がパイプを握ったまま残る。
            crate::procx::kill_tree(pid);
            let _ = child.wait();
            if let Some(h) = reader.take() {
                let _ = h.join();
            }
            return Ok(Failure::Other);
        }
        match child.try_wait()? {
            Some(_) => break,
            None => {
                // 合図行を取りこぼしても、猶予を越えて生きていれば確立とみなす
                if started.elapsed() >= GRACE {
                    shared.set(ctx, |s| {
                        if s.stage == Stage::Connecting {
                            s.stage = Stage::Connected;
                            s.attempt = 0;
                        }
                    });
                }
            }
        }
    }

    if let Some(h) = reader.take() {
        let _ = h.join();
    }
    let text = lock_ok(&tail).clone();
    Ok(classify(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 接続先パース (テーブル) ──────────────────────────────────────

    #[test]
    fn 接続先パースの表() {
        let t = |u: Option<&str>, h: &str, p: Option<u16>| {
            Ok(Target {
                user: u.map(|s| s.to_string()),
                host: h.to_string(),
                port: p,
            })
        };
        let table: &[(&str, Result<Target, TargetError>)] = &[
            // 正常系
            ("user@example.com", t(Some("user"), "example.com", None)),
            ("user@example.com:2222", t(Some("user"), "example.com", Some(2222))),
            ("example.com", t(None, "example.com", None)),
            ("example.com:22", t(None, "example.com", Some(22))),
            ("  user@example.com  ", t(Some("user"), "example.com", None)),
            ("root@192.168.1.10", t(Some("root"), "192.168.1.10", None)),
            ("ubuntu@bastion-1.internal_x", t(Some("ubuntu"), "bastion-1.internal_x", None)),
            // IPv6
            ("user@[::1]:2222", t(Some("user"), "::1", Some(2222))),
            ("user@[2001:db8::1]", t(Some("user"), "2001:db8::1", None)),
            ("[::1]:22", t(None, "::1", Some(22))),
            ("::1", t(None, "::1", None)),
            ("user@::1", t(Some("user"), "::1", None)),
            ("2001:db8::1", t(None, "2001:db8::1", None)),
            // 異常系
            ("", Err(TargetError::Empty)),
            ("   ", Err(TargetError::Empty)),
            ("@example.com", Err(TargetError::BadUser)),
            ("a@b@example.com", Err(TargetError::BadUser)),
            ("user@", Err(TargetError::MissingHost)),
            ("user@example.com:", Err(TargetError::BadPort)),
            ("user@example.com:0", Err(TargetError::BadPort)),
            ("user@example.com:99999", Err(TargetError::BadPort)),
            ("user@example.com:abc", Err(TargetError::BadPort)),
            ("user@exam ple.com", Err(TargetError::BadHost)),
            ("user@[::1", Err(TargetError::BadHost)),
            ("user@[zzz]:22", Err(TargetError::BadHost)),
            ("user@[::1]22", Err(TargetError::BadPort)),
            ("user@ex/ample.com", Err(TargetError::BadHost)),
            ("user@:::::", Err(TargetError::BadHost)),
        ];
        for (input, want) in table {
            assert_eq!(&parse_target(input), want, "入力 {input:?}");
        }
    }

    #[test]
    fn 接続先の表示は入力欄へ戻せる形() {
        for s in [
            "user@example.com",
            "user@example.com:2222",
            "example.com",
            "user@[::1]:2222",
            "::1",
        ] {
            let t = parse_target(s).expect("パースできる");
            let round = parse_target(&t.display()).expect("表示を読み直せる");
            assert_eq!(t, round, "{s} の往復");
        }
    }

    #[test]
    fn ipv6はurlで角括弧に包む() {
        assert_eq!(parse_target("user@[::1]").unwrap().url_host(), "[::1]");
        assert_eq!(parse_target("example.com").unwrap().url_host(), "example.com");
        // ssh へ渡す側は角括弧なし
        assert_eq!(parse_target("user@[::1]").unwrap().dest(), "user@::1");
        assert_eq!(parse_target("example.com").unwrap().dest(), "example.com");
    }

    // ── ssh エラー分類 (テーブル) ────────────────────────────────────

    #[test]
    fn ssh_エラー分類の表() {
        let table: &[(&str, Failure)] = &[
            (
                "user@example.com: Permission denied (publickey).",
                Failure::AuthDenied,
            ),
            (
                "Permission denied, please try again.\nuser@h: Permission denied (publickey,password).",
                Failure::NeedsInteractive,
            ),
            (
                "user@h: Permission denied (publickey,keyboard-interactive).",
                Failure::NeedsInteractive,
            ),
            (
                "Received disconnect from 1.2.3.4 port 22:2: Too many authentication failures",
                Failure::AuthDenied,
            ),
            (
                "ssh: Could not resolve hostname nope.invalid: nodename nor servname provided",
                Failure::UnknownHost,
            ),
            (
                "ssh: Could not resolve hostname x: Name or service not known",
                Failure::UnknownHost,
            ),
            (
                "ssh: connect to host 127.0.0.1 port 1: Connection refused",
                Failure::Refused,
            ),
            (
                "ssh: connect to host example.com port 22: Operation timed out",
                Failure::Timeout,
            ),
            (
                "Timeout, server example.com not responding.",
                Failure::Timeout,
            ),
            (
                "ssh: connect to host x port 22: Network is unreachable",
                Failure::Unreachable,
            ),
            (
                "ssh: connect to host x port 22: No route to host",
                Failure::Unreachable,
            ),
            ("Host key verification failed.", Failure::HostKey),
            (
                "@@@ WARNING: REMOTE HOST IDENTIFICATION HAS CHANGED! @@@",
                Failure::HostKey,
            ),
            (
                "Warning: remote port forwarding failed for listen port 8899",
                Failure::PortInUse,
            ),
            (
                "bind: Address already in use\ncannot listen to port: 8899",
                Failure::PortInUse,
            ),
            ("", Failure::Other),
            ("debug1: Entering interactive session.", Failure::Other),
            // 具体的な行が混ざっていても、そちらを優先する
            (
                "Warning: remote port forwarding failed for listen port 8899\n\
                 Permission denied (publickey).",
                Failure::PortInUse,
            ),
        ];
        for (raw, want) in table {
            assert_eq!(&classify(raw), want, "stderr {raw:?}");
        }
    }

    #[test]
    fn 時間で直らない失敗は再試行しない() {
        let table: &[(Failure, bool)] = &[
            (Failure::NoSsh, false),
            (Failure::AuthDenied, false),
            (Failure::NeedsInteractive, false),
            (Failure::UnknownHost, false),
            (Failure::HostKey, false),
            (Failure::Other, false),
            (Failure::Refused, true),
            (Failure::Timeout, true),
            (Failure::PortInUse, true),
            (Failure::Unreachable, true),
        ];
        for (f, want) in table {
            assert_eq!(f.retryable(), *want, "{f:?}");
            assert!(!f.headline().is_empty());
            assert!(!f.hint().is_empty());
        }
    }

    #[test]
    fn 転送確立の合図を見分ける() {
        let table: &[(&str, bool)] = &[
            (
                "debug1: remote forward success for: listen 8899, connect 127.0.0.1:8899",
                true,
            ),
            ("debug1: All remote forwarding requests processed", true),
            (
                "Allocated port 54321 for remote forward to 127.0.0.1:8899",
                true,
            ),
            ("debug1: Authentication succeeded (publickey).", false),
            ("debug1: Local connections to LOCALHOST:8899 forwarded", false),
            ("Warning: remote port forwarding failed for listen port 8899", false),
            ("", false),
        ];
        for (line, want) in table {
            assert_eq!(forward_established(line), *want, "{line:?}");
        }
    }

    // ── バックオフ (テーブル) ────────────────────────────────────────

    #[test]
    fn バックオフの表() {
        let table: &[(u32, Option<u64>)] = &[
            (0, None),
            (1, Some(2)),
            (2, Some(4)),
            (3, Some(8)),
            (4, Some(16)),
            (5, Some(30)),
            (6, None),
            (99, None),
            (u32::MAX, None),
        ];
        for (attempt, want) in table {
            assert_eq!(
                backoff(*attempt).map(|d| d.as_secs()),
                *want,
                "attempt {attempt}"
            );
        }
        // 単調非減少 + 上限内で必ず止まる
        let total: u64 = (1..=MAX_RETRIES)
            .map(|a| backoff(a).unwrap().as_secs())
            .sum();
        assert!(total <= 120, "打ち切りまでが長すぎる: {total} 秒");
        assert!(backoff(MAX_RETRIES + 1).is_none(), "上限を越えたら諦める");
    }

    // ── ssh 引数 (テーブル) ──────────────────────────────────────────

    #[test]
    fn ssh_引数の表() {
        let t = parse_target("user@example.com").unwrap();
        let a = ssh_args(&t, 8899, 8899);
        let joined = a.join(" ");
        for must in [
            "-N",
            "-T",
            "BatchMode=yes",
            "ExitOnForwardFailure=yes",
            "ServerAliveInterval=30",
            "ServerAliveCountMax=3",
            "-R 8899:127.0.0.1:8899",
            "user@example.com",
        ] {
            assert!(joined.contains(must), "{must} が引数に無い: {joined}");
        }
        // 踏み台側の bind アドレスは書かない (GatewayPorts はユーザーが決める)
        assert!(
            !joined.contains("0.0.0.0:8899:") && !joined.contains("*:8899:"),
            "インターネットへ晒す bind を既定にしてはいけない: {joined}"
        );
        // 接続先は必ず最後 (ssh は以降をリモートコマンドとして扱う)
        assert_eq!(a.last().map(|s| s.as_str()), Some("user@example.com"));

        // ポート指定は -p で渡す (接続先には残さない)
        let t = parse_target("user@example.com:2222").unwrap();
        let a = ssh_args(&t, 8900, 8899);
        assert!(a.windows(2).any(|w| w == ["-p", "2222"]), "{a:?}");
        assert_eq!(a.last().map(|s| s.as_str()), Some("user@example.com"));
        assert!(a.contains(&"8900:127.0.0.1:8899".to_string()), "{a:?}");

        // IPv6 は角括弧なしで最後に置く
        let t = parse_target("user@[::1]:2222").unwrap();
        let a = ssh_args(&t, 8899, 8899);
        assert_eq!(a.last().map(|s| s.as_str()), Some("user@::1"));
    }

    #[test]
    fn ssh_l_コマンドの表() {
        let table: &[(&str, u16, u16, &str)] = &[
            (
                "user@example.com",
                8899,
                8899,
                "ssh -N -L 8899:127.0.0.1:8899 user@example.com",
            ),
            (
                "user@example.com:2222",
                8899,
                8899,
                "ssh -N -L 8899:127.0.0.1:8899 -p 2222 user@example.com",
            ),
            ("bastion", 9000, 8899, "ssh -N -L 8899:127.0.0.1:9000 bastion"),
        ];
        for (raw, pub_p, local_p, want) in table {
            let t = parse_target(raw).unwrap();
            assert_eq!(&ssh_l_command(&t, *pub_p, *local_p), want);
        }
    }

    // ── 状態機械 ────────────────────────────────────────────────────

    #[test]
    fn 段の見出しは全て埋まっている() {
        for s in [
            Stage::Disconnected,
            Stage::Connecting,
            Stage::Connected,
            Stage::Failed(Failure::AuthDenied),
        ] {
            assert!(!s.label().is_empty(), "{s:?}");
        }
    }

    #[test]
    fn 接続済みのときだけスマホurlを出す() {
        let mut st = State {
            stage: Stage::Connecting,
            attempt: 0,
            target: parse_target("user@example.com").ok(),
            port: 8899,
            last_failure: None,
        };
        assert!(st.phone_url("abc").is_none(), "接続中は URL を出さない");
        st.stage = Stage::Connected;
        assert_eq!(
            st.phone_url("abc").unwrap(),
            "http://example.com:8899/?t=abc"
        );
        // IPv6 は角括弧つき
        st.target = parse_target("user@[2001:db8::1]").ok();
        assert_eq!(
            st.phone_url("abc").unwrap(),
            "http://[2001:db8::1]:8899/?t=abc"
        );
        // 相手が無ければ URL も無い
        st.target = None;
        assert!(st.phone_url("abc").is_none());
    }

    #[test]
    fn 新品のトンネルは切断状態で_drop_しても固まらない() {
        let t = Tunnel::new(egui::Context::default());
        let s = t.state();
        assert_eq!(s.stage, Stage::Disconnected);
        assert_eq!(s.attempt, 0);
        assert!(s.target.is_none());
        drop(t);
    }

    #[test]
    fn 切断は何度呼んでも安全() {
        let mut t = Tunnel::new(egui::Context::default());
        t.disconnect();
        t.disconnect();
        assert_eq!(t.state().stage, Stage::Disconnected);
    }

    /// 列挙が返す画面文字列は、同梱の英語辞書 (english-mode) にも載っていること。
    /// 載せ忘れると英語モードでここだけ日本語が残る — 見た目で気付きにくいので
    /// テストで塞ぐ。
    #[test]
    fn 画面に出す文字列は英語辞書にも載っている() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/plugins/english-mode/lang");
        let dict = crate::i18n::load_dict(&dir).expect("同梱辞書が読める");

        let mut want: Vec<&'static str> = vec![
            crate::remote::Bind::Lan.label(),
            crate::remote::Bind::Loopback.label(),
            // OS 別の案内は、いま走っている OS の 1 本だけ当たる
            install_hint(),
            HOST_HINT,
            SSH_L_HINT,
            GATEWAY_NOTE,
        ];
        for s in [
            Stage::Disconnected,
            Stage::Connecting,
            Stage::Connected,
            Stage::Failed(Failure::Other),
        ] {
            want.push(s.label());
        }
        for f in [
            Failure::NoSsh,
            Failure::AuthDenied,
            Failure::NeedsInteractive,
            Failure::UnknownHost,
            Failure::Refused,
            Failure::Timeout,
            Failure::HostKey,
            Failure::PortInUse,
            Failure::Unreachable,
            Failure::Other,
        ] {
            want.push(f.headline());
            want.push(f.hint());
        }
        for e in [
            TargetError::Empty,
            TargetError::BadUser,
            TargetError::MissingHost,
            TargetError::BadHost,
            TargetError::BadPort,
        ] {
            want.push(e.msg());
        }
        let missing: Vec<&&str> = want.iter().filter(|k| !dict.contains_key(**k)).collect();
        assert!(missing.is_empty(), "英語辞書に無い文字列: {missing:#?}");
    }

    /// **実測**: 本物の `ssh` を起動し、閉じたポートへの接続が
    /// [`Failure::Refused`] に畳まれることを確かめる。
    /// `ssh` が無い環境 (ごく一部の CI) では黙って飛ばす。
    #[test]
    fn 実際の_ssh_を起動して分類が効く() {
        let Some(exe) = ssh_path() else {
            eprintln!("ssh が無いので skip");
            return;
        };
        // ポート 1 は誰も listen していない → 即 Connection refused
        let t = parse_target("127.0.0.1:1").expect("パースできる");
        let args = ssh_args(&t, 8899, 8899);
        let mut cmd = crate::procx::hidden_command(&exe);
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped());
        let out = cmd.output().expect("ssh を起動できる");
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(!out.status.success(), "閉じたポートへ繋がってはいけない");
        assert_eq!(classify(&err), Failure::Refused, "stderr: {err}");
    }

    /// **実測 (手動)**: 本物の踏み台へ張って、段が `接続済み` まで進み、
    /// 切断で ssh がツリーごと消えることを確かめる。
    ///
    /// 到達可能な踏み台が要るので、環境変数がある時だけ走る:
    ///
    /// ```sh
    /// # 鍵は ssh-agent、ホスト鍵は ~/.ssh/known_hosts に入れておくこと
    /// ZAIVERN_SSH_E2E='user@bastion.example:22' cargo test -- tunnel::実際に
    /// ```
    #[test]
    fn 実際にトンネルを張って段が進み切断で消える() {
        let Ok(raw) = std::env::var("ZAIVERN_SSH_E2E") else {
            eprintln!("ZAIVERN_SSH_E2E が無いので skip (到達可能な踏み台が要る)");
            return;
        };
        let target = parse_target(&raw).expect("ZAIVERN_SSH_E2E をパースできる");

        // 踏み台側で開く公開ポート = ここで確保して即手放した空きポート
        let port = {
            let l = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("空きポート");
            l.local_addr().expect("addr").port()
        };

        let mut t = Tunnel::new(egui::Context::default());
        t.connect(target, port);

        let deadline = Instant::now() + Duration::from_secs(25);
        while Instant::now() < deadline {
            match t.state().stage {
                Stage::Connected => break,
                Stage::Failed(f) => panic!("接続に失敗: {f:?} — {}", f.headline()),
                _ => std::thread::sleep(Duration::from_millis(200)),
            }
        }
        assert_eq!(t.state().stage, Stage::Connected, "接続済みまで進むこと");

        // 踏み台側で公開ポートが開いている = 転送が本当に確立している証拠
        let addr = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        assert!(
            std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(2)).is_ok(),
            "公開ポート {port} が開いていない"
        );

        t.disconnect();
        assert_eq!(t.state().stage, Stage::Disconnected);
        // ssh がツリーごと消えていれば、公開ポートも閉じる
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut closed = false;
        while Instant::now() < deadline {
            if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_err() {
                closed = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        assert!(closed, "切断しても公開ポート {port} が開いたまま (ssh が残っている)");
    }
}
