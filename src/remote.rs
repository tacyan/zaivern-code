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

/// 承認キューの 1 件に対して撃てる操作。
///
/// **知らない語は `None`** — 綴り違いを黙って「承認」に落とすと、
/// 押していない承認が飛ぶ ([`AgentAct::parse`] と同じ立場)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApproveAct {
    /// この 1 件だけ承認する
    Approve,
    /// この 1 件だけ拒否する
    Deny,
    /// 以後この種別 × このエージェントを常に許可する
    Always,
    /// 以後この種別 × このエージェントを常に拒否する
    AlwaysDeny,
}

impl ApproveAct {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" => Some(Self::Approve),
            "deny" => Some(Self::Deny),
            "always" => Some(Self::Always),
            "always_deny" => Some(Self::AlwaysDeny),
            _ => None,
        }
    }
}

/// UI スレッドへ渡す問い合わせの種類。
///
/// `Clone` なのは、**git を UI スレッドで待たない**ための聞き直しに要るため
/// ([`Query::retries_while_pending`])。
#[derive(Clone)]
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

    // ─── ここから下はスマホの「PC と同じことができる」ための読み取り ───
    //
    // **git と横断検索は UI スレッドで走らせない** (CLAUDE.md の鉄則:
    // `git status` は単独 0.03 秒でも同時実行で 2.3〜10.2 秒かかる)。
    // UI 側は控えを読むだけで、控えが無ければ `pending` と即答する。
    // 実際に待つのは接続ごとのスレッド ([`Query::retries_while_pending`])。
    /// 未コミットの変更をファイル単位で返す (PC の `open_changes_multibuffer`
    /// と同じ入口 = `git::working_tree_diff` + `diff::parse_unified`)。
    Changes,
    /// 1 ファイルぶんのハンク。`rel` はルート相対 ([`safe_rel`] 済み)。
    Diff { rel: String },
    /// 端末の履歴を**色つき**で返す。`agent` が負ならアクティブなセッション。
    /// `before` は「この絶対行より前」(None なら末尾)。
    Scrollback {
        agent: i64,
        lines: usize,
        before: Option<usize>,
    },
    /// 承認キューの中身 (種別・根拠行・待ち時間)。
    Approvals,
    /// 承認キューの 1 件を決着させる。
    Approve { id: u64, act: ApproveAct },
    /// ファイルを**開かずに**読む (PC のアクティブタブを奪わない)。
    Read {
        rel: String,
        from: usize,
        lines: usize,
    },
    /// ワークスペース横断検索 (`file_search` へ合流)。
    Search { q: String, max: usize },
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

    /// 応答が `"pending": true` のとき、**聞き直してよい**要求か。
    ///
    /// git と横断検索は UI スレッドで待てない (CLAUDE.md: 同時実行で
    /// 2.3〜10.2 秒かかり、その間フレームが止まる)。そこで UI 側は
    /// 「まだ用意できていない」と**即答**し、裏のスレッドが結果を作る。
    /// 待つのは接続ごとのこのスレッドなので、**UI は 1 フレームも止まらない**
    /// のに、スマホから見れば 1 回の GET で答えが返る。
    fn retries_while_pending(&self) -> bool {
        matches!(
            self,
            Query::Changes | Query::Diff { .. } | Query::Search { .. }
        )
    }
}

/// 応答 JSON が「まだ用意できていない」と言っているか (**純関数**)。
///
/// 読めない JSON は `false` — 聞き直しの輪に入れて塞ぐより、
/// そのままスマホへ返して見せるほうが原因が分かる。
pub fn is_pending(json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .and_then(|v| v.get("pending").and_then(|p| p.as_bool()))
        .unwrap_or(false)
}

