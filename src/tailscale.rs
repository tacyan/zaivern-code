//! Tailscale (WireGuard の VPN) 経由のスマホ接続 — 「同じ Wi-Fi」を外す 3 本目の経路。
//!
//! LAN モード ([`crate::remote::Bind::Lan`]) は同じ Wi-Fi が前提、SSH トンネル
//! ([`crate::tunnel`]) は踏み台になるサーバが要る。Tailscale を入れている人は
//! **PC とスマホが同じ tailnet に居るだけ**でよく、経路も鍵も NAT 越えも
//! Tailscale が持つ。こちら側の仕事は 2 つしかない。
//!
//! 1. 自分の tailnet IP (`100.64.0.0/10`) を見つける
//! 2. そこ **と `127.0.0.1`** だけで待ち受ける ([`crate::remote::Bind::Tailscale`])
//!
//! 設計原則 5 (ハンドラは 1 面、トランスポートは多数) の 3 例目で、HTTP の
//! ハンドラは [`crate::remote`] のまま 1 面、変わるのは待ち受け先だけである。
//!
//! ## `tailscale` コマンドを実行しない (実測にもとづく判断)
//!
//! 状態は CLI (`tailscale status --json`) からも取れる。**が、使わない。**
//! macOS の Tailscale.app が置く `/usr/local/bin/tailscale` は
//!
//! ```sh
//! #!/bin/sh
//! /Applications/Tailscale.app/Contents/MacOS/Tailscale "$@"
//! ```
//!
//! という `exec` しないシェルラッパで、デーモンに繋がらないと**返ってこない**。
//! 実測 (2026-08-16 / Tailscale.app は起動済み・tailnet 未接続):
//!
//! - `tailscale status` が **120 秒を過ぎても無反応** (出力 0 バイト)
//! - ラッパの `/bin/sh` を kill しても、孫の
//!   `/Applications/Tailscale.app/Contents/MacOS/Tailscale status` が
//!   **10 分以上ぶら下がったまま生き残った**
//!
//! CLAUDE.md の「プロセスを殺すときは必ずツリーごと。直接の子だけを kill すると、
//! シェルが `exec` せずに起動した孫がパイプを握ったまま残り、読み取りの join が
//! 孫の寿命まで戻らない (= UI が固まる)」の実例そのものである。
//! **いちばん状態を知りたい局面 (繋がっていないとき) が、いちばん固まる局面**
//! なので、この経路は最初から採らない。
//!
//! ## 代わりに使うもの — カーネルの経路表
//!
//! UDP ソケットを tailnet の宛先へ `connect` すると、**パケットは 1 バイトも
//! 出ないまま**経路が引かれ、`local_addr()` が「その宛先へ送るときの送信元」
//! = 自分の tailnet IP を返す。システムコール数回で終わり、経路が無ければ
//! 即座にエラーで返る (待ちもタイムアウトも無い)。
//!
//! 宛先は 2 つ使う。
//!
//! - `100.100.100.100` (MagicDNS): tailnet が上がっていれば必ず経路がある。
//!   ただし **`100.64.0.0/10` は CGNAT 用の空間**でもあり、キャリアのテザリングや
//!   一部の Wi-Fi はここからアドレスを配る。既定経路へ落ちると素の LAN IP が
//!   返るので、これ「だけ」では Tailscale の証拠にならない。
//! - `fd7a:115c:a1e0::/48` (tailnet の IPv6 ULA): **この /48 を自分の
//!   インタフェースに載せるのは Tailscale だけ**。既定経路 (`::/0`) へ落ちた
//!   場合の送信元はグローバル IPv6 になるので、送信元がこの /48 に入っていたら
//!   Tailscale 以外にありえない。
//!
//! 判定は [`decide`] に純関数として切り出してある (I/O を持たないので表で固定できる)。

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, UdpSocket};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// MagicDNS の固定アドレス。tailnet が上がっているときだけ経路がある。
pub const MAGIC_DNS_V4: Ipv4Addr = Ipv4Addr::new(100, 100, 100, 100);

/// tailnet の IPv6 (ULA) 側へ引く経路確認用の宛先。誰も居なくてよい
/// (UDP の `connect` はパケットを出さない)。
pub const TAILNET_V6_PROBE: Ipv6Addr = Ipv6Addr::new(0xfd7a, 0x115c, 0xa1e0, 0, 0, 0, 0, 0x53);

/// `100.64.0.0/10` — Tailscale が配る IPv4 の範囲 (RFC 6598 の CGNAT 空間)。
///
/// **CGNAT と共用の空間**なので、これだけでは Tailscale の証拠にならない
/// ([`decide`] が導入の有無や IPv6 側と突き合わせる)。
pub fn is_tailnet_v4(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

/// `fd7a:115c:a1e0::/48` — Tailscale の IPv6 (ULA)。
/// この /48 を自分のインタフェースに載せるのは Tailscale だけ。
pub fn is_tailnet_v6(ip: Ipv6Addr) -> bool {
    let s = ip.segments();
    s[0] == 0xfd7a && s[1] == 0x115c && s[2] == 0xa1e0
}

/// どちらのアドレス族でも tailnet かどうかを見る。
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_tailnet_v4(v4),
        IpAddr::V6(v6) => is_tailnet_v6(v6),
    }
}

// ─── 状態 ────────────────────────────────────────────────────────────

/// いま Tailscale がどの段にいるか。**UI には必ずこの段を出す**
/// (「繋がりません」だけでは、入っていないのか止まっているのか分からない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// 見つからない (入っていない)
    Missing,
    /// 入っているが tailnet に繋がっていない (ログアウト / 停止中)
    Down,
    /// 繋がっている — この IP で待ち受けられる
    Up,
}

impl Stage {
    /// 段の見出し (呼び出し側で `tr()` を通すこと)。
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Missing => "Tailscale が見つかりません",
            Stage::Down => "Tailscale が tailnet に繋がっていません",
            Stage::Up => "Tailscale に繋がっています",
        }
    }

    /// 次にすること 1 行 (呼び出し側で `tr()` を通すこと)。
    pub fn hint(&self) -> &'static str {
        match self {
            Stage::Missing => install_hint(),
            Stage::Down => "Tailscale アプリを開いてログイン (または「接続」) してください",
            Stage::Up => "スマホも同じ tailnet に入れておけば、Wi-Fi が違っても繋がります",
        }
    }
}

/// 検出結果ひとつ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
    pub stage: Stage,
    /// 待ち受けに使える tailnet IP ([`Stage::Up`] のときだけ `Some`)。
    pub ip: Option<IpAddr>,
}

impl Default for Status {
    fn default() -> Self {
        Self {
            stage: Stage::Missing,
            ip: None,
        }
    }
}

impl Status {
    /// tailnet で待ち受けられるか。
    pub fn ready(&self) -> bool {
        self.stage == Stage::Up && self.ip.is_some()
    }
}

