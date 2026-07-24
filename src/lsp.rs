//! 最小 LSP クライアント (stdio JSON-RPC)。
//! std::thread + mpsc レス設計: 受信スレッドが共有状態(Mutex)へ書き込み、UI は poll で取得。
//! UI 依存は egui Context の request_repaint 通知のみ。

#![allow(dead_code)]

use crate::lockx::lock_ok;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 公開データ型
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub line: usize, // 0-based
    pub col: usize,  // UTF-16 code unit
    pub end_line: usize,
    pub end_col: usize,
    pub severity: u8, // 1=err 2=warn 3=info 4=hint
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompletionItem {
    pub label: String,
    pub insert_text: String,
    pub detail: String,
    pub kind: u8,
}

#[derive(Debug, Clone, PartialEq)]
pub struct HoverInfo {
    pub contents: String,
}

// ---------------------------------------------------------------------------
// Content-Length フレーミング (純関数)
// ---------------------------------------------------------------------------

pub fn encode_message(json: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(json.len() + 32);
    out.extend_from_slice(format!("Content-Length: {}\r\n\r\n", json.len()).as_bytes());
    out.extend_from_slice(json.as_bytes());
    out
}

#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    pub fn next_message(&mut self) -> Option<String> {
        let hdr_end = find_subslice(&self.buf, b"\r\n\r\n")?;
        let header = String::from_utf8_lossy(&self.buf[..hdr_end]).into_owned();
        let mut content_len: Option<usize> = None;
        for line in header.split("\r\n") {
            if let Some((name, val)) = line.split_once(':') {
                if name.trim().eq_ignore_ascii_case("content-length") {
                    content_len = val.trim().parse().ok();
                }
            }
        }
        let len = match content_len {
            Some(l) => l,
            None => {
                // 不正ヘッダ: 読み捨てて前進 (無限ループ防止)
                self.buf.drain(..hdr_end + 4);
                return None;
            }
        };
        let body_start = hdr_end + 4;
        if self.buf.len() < body_start + len {
            return None; // 本文未着
        }
        let body = String::from_utf8_lossy(&self.buf[body_start..body_start + len]).into_owned();
        self.buf.drain(..body_start + len);
        Some(body)
    }
}

fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// ---------------------------------------------------------------------------
// 位置変換: LSP は UTF-16 code unit オフセット
// ---------------------------------------------------------------------------

/// text 内の char index → (line, utf16 col)
pub fn char_index_to_lsp_pos(text: &str, char_idx: usize) -> (usize, usize) {
    let mut line = 0usize;
    let mut col16 = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if i == char_idx {
            return (line, col16);
        }
        if ch == '\n' {
            line += 1;
            col16 = 0;
        } else {
            col16 += ch.len_utf16();
        }
    }
    (line, col16)
}

/// (line, utf16 col) → char index。範囲外はクランプ。
pub fn lsp_pos_to_char_index(text: &str, line: usize, utf16_col: usize) -> usize {
    let mut cur_line = 0usize;
    let mut col16 = 0usize;
    for (i, ch) in text.chars().enumerate() {
        if cur_line == line {
            if col16 >= utf16_col {
                return i;
            }
            if ch == '\n' {
                return i; // 行末を超える col は行末へクランプ
            }
            col16 += ch.len_utf16();
        } else if ch == '\n' {
            cur_line += 1;
        }
    }
    text.chars().count()
}

// ---------------------------------------------------------------------------
// URI ヘルパ
// ---------------------------------------------------------------------------

fn path_to_uri(path: &Path) -> String {
    let p = path.to_string_lossy();
    let mut enc = String::with_capacity(p.len() + 8);
    for b in p.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                enc.push(*b as char)
            }
            _ => enc.push_str(&format!("%{:02X}", b)),
        }
    }
    format!("file://{}", enc)
}

fn uri_to_path(uri: &str) -> PathBuf {
    let s = uri.strip_prefix("file://").unwrap_or(uri);
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
            if let Some(v) = hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    PathBuf::from(String::from_utf8_lossy(&out).into_owned())
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

// ---------------------------------------------------------------------------
// LspClient
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy)]
enum Pending {
    Initialize,
    Completion,
    Hover,
    Definition,
}