/// `pending` のときに聞き直す間隔。短すぎると UI スレッドを無駄に起こし、
/// 長すぎるとスマホの体感が悪くなる。
const PENDING_POLL: Duration = Duration::from_millis(120);

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
        // ─── ここから下はスマホを PC と同じ土俵に載せるための読み取り ───
        // GET のクエリは `?t=` と同じ文字列から取り出す (認証は上で済んでいる)。
        ("GET", "/api/changes") => Query::Changes,
        ("GET", "/api/diff") => {
            // パスは必ずルート相対へ畳む。畳めない綴りは**実行前に**断る
            let Some(rel) = query_param(&query_str, "path")
                .as_deref()
                .and_then(safe_rel)
            else {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"bad path"}"#,
                );
            };
            Query::Diff { rel }
        }
        ("GET", "/api/scrollback") => Query::Scrollback {
            agent: query_param(&query_str, "agent")
                .and_then(|v| v.parse::<i64>().ok())
                .unwrap_or(-1),
            lines: clamp_count(
                query_param(&query_str, "lines")
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0),
                SCROLLBACK_DEFAULT,
                SCROLLBACK_MAX,
            ),
            // 0 は「先頭より前」= 空。無指定 (末尾) と区別する
            before: query_param(&query_str, "before")
                .and_then(|v| v.parse::<i64>().ok())
                .filter(|v| *v >= 0)
                .map(|v| v as usize),
        },
        ("GET", "/api/approvals") => Query::Approvals,
        // 承認は**知らない語を絶対に通さない** (押していない承認が飛ぶ)
        ("POST", "/api/approve") => {
            let Some(act) = ApproveAct::parse(&s("act")) else {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"unknown act (approve|deny|always|always_deny)"}"#,
                );
            };
            // id は文字列でも数値でも受ける (JS の JSON は数値に落としがち)
            let id = json
                .get("id")
                .and_then(|v| {
                    v.as_u64()
                        .or_else(|| v.as_str().and_then(|t| t.trim().parse::<u64>().ok()))
                })
                .unwrap_or(u64::MAX);
            Query::Approve { id, act }
        }
        ("GET", "/api/read") => {
            let Some(rel) = query_param(&query_str, "path")
                .as_deref()
                .and_then(safe_rel)
            else {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"bad path"}"#,
                );
            };
            Query::Read {
                rel,
                from: query_param(&query_str, "from")
                    .and_then(|v| v.parse::<i64>().ok())
                    .filter(|v| *v > 0)
                    .unwrap_or(1) as usize,
                lines: clamp_count(
                    query_param(&query_str, "lines")
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0),
                    READ_DEFAULT,
                    READ_MAX,
                ),
            }
        }
        ("GET", "/api/search") => {
            // 1 文字の検索は索引全体を舐めるだけで役に立たない。断る
            let q = query_param(&query_str, "q").unwrap_or_default();
            if q.trim().chars().count() < SEARCH_MIN_CHARS {
                return respond(
                    &mut stream,
                    400,
                    "application/json",
                    br#"{"ok":false,"error":"query too short (2+ chars)"}"#,
                );
            }
            Query::Search {
                q: q.trim().to_string(),
                max: clamp_count(
                    query_param(&query_str, "max")
                        .and_then(|v| v.parse::<i64>().ok())
                        .unwrap_or(0),
                    SEARCH_DEFAULT,
                    SEARCH_MAX,
                ),
            }
        }
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
    let immediate = query.is_fire_and_forget().then(|| query.ack());
    let retry_pending = query.retries_while_pending();
    let deadline = Instant::now() + REMOTE_TIMEOUT;
    let closed = |stream: &mut TcpStream| {
        respond(
            stream,
            500,
            "application/json",
            br#"{"ok":false,"error":"app closed"}"#,
        )
    };
    let Ok(mut reply) = send_query(&tx, &ctx, query.clone(), immediate, deadline) else {
        return closed(&mut stream);
    };
    // **git と横断検索を UI スレッドで待たない**ための聞き直し (CLAUDE.md)。
    // UI 側は控えが無ければ `pending` と即答し、裏のスレッドが結果を作る。
    // 待つのはこのスレッドだけなので、フレームは 1 度も止まらない。
    while retry_pending && reply.as_deref().is_some_and(is_pending) && Instant::now() < deadline {
        std::thread::sleep(PENDING_POLL);
        let Ok(next) = send_query(&tx, &ctx, query.clone(), immediate, deadline) else {
            return closed(&mut stream);
        };
        reply = next;
    }
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

