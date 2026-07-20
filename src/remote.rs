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

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream, UdpSocket};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use eframe::egui;

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
    VoiceSend {
        text: String,
        id: i64,
        submit: bool,
    },
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

pub struct RemoteServer {
    pub port: u16,
    pub token: String,
    /// トークンなしのベース URL (例: http://192.168.1.10:8899/)
    pub url: String,
    rx: mpsc::Receiver<Request>,
}

impl RemoteServer {
    /// サーバを起動する。8899 から順に空きポートを探す。
    pub fn start(ctx: egui::Context) -> Result<Self, String> {
        let mut listener = None;
        let mut port = 0u16;
        for p in 8899..8920u16 {
            if let Ok(l) = TcpListener::bind(("0.0.0.0", p)) {
                listener = Some(l);
                port = p;
                break;
            }
        }
        let listener = listener.ok_or("空きポートがありません (8899-8919)")?;

        let token = gen_token();
        let url = format!("http://{}:{}/", lan_ip(), port);
        let (tx, rx) = mpsc::channel::<Request>();

        let tok = token.clone();
        std::thread::Builder::new()
            .name("zv-remote-accept".into())
            .spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { continue };
                    let tx = tx.clone();
                    let ctx = ctx.clone();
                    let tok = tok.clone();
                    let _ = std::thread::Builder::new()
                        .name("zv-remote-conn".into())
                        .spawn(move || handle_conn(stream, tx, ctx, tok));
                }
            })
            .map_err(|e| format!("サーバスレッド起動失敗: {e}"))?;

        eprintln!("📱 スマホリモート起動: {url}?t={token}");
        Ok(Self {
            port,
            token,
            url,
            rx,
        })
    }

    /// UI スレッドから毎フレーム呼ぶ。溜まっているリクエストを取り出す。
    pub fn poll(&self) -> Vec<Request> {
        self.rx.try_iter().collect()
    }
}

/// 起動ごとのランダムトークン (10桁hex)。
fn gen_token() -> String {
    let mut h = DefaultHasher::new();
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0)
        .hash(&mut h);
    std::process::id().hash(&mut h);
    format!("{:016x}", h.finish())[..10].to_string()
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