/// textDocument/definition の結果 (先頭の 1 件)。
#[derive(Debug, Clone, PartialEq)]
pub struct DefinitionLoc {
    pub path: PathBuf,
    pub line: usize, // 0-based
    pub col: usize,  // UTF-16 code unit
}

struct Shared {
    alive: AtomicBool,
    /// initialize 応答を受信し initialized 通知を送信済み。
    /// これが立つまで他の通知・リクエストを送ってはならない (LSP 仕様)。
    init_done: AtomicBool,
    diags: Mutex<HashMap<PathBuf, Arc<Vec<Diagnostic>>>>,
    pending: Mutex<HashMap<u64, Pending>>,
    completion: Mutex<Option<Vec<CompletionItem>>>,
    hover: Mutex<Option<HoverInfo>>,
    /// 外側 Some = 応答受信済み。内側 None = 定義が見つからなかった。
    definition: Mutex<Option<Option<DefinitionLoc>>>,
    latest_completion: AtomicU64,
    latest_hover: AtomicU64,
    latest_definition: AtomicU64,
}

impl Shared {
    fn new() -> Self {
        Shared {
            alive: AtomicBool::new(true),
            init_done: AtomicBool::new(false),
            diags: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            completion: Mutex::new(None),
            hover: Mutex::new(None),
            definition: Mutex::new(None),
            latest_completion: AtomicU64::new(0),
            latest_hover: AtomicU64::new(0),
            latest_definition: AtomicU64::new(0),
        }
    }
}

pub struct LspClient {
    child: Child,
    tx: mpsc::Sender<Value>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    versions: Mutex<HashMap<PathBuf, i64>>,
}

/// チャネルへ積むだけ (サーバー I/O 待ちなし)。実際の書き込みは writer_loop が行う。
fn send_json(tx: &mpsc::Sender<Value>, v: Value) -> Result<(), mpsc::SendError<Value>> {
    tx.send(v)
}

/// 書き込み専用スレッド: ChildStdin を専有し、チャネルで受けた JSON をフレーミングして書く。
/// サーバーが詰まってもブロックするのはこのスレッドだけで、送信側は巻き込まれない。
/// 全 Sender の drop (チャネル切断) か書き込み失敗で終了する。
fn writer_loop<W: Write>(mut stdin: W, rx: mpsc::Receiver<Value>, shared: Arc<Shared>) {
    while let Ok(v) = rx.recv() {
        let bytes = encode_message(&v.to_string());
        if stdin.write_all(&bytes).and_then(|_| stdin.flush()).is_err() {
            shared.alive.store(false, Ordering::SeqCst);
            break;
        }
    }
}

impl LspClient {
    /// server_cmd は $SHELL -lc 経由で起動 (PATH 解決のため)。initialize は送信だけして
    /// すぐ返る (UI スレッドをブロックしない)。応答は受信スレッドが処理して is_ready が
    /// true になるので、呼び出し側はそれまで通知・リクエストを送らないこと。
    ///
    /// マルチルートワークスペースの扱い:
    /// ここでは 1 サーバー = 1 ルート（`rootUri` / `workspaceFolders` は常に 1 要素）とし、
    /// 呼び出し側 (app.rs) が **(言語ID, ルート) をキーにサーバーを 1 つずつ起動する**。
    ///
    /// もう一方の選択肢は `workspaceFolders` に全ルートを並べて
    /// `workspace/didChangeWorkspaceFolders` で増減を通知する方式で、
    /// プロセス数は 1 つで済む。しかし
    /// - サーバー側の対応がまちまち (rust-analyzer は複数ルートを 1 プロセスで
    ///   扱えるが、多くの軽量サーバーは最初の rootUri しか見ない)
    /// - 動的追加/削除の通知に対応していないサーバーでは無言で壊れる
    ///
    /// ため、確実に正しく動く「ルート毎に 1 プロセス」を採用した。
    /// トレードオフはルート数 × 言語数だけプロセスが増えること
    /// (実際にはそのルートでファイルを開いた言語のぶんだけ遅延起動される)。
    pub fn spawn(
        server_cmd: &str,
        root: &Path,
        ctx: eframe::egui::Context,
    ) -> Result<Self, String> {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        let mut child = Command::new(&shell)
            .arg("-lc")
            .arg(server_cmd)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("failed to spawn '{server_cmd}': {e}"))?;