/// 要求を 1 件 UI スレッドへ渡して応答を受け取る。
///
/// `immediate` が `Some` なら**積んだ時点で成功**として即座に返す
/// (一方向の指示。macOS はウィンドウが背面だとイベントループごと凍結する)。
/// `Err(())` はアプリが閉じたことだけを意味する。
fn send_query(
    tx: &mpsc::Sender<Request>,
    ctx: &egui::Context,
    query: Query,
    immediate: Option<&'static str>,
    deadline: Instant,
) -> Result<Option<String>, ()> {
    let (rtx, rrx) = mpsc::sync_channel::<String>(1);
    if tx.send(Request { query, reply: rtx }).is_err() {
        return Err(());
    }
    if let Some(js) = immediate {
        crate::perf::repaint(ctx, "remote");
        return Ok(Some(js.to_string()));
    }
    // UI スレッドは次のフレームでしか応答できない。ウィンドウが背面や
    // 非表示だとフレームが来る間隔が延びるため、1 回だけ起こして待つと
    // 取りこぼす。応答が返るまで一定間隔で起こし続ける。
    Ok(loop {
        crate::perf::repaint(ctx, "remote");
        match rrx.recv_timeout(Duration::from_millis(150)) {
            Ok(js) => break Some(js),
            Err(mpsc::RecvTimeoutError::Disconnected) => break None,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                if Instant::now() >= deadline {
                    break None;
                }
            }
        }
    })
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

// ══════════════════════════════════════════════════════════════════════
//  上限 — **どれも黙って切らない**。切ったら必ず `truncated` で伝える
//  (切られたことを知らせない一覧は、無いのと同じくらい危ない)。
// ══════════════════════════════════════════════════════════════════════

/// `/api/scrollback` の既定行数と上限 (契約: 1〜2000)。
pub const SCROLLBACK_DEFAULT: usize = 200;
pub const SCROLLBACK_MAX: usize = 2000;
/// `/api/read` の既定行数と上限 (契約: 1〜2000)。
pub const READ_DEFAULT: usize = 400;
pub const READ_MAX: usize = 2000;
/// `/api/search` の検索語の最小文字数 / 既定件数 / 上限。
pub const SEARCH_MIN_CHARS: usize = 2;
pub const SEARCH_DEFAULT: usize = 100;
pub const SEARCH_MAX: usize = 500;
/// `/api/changes` が返すファイル数の上限。
pub const CHANGES_CAP: usize = 500;
/// `/api/diff` が返すハンク数の上限。
pub const DIFF_HUNK_CAP: usize = 300;
/// 追跡外ファイルを「全部追加された差分」として読むときの 1 ファイル上限。
pub const UNTRACKED_READ_CAP: u64 = 256 * 1024;

// ══════════════════════════════════════════════════════════════════════
//  GET のクエリ文字列 / パスの正規化 (すべて純関数)
//
//  スマホから届く文字列は**全部疑う**。パスは必ずルート配下へ畳んでから
//  使い、畳めないものは受け取らない (fail-closed)。
// ══════════════════════════════════════════════════════════════════════