/// 経路の手がかり 2 つと「入っているか」から段を決める **純関数**。
///
/// 引数は経路表から返ってきた**生の送信元アドレス**で、範囲の検査もここで行う
/// (検査を呼び出し側に置くと、表で固定できるのは検査済みの値だけになる)。
///
/// - IPv6 側が `fd7a:115c:a1e0::/48` → **Tailscale で確定** (他に出しようがない)
/// - IPv4 側が `100.64.0.0/10` かつ Tailscale が入っている → 繋がっているとみなす
/// - IPv4 側が `100.64.0.0/10` だが入っていない → **CGNAT の LAN**。乗らない
/// - どちらも引けない → 入っていれば [`Stage::Down`]、無ければ [`Stage::Missing`]
pub fn decide(v4: Option<Ipv4Addr>, v6: Option<Ipv6Addr>, installed: bool) -> Status {
    let v4 = v4.filter(|ip| is_tailnet_v4(*ip));
    let v6 = v6.filter(|ip| is_tailnet_v6(*ip));
    // IPv6 が引けたなら Tailscale で確定。待ち受けアドレスは v4 を優先する
    // (URL に入れたときに短く、IPv6 を切っているスマホからも届く)。
    if v6.is_some() {
        let ip = v4
            .map(IpAddr::V4)
            .or_else(|| v6.map(IpAddr::V6))
            .expect("v6 が Some なのでここは必ず取れる");
        return Status {
            stage: Stage::Up,
            ip: Some(ip),
        };
    }
    match (v4, installed) {
        (Some(ip), true) => Status {
            stage: Stage::Up,
            ip: Some(IpAddr::V4(ip)),
        },
        // 入っていないのに 100.64/10 が引けた = CGNAT の LAN。Tailscale ではない
        (Some(_), false) | (None, false) => Status {
            stage: Stage::Missing,
            ip: None,
        },
        (None, true) => Status {
            stage: Stage::Down,
            ip: None,
        },
    }
}

// ─── 実測 (I/O を持つのはここだけ) ───────────────────────────────────

/// 宛先へ「送るとしたらどの送信元になるか」をカーネルに聞く。
///
/// UDP の `connect` は**パケットを出さない** — 経路を引いてソケットに
/// 相手を覚えさせるだけなので、相手が居なくても、居ない方が速くてもよい。
fn source_for(dst: (IpAddr, u16)) -> Option<IpAddr> {
    let any: IpAddr = match dst.0 {
        IpAddr::V4(_) => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        IpAddr::V6(_) => IpAddr::V6(Ipv6Addr::UNSPECIFIED),
    };
    let s = UdpSocket::bind((any, 0)).ok()?;
    s.connect(dst).ok()?;
    s.local_addr().ok().map(|a| a.ip())
}

/// tailnet の IPv4 側の送信元 (未検査の生の値)。
fn probe_v4() -> Option<Ipv4Addr> {
    match source_for((IpAddr::V4(MAGIC_DNS_V4), 53))? {
        IpAddr::V4(v4) => Some(v4),
        IpAddr::V6(_) => None,
    }
}

/// tailnet の IPv6 側の送信元 (未検査の生の値)。
fn probe_v6() -> Option<Ipv6Addr> {
    match source_for((IpAddr::V6(TAILNET_V6_PROBE), 53))? {
        IpAddr::V6(v6) => Some(v6),
        IpAddr::V4(_) => None,
    }
}

/// 待ち受けに使う tailnet IP。無ければ `None`。
///
/// [`crate::remote::RemoteServer`] は**待ち受ける直前にこれを引き直す** —
/// 画面に出ている検出結果は数秒前のもので、その間に Tailscale が落ちていれば
/// bind は失敗する。古い値で bind すると理由の分からないエラーになる。
pub fn listen_ip() -> Option<IpAddr> {
    probe().ip
}

/// いまの状態を測る (システムコール数回 + 導入の有無)。
pub fn probe() -> Status {
    decide(probe_v4(), probe_v6(), installed())
}

// ─── 導入の有無 (実行はしない) ───────────────────────────────────────

/// Tailscale が入っているか。**見つけるのは「入っているか」を言うためだけで、
/// 実行はしない** (モジュール冒頭の実測を参照)。
pub fn installed() -> bool {
    install_path().is_some()
}

/// Tailscale の実行ファイル / アプリの場所。まず PATH、次に OS の既定の置き場。
///
/// 置き場は**環境変数から導く** (`%ProgramFiles%` / `$HOME`)。
/// macOS の `/Applications` だけは OS が定める固定の場所なので直接見る。
pub fn install_path() -> Option<PathBuf> {
    if let Some(p) = crate::shellenv::which("tailscale") {
        return Some(p);
    }
    candidates().into_iter().find(|p| p.exists())
}

/// PATH に無いときに見に行く場所 (OS ごと)。
fn candidates() -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = Vec::new();
    #[cfg(target_os = "macos")]
    {
        // App Store 版・スタンドアロン版とも .app の中に CLI を同梱する。
        // /Applications は OS が定める場所なので、ここだけは直接見てよい。
        v.push(PathBuf::from("/Applications/Tailscale.app"));
        if let Some(home) = dirs::home_dir() {
            v.push(home.join("Applications/Tailscale.app"));
        }
    }
    #[cfg(windows)]
    {
        // 32/64bit で環境変数が変わるので両方見る (直書きしない)。
        for key in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Some(dir) = std::env::var_os(key) {
                v.push(PathBuf::from(dir).join("Tailscale").join("tailscale.exe"));
            }
        }
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        // PATH が痩せている環境 (GUI から起動した .desktop など) の保険。
        // tailscaled のソケットは「入っていて動いている」の強い証拠になる。
        for p in [
            "/usr/bin/tailscale",
            "/usr/local/bin/tailscale",
            "/snap/bin/tailscale",
            "/var/run/tailscale/tailscaled.sock",
            "/run/tailscale/tailscaled.sock",
        ] {
            v.push(PathBuf::from(p));
        }
    }
    v
}

/// 入っていないときの、OS 別の入れ方 1 行 (呼び出し側で `tr()` を通すこと)。
pub fn install_hint() -> &'static str {
    if cfg!(windows) {
        "tailscale.com からインストーラを入れてサインインしてください"
    } else if cfg!(target_os = "macos") {
        "App Store か tailscale.com から Tailscale を入れてサインインしてください"
    } else {
        "curl -fsSL https://tailscale.com/install.sh | sh でインストールできます"
    }
}

// ─── 画面に出す長文 ──────────────────────────────────────────────────
//
// UI (`app/remote_api.rs`) から使う長い説明は、**辞書キーそのもの**なので
// const にして 1 箇所に置く。呼び出し側へ直書きすると、辞書側の文字
// (全角空白など) が 1 文字ずれただけで**英語モードのときだけ日本語が残る** —
// 見た目で気付けない壊れ方をする。const なら下のテストがバイト単位で見張れる。