        let stdin = child.stdin.take().ok_or("no stdin")?;
        let stdout = child.stdout.take().ok_or("no stdout")?;
        let stderr = child.stderr.take().ok_or("no stderr")?;

        let (tx, rx) = mpsc::channel::<Value>();
        let shared = Arc::new(Shared::new());

        // stderr 読み捨てスレッド (パイプ詰まり防止)
        std::thread::spawn(move || {
            let mut sink = stderr;
            let mut buf = [0u8; 4096];
            while matches!(sink.read(&mut buf), Ok(n) if n > 0) {}
        });

        // 書き込みスレッド (ChildStdin を専有。送信側はチャネルに積むだけ)
        {
            let shared = Arc::clone(&shared);
            std::thread::spawn(move || writer_loop(stdin, rx, shared));
        }

        // 受信スレッド
        {
            let shared = Arc::clone(&shared);
            let tx = tx.clone();
            std::thread::spawn(move || reader_loop(stdout, shared, tx, ctx));
        }

        let client = LspClient {
            child,
            tx,
            shared,
            next_id: AtomicU64::new(1),
            versions: Mutex::new(HashMap::new()),
        };

        // initialize リクエスト送信
        let root_canon = canonical(root);
        let root_uri = path_to_uri(&root_canon);
        let root_name = root_canon
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".into());
        let init_params = json!({
            "processId": std::process::id(),
            "rootUri": root_uri,
            "rootPath": root_canon.to_string_lossy(),
            "capabilities": {
                "textDocument": {
                    "synchronization": { "didSave": false },
                    "publishDiagnostics": { "relatedInformation": false },
                    "completion": { "completionItem": { "snippetSupport": false } },
                    "hover": { "contentFormat": ["plaintext", "markdown"] },
                    "definition": { "linkSupport": true }
                }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": root_name }]
        });
        let id = client.next_id.fetch_add(1, Ordering::SeqCst);
        lock_ok(&client.shared.pending).insert(id, Pending::Initialize);
        send_json(
            &client.tx,
            json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":init_params}),
        )
        .map_err(|e| format!("failed to send initialize: {e}"))?;

        // initialize 応答は待たない。受信スレッドが initialized 通知送信後に
        // init_done を立てるので、呼び出し側は is_ready で確認する。
        Ok(client)
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    /// initialize ハンドシェイク完了。false の間は LSP 機能は使えない (送信は保留すること)。
    pub fn is_ready(&self) -> bool {
        self.shared.init_done.load(Ordering::SeqCst)
    }

    pub fn did_open(&self, path: &Path, language_id: &str, text: &str) {
        let p = canonical(path);
        lock_ok(&self.versions).insert(p.clone(), 1);
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": path_to_uri(&p),
                    "languageId": language_id,
                    "version": 1,
                    "text": text
                }
            }),
        );
    }

    /// フル同期 (TextDocumentSyncKind.Full)。version 自動インクリメント。
    pub fn did_change(&self, path: &Path, text: &str) {
        let p = canonical(path);
        let version = {
            let mut versions = lock_ok(&self.versions);
            let v = versions.entry(p.clone()).or_insert(1);
            *v += 1;
            *v
        };
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": path_to_uri(&p), "version": version },
                "contentChanges": [{ "text": text }]
            }),
        );
    }

    pub fn did_close(&self, path: &Path) {
        let p = canonical(path);
        lock_ok(&self.versions).remove(&p);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": path_to_uri(&p) } }),
        );
    }

    /// 受信スレッドが貯めた最新の publishDiagnostics (パスごと)。
    /// ヒット時は `Arc` の clone のみで中身の `Vec` は複製しない
    /// (毎フレーム呼ばれるため。未受信のパスは None)。
    pub fn diagnostics(&self, path: &Path) -> Option<Arc<Vec<Diagnostic>>> {
        lock_ok(&self.shared.diags).get(&canonical(path)).cloned()
    }

    /// 非同期: 送信のみ。結果は poll_completion で取得。line/col は LSP (UTF-16) 座標。
    pub fn request_completion(&self, path: &Path, line: usize, col: usize) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        *lock_ok(&self.shared.completion) = None;
        self.shared.latest_completion.store(id, Ordering::SeqCst);
        lock_ok(&self.shared.pending).insert(id, Pending::Completion);
        let params = json!({
            "textDocument": { "uri": path_to_uri(&canonical(path)) },
            "position": { "line": line, "character": col }
        });
        self.request_raw(id, "textDocument/completion", params);
    }

    pub fn poll_completion(&self) -> Option<Vec<CompletionItem>> {
        lock_ok(&self.shared.completion).take()
    }

    pub fn request_hover(&self, path: &Path, line: usize, col: usize) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        *lock_ok(&self.shared.hover) = None;
        self.shared.latest_hover.store(id, Ordering::SeqCst);
        lock_ok(&self.shared.pending).insert(id, Pending::Hover);
        let params = json!({
            "textDocument": { "uri": path_to_uri(&canonical(path)) },
            "position": { "line": line, "character": col }
        });
        self.request_raw(id, "textDocument/hover", params);
    }

    pub fn poll_hover(&self) -> Option<HoverInfo> {
        lock_ok(&self.shared.hover).take()
    }

    /// 定義へ移動 (VS Code: F12)。応答は poll_definition で受け取る。
    pub fn request_definition(&self, path: &Path, line: usize, col: usize) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        *lock_ok(&self.shared.definition) = None;
        self.shared.latest_definition.store(id, Ordering::SeqCst);
        lock_ok(&self.shared.pending).insert(id, Pending::Definition);
        let params = json!({
            "textDocument": { "uri": path_to_uri(&canonical(path)) },
            "position": { "line": line, "character": col }
        });
        self.request_raw(id, "textDocument/definition", params);
    }

    /// 外側 Some = 応答あり (一度で消費)。内側 None = 定義が見つからなかった。
    pub fn poll_definition(&self) -> Option<Option<DefinitionLoc>> {
        lock_ok(&self.shared.definition).take()
    }

    /// shutdown/exit 送信 + kill。Drop でも kill される。
    pub fn shutdown(&mut self) {
        if self.is_alive() {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let _ = send_json(
                &self.tx,
                json!({"jsonrpc":"2.0","id":id,"method":"shutdown","params":null}),
            );
            let _ = send_json(
                &self.tx,
                json!({"jsonrpc":"2.0","method":"exit","params":null}),
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.shared.alive.store(false, Ordering::SeqCst);
    }

    fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        if send_json(&self.tx, msg).is_err() {
            self.shared.alive.store(false, Ordering::SeqCst);
        }
    }

    fn request_raw(&self, id: u64, method: &str, params: Value) {
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if send_json(&self.tx, msg).is_err() {
            self.shared.alive.store(false, Ordering::SeqCst);
        }
    }
}