/// `%XX` と `+` を解いた文字列。壊れた `%` はそのまま残す
/// (弾いてしまうと `100%` のような素直な検索語が通らなくなる)。
pub fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < b.len() => {
                let hex = |c: u8| (c as char).to_digit(16);
                match (hex(b[i + 1]), hex(b[i + 2])) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(b'%');
                        i += 1;
                    }
                }
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// クエリ文字列から 1 つのキーの値を取り出す (`%XX` 復号つき)。
/// 同じキーが複数あれば**最初の 1 つ**。
pub fn query_param(qs: &str, key: &str) -> Option<String> {
    qs.split('&').find_map(|kv| {
        let (k, v) = kv.split_once('=')?;
        (k == key).then(|| percent_decode(v))
    })
}

/// スマホから届いた相対パスを、**ルート配下に必ず収まる形**へ畳む。
///
/// 弾くもの (どれも `None`):
/// * 空、`..` を 1 つでも含む
/// * 絶対パス (`/a`、`C:\a`、`\\server\share`)
/// * NUL を含む (OS 呼び出しの終端に化ける)
///
/// 区切りは `/` へ寄せ、`.` と空の要素は落とす。**返り値は必ず相対**なので、
/// 呼び出し側は `root.join()` してよい (それでも実体の前方一致は別途見る)。
pub fn safe_rel(raw: &str) -> Option<String> {
    if raw.is_empty() || raw.contains('\0') {
        return None;
    }
    let unified = raw.replace('\\', "/");
    if unified.starts_with('/') {
        return None;
    }
    // Windows のドライブ指定 (`C:` / `c:/x`) とドライブ相対 (`C:x`) の両方を弾く
    let mut ch = unified.chars();
    if let (Some(c0), Some(':')) = (ch.next(), ch.next()) {
        if c0.is_ascii_alphabetic() {
            return None;
        }
    }
    let mut parts: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => continue,
            ".." => return None,
            s => parts.push(s),
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// 件数の指定を「無指定なら既定・範囲外なら丸める」で受ける (**純関数**)。
///
/// 0 や負数を「無指定」として扱うのは、`?lines=` を空で送ってくる素朴な
/// クライアントが必ず居るため。上限は呼び出し側が契約どおりに渡す。
pub fn clamp_count(v: i64, default: usize, max: usize) -> usize {
    if v <= 0 {
        return default.min(max);
    }
    (v as usize).min(max)
}

// ══════════════════════════════════════════════════════════════════════
//  端末の色 — ANSI ではなく**構造**でスマホへ渡す
//
//  スマホ側でエスケープを解釈させない (パーサが 2 つになると必ずずれる)。
// ══════════════════════════════════════════════════════════════════════

/// `#rrggbb`。
pub fn hex_rgb(r: u8, g: u8, b: u8) -> String {
    format!("#{r:02x}{g:02x}{b:02x}")
}

/// 256 色の番号を `#rrggbb` にする (**純関数**)。
///
/// 0〜15 はテーマの 16 色をそのまま使い、16〜231 は 6×6×6 の立方体、
/// 232〜255 は 24 段の灰色。段の作り方は `terminal::ansi_color` と同じ式で、
/// **PC の端末とスマホで同じ色に見える**ことがここの目的。
pub fn ansi_hex(i: u8, base: &[[u8; 3]; 16]) -> String {
    if i < 16 {
        let c = base[i as usize];
        hex_rgb(c[0], c[1], c[2])
    } else if i < 232 {
        let i = i - 16;
        let f = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
        hex_rgb(f(i / 36), f((i % 36) / 6), f(i % 6))
    } else {
        let v = 8 + (i - 232) * 10;
        hex_rgb(v, v, v)
    }
}

/// 端末セル 1 個ぶんの見た目。色は `#rrggbb`、既定色は `None` (= 省略)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CellStyle {
    pub fg: Option<String>,
    pub bg: Option<String>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
}

impl CellStyle {
    /// 既定の見た目か (span を畳むときの「捨ててよい」判定に使う)。
    pub fn is_plain(&self) -> bool {
        *self == CellStyle::default()
    }
}

/// 同じ見た目が続くセルを 1 つに畳んだもの。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: CellStyle,
}

