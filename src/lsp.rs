//! 最小 LSP クライアント (stdio JSON-RPC)。
//! std::thread + mpsc レス設計: 受信スレッドが共有状態(Mutex)へ書き込み、UI は poll で取得。
//! UI 依存は egui Context の request_repaint 通知のみ。

#![allow(dead_code)]

use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
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
}

struct Shared {
    alive: AtomicBool,
    init_done: Mutex<bool>,
    init_cv: Condvar,
    diags: Mutex<HashMap<PathBuf, Vec<Diagnostic>>>,
    pending: Mutex<HashMap<u64, Pending>>,
    completion: Mutex<Option<Vec<CompletionItem>>>,
    hover: Mutex<Option<HoverInfo>>,
    latest_completion: AtomicU64,
    latest_hover: AtomicU64,
}

pub struct LspClient {
    child: Child,
    writer: Arc<Mutex<ChildStdin>>,
    shared: Arc<Shared>,
    next_id: AtomicU64,
    versions: Mutex<HashMap<PathBuf, i64>>,
}

fn send_json(writer: &Mutex<ChildStdin>, v: &Value) -> std::io::Result<()> {
    let bytes = encode_message(&v.to_string());
    let mut w = writer.lock().unwrap();
    w.write_all(&bytes)?;
    w.flush()
}

impl LspClient {
    /// server_cmd は $SHELL -lc 経由で起動 (PATH 解決のため)。initialize→initialized まで行う。
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

        let writer = Arc::new(Mutex::new(stdin));
        let shared = Arc::new(Shared {
            alive: AtomicBool::new(true),
            init_done: Mutex::new(false),
            init_cv: Condvar::new(),
            diags: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            completion: Mutex::new(None),
            hover: Mutex::new(None),
            latest_completion: AtomicU64::new(0),
            latest_hover: AtomicU64::new(0),
        });

        // stderr 読み捨てスレッド (パイプ詰まり防止)
        std::thread::spawn(move || {
            let mut sink = stderr;
            let mut buf = [0u8; 4096];
            while matches!(sink.read(&mut buf), Ok(n) if n > 0) {}
        });

        // 受信スレッド
        {
            let shared = Arc::clone(&shared);
            let writer = Arc::clone(&writer);
            std::thread::spawn(move || reader_loop(stdout, shared, writer, ctx));
        }