impl Drop for LspClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.shared.alive.store(false, Ordering::SeqCst);
    }
}

// ---------------------------------------------------------------------------
// 受信スレッド
// ---------------------------------------------------------------------------

fn reader_loop(
    mut stdout: ChildStdout,
    shared: Arc<Shared>,
    tx: mpsc::Sender<Value>,
    ctx: eframe::egui::Context,
) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => {
                shared.alive.store(false, Ordering::SeqCst);
                // tx が drop され (LspClient 側と合わせて) writer_loop も終了する
                ctx.request_repaint();
                break;
            }
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Some(msg) = dec.next_message() {
                    handle_message(&msg, &shared, &tx);
                }
                ctx.request_repaint();
            }
        }
    }
}

fn handle_message(raw: &str, shared: &Arc<Shared>, tx: &mpsc::Sender<Value>) {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return,
    };
    let has_id = v.get("id").is_some();
    let method = v.get("method").and_then(|m| m.as_str());

    match (has_id, method) {
        // サーバ→クライアント リクエスト: 最小応答でストール防止
        (true, Some(m)) => {
            let id = v.get("id").cloned().unwrap_or(Value::Null);
            let result = if m == "workspace/configuration" {
                let n = v
                    .get("params")
                    .and_then(|p| p.get("items"))
                    .and_then(|i| i.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0);
                Value::Array(vec![Value::Null; n])
            } else {
                Value::Null
            };
            let _ = send_json(tx, json!({"jsonrpc":"2.0","id":id,"result":result}));
        }
        // 通知
        (false, Some("textDocument/publishDiagnostics")) => {
            if let Some(params) = v.get("params") {
                handle_publish_diagnostics(params, shared);
            }
        }
        (false, Some(_)) => {} // その他通知は無視
        // レスポンス: id で振り分け
        (true, None) => {
            let id = match v.get("id").and_then(|i| i.as_u64()) {
                Some(id) => id,
                None => return,
            };
            let kind = lock_ok(&shared.pending).remove(&id);
            let result = v.get("result").cloned().unwrap_or(Value::Null);
            match kind {
                Some(Pending::Initialize) => {
                    // initialized 通知を先にチャネルへ積んでからフラグを立てる (順序保証:
                    // is_ready を見てから送られる通知より必ず先にサーバーへ届く)
                    let _ = send_json(
                        tx,
                        json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
                    );
                    shared.init_done.store(true, Ordering::SeqCst);
                }
                Some(Pending::Completion) => {
                    if shared.latest_completion.load(Ordering::SeqCst) == id {
                        *lock_ok(&shared.completion) = Some(parse_completions(&result));
                    }
                }
                Some(Pending::Hover) => {
                    if shared.latest_hover.load(Ordering::SeqCst) == id {
                        let contents = result
                            .get("contents")
                            .map(hover_text)
                            .unwrap_or_default();
                        *lock_ok(&shared.hover) = Some(HoverInfo { contents });
                    }
                }
                Some(Pending::Definition)
                    if shared.latest_definition.load(Ordering::SeqCst) == id =>
                {
                    *lock_ok(&shared.definition) = Some(parse_definition(&result));
                }
                Some(Pending::Definition) | None => {}
            }
        }
        (false, None) => {}
    }
}