/// セル列を span へ畳む (**純関数**)。
///
/// 2 つのことを同時にやる:
/// * 同じ見た目が続く間は 1 つにまとめる (バイト数を減らす)
/// * **行末の「既定色の空白」を落とす** — 80〜200 桁の詰め物をそのまま
///   送ると、実際の文字よりパディングのほうが大きくなる
///
/// 落とすのは*既定色の*空白だけ。背景色の付いた空白は見た目そのものなので残す。
pub fn fold_spans(cells: &[(String, CellStyle)]) -> Vec<Span> {
    // 行末の詰め物を先に切る (畳んでから切ると、色付きの空白まで消しかねない)
    let mut end = cells.len();
    while end > 0 {
        let (t, st) = &cells[end - 1];
        if st.is_plain() && (t.is_empty() || t.chars().all(|c| c == ' ')) {
            end -= 1;
        } else {
            break;
        }
    }
    let mut out: Vec<Span> = Vec::new();
    for (t, st) in &cells[..end] {
        // 空セル (未描画) は 1 桁の空白として扱う。落とすと桁がずれる
        let t = if t.is_empty() { " " } else { t.as_str() };
        match out.last_mut() {
            Some(prev) if prev.style == *st => prev.text.push_str(t),
            _ => out.push(Span {
                text: t.to_string(),
                style: st.clone(),
            }),
        }
    }
    out
}

/// span 列を契約どおりの JSON へ落とす。
///
/// 既定値のキーは**出さない** (`fg`/`bg` は既定色なら省略、真偽は false なら省略)。
/// 1 画面 2000 行ぶん送るので、キー 1 つの差が実測で効く。
pub fn spans_json(spans: &[Span]) -> serde_json::Value {
    let arr: Vec<serde_json::Value> = spans
        .iter()
        .map(|s| {
            let mut o = serde_json::Map::new();
            o.insert("t".into(), serde_json::Value::String(s.text.clone()));
            if let Some(fg) = &s.style.fg {
                o.insert("fg".into(), serde_json::Value::String(fg.clone()));
            }
            if let Some(bg) = &s.style.bg {
                o.insert("bg".into(), serde_json::Value::String(bg.clone()));
            }
            for (k, v) in [
                ("bold", s.style.bold),
                ("italic", s.style.italic),
                ("underline", s.style.underline),
            ] {
                if v {
                    o.insert(k.into(), serde_json::Value::Bool(true));
                }
            }
            serde_json::Value::Object(o)
        })
        .collect();
    serde_json::Value::Array(arr)
}

// ══════════════════════════════════════════════════════════════════════
//  変更一覧 — 真実の在り処は git 1 つ (スマホ側で数え直さない)
// ══════════════════════════════════════════════════════════════════════

/// unified diff のヘッダから 1 文字の状態を決める (**純関数**)。
///
/// `"M"|"A"|"D"|"R"`。追跡外 (`"?"`) は diff に出てこないので、
/// `git status` 側から別に来る ([`untracked_paths_z`])。
pub fn change_status(old_path: &str, new_path: &str, is_rename: bool) -> &'static str {
    const DEV_NULL: &str = "/dev/null";
    if is_rename {
        "R"
    } else if old_path == DEV_NULL || old_path.is_empty() {
        "A"
    } else if new_path == DEV_NULL || new_path.is_empty() {
        "D"
    } else {
        "M"
    }
}

