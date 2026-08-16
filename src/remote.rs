//! スマホリモート操作 — 内蔵HTTPサーバ。
//!
//! PC で Zaivern Code を起動している間、同じ Wi-Fi (LAN) 上のスマホから
//! ブラウザでエディタを操作できる。QR コードを読み取るだけで接続完了。
//!
//! - サーバは std::net だけで実装した極小 HTTP/1.1 (Connection: close)。
//! - UI スレッドとは mpsc チャネルで通信する。サーバスレッドはリクエストを
//!   [`Request`] として送り、`egui::Context::request_repaint()` で UI を起こし、
//!   UI スレッドが次フレームで応答 JSON を返すのを待つ (最大3秒)。
//! - 認証: 起動ごとにランダム生成されるトークン。QR の URL に埋め込まれ、
//!   トークンなしの API アクセスは 401 で拒否する。

use std::hash::Hasher;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

/// 待ち受け先。**LAN / Tailscale / SSH トンネルの違いはここだけ**
/// (設計原則 5: ハンドラは 1 面、トランスポートは多数)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bind {
    /// 同じ Wi-Fi のスマホから直接繋ぐ (`0.0.0.0`)。
    Lan,
    /// SSH トンネル経由だけを許す (`127.0.0.1`)。
    ///
    /// トンネルを張ったまま `0.0.0.0` で待ち受けていると、**SSH を迂回して
    /// 平文で直接叩けてしまう** — トンネルを張る意味が消えるので必ず絞る。
    Loopback,
    /// Tailscale の tailnet からだけ繋ぐ (`100.64.0.0/10` の自分の IP)。
    ///
    /// `0.0.0.0` にしない理由は [`Bind::Loopback`] と同じで、**繋ぐ相手を
    /// tailnet に限る**ため。喫茶店や空港の Wi-Fi に居ても、その LAN からは
    /// ポートが見えない (経路も鍵も Tailscale が持つ)。
    Tailscale,
}

impl Bind {
    /// 実際に bind する IP の**集合**。
    ///
    /// **どのモードでも `127.0.0.1` に届くこと**が不変条件である。
    /// `zai` CLI は `127.0.0.1:<port>` へ繋ぎ ([`crate::cli`])、PC 側の
    /// ブラウザ音声ページも `http://127.0.0.1:<port>/voice` を開く。
    /// ここから loopback を落とすと「スマホからは繋がるのに PC の CLI と
    /// 🎤 だけが死ぬ」という、気付くまでに時間の掛かる壊れ方をする
    /// (`0.0.0.0` は loopback を含むので Lan は 1 本で足りる)。
    ///
    /// Tailscale だけは検出結果 (`ts`) が要る。純関数にしてあるのは、
    /// この不変条件を表で固定するため。
    pub fn listen_ips(&self, ts: Option<IpAddr>) -> Result<Vec<IpAddr>, String> {
        match self {
            Bind::Lan => Ok(vec![IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED)]),
            Bind::Loopback => Ok(vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)]),
            Bind::Tailscale => {
                let ip = ts.ok_or_else(|| {
                    "Tailscale の IP が見つかりません (tailnet に繋がっていません)".to_string()
                })?;
                // tailnet 以外を渡されたら断る。ここを通すと「Tailscale モードの
                // つもりで LAN の IP に晒す」という、いちばん危ない取り違えが
                // 黙って成立してしまう (検出側が CGNAT の LAN を掴んだ場合など)。
                if !crate::tailscale::is_tailnet(ip) {
                    return Err(format!(
                        "{ip} は tailnet (100.64.0.0/10 / fd7a:115c:a1e0::/48) のアドレスではありません"
                    ));
                }
                Ok(vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), ip])
            }
        }
    }

    /// UI に出す 1 行 (呼び出し側で `tr()` を通すこと)。
    ///
    /// Tailscale の行に IP を書かないのは、`&'static str` = 辞書キーだから。
    /// 実際の IP は URL 欄と QR に出る。
    pub fn label(&self) -> &'static str {
        match self {
            Bind::Lan => "0.0.0.0 (同じ Wi-Fi から直接)",
            Bind::Loopback => "127.0.0.1 (SSH トンネル経由のみ)",
            Bind::Tailscale => "Tailscale の IP のみ (同じ tailnet から)",
        }
    }
}

/// URL の host 部に置ける形にする (IPv6 リテラルは RFC 3986 の角括弧で包む)。
fn url_host(ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    }
}

/// 待ち受けている accept ループを**外から起こす**ための宛先。
///
/// `0.0.0.0` / `::` へは繋げない (Windows では明確なエラー、unix でも
/// 実装依存) ので、ワイルドカードのときは同じポートの loopback へ読み替える。
fn wake_addr(a: SocketAddr) -> SocketAddr {
    if !a.ip().is_unspecified() {
        return a;
    }
    match a.ip() {
        IpAddr::V4(_) => SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, a.port())),
        IpAddr::V6(_) => SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, a.port())),
    }
}

// ══════════════════════════════════════════════════════════════════════
//  一括操作の宛先 (スマホの「全員 / 待機 / 1 体」)
//
//  **誰に届くのか**を決めるのはここ 1 か所だけにする。送信・停止・件数表示の
//  3 つが別々に数え方を持つと、「3 体と出ているのに 5 体へ飛んだ」が起きる。
//  純関数なので表で固定でき、UI (件数表示) と実処理 (配達) が必ず一致する。
// ══════════════════════════════════════════════════════════════════════

/// 一括操作の宛先モード。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BulkMode {
    /// 起動中の全エージェント (PC 側の「📣 全エージェントへブロードキャスト」相当)
    All,
    /// 止まっている (待機中の) エージェントだけ (PC 側の「止まっているものへまとめて送る」相当)
    Stalled,
    /// いま選んでいる 1 体
    One,
}

impl BulkMode {
    /// 文字列から起こす。**知らない語は `None`**。
    ///
    /// 既定で「全員」へ落とすと、綴りを 1 文字間違えただけで全エージェントへ
    /// 誤爆する。宛先の解釈は fail-closed にして、呼び出し側が 400 で断る。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "all" => Some(Self::All),
            "stalled" => Some(Self::Stalled),
            "one" => Some(Self::One),
            _ => None,
        }
    }

    /// API とページで使う語 (ログ・エラー文言用)。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Stalled => "stalled",
            Self::One => "one",
        }
    }
}

/// 宛先を選ぶために必要な、エージェント 1 体ぶんの最小の姿。
///
/// `stalled` は **supervisor の状態判定から取った値**を入れること。
/// 画面のピクセルから推測してはいけない (設計原則 4)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentPick {
    pub id: u64,
    /// PTY が生きているか
    pub running: bool,
    /// 止まっている (待機中) と判定されているか
    pub stalled: bool,
    /// いま選ばれている 1 体か
    pub active: bool,
}

/// 一覧の行から撃てる 1 体宛ての操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentAct {
    /// 承認する (PC 側の「✅ 承認」= `press_pet_approve_button` と同じ入口)
    Approve,
    /// いまの作業を止める (Esc)
    Stop,
}

impl AgentAct {
    /// 文字列から起こす。**知らない語は `None`** — 綴り違いを黙って
    /// 「承認」に落とすと、押していない承認が飛ぶ。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(Self::Approve),
            "stop" => Some(Self::Stop),
            _ => None,
        }
    }
}

/// スマホの「待ち」一覧に載せるレーンか。
///
/// 人の手が要る 2 本 ([`kanban::Column::loud`] = 承認待ち / 停滞・異常) に、
/// 手が空いて指示を待っている 1 本 (`Ready`) を足したもの。
/// **レーンの定義そのものは `kanban.rs` が持つ** — しきい値も名前もここで
/// 作り直さない (真実の在り処を 1 つに保つ)。
pub fn is_waiting_lane(col: crate::kanban::Column) -> bool {
    col.loud() || col == crate::kanban::Column::Ready
}

/// 宛先の ID を選ぶ純関数。並びは起動順 (入力の順) のまま。
///
/// どのモードでも**動いていないセッションは必ず除く** — 終了済みへ書いても
/// 届かないのに「N 体へ送りました」と数えてしまうため。
pub fn bulk_targets(mode: BulkMode, agents: &[AgentPick]) -> Vec<u64> {
    agents
        .iter()
        .filter(|a| a.running)
        .filter(|a| match mode {
            BulkMode::All => true,
            BulkMode::Stalled => a.stalled,
            BulkMode::One => a.active,
        })
        .map(|a| a.id)
        .collect()
}

/// UI スレッドへ渡す問い合わせの種類。
pub enum Query {
    /// タブ・エージェント・カーソル等の全体状態
    State,
    /// アクティブバッファの本文
    File,
    /// ワークスペースのファイル一覧
    Files,
    /// バッファ本文を丸ごと置き換える。index はスマホ側が編集していたタブ。
    /// PC 側のアクティブタブと不一致なら拒否する (誤上書き防止)。
    /// save=true なら適用後にそのままディスクへ保存する (rfd ダイアログは開かない)。
    SetText {
        text: String,
        index: i64,
        save: bool,
    },
    /// コマンド実行 (name, 数値引数)
    Cmd(String, i64),
    /// ワークスペース相対パスのファイルを開く。
    /// line が Some なら、その行 (1 始まり) へカーソルを移動する。
    OpenFile(String, Option<usize>),
    /// トースト通知を出す (message, level)。
    /// level は "info" | "warn" | "error"。
    Notify(String, String),
    /// プラグインのパネル内容を書き換える (plugin, panel, text)。
    /// plugin が空文字なら、その panel id を持つ最初のプラグインへ送る。
    SetPanel {
        plugin: String,
        panel: String,
        text: String,
    },
    /// ステータスバーへ任意の文字列を出す (空文字で消す)。
    SetStatus(String),
    /// エージェントの入力欄へ差し込む。
    /// agent が空ならアクティブなエージェント、名前指定ならその名前に一致するもの。
    /// submit=false なら Enter は送らない (送信は人の操作で行う)。
    Prompt {
        text: String,
        agent: String,
        submit: bool,
    },
    /// タブ切替
    Tab(usize),
    /// アクティブなエージェントのターミナル画面テキスト
    Term,
    /// アクティブなエージェントへ入力を送る (payload, raw)。
    /// raw=false はテキスト+Enter、raw=true はバイト列そのまま (制御キー用)。
    TermInput(String, bool),
    /// 音声入力ページからの送信。id はセッション id (インデックスではない)、
    /// 負数なら全エージェントへブロードキャスト。
    /// submit=false ならテキストを入力欄へ挿入するだけで Enter は送らない
    /// (PC 側と同じく、送信は必ず人の操作で行う)。
    VoiceSend { text: String, id: i64, submit: bool },
    /// 一括送信。宛先は [`BulkMode`] で決まる (全員 / 待機だけ / 選んでいる 1 体)。
    /// submit=false なら入力欄へ入れるだけで確定キーは送らない。
    Bulk {
        text: String,
        mode: BulkMode,
        submit: bool,
    },
    /// エージェント一覧 (待ち一覧 / デッキ / 看板が読む 1 本)。
    ///
    /// 状態・レーン・直近出力を**まとめて 1 回**で返す。ビューごとに別の
    /// エンドポイントを叩くとポーリングが 3 倍になるので、形は 1 つにして
    /// 絞り込みと並べ替えはスマホ側で行う。
    Agents,
    /// 一覧の行から撃つ 1 体宛ての操作 (承認 / 停止)。
    /// `id` は**セッション ID** — 一覧が並び替わっても宛先がずれない。
    AgentAct { id: i64, act: AgentAct },
    /// 一括停止。宛先ごとに Esc を送って**いまの作業を中断**させる。
    ///
    /// セッションを殺す `Cmd::StopAllAgents` は PC 側に確認モーダルを開くので、
    /// スマホから押すと**誰も押せないダイアログが PC に出たまま**になる。
    /// リモートから届くのは「Esc で止める」までに留める。
    BulkStop { mode: BulkMode },
}

impl Query {
    /// UI スレッドの応答を待たずに即座に 200 を返してよい要求か。
    ///
    /// macOS はウィンドウが前面に無いとイベントループごと凍結させるため、
    /// 「UI スレッドに投げて応答を待つ」方式だと CLI から叩いたときに
    /// 高確率でタイムアウトする (実測: 10 秒間で CPU 時間 0.01 秒)。
    ///
    /// 状態を返さない一方向の指示は、キューに積んだ時点で成功とみなす。
    /// エディタが次に動いたときに必ず適用されるので取りこぼしは無い。
    /// 逆に現在の状態を読む要求 (State/File/Files/Term) は、
    /// 実際の値が必要なので従来どおり待つ。
    fn is_fire_and_forget(&self) -> bool {
        matches!(
            self,
            Query::Notify(..)
                | Query::SetPanel { .. }
                | Query::SetStatus(..)
                | Query::Prompt { .. }
                | Query::OpenFile(..)
                | Query::Cmd(..)
                | Query::TermInput(..)
        )
    }

    /// 即答するときに返す JSON。
    fn ack(&self) -> &'static str {
        r#"{"ok":true,"queued":true}"#
    }
}

/// サーバスレッド → UI スレッドへのリクエスト。UI 側は必ず respond すること。
pub struct Request {
    pub query: Query,
    reply: mpsc::SyncSender<String>,
}

impl Request {
    pub fn respond(self, json: String) {
        let _ = self.reply.send(json);
    }
}

/// LAN の相手 (= スマホ) から実際に接続が届いたか。
///
/// 「許可したのにスマホは真っ白なまま」で詰まるのは、**届いていないのか
/// 届いた上で失敗しているのかが PC 側から見えない**からである。ファイアウォールの
/// 規則をいくら読んでも「実際に通ったか」は分からない (規則があっても
/// ルータのクライアント分離や別セグメントなら届かない)。
///
/// そこで accept した時点の相手アドレスだけを記録する。ここが 0 件のままなら
/// **パケットが PC まで来ていない** = 規則やネットワーク経路の問題、
/// 1 件でもあれば **届いてはいる** = 以降はアプリ側の問題、と切り分けられる。
#[derive(Clone, Debug, Default)]
pub struct Reach {
    /// LAN 側 (ループバック以外) から accept した接続の数。
    pub hits: u64,
    /// 直近の接続元アドレス (表示用)。
    pub last_ip: Option<String>,
    /// 直近の接続を受けた時刻。
    pub last_at: Option<Instant>,
}

pub struct RemoteServer {
    pub port: u16,
    pub token: String,
    /// トークンなしのベース URL (例: http://192.168.1.10:8899/)
    pub url: String,
    /// いまどこで待ち受けているか (UI に必ず出す)
    pub bind: Bind,
    /// 実際に待ち受けているアドレス (`Drop` が 1 本ずつ起こすので必要)。
    /// Tailscale モードでは loopback と tailnet の 2 本になる。
    pub addrs: Vec<SocketAddr>,
    rx: mpsc::Receiver<Request>,
    reach: Arc<Mutex<Reach>>,
    /// accept ループの停止指示 (`Drop` で立てる)
    stop: Arc<AtomicBool>,
    accept: Vec<std::thread::JoinHandle<()>>,
    /// accept ループが 1 本終わるごとに 1 つ届く合図。
    /// **`Drop` が「本当に起きたか」を確かめるために要る** — 起こし用の接続は
    /// 別プロセスに横取りされることがある ([`RemoteServer::drop`])。
    done: mpsc::Receiver<()>,
}

/// スレッドがどの経路で終わっても 1 度だけ合図を送る見張り。
struct DoneSignal(mpsc::Sender<()>);

impl Drop for DoneSignal {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

/// `Drop` が accept を起こすのに使ってよい時間。
///
/// **超えたら join せずに手放す。** 待ち続けるとアプリの終了が固まるが、
/// 手放して困るのはポートが 1 つ塞がったままになることだけである。
const WAKE_BUDGET: Duration = Duration::from_secs(2);

/// 待ち受けに使うポートの範囲 (両端を含む)。
///
/// Windows の受信許可規則もこの範囲で作るので、
/// [`crate::firewall::PORT_FROM`] / [`crate::firewall::PORT_TO`] と必ず一致させること
/// (広げてもファイアウォール側を直さないと、その分は Windows で繋がらない)。
pub const PORT_FROM: u16 = 8899;
pub const PORT_TO: u16 = 8919;

impl RemoteServer {
    /// サーバを起動する。8899 から順に空きポートを探す。
    pub fn start(ctx: egui::Context, bind: Bind) -> Result<Self, String> {
        Self::start_with(ctx, bind, None, None)
    }

    /// 待ち受け先だけを変えて張り直す (LAN ⇄ SSH トンネル)。
    ///
    /// **トークンとポートは引き継ぐ** — トークンを変えると既に QR を読み込んだ
    /// スマホや CLI の接続情報が一斉に無効になり、ポートが動くと別インスタンスの
    /// 待ち受けを横取りしかねない。呼び出し側は古いサーバを先に drop して
    /// ポートを解放すること (`Drop` が accept スレッドの終了まで待つ)。
    pub fn rebind(
        ctx: egui::Context,
        bind: Bind,
        token: String,
        prefer_port: u16,
    ) -> Result<Self, String> {
        Self::start_with(ctx, bind, Some(token), Some(prefer_port))
    }