/// 「Tailscale で待ち受ける」のホバー説明。
pub const SWITCH_HINT: &str = "tailnet の IP と 127.0.0.1 だけで待ち受け直します。\n\
     喫茶店や空港の Wi-Fi に居ても、その LAN からは\n\
     ポートが見えません (経路も鍵も Tailscale が持ちます)";

/// 「同じ Wi-Fi に戻す」のホバー説明。
pub const BACK_HINT: &str = "0.0.0.0 で待ち受け直します\n\
     (同じ Wi-Fi のスマホから直接繋がるようになります)";

/// Tailscale で待ち受けている間に出す注意 (1 段落)。
///
/// **これを出さないと「同じ Wi-Fi なのに繋がらない」で必ず詰まる。**
/// tailnet に絞るとは、同じ部屋のスマホでも Tailscale に入っていなければ
/// 届かない、ということである。
pub const ONLY_TAILNET_NOTE: &str = "※ いまは tailnet と PC 自身からだけ届きます。\n\u{3000}\
     同じ Wi-Fi でも Tailscale に入っていないスマホからは繋がりません";

/// まだ 1 度も届いていないときの案内 (Tailscale モード)。
///
/// LAN の文面 (ファイアウォール / 同じ Wi-Fi / プライバシーセパレータ) を
/// そのまま出すと、**直しようのないところを疑わせる**ので分ける。
pub const NO_REACH: &str = "📶 まだスマホからの接続はありません\n\u{3000}\
     スマホ側でも Tailscale を繋いで、同じ tailnet に\n\u{3000}\
     入れてください (ACL で塞いでいないかも確認)";

/// QR の上に出す 1 行。
pub const HEADLINE: &str = "Tailscale 経由 — 同じ tailnet のスマホで QR を読み取って接続";

/// QR の上に出す 1 行 (HTTPS 経由)。
pub const HTTPS_HEADLINE: &str =
    "Tailscale の HTTPS 経由 — 🎤 音声入力が使えます (QR を読み取って接続)";

/// 「HTTPS で待ち受ける」のホバー説明。
///
/// **なぜ HTTPS が要るのか**を必ず書く。書かないと「暗号化されるだけなら
/// 要らない」と読まれて、音声入力が使えない理由が永久に分からない。
pub const HTTPS_ON_HINT: &str = "Tailscale に TLS を終端させ、tailnet のホスト名で\n\
     本物の証明書 (Let's Encrypt) を出します。スマホから見た URL が\n\
     https になるので、ブラウザの音声認識が動きます\n\
     (平文の http では、どの端末でも仕様上ぜったいに動きません)";

/// 「HTTPS をやめる」のホバー説明。
pub const HTTPS_OFF_HINT: &str = "tailnet の HTTPS 公開 (tailscale serve) を解除して、\n\
     同じ Wi-Fi から繋げるように戻します";

/// 1 回目の接続だけ遅い / 失敗することの説明。
///
/// **これを出さないと「繋がらない」と判断されて終わる。** Tailscale は
/// 最初の接続で Let's Encrypt へ証明書を取りに行くので、そこだけ待ちがある
/// (実測: 温めずに叩くと 1 回目の TLS ハンドシェイクが失敗し、2 回目は 34ms)。
pub const FIRST_CONNECT_NOTE: &str = "※ 最初の 1 回だけ証明書の取得で待つことがあります\n\u{3000}\
     (失敗したらもう一度読み込んでください。2 回目からは一瞬です)";

// ─── 画面から毎フレーム読むための薄いキャッシュ ──────────────────────

/// 測り直す間隔。UI が開いている間だけ引かれるので短くてよい。
/// (`probe` はシステムコール数回だが、毎フレーム PATH を走査させる意味は無い)
pub const TTL: Duration = Duration::from_secs(2);

/// 検出結果のキャッシュ。**アイドル時のコストはゼロ** — 描画された
/// フレームからしか引かれず、スレッドもタイマーも持たない (設計原則 3)。
#[derive(Default)]
pub struct Probe {
    last: Option<Instant>,
    cur: Status,
}

impl Probe {
    /// いまの状態。TTL を過ぎていれば測り直す。
    pub fn get(&mut self) -> Status {
        let stale = self.last.map(|t| t.elapsed() >= TTL).unwrap_or(true);
        if stale {
            self.cur = probe();
            self.last = Some(Instant::now());
        }
        self.cur
    }

    /// 次に読むとき必ず測り直す (待ち受けの切り替え直後など)。
    pub fn invalidate(&mut self) {
        self.last = None;
    }
}

// ══════════════════════════════════════════════════════════════════════
//  HTTPS (tailscale serve) — スマホの音声入力を動かすための唯一の道
//
//  スマホのブラウザの `SpeechRecognition` は **セキュアコンテキスト
//  (`isSecureContext`) でしか動かない**。判定は端末でもブラウザでもなく
//  「その URL が https か localhost か」なので、平文の LAN / tailnet で
//  待ち受けている限り、こちら側のコードを何行書いても 🎤 は生えない。
//
//  Tailscale は tailnet 内のホスト名に**本物の証明書** (Let's Encrypt) を
//  出せる。`tailscale serve --https=443 http://127.0.0.1:<port>` を撃つと
//  tailscaled が 443 で TLS を終端して loopback へ流すので、
//  **スマホ側の JS を 1 バイトも変えずに** `isSecureContext === true` になる。
//
//  ## ここでだけ `tailscale` コマンドを実行する
//
//  モジュール冒頭の「CLI を実行しない」は**状態を知るための常時の経路**の話で
//  ある ([`probe`] は経路表しか見ない)。こちらは**利用者がボタンを押した
//  ときだけ**動き、しかも
//
//  - 撃つ前に [`probe`] で tailnet が上がっていることを確かめ
//    (落ちているときに叩くと 120 秒返ってこない — 冒頭の実測)、
//  - 必ずハードな時限を持たせ、超えたら [`crate::procx::kill_tree`] で
//    **ツリーごと**畳む (直接の子だけ kill すると、`exec` しない
//    シェルラッパの孫が 10 分以上残る)。
//
//  実測 (2026-08-17 / macOS / Tailscale.app 起動済み・tailnet 接続済み):
//  `status --json` は **0.058 秒**で返る (rc=0)。
// ══════════════════════════════════════════════════════════════════════

/// Tailscale が TLS を終端するポート。`tailscale serve --https=<この番号>`。
///
/// **URL には書かない** — https の既定ポートなので、書くと QR が無駄に長くなる。
pub const HTTPS_PORT: u16 = 443;