/// `git status --porcelain=v1 -z` の出力から**追跡外**のパスだけ拾う (**純関数**)。
///
/// `-z` を使うのは、引用 (`"a b.txt"`) を一切通さないため。改名 (`R`/`C`) は
/// **2 レコード**使うので、後ろの 1 つを読み飛ばさないと元パスを
/// エントリと取り違える。
pub fn untracked_paths_z(out: &str) -> Vec<String> {
    let mut v = Vec::new();
    let mut skip_next = false;
    for rec in out.split('\0').filter(|r| !r.is_empty()) {
        if skip_next {
            skip_next = false;
            continue;
        }
        let Some((xy, path)) = rec.split_at_checked(3) else {
            continue;
        };
        let x = xy.as_bytes()[0];
        if x == b'R' || x == b'C' {
            skip_next = true;
        }
        if xy.starts_with("??") {
            v.push(path.replace('\\', "/"));
        }
    }
    v
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

// スマホ用ページ。**中身は `assets/remote/` に置いてある。**
//
// 1 つの巨大な `const` 文字列だったものを、層ごとのファイルへ割った。理由は 2 つ:
//
// * **複数のエージェントが同時に触れるようにするため。** git が衝突を作るのは
//   「2 つのブランチが同じファイルの近い行を触った」時だけなので、画面の担当ごとに
//   ファイルを分ければ、並行して足しても構造的に衝突しない
//   (`src/features/` で潰したのと同じ形の衝突が web 側に残っていた)。
//   **`assets/remote/js/<名前>.js` を 1 つ置くだけで画面が増える** —
//   共有ファイルへの追記が 1 行も要らない。
// * 3200 行の Rust の中に 900 行の HTML/CSS/JS が埋まっていると、
//   エディタの補完も検索も効かない。
//
// 一覧は `build.rs` が走査して `OUT_DIR/remote_page.rs` へ生成する
// (`concat!` はリテラルしか受け取らないので、`PAGE` ごと生成している)。
// **ビルド時に 1 本の文字列へ畳まれる**ので、実行時にファイルを探しに行かない。
// JS はファイル名順に繋ぐ (1 つのスコープを共有するため番号で並びを固定する)。
include!(concat!(env!("OUT_DIR"), "/remote_page.rs"));

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
            Query::Changes,
            Query::Diff {
                rel: "src/app.rs".into(),
            },
            Query::Scrollback {
                agent: -1,
                lines: 200,
                before: None,
            },
            Query::Approvals,
            Query::Approve {
                id: 3,
                act: ApproveAct::Approve,
            },
            Query::Read {
                rel: "src/app.rs".into(),
                from: 1,
                lines: 400,
            },
            Query::Search {
                q: "fn main".into(),
                max: 100,
            },
        ]
    }

    // ─── スマホ用 API の純粋ロジック (契約をここで固定する) ─────────

    #[test]
    fn 知らない承認操作は受け付けない() {
        // 綴り違いを黙って「承認」に落とすと、押していない承認が飛ぶ
        for (s, want) in [
            ("approve", Some(ApproveAct::Approve)),
            ("deny", Some(ApproveAct::Deny)),
            ("always", Some(ApproveAct::Always)),
            ("always_deny", Some(ApproveAct::AlwaysDeny)),
            ("APPROVE", None),
            ("yes", None),
            ("", None),
            ("approve ", None),
        ] {
            assert_eq!(ApproveAct::parse(s), want, "act={s:?}");
        }
    }

    #[test]
    fn パスはルート配下へ畳めないものを断る() {
        for (raw, want) in [
            ("src/app.rs", Some("src/app.rs")),
            ("./src/./app.rs", Some("src/app.rs")),
            ("src//app.rs", Some("src/app.rs")),
            // Windows の区切りは / へ寄せる (同じファイルが 2 通りに見えない)
            ("src\\app.rs", Some("src/app.rs")),
            ("a/b/c.txt", Some("a/b/c.txt")),
            // ここから下は全部お断り
            ("", None),
            ("..", None),
            ("../etc/passwd", None),
            ("src/../../etc/passwd", None),
            ("/etc/passwd", None),
            ("\\etc\\passwd", None),
            ("C:/Windows/system32", None),
            ("c:notes.txt", None),
            ("\\\\server\\share\\x", None),
            (".", None),
            ("./", None),
            ("a\0b", None),
        ] {
            assert_eq!(
                safe_rel(raw).as_deref(),
                want,
                "safe_rel({raw:?}) が契約と違う"
            );
        }
    }

    #[test]
    fn 件数は無指定と範囲外を丸める() {
        // 0 / 負数は「無指定」= 既定。上限は必ず効かせる
        for (v, def, max, want) in [
            (0i64, 200usize, 2000usize, 200usize),
            (-5, 200, 2000, 200),
            (1, 200, 2000, 1),
            (2000, 200, 2000, 2000),
            (99999, 200, 2000, 2000),
            // 既定が上限を超えていても上限で止まる
            (0, 5000, 2000, 2000),
        ] {
            assert_eq!(clamp_count(v, def, max), want, "v={v}");
        }
    }

    #[test]
    fn クエリ文字列を復号して取り出す() {
        let qs = "t=abc&path=src%2Fapp.rs&q=fn+main&empty=";
        assert_eq!(query_param(qs, "path").as_deref(), Some("src/app.rs"));
        assert_eq!(query_param(qs, "q").as_deref(), Some("fn main"));
        assert_eq!(query_param(qs, "empty").as_deref(), Some(""));
        assert_eq!(query_param(qs, "nope"), None);
        // 日本語も通る (スマホの検索窓から普通に来る)
        assert_eq!(
            query_param("q=%E6%97%A5%E6%9C%AC%E8%AA%9E", "q").as_deref(),
            Some("日本語")
        );
        // 壊れた % は落とさずそのまま残す (100% のような検索語を殺さない)
        assert_eq!(query_param("q=100%", "q").as_deref(), Some("100%"));
        assert_eq!(query_param("q=a%zz", "q").as_deref(), Some("a%zz"));
    }

    #[test]
    fn 変更の状態は1文字に決まる() {
        for (o, n, ren, want) in [
            ("a.rs", "a.rs", false, "M"),
            ("/dev/null", "a.rs", false, "A"),
            ("a.rs", "/dev/null", false, "D"),
            ("a.rs", "b.rs", true, "R"),
            // 改名は他のどれより優先する (中身も変わっているのが普通)
            ("/dev/null", "b.rs", true, "R"),
            ("", "a.rs", false, "A"),
        ] {
            assert_eq!(change_status(o, n, ren), want, "{o} -> {n} rename={ren}");
        }
    }

    #[test]
    fn 追跡外だけをstatusから拾う() {
        // -z なので引用は無い。改名は 2 レコード使う
        let out = "?? new.txt\0 M src/app.rs\0R  dst.rs\0src.rs\0?? a b.txt\0A  added.rs\0";
        assert_eq!(untracked_paths_z(out), vec!["new.txt", "a b.txt"]);
        // 改名の「元パス」をエントリと取り違えない
        let ren = "R  ?? weird.txt\0?? old.txt\0";
        assert_eq!(untracked_paths_z(ren), Vec::<String>::new());
        assert_eq!(untracked_paths_z(""), Vec::<String>::new());
    }

    #[test]
    fn 拡張256色は同じ式でrgbになる() {
        let base = {
            let mut b = [[0u8; 3]; 16];
            b[1] = [255, 0, 0];
            b[15] = [255, 255, 255];
            b
        };
        // 0〜15 はテーマの色をそのまま
        assert_eq!(ansi_hex(1, &base), "#ff0000");
        assert_eq!(ansi_hex(15, &base), "#ffffff");
        // 6×6×6 の立方体 (16 が黒、231 が白)
        assert_eq!(ansi_hex(16, &base), "#000000");
        assert_eq!(ansi_hex(231, &base), "#ffffff");
        assert_eq!(ansi_hex(196, &base), "#ff0000");
        // 24 段の灰色
        assert_eq!(ansi_hex(232, &base), "#080808");
        assert_eq!(ansi_hex(255, &base), "#eeeeee");
    }

    /// span 畳み込み。**行末の詰め物を落とすこと**が本題
    /// (80〜200 桁の空白をそのまま送ると、本文よりパディングが大きくなる)。
    #[test]
    fn 同じ見た目のセルは1つに畳まれ行末の空白は落ちる() {
        let plain = CellStyle::default();
        let red = CellStyle {
            fg: Some("#ff0000".into()),
            ..Default::default()
        };
        let cell = |t: &str, st: &CellStyle| (t.to_string(), st.clone());

        // 同じ属性が続けば 1 span
        let out = fold_spans(&[
            cell("a", &plain),
            cell("b", &plain),
            cell("c", &red),
            cell("d", &red),
        ]);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].text, "ab");
        assert_eq!(out[1].text, "cd");
        assert_eq!(out[1].style, red);

        // 行末の既定色の空白は消える (空セルも同じ扱い)
        let out = fold_spans(&[
            cell("x", &plain),
            cell(" ", &plain),
            cell("", &plain),
            cell(" ", &plain),
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "x");

        // 背景色の付いた空白は**見た目そのもの**なので残す
        let bg = CellStyle {
            bg: Some("#003300".into()),
            ..Default::default()
        };
        let out = fold_spans(&[cell("x", &plain), cell(" ", &bg)]);
        assert_eq!(out.len(), 2, "色付きの空白まで落としている");
        assert_eq!(out[1].text, " ");

        // 途中の空白は落とさない (桁がずれる)
        let out = fold_spans(&[cell("a", &plain), cell(" ", &plain), cell("b", &plain)]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].text, "a b");

        // 全部が詰め物なら空 (1 行まるごと省ける)
        assert!(fold_spans(&[cell(" ", &plain), cell(" ", &plain)]).is_empty());
        assert!(fold_spans(&[]).is_empty());
    }

    #[test]
    fn spanのjsonは既定値のキーを出さない() {
        let spans = vec![
            Span {
                text: "err".into(),
                style: CellStyle {
                    fg: Some("#f85149".into()),
                    bold: true,
                    ..Default::default()
                },
            },
            Span {
                text: ": failed".into(),
                style: CellStyle::default(),
            },
        ];
        let v = spans_json(&spans);
        let a = v.as_array().expect("配列");
        assert_eq!(a[0]["t"], "err");
        assert_eq!(a[0]["fg"], "#f85149");
        assert_eq!(a[0]["bold"], true);
        assert!(a[0].get("bg").is_none(), "既定の背景色を書いている");
        assert!(a[0].get("italic").is_none(), "false を書いている");
        // 既定色の span は t だけ
        assert_eq!(a[1].as_object().expect("obj").len(), 1);
    }

    #[test]
    fn pendingの応答だけ聞き直す() {
        assert!(is_pending(r#"{"ok":false,"pending":true}"#));
        assert!(!is_pending(r#"{"ok":false,"pending":false}"#));
        assert!(!is_pending(r#"{"ok":true,"files":[]}"#));
        // 読めない応答を聞き直しの輪に入れない (塞ぐより見せる)
        assert!(!is_pending("not json"));
        assert!(!is_pending(""));
        assert!(!is_pending(r#"{"pending":"yes"}"#));
    }

    #[test]
    fn 聞き直すのはgitと検索だけ() {
        // 聞き直す = UI スレッドで待たない要求。他は 1 往復で答えが出る
        for q in all_query_variants() {
            let expected = matches!(
                &q,
                Query::Changes | Query::Diff { .. } | Query::Search { .. }
            );
            assert_eq!(q.retries_while_pending(), expected);
        }
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
                | Query::AgentAct { .. }
                // 読み取りは値そのものが要る。承認の決着も「効いたか」を返す
                | Query::Changes
                | Query::Diff { .. }
                | Query::Scrollback { .. }
                | Query::Approvals
                | Query::Approve { .. }
                | Query::Read { .. }
                | Query::Search { .. } => false,
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