        let client = LspClient {
            child,
            writer,
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
                    "hover": { "contentFormat": ["plaintext", "markdown"] }
                }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": root_name }]
        });
        let id = client.next_id.fetch_add(1, Ordering::SeqCst);
        client
            .shared
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Initialize);
        send_json(
            &client.writer,
            &json!({"jsonrpc":"2.0","id":id,"method":"initialize","params":init_params}),
        )
        .map_err(|e| format!("failed to send initialize: {e}"))?;

        // initialize 応答待ち (受信スレッドが initialized 通知送信後にフラグを立てる)
        let done = client.shared.init_done.lock().unwrap();
        let (done, timeout) = client
            .shared
            .init_cv
            .wait_timeout_while(done, Duration::from_secs(20), |d| !*d)
            .unwrap();
        drop(done);
        if timeout.timed_out() {
            let mut client = client;
            let _ = client.child.kill();
            let _ = client.child.wait();
            client.shared.alive.store(false, Ordering::SeqCst);
            return Err("LSP initialize timed out".into());
        }
        Ok(client)
    }

    pub fn is_alive(&self) -> bool {
        self.shared.alive.load(Ordering::SeqCst)
    }

    pub fn did_open(&self, path: &Path, language_id: &str, text: &str) {
        let p = canonical(path);
        self.versions.lock().unwrap().insert(p.clone(), 1);
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
            let mut versions = self.versions.lock().unwrap();
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
        self.versions.lock().unwrap().remove(&p);
        self.notify(
            "textDocument/didClose",
            json!({ "textDocument": { "uri": path_to_uri(&p) } }),
        );
    }

    /// 受信スレッドが貯めた最新の publishDiagnostics (パスごと)
    pub fn diagnostics(&self, path: &Path) -> Vec<Diagnostic> {
        self.shared
            .diags
            .lock()
            .unwrap()
            .get(&canonical(path))
            .cloned()
            .unwrap_or_default()
    }

    /// 非同期: 送信のみ。結果は poll_completion で取得。line/col は LSP (UTF-16) 座標。
    pub fn request_completion(&self, path: &Path, line: usize, col: usize) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        *self.shared.completion.lock().unwrap() = None;
        self.shared.latest_completion.store(id, Ordering::SeqCst);
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Completion);
        let params = json!({
            "textDocument": { "uri": path_to_uri(&canonical(path)) },
            "position": { "line": line, "character": col }
        });
        self.request_raw(id, "textDocument/completion", params);
    }

    pub fn poll_completion(&self) -> Option<Vec<CompletionItem>> {
        self.shared.completion.lock().unwrap().take()
    }

    pub fn request_hover(&self, path: &Path, line: usize, col: usize) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        *self.shared.hover.lock().unwrap() = None;
        self.shared.latest_hover.store(id, Ordering::SeqCst);
        self.shared
            .pending
            .lock()
            .unwrap()
            .insert(id, Pending::Hover);
        let params = json!({
            "textDocument": { "uri": path_to_uri(&canonical(path)) },
            "position": { "line": line, "character": col }
        });
        self.request_raw(id, "textDocument/hover", params);
    }

    pub fn poll_hover(&self) -> Option<HoverInfo> {
        self.shared.hover.lock().unwrap().take()
    }

    /// shutdown/exit 送信 + kill。Drop でも kill される。
    pub fn shutdown(&mut self) {
        if self.is_alive() {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let _ = send_json(
                &self.writer,
                &json!({"jsonrpc":"2.0","id":id,"method":"shutdown","params":null}),
            );
            let _ = send_json(
                &self.writer,
                &json!({"jsonrpc":"2.0","method":"exit","params":null}),
            );
            std::thread::sleep(Duration::from_millis(100));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        self.shared.alive.store(false, Ordering::SeqCst);
    }

    fn notify(&self, method: &str, params: Value) {
        let msg = json!({"jsonrpc":"2.0","method":method,"params":params});
        if send_json(&self.writer, &msg).is_err() {
            self.shared.alive.store(false, Ordering::SeqCst);
        }
    }

    fn request_raw(&self, id: u64, method: &str, params: Value) {
        let msg = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        if send_json(&self.writer, &msg).is_err() {
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
    writer: Arc<Mutex<ChildStdin>>,
    ctx: eframe::egui::Context,
) {
    let mut dec = FrameDecoder::new();
    let mut buf = [0u8; 8192];
    loop {
        match stdout.read(&mut buf) {
            Ok(0) | Err(_) => {
                shared.alive.store(false, Ordering::SeqCst);
                // spawn が initialize 待ちで停止しないよう起こす
                shared.init_cv.notify_all();
                ctx.request_repaint();
                break;
            }
            Ok(n) => {
                dec.push(&buf[..n]);
                while let Some(msg) = dec.next_message() {
                    handle_message(&msg, &shared, &writer);
                }
                ctx.request_repaint();
            }
        }
    }
}

fn handle_message(raw: &str, shared: &Arc<Shared>, writer: &Mutex<ChildStdin>) {
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
            let _ = send_json(writer, &json!({"jsonrpc":"2.0","id":id,"result":result}));
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
            let kind = shared.pending.lock().unwrap().remove(&id);
            let result = v.get("result").cloned().unwrap_or(Value::Null);
            match kind {
                Some(Pending::Initialize) => {
                    // initialized 通知を先に送ってからフラグを立てる (順序保証)
                    let _ = send_json(
                        writer,
                        &json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
                    );
                    *shared.init_done.lock().unwrap() = true;
                    shared.init_cv.notify_all();
                }
                Some(Pending::Completion) => {
                    if shared.latest_completion.load(Ordering::SeqCst) == id {
                        *shared.completion.lock().unwrap() = Some(parse_completions(&result));
                    }
                }
                Some(Pending::Hover) => {
                    if shared.latest_hover.load(Ordering::SeqCst) == id {
                        let contents = result
                            .get("contents")
                            .map(hover_text)
                            .unwrap_or_default();
                        *shared.hover.lock().unwrap() = Some(HoverInfo { contents });
                    }
                }
                None => {}
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
    shared.diags.lock().unwrap().insert(path, diags);
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
        assert!(client.is_alive(), "client should be alive after initialize");
        let main_rs = root.join("src").join("main.rs");
        let text = std::fs::read_to_string(&main_rs).expect("read src/main.rs");
        client.did_open(&main_rs, "rust", &text);
        std::thread::sleep(Duration::from_secs(3));
        let diags = client.diagnostics(&main_rs); // panic しないこと
        eprintln!("smoke: {} diagnostics after 3s", diags.len());
        client.shutdown();
        assert!(!client.is_alive());
    }
}