fn handle_conn(mut stream: TcpStream, tx: mpsc::Sender<Request>, ctx: egui::Context, token: String) {
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
        let Some((k, v)) = l.split_once(':') else { continue };
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

    // ボディを読む
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

    // ─── ルーティング ───
    if path == "/" || path == "/index.html" {
        return respond(&mut stream, 200, "text/html; charset=utf-8", PAGE.as_bytes());
    }
    if path == "/voice" {
        // PC 用の音声入力ページ (Web Speech API — 127.0.0.1 で開くこと)
        return respond(&mut stream, 200, "text/html; charset=utf-8", VOICE_PAGE.as_bytes());
    }
    if !path.starts_with("/api/") {
        return respond(&mut stream, 404, "text/plain", b"not found");
    }

    // 認証: X-Token ヘッダ または ?t= クエリ
    let q_token = query_str
        .split('&')
        .find_map(|kv| kv.strip_prefix("t="))
        .unwrap_or("");
    if hdr_token != token && q_token != token {
        return respond(&mut stream, 401, "application/json", br#"{"ok":false,"error":"unauthorized"}"#);
    }

    let json: serde_json::Value =
        serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
    let s = |k: &str| json.get(k).and_then(|v| v.as_str()).unwrap_or("").to_string();
    let n = |k: &str| json.get(k).and_then(|v| v.as_i64()).unwrap_or(0);

    let query = match (method.as_str(), path.as_str()) {
        ("GET", "/api/state") => Query::State,
        ("GET", "/api/file") => Query::File,
        ("GET", "/api/files") => Query::Files,
        ("GET", "/api/term") => Query::Term,
        ("POST", "/api/text") => Query::SetText {
            text: s("text"),
            index: json.get("index").and_then(|v| v.as_i64()).unwrap_or(-1),
            save: json.get("save").and_then(|v| v.as_bool()).unwrap_or(false),
        },
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
            let level = if level.is_empty() { "info".into() } else { level };
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
            submit: json.get("submit").and_then(|v| v.as_bool()).unwrap_or(false),
        },
        ("POST", "/api/tab") => Query::Tab(n("index").max(0) as usize),
        ("POST", "/api/term") => {
            Query::TermInput(s("text"), json.get("raw").and_then(|v| v.as_bool()).unwrap_or(false))
        }
        ("POST", "/api/voice") => Query::VoiceSend {
            text: s("text"),
            id: json.get("id").and_then(|v| v.as_i64()).unwrap_or(-1),
            // 既定は「挿入のみ」。送信は明示的に submit=true を渡したときだけ
            submit: json.get("submit").and_then(|v| v.as_bool()).unwrap_or(false),
        },
        _ => return respond(&mut stream, 404, "application/json", br#"{"ok":false,"error":"unknown api"}"#),
    };

    // UI スレッドへ渡す
    let (rtx, rrx) = mpsc::sync_channel::<String>(1);
    let immediate = query.is_fire_and_forget().then(|| query.ack());
    if tx.send(Request { query, reply: rtx }).is_err() {
        return respond(&mut stream, 500, "application/json", br#"{"ok":false,"error":"app closed"}"#);
    }

    // 一方向の指示は積んだ時点で成功。UI スレッドの復帰を待たない。
    if let Some(js) = immediate {
        ctx.request_repaint();
        return respond(&mut stream, 200, "application/json; charset=utf-8", js.as_bytes());
    }
    // UI スレッドは次のフレームでしか応答できない。ウィンドウが背面や
    // 非表示だとフレームが来る間隔が延びるため、1 回だけ起こして待つと
    // 取りこぼす。応答が返るまで一定間隔で起こし続ける。
    let deadline = Instant::now() + REMOTE_TIMEOUT;
    let reply = loop {
        ctx.request_repaint();
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
        Some(js) => respond(&mut stream, 200, "application/json; charset=utf-8", js.as_bytes()),
        None => respond(&mut stream, 504, "application/json", br#"{"ok":false,"error":"timeout"}"#),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_is_10_hex_chars() {
        let t = gen_token();
        assert_eq!(t.len(), 10);
        assert!(t.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn subslice_finds_header_end() {
        assert_eq!(find_subslice(b"GET / HTTP/1.1\r\n\r\nbody", b"\r\n\r\n"), Some(14));
        assert_eq!(find_subslice(b"abc", b"\r\n\r\n"), None);
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
  <span class="ws" id="ws">接続中…</span>
  <span id="dot"></span>
</header>
<main>
  <!-- エディタ -->
  <section class="view act" id="v-editor">
    <div class="chips" id="tabs"></div>
    <textarea id="ta" autocapitalize="off" autocorrect="off" spellcheck="false"
      placeholder="PC 側でファイルを開くか、[ファイル] タブから選択してください"></textarea>
    <div class="bar">
      <span id="meta"></span>
      <span class="grow"></span>
      <button class="btn" id="reload">&#8635; 再読込</button>
      <button class="btn pri" id="save">&#128190; 保存</button>
    </div>
  </section>
  <!-- ファイル -->
  <section class="view" id="v-files">
    <div class="bar" style="border-top:none;border-bottom:1px solid #21262d">
      <input id="filter" type="search" placeholder="ファイル名で絞り込み…">
    </div>
    <div id="flist"></div>
  </section>
  <!-- エージェント -->
  <section class="view" id="v-agent">
    <div class="chips" id="achips"></div>
    <div id="scr" class="empty">エージェントがいません</div>
    <div class="keys" id="keys"></div>
    <div class="bar">
      <input id="ti" type="text" autocapitalize="off" autocorrect="off"
        placeholder="エージェントへ指示を送る…">
      <button class="btn" id="tput" title="Enter を送らずに入力欄へ入れるだけ">&#10549; 入れる</button>
      <button class="btn pri" id="tsend">送信</button>
    </div>
  </section>
  <!-- コマンド -->
  <section class="view" id="v-cmds">
    <div class="grid" id="cmds"></div>
  </section>
</main>
<nav id="nav">
  <button data-v="editor" class="act"><span class="ico">&#128196;</span>エディタ</button>
  <button data-v="files"><span class="ico">&#128194;</span>ファイル</button>
  <button data-v="agent"><span class="ico">&#129302;</span>エージェント</button>
  <button data-v="cmds"><span class="ico">&#127899;</span>コマンド</button>
</nav>
<div id="toast"></div>
<script>
'use strict';
const qs = new URLSearchParams(location.search);
let TOK = qs.get('t') || localStorage.getItem('zv_tok') || '';
if (qs.get('t')) localStorage.setItem('zv_tok', qs.get('t'));
const $ = id => document.getElementById(id);
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
  if (r.status === 401) { toast('認証エラー: QRコードを読み直してください'); throw 0; }
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
    renderTabs(); renderAgents(); renderCmds();
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
    $('meta').textContent = f.title + '  ·  ' + f.lang;
    taTab = (f.index === undefined || f.index === null) ? -1 : f.index;
    dirty = false;
  } catch (e) {}
}
$('ta').addEventListener('input', () => { dirty = true; });
$('reload').onclick = () => { dirty = false; loadFile().then(() => toast('再読込しました')); };
$('save').onclick = async () => {
  try {
    // 適用+保存を 1 リクエストで原子的に行う。タブ不一致はサーバ側で拒否される
    const r = await api('/api/text', {text: $('ta').value, index: taTab, save: true});
    if (r.ok) {
      dirty = false;
      toast('PC 側で保存しました ✅');
    } else {
      toast(r.error || '保存に失敗しました');
    }
  } catch (e) { toast('保存に失敗しました'); }
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
  if (!hit.length) { el.innerHTML = '<div class="empty">該当なし</div>'; return; }
  hit.forEach(f => {
    const d = document.createElement('div');
    const i = f.lastIndexOf('/');
    d.innerHTML = '<span></span><br><span class="dir"></span>';
    d.children[0].textContent = i >= 0 ? f.slice(i + 1) : f;
    d.children[2].textContent = i >= 0 ? f.slice(0, i) : '';
    d.onclick = async () => {
      await api('/api/open', {path: f});
      dirty = false;
      toast(f + ' を開きました');
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
  ['Tab', '\t'], ['⇧Tab 権限', ESC + '[Z'],
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
let voiceAgent = -1, recog = null, lastInterim = '';
function speechAPI() { return window.SpeechRecognition || window.webkitSpeechRecognition; }
function stopVoice0() {
  voiceAgent = -1;
  const r = recog; recog = null;
  if (r) { r.onend = null; try { r.stop(); } catch (e) {} }
  if ($('ti').value === lastInterim) $('ti').value = '';
  lastInterim = '';
  $('ti').placeholder = 'エージェントへ指示を送る…';
}
function stopVoice() { stopVoice0(); renderAgents(); toast('\u{1F3A4} 音声入力モード OFF'); }
function startVoice(i) {
  const C = speechAPI();
  if (!C) { toast('この端末のブラウザは音声入力に未対応です'); return; }
  stopVoice0();
  voiceAgent = i;
  api('/api/cmd', {name:'agent_focus', arg:i}).then(pollState).catch(() => {});
  const r = new C();
  recog = r;
  r.lang = 'ja-JP';
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
    if (ev.error === 'not-allowed' || ev.error === 'service-not-allowed') {
      toast('マイクが許可されていません（ブラウザ設定を確認）'); stopVoice();
    } else if (ev.error === 'audio-capture') {
      toast('マイクが見つかりません'); stopVoice();
    }
  };
  r.onend = () => {
    if (recog === r && voiceAgent === i) {
      try { r.start(); } catch (e) { stopVoice(); }
    }
  };
  try { r.start(); } catch (e) { toast('音声入力を開始できません'); stopVoice0(); renderAgents(); return; }
  $('ti').placeholder = '\u{1F3A4} 話した内容がここに溜まります — 送信はボタンで';
  renderAgents();
  const a = (state.agents || [])[i];
  toast('\u{1F3A4} 音声入力モード ON → ' + (a ? a.title : '') + ' (自動送信はしません)');
}
function renderAgents() {
  const el = $('achips');
  el.innerHTML = '';
  const agents = state.agents || [];
  if (voiceAgent >= agents.length) stopVoice0();
  agents.forEach((a, i) => {
    const c = document.createElement('button');
    c.className = 'chip' + (i === state.agent_active ? ' act' : '');
    c.textContent = (a.running ? (a.attention ? '\u{1F514} ' : '● ') : '○ ') + a.icon + ' ' + a.title;
    c.onclick = () => api('/api/cmd', {name:'agent_focus', arg:i}).then(pollState).catch(() => {});
    el.appendChild(c);
    const m = document.createElement('button');
    m.className = 'chip mic' + (i === voiceAgent ? ' rec' : '');
    m.textContent = i === voiceAgent ? '⏹ 停止' : '\u{1F3A4}';
    m.title = a.title + ' へ音声入力';
    m.onclick = () => (i === voiceAgent ? stopVoice() : startVoice(i));
    el.appendChild(m);
  });
  const plus = document.createElement('button');
  plus.className = 'chip'; plus.textContent = '＋ 起動';
  plus.onclick = () => {
    const names = (state.presets || []).map((p, i) => i + ': ' + p.icon + ' ' + p.name).join('\n');
    const v = prompt('起動するプリセット番号\n' + names, '0');
    if (v !== null) api('/api/cmd', {name:'agent_launch', arg:parseInt(v) || 0}).then(pollState).catch(() => {});
  };
  el.appendChild(plus);
}
let termTimer = null;
async function pollTerm() {
  if (view !== 'agent') return;
  try {
    const r = await api('/api/term');
    const el = $('scr');
    if (r.ok) {
      const stick = el.scrollTop + el.clientHeight >= el.scrollHeight - 24;
      el.classList.remove('empty');
      el.textContent = r.text;
      if (stick) el.scrollTop = el.scrollHeight;
    } else {
      el.classList.add('empty');
      el.textContent = 'エージェントがいません — ＋ 起動 から始められます';
    }
  } catch (e) {}
  clearTimeout(termTimer);
  termTimer = setTimeout(pollTerm, 1500);
}
// 送信 = テキスト + Enter。入れる = テキストのみ (PC 側で内容を見て Enter)
async function sendInput(submit) {
  const v = $('ti').value.trim();
  if (!v) return;
  if (voiceAgent >= 0) {
    // 音声モード中は、選んだエージェントへ確実に届くようフォーカスし直す
    await api('/api/cmd', {name:'agent_focus', arg:voiceAgent}).catch(() => {});
  }
  await api('/api/term', {text: v, raw: !submit}).catch(() => {});
  $('ti').value = ''; lastInterim = '';
  toast(submit ? '送信しました' : 'PC の入力欄に入れました (Enter で送信)');
}
$('tsend').onclick = () => sendInput(true);
$('tput').onclick = () => sendInput(false);
$('ti').addEventListener('keydown', e => { if (e.key === 'Enter') sendInput(true); });

// ─── コマンド ───
const CMDS = [
  ['\u{1F4BE} 保存', 'save'], ['\u{1F4C4} 新規ファイル', 'new'],
  ['❌ タブを閉じる', 'close_tab'], ['\u{1F5A5} ターミナル', 'terminal'],
  ['\u{1F4C1} サイドバー', 'sidebar'], ['\u{1F39b} Cockpit', 'cockpit'],
  ['\u{1F520} フォント +', 'font_inc'], ['\u{1F520} フォント −', 'font_dec'],
  ['\u{1F332} ツリー更新', 'tree'], ['\u{1F6e1} 承認モード', 'approval_ask'],
  ['⚡ 全自動モード', 'approval_auto'], ['\u{1F916} Agent優先モード', 'approval_agent'],
  ['\u{1F6e1} 権限切替(全Agent)', 'permission_cycle'],
];
function renderCmds() {
  const el = $('cmds');
  if (el.childElementCount) return;
  CMDS.forEach(([label, name]) => {
    const b = document.createElement('button');
    b.className = 'btn' + (name === 'approval_auto' ? ' warn' : '');
    b.textContent = label;
    b.onclick = () => api('/api/cmd', {name: name, arg: 0})
      .then(r => toast(r.ok ? label + ' を実行' : (r.error || '失敗しました')))
      .catch(() => {});
    el.appendChild(b);
  });
}

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
<title>Zaivern 音声入力</title>
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
  <span class="logo">&#9889; ZAIVERN &#127908; 音声入力</span>
  <span id="dot"></span>
</header>
<main>
  <h2>送信先 (クリックで切替 — 話している途中でも変更できます)</h2>
  <div class="chips" id="targets"></div>
  <button id="mic">&#127908;</button>
  <div id="hint">マイクボタンを押して話しかけてください — 内容を確認してからボタンで送ります</div>
  <div id="interim"></div>
  <textarea id="draft" placeholder="話した内容がここに溜まります。直してから送信できます。"></textarea>
  <div class="row">
    <button class="btn" id="clear">&#128465; 消す</button>
    <span class="grow"></span>
    <button class="btn" id="put" title="Enter を送らずに入力欄へ入れるだけ">&#10549; 入力欄へ入れる</button>
    <button class="btn pri" id="send">&#9654; 送信 (Enter まで送る)</button>
  </div>
  <div id="log"></div>
</main>
<script>
'use strict';
const qs = new URLSearchParams(location.search);
const TOK = qs.get('t') || '';
let target = qs.get('target') || 'all';  // 'all' またはセッション id
let agents = [], active = false, recog = null;
const $ = id => document.getElementById(id);
const HINT0 = 'マイクボタンを押して話しかけてください — 内容を確認してからボタンで送ります';

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
  all.textContent = '\u{1F4E3} 全エージェントへブロードキャスト';
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
  if (target === 'all') return '\u{1F4E3} 全エージェント';
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
      addLog((submit ? '▶ 送信 ' : '⤵ 入力欄へ ') + name + ' ← ' + text);
      $('draft').value = '';
    } else {
      addLog('⚠ ' + (r.error || '失敗') + ': ' + text);
    }
  } catch (e) { addLog('⚠ 送信に失敗しました: ' + text); }
}
$('send').onclick = () => send(true);
$('put').onclick = () => send(false);
$('clear').onclick = () => { $('draft').value = ''; };
function speechAPI() { return window.SpeechRecognition || window.webkitSpeechRecognition; }
function stopVoice() {
  active = false;
  const r = recog; recog = null;
  if (r) { r.onend = null; try { r.stop(); } catch (e) {} }
  $('mic').classList.remove('rec');
  $('hint').textContent = HINT0;
  $('interim').textContent = '';
}
function startVoice() {
  const C = speechAPI();
  if (!C) {
    $('hint').textContent = 'このブラウザは音声認識に未対応です — Chrome か Safari をお使いください';
    return;
  }
  const r = new C();
  recog = r; active = true;
  r.lang = 'ja-JP';
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
    if (ev.error === 'not-allowed' || ev.error === 'service-not-allowed') {
      $('hint').textContent = 'マイクが許可されていません — アドレスバーのマイク設定を確認してください';
      stopVoice();
    } else if (ev.error === 'audio-capture') {
      $('hint').textContent = 'マイクが見つかりません';
      stopVoice();
    }
  };
  r.onend = () => {
    // 無音で切れてもモードが ON の間は自動で再開する
    if (recog === r && active) { try { r.start(); } catch (e) { stopVoice(); } }
  };
  try { r.start(); } catch (e) { $('hint').textContent = '音声認識を開始できません'; stopVoice(); return; }
  $('mic').classList.add('rec');
  $('hint').textContent = '\u{1F3A4} 認識中 — もう一度押すと停止します';
}
$('mic').onclick = () => (active ? stopVoice() : startVoice());
poll();
setInterval(poll, 2500);
</script>
</body>
</html>
"##;