fn handle_publish_diagnostics(params: &Value, shared: &Arc<Shared>) {
    let uri = match params.get("uri").and_then(|u| u.as_str()) {
        Some(u) => u,
        None => return,
    };
    let path = canonical(&uri_to_path(uri));
    let diags: Vec<Diagnostic> = params
        .get("diagnostics")
        .and_then(|d| d.as_array())
        .map(|arr| arr.iter().filter_map(parse_diagnostic).collect())
        .unwrap_or_default();
    lock_ok(&shared.diags).insert(path, Arc::new(diags));
}

/// textDocument/definition の結果から先頭 1 件を取り出す。
/// 形式は Location | Location[] | LocationLink[] | null (LSP 仕様)。
fn parse_definition(result: &Value) -> Option<DefinitionLoc> {
    let first = if result.is_array() {
        result.as_array()?.first()?
    } else {
        result
    };
    // LocationLink (targetUri + targetSelectionRange) を先に試す
    let (uri, range) = if let Some(u) = first.get("targetUri").and_then(|u| u.as_str()) {
        let r = first
            .get("targetSelectionRange")
            .or_else(|| first.get("targetRange"))?;
        (u, r)
    } else {
        (
            first.get("uri").and_then(|u| u.as_str())?,
            first.get("range")?,
        )
    };
    let start = range.get("start")?;
    Some(DefinitionLoc {
        path: uri_to_path(uri),
        line: start.get("line").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
        col: start.get("character").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
    })
}

