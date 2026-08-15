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
use std::path::PathBuf;
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

    /// 列挙が返す画面文字列は、同梱の英語辞書 (english-mode) にも載っていること。
    #[test]
    fn 画面に出す文字列は英語辞書にも載っている() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("assets/plugins/english-mode/lang");
        let dict = crate::i18n::load_dict(&dir).expect("同梱辞書が読める");
        let mut want: Vec<&'static str> = vec![
            crate::remote::Bind::Tailscale.label(),
            // OS 別の案内は、いま走っている OS の 1 本だけ当たる
            install_hint(),
            SWITCH_HINT,
            BACK_HINT,
            ONLY_TAILNET_NOTE,
            NO_REACH,
            HEADLINE,
        ];
        for s in [Stage::Missing, Stage::Down, Stage::Up] {
            want.push(s.label());
            want.push(s.hint());
        }
        let missing: Vec<&str> = want
            .into_iter()
            .filter(|s| !dict.contains_key(*s))
            .collect();
        assert!(missing.is_empty(), "英語辞書に無い文字列: {missing:#?}");
    }
}