/// tailnet 側で HTTPS 証明書を有効にする画面。
/// **`CertDomains` が空のときは、ここを開く以外に直しようが無い。**
pub const ADMIN_DNS_URL: &str = "https://login.tailscale.com/admin/dns";

/// `tailscale status` / `serve` 1 回に与える時限。
///
/// 上がっているときは 0.06 秒で返る。ここを超えるのは
/// **デーモンに繋がらなくなった**ときなので、待っても好転しない。
pub const CLI_BUDGET: Duration = Duration::from_secs(10);

/// 証明書を取りに行く 1 回だけの時限。
///
/// 1 回目は Let's Encrypt との往復があるので秒では終わらない
/// (実測: 温めずに `curl` すると **1 回目の TLS ハンドシェイクが失敗**し、
///  2 回目以降は 34ms)。ここが長いのは異常ではない。
pub const CERT_BUDGET: Duration = Duration::from_secs(90);

/// 子プロセスから読み取る上限。`status --json` は peer が多いと数百 KB になる。
const READ_CAP: usize = 4 << 20;

/// HTTPS にできない理由。**「できませんでした」で終わらせないための列挙**。
///
/// 4 通りは**直し方がそれぞれ違う**ので、1 つにまとめてはいけない
/// (入れる / 繋ぐ / 管理コンソールで有効にする / 出力を読む)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HttpsBlock {
    /// Tailscale が入っていない ([`Stage::Missing`]) か、
    /// tailnet に繋がっていない ([`Stage::Down`])。
    NotUp(Stage),
    /// 繋がってはいるが、**実行できる `tailscale` が見つからない**。
    /// (経路表からは検出できるので [`Stage::Up`] とは矛盾しない —
    ///  Linux でデーモンだけ入っている / CLI が PATH の外、など)
    NoCli,
    /// tailnet 側で **HTTPS 証明書が有効になっていない** (`CertDomains` が空)。
    CertsOff,
    /// `status` / `serve` が失敗した。**出力をそのまま持つ**
    /// (要約すると、いちばん知りたい 1 行が消える)。
    Failed(String),
}

impl HttpsBlock {
    /// 見出し 1 行 (呼び出し側で `tr()` を通すこと)。
    pub fn headline(&self) -> &'static str {
        match self {
            HttpsBlock::NotUp(s) => s.label(),
            HttpsBlock::NoCli => "tailscale コマンドが見つかりません",
            HttpsBlock::CertsOff => "この tailnet では HTTPS 証明書が無効です",
            HttpsBlock::Failed(_) => "Tailscale の HTTPS 公開に失敗しました",
        }
    }

    /// 次にすること 1 行 (呼び出し側で `tr()` を通すこと)。
    pub fn hint(&self) -> &'static str {
        match self {
            HttpsBlock::NotUp(s) => s.hint(),
            HttpsBlock::NoCli => "Tailscale アプリは見つかりましたが、CLI を実行できませんでした",
            HttpsBlock::CertsOff => {
                "管理コンソールの DNS 画面で「HTTPS Certificates」を有効にしてください"
            }
            HttpsBlock::Failed(_) => "下の出力がそのままの理由です",
        }
    }

    /// 出力をそのまま見せる必要があるときだけ中身を返す。
    pub fn detail(&self) -> Option<&str> {
        match self {
            HttpsBlock::Failed(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// `tailscale status --json` の本文から、HTTPS で名乗れるドメインを取り出す**純関数**。
///
/// I/O を持たないので表で固定できる (持たせると固定できるのは実行環境の数だけになる)。
///
/// - JSON が読めない → [`HttpsBlock::Failed`] (本文の頭を添える)
/// - `BackendState` が `Running` 以外 → [`HttpsBlock::NotUp`]
///   (経路表と食い違うことがある: ログアウト直後など)
/// - `CertDomains` が空 / 無い → [`HttpsBlock::CertsOff`]
/// - 入っているが host として使えない → [`HttpsBlock::Failed`]
///   (**URL へそのまま入れない** — `/` や `:` が混ざると別の宛先になる)
pub fn parse_cert_domain(json: &str) -> Result<String, HttpsBlock> {
    let v: serde_json::Value = serde_json::from_str(json).map_err(|e| {
        let head: String = json.chars().take(200).collect();
        HttpsBlock::Failed(format!("status --json を読めません: {e}\n{head}"))
    })?;
    if let Some(state) = v.get("BackendState").and_then(|s| s.as_str()) {
        if state != "Running" {
            return Err(HttpsBlock::NotUp(Stage::Down));
        }
    }
    let domains = v.get("CertDomains").and_then(|d| d.as_array());
    let raw: Vec<&str> = domains
        .map(|a| a.iter().filter_map(|d| d.as_str()).collect())
        .unwrap_or_default();
    if raw.is_empty() {
        return Err(HttpsBlock::CertsOff);
    }
    match raw.iter().find(|d| is_plausible_host(d)) {
        Some(d) => Ok(d.trim_end_matches('.').to_string()),
        None => Err(HttpsBlock::Failed(format!(
            "CertDomains にホスト名として使えない値しかありません: {raw:?}"
        ))),
    }
}

/// URL の host 部にそのまま置ける形か。**ここを緩めると URL 全体を
/// 差し替えられる** (`evil.example/` や `host:1234` が混ざる)。
pub fn is_plausible_host(s: &str) -> bool {
    let s = s.trim_end_matches('.');
    !s.is_empty()
        && s.len() <= 253
        && s.contains('.')
        && !s.starts_with('.')
        && !s.contains("..")
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.')
}

/// 実行してよい `tailscale` の実体。
///
/// macOS では **`.app` の中の実体を最優先する**。PATH に置かれる
/// `/usr/local/bin/tailscale` は `exec` しないシェルラッパで、時限で畳むときに
/// 孫が残る (モジュール冒頭の実測)。見つからなければ PATH へ落ちる。
pub fn cli_path() -> Option<PathBuf> {
    for c in candidates() {
        let p = if c.extension().is_some_and(|e| e == "app") {
            // `.app` の内部配置は macOS が定めるものなので、ここだけは組み立てる
            c.join("Contents").join("MacOS").join("Tailscale")
        } else {
            c
        };
        // ソケット (`tailscaled.sock`) は「入っている」の証拠にはなるが
        // **実行してはいけない**。名前で弾く
        let ok_name = p.file_name().is_some_and(|n| {
            let n = n.to_string_lossy().to_ascii_lowercase();
            n == "tailscale" || n == "tailscale.exe"
        });
        if ok_name && p.is_file() {
            return Some(p);
        }
    }
    crate::shellenv::which("tailscale")
}

/// 子プロセスの出力 1 回ぶん。
struct CliOut {
    ok: bool,
    stdout: String,
    stderr: String,
}

impl CliOut {
    /// 画面に出す「そのままの出力」。stderr を先に置く (理由はそちらにある)。
    fn message(&self) -> String {
        let mut s = String::new();
        for part in [self.stderr.trim(), self.stdout.trim()] {
            if !part.is_empty() {
                if !s.is_empty() {
                    s.push('\n');
                }
                s.push_str(part);
            }
        }
        if s.is_empty() {
            s.push_str("(出力なし)");
        }
        s
    }
}

/// 上限付きで読み切るリーダースレッド。**保持は有界、読み取りは EOF まで**
/// (途中で止めると子が書き込みでブロックし、時限まで終わらない)。
fn spawn_capped_reader<R: std::io::Read + Send + 'static>(
    mut r: R,
) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut chunk = [0u8; 8192];
        loop {
            match r.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if buf.len() < READ_CAP {
                        let room = READ_CAP - buf.len();
                        buf.extend_from_slice(&chunk[..n.min(room)]);
                    }
                }
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
}

/// `tailscale` を 1 回起こす。**必ず時限つき**で、超えたらツリーごと畳む。
fn run_capped(exe: &Path, args: &[&str], budget: Duration) -> Result<CliOut, String> {
    let mut cmd = crate::procx::hidden_command(exe);
    for a in args {
        cmd.arg(a);
    }
    cmd.env("NO_COLOR", "1")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 子を独立したプロセスグループへ。こうしないと kill_tree が
        // 孫 (`exec` しないシェルラッパが起こす本体) を取り逃がす。
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().map_err(|e| format!("{}: {e}", exe.display()))?;
    let out_rx = child.stdout.take().map(spawn_capped_reader);
    let err_rx = child.stderr.take().map(spawn_capped_reader);

    let deadline = Instant::now() + budget;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    // **まだ生きている**ことを try_wait で確かめた上で撃つ。
                    // wait 済みの PID へ撃つと無関係なプロセスを巻き添えにする。
                    crate::procx::kill_tree(child.id());
                    let _ = child.wait();
                    return Err(format!(
                        "{} {} が {} 秒で終わりませんでした",
                        exe.display(),
                        args.join(" "),
                        budget.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(30));
            }
            Err(e) => return Err(format!("{}: {e}", exe.display())),
        }
    };
    // kill 済みでもパイプが閉じるので join は必ず戻る。
    let stdout = out_rx.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_rx.and_then(|h| h.join().ok()).unwrap_or_default();
    Ok(CliOut {
        ok: status.success(),
        stdout,
        stderr,
    })
}