fn parse_diagnostic(v: &Value) -> Option<Diagnostic> {
    let range = v.get("range")?;
    let pos = |which: &str, field: &str| -> usize {
        range
            .get(which)
            .and_then(|p| p.get(field))
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as usize
    };
    Some(Diagnostic {
        line: pos("start", "line"),
        col: pos("start", "character"),
        end_line: pos("end", "line"),
        end_col: pos("end", "character"),
        severity: v.get("severity").and_then(|s| s.as_u64()).unwrap_or(1) as u8,
        message: v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

fn parse_completions(result: &Value) -> Vec<CompletionItem> {
    let empty = Vec::new();
    let items = if let Some(arr) = result.as_array() {
        arr
    } else if let Some(arr) = result.get("items").and_then(|i| i.as_array()) {
        arr
    } else {
        &empty
    };
    items
        .iter()
        .map(|it| {
            let label = it
                .get("label")
                .and_then(|l| l.as_str())
                .unwrap_or("")
                .to_string();
            let insert_text = it
                .get("textEdit")
                .and_then(|t| t.get("newText"))
                .and_then(|n| n.as_str())
                .or_else(|| it.get("insertText").and_then(|n| n.as_str()))
                .unwrap_or(label.as_str())
                .to_string();
            CompletionItem {
                insert_text,
                detail: it
                    .get("detail")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                kind: it.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u8,
                label,
            }
        })
        .collect()
}

/// Hover contents: string | MarkupContent | MarkedString | それらの配列
fn hover_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .map(hover_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(obj) => obj
            .get("value")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- parse_definition ----

    #[test]
    fn parse_definition_single_location() {
        let v = serde_json::json!({
            "uri": "file:///a/b.rs",
            "range": { "start": { "line": 3, "character": 7 }, "end": { "line": 3, "character": 9 } }
        });
        assert_eq!(
            parse_definition(&v),
            Some(DefinitionLoc { path: PathBuf::from("/a/b.rs"), line: 3, col: 7 })
        );
    }

    #[test]
    fn parse_definition_location_array_takes_first() {
        let v = serde_json::json!([
            { "uri": "file:///x.py", "range": { "start": { "line": 1, "character": 0 } } },
            { "uri": "file:///y.py", "range": { "start": { "line": 9, "character": 9 } } }
        ]);
        let got = parse_definition(&v).unwrap();
        assert_eq!(got.path, PathBuf::from("/x.py"));
        assert_eq!((got.line, got.col), (1, 0));
    }

    #[test]
    fn parse_definition_location_link_and_percent_decode() {
        let v = serde_json::json!([{
            "targetUri": "file:///dir%20name/f.ts",
            "targetRange": { "start": { "line": 5, "character": 2 } },
            "targetSelectionRange": { "start": { "line": 6, "character": 4 } }
        }]);
        let got = parse_definition(&v).unwrap();
        // targetSelectionRange を優先し、%20 はデコードされる
        assert_eq!(got.path, PathBuf::from("/dir name/f.ts"));
        assert_eq!((got.line, got.col), (6, 4));
    }

    #[test]
    fn parse_definition_null_or_empty_is_none() {
        assert_eq!(parse_definition(&Value::Null), None);
        assert_eq!(parse_definition(&serde_json::json!([])), None);
    }

    // ---- encode / FrameDecoder ----

    #[test]
    fn encode_basic() {
        assert_eq!(encode_message("{}"), b"Content-Length: 2\r\n\r\n{}".to_vec());
    }

    #[test]
    fn decoder_single_message() {
        let mut d = FrameDecoder::new();
        d.push(&encode_message(r#"{"a":1}"#));
        assert_eq!(d.next_message().as_deref(), Some(r#"{"a":1}"#));
        assert_eq!(d.next_message(), None);
    }

    #[test]
    fn decoder_split_arrival() {
        let mut d = FrameDecoder::new();
        let full = encode_message(r#"{"hello":"world"}"#);
        // 1バイトずつ到着しても最後まで完成しない
        for (i, b) in full.iter().enumerate() {
            d.push(&[*b]);
            if i + 1 < full.len() {
                assert_eq!(d.next_message(), None, "premature message at byte {}", i);
            }
        }
        assert_eq!(d.next_message().as_deref(), Some(r#"{"hello":"world"}"#));
    }

    #[test]
    fn decoder_two_messages_one_push() {
        let mut d = FrameDecoder::new();
        let mut bytes = encode_message(r#"{"m":1}"#);
        bytes.extend_from_slice(&encode_message(r#"{"m":2}"#));
        d.push(&bytes);
        assert_eq!(d.next_message().as_deref(), Some(r#"{"m":1}"#));
        assert_eq!(d.next_message().as_deref(), Some(r#"{"m":2}"#));
        assert_eq!(d.next_message(), None);
    }

    #[test]
    fn decoder_multiple_header_lines() {
        let mut d = FrameDecoder::new();
        d.push(
            b"Content-Type: application/vscode-jsonrpc; charset=utf-8\r\ncontent-length: 2\r\n\r\n{}",
        );
        assert_eq!(d.next_message().as_deref(), Some("{}"));
    }

    #[test]
    fn decoder_multibyte_body_byte_length() {
        // Content-Length はバイト長 (日本語 UTF-8 は 3 bytes/char)
        let body = r#"{"msg":"こんにちは"}"#;
        let mut d = FrameDecoder::new();
        d.push(&encode_message(body));
        assert_eq!(d.next_message().as_deref(), Some(body));
        assert_eq!(d.next_message(), None);
    }

    // ---- UTF-16 位置変換 ----

    #[test]
    fn utf16_ascii() {
        let t = "hello\nworld";
        assert_eq!(char_index_to_lsp_pos(t, 7), (1, 1));
        assert_eq!(lsp_pos_to_char_index(t, 1, 1), 7);
        assert_eq!(char_index_to_lsp_pos(t, 0), (0, 0));
        assert_eq!(lsp_pos_to_char_index(t, 0, 0), 0);
    }

    #[test]
    fn utf16_japanese() {
        // 日本語 1 文字 = UTF-16 で 1 code unit (BMP 内)
        let t = "こんにちは\n世界";
        assert_eq!(char_index_to_lsp_pos(t, 3), (0, 3)); // "ち"
        assert_eq!(char_index_to_lsp_pos(t, 6), (1, 0)); // "世"
        assert_eq!(char_index_to_lsp_pos(t, 7), (1, 1)); // "界"
        assert_eq!(lsp_pos_to_char_index(t, 0, 3), 3);
        assert_eq!(lsp_pos_to_char_index(t, 1, 0), 6);
        assert_eq!(lsp_pos_to_char_index(t, 1, 1), 7);
    }

    #[test]
    fn utf16_emoji_surrogate_pair() {
        // 😀 は UTF-16 でサロゲートペア = 2 code units
        let t = "a😀b";
        assert_eq!(char_index_to_lsp_pos(t, 1), (0, 1)); // 😀 の前
        assert_eq!(char_index_to_lsp_pos(t, 2), (0, 3)); // 'b' は col 1+2=3
        assert_eq!(lsp_pos_to_char_index(t, 0, 1), 1);
        assert_eq!(lsp_pos_to_char_index(t, 0, 3), 2);
    }

    #[test]
    fn utf16_line_boundaries() {
        let t = "ab\nかき";
        assert_eq!(char_index_to_lsp_pos(t, 2), (0, 2)); // 行末 ('\n' の位置)
        assert_eq!(char_index_to_lsp_pos(t, 3), (1, 0)); // 次行頭
        assert_eq!(char_index_to_lsp_pos(t, 5), (1, 2)); // テキスト末尾
        assert_eq!(lsp_pos_to_char_index(t, 0, 2), 2);
        assert_eq!(lsp_pos_to_char_index(t, 1, 0), 3);
        assert_eq!(lsp_pos_to_char_index(t, 1, 2), 5);
    }

    #[test]
    fn utf16_roundtrip_mixed() {
        let t = "fn main() {\n    let 変数 = \"😀テスト\";\n}\n";
        for idx in 0..=t.chars().count() {
            let (line, col) = char_index_to_lsp_pos(t, idx);
            assert_eq!(
                lsp_pos_to_char_index(t, line, col),
                idx,
                "roundtrip failed at char {}",
                idx
            );
        }
    }

    #[test]
    fn utf16_clamp_and_empty() {
        assert_eq!(char_index_to_lsp_pos("", 5), (0, 0));
        assert_eq!(lsp_pos_to_char_index("", 3, 3), 0);
        let t = "あい\nうえ";
        // 行末を超える col は行末へクランプ
        assert_eq!(lsp_pos_to_char_index(t, 0, 99), 2);
        // 存在しない行はテキスト末尾へ
        assert_eq!(lsp_pos_to_char_index(t, 9, 0), 5);
        // 末尾を超える char index は最終位置
        assert_eq!(char_index_to_lsp_pos(t, 99), (1, 2));
    }

    // ---- 書き込みスレッド / initialize フラグ ----

    #[test]
    fn writer_loop_encodes_messages_in_order() {
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::new());
        send_json(&tx, json!({"a":1})).unwrap();
        send_json(&tx, json!({"b":2})).unwrap();
        drop(tx); // チャネル切断で writer_loop が終了する
        let mut out: Vec<u8> = Vec::new();
        writer_loop(&mut out, rx, Arc::clone(&shared));
        let mut expected = encode_message(&json!({"a":1}).to_string());
        expected.extend_from_slice(&encode_message(&json!({"b":2}).to_string()));
        assert_eq!(out, expected);
        assert!(shared.alive.load(Ordering::SeqCst));
    }

    #[test]
    fn writer_loop_exits_on_write_error_without_hanging() {
        struct FailWriter;
        impl Write for FailWriter {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "stuck"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let (tx, rx) = mpsc::channel();
        let shared = Arc::new(Shared::new());
        send_json(&tx, json!({"x":true})).unwrap();
        // Sender が生きていても書き込み失敗で戻る (recv で永久待ちしない)
        writer_loop(FailWriter, rx, Arc::clone(&shared));
        assert!(!shared.alive.load(Ordering::SeqCst));
        drop(tx);
    }

    #[test]
    fn initialize_response_flips_ready_and_queues_initialized() {
        let shared = Arc::new(Shared::new());
        shared.pending.lock().unwrap().insert(1, Pending::Initialize);
        let (tx, rx) = mpsc::channel();
        assert!(!shared.init_done.load(Ordering::SeqCst));
        handle_message(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#, &shared, &tx);
        assert!(shared.init_done.load(Ordering::SeqCst));
        let v = rx.try_recv().expect("initialized notification queued");
        assert_eq!(
            v.get("method").and_then(|m| m.as_str()),
            Some("initialized")
        );
    }

    // ---- 統合スモーク: rust-analyzer ----

    #[test]
    fn smoke_rust_analyzer() {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
        // which だけでは rustup シム (実体未インストール) を誤検出するため実行可否まで確認
        let found = Command::new(&shell)
            .arg("-lc")
            .arg("which rust-analyzer >/dev/null 2>&1 && rust-analyzer --version >/dev/null 2>&1")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !found {
            eprintln!("smoke: rust-analyzer not found, skipping");
            return;
        }
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let ctx = eframe::egui::Context::default();
        let mut client = match LspClient::spawn("rust-analyzer", &root, ctx) {
            Ok(c) => c,
            Err(e) => panic!("spawn failed: {e}"),
        };
        // spawn は待たずに返るので、テスト側で initialize 完了をポーリングする
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !client.is_ready() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(client.is_ready(), "initialize should complete within 20s");
        assert!(client.is_alive(), "client should be alive after initialize");
        let main_rs = root.join("src").join("main.rs");
        let text = std::fs::read_to_string(&main_rs).expect("read src/main.rs");
        client.did_open(&main_rs, "rust", &text);
        std::thread::sleep(Duration::from_secs(3));
        let diags = client.diagnostics(&main_rs); // panic しないこと
        eprintln!(
            "smoke: {} diagnostics after 3s",
            diags.map_or(0, |d| d.len())
        );
        client.shutdown();
        assert!(!client.is_alive());
    }
}