    fn start_with(
        ctx: egui::Context,
        bind: Bind,
        keep_token: Option<String>,
        prefer_port: Option<u16>,
    ) -> Result<Self, String> {
        // **待ち受ける直前に**引き直す。UI に出ている検出結果は最大 2 秒前の
        // もので、その間に Tailscale が落ちていれば bind は必ず失敗する。
        let ts = match bind {
            Bind::Tailscale => crate::tailscale::listen_ip(),
            _ => None,
        };
        let ips = bind.listen_ips(ts)?;
        Self::start_on(ctx, bind, &ips, keep_token, prefer_port)
    }

    /// 待ち受けるアドレスを明示して起動する (`Bind` の解決は済んでいる)。
    fn start_on(
        ctx: egui::Context,
        bind: Bind,
        ips: &[IpAddr],
        keep_token: Option<String>,
        prefer_port: Option<u16>,
    ) -> Result<Self, String> {
        let mut listeners: Vec<TcpListener> = Vec::new();
        let mut port = 0u16;
        let mut last_err: Option<String> = None;
        // 張り直しでは元のポートを最優先で試す。unix では SO_REUSEADDR が
        // 効くため `0.0.0.0:P` と `127.0.0.1:P` が同居できてしまい、素直に
        // 先頭から走査すると**別インスタンスのポート**を掴むことがある。
        //
        // 複数アドレスのときは **1 つでも取れなければその番号ごと捨てる** —
        // ポートが 2 つに割れると、URL に書ける番号が 1 つに決まらない。
        for p in prefer_port.into_iter().chain(PORT_FROM..=PORT_TO) {
            let mut got: Vec<TcpListener> = Vec::with_capacity(ips.len());
            let mut ok = true;
            for ip in ips {
                match TcpListener::bind((*ip, p)) {
                    Ok(l) => got.push(l),
                    Err(e) => {
                        last_err = Some(format!("{ip}:{p} — {e}"));
                        ok = false;
                        break;
                    }
                }
            }
            if ok {
                listeners = got;
                port = p;
                break;
            }
            // 取れた分はここで閉じる (次の番号へ持ち越さない)
        }
        if listeners.is_empty() {
            // 「空きポートが無い」で片付けない — Tailscale の IP が消えている
            // ような、ポートとは無関係の理由もここへ来る。最後の理由を必ず出す。
            let why = last_err.unwrap_or_else(|| "理由不明".to_string());
            return Err(format!(
                "待ち受けを開始できません ({PORT_FROM}-{PORT_TO}): {why}"
            ));
        }
        let addrs: Vec<SocketAddr> = listeners
            .iter()
            .filter_map(|l| l.local_addr().ok())
            .collect();

        let token = keep_token.unwrap_or_else(gen_token);
        // 待ち受けが loopback だけのときに LAN の IP を出すと、
        // 「その URL では絶対に繋がらない」嘘の案内になる。
        let host = match bind {
            Bind::Lan => lan_ip(),
            Bind::Loopback => "127.0.0.1".to_string(),
            // loopback 以外 = tailnet 側。スマホが読むのはこちら
            Bind::Tailscale => addrs
                .iter()
                .map(|a| a.ip())
                .find(|ip| !ip.is_loopback())
                .map(url_host)
                .unwrap_or_else(|| "127.0.0.1".to_string()),
        };
        let url = format!("http://{host}:{port}/");
        let (tx, rx) = mpsc::channel::<Request>();
        let reach = Arc::new(Mutex::new(Reach::default()));
        let stop = Arc::new(AtomicBool::new(false));

        // 待ち受けるアドレスごとに 1 本ずつ accept ループを持つ。
        // (1 スレッドで多重化するには非同期 I/O か select が要る。ここは
        //  多くても 2 本なので、素直にスレッドを分けるほうが読める)
        let mut accept: Vec<std::thread::JoinHandle<()>> = Vec::with_capacity(listeners.len());
        let (done_tx, done) = mpsc::channel::<()>();
        for listener in listeners {
            let tx = tx.clone();
            let ctx = ctx.clone();
            let tok = token.clone();
            let reach_srv = Arc::clone(&reach);
            let stop_srv = Arc::clone(&stop);
            let signal = DoneSignal(done_tx.clone());
            let h = std::thread::Builder::new()
                .name("zv-remote-accept".into())
                .spawn(move || {
                    // どの経路で抜けても `Drop` へ「終わった」と伝える
                    let _signal = signal;
                    for stream in listener.incoming() {
                        // 停止指示は accept を抜けた直後に見る。`Drop` が自分自身へ
                        // 1 本繋いで起こすので、ここが最初に踏まれる。
                        if stop_srv.load(Ordering::SeqCst) {
                            return;
                        }
                        let Ok(stream) = stream else {
                            // fd 枯渇などで accept が失敗し続けると待機なしの
                            // ビジーループになるため、少し休んでから再試行する
                            std::thread::sleep(std::time::Duration::from_millis(50));
                            continue;
                        };
                        // 「スマホから届いたか」はここでしか分からない。
                        if let Ok(peer) = stream.peer_addr() {
                            if counts_as_remote(&peer) {
                                if let Ok(mut r) = reach_srv.lock() {
                                    r.hits += 1;
                                    r.last_ip = Some(peer.ip().to_string());
                                    r.last_at = Some(Instant::now());
                                }
                            }
                        }
                        let tx = tx.clone();
                        let ctx = ctx.clone();
                        let tok = tok.clone();
                        let _ = std::thread::Builder::new()
                            .name("zv-remote-conn".into())
                            .spawn(move || handle_conn(stream, tx, ctx, tok));
                    }
                })
                .map_err(|e| format!("サーバスレッド起動失敗: {e}"))?;
            accept.push(h);
        }

        let where_ = addrs
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("📱 スマホリモート起動 ({where_}): {url}?t={token}");
        Ok(Self {
            port,
            token,
            url,
            bind,
            addrs,
            rx,
            reach,
            stop,
            accept,
            done,
        })
    }

    /// UI スレッドから毎フレーム呼ぶ。溜まっているリクエストを取り出す。
    pub fn poll(&self) -> Vec<Request> {
        self.rx.try_iter().collect()
    }

    /// LAN 側から実際に接続が届いたか ([`Reach`])。
    /// 毒された Mutex でも UI を落とさないよう、取れなければ既定値を返す。
    pub fn reach(&self) -> Reach {
        self.reach.lock().map(|r| r.clone()).unwrap_or_default()
    }
}

impl Drop for RemoteServer {
    /// accept ループを確実に終わらせてからポートを手放す。
    ///
    /// 単にフラグを立てるだけでは accept が次の接続まで起きないので、
    /// **自分自身へ 1 本繋いで起こす**。join まで待つのは、直後に同じポートへ
    /// 張り直す ([`RemoteServer::rebind`]) から — 待たないと EADDRINUSE で
    /// ポート番号がずれ、トンネルの転送先と食い違う。
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let n = self.accept.len();
        let deadline = Instant::now() + WAKE_BUDGET;
        let mut woken = 0usize;
        while woken < n && Instant::now() < deadline {
            // **1 本の accept を起こせるのは 1 接続だけ**。待ち受けている
            // アドレスごとに 1 本ずつ繋ぐ (Tailscale モードは loopback と
            // tailnet の 2 本)。
            for a in &self.addrs {
                if let Ok(s) =
                    TcpStream::connect_timeout(&wake_addr(*a), Duration::from_millis(300))
                {
                    let _ = s.shutdown(std::net::Shutdown::Both);
                }
            }
            // **繋いだ = 起きた、ではない。** unix は `SO_REUSEADDR` で
            // `0.0.0.0:P` と `127.0.0.1:P` が同居でき、接続はより具体的な
            // `127.0.0.1` 側へ行く。別インスタンスが同じ番号の loopback を
            // 握っていると、こちらの起こし用接続は**そちらに攫われる**。
            // 実際に CI で踏んだ (テストが 60 秒で打ち切られた)。
            while woken < n {
                match self.done.recv_timeout(Duration::from_millis(100)) {
                    Ok(()) => woken += 1,
                    Err(_) => break,
                }
            }
        }
        if woken == n {
            for h in self.accept.drain(..) {
                let _ = h.join();
            }
            return;
        }
        // 起こせなかった。**join すると終了が固まる**ので待たずに手放す。
        // 手放したスレッドはリスナを握ったままなので、この番号は
        // プロセスが終わるまで空かない (張り直しは次の番号へ落ちる)。
        eprintln!(
            "📱 スマホリモート: 待ち受け {}/{} 本を起こせませんでした \
             (同じポートの 127.0.0.1 を別プロセスが握っている可能性)。\
             待たずに手放します",
            n - woken,
            n
        );
        self.accept.clear();
    }
}

/// 起動ごとのランダムトークン (10桁hex)。
fn gen_token() -> String {
    // RandomState は OS の乱数で鍵付けされた SipHash なので、鍵を知らない
    // 相手には出力を予測できない。2 つ独立に作って 128bit 分を合成する
    // (旧実装は DefaultHasher + 時刻 + PID で、オフライン列挙が可能だった)。
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    let a = RandomState::new().build_hasher().finish();
    let b = RandomState::new().build_hasher().finish();
    format!("{:016x}", a ^ b.rotate_left(17))
}

/// この接続を「LAN の相手 (= スマホ) から届いた」と数えるか。
///
/// ループバックは PC 自身 — 🎤 の音声ページや動作確認のブラウザがそれに当たる。
/// これを数えると **スマホからは 1 度も届いていないのに「届いています」と表示** して
/// しまい、切り分けの役に立たないどころか誤誘導になる。
fn counts_as_remote(peer: &std::net::SocketAddr) -> bool {
    !peer.ip().is_loopback()
}

/// LAN 上での自分の IP アドレスを推定する (UDP connect トリック)。
fn lan_ip() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| {
            s.connect("8.8.8.8:80")?;
            s.local_addr()
        })
        .map(|a| a.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".into())
}

// ─── HTTP 処理 ──────────────────────────────────────────────────────

fn handle_conn(
    mut stream: TcpStream,
    tx: mpsc::Sender<Request>,
    ctx: egui::Context,
    token: String,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));

    // ヘッダ終端 (\r\n\r\n) まで読む
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(p) = find_subslice(&buf, b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 64 * 1024 {
            return respond(&mut stream, 431, "text/plain", b"header too large");
        }
        match stream.read(&mut tmp) {
            Ok(0) => return,
            Ok(n) => buf.extend_from_slice(&tmp[..n]),
            Err(_) => return,
        }
    };

    let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = head.lines();
    let req_line = lines.next().unwrap_or("");
    let mut parts = req_line.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let target = parts.next().unwrap_or("/").to_string();
    let (path, query_str) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target.clone(), String::new()),
    };

    let mut content_len = 0usize;
    let mut hdr_token = String::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            continue;
        };
        let k = k.trim().to_ascii_lowercase();
        let v = v.trim();
        if k == "content-length" {
            content_len = v.parse().unwrap_or(0);
        } else if k == "x-token" {
            hdr_token = v.to_string();
        }
    }
    if content_len > 2 * 1024 * 1024 {
        return respond(&mut stream, 413, "text/plain", b"body too large");
    }

    // ─── ルーティング (静的ページはボディ不要なので先に返す) ───
    if path == "/" || path == "/index.html" {
        return respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            page_for_client(PAGE).as_bytes(),
        );
    }
    if path == "/voice" {
        // PC 用の音声入力ページ (Web Speech API — 127.0.0.1 で開くこと)
        return respond(
            &mut stream,
            200,
            "text/html; charset=utf-8",
            page_for_client(VOICE_PAGE).as_bytes(),
        );
    }
    if !path.starts_with("/api/") {
        return respond(&mut stream, 404, "text/plain", b"not found");
    }

    // 認証: X-Token ヘッダ または ?t= クエリ。
    // トークンはヘッダ解析の時点で分かるので、ボディを読む「前」に検証する
    // (未認証の相手に最大 2MB のバッファリングを強制されないように)。
    let q_token = query_str
        .split('&')
        .find_map(|kv| kv.strip_prefix("t="))
        .unwrap_or("");
    if hdr_token != token && q_token != token {
        // 総当たりを減速させる (接続ごとスレッドなので他リクエストは塞がない)
        std::thread::sleep(std::time::Duration::from_millis(250));
        return respond(
            &mut stream,
            401,
            "application/json",
            br#"{"ok":false,"error":"unauthorized"}"#,
        );
    }

    // ボディを読む (認証済みのリクエストのみ)
    let mut body: Vec<u8> = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        match stream.read(&mut tmp) {
            Ok(0) => break,
            Ok(n) => body.extend_from_slice(&tmp[..n]),
            Err(_) => return,
        }
    }
    // 通信断などでボディが揃わないまま既定値で実行しない (空文字適用の防止)
    if body.len() < content_len {
        return respond(
            &mut stream,
            400,
            "application/json",
            br#"{"ok":false,"error":"incomplete body"}"#,
        );
    }

    // POST のボディが JSON として読めない場合は 400 で弾く。
    // Null に落として続行すると全フィールドが既定値になり、
    // /api/text が text="" でアクティブバッファを空にしてしまう。
    let json: serde_json::Value = if method == "POST" {
        match serde_json::from_slice(&body) {
            Ok(v) => v,
            Err(_) => {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"invalid json body"}"#,
                );
            }
        }
    } else {
        serde_json::Value::Null
    };
    let s = |k: &str| {
        json.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    let n = |k: &str| json.get(k).and_then(|v| v.as_i64()).unwrap_or(0);

    let query = match (method.as_str(), path.as_str()) {
        ("GET", "/api/state") => Query::State,
        ("GET", "/api/file") => Query::File,
        ("GET", "/api/files") => Query::Files,
        ("GET", "/api/term") => Query::Term,
        ("GET", "/api/agents") => Query::Agents,
        // 一覧の行から撃つ 1 体宛ての操作。知らない語は実行せずに断る
        ("POST", "/api/agent_act") => {
            let Some(act) = AgentAct::parse(&s("act")) else {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"unknown act (approve|stop)"}"#,
                );
            };
            Query::AgentAct { id: n("id"), act }
        }
        ("POST", "/api/text") => {
            // text フィールドが無いリクエストで空文字を適用しない (バッファ全消し防止)
            if json.get("text").and_then(|v| v.as_str()).is_none() {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"missing text"}"#,
                );
            }
            Query::SetText {
                text: s("text"),
                index: json.get("index").and_then(|v| v.as_i64()).unwrap_or(-1),
                save: json.get("save").and_then(|v| v.as_bool()).unwrap_or(false),
            }
        }
        ("POST", "/api/cmd") => Query::Cmd(s("name"), n("arg")),
        ("POST", "/api/open") => Query::OpenFile(
            s("path"),
            json.get("line")
                .and_then(|v| v.as_i64())
                .filter(|l| *l > 0)
                .map(|l| l as usize),
        ),
        ("POST", "/api/notify") => {
            let level = s("level");
            let level = if level.is_empty() {
                "info".into()
            } else {
                level
            };
            Query::Notify(s("message"), level)
        }
        ("POST", "/api/panel") => Query::SetPanel {
            plugin: s("plugin"),
            panel: s("panel"),
            text: s("text"),
        },
        ("POST", "/api/status") => Query::SetStatus(s("text")),
        ("POST", "/api/prompt") => Query::Prompt {
            text: s("text"),
            agent: s("agent"),
            // 既定は「挿入のみ」。/api/voice と同じ約束にする
            submit: json
                .get("submit")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
        ("POST", "/api/tab") => Query::Tab(n("index").max(0) as usize),
        ("POST", "/api/term") => Query::TermInput(
            s("text"),
            json.get("raw").and_then(|v| v.as_bool()).unwrap_or(false),
        ),
        // 一括送信 / 一括停止。宛先の語を読めなければ**送らずに断る**
        // (既定で「全員」に落とすと綴り違いで誤爆する)。
        ("POST", "/api/bulk") | ("POST", "/api/bulk_stop") => {
            let Some(mode) = BulkMode::parse(&s("mode")) else {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"unknown mode (all|stalled|one)"}"#,
                );
            };
            if path == "/api/bulk_stop" {
                Query::BulkStop { mode }
            } else {
                Query::Bulk {
                    text: s("text"),
                    mode,
                    // 既定は「挿入のみ」。/api/voice・/api/prompt と同じ約束にする
                    submit: json
                        .get("submit")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false),
                }
            }
        }
        ("POST", "/api/voice") => Query::VoiceSend {
            text: s("text"),
            id: json.get("id").and_then(|v| v.as_i64()).unwrap_or(-1),
            // 既定は「挿入のみ」。送信は明示的に submit=true を渡したときだけ
            submit: json
                .get("submit")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        },
        _ => {
            return respond(
                &mut stream,
                404,
                "application/json",
                br#"{"ok":false,"error":"unknown api"}"#,
            )
        }
    };

    // UI スレッドへ渡す
    let (rtx, rrx) = mpsc::sync_channel::<String>(1);
    let immediate = query.is_fire_and_forget().then(|| query.ack());
    if tx.send(Request { query, reply: rtx }).is_err() {
        return respond(
            &mut stream,
            500,
            "application/json",
            br#"{"ok":false,"error":"app closed"}"#,
        );
    }

    // 一方向の指示は積んだ時点で成功。UI スレッドの復帰を待たない。
    if let Some(js) = immediate {
        crate::perf::repaint(&ctx, "remote");
        return respond(
            &mut stream,
            200,
            "application/json; charset=utf-8",
            js.as_bytes(),
        );
    }
    // UI スレッドは次のフレームでしか応答できない。ウィンドウが背面や
    // 非表示だとフレームが来る間隔が延びるため、1 回だけ起こして待つと
    // 取りこぼす。応答が返るまで一定間隔で起こし続ける。
    let deadline = Instant::now() + REMOTE_TIMEOUT;
    let reply = loop {
        crate::perf::repaint(&ctx, "remote");
        match rrx.recv_timeout(Duration::from_millis(150)) {
            Ok(js) => break Some(js),
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break None;
                }
            }
        }
    };
    match reply {
        Some(js) => respond(
            &mut stream,
            200,
            "application/json; charset=utf-8",
            js.as_bytes(),
        ),
        None => respond(
            &mut stream,
            504,
            "application/json",
            br#"{"ok":false,"error":"timeout"}"#,
        ),
    }
}