/// 撃つ前の門番。**tailnet が上がっていない状態で CLI を叩かない**
/// (冒頭の実測どおり、そこがいちばん固まる局面である)。
fn cli_gate() -> Result<PathBuf, HttpsBlock> {
    let st = probe();
    if st.stage != Stage::Up {
        return Err(HttpsBlock::NotUp(st.stage));
    }
    cli_path().ok_or(HttpsBlock::NoCli)
}

/// HTTPS で名乗れるドメイン。**裏のスレッドから呼ぶこと** (CLI を起こす)。
pub fn https_domain() -> Result<String, HttpsBlock> {
    let exe = cli_gate()?;
    let out = run_capped(&exe, &["status", "--json"], CLI_BUDGET).map_err(HttpsBlock::Failed)?;
    if !out.ok {
        return Err(HttpsBlock::Failed(out.message()));
    }
    parse_cert_domain(&out.stdout)
}

/// `serve` に渡す引数を組み立てる**純関数** (組み立てを表で固定するため)。
pub fn serve_on_args(port: u16) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--bg".to_string(),
        format!("--https={HTTPS_PORT}"),
        // TLS を終端した先は **loopback だけ**。tailnet の IP へ流すと
        // 平文の口が開いたままになる (= 音声が使えない経路が残る)。
        format!("http://127.0.0.1:{port}"),
    ]
}

/// `serve` を解除する引数を組み立てる**純関数**。
pub fn serve_off_args() -> Vec<String> {
    vec![
        "serve".to_string(),
        format!("--https={HTTPS_PORT}"),
        "off".to_string(),
    ]
}

/// `http://127.0.0.1:<port>` を tailnet の HTTPS 前面に載せる。
pub fn serve_https_on(port: u16) -> Result<(), HttpsBlock> {
    let exe = cli_gate()?;
    let args = serve_on_args(port);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_capped(&exe, &argv, CLI_BUDGET).map_err(HttpsBlock::Failed)?;
    if !out.ok {
        return Err(HttpsBlock::Failed(out.message()));
    }
    Ok(())
}

/// 公開をやめる。**やめ忘れると利用者の tailnet に proxy 設定が残り続ける**
/// ので、モードを変えるとき・終了するときに必ず撃つこと。
///
/// tailnet が落ちていて撃てなかったときも、直し方 (手で叩くコマンド) が
/// 分かるように理由を返す。
pub fn serve_https_off() -> Result<(), HttpsBlock> {
    let exe = cli_gate()?;
    let args = serve_off_args();
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let out = run_capped(&exe, &argv, CLI_BUDGET).map_err(HttpsBlock::Failed)?;
    if !out.ok {
        return Err(HttpsBlock::Failed(out.message()));
    }
    Ok(())
}

/// 証明書を先に取りに行く (**1 回目だけ Let's Encrypt との往復がある**)。
///
/// 温めずに QR を出すと、**利用者の最初の 1 接続がエラーになる** (実測:
/// 1 回目の TLS ハンドシェイクが失敗し、2 回目から 34ms)。
/// ここは失敗しても致命ではない — serve は立っているので、ブラウザ側の
/// ハンドシェイクが同じことをやり直す。だから戻り値は警告として扱う。
///
/// `--cert-file -` / `--key-file -` は**標準出力へ出すだけでディスクに書かない**
/// (実物の `tailscale cert --help` で確認済み。既定は `DOMAIN.crt` を
///  カレントディレクトリへ書いてしまうので、この 2 つを外してはいけない)。
pub fn warm_cert(domain: &str) -> Result<(), String> {
    let exe = cli_path().ok_or_else(|| "tailscale が見つかりません".to_string())?;
    let out = run_capped(
        &exe,
        &["cert", "--cert-file", "-", "--key-file", "-", domain],
        CERT_BUDGET,
    )?;
    if out.ok {
        Ok(())
    } else {
        Err(out.message())
    }
}

// ─── 裏のスレッドで回す係 (UI は 1 度も待たない) ─────────────────────

/// いま何をしている最中か。**UI に必ず出す** — HTTPS を立てるには
/// 証明書の取得が要り、1 回目は秒では終わらない。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HttpsBusy {
    /// `status --json` でドメインを確かめて `serve` を立てている
    Starting,
    /// 証明書を取りに行っている (1 回目だけ長い)
    Warming,
    /// `serve --https=443 off` を撃っている
    Stopping,
}

impl HttpsBusy {
    /// 進行中の 1 行 (呼び出し側で `tr()` を通すこと)。
    pub fn label(&self) -> &'static str {
        match self {
            HttpsBusy::Starting => "🔒 tailnet の HTTPS 公開を準備しています…",
            HttpsBusy::Warming => {
                "🔒 証明書を取得しています… (初回だけ 1 分ほどかかることがあります)"
            }
            HttpsBusy::Stopping => "🔓 tailnet の HTTPS 公開を解除しています…",
        }
    }
}

/// 裏のスレッドが返す結果。
#[derive(Clone, Debug)]
pub enum HttpsDone {
    /// 使えるようになった。`warn` は証明書の先取りに失敗したときだけ付く
    /// (serve は立っているので、最初の 1 接続が遅くなるだけ)。
    On {
        domain: String,
        warn: Option<String>,
    },
    /// 公開をやめた
    Off,
    /// できなかった。理由は 4 通りに分かれている
    Blocked(HttpsBlock),
}

/// HTTPS の入り切りを**裏のスレッド**で回す係。
///
/// UI は [`Https::start`] / [`Https::stop`] を呼んで、毎フレーム
/// [`Https::poll`] を読むだけ。**アイドル時のコストはゼロ** —
/// スレッドは押されたときにだけ 1 本立ち、終われば消える (設計原則 3)。
#[derive(Default)]
pub struct Https {
    rx: Option<std::sync::mpsc::Receiver<HttpsDone>>,
    busy: Option<HttpsBusy>,
    /// 進行中の段を裏のスレッドから知らせる口
    stage_rx: Option<std::sync::mpsc::Receiver<HttpsBusy>>,
    /// いま serve が向いているドメイン (立っている間だけ `Some`)
    domain: Option<String>,
    /// この起動で 1 度でも serve を立てたか。
    /// **終了時に撃つかどうかの判断がこれ 1 つで決まる** (立てていなければ
    /// 何もしない = 利用者の tailnet を勝手に触らない)。
    touched: bool,
}

impl Https {
    /// いま何かしている最中か。
    pub fn busy(&self) -> Option<HttpsBusy> {
        self.busy
    }

    /// serve が向いているドメイン (立っている間だけ)。
    pub fn domain(&self) -> Option<&str> {
        self.domain.as_deref()
    }

    /// `port` を tailnet の HTTPS 前面に載せる。**すぐ戻る**。
    pub fn start(&mut self, port: u16) {
        if self.busy.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        let (stx, srx) = std::sync::mpsc::channel();
        self.busy = Some(HttpsBusy::Starting);
        self.rx = Some(rx);
        self.stage_rx = Some(srx);
        let _ = std::thread::Builder::new()
            .name("zv-ts-https".into())
            .spawn(move || {
                let msg = match https_domain() {
                    Err(e) => HttpsDone::Blocked(e),
                    Ok(domain) => match serve_https_on(port) {
                        Err(e) => HttpsDone::Blocked(e),
                        Ok(()) => {
                            // ここから先は「立っている」。温めは失敗しても続ける
                            let _ = stx.send(HttpsBusy::Warming);
                            let warn = warm_cert(&domain).err();
                            HttpsDone::On { domain, warn }
                        }
                    },
                };
                let _ = tx.send(msg);
            });
    }