/// UI スレッドの応答を待つ上限。背面ウィンドウでもフレームが 1 回は来る余裕を取る。
const REMOTE_TIMEOUT: Duration = Duration::from_secs(15);

fn respond(stream: &mut TcpStream, code: u16, ctype: &str, body: &[u8]) {
    let status = match code {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        413 => "Payload Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        504 => "Gateway Timeout",
        _ => "Error",
    };
    let head = format!(
        "HTTP/1.1 {code} {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

// ─── ページの多言語化 (Language Pack) ────────────────────────────────
//
// スマホ側は別プロセス (ブラウザ) なので `tr()` を直接呼べない。そこで
// **配信の瞬間に 1 回だけ**、`remote.*` の訳を JSON で `<head>` へ流し込み、
// あとは JS が `T(id, 日本語原文)` で引く。辞書が 1 件も無くても
// フォールバック (日本語原文) が出るので、画面は決して壊れない。

/// `<head>` に置いた差し込み口。[`localize_page`] がここを
/// `window.ZVI18N = {…};` へ置き換える。置き換えられなくても
/// ただの空コメントなので、素の `PAGE` も正しい HTML のまま。
const I18N_SLOT: &str = "/*__ZV_I18N__*/";

/// `PAGE` / `VOICE_PAGE` へ現在の言語の文言を差し込む。
///
/// HTTP を 1 バイトも触らない**ただの文字列処理**にしてあるので、サーバを
/// 起こさずにテストできる (引数で受けて `String` を返すだけ)。
/// `dict_json` は `{"remote.save":"Save",…}` 形式の JSON オブジェクト。
fn localize_page(template: &str, dict_json: &str, lang: &str) -> String {
    // `<html lang>` は読み上げ・折り返し・フォント選択に効くので実際の言語にする
    let out = template.replacen(
        "<html lang=\"ja\">",
        &format!("<html lang=\"{}\">", html_lang_attr(lang)),
        1,
    );
    out.replace(
        I18N_SLOT,
        &format!("window.ZVI18N = {};", script_safe_json(dict_json)),
    )
}

/// `<html lang="…">` へ入れてよい形だけを通す。
///
/// 言語 ID は利用者が置いた `locales/*.json` 由来なので、引用符や `>` が
/// 混ざると属性を抜け出して HTML を書き換えられる。**通すものを決める**
/// (弾くものを列挙しない) 方式にして、想定外の文字は落とす。
fn html_lang_attr(lang: &str) -> String {
    let ok: String = lang
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if ok.is_empty() {
        crate::locale::SOURCE_LANG.to_string()
    } else {
        ok
    }
}

/// `<script>` の中へ JSON を置くための無害化。
///
/// ブラウザは **文字列リテラルの途中でも** `</script>` を見つけた時点で
/// スクリプトを終わらせる。訳文に `</script>` が入っていると、そこから先が
/// HTML として解釈され、ページが壊れるだけでなく任意のタグを注入できてしまう。
/// JSON の文字列中では `<` と `<` が同じ値なので、`<` を全部書き換えれば
/// `</script>` は**構造的に作れない**。
/// U+2028 / U+2029 は古い JS で行終端として扱われるので併せて潰す。
/// 壊れた入力 (JSON でない・オブジェクトでない) は空辞書に落として、
/// JS の構文まで道連れにしない。
fn script_safe_json(dict_json: &str) -> String {
    let v: serde_json::Value = match serde_json::from_str(dict_json) {
        Ok(v) => v,
        Err(_) => return "{}".to_string(),
    };
    if !v.is_object() {
        return "{}".to_string();
    }
    let s = match serde_json::to_string(&v) {
        Ok(s) => s,
        Err(_) => return "{}".to_string(),
    };
    s.replace('<', "\\u003c")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029")
}

/// 配信直前に、いまの UI 言語でページを組み立てる。
fn page_for_client(template: &str) -> String {
    let dict = crate::i18n::export_prefix("remote.");
    let json = serde_json::to_string(&dict).unwrap_or_else(|_| "{}".to_string());
    localize_page(template, &json, &crate::i18n::current())
}

// ─── スマホ用ページ (完全内蔵・依存ゼロ) ─────────────────────────────

const PAGE: &str = r##"<!DOCTYPE html>
<html lang="ja">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1, maximum-scale=1, viewport-fit=cover">
<meta name="apple-mobile-web-app-capable" content="yes">
<meta name="theme-color" content="#0d1117">
<title>Zaivern Remote</title>
<script>/*__ZV_I18N__*/</script>
<style>
  * { margin:0; padding:0; box-sizing:border-box; -webkit-tap-highlight-color:transparent; }
  html,body { height:100%; }
  body {
    background:#0d1117; color:#e6edf3;
    font-family:-apple-system,BlinkMacSystemFont,"Hiragino Sans","Noto Sans JP",sans-serif;
    display:flex; flex-direction:column; overflow:hidden;
    -webkit-text-size-adjust:100%;
  }
  header {
    flex:none; display:flex; align-items:center; gap:8px;
    padding:calc(env(safe-area-inset-top) + 10px) 14px 10px;
    background:#161b22; border-bottom:1px solid #21262d;
  }
  header .logo { font-weight:800; font-size:15px; color:#7ee1ff; letter-spacing:.5px; }
  header .ws { font-size:12px; color:#8b949e; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; flex:1; }
  #dot { width:9px; height:9px; border-radius:50%; background:#f85149; flex:none; }
  #dot.on { background:#3fb950; box-shadow:0 0 6px #3fb95088; }
  main { flex:1; overflow:hidden; position:relative; }
  .view { position:absolute; inset:0; display:none; flex-direction:column; }
  .view.act { display:flex; }
  nav {
    flex:none; display:flex; background:#161b22; border-top:1px solid #21262d;
    padding-bottom:env(safe-area-inset-bottom);
  }
  nav button {
    flex:1; background:none; border:none; color:#8b949e; font-size:10.5px;
    padding:8px 0 6px; display:flex; flex-direction:column; align-items:center; gap:2px;
  }
  nav button .ico { font-size:20px; }
  nav button.act { color:#7ee1ff; }
  .chips { flex:none; display:flex; gap:6px; overflow-x:auto; padding:8px 10px; -webkit-overflow-scrolling:touch; }
  .chips::-webkit-scrollbar { display:none; }
  .chip {
    flex:none; background:#21262d; color:#c9d1d9; border:1px solid #30363d;
    border-radius:14px; padding:6px 12px; font-size:12.5px; max-width:46vw;
    overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
  }
  .chip.act { background:#1f3a5f; border-color:#7ee1ff; color:#7ee1ff; }
  .chip.mic { font-size:15px; padding:6px 10px; }
  .chip.mic.rec { background:#6e2c1e; border-color:#f85149; color:#fff; animation:zvpulse 1.1s ease-in-out infinite; }
  @keyframes zvpulse { 50% { box-shadow:0 0 12px #f85149; } }
  #ta {
    flex:1; width:100%; background:#0d1117; color:#e6edf3; border:none; outline:none;
    font:13px/1.55 ui-monospace,SFMono-Regular,Menlo,monospace;
    padding:10px 12px; resize:none; white-space:pre; overflow:auto;
  }
  .bar { flex:none; display:flex; gap:8px; padding:8px 10px; background:#161b22; border-top:1px solid #21262d; align-items:center; }
  .btn {
    background:#21262d; color:#e6edf3; border:1px solid #30363d; border-radius:8px;
    padding:10px 14px; font-size:13.5px; font-weight:600;
  }
  .btn.pri { background:#1f6feb; border-color:#1f6feb; color:#fff; }
  .btn.warn { background:#6e2c1e; border-color:#f85149; }
  .btn:active { opacity:.7; }
  /* 宛先がいないときは押せない (誤爆ではなく「押しても何も起きない」を潰す) */
  .btn[disabled] { opacity:.35; }
  /* 一括操作の宛先行。宛先が無い (エージェント 0 体) ときは高さも取らない */
  #btgt {
    flex:none; display:none; align-items:center; gap:8px;
    padding:6px 10px; background:#161b22; border-bottom:1px solid #21262d;
    font-size:12px; color:#8b949e;
  }
  #btgt.show { display:flex; }
  #btgt .n { color:#e6edf3; font-weight:700; }
  #btgt .btn { padding:6px 10px; font-size:12px; }
  /* エージェントタブの中のビュー切替 (端末 / 待ち / デッキ / 看板)。
     下部ナビを増やさずに済ませる。指の当たり判定は 44px 以上を保つ */
  .seg { flex:none; display:flex; background:#161b22; border-bottom:1px solid #21262d; }
  .seg button {
    flex:1; min-width:0; padding:14px 2px; font-size:12.5px; font-weight:600;
    background:none; border:none; border-bottom:2px solid transparent; color:#8b949e;
    overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
  }
  .seg button.act { color:#7ee1ff; border-bottom-color:#7ee1ff; }
  .seg .badge {
    display:inline-block; margin-left:4px; padding:0 5px; border-radius:8px;
    background:#6e2c1e; color:#fff; font-size:10.5px; font-weight:700;
  }
  /* 一覧 (待ち / デッキ / 看板 で共有する 1 枚の入れ物) */
  #alist { flex:1; overflow-y:auto; -webkit-overflow-scrolling:touch; display:none; padding:8px 10px 12px; }
  #alist.show { display:block; }
  /* 空状態は「利用可能領域の中央に 1 枚」。下や上に取り残さない */
  #alist.mid { display:flex; align-items:center; justify-content:center; padding:16px; }
  .card {
    background:#161b22; border:1px solid #21262d; border-radius:10px;
    padding:10px 12px; margin-bottom:8px;
  }
  .card.act { border-color:#7ee1ff; }
  .card:active { background:#1c2432; }
  .card .hd { display:flex; align-items:center; gap:6px; font-size:13.5px; }
  .card .nm { flex:1; min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; font-weight:700; }
  .card .st { flex:none; font-size:11px; color:#8b949e; max-width:46%; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; }
  .card .pv {
    margin:6px 0 0; font:11px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;
    color:#8b949e; white-space:pre-wrap; word-break:break-all; max-height:3em; overflow:hidden;
  }
  .card .ax { display:flex; gap:8px; margin-top:8px; }
  .card .ax .btn {
    flex:1; min-width:0; padding:12px 6px; font-size:12.5px;
    overflow:hidden; text-overflow:ellipsis; white-space:nowrap;
  }
  .lane { margin:0 0 8px; }
  .lane .lhd {
    display:flex; align-items:center; gap:8px; width:100%; padding:12px 10px;
    background:#161b22; border:1px solid #21262d; border-radius:10px;
    color:#c9d1d9; font-size:13px; font-weight:700; text-align:left;
  }
  .lane .lhd .n { margin-left:auto; color:#8b949e; }
  .lane .body { padding:8px 0 0; }
  .mid-card {
    max-width:300px; text-align:center; background:#161b22; border:1px solid #21262d;
    border-radius:12px; padding:22px 18px; color:#8b949e; font-size:13px; line-height:1.7;
  }
  .mid-card .big { display:block; font-size:30px; margin-bottom:8px; }
  /* 音声が使えない端末への案内 (トーストと違い消えない) */
  #vnote {
    display:none; margin:0 0 8px; padding:9px 11px; border-radius:8px;
    background:#3a2c12; border:1px solid #d29922; color:#f2dfb4;
    font-size:12px; line-height:1.65; word-break:break-all;
  }
  #vnote.show { display:block; }
  .grow { flex:1; }
  #meta { font-size:11px; color:#8b949e; }
  #filter, #ti {
    flex:1; background:#0d1117; color:#e6edf3; border:1px solid #30363d; border-radius:8px;
    padding:10px 12px; font-size:16px; outline:none; min-width:0;
  }
  #flist { flex:1; overflow-y:auto; -webkit-overflow-scrolling:touch; }
  #flist div { padding:12px 14px; border-bottom:1px solid #1c2128; font-size:13.5px; }
  #flist div:active { background:#1f3a5f; }
  #flist .dir { color:#8b949e; font-size:11px; }
  #scr {
    flex:1; overflow:auto; -webkit-overflow-scrolling:touch; background:#010409;
    font:11px/1.45 ui-monospace,SFMono-Regular,Menlo,monospace;
    padding:8px 10px; white-space:pre; color:#c9d1d9;
  }
  .keys { flex:none; display:flex; gap:6px; overflow-x:auto; padding:6px 10px; background:#161b22; }
  .keys::-webkit-scrollbar { display:none; }
  .key {
    flex:none; background:#21262d; color:#e6edf3; border:1px solid #30363d;
    border-radius:8px; padding:9px 13px; font-size:13px; font-weight:600;
  }
  .key:active { background:#1f3a5f; }
  .grid { flex:1; overflow-y:auto; display:grid; grid-template-columns:1fr 1fr; gap:10px; padding:12px; align-content:start; }
  .grid .btn { padding:16px 8px; font-size:14px; text-align:center; }
  #toast {
    position:fixed; left:50%; bottom:calc(env(safe-area-inset-bottom) + 74px);
    transform:translateX(-50%); background:#1f6feb; color:#fff; padding:10px 18px;
    border-radius:20px; font-size:13px; opacity:0; transition:opacity .25s; pointer-events:none;
    max-width:86vw; white-space:nowrap; overflow:hidden; text-overflow:ellipsis; z-index:9;
  }
  #toast.show { opacity:1; }
  .empty { color:#8b949e; text-align:center; padding:40px 20px; font-size:13px; }
</style>
</head>
<body>
<header>
  <span class="logo">&#9889; ZAIVERN</span>
  <span class="ws" id="ws" data-i18n="remote.connecting">接続中…</span>
  <span id="dot"></span>
</header>
<main>
  <!-- エディタ -->
  <section class="view act" id="v-editor">
    <div class="chips" id="tabs"></div>
    <textarea id="ta" autocapitalize="off" autocorrect="off" spellcheck="false"
      data-i18n-ph="remote.editor_placeholder"
      placeholder="PC 側でファイルを開くか、[ファイル] タブから選択してください"></textarea>
    <div class="bar">
      <span id="meta"></span>
      <span class="grow"></span>
      <button class="btn" id="reload" data-i18n="remote.reload">&#8635; 再読込</button>
      <button class="btn pri" id="save" data-i18n="remote.save">&#128190; 保存</button>
    </div>
  </section>
  <!-- ファイル -->
  <section class="view" id="v-files">
    <div class="bar" style="border-top:none;border-bottom:1px solid #21262d">
      <input id="filter" type="search" data-i18n-ph="remote.file_filter_placeholder"
        placeholder="ファイル名で絞り込み…">
    </div>
    <div id="flist"></div>
  </section>
  <!-- エージェント -->
  <section class="view" id="v-agent">
    <div class="seg" id="aseg"></div>
    <div class="chips" id="achips"></div>
    <div id="btgt"></div>
    <div id="vnote"></div>
    <div id="scr" class="empty" data-i18n="remote.no_agents">エージェントがいません</div>
    <div id="alist"></div>
    <div class="keys" id="keys"></div>
    <div class="bar">
      <input id="ti" type="text" autocapitalize="off" autocorrect="off"
        data-i18n-ph="remote.agent_input_placeholder"
        placeholder="エージェントへ指示を送る…">
      <button class="btn" id="tput" data-i18n="remote.put" data-i18n-title="remote.put_title"
        title="Enter を送らずに入力欄へ入れるだけ">&#10549; 入れる</button>
      <button class="btn pri" id="tsend" data-i18n="remote.send">送信</button>
    </div>
  </section>
  <!-- コマンド -->
  <section class="view" id="v-cmds">
    <div class="grid" id="cmds"></div>
  </section>
</main>
<nav id="nav">
  <button data-v="editor" class="act"><span class="ico">&#128196;</span><span data-i18n="remote.tab_editor">エディタ</span></button>
  <button data-v="files"><span class="ico">&#128194;</span><span data-i18n="remote.tab_files">ファイル</span></button>
  <button data-v="agent"><span class="ico">&#129302;</span><span data-i18n="remote.tab_agent">エージェント</span></button>
  <button data-v="cmds"><span class="ico">&#127899;</span><span data-i18n="remote.tab_cmds">コマンド</span></button>
</nav>
<div id="toast"></div>
<script>
'use strict';
const qs = new URLSearchParams(location.search);
let TOK = qs.get('t') || localStorage.getItem('zv_tok') || '';
if (qs.get('t')) localStorage.setItem('zv_tok', qs.get('t'));
const $ = id => document.getElementById(id);
// ─── 多言語 (Language Pack) ───
// 文言はサーバが <head> の window.ZVI18N へ 1 回だけ注入する。
// 第 2 引数 d は日本語の原文フォールバック — 辞書が届かなくても画面は壊れない。
const T = (k, d) => (window.ZVI18N && window.ZVI18N[k]) || d;
// 静的な文言は HTML 側に data-i18n 属性で宣言しておき、起動時に一括で差し込む。
// 差し込み先は属性ごとに分ける (本文 / placeholder / title)。
// 訳が無いときは HTML に書いてある原文をそのまま残す (上書きしない)。
function applyI18n() {
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const v = T(el.dataset.i18n, ''); if (v) el.textContent = v;
  });
  document.querySelectorAll('[data-i18n-ph]').forEach(el => {
    const v = T(el.dataset.i18nPh, ''); if (v) el.placeholder = v;
  });
  document.querySelectorAll('[data-i18n-title]').forEach(el => {
    const v = T(el.dataset.i18nTitle, ''); if (v) el.title = v;
  });
}
// 音声認識の言語は画面の言語に合わせる (英語 UI なのに日本語を聞き取ろうとしない)。
// ja だけは地域つきの ja-JP が明確に良いので特別扱いする。
function speechLang() {
  const l = document.documentElement.lang || 'ja';
  return l === 'ja' ? 'ja-JP' : l;
}
let view = 'editor', dirty = false, files = [], state = null, curTab = -1;
let taTab = -1;  // textarea の内容がどのタブのものか (誤上書き防止)

function toast(m) {
  const t = $('toast'); t.textContent = m; t.classList.add('show');
  clearTimeout(t._h); t._h = setTimeout(() => t.classList.remove('show'), 1800);
}
async function api(path, body) {
  const opt = body
    ? { method:'POST', headers:{'Content-Type':'application/json','X-Token':TOK}, body:JSON.stringify(body) }
    : { headers:{'X-Token':TOK} };
  const r = await fetch(path, opt);
  if (r.status === 401) { toast(T('remote.auth_error', '認証エラー: QRコードを読み直してください')); throw 0; }
  if (!r.ok) throw 0;
  return r.json();
}

// ─── ビュー切替 ───
$('nav').addEventListener('click', e => {
  const b = e.target.closest('button'); if (!b) return;
  view = b.dataset.v;
  document.querySelectorAll('nav button').forEach(x => x.classList.toggle('act', x === b));
  document.querySelectorAll('.view').forEach(x => x.classList.toggle('act', x.id === 'v' + '-' + view));
  if (view === 'files' && !files.length) loadFiles();
  if (view === 'agent') pollTerm();
});

// ─── 状態ポーリング ───
async function pollState() {
  try {
    state = await api('/api/state');
    $('dot').classList.add('on');
    $('ws').textContent = state.workspace + (state.file ? ' — ' + state.file + (state.dirty ? ' ●' : '') : '');
    renderTabs(); renderAgents(); renderCmds(); renderSeg();
    if (curTab !== state.active) { curTab = state.active; if (!dirty) loadFile(); }
  } catch (e) { $('dot').classList.remove('on'); }
}
function renderTabs() {
  const el = $('tabs');
  el.innerHTML = '';
  (state.tabs || []).forEach((t, i) => {
    const c = document.createElement('button');
    c.className = 'chip' + (i === state.active ? ' act' : '');
    c.textContent = t.title + (t.dirty ? ' ●' : '');
    c.onclick = async () => { await api('/api/tab', {index:i}); dirty = false; await pollState(); };
    el.appendChild(c);
  });
}

// ─── エディタ ───
async function loadFile() {
  try {
    const f = await api('/api/file');
    if (!f.ok) { $('ta').value = ''; $('meta').textContent = ''; taTab = -1; return; }
    $('ta').value = f.text;
    // 文字コードは UTF-8 以外のときだけ出す (PC 側のステータスバーと同じ扱い)。
    // 保存でどう書かれるかがスマホからも分かるようにするため。
    $('meta').textContent =
      f.title + '  ·  ' + f.lang + (f.encoding ? '  ·  ' + f.encoding : '');
    taTab = (f.index === undefined || f.index === null) ? -1 : f.index;
    dirty = false;
  } catch (e) {}
}
$('ta').addEventListener('input', () => { dirty = true; });
$('reload').onclick = () => { dirty = false; loadFile().then(() => toast(T('remote.reloaded', '再読込しました'))); };
$('save').onclick = async () => {
  try {
    // 適用+保存を 1 リクエストで原子的に行う。タブ不一致はサーバ側で拒否される
    const r = await api('/api/text', {text: $('ta').value, index: taTab, save: true});
    if (r.ok) {
      dirty = false;
      // 元の文字コードで表せない文字を足すと UTF-8 へ切り替わる。
      // 黙って変わると「他のツールで読めなくなった」原因が分からないので必ず伝える
      if (r.promoted) {
        toast(T('remote.encoding_promoted', '{enc} では表せない文字があるため UTF-8 で保存しました')
          .replace('{enc}', r.was));
        loadFile();
      } else {
        toast(T('remote.saved', 'PC 側で保存しました ✅'));
      }
    } else {
      toast(r.error || T('remote.save_failed', '保存に失敗しました'));
    }
  } catch (e) { toast(T('remote.save_failed', '保存に失敗しました')); }
};

// ─── ファイル ───
async function loadFiles() {
  try {
    const r = await api('/api/files');
    files = r.files || [];
    renderFiles();
  } catch (e) {}
}
function renderFiles() {
  const q = $('filter').value.toLowerCase();
  const el = $('flist');
  el.innerHTML = '';
  const hit = files.filter(f => f.toLowerCase().includes(q)).slice(0, 400);
  if (!hit.length) {
    const e0 = document.createElement('div');
    e0.className = 'empty'; e0.textContent = T('remote.no_match', '該当なし');
    el.appendChild(e0); return;
  }
  hit.forEach(f => {
    const d = document.createElement('div');
    // Windows のパスは `src\app.rs` と区切りが `\` で来る。
    // `/` だけを見ていると全体がファイル名として出てフォルダ行が空になる
    const i = Math.max(f.lastIndexOf('/'), f.lastIndexOf('\\'));
    d.innerHTML = '<span></span><br><span class="dir"></span>';
    d.children[0].textContent = i >= 0 ? f.slice(i + 1) : f;
    d.children[2].textContent = i >= 0 ? f.slice(0, i) : '';
    d.onclick = async () => {
      await api('/api/open', {path: f});
      dirty = false;
      toast(T('remote.opened', '{path} を開きました').replace('{path}', f));
      document.querySelector('nav button[data-v=editor]').click();
      await pollState();
    };
    el.appendChild(d);
  });
}
$('filter').addEventListener('input', renderFiles);

// ─── エージェント ───
const ESC = '\u001b';
const KEYS = [
  ['Enter', '\r'], ['Esc', ESC], ['^C', '\u0003'],
  ['↑', ESC + '[A'], ['↓', ESC + '[B'],
  ['Tab', '\t'], [T('remote.key_shift_tab_perm', '⇧Tab 権限'), ESC + '[Z'],
  ['1', '1'], ['2', '2'], ['3', '3'], ['y', 'y'],
];
KEYS.forEach(([label, seq]) => {
  const b = document.createElement('button');
  b.className = 'key'; b.textContent = label;
  b.onclick = () => api('/api/term', {text: seq, raw: true}).catch(() => {});
  $('keys').appendChild(b);
});
// ─── 音声入力モード (エージェント毎) ───
// マイクボタンでトグル。話した内容は下の入力欄に溜まっていくだけで、
// 自動送信はしない。送るのは [⤵ 入れる] か [送信] を押したときだけ。
// 無音で認識が切れてもモードが ON なら自動で録音を再開する。
// voiceFatal = 復帰不能なエラーで止めた印。これが立っている間は onend で再開しない
// (network 等で無限リスタートし、画面上は無反応のまま壊れるのを防ぐ)
let voiceAgent = -1, recog = null, lastInterim = '', voiceFatal = false;
function speechAPI() { return window.SpeechRecognition || window.webkitSpeechRecognition; }
// 音声認識が使えるかを事前判定する。使えない理由コードを返す:
//   'insecure'    … http 接続 = セキュアコンテキストでない (スマホから見る場合はこれ)
//   'unsupported' … SpeechRecognition が無い (iOS Safari / Firefox など)
//   ''            … 使える
function speechBlockReason() {
  if (!window.isSecureContext) return 'insecure';
  if (!speechAPI()) return 'unsupported';
  return '';
}
// OS キーボードのディクテーション (Gboard の 🎤 / iOS 音声入力) への案内文。
// キーボード側の音声入力は https でなくても、ページ側の権限も要らずに使える。
// 原因と、いま何をすればいいかの両方を必ず書く。
function dictationHint(reason) {
  // 実際に待ち受けているポートをそのまま案内する (既定 8899 とは限らない)。
  // /voice の API はトークンを要るので、いま持っているものを付けて渡す
  const p = location.port || '8899';
  const u = 'http://127.0.0.1:' + p + '/voice' + (TOK ? '?t=' + encodeURIComponent(TOK) : '');
  const how = T('remote.dictation_how',
    'キーボードの \u{1F3A4} を押して、入力欄に話しかけてください（送信は手動 Enter）。'
    + 'PC からは {url} で連続認識が使えます。').replace('{url}', u);
  const why = reason === 'unsupported'
    ? T('remote.speech_unsupported', 'このブラウザは音声認識 (Web Speech API) に未対応です。')
    : reason === 'network'
    ? T('remote.speech_network', '音声認識サーバーに接続できませんでした（http 接続では利用できません）。')
    : T('remote.speech_insecure', 'この接続 (http) ではブラウザの音声認識が使えません。');
  return why + how;
}
function showNote(m) { const n = $('vnote'); n.textContent = m; n.classList.add('show'); }
function hideNote() { const n = $('vnote'); n.textContent = ''; n.classList.remove('show'); }
// 認識が使えないときの代替: 入力欄にフォーカスしてキーボード音声入力へ誘導する。
// 自動送信はしないので、話した内容は入力欄に残ったままになる。
function keyboardDictation(i, reason) {
  if (i >= 0) api('/api/cmd', {name:'agent_focus', arg:i}).then(pollState).catch(() => {});
  const t = $('ti');
  t.focus();
  try { t.setSelectionRange(t.value.length, t.value.length); } catch (e) {}
  t.placeholder = T('remote.dictation_placeholder', '\u{1F3A4} キーボードの音声入力で話しかけてください — 送信は手動');
  showNote(dictationHint(reason));
  toast(T('remote.dictation_toast', 'キーボードの \u{1F3A4} から入力してください'));
}
// 復帰不能なエラー。再開させずに止め、理由を消えない形で残す
function fatalVoiceStop(msg) {
  voiceFatal = true;
  stopVoice0();
  renderAgents();
  showNote(msg);
  toast(msg);
}
function stopVoice0() {
  voiceAgent = -1;
  const r = recog; recog = null;
  if (r) { r.onend = null; try { r.stop(); } catch (e) {} }
  if ($('ti').value === lastInterim) $('ti').value = '';
  lastInterim = '';
  $('ti').placeholder = T('remote.agent_input_placeholder', 'エージェントへ指示を送る…');
}
function stopVoice() { stopVoice0(); hideNote(); renderAgents(); toast(T('remote.voice_mode_off', '\u{1F3A4} 音声入力モード OFF')); }
function startVoice(i) {
  // 使えない端末では死んだエラーを出さず、キーボード音声入力へ逃がす
  const reason = speechBlockReason();
  if (reason) { stopVoice0(); renderAgents(); keyboardDictation(i, reason); return; }
  const C = speechAPI();
  stopVoice0();
  hideNote();
  voiceFatal = false;
  voiceAgent = i;
  api('/api/cmd', {name:'agent_focus', arg:i}).then(pollState).catch(() => {});
  const r = new C();
  recog = r;
  r.lang = speechLang();
  r.continuous = true;
  r.interimResults = true;
  r.onresult = ev => {
    let fin = '', interim = '';
    for (let k = ev.resultIndex; k < ev.results.length; k++) {
      const t = ev.results[k][0].transcript;
      if (ev.results[k].isFinal) fin += t; else interim += t;
    }
    // 途中経過は「入力欄の末尾に仮表示」。確定したらその場で本文に変わる
    const base = $('ti').value.endsWith(lastInterim) && lastInterim
      ? $('ti').value.slice(0, -lastInterim.length)
      : $('ti').value;
    fin = fin.trim();
    if (fin) {
      $('ti').value = (base + (base && !base.endsWith(' ') ? ' ' : '') + fin).trim();
      lastInterim = '';
    } else {
      $('ti').value = base + interim;
      lastInterim = interim;
    }
  };
  r.onerror = ev => {
    const e = ev.error;
    if (e === 'no-speech') return;              // 無音だけ: onend の自動再開に任せる
    if (e === 'not-allowed' || e === 'service-not-allowed') {
      fatalVoiceStop(T('remote.mic_not_allowed', 'マイクが許可されていません（ブラウザ設定を確認）'));
    } else if (e === 'network') {
      // 認識サーバーへ到達できない = http 経由ではほぼ復帰しない。案内して終わる
      voiceFatal = true;
      stopVoice0(); renderAgents();
      keyboardDictation(i, 'network');
    } else if (e === 'audio-capture') {
      fatalVoiceStop(T('remote.mic_not_found', 'マイクが見つかりません'));
    } else if (e === 'aborted') {
      stopVoice0(); renderAgents();            // 明示停止・画面遷移。黙って終わる
    }
  };
  r.onend = () => {
    if (voiceFatal) return;                    // 致命的エラー後は再開しない
    if (recog === r && voiceAgent === i) {
      try { r.start(); } catch (e) { stopVoice(); }
    }
  };
  try { r.start(); } catch (e) { toast(T('remote.voice_start_failed', '音声入力を開始できません')); stopVoice0(); renderAgents(); return; }
  $('ti').placeholder = T('remote.voice_placeholder', '\u{1F3A4} 話した内容がここに溜まります — 送信はボタンで');
  renderAgents();
  const a = (state.agents || [])[i];
  toast(T('remote.voice_mode_on', '\u{1F3A4} 音声入力モード ON → {agent} (自動送信はしません)')
    .replace('{agent}', a ? a.title : ''));
}
// ─── 一括操作 (宛先の粒度) ───
// 'one'     … いま選んでいる 1 体 (既定)
// 'all'     … 起動中の全エージェント (PC 側の「📣 全エージェントへブロードキャスト」)
// 'stalled' … 止まっている (待機中の) ものだけ (PC 側の「止まっているものへまとめて送る」)
//
// 既定をいちばん狭い 'one' にするのは、画面を開いた瞬間が全員宛てだと
// 打ち込んだ 1 行がそのまま全機へ飛ぶため。
// 件数は **PC 側が数えた state.bulk をそのまま出す**。スマホ側でも数えると
// 数え方が 2 か所になり、「3 体と出ているのに 5 体へ飛んだ」が起こりうる。
let bulkMode = 'one';
function bulkCount(m) { return ((state && state.bulk) || {})[m || bulkMode] || 0; }
function bulkModeLabel(m) {
  return m === 'all' ? T('remote.bulk_all', '\u{1F4E3} 全員')
    : m === 'stalled' ? T('remote.bulk_stalled', '⏸ 待機')
    : T('remote.bulk_one', '\u{1F916} 選択中');
}
// 宛先行: 何体へ届くのかを送信前に必ず見せる + 一括停止をここから届かせる。
// エージェントが 0 体なら行ごと消す (中身の無い帯で高さを取らない)。
function renderBulk() {
  const el = $('btgt');
  el.innerHTML = '';
  const agents = (state && state.agents) || [];
  const n = bulkCount();
  el.classList.toggle('show', agents.length > 0);
  if (agents.length) {
    const lab = document.createElement('span');
    lab.className = 'grow';
    lab.textContent = T('remote.bulk_target', '宛先: {mode} — {n} 体')
      .replace('{mode}', bulkModeLabel(bulkMode)).replace('{n}', n);
    el.appendChild(lab);
    const stop = document.createElement('button');
    stop.className = 'btn warn';
    stop.textContent = T('remote.bulk_stop', '⏹ 停止');
    stop.title = T('remote.bulk_stop_title', '宛先へ Esc を送っていまの作業を止める');
    stop.disabled = n === 0;
    stop.onclick = bulkStop;
    el.appendChild(stop);
  }
  // 宛先 0 体では送れない。押しても届かないボタンは押させない
  $('tsend').disabled = n === 0;
  $('tput').disabled = n === 0;
}
// 一括停止 = 宛先へ Esc。セッションを殺すのは PC 側の確認モーダルに任せる
// (スマホから殺すと、誰も押せないダイアログが PC に開いたままになる)。
async function bulkStop() {
  if (!bulkCount()) return;
  try {
    const r = await api('/api/bulk_stop', {mode: bulkMode});
    toast(r.ok
      ? T('remote.bulk_stopped', '⏹ {n} 体へ停止 (Esc) を送りました').replace('{n}', r.sent)
      : (r.error || T('remote.bulk_failed', '送信できませんでした')));
  } catch (e) {}
}
function renderAgents() {
  const el = $('achips');
  el.innerHTML = '';
  const agents = state.agents || [];
  if (voiceAgent >= agents.length) stopVoice0();
  // 1 体以下なら「全員 / 待機」を選ぶ余地が無いので出さない (到達経路を増やさない)。
  // 減ったときは宛先を 1 体へ戻す — 消えた宛先のまま送らせない
  if (agents.length < 2 && bulkMode !== 'one') bulkMode = 'one';
  if (agents.length >= 2) {
    ['all', 'stalled'].forEach(m => {
      const c = document.createElement('button');
      c.className = 'chip' + (bulkMode === m ? ' act' : '');
      c.textContent = bulkModeLabel(m) + ' ' + bulkCount(m);
      c.onclick = () => { bulkMode = m; renderAgents(); };
      el.appendChild(c);
    });
  }
  agents.forEach((a, i) => {
    const c = document.createElement('button');
    c.className = 'chip' + (bulkMode === 'one' && i === state.agent_active ? ' act' : '');
    c.textContent = (a.running ? (a.attention ? '\u{1F514} ' : a.stalled ? '⏸ ' : '● ') : '○ ') + a.icon + ' ' + a.title;
    // 1 体を選んだら宛先も 1 体へ戻す (全員宛てのまま個別チップを押して誤爆しない)
    c.onclick = () => { bulkMode = 'one'; api('/api/cmd', {name:'agent_focus', arg:i}).then(pollState).catch(() => renderAgents()); };
    el.appendChild(c);
    const m = document.createElement('button');
    m.className = 'chip mic' + (i === voiceAgent ? ' rec' : '');
    m.textContent = i === voiceAgent ? T('remote.stop', '⏹ 停止') : '\u{1F3A4}';
    m.title = T('remote.mic_title', '{agent} へ音声入力').replace('{agent}', a.title);
    m.onclick = () => (i === voiceAgent ? stopVoice() : startVoice(i));
    el.appendChild(m);
  });
  const plus = document.createElement('button');
  plus.className = 'chip'; plus.textContent = T('remote.launch', '＋ 起動');
  plus.onclick = () => {
    const names = (state.presets || []).map((p, i) => i + ': ' + p.icon + ' ' + p.name).join('\n');
    const v = prompt(T('remote.launch_prompt', '起動するプリセット番号') + '\n' + names, '0');
    if (v !== null) api('/api/cmd', {name:'agent_launch', arg:parseInt(v) || 0}).then(pollState).catch(() => {});
  };
  el.appendChild(plus);
  renderBulk();
}
// ─── エージェントタブの中のビュー切替 ───────────────────────────
// 下部ナビ (エディタ/ファイル/エージェント/コマンド) は増やさず、この中で切り替える。
//   'term'   … 端末 (従来どおり)
//   'wait'   … 人の手が要るもの (返事待ち・承認・停滞) だけを縦に並べる
//   'deck'   … PC のデッキ相当。スマホなので 1 列固定 (横スクロールを作らない)
//   'kanban' … PC の看板相当。レーンは横に並べず「見出し + カード」の縦積み
// レーンの定義・状態ラベルは **PC 側 (kanban.rs) が決めたものをそのまま出す**。
// スマホ側で画面文字から状態を決め直さない (設計原則 4)。
let aview = 'term', alist = [], alanes = [], laneOpen = {};
const AVIEWS = [
  ['term', () => T('remote.view_term', '\u{1F5A5} 端末')],
  ['wait', () => T('remote.view_wait', '⏳ 待ち')],
  ['deck', () => T('remote.view_deck', '\u{1F0CF} デッキ')],
  ['kanban', () => T('remote.view_kanban', '\u{1F4CB} 看板')],
];
// 待ち件数は **PC が数えた state.waiting** をそのまま出す。一覧を開いていない
// 間も /api/state だけで最新になるので、バッジのために余分に叩かない。
function waitCount() { return (state && state.waiting) || 0; }
function renderSeg() {
  const el = $('aseg');
  el.innerHTML = '';
  AVIEWS.forEach(([k, lab]) => {
    const b = document.createElement('button');
    if (aview === k) b.className = 'act';
    b.appendChild(document.createTextNode(lab()));
    const n = k === 'wait' ? waitCount() : 0;
    if (n) {
      const s = document.createElement('span');
      s.className = 'badge'; s.textContent = n;
      b.appendChild(s);
    }
    b.onclick = () => setAView(k);
    el.appendChild(b);
  });
}
function setAView(k) {
  if (aview === k) return;
  aview = k;
  const term = k === 'term';
  // 端末と一覧は同じ場所を使う。切り替えで**上下のバーは動かさない**ので、
  // 画面が突然作り替わったようには見えない
  $('scr').style.display = term ? '' : 'none';
  $('keys').style.display = term ? '' : 'none';
  $('alist').classList.toggle('show', !term);
  renderSeg();
  renderList();
  pollTerm();   // 切り替えた瞬間に取りに行く (1 テンポ空白にしない)
}
function laneIcon(i) { const L = alanes.find(x => x.i === i); return L ? L.icon : ''; }
// 空状態は利用可能領域の中央に 1 枚のカードで出す (CLAUDE.md の UI 原則)
function emptyCard(k) {
  const d = document.createElement('div');
  d.className = 'mid-card';
  const b = document.createElement('span');
  b.className = 'big';
  b.textContent = alist.length === 0 ? '\u{1F916}' : (k === 'wait' ? '✅' : '\u{1F4A4}');
  d.appendChild(b);
  const t = document.createElement('span');
  t.textContent = alist.length === 0
    ? T('remote.no_agents_hint', 'エージェントがいません — ＋ 起動 から始められます')
    : (k === 'wait'
      ? T('remote.wait_empty', '待っているエージェントはいません — 全員動いています')
      : T('remote.list_empty', '表示できるエージェントがいません'));
  d.appendChild(t);
  return d;
}
function cardBtn(label, cls, fn) {
  const b = document.createElement('button');
  b.className = 'btn' + (cls ? ' ' + cls : '');
  b.textContent = label;
  b.onclick = e => { e.stopPropagation(); fn(); };
  return b;
}
// 1 枚のカード: 名前 / 状態 / 直近出力の末尾 2 行 / 経過時間 + その場の操作。
// 3 つのビュー (待ち・デッキ・看板) で同じカードを使う — 見た目を作り分けない。
function agentCard(a) {
  const c = document.createElement('div');
  c.className = 'card' + (a.active ? ' act' : '');
  const hd = document.createElement('div'); hd.className = 'hd';
  const ic = document.createElement('span'); ic.textContent = a.icon; hd.appendChild(ic);
  const nm = document.createElement('span'); nm.className = 'nm';
  nm.textContent = (a.running ? (a.attention ? '\u{1F514} ' : a.unread ? '● ' : '') : '○ ') + a.title;
  hd.appendChild(nm);
  const st = document.createElement('span'); st.className = 'st';
  st.textContent = laneIcon(a.lane) + ' ' + a.state + (a.running ? ' · ' + a.uptime : '');
  hd.appendChild(st);
  c.appendChild(hd);
  if (a.preview) {
    const p = document.createElement('pre'); p.className = 'pv'; p.textContent = a.preview;
    c.appendChild(p);
  }
  const ax = document.createElement('div'); ax.className = 'ax';
  // 承認は「人の手が要る」ときだけ出す (いつも並ぶ押せないボタンを作らない)
  if (a.attention) ax.appendChild(cardBtn(T('remote.card_approve', '✅ 承認'), 'pri', () => agentAct(a, 'approve')));
  if (a.running) ax.appendChild(cardBtn(T('remote.card_send', '✏ 指示'), '', () => openAgent(a, true)));
  if (a.running) ax.appendChild(cardBtn(T('remote.card_stop', '⏹ 停止'), 'warn', () => agentAct(a, 'stop')));
  if (ax.childNodes.length) c.appendChild(ax);
  // タップ = そのエージェントへ入る
  c.onclick = () => openAgent(a, false);
  return c;
}
// カードをタップ = 選んで端末へ入る。[✏ 指示] は一覧に留まったまま宛先だけ移す
// (「一覧で見つけて、その場で 1 行送る」を 1 タップで終わらせる)
function openAgent(a, stay) {
  bulkMode = 'one';
  api('/api/cmd', {name:'agent_focus', arg:a.idx}).then(pollState).catch(() => renderAgents());
  if (stay) { $('ti').focus(); } else { setAView('term'); }
}
// 行内の操作。承認キーは PC 側 (エージェントのカタログ) が知っているので、
// スマホから当て推量の文字を送らない
async function agentAct(a, act) {
  try {
    const r = await api('/api/agent_act', {id: a.id, act: act});
    if (!r.ok) { toast(r.error || T('remote.bulk_failed', '送信できませんでした')); return; }
    toast((act === 'approve'
      ? T('remote.approved', '✅ {agent} を承認しました')
      : T('remote.stopped_one', '⏹ {agent} を止めました')).replace('{agent}', a.title));
    pollTerm();
  } catch (e) {}
}
function renderList() {
  if (aview === 'term') return;
  const el = $('alist');
  // 1.5 秒ごとに作り直すので、読んでいる位置を必ず戻す
  // (戻さないと、スクロールした瞬間に毎回先頭へ跳ね上がる)
  const keep = el.scrollTop;
  el.innerHTML = '';
  el.classList.remove('mid');
  // 「待ち」は PC 側の判定 (remote::is_waiting_lane) で印が付いたものだけ
  const rows = aview === 'wait' ? alist.filter(a => a.waiting) : alist;
  if (!rows.length) { el.classList.add('mid'); el.appendChild(emptyCard(aview)); return; }
  if (aview === 'kanban') renderKanban(el, rows);
  else rows.forEach(a => el.appendChild(agentCard(a)));
  el.scrollTop = keep;
}
// 看板: レーン見出し + その下にカードの縦積み。空のレーンは見出しごと出さない
// (常に 0 と出る見出しを 8 本並べない)。見出しをタップで畳める。
function renderKanban(el, rows) {
  let shown = 0;
  alanes.forEach(L => {
    const mem = rows.filter(a => a.lane === L.i);
    if (!mem.length) return;
    shown += mem.length;
    const open = laneOpen[L.i] !== false;
    const box = document.createElement('div'); box.className = 'lane';
    const hd = document.createElement('button'); hd.className = 'lhd';
    const car = document.createElement('span'); car.textContent = open ? '▾' : '▸';
    hd.appendChild(car);
    const t = document.createElement('span'); t.textContent = L.icon + ' ' + L.title;
    hd.appendChild(t);
    const n = document.createElement('span'); n.className = 'n'; n.textContent = mem.length;
    hd.appendChild(n);
    hd.onclick = () => { laneOpen[L.i] = !open; renderList(); };
    box.appendChild(hd);
    if (open) {
      const body = document.createElement('div'); body.className = 'body';
      mem.forEach(a => body.appendChild(agentCard(a)));
      box.appendChild(body);
    }
    el.appendChild(box);
  });
  if (!shown) { el.classList.add('mid'); el.appendChild(emptyCard('kanban')); }
}
let termTimer = null;
// ポーリングは**ビューが増えても 1 本のまま**。端末を見ているときは /api/term、
// 一覧を見ているときは /api/agents を同じ間隔で叩く (合計回数は増えない)。
// 見ていないビューのために PTY を読ませない。
async function pollTerm() {
  clearTimeout(termTimer);
  if (view !== 'agent') return;
  try {
    if (aview === 'term') {
      const r = await api('/api/term');
      const el = $('scr');
      if (r.ok) {
        const stick = el.scrollTop + el.clientHeight >= el.scrollHeight - 24;
        el.classList.remove('empty');
        el.textContent = r.text;
        if (stick) el.scrollTop = el.scrollHeight;
      } else {
        el.classList.add('empty');
        el.textContent = T('remote.no_agents_hint', 'エージェントがいません — ＋ 起動 から始められます');
      }
    } else {
      const r = await api('/api/agents');
      if (r.ok) { alist = r.agents || []; alanes = r.lanes || []; renderList(); }
    }
  } catch (e) {}
  termTimer = setTimeout(pollTerm, 1500);
}
// 送信 = テキスト + Enter。入れる = テキストのみ (PC 側で内容を見て Enter)
// 宛先は bulkMode が決める。1 体宛て / 全員 / 待機だけ のどれも同じ入口を通る
// ので、「1 体には届くのに一括だけ挙動が違う」が起きない。
async function sendInput(submit) {
  const v = $('ti').value.trim();
  if (!v) return;
  if (!bulkCount()) { toast(T('remote.bulk_none', '送れる宛先がいません')); return; }
  if (bulkMode === 'one' && voiceAgent >= 0) {
    // 音声モード中は、選んだエージェントへ確実に届くようフォーカスし直す
    await api('/api/cmd', {name:'agent_focus', arg:voiceAgent}).catch(() => {});
  }
  let r = null;
  try { r = await api('/api/bulk', {text: v, mode: bulkMode, submit: submit}); } catch (e) { return; }
  if (!r.ok) { toast(r.error || T('remote.bulk_failed', '送信できませんでした')); return; }
  $('ti').value = ''; lastInterim = '';
  if (bulkMode === 'one') {
    toast(submit
      ? T('remote.sent', '送信しました')
      : T('remote.put_done', 'PC の入力欄に入れました (Enter で送信)'));
  } else {
    toast((submit
      ? T('remote.bulk_sent', '\u{1F4E3} {n} 体へ送信しました')
      : T('remote.bulk_put', '\u{1F4E3} {n} 体の入力欄に入れました')).replace('{n}', r.sent));
  }
}
$('tsend').onclick = () => sendInput(true);
$('tput').onclick = () => sendInput(false);
$('ti').addEventListener('keydown', e => { if (e.key === 'Enter') sendInput(true); });

// ─── コマンド ───
const CMDS = [
  [T('remote.save', '\u{1F4BE} 保存'), 'save'],
  [T('remote.cmd_new', '\u{1F4C4} 新規ファイル'), 'new'],
  [T('remote.cmd_close_tab', '❌ タブを閉じる'), 'close_tab'],
  [T('remote.cmd_terminal', '\u{1F5A5} ターミナル'), 'terminal'],
  [T('remote.cmd_sidebar', '\u{1F4C1} サイドバー'), 'sidebar'],
  [T('remote.cmd_cockpit', '\u{1F39b} Cockpit'), 'cockpit'],
  [T('remote.cmd_zoom_in', '\u{1F50D} ズーム +'), 'zoom_in'],
  [T('remote.cmd_zoom_out', '\u{1F50D} ズーム −'), 'zoom_out'],
  [T('remote.cmd_zoom_reset', '\u{1F50D} ズーム 100%'), 'zoom_reset'],
  [T('remote.cmd_tree', '\u{1F332} ツリー更新'), 'tree'],
  [T('remote.cmd_approval_ask', '\u{1F6e1} 承認モード'), 'approval_ask'],
  [T('remote.cmd_approval_auto', '⚡ 全自動モード'), 'approval_auto'],
  [T('remote.cmd_approval_agent', '\u{1F916} Agent優先モード'), 'approval_agent'],
  [T('remote.cmd_permission_cycle', '\u{1F6e1} 権限切替(全Agent)'), 'permission_cycle'],
];
function renderCmds() {
  const el = $('cmds');
  if (el.childElementCount) return;
  CMDS.forEach(([label, name]) => {
    const b = document.createElement('button');
    b.className = 'btn' + (name === 'approval_auto' ? ' warn' : '');
    b.textContent = label;
    b.onclick = () => api('/api/cmd', {name: name, arg: 0})
      .then(r => toast(r.ok
        ? T('remote.cmd_done', '{label} を実行').replace('{label}', label)
        : (r.error || T('remote.failed', '失敗しました'))))
      .catch(() => {});
    el.appendChild(b);
  });
}

applyI18n();
renderSeg();
pollState();
setInterval(pollState, 2500);
</script>
</body>
</html>
"##;

// ─── PC 用 音声入力ページ ────────────────────────────────────────────
//
// デスクトップの 🎤 ボタンから 127.0.0.1 で開かれる (Web Speech API は
// セキュアコンテキスト必須のため localhost であることが重要)。
// 送信先はセッション id で選択でき、?target=<id|all> で初期選択が決まる。

const VOICE_PAGE: &str = r##"<!DOCTYPE html>
<html lang="ja">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta name="theme-color" content="#0d1117">
<title data-i18n="remote.voice_page_title">Zaivern 音声入力</title>
<script>/*__ZV_I18N__*/</script>
<style>
  * { margin:0; padding:0; box-sizing:border-box; }
  body {
    background:#0d1117; color:#e6edf3; min-height:100vh;
    font-family:-apple-system,BlinkMacSystemFont,"Hiragino Sans","Noto Sans JP",sans-serif;
    display:flex; flex-direction:column; align-items:center;
  }
  header {
    width:100%; display:flex; align-items:center; gap:10px;
    padding:12px 18px; background:#161b22; border-bottom:1px solid #21262d;
  }
  .logo { font-weight:800; font-size:15px; color:#7ee1ff; letter-spacing:.5px; }
  #dot { width:9px; height:9px; border-radius:50%; background:#f85149; }
  #dot.on { background:#3fb950; box-shadow:0 0 6px #3fb95088; }
  main { width:100%; max-width:680px; padding:22px 18px 40px; display:flex; flex-direction:column; gap:12px; }
  h2 { font-size:12.5px; color:#8b949e; font-weight:600; }
  .chips { display:flex; flex-wrap:wrap; gap:8px; }
  .chip {
    background:#21262d; color:#c9d1d9; border:1px solid #30363d;
    border-radius:16px; padding:8px 14px; font-size:13.5px; cursor:pointer;
  }
  .chip.act { background:#1f3a5f; border-color:#7ee1ff; color:#7ee1ff; }
  #mic {
    margin:14px auto 4px; width:120px; height:120px; border-radius:50%;
    border:2px solid #30363d; background:#161b22; color:#e6edf3;
    font-size:46px; cursor:pointer;
  }
  #mic.rec { background:#6e2c1e; border-color:#f85149; animation:zvpulse 1.1s ease-in-out infinite; }
  @keyframes zvpulse { 50% { box-shadow:0 0 24px #f85149; } }
  #hint { text-align:center; color:#8b949e; font-size:13px; min-height:1.5em; }
  #interim { text-align:center; color:#7ee1ff; font-size:15px; min-height:1.6em; }
  #draft {
    width:100%; min-height:96px; resize:vertical; background:#0d1117; color:#e6edf3;
    border:1px solid #30363d; border-radius:10px; padding:12px 14px; font-size:15px;
    line-height:1.6; outline:none; font-family:inherit;
  }
  #draft:focus { border-color:#7ee1ff; }
  .row { display:flex; gap:8px; align-items:center; }
  .grow { flex:1; }
  .btn {
    background:#21262d; color:#e6edf3; border:1px solid #30363d; border-radius:8px;
    padding:10px 16px; font-size:13.5px; font-weight:600; cursor:pointer;
  }
  .btn.pri { background:#1f6feb; border-color:#1f6feb; color:#fff; }
  .btn:active { opacity:.7; }
  #log { display:flex; flex-direction:column; gap:6px; }
  #log div {
    background:#161b22; border:1px solid #21262d; border-radius:8px;
    padding:8px 12px; font-size:13.5px; word-break:break-all;
  }
</style>
</head>
<body>
<header>
  <span class="logo">&#9889; ZAIVERN &#127908; <span data-i18n="remote.voice_header">音声入力</span></span>
  <span id="dot"></span>
</header>
<main>
  <h2 data-i18n="remote.voice_target_heading">送信先 (クリックで切替 — 話している途中でも変更できます)</h2>
  <div class="chips" id="targets"></div>
  <button id="mic">&#127908;</button>
  <div id="hint" data-i18n="remote.voice_hint">マイクボタンを押して話しかけてください — 内容を確認してからボタンで送ります</div>
  <div id="interim"></div>
  <textarea id="draft" data-i18n-ph="remote.voice_draft_placeholder"
    placeholder="話した内容がここに溜まります。直してから送信できます。"></textarea>
  <div class="row">
    <button class="btn" id="clear" data-i18n="remote.voice_clear">&#128465; 消す</button>
    <span class="grow"></span>
    <button class="btn" id="put" data-i18n="remote.voice_put" data-i18n-title="remote.put_title"
      title="Enter を送らずに入力欄へ入れるだけ">&#10549; 入力欄へ入れる</button>
    <button class="btn pri" id="send" data-i18n="remote.voice_send">&#9654; 送信 (Enter まで送る)</button>
  </div>
  <div id="log"></div>
</main>
<script>
'use strict';
const qs = new URLSearchParams(location.search);
const TOK = qs.get('t') || '';
let target = qs.get('target') || 'all';  // 'all' またはセッション id
// voiceFatal = 復帰不能なエラーで止めた印。立っている間は onend で再開しない
let agents = [], active = false, recog = null, voiceFatal = false;
const $ = id => document.getElementById(id);
// ─── 多言語 (Language Pack) ───
// 文言はサーバが <head> の window.ZVI18N へ 1 回だけ注入する。
// 第 2 引数 d は日本語の原文フォールバック — 辞書が届かなくても画面は壊れない。
const T = (k, d) => (window.ZVI18N && window.ZVI18N[k]) || d;
// 静的な文言は HTML 側に data-i18n 属性で宣言しておき、起動時に一括で差し込む。
// 差し込み先は属性ごとに分ける (本文 / placeholder / title)。
// 訳が無いときは HTML に書いてある原文をそのまま残す (上書きしない)。
function applyI18n() {
  document.querySelectorAll('[data-i18n]').forEach(el => {
    const v = T(el.dataset.i18n, ''); if (v) el.textContent = v;
  });
  document.querySelectorAll('[data-i18n-ph]').forEach(el => {
    const v = T(el.dataset.i18nPh, ''); if (v) el.placeholder = v;
  });
  document.querySelectorAll('[data-i18n-title]').forEach(el => {
    const v = T(el.dataset.i18nTitle, ''); if (v) el.title = v;
  });
}
// 音声認識の言語は画面の言語に合わせる (英語 UI なのに日本語を聞き取ろうとしない)。
// ja だけは地域つきの ja-JP が明確に良いので特別扱いする。
function speechLang() {
  const l = document.documentElement.lang || 'ja';
  return l === 'ja' ? 'ja-JP' : l;
}
const HINT0 = T('remote.voice_hint', 'マイクボタンを押して話しかけてください — 内容を確認してからボタンで送ります');

async function api(path, body) {
  const opt = body
    ? { method:'POST', headers:{'Content-Type':'application/json','X-Token':TOK}, body:JSON.stringify(body) }
    : { headers:{'X-Token':TOK} };
  const r = await fetch(path, opt);
  if (!r.ok) throw 0;
  return r.json();
}
function renderTargets() {
  const el = $('targets');
  el.innerHTML = '';
  const all = document.createElement('button');
  all.className = 'chip' + (target === 'all' ? ' act' : '');
  all.textContent = T('remote.voice_broadcast', '\u{1F4E3} 全エージェントへブロードキャスト');
  all.onclick = () => { target = 'all'; renderTargets(); };
  el.appendChild(all);
  agents.forEach(a => {
    const c = document.createElement('button');
    c.className = 'chip' + (String(a.id) === String(target) ? ' act' : '');
    c.textContent = (a.running ? '● ' : '○ ') + a.icon + ' ' + a.title;
    c.onclick = () => { target = String(a.id); renderTargets(); };
    el.appendChild(c);
  });
}
async function poll() {
  try {
    const s = await api('/api/state');
    agents = s.agents || [];
    $('dot').classList.add('on');
    // 選択中のセッションが閉じられたらブロードキャストへ戻す
    if (target !== 'all' && !agents.some(a => String(a.id) === String(target))) target = 'all';
    renderTargets();
  } catch (e) { $('dot').classList.remove('on'); }
}
function targetName() {
  if (target === 'all') return T('remote.voice_all_agents', '\u{1F4E3} 全エージェント');
  const a = agents.find(x => String(x.id) === String(target));
  return a ? a.icon + ' ' + a.title : '?';
}
function addLog(m) {
  const d = document.createElement('div');
  d.textContent = m;
  $('log').prepend(d);
  while ($('log').childElementCount > 50) $('log').lastChild.remove();
}
// submit=false は入力欄へ入れるだけ (Enter は送らない)。
// 話しただけでは絶対に送信されない — 押したときだけ送る。
async function send(submit) {
  const text = $('draft').value.trim();
  if (!text) return;
  const id = target === 'all' ? -1 : Number(target);
  const name = targetName();
  try {
    const r = await api('/api/voice', {text: text, id: id, submit: submit});
    if (r.ok) {
      addLog(T(submit ? 'remote.voice_log_sent' : 'remote.voice_log_put',
        submit ? '▶ 送信 {target} ← {text}' : '⤵ 入力欄へ {target} ← {text}')
        .replace('{target}', name).replace('{text}', text));
      $('draft').value = '';
    } else {
      addLog('⚠ ' + (r.error || T('remote.voice_log_failed', '失敗')) + ': ' + text);
    }
  } catch (e) {
    addLog(T('remote.voice_send_failed', '⚠ 送信に失敗しました: {text}').replace('{text}', text));
  }
}
$('send').onclick = () => send(true);
$('put').onclick = () => send(false);
$('clear').onclick = () => { $('draft').value = ''; };
function speechAPI() { return window.SpeechRecognition || window.webkitSpeechRecognition; }
// 音声認識が使えるかを事前判定する。使えない理由コードを返す:
//   'insecure'    … http 接続 = セキュアコンテキストでない (LAN の IP で開いた場合)
//   'unsupported' … SpeechRecognition が無い (iOS Safari / Firefox など)
//   ''            … 使える
function speechBlockReason() {
  if (!window.isSecureContext) return 'insecure';
  if (!speechAPI()) return 'unsupported';
  return '';
}
// OS キーボードのディクテーションへの案内文。キーボード側の音声入力なら
// https でなくても、ページ側の権限も要らずに使える。
// 原因と、いま何をすればいいかの両方を必ず書く。
function dictationHint(reason) {
  // 実際に待ち受けているポートをそのまま案内する (既定 8899 とは限らない)。
  // /voice の API はトークンを要るので、いま持っているものを付けて渡す
  const p = location.port || '8899';
  const u = 'http://127.0.0.1:' + p + '/voice' + (TOK ? '?t=' + encodeURIComponent(TOK) : '');
  const how = T('remote.dictation_how',
    'キーボードの \u{1F3A4} を押して、入力欄に話しかけてください（送信は手動 Enter）。'
    + 'PC からは {url} で連続認識が使えます。').replace('{url}', u);
  const why = reason === 'unsupported'
    ? T('remote.speech_unsupported', 'このブラウザは音声認識 (Web Speech API) に未対応です。')
    : reason === 'network'
    ? T('remote.speech_network', '音声認識サーバーに接続できませんでした（http 接続では利用できません）。')
    : T('remote.speech_insecure', 'この接続 (http) ではブラウザの音声認識が使えません。');
  return why + how;
}
// 認識が使えないときの代替: 下書き欄へフォーカスしてキーボード音声入力へ誘導する。
// 自動送信はしないので、話した内容は下書き欄に残ったままになる。
function keyboardDictation(reason) {
  const d = $('draft');
  d.focus();
  try { d.setSelectionRange(d.value.length, d.value.length); } catch (e) {}
  d.placeholder = T('remote.dictation_placeholder', '\u{1F3A4} キーボードの音声入力で話しかけてください — 送信は手動');
  $('hint').textContent = dictationHint(reason);
}
// 復帰不能なエラー。再開させずに止め、理由を残す
function fatalVoiceStop(msg) {
  voiceFatal = true;
  stopVoice();
  $('hint').textContent = msg;
}
function stopVoice() {
  active = false;
  const r = recog; recog = null;
  if (r) { r.onend = null; try { r.stop(); } catch (e) {} }
  $('mic').classList.remove('rec');
  $('hint').textContent = HINT0;
  $('interim').textContent = '';
  $('draft').placeholder = T('remote.voice_draft_placeholder', '話した内容がここに溜まります。直してから送信できます。');
}
function startVoice() {
  // 使えない環境では死んだエラーを出さず、キーボード音声入力へ逃がす
  const reason = speechBlockReason();
  if (reason) { keyboardDictation(reason); return; }
  const C = speechAPI();
  voiceFatal = false;
  const r = new C();
  recog = r; active = true;
  r.lang = speechLang();
  r.continuous = true;
  r.interimResults = true;
  r.onresult = ev => {
    let fin = '', interim = '';
    for (let k = ev.resultIndex; k < ev.results.length; k++) {
      const t = ev.results[k][0].transcript;
      if (ev.results[k].isFinal) fin += t; else interim += t;
    }
    $('interim').textContent = interim;
    fin = fin.trim();
    if (fin) {
      // 確定した文は下書き欄へ足していくだけ。送信はボタンを押したときだけ
      $('interim').textContent = '';
      const d = $('draft');
      d.value = (d.value + (d.value && !d.value.endsWith(' ') ? ' ' : '') + fin).trim();
    }
  };
  r.onerror = ev => {
    const e = ev.error;
    if (e === 'no-speech') return;             // 無音だけ: onend の自動再開に任せる
    if (e === 'not-allowed' || e === 'service-not-allowed') {
      fatalVoiceStop(T('remote.voice_mic_not_allowed', 'マイクが許可されていません — アドレスバーのマイク設定を確認してください'));
    } else if (e === 'network') {
      // 認識サーバーへ到達できない = http 経由ではほぼ復帰しない。案内して終わる
      voiceFatal = true;
      stopVoice();
      keyboardDictation('network');
    } else if (e === 'audio-capture') {
      fatalVoiceStop(T('remote.mic_not_found', 'マイクが見つかりません'));
    } else if (e === 'aborted') {
      stopVoice();                             // 明示停止・画面遷移。黙って終わる
    }
  };
  r.onend = () => {
    // 無音で切れてもモードが ON の間は自動で再開する。
    // ただし致命的エラー後は再開しない (無反応のまま無限に回るのを防ぐ)
    if (voiceFatal) return;
    if (recog === r && active) { try { r.start(); } catch (e) { stopVoice(); } }
  };
  try { r.start(); } catch (e) { $('hint').textContent = T('remote.voice_recog_start_failed', '音声認識を開始できません'); stopVoice(); return; }
  $('mic').classList.add('rec');
  $('hint').textContent = T('remote.voice_listening', '\u{1F3A4} 認識中 — もう一度押すと停止します');
}
$('mic').onclick = () => (active ? stopVoice() : startVoice());
applyI18n();
poll();
setInterval(poll, 2500);
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;

    /// 「スマホから届いているのか分からない」を潰すための判定。
    /// PC 自身のアクセスを数えると、届いていないのに「届いた」と出てしまう。
    #[test]
    fn only_lan_peers_count_as_the_phone() {
        let p = |s: &str| s.parse::<std::net::SocketAddr>().expect("addr");
        assert!(
            counts_as_remote(&p("192.168.1.23:51000")),
            "同じ Wi-Fi のスマホ"
        );
        assert!(counts_as_remote(&p("10.0.0.5:51000")));
        assert!(
            !counts_as_remote(&p("127.0.0.1:51000")),
            "PC 自身は数えない"
        );
        assert!(
            !counts_as_remote(&p("[::1]:51000")),
            "IPv6 のループバックも同じ"
        );
    }

    #[test]
    fn a_fresh_server_has_not_been_reached_yet() {
        // 既定は「まだ届いていない」。ここが真になっていると、繋がっていないのに
        // 画面が「接続あり」と言い張る。
        let r = Reach::default();
        assert_eq!(r.hits, 0);
        assert!(r.last_ip.is_none());
        assert!(r.last_at.is_none());
    }

    #[test]
    fn token_is_16_hex_chars_and_unpredictable() {
        let t = gen_token();
        assert_eq!(t.len(), 16);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
        // OS 乱数由来なので毎回変わる (固定シードなら等しくなり検出できる)
        assert_ne!(gen_token(), gen_token());
    }

    /// トンネル使用時に `0.0.0.0` のままだと、SSH を迂回して LAN から平文で
    /// 直接叩けてしまう (= トンネルを張った意味が消える)。Tailscale も同じ理由で
    /// `0.0.0.0` にしない (喫茶店の Wi-Fi からポートが見えてしまう)。
    #[test]
    fn bind先はモードで切り替わる() {
        let ts: IpAddr = "100.101.102.103".parse().unwrap();
        assert_eq!(
            Bind::Lan.listen_ips(None).unwrap(),
            vec!["0.0.0.0".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            Bind::Loopback.listen_ips(None).unwrap(),
            vec!["127.0.0.1".parse::<IpAddr>().unwrap()]
        );
        assert_eq!(
            Bind::Tailscale.listen_ips(Some(ts)).unwrap(),
            vec!["127.0.0.1".parse::<IpAddr>().unwrap(), ts]
        );
        // tailnet の IP が引けないのに Tailscale で待ち受けようとしたら、
        // 黙って LAN へ落とさずに理由を返す (繋がらない QR を出さないため)
        assert!(Bind::Tailscale.listen_ips(None).is_err());
        // tailnet 以外を渡されても断る (LAN の IP に晒す取り違えを構造的に塞ぐ)
        for bad in ["192.168.1.3", "127.0.0.1", "10.0.0.5", "2001:db8::1"] {
            assert!(
                Bind::Tailscale
                    .listen_ips(Some(bad.parse().unwrap()))
                    .is_err(),
                "{bad} を tailnet として受け入れてはいけない"
            );
        }
        // tailnet の IPv6 は受け入れる
        assert!(Bind::Tailscale
            .listen_ips(Some("fd7a:115c:a1e0::1".parse().unwrap()))
            .is_ok());

        for b in [Bind::Lan, Bind::Loopback, Bind::Tailscale] {
            assert!(!b.label().is_empty(), "{b:?} の説明が無い");
        }
        assert!(
            Bind::Loopback.label().contains("127.0.0.1"),
            "どちらで待ち受けているかが UI から読めること"
        );
    }

    /// **どのモードでも `127.0.0.1` に届くこと。**
    /// `zai` CLI (`cli.rs` は 127.0.0.1:<port> へ繋ぐ) と PC 側の 🎤 音声ページ
    /// (`http://127.0.0.1:<port>/voice`) がそこに居る。ここを落とすと
    /// 「スマホからは繋がるのに PC の CLI と 🎤 だけが死ぬ」という、
    /// 触っている本人からは見えない壊れ方をする。
    #[test]
    fn どのモードでもloopbackに届く() {
        let ts: IpAddr = "100.101.102.103".parse().unwrap();
        for b in [Bind::Lan, Bind::Loopback, Bind::Tailscale] {
            let ips = b.listen_ips(Some(ts)).expect("解決できる");
            let reaches_loopback = ips.iter().any(|ip| ip.is_loopback() || ip.is_unspecified());
            assert!(reaches_loopback, "{b:?} が 127.0.0.1 を捨てている: {ips:?}");
        }
    }

    /// ワイルドカードで待ち受けている accept は `0.0.0.0` へ繋いでも起こせない
    /// (Windows は明確なエラー)。同じポートの loopback へ読み替える。
    #[test]
    fn 起こす宛先はワイルドカードを畳む() {
        let f = |s: &str| wake_addr(s.parse().unwrap()).to_string();
        assert_eq!(f("0.0.0.0:8899"), "127.0.0.1:8899");
        assert_eq!(f("[::]:8899"), "[::1]:8899");
        assert_eq!(f("127.0.0.1:8899"), "127.0.0.1:8899");
        assert_eq!(f("100.101.102.103:8900"), "100.101.102.103:8900");
    }

    /// URL のホスト部は IPv6 だけ角括弧で包む (RFC 3986)。
    /// 包み忘れると `http://fd7a:...:8899/` になり、ポートと区別が付かない。
    #[test]
    fn urlのホスト部はipv6を角括弧で包む() {
        assert_eq!(url_host("100.64.0.1".parse().unwrap()), "100.64.0.1");
        assert_eq!(
            url_host("fd7a:115c:a1e0::1".parse().unwrap()),
            "[fd7a:115c:a1e0::1]"
        );
    }

    /// 2 つのアドレスで待ち受けたとき、**両方が応答し、両方の accept が
    /// `Drop` で畳まれる**こと。1 本ずつ起こしていないと join が返らず、
    /// ここがそのまま固まる (タイムアウトで落ちる)。
    #[test]
    fn 複数アドレスで待ち受けても両方応答して畳める() {
        let v6 = std::net::Ipv6Addr::LOCALHOST;
        // IPv6 が無効な環境 (一部の Docker) では素直に降りる
        if TcpListener::bind((v6, 0)).is_err() {
            eprintln!("[skip] IPv6 loopback が使えないので飛ばす");
            return;
        }
        let ctx = egui::Context::default();
        let ips: Vec<IpAddr> = vec![IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), IpAddr::V6(v6)];
        // **8899-8919 を他のテストと取り合わない。** unix は SO_REUSEADDR で
        // `0.0.0.0:P` と `127.0.0.1:P` が同居でき、接続はより具体的な
        // `127.0.0.1` 側へ行く。ここが同じ番号の loopback を握ると、
        // 並列に走っている `張り直しても…` (0.0.0.0) の**起こし用接続を
        // 攫って**しまう (CI の macOS で実際に 60 秒打ち切りになった)。
        // 空き番号は OS に選ばせる。
        let free = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .expect("空きポートを 1 つ借りられる");
        let want = free.local_addr().expect("番号が取れる").port();
        drop(free);
        let srv = RemoteServer::start_on(ctx, Bind::Tailscale, &ips, None, Some(want))
            .expect("2 本で待ち受けられる");
        assert_eq!(srv.addrs.len(), 2, "2 本とも掴んでいること");
        let port = srv.port;
        for a in [
            SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
            SocketAddr::from((v6, port)),
        ] {
            let mut c = TcpStream::connect_timeout(&a, Duration::from_secs(2))
                .unwrap_or_else(|e| panic!("{a} へ繋がらない: {e}"));
            // トークン無しなので 401 でよい。**応答が返ること**が要件
            let _ = c.write_all(b"GET /api/state HTTP/1.1\r\nHost: x\r\n\r\n");
            let mut buf = [0u8; 16];
            let n = c.read(&mut buf).unwrap_or(0);
            assert!(n > 0, "{a} から応答が無い");
            assert!(
                String::from_utf8_lossy(&buf[..n]).starts_with("HTTP/1.1"),
                "{a} の応答が HTTP でない"
            );
        }
        // ここで固まらないこと (両方の accept を起こせているか)
        drop(srv);
    }

    /// 張り直しでトークンが変わると、既に QR を読んだスマホが一斉に 401 になる。
    /// URL のホスト部も待ち受けに合わせて変える (繋がらない URL を出さない)。
    #[test]
    fn 張り直してもトークンは変わらずurlのホストだけ変わる() {
        let ctx = egui::Context::default();
        let lan = RemoteServer::start(ctx.clone(), Bind::Lan).expect("LAN で起動できる");
        let token = lan.token.clone();
        let port = lan.port;
        assert_eq!(lan.bind, Bind::Lan);
        drop(lan); // Drop が accept を畳んでポートを解放するまで待つ

        let lo = RemoteServer::rebind(ctx, Bind::Loopback, token.clone(), port)
            .expect("loopback へ張り直せる");
        assert_eq!(lo.token, token, "トークンは引き継ぐ");
        assert_eq!(
            lo.port, port,
            "ポートも引き継ぐ (トンネルの転送先とずれない)"
        );
        assert_eq!(lo.bind, Bind::Loopback);
        // URL は待ち受けと必ず一致させる (繋がらない URL を QR にしない)
        assert_eq!(lo.url, format!("http://127.0.0.1:{}/", lo.port));
    }

    /// **起こし用の接続を別の待ち受けに攫われても、終了で固まらないこと。**
    ///
    /// unix は `SO_REUSEADDR` で `0.0.0.0:P` と `127.0.0.1:P` が同居でき、
    /// 接続はより具体的な `127.0.0.1` 側へ行く。Zaivern を 2 つ起動して
    /// 片方が SSH モード (127.0.0.1) だと、もう片方 (0.0.0.0) の `Drop` は
    /// **自分の accept を永久に起こせない**。以前はそこで join し続けて
    /// いたので、アプリの終了がそのまま固まった。
    #[test]
    fn 起こす接続を横取りされても終了で固まらない() {
        let ctx = egui::Context::default();
        let any: IpAddr = IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED);
        let srv = RemoteServer::start_on(ctx, Bind::Lan, &[any], None, None)
            .expect("0.0.0.0 で待ち受けられる");
        let port = srv.port;
        // 後から同じ番号の loopback を横から握る (= 別インスタンスの SSH モード)
        let Ok(thief) = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)) else {
            // Windows は同居できない = この事故自体が起こらない
            eprintln!("[skip] 127.0.0.1:{port} を同居させられないので飛ばす");
            return;
        };
        // 攫った接続は握りつぶす (起こしの合図を通さない)
        let stop = Arc::new(AtomicBool::new(false));
        let s2 = Arc::clone(&stop);
        let t = std::thread::spawn(move || {
            for c in thief.incoming() {
                if s2.load(Ordering::SeqCst) {
                    return;
                }
                drop(c);
            }
        });

        let t0 = Instant::now();
        drop(srv);
        let took = t0.elapsed();
        // 予算は 2 秒 (`WAKE_BUDGET`)。ここは速さではなく
        // **返ってくること**の検査なので、余裕を持って倍以上で見る
        assert!(
            took < WAKE_BUDGET * 5,
            "終了に {took:?} 掛かった (固まっている)"
        );

        stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect_timeout(
            &SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
            Duration::from_millis(500),
        );
        let _ = t.join();
    }

    /// **実測**: いまのこのマシンで `Bind::Tailscale` を起動してみる。
    ///
    /// tailnet が上がっていれば**本当に tailnet の IP で待ち受けられる**ことを、
    /// 上がっていなければ**理由の分かるエラーで断る**ことを確かめる。
    /// どちらの環境でも意味のある表明になるので、CI でも手元でも走らせられる。
    #[test]
    fn tailscaleモードは繋がっていれば待ち受け繋がっていなければ断る() {
        let ctx = egui::Context::default();
        let st = crate::tailscale::probe();
        match RemoteServer::start(ctx, Bind::Tailscale) {
            Ok(s) => {
                assert!(st.ready(), "検出は繋がっていないのに待ち受けられてしまった");
                let ip = st.ip.expect("ready なら IP がある");
                // URL のホストは tailnet の IP そのもの (loopback を出さない)
                assert_eq!(s.url, format!("http://{}:{}/", url_host(ip), s.port));
                // loopback も必ず握っていること (`zai` CLI と 🎤 の経路)
                assert!(
                    s.addrs.iter().any(|a| a.ip().is_loopback()),
                    "loopback を握っていない: {:?}",
                    s.addrs
                );
                assert!(
                    s.addrs.iter().any(|a| a.ip() == ip),
                    "tailnet の IP を握っていない: {:?}",
                    s.addrs
                );
                // 戻り道 (📶 同じ Wi-Fi に戻す) も同じ port / token のまま
                let (port, token) = (s.port, s.token.clone());
                drop(s);
                let back =
                    RemoteServer::rebind(egui::Context::default(), Bind::Lan, token.clone(), port)
                        .expect("Wi-Fi へ戻せる");
                assert_eq!(back.token, token, "戻しでトークンを変えない");
                // ポート番号は要求しない。**別インスタンスが `0.0.0.0:P` を
                // 握っていると、unix では `127.0.0.1:P` だけが取れてしまう**
                // ので (SO_REUSEADDR)、Tailscale で取れた番号が Lan では
                // 取れないことがある。番号の引き継ぎは
                // `張り直してもトークンは変わらずurlのホストだけ変わる` が
                // 他が居ない前提で単独に見る。
                assert!(
                    back.addrs.iter().all(|a| a.ip().is_unspecified()),
                    "Wi-Fi へ戻したら 0.0.0.0 だけ: {:?}",
                    back.addrs
                );
            }
            Err(e) => {
                assert!(
                    !st.ready(),
                    "検出は繋がっているのに待ち受けられなかった: {e}"
                );
                // 「空きポートがありません」で片付けず、Tailscale の話だと分かること
                assert!(
                    e.contains("Tailscale"),
                    "理由が Tailscale の話になっていない: {e}"
                );
            }
        }
    }

    #[test]
    fn subslice_finds_header_end() {
        assert_eq!(
            find_subslice(b"GET / HTTP/1.1\r\n\r\nbody", b"\r\n\r\n"),
            Some(14)
        );
        assert_eq!(find_subslice(b"abc", b"\r\n\r\n"), None);
    }

    /// 一括操作の宛先を表で固定する。
    ///
    /// 送信・停止・件数表示の 3 つがこの 1 本を通るので、ここがずれると
    /// 「3 体と出ているのに 5 体へ飛んだ」になる。
    #[test]
    fn 一括操作の宛先はモードで決まる() {
        let pick = |id, running, stalled, active| AgentPick {
            id,
            running,
            stalled,
            active,
        };
        // 1: 動いていて止まっている / 2: 動いていて働いている (選択中)
        // 3: 終了済み (止まっている扱いでも宛先にしない)
        let all = [
            pick(1, true, true, false),
            pick(2, true, false, true),
            pick(3, false, true, false),
        ];
        let table: &[(BulkMode, &[u64])] = &[
            (BulkMode::All, &[1, 2]),
            (BulkMode::Stalled, &[1]),
            (BulkMode::One, &[2]),
        ];
        for (mode, want) in table {
            assert_eq!(
                bulk_targets(*mode, &all),
                want.to_vec(),
                "mode={}",
                mode.as_str()
            );
        }
        // 終了済みしか居なければ、どのモードでも宛先はゼロ
        let dead = [pick(3, false, true, true)];
        for mode in [BulkMode::All, BulkMode::Stalled, BulkMode::One] {
            assert!(
                bulk_targets(mode, &dead).is_empty(),
                "終了済みを宛先に数えている: {}",
                mode.as_str()
            );
        }
        // 誰も居なければ空 (0 体表示 → 送信ボタンを塞ぐ側の入力になる)
        assert!(bulk_targets(BulkMode::All, &[]).is_empty());
    }

    /// 知らない宛先の語は**送らずに断る**。既定で「全員」へ落とすと、
    /// 綴りを 1 文字間違えただけで全エージェントへ誤爆する。
    #[test]
    fn 知らない宛先モードは受け付けない() {
        assert_eq!(BulkMode::parse("all"), Some(BulkMode::All));
        assert_eq!(BulkMode::parse("stalled"), Some(BulkMode::Stalled));
        assert_eq!(BulkMode::parse("one"), Some(BulkMode::One));
        for bad in ["", "ALL", "everyone", "全員", "al", "broadcast"] {
            assert_eq!(BulkMode::parse(bad), None, "{bad} を受けてしまった");
        }
        // as_str → parse は往復する (ページと API が同じ語を使う保証)
        for m in [BulkMode::All, BulkMode::Stalled, BulkMode::One] {
            assert_eq!(BulkMode::parse(m.as_str()), Some(m));
        }
    }

    /// 「待ち」一覧に載せるレーンを表で固定する。
    ///
    /// バッジ (`/api/state` の waiting) と一覧 (`/api/agents` の waiting) が
    /// この 1 本を共有するので、ここがずれると「バッジ 3 なのに一覧は 5 件」になる。
    #[test]
    fn 待ち一覧に載せるレーンを表で固定する() {
        use crate::kanban::Column;
        let table: &[(Column, bool)] = &[
            // 人の手が要る 2 本 + 指示待ちの 1 本だけ
            (Column::Ready, true),
            (Column::Approval, true),
            (Column::Trouble, true),
            // 動いているものは載せない (見ても何もすることが無い)
            (Column::Thinking, false),
            (Column::Editing, false),
            (Column::Running, false),
            (Column::Verifying, false),
            (Column::Done, false),
        ];
        for (col, want) in table {
            assert_eq!(is_waiting_lane(*col), *want, "{:?}", col);
        }
        // 表が 8 本すべてを覆っていること (レーンが増えたら気付く)
        assert_eq!(table.len(), crate::kanban::LANES);
    }

    /// 行の操作は知らない語を実行しない (押していない承認を飛ばさない)。
    #[test]
    fn 知らない行操作は受け付けない() {
        assert_eq!(AgentAct::parse("approve"), Some(AgentAct::Approve));
        assert_eq!(AgentAct::parse("stop"), Some(AgentAct::Stop));
        for bad in ["", "Approve", "yes", "kill", "承認"] {
            assert_eq!(AgentAct::parse(bad), None, "{bad} を受けてしまった");
        }
    }

    /// スマホから「待ち一覧 / デッキ / 看板」へ届くこと。
    ///
    /// UI から到達できない実装は未完成なので、入口 (セグメント) と
    /// 取得先 (API) の両方が埋め込みページに居ることを固定する。
    #[test]
    fn page_contains_agent_views() {
        assert!(PAGE.contains("/api/agents"), "一覧の取得先が無い");
        assert!(PAGE.contains("/api/agent_act"), "行内の操作が無い");
        assert!(PAGE.contains("id=\"aseg\""), "ビュー切替が無い");
        assert!(PAGE.contains("id=\"alist\""), "一覧の入れ物が無い");
        for v in ["'term'", "'wait'", "'deck'", "'kanban'"] {
            assert!(PAGE.contains(v), "ビュー {v} が無い");
        }
        // 下部ナビは増やさない (エディタ/ファイル/エージェント/コマンドの 4 つのまま)
        assert_eq!(
            PAGE.matches("data-v=\"").count(),
            4,
            "下部ナビのタブが増えている"
        );
        // 看板のレーンはサーバ (kanban.rs) から来たものを出す。
        // スマホ側でレーン名やしきい値を作り直していないこと
        assert!(
            PAGE.contains("alanes.forEach"),
            "レーンをサーバから出していない"
        );
        for ng in ["承認待ち", "停滞・異常", "思考中", "検証中"] {
            assert!(
                !PAGE.contains(ng),
                "レーン名 {ng} をページ側で作り直している"
            );
        }
        // 空状態は利用可能領域の中央に 1 枚のカードで出す
        assert!(PAGE.contains("mid-card"));
        assert!(PAGE.contains("classList.add('mid')"));
        // 一覧を見ている間は /api/term を叩かない (ポーリングを増やさない)
        assert!(PAGE.contains("if (aview === 'term') {"));
    }

    /// スマホから一括操作へ届けること。**件数を見せずに送らせない**。
    #[test]
    fn page_contains_bulk_actions() {
        assert!(PAGE.contains("/api/bulk"), "一括送信の入口が無い");
        assert!(PAGE.contains("/api/bulk_stop"), "一括停止の入口が無い");
        // 「全員 / 待機 / 選択中」の 3 粒度
        for m in ["'all'", "'stalled'", "'one'"] {
            assert!(PAGE.contains(m), "宛先モード {m} が無い");
        }
        // 既定はいちばん狭い宛先 (開いた瞬間に全員宛てだと誤爆する)
        assert!(
            PAGE.contains("let bulkMode = 'one';"),
            "既定が 1 体宛てでない"
        );
        // 送信前に件数を見せ、0 体なら押せないこと
        assert!(PAGE.contains("bulkCount()"));
        assert!(PAGE.contains("$('tsend').disabled = n === 0;"));
        assert!(PAGE.contains("$('tput').disabled = n === 0;"));
        // 件数は PC が数えた値をそのまま出す (スマホ側で数え直さない)
        assert!(PAGE.contains("state.bulk"), "件数を PC 側から取っていない");
    }

    #[test]
    fn page_contains_required_parts() {
        // 埋め込みページが最低限の構造を持つこと (生文字列の破損検知)
        assert!(PAGE.contains("<!DOCTYPE html>"));
        assert!(PAGE.contains("/api/state"));
        assert!(PAGE.contains("/api/term"));
        assert!(PAGE.contains("</html>"));
        // JS 側のエスケープが実制御文字に化けていないこと
        assert!(PAGE.contains("\\u001b"));
        assert!(!PAGE.contains('\u{1b}'));
    }

    /// スマホからも「このファイルが何で保存されるか」が見えること。
    /// UTF-8 以外のファイルを開いたまま保存すると、表せない文字があったときだけ
    /// UTF-8 へ切り替わる — 黙って変わると他ツールが読めなくなる原因が
    /// スマホ側から分からなくなるので、必ず知らせる作りであることを固定する。
    #[test]
    fn page_shows_the_file_encoding() {
        assert!(PAGE.contains("f.encoding"), "文字コードを表示していない");
        assert!(
            PAGE.contains("r.promoted"),
            "UTF-8 への格上げを伝えていない"
        );
        assert!(PAGE.contains("UTF-8 で保存しました"));
    }

    #[test]
    fn page_contains_voice_input_parts() {
        // エージェント毎の音声入力モード (Web Speech API) が組み込まれていること
        assert!(PAGE.contains("webkitSpeechRecognition"));
        assert!(PAGE.contains("音声入力モード"));
        assert!(PAGE.contains("startVoice"));
        assert!(PAGE.contains("stopVoice"));
        assert!(PAGE.contains("chip mic"));
    }

    #[test]
    fn pages_never_auto_send() {
        // 話しただけで送信されないこと: 送信はボタン経由の関数だけが行う。
        // 認識結果ハンドラから直接 API を叩く実装に戻したら気付けるようにする。
        assert!(PAGE.contains("sendInput"));
        assert!(!PAGE.contains("sendVoice"));
        assert!(PAGE.contains("入れる"));
        assert!(VOICE_PAGE.contains("id=\"draft\""));
        assert!(VOICE_PAGE.contains("send(true)"));
        assert!(VOICE_PAGE.contains("send(false)"));
        assert!(VOICE_PAGE.contains("submit: submit"));
    }

    #[test]
    fn voice_page_contains_required_parts() {
        // PC 用音声入力ページ (生文字列の破損検知)
        assert!(VOICE_PAGE.contains("<!DOCTYPE html>"));
        assert!(VOICE_PAGE.contains("webkitSpeechRecognition"));
        assert!(VOICE_PAGE.contains("/api/voice"));
        assert!(VOICE_PAGE.contains("/api/state"));
        assert!(VOICE_PAGE.contains("全エージェントへブロードキャスト"));
        assert!(VOICE_PAGE.contains("入力欄へ入れる"));
        assert!(VOICE_PAGE.contains("</html>"));
        // 実制御文字が紛れ込んでいないこと
        assert!(!VOICE_PAGE.contains('\u{1b}'));
    }

    #[test]
    fn pages_detect_insecure_context() {
        // http (LAN の IP) では Web Speech API が動かない。両ページとも
        // 事前判定して、黙って壊れるのではなく理由を出すこと
        for p in [PAGE, VOICE_PAGE] {
            assert!(p.contains("isSecureContext"));
            assert!(p.contains("speechBlockReason"));
            assert!(p.contains("'insecure'"));
            assert!(p.contains("'unsupported'"));
        }
    }

    #[test]
    fn pages_guide_to_keyboard_dictation() {
        // 使えない端末では OS キーボードの音声入力へ逃がす。
        // 案内文は「原因」と「次にすること」の両方を日本語で書くこと
        for p in [PAGE, VOICE_PAGE] {
            assert!(p.contains("keyboardDictation"));
            assert!(p.contains("dictationHint"));
            // 次にすること (キーボードの音声入力を使う)
            assert!(p.contains("を押して、入力欄に話しかけてください"));
            // 原因
            assert!(p.contains("この接続 (http) ではブラウザの音声認識が使えません。"));
            assert!(p.contains("このブラウザは音声認識 (Web Speech API) に未対応です。"));
            // PC 側の案内は実ポートを埋める (8899 決め打ちにしない)
            assert!(p.contains("location.port"));
            assert!(p.contains("'/voice'"));
        }
    }

    #[test]
    fn pages_handle_network_error_without_restart_loop() {
        // network は http 経由では復帰しない。再開させず案内に切り替えること。
        // no-speech (無音) だけは従来どおり onend で再開してよい
        for p in [PAGE, VOICE_PAGE] {
            assert!(p.contains("e === 'network'"));
            assert!(p.contains("keyboardDictation"));
            assert!(p.contains("if (e === 'no-speech') return;"));
        }
    }

    #[test]
    fn fatal_voice_error_does_not_auto_restart() {
        // 致命的エラー後に onend が再開すると、画面上は無反応のまま無限に回る。
        // voiceFatal ガードで止まっていること
        for p in [PAGE, VOICE_PAGE] {
            assert!(p.contains("voiceFatal"));
            assert!(p.contains("if (voiceFatal) return;"));
            assert!(p.contains("voiceFatal = true"));
            // 再開のたびにガードを解除していること (一度きりで死なない)
            assert!(p.contains("voiceFatal = false"));
        }
    }

    // ─── 多言語化 (Language Pack) ───────────────────────────────────

    /// 属性値 (`data-i18n="…"`) を全部集める。**正規表現は使わない**
    /// (この 1 個のために依存を増やさない・見て分かる形にする)。
    fn attr_values<'a>(src: &'a str, needle: &str) -> Vec<&'a str> {
        let mut out = Vec::new();
        let mut rest = src;
        while let Some(i) = rest.find(needle) {
            let after = &rest[i + needle.len()..];
            match after.find('"') {
                Some(e) => out.push(&after[..e]),
                None => break,
            }
            rest = after;
        }
        out
    }

    /// `T('…'` の第 1 引数を集める。直前が識別子の一部なら別物 (`setTimeout(` 等)
    /// なので飛ばす。
    fn t_call_keys(src: &str) -> Vec<&str> {
        let b = src.as_bytes();
        let mut out = Vec::new();
        let mut i = 0;
        while let Some(p) = src[i..].find("T('") {
            let at = i + p;
            let prev = if at == 0 { b' ' } else { b[at - 1] };
            let part_of_ident =
                prev.is_ascii_alphanumeric() || prev == b'_' || prev == b'$' || prev == b'.';
            if !part_of_ident {
                if let Some(e) = src[at + 3..].find('\'') {
                    out.push(&src[at + 3..at + 3 + e]);
                }
            }
            i = at + 3;
        }
        out
    }

    /// 画面に出す文言の ID は必ず `remote.` 接頭辞を持つ。
    /// (`i18n::export_prefix("remote.")` で書き出す辞書に入らない ID を
    /// 書いてしまうと、その文言だけ**永久に翻訳されない**まま静かに残る)
    #[test]
    fn 画面文言のidはremoteで始まる() {
        for p in [PAGE, VOICE_PAGE] {
            let mut ids: Vec<&str> = Vec::new();
            for n in ["data-i18n=\"", "data-i18n-ph=\"", "data-i18n-title=\""] {
                ids.extend(attr_values(p, n));
            }
            assert!(!ids.is_empty(), "data-i18n が 1 つも無い");
            for id in &ids {
                assert!(id.starts_with("remote."), "接頭辞が違う: {id}");
            }
            let keys = t_call_keys(p);
            assert!(
                keys.len() >= 10,
                "T() の呼び出しが少なすぎる: {}",
                keys.len()
            );
            for k in &keys {
                assert!(k.starts_with("remote."), "接頭辞が違う: {k}");
            }
        }
    }

    /// フォールバック (第 2 引数) を書き忘れると、辞書が届かない言語で
    /// **空文字のボタン**が出る。全ての `T(` が 2 引数であることまでは
    /// 構文解析なしに見られないので、少なくとも空の既定値が無いことを見る。
    #[test]
    fn t呼び出しに空のフォールバックが無い() {
        for p in [PAGE, VOICE_PAGE] {
            assert!(!p.contains("', '')"), "フォールバックが空の T( がある");
        }
    }

    #[test]
    fn localize_pageは言語idをlang属性へ入れる() {
        let out = localize_page(PAGE, "{}", "zh-CN");
        assert!(out.contains("<html lang=\"zh-CN\">"));
        assert!(!out.contains("<html lang=\"ja\">"));
        // 言語 ID は利用者の locales 由来。属性を抜け出せないこと
        let bad = localize_page(PAGE, "{}", "ja\"><script>x()</script>");
        assert!(bad.contains("<html lang=\"jascriptxscript\">"));
        assert!(!bad.contains("<script>x()"));
        // 空 (あり得ないが) でも属性は壊れない
        assert!(localize_page(PAGE, "{}", "").contains("<html lang=\"ja\">"));
    }

    #[test]
    fn localize_pageは辞書を注入する() {
        // 差し込み口はページごとにちょうど 1 つ
        assert_eq!(PAGE.matches(I18N_SLOT).count(), 1);
        assert_eq!(VOICE_PAGE.matches(I18N_SLOT).count(), 1);
        let out = localize_page(PAGE, r#"{"remote.save":"Save"}"#, "en");
        assert!(out.contains(r#"window.ZVI18N = {"remote.save":"Save"};"#));
        assert!(!out.contains(I18N_SLOT), "差し込み口が残っている");
        // JS 側の取り出し口も残っていること
        assert!(out.contains("window.ZVI18N && window.ZVI18N[k]"));
    }

    /// 訳文に `</script>` が入っていても、そこでスクリプトが終わらないこと。
    /// ブラウザは**文字列の途中でも** `</script>` を終端として扱うので、
    /// エスケープを外すと任意のタグを注入できてしまう。
    #[test]
    fn 辞書のscript終端はページを壊さない() {
        let mut m = std::collections::BTreeMap::new();
        m.insert(
            "remote.save".to_string(),
            "</script><img src=x onerror=alert(1)>".to_string(),
        );
        let json = serde_json::to_string(&m).unwrap();
        let out = localize_page(PAGE, &json, "en");
        assert!(!out.contains("</script><img"), "script を閉じられている");
        assert!(out.contains("\\u003c/script>"), "< が退避されていない");
        // <script> の数が増減していない = 構造が保たれている
        assert_eq!(
            out.matches("</script>").count(),
            PAGE.matches("</script>").count()
        );
    }

    #[test]
    fn 壊れた辞書は空オブジェクトになる() {
        // JSON でない・オブジェクトでない入力で JS 構文まで壊さない
        for bad in ["", "not json", "[1,2]", "\"str\"", "null", "{"] {
            let out = localize_page(PAGE, bad, "ja");
            assert!(out.contains("window.ZVI18N = {};"), "入力={bad:?}");
        }
    }

    /// 実際に配る経路 (i18n の辞書 → JSON → 差し込み) が繋がっていること。
    /// HTTP は触らないので、サーバを起こさずに確かめられる。
    #[test]
    fn 同梱の全言語でスマホ画面の文言が入る() {
        // 「PC で言語を変えたらスマホもその言語になる」の**中身**を見る。
        // 仕組み (差し込み口があること) だけでなく、**同梱 6 言語ぶんの実際の
        // 訳がページへ入ること**まで固定する。グローバル状態は触らない
        // (並列に走る他のテストの tr() を揺らさないため、辞書は直に読む)。
        for (id, _, _) in crate::locale::BUILTIN {
            let mut errs = Vec::new();
            let map = crate::locale::resolved(id, &[], &mut errs);
            assert!(errs.is_empty(), "{id}: {errs:?}");
            let dict: std::collections::BTreeMap<&String, &String> = map
                .iter()
                .filter(|(k, _)| k.starts_with("remote."))
                .collect();
            assert!(
                dict.len() >= 50,
                "{id}: remote.* が {} 件しかない",
                dict.len()
            );
            let json = serde_json::to_string(&dict).expect("json");
            let out = localize_page(PAGE, &json, id);

            assert!(
                out.contains(&format!("<html lang=\"{id}\">")),
                "{id}: lang 属性"
            );
            // 代表的な 3 つの文言が、その言語の綴りで入っていること
            for key in ["remote.save", "remote.tab_agent", "remote.send"] {
                let Some(want) = map.get(key) else {
                    panic!("{id}: {key} が辞書に無い");
                };
                let esc = serde_json::to_string(want).expect("json");
                let esc = esc.trim_matches('"');
                assert!(
                    out.contains(esc),
                    "{id}: {key} の訳 {want:?} がページに入っていない"
                );
            }
        }
    }

    #[test]
    fn 配信するページには辞書と言語が入っている() {
        for p in [PAGE, VOICE_PAGE] {
            let out = page_for_client(p);
            assert!(out.contains("window.ZVI18N = {"));
            assert!(!out.contains(I18N_SLOT));
            assert!(out.contains("<html lang=\""));
            assert!(out.contains("</html>"));
        }
    }

    // ─── Query の純粋ロジック ────────────────────────────────────────

    /// 全 variant を代表値で 1 つずつ構築する (網羅テスト用)。
    fn all_query_variants() -> Vec<Query> {
        vec![
            Query::State,
            Query::File,
            Query::Files,
            Query::SetText {
                text: "abc".into(),
                index: 0,
                save: false,
            },
            Query::Cmd("save".into(), 1),
            Query::OpenFile("src/main.rs".into(), Some(10)),
            Query::Notify("hello".into(), "info".into()),
            Query::SetPanel {
                plugin: "p".into(),
                panel: "out".into(),
                text: "t".into(),
            },
            Query::SetStatus("busy".into()),
            Query::Prompt {
                text: "fix it".into(),
                agent: String::new(),
                submit: false,
            },
            Query::Tab(2),
            Query::Term,
            Query::TermInput("ls".into(), false),
            Query::VoiceSend {
                text: "音声".into(),
                id: -1,
                submit: false,
            },
            Query::Agents,
            Query::AgentAct {
                id: 7,
                act: AgentAct::Approve,
            },
            Query::Bulk {
                text: "まとめて".into(),
                mode: BulkMode::All,
                submit: true,
            },
            Query::BulkStop {
                mode: BulkMode::Stalled,
            },
        ]
    }

    #[test]
    fn fire_and_forget_classification_covers_every_variant() {
        // 期待値をワイルドカード無しの match で書く: variant を追加すると
        // ここがコンパイルエラーになり、分類の見直しを強制できる
        for q in all_query_variants() {
            let expected = match &q {
                // 現在の状態を読む要求 (+ 状態を返す必要がある操作) は応答を待つ
                Query::State
                | Query::File
                | Query::Files
                | Query::SetText { .. }
                | Query::Tab(..)
                | Query::Term
                | Query::Agents
                | Query::VoiceSend { .. }
                // 一括操作は**何体へ届いたか**を返して誤爆を見せるので待つ
                | Query::Bulk { .. }
                | Query::BulkStop { .. }
                // 承認は「効いたか」が返らないと、押したのに止まったままか
                // 分からない (press_pet_approve_button は失敗しうる)
                | Query::AgentAct { .. } => false,
                // 一方向の指示はキューに積んだ時点で成功 (macOS 凍結対策)
                Query::Notify(..)
                | Query::SetPanel { .. }
                | Query::SetStatus(..)
                | Query::Prompt { .. }
                | Query::OpenFile(..)
                | Query::Cmd(..)
                | Query::TermInput(..) => true,
            };
            assert_eq!(q.is_fire_and_forget(), expected);
        }
    }

    #[test]
    fn state_reading_queries_wait_for_reply() {
        // State/File/Files/Term は実際の値が必要なので即答してはいけない
        assert!(!Query::State.is_fire_and_forget());
        assert!(!Query::File.is_fire_and_forget());
        assert!(!Query::Files.is_fire_and_forget());
        assert!(!Query::Term.is_fire_and_forget());
        assert!(!Query::Tab(0).is_fire_and_forget());
    }

    #[test]
    fn one_way_commands_are_fire_and_forget() {
        assert!(Query::Notify("n".into(), "warn".into()).is_fire_and_forget());
        assert!(Query::SetStatus(String::new()).is_fire_and_forget());
        assert!(Query::Cmd("build".into(), 0).is_fire_and_forget());
        assert!(Query::SetPanel {
            plugin: String::new(),
            panel: "log".into(),
            text: "x".into(),
        }
        .is_fire_and_forget());
    }

    #[test]
    fn set_text_always_waits_even_with_save() {
        // タブ不一致なら拒否される (誤上書き防止) ので、結果を返す必要がある
        for save in [false, true] {
            let q = Query::SetText {
                text: "body".into(),
                index: 1,
                save,
            };
            assert!(!q.is_fire_and_forget());
        }
    }

    #[test]
    fn voice_send_always_waits() {
        // 挿入のみ / 送信あり / ブロードキャストのどれでも応答を待つ
        for (id, submit) in [(0i64, false), (3, true), (-1, false), (-1, true)] {
            let q = Query::VoiceSend {
                text: "テスト".into(),
                id,
                submit,
            };
            assert!(!q.is_fire_and_forget());
        }
    }

    #[test]
    fn prompt_is_fire_and_forget_regardless_of_flags() {
        // agent 指定や submit の有無で分類が変わらないこと
        for (agent, submit) in [("", false), ("", true), ("claude", false), ("claude", true)] {
            let q = Query::Prompt {
                text: "p".into(),
                agent: agent.into(),
                submit,
            };
            assert!(q.is_fire_and_forget());
        }
    }

    #[test]
    fn term_input_is_fire_and_forget_for_text_and_raw() {
        // テキスト+Enter (raw=false) も制御バイト列 (raw=true) も片道
        assert!(Query::TermInput("echo hi".into(), false).is_fire_and_forget());
        assert!(Query::TermInput("\x03".into(), true).is_fire_and_forget());
    }

    #[test]
    fn open_file_is_fire_and_forget_with_and_without_line() {
        assert!(Query::OpenFile("a.rs".into(), None).is_fire_and_forget());
        assert!(Query::OpenFile("a.rs".into(), Some(42)).is_fire_and_forget());
    }

    #[test]
    fn ack_is_fixed_queued_json_for_all_variants() {
        // ack は variant によらず固定の JSON (スマホ側 JS が queued を見る)
        for q in all_query_variants() {
            assert_eq!(q.ack(), r#"{"ok":true,"queued":true}"#);
        }
    }
}