    /// 公開をやめる。**すぐ戻る**。
    pub fn stop(&mut self) {
        if self.busy.is_some() {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel();
        self.busy = Some(HttpsBusy::Stopping);
        self.rx = Some(rx);
        self.stage_rx = None;
        let _ = std::thread::Builder::new()
            .name("zv-ts-https-off".into())
            .spawn(move || {
                let msg = match serve_https_off() {
                    Ok(()) => HttpsDone::Off,
                    Err(e) => HttpsDone::Blocked(e),
                };
                let _ = tx.send(msg);
            });
    }

    /// 毎フレーム呼ぶ。終わっていれば 1 度だけ結果を返す。
    pub fn poll(&mut self) -> Option<HttpsDone> {
        if let Some(rx) = self.stage_rx.as_ref() {
            if let Ok(s) = rx.try_recv() {
                self.busy = Some(s);
            }
        }
        let rx = self.rx.as_ref()?;
        let msg = rx.try_recv().ok()?;
        self.rx = None;
        self.stage_rx = None;
        let was = self.busy.take();
        match &msg {
            HttpsDone::On { domain, .. } => {
                self.domain = Some(domain.clone());
                self.touched = true;
            }
            HttpsDone::Off => self.domain = None,
            // 立てようとして断られたなら、立っていない。
            // やめようとして断られたなら、**立ったままかもしれない** ので
            // ドメインは消さない (画面に出し続けて、やり直せるようにする)。
            HttpsDone::Blocked(_) => {
                if was != Some(HttpsBusy::Stopping) {
                    self.domain = None;
                }
            }
        }
        Some(msg)
    }

    /// 終了時の後片付け。**この起動で立てたときだけ**撃つ。
    ///
    /// 終了処理なので同期で待つが、時限は [`CLI_BUDGET`] で頭打ちになる
    /// (立てていなければシステムコール 1 つも撃たない)。
    pub fn cleanup_on_exit(&mut self) {
        if !self.touched {
            return;
        }
        self.touched = false;
        self.domain = None;
        if let Err(e) = serve_https_off() {
            eprintln!(
                "tailnet の HTTPS 公開を解除できませんでした: {:?}\n手で解除するには: tailscale {}",
                e,
                serve_off_args().join(" ")
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tailnet_v4は100_64から100_127まで() {
        // 端の 1 つ外側まで見る (範囲を >= / <= で書き間違えると必ずここが落ちる)
        for (ip, want) in [
            ("100.63.255.255", false),
            ("100.64.0.0", true),
            ("100.100.100.100", true),
            ("100.127.255.255", true),
            ("100.128.0.0", false),
            ("192.168.1.3", false),
            ("127.0.0.1", false),
            ("10.0.0.1", false),
            ("99.64.0.1", false),
            ("101.64.0.1", false),
        ] {
            assert_eq!(
                is_tailnet_v4(ip.parse().unwrap()),
                want,
                "{ip} の判定が違う"
            );
        }
    }

    #[test]
    fn tailnet_v6はfd7a_115c_a1e0の48だけ() {
        for (ip, want) in [
            ("fd7a:115c:a1e0::1", true),
            ("fd7a:115c:a1e0:ab12:3456:7890:abcd:ef01", true),
            // 上位 48bit が 1 つでも違えば別物
            ("fd7a:115c:a1e1::1", false),
            ("fd7a:115d:a1e0::1", false),
            ("fd7b:115c:a1e0::1", false),
            // ふつうの ULA / グローバル / ループバック
            ("fd00::1", false),
            ("2001:db8::1", false),
            ("::1", false),
        ] {
            assert_eq!(
                is_tailnet_v6(ip.parse().unwrap()),
                want,
                "{ip} の判定が違う"
            );
        }
    }

    /// **CGNAT の LAN を Tailscale と取り違えない**ことが、この表の主眼。
    /// キャリアのテザリングは `100.64.0.0/10` からアドレスを配ることがあり、
    /// IPv4 の経路だけを見ていると「Tailscale が上がっている」と嘘をつく。
    #[test]
    fn 判定はcgnatのlanとtailnetを取り違えない() {
        let v4: Ipv4Addr = "100.101.102.103".parse().unwrap();
        let v6: Ipv6Addr = "fd7a:115c:a1e0::1234".parse().unwrap();
        let lan: Ipv4Addr = "192.168.1.3".parse().unwrap();
        let glob: Ipv6Addr = "2001:db8::5".parse().unwrap();

        // 1) IPv6 が引けたら、入っている判定が付かなくても Tailscale で確定
        let s = decide(Some(v4), Some(v6), false);
        assert_eq!(s.stage, Stage::Up);
        assert_eq!(s.ip, Some(IpAddr::V4(v4)), "URL には v4 を使う");

        // 2) IPv6 が引けて v4 が無ければ v6 で待ち受ける
        let s = decide(None, Some(v6), true);
        assert_eq!(s.ip, Some(IpAddr::V6(v6)));

        // 3) v4 だけ + 入っている → 繋がっているとみなす
        assert_eq!(decide(Some(v4), None, true).stage, Stage::Up);

        // 4) v4 だけ + 入っていない → CGNAT の LAN。乗らない
        let s = decide(Some(v4), None, false);
        assert_eq!(s.stage, Stage::Missing);
        assert_eq!(s.ip, None, "繋がらない IP を待ち受け先にしない");

        // 5) 既定経路へ落ちた (LAN IP / グローバル IPv6) → 範囲外なので無視
        assert_eq!(decide(Some(lan), Some(glob), true).stage, Stage::Down);
        assert_eq!(decide(Some(lan), Some(glob), false).stage, Stage::Missing);

        // 6) 何も引けない
        assert_eq!(decide(None, None, true).stage, Stage::Down);
        assert_eq!(decide(None, None, false).stage, Stage::Missing);
    }

    #[test]
    fn readyはupかつipがあるときだけ() {
        assert!(!Status::default().ready());
        assert!(!Status {
            stage: Stage::Up,
            ip: None
        }
        .ready());
        assert!(Status {
            stage: Stage::Up,
            ip: Some(IpAddr::V4("100.64.0.1".parse().unwrap()))
        }
        .ready());
    }

    /// 経路が無い環境でも「速く」「静かに」失敗すること。
    /// (ここが待つ実装だと UI スレッドから引けなくなる)
    #[test]
    fn 検出はブロックしない() {
        let t = Instant::now();
        let s = probe();
        let dt = t.elapsed();
        // 絶対時間で線を引かない — が、「秒のオーダーで待つ実装になっていない」
        // ことだけは見たい。CI の遅いマシンでも 3 秒は掛かりようがない
        // (システムコール数回 + PATH 走査)。
        assert!(dt < Duration::from_secs(3), "検出に {dt:?} 掛かった");
        // 上がっているなら IP が付いていること (段と値が食い違わない)
        assert_eq!(s.ready(), s.stage == Stage::Up);
    }

    #[test]
    fn キャッシュはttlの間は測り直さない() {
        let mut p = Probe::default();
        let a = p.get();
        let b = p.get();
        assert_eq!(a, b);
        assert!(p.last.is_some());
        p.invalidate();
        assert!(p.last.is_none(), "invalidate したら次は必ず測り直す");
    }

    #[test]
    fn install_hintはosごとに1行返す() {
        assert!(!install_hint().is_empty());
        assert!(!install_hint().contains('\n'), "1 行であること");
    }

    /// **`CertDomains` が空 = tailnet で HTTPS が有効になっていない。**
    /// ここを「失敗」で一括りにすると、管理コンソールで 1 度チェックを
    /// 入れれば済む人が、永久に直し方の分からないエラーを見続けることになる。
    #[test]
    fn statusのjsonから4通りの理由を見分ける() {
        let up = r#"{"BackendState":"Running","CertDomains":["macbook-pro.tail4900de.ts.net"]}"#;
        assert_eq!(
            parse_cert_domain(up).unwrap(),
            "macbook-pro.tail4900de.ts.net"
        );
        // 末尾のドット付き (Self.DNSName はこの形で来る) は落として使う
        assert_eq!(
            parse_cert_domain(r#"{"CertDomains":["h.example.ts.net."]}"#).unwrap(),
            "h.example.ts.net"
        );
        // 空 / 無い → 管理コンソールで有効にする以外に直しようが無い
        for json in [
            r#"{"BackendState":"Running","CertDomains":[]}"#,
            r#"{"BackendState":"Running","CertDomains":null}"#,
            r#"{"BackendState":"Running"}"#,
        ] {
            assert_eq!(
                parse_cert_domain(json),
                Err(HttpsBlock::CertsOff),
                "{json} を CertsOff と読めていない"
            );
        }
        // デーモンが動いていない — 経路表と食い違うことがある (ログアウト直後)
        for st in ["NeedsLogin", "Stopped", "NoState", "Starting"] {
            let json = format!(r#"{{"BackendState":"{st}","CertDomains":["h.example.ts.net"]}}"#);
            assert_eq!(
                parse_cert_domain(&json),
                Err(HttpsBlock::NotUp(Stage::Down))
            );
        }
        // 読めない / host にできない値 → 出力をそのまま持つ
        assert!(matches!(
            parse_cert_domain("これは JSON ではない"),
            Err(HttpsBlock::Failed(_))
        ));
        assert!(
            matches!(
                parse_cert_domain(r#"{"CertDomains":["evil.example/path"]}"#),
                Err(HttpsBlock::Failed(_))
            ),
            "URL を差し替えられる値を通してはいけない"
        );
    }

    /// URL の host 部にそのまま置ける形だけを通す。**ここを緩めると
    /// QR の宛先ごと差し替えられる。**
    #[test]
    fn httpsのホスト名はurlに置ける形だけ通す() {
        for (h, want) in [
            ("macbook-pro.tail4900de.ts.net", true),
            ("macbook-pro.tail4900de.ts.net.", true),
            ("a.b", true),
            // ホスト部を抜け出せる形はすべて拒む
            ("", false),
            ("nodots", false),
            ("host:1234", false),
            ("evil.example/path", false),
            ("evil.example?x=1", false),
            ("evil.example#f", false),
            ("a..b.example", false),
            (".example.com", false),
            ("ho st.example", false),
            ("例え.jp", false),
            ("h.example\nX", false),
        ] {
            assert_eq!(is_plausible_host(h), want, "{h:?} の判定が違う");
        }
        // 253 文字を超える名前は DNS で名乗れない
        let long = format!("{}.example", "a".repeat(250));
        assert!(!is_plausible_host(&long));
    }

    /// **TLS を終端した先は loopback だけ。** ここに `0.0.0.0` や tailnet の IP を
    /// 書くと、平文の口が tailnet 側に開いたままになる (= 音声が使えない経路が
    /// 残り、そちらを開いた人には理由が分からない)。
    #[test]
    fn serveの引数はloopbackだけを指す() {
        let on = serve_on_args(8899);
        assert_eq!(
            on,
            vec![
                "serve".to_string(),
                "--bg".to_string(),
                "--https=443".to_string(),
                "http://127.0.0.1:8899".to_string(),
            ]
        );
        // ポートは実行時の値を使う (どこにも 8899 を焼き付けない)
        assert!(serve_on_args(8907).last().unwrap().ends_with(":8907"));
        for a in &on {
            for bad in ["0.0.0.0", "100.", "fd7a:", "localhost"] {
                assert!(!a.contains(bad), "{a} に {bad} が混ざっている");
            }
        }
        // 解除は同じポート指定で off。片方だけ変えると解除できなくなる
        let off = serve_off_args();
        assert_eq!(
            off,
            vec![
                "serve".to_string(),
                "--https=443".to_string(),
                "off".to_string()
            ]
        );
        assert_eq!(
            on.iter().find(|a| a.starts_with("--https=")),
            off.iter().find(|a| a.starts_with("--https=")),
            "立てるときと解除するときでポートが違うと、解除できない設定が残る"
        );
        assert!(on[2].ends_with(&HTTPS_PORT.to_string()));
    }

    /// **4 通りは直し方がそれぞれ違う。** 見出しも次の一手も全部違うこと
    /// (同じ文言に畳むと「できませんでした」と同じ価値しか無くなる)。
    #[test]
    fn httpsにできない理由は4通りに分かれている() {
        let all = [
            HttpsBlock::NotUp(Stage::Missing),
            HttpsBlock::NotUp(Stage::Down),
            HttpsBlock::NoCli,
            HttpsBlock::CertsOff,
            HttpsBlock::Failed("serve: something went wrong".into()),
        ];
        // **見出しと次の一手は「別々に」全部違うこと。**
        // 組で見ると、見出しだけ同じにしても素通しする — 利用者が最初に
        // 読むのは見出し 1 行なので、そこが同じなら区別できていない
        // (実際にこの緩い版が変異試験を通してしまった)。
        let mut heads: Vec<&str> = Vec::new();
        let mut hints: Vec<&str> = Vec::new();
        for b in &all {
            assert!(!b.headline().is_empty(), "{b:?} に見出しが無い");
            assert!(!b.hint().is_empty(), "{b:?} に次の一手が無い");
            assert!(
                !heads.contains(&b.headline()),
                "{b:?} の見出しが別の理由と同じ: {:?}",
                b.headline()
            );
            assert!(
                !hints.contains(&b.hint()),
                "{b:?} の次の一手が別の理由と同じ: {:?}",
                b.hint()
            );
            heads.push(b.headline());
            hints.push(b.hint());
        }
        // 出力をそのまま見せるのは Failed だけ (要約すると理由が消える)
        assert_eq!(
            HttpsBlock::Failed("boom".into()).detail(),
            Some("boom"),
            "生の出力を落としてはいけない"
        );
        assert_eq!(HttpsBlock::CertsOff.detail(), None);
        // 直し方を出せる場所を必ず持つ
        assert!(ADMIN_DNS_URL.starts_with("https://"));
    }

    /// このモジュールが画面へ出す文字列を全部集める。
    ///
    /// **`tr("…")` のリテラルではないので `locale::scan_source_literals` からは
    /// 見えない。** つまり `zai i18n missing` も番人テストも何も言わないまま、
    /// どの言語でも永久に日本語のまま残りうる。ここが唯一の見張りである。
    fn 画面文字列一覧() -> Vec<&'static str> {
        let mut want: Vec<&'static str> = vec![
            crate::remote::Bind::Tailscale.label(),
            crate::remote::Bind::TailscaleHttps.label(),
            // OS 別の案内は、いま走っている OS の 1 本だけ当たる
            install_hint(),
            SWITCH_HINT,
            BACK_HINT,
            ONLY_TAILNET_NOTE,
            NO_REACH,
            HEADLINE,
            HTTPS_HEADLINE,
            HTTPS_ON_HINT,
            HTTPS_OFF_HINT,
            FIRST_CONNECT_NOTE,
        ];
        for s in [Stage::Missing, Stage::Down, Stage::Up] {
            want.push(s.label());
            want.push(s.hint());
        }
        for b in [HttpsBusy::Starting, HttpsBusy::Warming, HttpsBusy::Stopping] {
            want.push(b.label());
        }
        for b in [
            HttpsBlock::NoCli,
            HttpsBlock::CertsOff,
            HttpsBlock::Failed(String::new()),
        ] {
            want.push(b.headline());
            want.push(b.hint());
        }
        want
    }

    /// **同梱の 6 言語パックから引けること。**
    ///
    /// 実行時は `i18n::tr` が「日本語原文 → 訳文」の逆引き (`by_source`) を
    /// 通る。その地図は `locales/ja.json` の**値**から作られるので、
    /// ここに載っていない文字列は英語モードでも中国語モードでも
    /// **日本語のまま出る** (画面を見ても気付けない壊れ方をする)。
    #[test]
    fn 画面に出す文字列は同梱の6言語辞書から引ける() {
        let mut errs = Vec::new();
        let ja = crate::locale::load_one(crate::locale::SOURCE_LANG, &[], &mut errs);
        assert!(errs.is_empty(), "同梱辞書が読めない: {errs:?}");
        let values: std::collections::HashSet<&str> = ja.values().map(|s| s.as_str()).collect();
        let missing: Vec<&str> = 画面文字列一覧()
            .into_iter()
            .filter(|s| !values.contains(*s))
            .collect();
        assert!(
            missing.is_empty(),
            "locales/ja.json に原文が無い (どの言語でも日本語のまま出る): {missing:#?}"
        );
    }

    /// 列挙が返す画面文字列は、同梱の英語辞書 (english-mode) にも載っていること。
    #[test]
    fn 画面に出す文字列は英語辞書にも載っている() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/plugins/english-mode/lang");
        let dict = crate::i18n::load_dict(&dir).expect("同梱辞書が読める");
        let missing: Vec<&str> = 画面文字列一覧()
            .into_iter()
            .filter(|s| !dict.contains_key(*s))
            .collect();
        assert!(missing.is_empty(), "英語辞書に無い文字列: {missing:#?}");
    }
}
