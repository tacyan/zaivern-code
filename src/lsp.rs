//! 最小 LSP クライアント (stdio JSON-RPC)。
//! std::thread + mpsc レス設計: 受信スレッドが共有状態(Mutex)へ書き込み、UI は poll で取得。
//! UI 依存は egui Context の request_repaint 通知のみ。

#![allow(dead_code)]

use crate::lockx::lock_ok;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Stdio};
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

/// LSP の位置。`character` は **UTF-16 code unit** 数 (仕様の既定 PositionEncodingKind)。
/// byte でも char でもないので、日本語・絵文字を含む行では必ず
/// [`lsp_pos_to_byte_index`] / [`byte_index_to_lsp_pos`] を通して変換すること。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Position {
    pub line: usize,
    pub character: usize,
}

impl Position {
    pub fn new(line: usize, character: usize) -> Self {
        Position { line, character }
    }
}

/// LSP の範囲 (start は含む / end は含まない)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub fn new(start: Position, end: Position) -> Self {
        Range { start, end }
    }
    /// 1 点だけを指す空範囲 (挿入位置)。
    pub fn empty_at(pos: Position) -> Self {
        Range {
            start: pos,
            end: pos,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }
    /// `other` を完全に含むか (ドキュメントシンボルの入れ子判定に使う)。
    pub fn contains_range(&self, other: &Range) -> bool {
        self.start <= other.start && other.end <= self.end
    }
}

/// LSP の TextEdit。range は UTF-16 座標のまま保持し、適用時に byte へ落とす。
#[derive(Debug, Clone, PartialEq)]
pub struct TextEdit {
    pub range: Range,
    pub new_text: String,
}

impl TextEdit {
    pub fn new(range: Range, new_text: impl Into<String>) -> Self {
        TextEdit {
            range,
            new_text: new_text.into(),
        }
    }
}

/// 補完候補。LSP の CompletionItem のうち、エディタ側が実際に使うフィールドだけを持つ。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompletionItem {
    pub label: String,
    /// textEdit が無い候補の挿入文字列 (insertText → label の順にフォールバック)。
    pub insert_text: String,
    pub detail: String,
    /// documentation (string | MarkupContent) を markdown 化したもの。
    pub documentation: String,
    pub kind: u8,
    /// サーバー指定の置換範囲。ある場合はこちらが insert_text より優先される。
    pub text_edit: Option<TextEdit>,
    /// 自動 import などの副次編集。**同時に**適用しなければ壊れる。
    pub additional_text_edits: Vec<TextEdit>,
    /// 並び替えキー (無ければ label)。
    pub sort_text: Option<String>,
    /// 絞り込みキー (無ければ label)。
    pub filter_text: Option<String>,
    pub preselect: bool,
    /// true = スニペット構文 ($1, ${2:x})。展開は呼び出し側の責務。
    pub is_snippet: bool,
    /// deprecated / obsolete マーク (tags に 1 が入っている場合も含む)。
    pub deprecated: bool,
}

impl CompletionItem {
    /// 絞り込みに使う文字列。
    pub fn filter_key(&self) -> &str {
        self.filter_text.as_deref().unwrap_or(&self.label)
    }
    /// 並び替えに使う文字列。
    pub fn sort_key(&self) -> &str {
        self.sort_text.as_deref().unwrap_or(&self.label)
    }
}

/// textDocument/completion の応答全体。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CompletionList {
    /// true = まだ候補が絞りきれていない。入力が進んだら再要求すること。
    pub is_incomplete: bool,
    pub items: Vec<CompletionItem>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct HoverInfo {
    /// MarkedString / MarkupContent を markdown へ正規化した本文。
    pub contents: String,
    /// サーバーが返した対象範囲 (ハイライト用。無ければ None)。
    pub range: Option<Range>,
}

/// 1 箇所の参照/定義 (references, documentHighlight, definition 共通)。
#[derive(Debug, Clone, PartialEq)]
pub struct Location {
    pub path: PathBuf,
    pub range: Range,
}

/// 参照結果をファイル単位にまとめたもの (結果パネル向け)。
#[derive(Debug, Clone, PartialEq)]
pub struct ReferenceGroup {
    pub path: PathBuf,
    /// 出現順ではなく **位置の昇順**に整列済み。
    pub locations: Vec<Range>,
}

/// documentHighlight の 1 件。kind: 1=Text 2=Read 3=Write。
#[derive(Debug, Clone, PartialEq)]
pub struct DocumentHighlight {
    pub range: Range,
    pub kind: u8,
}

/// 1 ファイル分の編集。`edits` は **後ろから適用できる順** (開始位置の降順) に整列済み。
#[derive(Debug, Clone, PartialEq)]
pub struct FileEdits {
    pub path: PathBuf,
    pub edits: Vec<TextEdit>,
}

/// WorkspaceEdit を「ファイル毎の編集リスト」へ正規化したもの。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct WorkspaceEditPlan {
    pub files: Vec<FileEdits>,
    /// create/rename/delete file 操作が含まれていた (本エディタでは未対応なので警告用)。
    pub has_resource_ops: bool,
}

impl WorkspaceEditPlan {
    pub fn is_empty(&self) -> bool {
        self.files.iter().all(|f| f.edits.is_empty())
    }
    pub fn edit_count(&self) -> usize {
        self.files.iter().map(|f| f.edits.len()).sum()
    }
}

/// documentSymbol を階層 1 本に正規化した木。
/// フラットな SymbolInformation[] も範囲の包含関係で同じ形に組み直す。
#[derive(Debug, Clone, PartialEq)]
pub struct SymbolNode {
    pub name: String,
    pub detail: String,
    /// SymbolKind (1=File 5=Class 6=Method 12=Function ...)
    pub kind: u8,
    /// 本体全体の範囲。
    pub range: Range,
    /// 名前だけの範囲 (ジャンプ先)。
    pub selection_range: Range,
    pub deprecated: bool,
    pub children: Vec<SymbolNode>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct ParameterInfo {
    pub label: String,
    pub documentation: String,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SignatureInfo {
    pub label: String,
    pub documentation: String,
    pub parameters: Vec<ParameterInfo>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct SignatureHelp {
    pub signatures: Vec<SignatureInfo>,
    pub active_signature: usize,
    pub active_parameter: Option<usize>,
}

/// codeAction の 1 件 (Command 形式も CodeAction 形式もここへ寄せる)。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CodeAction {
    pub title: String,
    /// "quickfix" / "refactor.extract" など。無指定は空文字。
    pub kind: String,
    pub is_preferred: bool,
    /// その場で適用できる編集 (無ければ command 実行が必要)。
    pub edit: WorkspaceEditPlan,
    /// workspace/executeCommand へ渡すコマンド。
    pub command: Option<CommandRef>,
    /// サーバーが「解決前」を返した場合 true (codeAction/resolve が要る)。
    pub needs_resolve: bool,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CommandRef {
    pub title: String,
    pub command: String,
    pub arguments: Vec<Value>,
}

/// textDocument/formatting のオプション (FormattingOptions)。
#[derive(Debug, Clone, PartialEq)]
pub struct FormatOptions {
    pub tab_size: u32,
    pub insert_spaces: bool,
    pub trim_trailing_whitespace: bool,
    pub insert_final_newline: bool,
    pub trim_final_newlines: bool,
}

impl Default for FormatOptions {
    fn default() -> Self {
        FormatOptions {
            tab_size: 4,
            insert_spaces: true,
            trim_trailing_whitespace: true,
            insert_final_newline: true,
            trim_final_newlines: true,
        }
    }
}

impl FormatOptions {
    fn to_json(&self) -> Value {
        json!({
            "tabSize": self.tab_size,
            "insertSpaces": self.insert_spaces,
            "trimTrailingWhitespace": self.trim_trailing_whitespace,
            "insertFinalNewline": self.insert_final_newline,
            "trimFinalNewlines": self.trim_final_newlines,
        })
    }
}

/// リクエスト送信の結果。**能力未対応は「エラー」ではなく no-op** として返す
/// (VS Code と同じで、対応していないサーバーでもメニューが壊れないため)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestStatus {
    /// 送信した。この id の応答を poll_* で待つ。
    Sent(u64),
    /// サーバーがこの機能を advertise していない。何も起きない。
    Unsupported,
    /// initialize 未完了。少し待って再試行してよい。
    NotReady,
    /// サーバーが死んでいる。再起動 ([`RestartPolicy`]) の判断は呼び出し側。
    Dead,
}

impl RequestStatus {
    pub fn is_sent(&self) -> bool {
        matches!(self, RequestStatus::Sent(_))
    }
    pub fn id(&self) -> Option<u64> {
        match self {
            RequestStatus::Sent(id) => Some(*id),
            _ => None,
        }
    }
}

/// initialize 応答の serverCapabilities から、使う機能だけ抜き出したもの。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ServerCaps {
    pub completion: bool,
    /// サーバーが宣言した補完トリガ文字 ('.' や '::' の先頭文字など)。
    pub completion_trigger_chars: Vec<char>,
    pub hover: bool,
    pub definition: bool,
    pub references: bool,
    pub document_highlight: bool,
    pub rename: bool,
    /// renameProvider.prepareProvider。false なら prepareRename は送らない。
    pub prepare_rename: bool,
    pub formatting: bool,
    pub range_formatting: bool,
    pub signature_help: bool,
    pub signature_trigger_chars: Vec<char>,
    pub code_action: bool,
    pub document_symbol: bool,
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

/// (line, utf16 col) → **byte** index。TextEdit の適用はこちらを使う。
///
/// クランプ規則 (すべてテスト済み):
/// * 行が足りない → テキスト末尾 (`text.len()`)
/// * col が行末を超える → その行の改行の直前
/// * col がサロゲートペア/合字の**途中**を指す → その文字の**先頭**へ丸める
///   (byte index が必ず char 境界になり、`String::replace_range` が panic しない)
pub fn lsp_pos_to_byte_index(text: &str, pos: Position) -> usize {
    let mut line = 0usize;
    let mut line_start = 0usize;
    // 目的の行の先頭 byte を探す
    if pos.line > 0 {
        let mut found = false;
        for (b, ch) in text.char_indices() {
            if ch == '\n' {
                line += 1;
                if line == pos.line {
                    line_start = b + 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            return text.len(); // 行が足りない
        }
    }
    let rest = &text[line_start..];
    let line_str = match rest.find('\n') {
        Some(n) => &rest[..n],
        None => rest,
    };
    let mut col16 = 0usize;
    for (b, ch) in line_str.char_indices() {
        // 等しいときも「文字の先頭」を返す。途中を指すときも同じ枝で先頭へ丸まる。
        if col16 + ch.len_utf16() > pos.character {
            return line_start + b;
        }
        col16 += ch.len_utf16();
    }
    line_start + line_str.len()
}

/// **byte** index → (line, utf16 col)。char 境界でない index は直前の文字境界として扱う。
pub fn byte_index_to_lsp_pos(text: &str, byte_idx: usize) -> Position {
    let mut line = 0usize;
    let mut col16 = 0usize;
    for (b, ch) in text.char_indices() {
        if b >= byte_idx {
            return Position::new(line, col16);
        }
        if ch == '\n' {
            line += 1;
            col16 = 0;
        } else {
            col16 += ch.len_utf16();
        }
    }
    Position::new(line, col16)
}

/// Range → byte 範囲。start > end のサーバーバグにも耐える (入れ替える)。
pub fn range_to_byte_span(text: &str, range: &Range) -> (usize, usize) {
    let a = lsp_pos_to_byte_index(text, range.start);
    let b = lsp_pos_to_byte_index(text, range.end);
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

/// TextEdit 群をテキストへ適用する。
///
/// LSP 仕様では範囲は重複しない前提で、**同じ位置への複数挿入は配列の順に**
/// 並ぶことになっている。そのため (1) 開始位置の昇順に安定ソート (同位置は
/// 元の配列順を保つ) → (2) **後ろから**適用する (前方の byte index がずれない)
/// の順で処理する。仕様違反の重なりが来た場合は直前に適用した位置で切り詰めて
/// panic を避ける (壊れた編集より、切り詰めた編集の方がまだ復帰できる)。
pub fn apply_text_edits(text: &str, edits: &[TextEdit]) -> String {
    let mut spans: Vec<(usize, usize, &str)> = edits
        .iter()
        .map(|e| {
            let (s, t) = range_to_byte_span(text, &e.range);
            (s, t, e.new_text.as_str())
        })
        .collect();
    // 安定ソート: 同じ (start,end) は元の順序のまま = 逆順適用で配列順に並ぶ
    spans.sort_by_key(|(s, e, _)| (*s, *e));
    let mut out = text.to_string();
    let mut limit = out.len();
    for (s, e, new_text) in spans.into_iter().rev() {
        let s = s.min(limit);
        let e = e.clamp(s, limit);
        out.replace_range(s..e, new_text);
        limit = s;
    }
    out
}

/// [`WorkspaceEditPlan`] の 1 ファイル分をテキストへ適用する。
pub fn apply_file_edits(text: &str, file: &FileEdits) -> String {
    apply_text_edits(text, &file.edits)
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

/// URI の素材にするパス。エディタのバッファ側 (`editor::Editor::open`) と
/// 同じ形 (Windows なら `\\?\` を外した素のパス) に揃えないと、
/// サーバーへ送った URI と手元のパスが一致しなくなる。
fn canonical(path: &Path) -> PathBuf {
    crate::pathx::canonical(path)
}

// ---------------------------------------------------------------------------
// LspClient
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Pending {
    Initialize,
    Completion,
    Hover,
    Definition,
    References,
    Highlight,
    PrepareRename,
    Rename,
    Formatting,
    Symbols,
    Signature,
    CodeAction,
}

/// 送信済みリクエストの控え。`at` はタイムアウト掃除 ([`LspClient::sweep_timeouts`]) 用。
#[derive(Debug, Clone, Copy)]
struct PendingEntry {
    kind: Pending,
    at: std::time::Instant,
}

/// 「最新のリクエスト id」と「その応答」の受け渡し箱。
///
/// * `begin(id)` で待機開始 (前の結果は捨てる)
/// * `fulfill(id, v)` は **id が最新のときだけ**書き込む → 古い応答は自動で破棄
/// * `abandon()` はタイムアウト/サーバー死亡時に「空の結果」を置いて UI の待ちを解く
///   (UI からは「結果 0 件」と同じに見える。エラー表示のために別経路は作らない)
struct Slot<T> {
    latest: AtomicU64,
    value: Mutex<Option<T>>,
}

impl<T: Default> Slot<T> {
    fn new() -> Self {
        Slot {
            latest: AtomicU64::new(0),
            value: Mutex::new(None),
        }
    }
    fn begin(&self, id: u64) {
        self.latest.store(id, Ordering::SeqCst);
        *lock_ok(&self.value) = None;
    }
    fn fulfill(&self, id: u64, v: T) {
        if self.latest.load(Ordering::SeqCst) == id {
            *lock_ok(&self.value) = Some(v);
        }
    }
    fn take(&self) -> Option<T> {
        lock_ok(&self.value).take()
    }
    /// 待機中なら空の結果で打ち切る。待機していなければ何もしない。
    fn abandon(&self) {
        if self.latest.swap(0, Ordering::SeqCst) != 0 {
            *lock_ok(&self.value) = Some(T::default());
        }
    }
    /// 応答を待たずに取り下げる (UI 側のキャンセル)。結果は置かない。
    fn cancel(&self) {
        self.latest.store(0, Ordering::SeqCst);
        *lock_ok(&self.value) = None;
    }
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
    /// initialize 応答から抜き出したサーバー能力。未受信の間は全 false。
    caps: Mutex<ServerCaps>,
    diags: Mutex<HashMap<PathBuf, Arc<Vec<Diagnostic>>>>,
    pending: Mutex<HashMap<u64, PendingEntry>>,
    /// タイムアウト/サーバー死亡で打ち切ったリクエスト数 (診断用)。
    abandoned: AtomicU64,
    completion: Slot<CompletionList>,
    hover: Slot<HoverInfo>,
    /// 外側 Some = 応答受信済み。内側 None = 定義が見つからなかった。
    definition: Slot<Option<DefinitionLoc>>,
    references: Slot<Vec<ReferenceGroup>>,
    highlight: Slot<Vec<DocumentHighlight>>,
    /// 外側 Some = 応答受信済み。内側 None = ここでは rename できない。
    prepare_rename: Slot<Option<Range>>,
    rename: Slot<WorkspaceEditPlan>,
    formatting: Slot<Vec<TextEdit>>,
    symbols: Slot<Vec<SymbolNode>>,
    signature: Slot<SignatureHelp>,
    code_action: Slot<Vec<CodeAction>>,
}

impl Shared {
    fn new() -> Self {
        Shared {
            alive: AtomicBool::new(true),
            init_done: AtomicBool::new(false),
            caps: Mutex::new(ServerCaps::default()),
            diags: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            abandoned: AtomicU64::new(0),
            completion: Slot::new(),
            hover: Slot::new(),
            definition: Slot::new(),
            references: Slot::new(),
            highlight: Slot::new(),
            prepare_rename: Slot::new(),
            rename: Slot::new(),
            formatting: Slot::new(),
            symbols: Slot::new(),
            signature: Slot::new(),
            code_action: Slot::new(),
        }
    }

    /// 送信したリクエストを控える (タイムアウト掃除の対象になる)。
    fn remember(&self, id: u64, kind: Pending) {
        lock_ok(&self.pending).insert(
            id,
            PendingEntry {
                kind,
                at: std::time::Instant::now(),
            },
        );
    }

    /// 指定種別の待機を「空の結果」で打ち切る。
    fn abandon(&self, kind: Pending) {
        match kind {
            Pending::Initialize => {}
            Pending::Completion => self.completion.abandon(),
            Pending::Hover => self.hover.abandon(),
            Pending::Definition => self.definition.abandon(),
            Pending::References => self.references.abandon(),
            Pending::Highlight => self.highlight.abandon(),
            Pending::PrepareRename => self.prepare_rename.abandon(),
            Pending::Rename => self.rename.abandon(),
            Pending::Formatting => self.formatting.abandon(),
            Pending::Symbols => self.symbols.abandon(),
            Pending::Signature => self.signature.abandon(),
            Pending::CodeAction => self.code_action.abandon(),
        }
        self.abandoned.fetch_add(1, Ordering::SeqCst);
    }

    /// 未応答のリクエストを全部打ち切る (サーバー死亡時)。
    fn abandon_all(&self) {
        let kinds: Vec<Pending> = {
            let mut p = lock_ok(&self.pending);
            let ks = p.values().map(|e| e.kind).collect();
            p.clear();
            ks
        };
        for k in kinds {
            self.abandon(k);
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
    /// server_cmd は [`crate::shellenv::shell_command`] 経由で起動する
    /// (引数付きのコマンド行をそのまま扱うため。PATH の補正も込み)。initialize は送信だけして
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
        // シェル経由で起動する (`typescript-language-server --stdio` のように
        // 引数付きのコマンド行をそのまま扱えるため)。呼び出し規約は OS で違うので
        // shellenv に任せる — Windows で `-lc` を渡すと cmd.exe が何も実行しない。
        let mut child = crate::shellenv::shell_command(server_cmd)
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
                    "completion": {
                        "completionItem": {
                            // スニペット展開は未実装。true にすると "$0" が本文に混ざる
                            "snippetSupport": false,
                            "documentationFormat": ["markdown", "plaintext"],
                            "deprecatedSupport": true,
                            "preselectSupport": true,
                            "insertReplaceSupport": true
                        },
                        "contextSupport": true
                    },
                    "hover": { "contentFormat": ["markdown", "plaintext"] },
                    "definition": { "linkSupport": true },
                    "references": {},
                    "documentHighlight": {},
                    "rename": { "prepareSupport": true },
                    "formatting": {},
                    "rangeFormatting": {},
                    "signatureHelp": {
                        "signatureInformation": {
                            "documentationFormat": ["markdown", "plaintext"],
                            "parameterInformation": { "labelOffsetSupport": true }
                        }
                    },
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": { "valueSet": [
                                "quickfix", "refactor", "refactor.extract",
                                "refactor.inline", "refactor.rewrite", "source",
                                "source.organizeImports"
                            ] }
                        }
                    },
                    "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                },
                "workspace": {
                    // 未対応: サーバーが勝手にファイルを作り替える applyEdit は受けない
                    "applyEdit": false,
                    "workspaceEdit": { "documentChanges": true }
                }
            },
            "workspaceFolders": [{ "uri": root_uri, "name": root_name }]
        });
        let id = client.next_id.fetch_add(1, Ordering::SeqCst);
        client.remember_pending(id, Pending::Initialize);
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

    /// initialize 応答から読み取ったサーバー能力 (未受信の間は全 false)。
    pub fn caps(&self) -> ServerCaps {
        lock_ok(&self.shared.caps).clone()
    }

    /// 未応答リクエスト数 (UI のスピナー判定に使える)。
    pub fn pending_count(&self) -> usize {
        lock_ok(&self.shared.pending).len()
    }

    /// タイムアウト/サーバー死亡で打ち切ったリクエストの累計。
    pub fn abandoned_count(&self) -> u64 {
        self.shared.abandoned.load(Ordering::SeqCst)
    }

    /// `timeout` より古い未応答リクエストを打ち切る。**毎フレーム呼ぶこと**。
    /// 打ち切られた待機は「空の結果」になるので UI のスピナーが止まる。
    /// 戻り値は打ち切った件数。
    pub fn sweep_timeouts(&self, timeout: Duration) -> usize {
        let now = std::time::Instant::now();
        let stale: Vec<Pending> = {
            let mut p = lock_ok(&self.shared.pending);
            let ids: Vec<u64> = p
                .iter()
                .filter(|(_, e)| now.saturating_duration_since(e.at) >= timeout)
                .map(|(id, _)| *id)
                .collect();
            ids.iter().filter_map(|id| p.remove(id)).map(|e| e.kind).collect()
        };
        for k in &stale {
            self.shared.abandon(*k);
        }
        stale.len()
    }

    /// 送信前チェック: ready か / 生きているか / 能力があるか。
    /// `supported=false` は **エラーではなく no-op** ([`RequestStatus::Unsupported`])。
    fn begin(&self, supported: bool, kind: Pending) -> RequestStatus {
        if !self.is_alive() {
            return RequestStatus::Dead;
        }
        if !self.is_ready() {
            return RequestStatus::NotReady;
        }
        if !supported {
            return RequestStatus::Unsupported;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.remember_pending(id, kind);
        RequestStatus::Sent(id)
    }

    fn remember_pending(&self, id: u64, kind: Pending) {
        self.shared.remember(id, kind);
    }

    fn doc_pos(path: &Path, pos: Position) -> Value {
        json!({
            "textDocument": { "uri": path_to_uri(&canonical(path)) },
            "position": { "line": pos.line, "character": pos.character }
        })
    }

    fn doc(path: &Path) -> Value {
        json!({ "textDocument": { "uri": path_to_uri(&canonical(path)) } })
    }

    fn range_json(range: &Range) -> Value {
        json!({
            "start": { "line": range.start.line, "character": range.start.character },
            "end": { "line": range.end.line, "character": range.end.character }
        })
    }

    // ── 補完 ────────────────────────────────────────────────────

    /// 非同期: 送信のみ。結果は [`Self::poll_completion`] で取得。`pos` は LSP (UTF-16) 座標。
    /// `trigger` に文字を渡すとサーバーへ triggerKind=2 (TriggerCharacter) で伝える。
    pub fn request_completion_at(
        &self,
        path: &Path,
        pos: Position,
        trigger: Option<char>,
    ) -> RequestStatus {
        let st = self.begin(self.caps().completion, Pending::Completion);
        if let RequestStatus::Sent(id) = st {
            self.shared.completion.begin(id);
            let mut params = Self::doc_pos(path, pos);
            let ctx = match trigger {
                Some(c) => json!({ "triggerKind": 2, "triggerCharacter": c.to_string() }),
                None => json!({ "triggerKind": 1 }),
            };
            params["context"] = ctx;
            self.request_raw(id, "textDocument/completion", params);
        }
        st
    }

    /// 旧シグネチャ互換 (line/col 直指定)。
    pub fn request_completion(&self, path: &Path, line: usize, col: usize) -> RequestStatus {
        self.request_completion_at(path, Position::new(line, col), None)
    }

    /// 応答を 1 度だけ取り出す。superseded なリクエストの応答はここへは来ない。
    pub fn poll_completion(&self) -> Option<CompletionList> {
        self.shared.completion.take()
    }

    /// 待機中の補完を取り下げる (Esc / カーソル移動)。
    pub fn cancel_completion(&self) {
        self.shared.completion.cancel();
    }

    // ── ホバー ──────────────────────────────────────────────────

    pub fn request_hover_at(&self, path: &Path, pos: Position) -> RequestStatus {
        let st = self.begin(self.caps().hover, Pending::Hover);
        if let RequestStatus::Sent(id) = st {
            self.shared.hover.begin(id);
            self.request_raw(id, "textDocument/hover", Self::doc_pos(path, pos));
        }
        st
    }

    /// 旧シグネチャ互換。
    pub fn request_hover(&self, path: &Path, line: usize, col: usize) -> RequestStatus {
        self.request_hover_at(path, Position::new(line, col))
    }

    pub fn poll_hover(&self) -> Option<HoverInfo> {
        self.shared.hover.take()
    }

    /// カーソルが動いたら呼ぶ: 待機中のホバーを捨てる。
    pub fn cancel_hover(&self) {
        self.shared.hover.cancel();
    }

    // ── 定義へ移動 ──────────────────────────────────────────────

    /// 定義へ移動 (VS Code: F12)。応答は poll_definition で受け取る。
    pub fn request_definition(&self, path: &Path, line: usize, col: usize) -> RequestStatus {
        let st = self.begin(self.caps().definition, Pending::Definition);
        if let RequestStatus::Sent(id) = st {
            self.shared.definition.begin(id);
            self.request_raw(
                id,
                "textDocument/definition",
                Self::doc_pos(path, Position::new(line, col)),
            );
        }
        st
    }

    /// 外側 Some = 応答あり (一度で消費)。内側 None = 定義が見つからなかった。
    pub fn poll_definition(&self) -> Option<Option<DefinitionLoc>> {
        self.shared.definition.take()
    }

    // ── 参照検索 / ハイライト ──────────────────────────────────

    /// textDocument/references (VS Code: Shift+F12)。結果はファイル単位にまとめて返る。
    pub fn request_references(
        &self,
        path: &Path,
        pos: Position,
        include_declaration: bool,
    ) -> RequestStatus {
        let st = self.begin(self.caps().references, Pending::References);
        if let RequestStatus::Sent(id) = st {
            self.shared.references.begin(id);
            let mut params = Self::doc_pos(path, pos);
            params["context"] = json!({ "includeDeclaration": include_declaration });
            self.request_raw(id, "textDocument/references", params);
        }
        st
    }

    pub fn poll_references(&self) -> Option<Vec<ReferenceGroup>> {
        self.shared.references.take()
    }

    /// カーソル下のシンボルの同一ファイル内出現 (VS Code の薄いハイライト)。
    pub fn request_document_highlight(&self, path: &Path, pos: Position) -> RequestStatus {
        let st = self.begin(self.caps().document_highlight, Pending::Highlight);
        if let RequestStatus::Sent(id) = st {
            self.shared.highlight.begin(id);
            self.request_raw(id, "textDocument/documentHighlight", Self::doc_pos(path, pos));
        }
        st
    }

    pub fn poll_document_highlight(&self) -> Option<Vec<DocumentHighlight>> {
        self.shared.highlight.take()
    }

    // ── リネーム ────────────────────────────────────────────────

    /// prepareRename: 「ここで rename できるか」と対象範囲を先に確認する (F2 の下準備)。
    /// サーバーが prepareProvider を出していなければ Unsupported (rename 自体は可能)。
    pub fn request_prepare_rename(&self, path: &Path, pos: Position) -> RequestStatus {
        let st = self.begin(self.caps().prepare_rename, Pending::PrepareRename);
        if let RequestStatus::Sent(id) = st {
            self.shared.prepare_rename.begin(id);
            self.request_raw(id, "textDocument/prepareRename", Self::doc_pos(path, pos));
        }
        st
    }

    /// 外側 Some = 応答あり。内側 None = ここでは rename できない。
    pub fn poll_prepare_rename(&self) -> Option<Option<Range>> {
        self.shared.prepare_rename.take()
    }

    /// textDocument/rename。結果は [`WorkspaceEditPlan`] (ファイル毎・後ろから適用順)。
    pub fn request_rename(&self, path: &Path, pos: Position, new_name: &str) -> RequestStatus {
        let st = self.begin(self.caps().rename, Pending::Rename);
        if let RequestStatus::Sent(id) = st {
            self.shared.rename.begin(id);
            let mut params = Self::doc_pos(path, pos);
            params["newName"] = json!(new_name);
            self.request_raw(id, "textDocument/rename", params);
        }
        st
    }

    pub fn poll_rename(&self) -> Option<WorkspaceEditPlan> {
        self.shared.rename.take()
    }

    // ── 整形 ────────────────────────────────────────────────────

    /// ファイル全体の整形。保存時整形もこれを使い、**いつ呼ぶかは呼び出し側が決める**
    /// (保存 → request_formatting → poll_formatting → [`apply_text_edits`] → 書き出し)。
    pub fn request_formatting(&self, path: &Path, opts: &FormatOptions) -> RequestStatus {
        let st = self.begin(self.caps().formatting, Pending::Formatting);
        if let RequestStatus::Sent(id) = st {
            self.shared.formatting.begin(id);
            let mut params = Self::doc(path);
            params["options"] = opts.to_json();
            self.request_raw(id, "textDocument/formatting", params);
        }
        st
    }

    /// 選択範囲だけの整形。範囲整形に対応していないサーバーは Unsupported。
    pub fn request_range_formatting(
        &self,
        path: &Path,
        range: &Range,
        opts: &FormatOptions,
    ) -> RequestStatus {
        let st = self.begin(self.caps().range_formatting, Pending::Formatting);
        if let RequestStatus::Sent(id) = st {
            self.shared.formatting.begin(id);
            let mut params = Self::doc(path);
            params["range"] = Self::range_json(range);
            params["options"] = opts.to_json();
            self.request_raw(id, "textDocument/rangeFormatting", params);
        }
        st
    }

    /// 整形結果の TextEdit 群 (全体整形/範囲整形の共通スロット)。
    pub fn poll_formatting(&self) -> Option<Vec<TextEdit>> {
        self.shared.formatting.take()
    }

    // ── ドキュメントシンボル ────────────────────────────────────

    /// アウトライン / シンボルへ移動 (VS Code: Ctrl+Shift+O)。
    /// 平坦形式・階層形式のどちらの応答も同じ木 ([`SymbolNode`]) に正規化される。
    pub fn request_document_symbols(&self, path: &Path) -> RequestStatus {
        let st = self.begin(self.caps().document_symbol, Pending::Symbols);
        if let RequestStatus::Sent(id) = st {
            self.shared.symbols.begin(id);
            self.request_raw(id, "textDocument/documentSymbol", Self::doc(path));
        }
        st
    }

    pub fn poll_document_symbols(&self) -> Option<Vec<SymbolNode>> {
        self.shared.symbols.take()
    }

    // ── シグネチャヘルプ ────────────────────────────────────────

    /// 関数呼び出しの引数ヒント ('(' や ',' の入力後)。
    pub fn request_signature_help(&self, path: &Path, pos: Position) -> RequestStatus {
        let st = self.begin(self.caps().signature_help, Pending::Signature);
        if let RequestStatus::Sent(id) = st {
            self.shared.signature.begin(id);
            self.request_raw(id, "textDocument/signatureHelp", Self::doc_pos(path, pos));
        }
        st
    }

    pub fn poll_signature_help(&self) -> Option<SignatureHelp> {
        self.shared.signature.take()
    }

    pub fn cancel_signature_help(&self) {
        self.shared.signature.cancel();
    }

    // ── コードアクション ────────────────────────────────────────

    /// クイックフィックス / リファクタ候補 (VS Code: Ctrl+.)。
    /// `diags` にはその範囲に重なる診断を渡す (サーバーが修正候補を絞るのに使う)。
    pub fn request_code_actions(
        &self,
        path: &Path,
        range: &Range,
        diags: &[Diagnostic],
    ) -> RequestStatus {
        let st = self.begin(self.caps().code_action, Pending::CodeAction);
        if let RequestStatus::Sent(id) = st {
            self.shared.code_action.begin(id);
            let mut params = Self::doc(path);
            params["range"] = Self::range_json(range);
            params["context"] = json!({
                "diagnostics": diags.iter().map(diagnostic_to_json).collect::<Vec<_>>()
            });
            self.request_raw(id, "textDocument/codeAction", params);
        }
        st
    }

    pub fn poll_code_actions(&self) -> Option<Vec<CodeAction>> {
        self.shared.code_action.take()
    }

    /// コードアクションの command を実行する (編集はサーバー側から applyEdit で来る想定だが、
    /// 本エディタは applyEdit を受けないので「サーバー内で完結する command」専用)。
    pub fn execute_command(&self, cmd: &CommandRef) -> RequestStatus {
        if !self.is_alive() {
            return RequestStatus::Dead;
        }
        if !self.is_ready() {
            return RequestStatus::NotReady;
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        self.request_raw(
            id,
            "workspace/executeCommand",
            json!({ "command": cmd.command, "arguments": cmd.arguments }),
        );
        RequestStatus::Sent(id)
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
                // 未応答のリクエストを全部空結果で打ち切る。
                // これをやらないと UI が「応答待ち」のまま永久に固まる。
                shared.abandon_all();
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
            let kind = lock_ok(&shared.pending).remove(&id).map(|e| e.kind);
            // エラー応答 (result 無し) は「結果ゼロ」として扱う。
            // 例: サーバーが能力を宣言しつつ MethodNotFound を返すことがある。
            let result = v.get("result").cloned().unwrap_or(Value::Null);
            match kind {
                Some(Pending::Initialize) => {
                    *lock_ok(&shared.caps) =
                        parse_server_caps(result.get("capabilities").unwrap_or(&Value::Null));
                    // initialized 通知を先にチャネルへ積んでからフラグを立てる (順序保証:
                    // is_ready を見てから送られる通知より必ず先にサーバーへ届く)
                    let _ = send_json(
                        tx,
                        json!({"jsonrpc":"2.0","method":"initialized","params":{}}),
                    );
                    shared.init_done.store(true, Ordering::SeqCst);
                }
                Some(Pending::Completion) => {
                    shared.completion.fulfill(id, parse_completion_list(&result))
                }
                Some(Pending::Hover) => shared.hover.fulfill(id, parse_hover(&result)),
                Some(Pending::Definition) => {
                    shared.definition.fulfill(id, parse_definition(&result))
                }
                Some(Pending::References) => shared
                    .references
                    .fulfill(id, group_locations(parse_locations(&result))),
                Some(Pending::Highlight) => {
                    shared.highlight.fulfill(id, parse_document_highlights(&result))
                }
                Some(Pending::PrepareRename) => {
                    shared.prepare_rename.fulfill(id, parse_prepare_rename(&result))
                }
                Some(Pending::Rename) => {
                    shared.rename.fulfill(id, parse_workspace_edit(&result))
                }
                Some(Pending::Formatting) => {
                    shared.formatting.fulfill(id, parse_text_edits(&result))
                }
                Some(Pending::Symbols) => {
                    shared.symbols.fulfill(id, parse_document_symbols(&result))
                }
                Some(Pending::Signature) => {
                    shared.signature.fulfill(id, parse_signature_help(&result))
                }
                Some(Pending::CodeAction) => {
                    shared.code_action.fulfill(id, parse_code_actions(&result))
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

fn diagnostic_to_json(d: &Diagnostic) -> Value {
    json!({
        "range": {
            "start": { "line": d.line, "character": d.col },
            "end": { "line": d.end_line, "character": d.end_col }
        },
        "severity": d.severity,
        "message": d.message,
    })
}

// ---------------------------------------------------------------------------
// パーサ (純関数: すべて Value を受けて壊れていても panic しない)
// ---------------------------------------------------------------------------

fn get_str(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}

fn get_pos(v: &Value) -> Position {
    Position::new(
        v.get("line").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
        v.get("character").and_then(|n| n.as_u64()).unwrap_or(0) as usize,
    )
}

/// range フィールドを読む。無ければ None。
fn get_range(v: &Value) -> Option<Range> {
    let start = v.get("start").map(get_pos)?;
    let end = v.get("end").map(get_pos).unwrap_or(start);
    Some(Range::new(start, end))
}

/// `xxxProvider` が有効か。仕様上 `true` / オプションオブジェクト / 省略 のいずれも来る。
fn provider_on(caps: &Value, key: &str) -> bool {
    match caps.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::Object(_)) => true,
        _ => false,
    }
}

/// トリガ文字の配列 (["." , ":"]) を char 集合へ。複数文字の要素は先頭文字を使う。
fn trigger_chars(caps: &Value, provider: &str) -> Vec<char> {
    caps.get(provider)
        .and_then(|p| p.get("triggerCharacters"))
        .and_then(|t| t.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().and_then(|s| s.chars().next()))
                .collect()
        })
        .unwrap_or_default()
}

/// initialize 応答の `capabilities` → [`ServerCaps`]。
/// 宣言されていない機能は false になり、対応するリクエストは no-op になる。
pub fn parse_server_caps(caps: &Value) -> ServerCaps {
    ServerCaps {
        completion: provider_on(caps, "completionProvider"),
        completion_trigger_chars: trigger_chars(caps, "completionProvider"),
        hover: provider_on(caps, "hoverProvider"),
        definition: provider_on(caps, "definitionProvider"),
        references: provider_on(caps, "referencesProvider"),
        document_highlight: provider_on(caps, "documentHighlightProvider"),
        rename: provider_on(caps, "renameProvider"),
        prepare_rename: caps
            .get("renameProvider")
            .and_then(|r| r.get("prepareProvider"))
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        formatting: provider_on(caps, "documentFormattingProvider"),
        range_formatting: provider_on(caps, "documentRangeFormattingProvider"),
        signature_help: provider_on(caps, "signatureHelpProvider"),
        signature_trigger_chars: trigger_chars(caps, "signatureHelpProvider"),
        code_action: provider_on(caps, "codeActionProvider"),
        document_symbol: provider_on(caps, "documentSymbolProvider"),
    }
}

/// documentation: string | MarkupContent
fn parse_documentation(v: Option<&Value>) -> String {
    match v {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Object(o)) => o
            .get("value")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

/// TextEdit | InsertReplaceEdit → [`TextEdit`]。
///
/// InsertReplaceEdit の `insert` と `replace` では **insert を採る**
/// (VS Code の既定 `editor.suggest.insertMode = "insert"` と同じ。
/// カーソル右の識別子を巻き込んで消さない方が事故が少ない)。
fn parse_one_text_edit(v: &Value) -> Option<TextEdit> {
    let new_text = v.get("newText").and_then(|s| s.as_str())?.to_string();
    let range = v
        .get("range")
        .and_then(get_range)
        .or_else(|| v.get("insert").and_then(get_range))
        .or_else(|| v.get("replace").and_then(get_range))?;
    Some(TextEdit { range, new_text })
}

/// TextEdit[] (AnnotatedTextEdit も同形なのでそのまま読める)。null は空配列。
pub fn parse_text_edits(v: &Value) -> Vec<TextEdit> {
    v.as_array()
        .map(|arr| arr.iter().filter_map(parse_one_text_edit).collect())
        .unwrap_or_default()
}

/// CompletionItem[] | CompletionList → [`CompletionList`]。
pub fn parse_completion_list(result: &Value) -> CompletionList {
    let empty = Vec::new();
    let (items, is_incomplete) = if let Some(arr) = result.as_array() {
        (arr, false)
    } else if let Some(arr) = result.get("items").and_then(|i| i.as_array()) {
        (
            arr,
            result
                .get("isIncomplete")
                .and_then(|b| b.as_bool())
                .unwrap_or(false),
        )
    } else {
        (&empty, false)
    };
    CompletionList {
        is_incomplete,
        items: items.iter().map(parse_completion_item).collect(),
    }
}

fn parse_completion_item(it: &Value) -> CompletionItem {
    let label = it
        .get("label")
        .and_then(|l| l.as_str())
        .unwrap_or("")
        .to_string();
    let text_edit = it.get("textEdit").and_then(parse_one_text_edit);
    let insert_text = it
        .get("insertText")
        .and_then(|n| n.as_str())
        .map(|s| s.to_string())
        .or_else(|| text_edit.as_ref().map(|e| e.new_text.clone()))
        .unwrap_or_else(|| label.clone());
    let deprecated = it
        .get("deprecated")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
        || it
            .get("tags")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().any(|t| t.as_u64() == Some(1)))
            .unwrap_or(false);
    CompletionItem {
        insert_text,
        detail: get_str(it, "detail"),
        documentation: parse_documentation(it.get("documentation")),
        kind: it.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u8,
        text_edit,
        additional_text_edits: it
            .get("additionalTextEdits")
            .map(parse_text_edits)
            .unwrap_or_default(),
        sort_text: it
            .get("sortText")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        filter_text: it
            .get("filterText")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string()),
        preselect: it
            .get("preselect")
            .and_then(|b| b.as_bool())
            .unwrap_or(false),
        // insertTextFormat: 1=PlainText 2=Snippet
        is_snippet: it.get("insertTextFormat").and_then(|n| n.as_u64()) == Some(2),
        deprecated,
        label,
    }
}

/// 補完候補を実際に適用するための TextEdit 群を作る。
///
/// * `textEdit` があればサーバーの範囲を尊重する (前方の `.` や部分入力を正しく消す)
/// * 無ければ `fallback` の範囲 (呼び出し側が計算した「入力中の語」の範囲) を置換する
/// * `additionalTextEdits` (自動 import 等) を **同じ配列に載せて返す**。
///   まとめて [`apply_text_edits`] へ渡せば後ろから正しい順で適用される。
pub fn completion_edits(item: &CompletionItem, fallback: Range) -> Vec<TextEdit> {
    let main = match &item.text_edit {
        Some(e) => e.clone(),
        None => TextEdit::new(fallback, item.insert_text.clone()),
    };
    let mut out = Vec::with_capacity(1 + item.additional_text_edits.len());
    out.push(main);
    out.extend(item.additional_text_edits.iter().cloned());
    out
}

/// Hover の結果 → [`HoverInfo`]。
pub fn parse_hover(result: &Value) -> HoverInfo {
    HoverInfo {
        contents: result.get("contents").map(hover_text).unwrap_or_default(),
        range: result.get("range").and_then(get_range),
    }
}

/// Hover contents: string | MarkupContent | MarkedString | それらの配列 → markdown。
///
/// MarkedString の `{language, value}` 形式は ```lang フェンスへ包む
/// (そのまま value だけ出すと markdown ビューアでコードとして描けないため)。
fn hover_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Array(arr) => arr
            .iter()
            .map(hover_text)
            .filter(|s| !s.trim().is_empty())
            .collect::<Vec<_>>()
            .join("\n\n"),
        Value::Object(obj) => {
            let value = obj
                .get("value")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();
            match obj.get("language").and_then(|l| l.as_str()) {
                // MarkedString {language, value}
                Some(lang) if !value.is_empty() => format!("```{lang}\n{value}\n```"),
                _ => value,
            }
        }
        _ => String::new(),
    }
}

/// Location | Location[] | LocationLink[] → [`Location`] の並び。
pub fn parse_locations(result: &Value) -> Vec<Location> {
    let one = |v: &Value| -> Option<Location> {
        if let Some(uri) = v.get("targetUri").and_then(|u| u.as_str()) {
            let range = v
                .get("targetSelectionRange")
                .and_then(get_range)
                .or_else(|| v.get("targetRange").and_then(get_range))?;
            return Some(Location {
                path: uri_to_path(uri),
                range,
            });
        }
        let uri = v.get("uri").and_then(|u| u.as_str())?;
        Some(Location {
            path: uri_to_path(uri),
            range: v.get("range").and_then(get_range)?,
        })
    };
    match result {
        Value::Array(arr) => arr.iter().filter_map(one).collect(),
        Value::Object(_) => one(result).into_iter().collect(),
        _ => Vec::new(),
    }
}

/// 参照結果をファイル単位へまとめる。ファイル順・位置順に整列し、重複は畳む
/// (結果パネルがそのまま描ける形)。
pub fn group_locations(locs: Vec<Location>) -> Vec<ReferenceGroup> {
    let mut map: HashMap<PathBuf, Vec<Range>> = HashMap::new();
    for l in locs {
        map.entry(canonical(&l.path)).or_default().push(l.range);
    }
    let mut groups: Vec<ReferenceGroup> = map
        .into_iter()
        .map(|(path, mut locations)| {
            locations.sort_by_key(|r| (r.start.line, r.start.character, r.end.line, r.end.character));
            locations.dedup();
            ReferenceGroup { path, locations }
        })
        .collect();
    groups.sort_by(|a, b| a.path.cmp(&b.path));
    groups
}

/// DocumentHighlight[]。
pub fn parse_document_highlights(result: &Value) -> Vec<DocumentHighlight> {
    result
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    Some(DocumentHighlight {
                        range: v.get("range").and_then(get_range)?,
                        kind: v.get("kind").and_then(|k| k.as_u64()).unwrap_or(1) as u8,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// prepareRename: Range | {range, placeholder} | {defaultBehavior} | null。
/// None = ここでは rename できない。
pub fn parse_prepare_rename(result: &Value) -> Option<Range> {
    if result.is_null() {
        return None;
    }
    if let Some(r) = get_range(result) {
        return Some(r);
    }
    if let Some(r) = result.get("range").and_then(get_range) {
        return Some(r);
    }
    // {defaultBehavior: true} は「範囲はクライアントが決めてよい」の意。
    // 呼び出し側が単語範囲を出すので、ここでは空範囲を返して「可能」だけ伝える。
    if result
        .get("defaultBehavior")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
    {
        return Some(Range::default());
    }
    None
}

/// WorkspaceEdit (`changes` 形式 / `documentChanges` 形式の両方) を
/// **ファイル毎・後ろから適用できる順** に正規化する。
///
/// `documentChanges` に create/rename/delete file が混ざっていたら
/// `has_resource_ops` を立てる (本エディタは適用しないので UI が警告する)。
pub fn parse_workspace_edit(result: &Value) -> WorkspaceEditPlan {
    let mut by_file: Vec<(PathBuf, Vec<TextEdit>)> = Vec::new();
    let mut has_resource_ops = false;
    let mut push = |path: PathBuf, edits: Vec<TextEdit>| {
        if let Some(slot) = by_file.iter_mut().find(|(p, _)| *p == path) {
            slot.1.extend(edits);
        } else {
            by_file.push((path, edits));
        }
    };

    // documentChanges を優先する (版管理付きで、リソース操作も表現できる正式形)
    if let Some(arr) = result.get("documentChanges").and_then(|d| d.as_array()) {
        for ch in arr {
            if ch.get("kind").is_some() {
                has_resource_ops = true; // create / rename / delete
                continue;
            }
            let uri = ch
                .get("textDocument")
                .and_then(|t| t.get("uri"))
                .and_then(|u| u.as_str());
            if let Some(uri) = uri {
                let edits = ch.get("edits").map(parse_text_edits).unwrap_or_default();
                push(canonical(&uri_to_path(uri)), edits);
            }
        }
    } else if let Some(map) = result.get("changes").and_then(|c| c.as_object()) {
        // changes は JSON オブジェクト = 順序不定なので URI 順に固定して再現性を出す
        let mut keys: Vec<&String> = map.keys().collect();
        keys.sort();
        for uri in keys {
            let edits = map.get(uri).map(parse_text_edits).unwrap_or_default();
            push(canonical(&uri_to_path(uri)), edits);
        }
    }

    let files = by_file
        .into_iter()
        .map(|(path, mut edits)| {
            // 後ろから適用できる順 = 開始位置の降順。同位置は元の順序を保つため
            // 「安定ソートで昇順 → reverse」ではなく、降順キーの安定ソートにする。
            edits.sort_by(|a, b| {
                (b.range.start, b.range.end).cmp(&(a.range.start, a.range.end))
            });
            FileEdits { path, edits }
        })
        .collect();
    WorkspaceEditPlan {
        files,
        has_resource_ops,
    }
}

/// DocumentSymbol[] (階層) | SymbolInformation[] (平坦) → 同じ木。
pub fn parse_document_symbols(result: &Value) -> Vec<SymbolNode> {
    let arr = match result.as_array() {
        Some(a) => a,
        None => return Vec::new(),
    };
    // 判別: location を持つのは SymbolInformation。selectionRange を持つのは DocumentSymbol。
    let flat = arr
        .iter()
        .any(|v| v.get("location").is_some() && v.get("selectionRange").is_none());
    if flat {
        nest_flat_symbols(arr)
    } else {
        arr.iter().filter_map(parse_hierarchical_symbol).collect()
    }
}

fn symbol_common(v: &Value) -> (String, String, u8, bool) {
    let deprecated = v
        .get("deprecated")
        .and_then(|b| b.as_bool())
        .unwrap_or(false)
        || v.get("tags")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().any(|t| t.as_u64() == Some(1)))
            .unwrap_or(false);
    (
        get_str(v, "name"),
        get_str(v, "detail"),
        v.get("kind").and_then(|k| k.as_u64()).unwrap_or(0) as u8,
        deprecated,
    )
}

fn parse_hierarchical_symbol(v: &Value) -> Option<SymbolNode> {
    let range = v.get("range").and_then(get_range)?;
    let selection_range = v
        .get("selectionRange")
        .and_then(get_range)
        .unwrap_or(range);
    let (name, detail, kind, deprecated) = symbol_common(v);
    let children = v
        .get("children")
        .and_then(|c| c.as_array())
        .map(|arr| arr.iter().filter_map(parse_hierarchical_symbol).collect())
        .unwrap_or_default();
    Some(SymbolNode {
        name,
        detail,
        kind,
        range,
        selection_range,
        deprecated,
        children,
    })
}

/// 平坦な SymbolInformation[] を **範囲の包含関係**で入れ子へ組み直す。
/// containerName に頼らないのは、同名の入れ子や無名スコープで曖昧になるため。
fn nest_flat_symbols(arr: &[Value]) -> Vec<SymbolNode> {
    let mut flat: Vec<SymbolNode> = arr
        .iter()
        .filter_map(|v| {
            let range = v
                .get("location")
                .and_then(|l| l.get("range"))
                .and_then(get_range)?;
            let (name, detail, kind, deprecated) = symbol_common(v);
            Some(SymbolNode {
                name,
                detail,
                kind,
                range,
                // 平坦形式は名前だけの範囲を持たないので全体範囲で代用する
                selection_range: range,
                deprecated,
                children: Vec::new(),
            })
        })
        .collect();
    // 開始位置の昇順、同じ開始なら「広いものが先」= 親が先に来る
    flat.sort_by(|a, b| {
        a.range
            .start
            .cmp(&b.range.start)
            .then(b.range.end.cmp(&a.range.end))
    });

    // スタックで親子を組む。親の range に完全に含まれる次の要素を子にする。
    let mut roots: Vec<SymbolNode> = Vec::new();
    // stack は roots からの経路 (index 列)。所有権の都合でインデックス経由で辿る。
    let mut stack: Vec<usize> = Vec::new();
    for node in flat {
        loop {
            let parent = walk_mut(&mut roots, &stack);
            match parent {
                Some(p) if p.range.contains_range(&node.range) => {
                    let idx = p.children.len();
                    p.children.push(node);
                    stack.push(idx);
                    break;
                }
                Some(_) => {
                    stack.pop();
                }
                None => {
                    roots.push(node);
                    stack.clear();
                    stack.push(roots.len() - 1);
                    break;
                }
            }
        }
    }
    roots
}

/// `path` (roots からの子インデックス列) の指す節点を可変で借りる。
fn walk_mut<'a>(roots: &'a mut [SymbolNode], path: &[usize]) -> Option<&'a mut SymbolNode> {
    let (first, rest) = path.split_first()?;
    let mut cur = roots.get_mut(*first)?;
    for i in rest {
        cur = cur.children.get_mut(*i)?;
    }
    Some(cur)
}

/// SignatureHelp。
pub fn parse_signature_help(result: &Value) -> SignatureHelp {
    let signatures = result
        .get("signatures")
        .and_then(|s| s.as_array())
        .map(|arr| {
            arr.iter()
                .map(|s| SignatureInfo {
                    label: get_str(s, "label"),
                    documentation: parse_documentation(s.get("documentation")),
                    parameters: s
                        .get("parameters")
                        .and_then(|p| p.as_array())
                        .map(|ps| {
                            ps.iter()
                                .map(|p| ParameterInfo {
                                    // label は string か [start,end] のオフセット対
                                    label: match p.get("label") {
                                        Some(Value::String(s)) => s.clone(),
                                        Some(Value::Array(a)) => {
                                            let n = |i: usize| {
                                                a.get(i).and_then(|v| v.as_u64()).unwrap_or(0)
                                                    as usize
                                            };
                                            let (s0, s1) = (n(0), n(1));
                                            let sig = get_str(s, "label");
                                            slice_utf16(&sig, s0, s1)
                                        }
                                        _ => String::new(),
                                    },
                                    documentation: parse_documentation(p.get("documentation")),
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    SignatureHelp {
        active_signature: result
            .get("activeSignature")
            .and_then(|n| n.as_u64())
            .unwrap_or(0) as usize,
        active_parameter: result
            .get("activeParameter")
            .and_then(|n| n.as_u64())
            .map(|n| n as usize),
        signatures,
    }
}

/// UTF-16 オフセット [start, end) で文字列を切り出す (parameter label 用)。
fn slice_utf16(s: &str, start: usize, end: usize) -> String {
    let mut col = 0usize;
    let mut out = String::new();
    for ch in s.chars() {
        if col >= start && col < end {
            out.push(ch);
        }
        col += ch.len_utf16();
        if col >= end {
            break;
        }
    }
    out
}

/// (Command | CodeAction)[] → [`CodeAction`]。
pub fn parse_code_actions(result: &Value) -> Vec<CodeAction> {
    result
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    // Command 形式は "command" が文字列 (CodeAction 形式ではオブジェクト)
                    if let Some(cmd) = v.get("command").and_then(|c| c.as_str()) {
                        let title = get_str(v, "title");
                        if title.is_empty() && cmd.is_empty() {
                            return None;
                        }
                        return Some(CodeAction {
                            title,
                            command: Some(CommandRef {
                                title: get_str(v, "title"),
                                command: cmd.to_string(),
                                arguments: v
                                    .get("arguments")
                                    .and_then(|a| a.as_array())
                                    .cloned()
                                    .unwrap_or_default(),
                            }),
                            ..CodeAction::default()
                        });
                    }
                    let title = get_str(v, "title");
                    if title.is_empty() {
                        return None;
                    }
                    let edit = v
                        .get("edit")
                        .map(parse_workspace_edit)
                        .unwrap_or_default();
                    let needs_resolve = v.get("edit").is_none() && v.get("command").is_none();
                    Some(CodeAction {
                        title,
                        kind: get_str(v, "kind"),
                        is_preferred: v
                            .get("isPreferred")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false),
                        edit,
                        command: v.get("command").and_then(|c| {
                            Some(CommandRef {
                                title: get_str(c, "title"),
                                command: c.get("command")?.as_str()?.to_string(),
                                arguments: c
                                    .get("arguments")
                                    .and_then(|a| a.as_array())
                                    .cloned()
                                    .unwrap_or_default(),
                            })
                        }),
                        needs_resolve,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// 応答の「見せ方」を決める純関数
//
// 並べ替え・切り詰め・塗る場所の算出はここに閉じ込める。egui も App の状態も
// 知らないので、モックした JSON 応答だけでテーブルテストできる
// (実際に LSP サーバーを起動するテストは CI にプロセスを残すので書かない)。
// ---------------------------------------------------------------------------

/// コードアクションのポップアップに並べる最大件数。
/// tsserver の import 候補のように数百件返すサーバーがあるため、
/// 「押したい順」に並べてから頭だけを切り取る。
pub const MAX_CODE_ACTIONS: usize = 30;

/// ポップアップ 1 行に出すタイトルの最大文字数 (超えたら末尾を … に畳む)。
/// どの幅でも見切れないよう、描画側の折り返しに頼らずここで決める。
pub const ACTION_TITLE_MAX: usize = 72;

/// 同一シンボルのハイライトを塗る最大件数。
/// 巨大ファイルで全出現が返ってきても、塗る矩形の数を有限に保つ。
pub const MAX_HIGHLIGHTS: usize = 200;

/// documentHighlight の既定デバウンス (キャレットが止まってから要求するまで)。
pub const HIGHLIGHT_DEBOUNCE: Duration = Duration::from_millis(250);

/// 改行・タブ・連続空白を 1 個の空白へ潰し、`max_chars` 文字で切り詰める。
///
/// LSP のタイトルやドキュメントは複数行で来ることがあり、そのまま 1 行に
/// 置くと行が伸びて見切れる。切り詰めは **char 単位**なので日本語でも
/// 文字の途中で切れない。
pub fn one_line_label(s: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut gap = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            gap = !out.is_empty();
            continue;
        }
        if gap {
            out.push(' ');
            gap = false;
        }
        out.push(ch);
    }
    if out.chars().count() > max_chars {
        let keep: String = out.chars().take(max_chars.saturating_sub(1)).collect();
        out = keep;
        out.push('…');
    }
    out
}

/// アクションの並び順の重み (小さいほど上)。
fn action_rank(a: &CodeAction) -> u8 {
    if a.is_preferred {
        return 0;
    }
    if a.kind.starts_with("quickfix") {
        return 1;
    }
    if a.kind.starts_with("source.fixAll") {
        return 2;
    }
    if a.kind.starts_with("refactor") {
        return 3;
    }
    if a.kind.starts_with("source") {
        return 4;
    }
    // kind 無し = Command 形式。何を直すのか分からないので後ろへ。
    5
}

/// コードアクションを「押したい順」に安定整列して上限で切る。
///
/// タイトルが空のものは押しても何を選んだか分からないので落とす。
/// 同順位はサーバーが返した順のまま (`sort_by_key` は安定ソート)。
pub fn rank_code_actions(actions: Vec<CodeAction>) -> Vec<CodeAction> {
    let mut v: Vec<CodeAction> = actions
        .into_iter()
        .filter(|a| !a.title.trim().is_empty())
        .collect();
    v.sort_by_key(action_rank);
    v.truncate(MAX_CODE_ACTIONS);
    v
}

/// 選んだときに実際に何かできるアクションか。
/// edit も command も無いものは `codeAction/resolve` が要る (本クライアントは未対応)。
pub fn action_is_actionable(a: &CodeAction) -> bool {
    !a.edit.is_empty() || a.command.is_some()
}

/// codeAction / rangeFormatting へ渡す範囲を決める。
///
/// 選択があればそれを (前後を正規化して) 使い、無ければ**キャレット行の全体**を使う。
/// 行全体にするのは「行のどこにキャレットがあってもその行の診断を拾う」ため
/// (VS Code の電球と同じ体感にする)。
pub fn action_range(
    text: &str,
    selection: Option<(Position, Position)>,
    caret: Position,
) -> Range {
    if let Some((a, b)) = selection {
        let (s, e) = if a <= b { (a, b) } else { (b, a) };
        if s != e {
            return Range::new(s, e);
        }
    }
    let width = text
        .split('\n')
        .nth(caret.line)
        .map(|l| l.trim_end_matches('\r').encode_utf16().count())
        .unwrap_or(caret.character);
    Range::new(
        Position::new(caret.line, 0),
        Position::new(caret.line, width),
    )
}

/// `range` に重なる診断だけを返す (codeAction の `context.diagnostics`)。
///
/// 端点だけが触れる場合も重なりとみなす — 空範囲の診断 (「ここに ; が要る」)
/// を落とすと、サーバーがその修正候補を返さなくなるため。
pub fn diagnostics_in_range(diags: &[Diagnostic], range: &Range) -> Vec<Diagnostic> {
    diags
        .iter()
        .filter(|d| {
            let s = Position::new(d.line, d.col);
            let e = Position::new(d.end_line, d.end_col);
            let (s, e) = if s <= e { (s, e) } else { (e, s) };
            s <= range.end && range.start <= e
        })
        .cloned()
        .collect()
}

/// char 添字の範囲を LSP の [`Range`] にする (選択範囲の整形など)。
pub fn char_span_to_range(text: &str, start: usize, end: usize) -> Range {
    let (a, b) = (start.min(end), start.max(end));
    let (sl, sc) = char_index_to_lsp_pos(text, a);
    let (el, ec) = char_index_to_lsp_pos(text, b);
    Range::new(Position::new(sl, sc), Position::new(el, ec))
}

/// シグネチャヘルプを 1 枚のポップアップに出すための材料。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SignatureDisplay {
    /// 関数のシグネチャ全体 (1 行に潰し済み)。
    pub label: String,
    /// いま入力中の引数のラベル。空なら強調しない。
    pub active_param: String,
    /// 1 行に潰したドキュメント (無ければ空)。
    pub doc: String,
    /// 何番目のシグネチャか (1 始まり) と総数。overload が 1 つなら (1, 1)。
    pub index: usize,
    pub total: usize,
}

/// 応答から「いま出すべき 1 枚」を作る。出すものが無ければ `None`。
///
/// `activeSignature` / `activeParameter` が範囲外を指す壊れた応答でも
/// panic せず、クランプするか強調を諦める。
pub fn signature_display(help: &SignatureHelp, doc_max: usize) -> Option<SignatureDisplay> {
    if help.signatures.is_empty() {
        return None;
    }
    let i = help.active_signature.min(help.signatures.len() - 1);
    let sig = &help.signatures[i];
    let label = one_line_label(&sig.label, ACTION_TITLE_MAX * 2);
    if label.is_empty() {
        return None;
    }
    let active_param = help
        .active_parameter
        .and_then(|p| sig.parameters.get(p))
        .map(|p| one_line_label(&p.label, ACTION_TITLE_MAX))
        .unwrap_or_default();
    Some(SignatureDisplay {
        label,
        active_param,
        doc: one_line_label(&sig.documentation, doc_max),
        index: i + 1,
        total: help.signatures.len(),
    })
}

/// documentHighlight を本文の char 添字スパンへ落とす。
///
/// 逆転・空・本文外は捨て、開始位置で整列して重複を除き、[`MAX_HIGHLIGHTS`] で切る。
/// **応答が来た瞬間に 1 回だけ**呼ぶこと (毎フレーム呼ぶと本文を何度も走査する)。
pub fn highlight_char_spans(text: &str, hl: &[DocumentHighlight]) -> Vec<(usize, usize)> {
    let total = text.chars().count();
    let mut out: Vec<(usize, usize)> = hl
        .iter()
        .filter_map(|h| {
            let s = lsp_pos_to_char_index(text, h.range.start.line, h.range.start.character);
            let e = lsp_pos_to_char_index(text, h.range.end.line, h.range.end.character);
            (s < e && e <= total).then_some((s, e))
        })
        .collect();
    out.sort_unstable();
    out.dedup();
    out.truncate(MAX_HIGHLIGHTS);
    out
}

// ---------------------------------------------------------------------------
// UI 非依存の状態層
//
// ここは egui を一切知らない。UI 側は
//   1. 入力イベントを on_* へ渡す
//   2. 毎フレーム due_request() を見て、返ってきたら client.request_* を呼び mark_sent
//   3. poll_* で応答が来たら apply_response
// という 3 手順だけを実装すればよい。時刻は引数で受け取るのでテストから制御できる。
// ---------------------------------------------------------------------------

/// 補完の既定デバウンス。入力の連打でサーバーを溺れさせない最小値として選んだ
/// (人の連続打鍵はおおむね 80〜150ms 間隔)。
pub const COMPLETION_DEBOUNCE: Duration = Duration::from_millis(120);
/// ホバーの既定デバウンス (カーソルを止めてから出るまで)。
pub const HOVER_DEBOUNCE: Duration = Duration::from_millis(350);
/// 未応答リクエストを諦めるまでの既定時間。
/// rust-analyzer の初回補完は数秒かかることがあるので短すぎない値にしてある。
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTrigger {
    /// 識別子の入力、または明示要求 (Ctrl+Space)。
    Invoked,
    /// サーバーが宣言したトリガ文字 ('.', ':' など)。
    TriggerChar(char),
}

/// 識別子を構成する文字か。`is_alphanumeric` なので日本語識別子もそのまま通る。
pub fn is_identifier_char(ch: char) -> bool {
    ch.is_alphanumeric() || ch == '_'
}

/// 補完 UI の状態。**表示方法は一切決めない**ので、ポップアップでもインラインでも使える。
#[derive(Debug, Default)]
pub struct CompletionState {
    items: Vec<CompletionItem>,
    /// items への添字。絞り込み済み・表示順。
    order: Vec<usize>,
    selected: usize,
    filter: String,
    is_incomplete: bool,
    open: bool,
    /// 送信済みで応答待ちのリクエスト id。これ以外の応答は捨てる。
    in_flight: Option<u64>,
    /// デバウンス待ちの要求 (トリガ種別, 予約時刻)。
    scheduled: Option<(CompletionTrigger, std::time::Instant)>,
    /// 要求を出した位置。UI が「置換すべき語の範囲」を作るのに使う。
    anchor: Option<Position>,
}

impl CompletionState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 1 文字入力されたときに呼ぶ。トリガ条件を満たせば要求を**予約**する
    /// (実際の送信は [`Self::due_request`] が返してから)。
    pub fn on_typed(&mut self, ch: char, caps: &ServerCaps, now: std::time::Instant) {
        if !caps.completion {
            return;
        }
        if caps.completion_trigger_chars.contains(&ch) {
            // '.' 等: 語は仕切り直し。必ず再要求する
            self.filter.clear();
            self.scheduled = Some((CompletionTrigger::TriggerChar(ch), now));
            return;
        }
        if is_identifier_char(ch) {
            self.filter.push(ch);
            if self.open && !self.is_incomplete {
                // 手元の候補だけで絞り込める = サーバーへ行かない (VS Code と同じ)
                self.refilter();
            } else {
                self.scheduled = Some((CompletionTrigger::Invoked, now));
            }
            return;
        }
        // 区切り文字は補完を閉じる
        self.dismiss();
    }

    /// Backspace 時。語が空になったら閉じる。
    pub fn on_backspace(&mut self, now: std::time::Instant) {
        if self.filter.pop().is_none() {
            self.dismiss();
            return;
        }
        if self.filter.is_empty() {
            self.dismiss();
        } else if self.is_incomplete {
            self.scheduled = Some((CompletionTrigger::Invoked, now));
        } else {
            self.refilter();
        }
    }

    /// 明示要求 (Ctrl+Space)。現在入力中の語を `word` に渡すとそのまま絞り込みに使う。
    pub fn invoke(&mut self, word: &str, now: std::time::Instant) {
        self.filter = word.to_string();
        self.scheduled = Some((CompletionTrigger::Invoked, now));
    }

    /// デバウンスが満了していれば要求種別を返す (返した時点で予約は消える)。
    /// 呼び出し側はこれを受けて `client.request_completion_at(..)` を呼び、
    /// 戻り値を [`Self::mark_sent`] へ渡すこと。
    pub fn due_request(
        &mut self,
        now: std::time::Instant,
        debounce: Duration,
    ) -> Option<CompletionTrigger> {
        let (trigger, at) = self.scheduled?;
        if now.saturating_duration_since(at) < debounce {
            return None;
        }
        self.scheduled = None;
        Some(trigger)
    }

    /// 送信結果を記録する。未対応/未起動/死亡なら補完 UI は開かない。
    pub fn mark_sent(&mut self, status: RequestStatus, anchor: Position) {
        match status {
            RequestStatus::Sent(id) => {
                self.in_flight = Some(id);
                self.anchor = Some(anchor);
            }
            _ => {
                // 待ち状態を残さない (スピナーが回りっぱなしにならない)
                self.in_flight = None;
                self.dismiss();
            }
        }
    }

    /// 応答を取り込む。**古い (superseded) リクエストの応答は捨てて false を返す**。
    pub fn apply_response(&mut self, id: u64, list: CompletionList) -> bool {
        if self.in_flight != Some(id) {
            return false;
        }
        self.in_flight = None;
        self.is_incomplete = list.is_incomplete;
        self.items = list.items;
        self.open = !self.items.is_empty();
        self.refilter();
        true
    }

    /// 絞り込み語を直接差し替える (UI が単語範囲を自前で持っている場合)。
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.refilter();
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    pub fn is_open(&self) -> bool {
        self.open && !self.order.is_empty()
    }

    pub fn is_incomplete(&self) -> bool {
        self.is_incomplete
    }

    pub fn in_flight(&self) -> Option<u64> {
        self.in_flight
    }

    pub fn anchor(&self) -> Option<Position> {
        self.anchor
    }

    /// 閉じる (Esc / カーソル移動 / 確定後)。予約と応答待ちも捨てる。
    pub fn dismiss(&mut self) {
        self.open = false;
        self.items.clear();
        self.order.clear();
        self.selected = 0;
        self.filter.clear();
        self.is_incomplete = false;
        self.scheduled = None;
        self.in_flight = None;
        self.anchor = None;
    }

    /// 表示順の候補。
    pub fn visible(&self) -> Vec<&CompletionItem> {
        self.order.iter().filter_map(|i| self.items.get(*i)).collect()
    }

    pub fn len(&self) -> usize {
        self.order.len()
    }

    pub fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    pub fn selected_index(&self) -> usize {
        self.selected
    }

    pub fn selected(&self) -> Option<&CompletionItem> {
        self.items.get(*self.order.get(self.selected)?)
    }

    /// 次候補へ (末尾で先頭へ回る)。
    pub fn select_next(&mut self) {
        if !self.order.is_empty() {
            self.selected = (self.selected + 1) % self.order.len();
        }
    }

    /// 前候補へ (先頭で末尾へ回る)。
    pub fn select_prev(&mut self) {
        if !self.order.is_empty() {
            self.selected = (self.selected + self.order.len() - 1) % self.order.len();
        }
    }

    /// 選択中の候補を確定するための TextEdit 群を返す。
    /// `fallback` は textEdit を持たない候補用の置換範囲 (入力中の語の範囲)。
    /// 返った配列をそのまま [`apply_text_edits`] へ渡せば additionalTextEdits も含めて適用される。
    pub fn accept(&self, fallback: Range) -> Option<Vec<TextEdit>> {
        Some(completion_edits(self.selected()?, fallback))
    }

    /// 絞り込み + 並び替え。
    ///
    /// 順位は (1) 完全前方一致 (2) 大小無視の前方一致 (3) 部分列マッチ の順で、
    /// 同点は sortText → label の辞書順。preselect は同点内でわずかに前へ出す。
    fn refilter(&mut self) {
        let mut scored: Vec<(i32, &str, &str, usize)> = Vec::with_capacity(self.items.len());
        let query = crate::fuzzy::PreparedQuery::new(&self.filter);
        let lower_filter = self.filter.to_lowercase();
        for (i, it) in self.items.iter().enumerate() {
            let key = it.filter_key();
            let score = if self.filter.is_empty() {
                0
            } else {
                let base = match query.score(key) {
                    Some(s) => s,
                    None => continue, // 部分列としても一致しない = 出さない
                };
                let bonus = if key.starts_with(&self.filter) {
                    2000
                } else if key.to_lowercase().starts_with(&lower_filter) {
                    1000
                } else {
                    0
                };
                base + bonus
            } + if it.preselect { 1 } else { 0 };
            scored.push((score, it.sort_key(), &it.label, i));
        }
        scored.sort_by(|a, b| {
            b.0.cmp(&a.0)
                .then_with(|| a.1.cmp(b.1))
                .then_with(|| a.2.cmp(b.2))
        });
        self.order = scored.into_iter().map(|(_, _, _, i)| i).collect();
        self.selected = 0;
        if self.order.is_empty() {
            self.open = false;
        }
    }
}

/// ホバーの状態。カーソルが止まったら要求し、動いたら捨てる。
#[derive(Debug, Default)]
pub struct HoverState {
    /// 最後にカーソルが動いた位置と時刻。
    resting: Option<(Position, std::time::Instant)>,
    requested_for: Option<Position>,
    in_flight: Option<u64>,
    info: Option<HoverInfo>,
    shown_at: Option<Position>,
}

impl HoverState {
    pub fn new() -> Self {
        Self::default()
    }

    /// カーソル/マウス位置が変わったら呼ぶ。同じ位置なら何もしない (時計を進めない)。
    pub fn on_move(&mut self, pos: Position, now: std::time::Instant) {
        if self.resting.map(|(p, _)| p) == Some(pos) {
            return;
        }
        self.resting = Some((pos, now));
        self.requested_for = None;
        self.in_flight = None;
        self.info = None;
        self.shown_at = None;
    }

    /// ホバーを閉じる (フォーカス喪失など)。
    pub fn dismiss(&mut self) {
        *self = Self::default();
    }

    /// デバウンス満了で要求すべき位置を返す (同じ位置で二度は返さない)。
    pub fn due_request(
        &mut self,
        now: std::time::Instant,
        debounce: Duration,
    ) -> Option<Position> {
        let (pos, at) = self.resting?;
        if self.requested_for == Some(pos) || now.saturating_duration_since(at) < debounce {
            return None;
        }
        self.requested_for = Some(pos);
        Some(pos)
    }

    pub fn mark_sent(&mut self, status: RequestStatus) {
        self.in_flight = status.id();
        if !status.is_sent() {
            self.requested_for = None;
        }
    }

    /// 応答を取り込む。古い応答・空本文は捨てる。
    pub fn apply_response(&mut self, id: u64, info: HoverInfo) -> bool {
        if self.in_flight != Some(id) {
            return false;
        }
        self.in_flight = None;
        if info.contents.trim().is_empty() {
            self.info = None;
            return true;
        }
        self.shown_at = self.requested_for;
        self.info = Some(info);
        true
    }

    /// 表示中のホバー本文 (markdown)。
    pub fn shown(&self) -> Option<&HoverInfo> {
        self.info.as_ref()
    }

    /// 表示中のホバーが指している位置。
    pub fn shown_at(&self) -> Option<Position> {
        self.shown_at
    }
}

/// カーソル下シンボルのハイライト状態。
///
/// **アイドル時にリクエストを撃たない**ための門番 (設計原則 3)。キャレットが
/// 止まってからデバウンス経過後に 1 回だけ要求し、同じ位置では二度と要求しない。
/// 表示中のハイライトはキャレット移動では消さない — 消すと打鍵のたびに点滅して
/// 「常時アニメーション」になるため、新しい応答が来たときに置き換える。
#[derive(Debug, Default)]
pub struct HighlightState {
    resting: Option<(Position, std::time::Instant)>,
    requested_for: Option<Position>,
    in_flight: Option<u64>,
    shown: Vec<DocumentHighlight>,
}

impl HighlightState {
    pub fn new() -> Self {
        Self::default()
    }

    /// キャレットが動いたら呼ぶ。同じ位置なら時計を進めない。
    pub fn on_move(&mut self, pos: Position, now: std::time::Instant) {
        if self.resting.map(|(p, _)| p) == Some(pos) {
            return;
        }
        self.resting = Some((pos, now));
        self.requested_for = None;
        self.in_flight = None;
    }

    /// 表示も予定も全部捨てる (タブ切替・本文編集・機能 OFF)。
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// デバウンス満了で要求すべき位置を返す (同じ位置で二度は返さない)。
    pub fn due_request(
        &mut self,
        now: std::time::Instant,
        debounce: Duration,
    ) -> Option<Position> {
        let (pos, at) = self.resting?;
        if self.requested_for == Some(pos) || now.saturating_duration_since(at) < debounce {
            return None;
        }
        self.requested_for = Some(pos);
        Some(pos)
    }

    /// 次のデバウンス満了までの残り時間。**再描画の予約を 1 回だけ入れる**ために使う。
    /// 予定が無い / もう要求済み / すでに満了しているなら `None` = 何も予約しない
    /// (常時再描画にならないことがこの `None` で担保される)。
    pub fn due_in(&self, now: std::time::Instant, debounce: Duration) -> Option<Duration> {
        let (pos, at) = self.resting?;
        if self.requested_for == Some(pos) {
            return None;
        }
        let elapsed = now.saturating_duration_since(at);
        (elapsed < debounce).then(|| debounce - elapsed)
    }

    pub fn mark_sent(&mut self, status: RequestStatus) {
        self.in_flight = status.id();
        if !status.is_sent() {
            self.requested_for = None;
        }
    }

    /// 応答を取り込む。古い応答は捨てる。取り込んだら true。
    pub fn apply_response(&mut self, id: u64, hl: Vec<DocumentHighlight>) -> bool {
        if self.in_flight != Some(id) {
            return false;
        }
        self.in_flight = None;
        self.shown = hl;
        true
    }

    pub fn shown(&self) -> &[DocumentHighlight] {
        &self.shown
    }

    /// 飛行中の要求 ID (応答の新旧を UI 側で見分けるため)。
    pub fn in_flight(&self) -> Option<u64> {
        self.in_flight
    }
}

/// サーバーが落ちたときの再起動方針 (指数バックオフ + 打ち切り)。
///
/// 落ちるたびに待ち時間を倍にしていき、`limit` 回落ちたら諦める。
/// 諦めないと、起動即クラッシュするサーバーで再起動ループになり CPU を焼く。
#[derive(Debug, Clone)]
pub struct RestartPolicy {
    failures: u32,
    next_at: Option<std::time::Instant>,
    base: Duration,
    max: Duration,
    limit: u32,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        RestartPolicy::new(Duration::from_millis(500), Duration::from_secs(30), 5)
    }
}

impl RestartPolicy {
    pub fn new(base: Duration, max: Duration, limit: u32) -> Self {
        RestartPolicy {
            failures: 0,
            next_at: None,
            base,
            max,
            limit,
        }
    }

    /// サーバーの死亡を記録する。次の再起動可能時刻が後ろへ伸びる。
    pub fn record_exit(&mut self, now: std::time::Instant) {
        self.failures = self.failures.saturating_add(1);
        self.next_at = Some(now + self.backoff());
    }

    /// initialize まで到達したら呼ぶ。バックオフを解除する。
    pub fn record_ready(&mut self) {
        self.failures = 0;
        self.next_at = None;
    }

    /// 今このタイミングで再起動してよいか。
    pub fn should_restart(&self, now: std::time::Instant) -> bool {
        if self.gave_up() {
            return false;
        }
        match self.next_at {
            Some(t) => now >= t,
            None => true,
        }
    }

    /// 現在の待ち時間 (base * 2^(failures-1)、max で頭打ち)。
    pub fn backoff(&self) -> Duration {
        if self.failures == 0 {
            return Duration::ZERO;
        }
        let shift = (self.failures - 1).min(16);
        let mult = 1u32 << shift;
        self.base.saturating_mul(mult).min(self.max)
    }

    /// 上限まで失敗して諦めた状態か。
    pub fn gave_up(&self) -> bool {
        self.failures >= self.limit
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用: 応答を受け付けられる状態にする (リクエスト送信済みの再現)。
    fn pend(shared: &Shared, id: u64, kind: Pending) {
        shared.remember(id, kind);
    }

    /// 合成 JSON 応答を 1 本流し込む。
    fn reply(shared: &Arc<Shared>, id: u64, kind: Pending, result: Value) {
        pend(shared, id, kind);
        begin_slot(shared, kind, id);
        let (tx, _rx) = mpsc::channel();
        let raw = json!({"jsonrpc":"2.0","id":id,"result":result}).to_string();
        handle_message(&raw, shared, &tx);
    }

    fn begin_slot(shared: &Shared, kind: Pending, id: u64) {
        match kind {
            Pending::Completion => shared.completion.begin(id),
            Pending::Hover => shared.hover.begin(id),
            Pending::Definition => shared.definition.begin(id),
            Pending::References => shared.references.begin(id),
            Pending::Highlight => shared.highlight.begin(id),
            Pending::PrepareRename => shared.prepare_rename.begin(id),
            Pending::Rename => shared.rename.begin(id),
            Pending::Formatting => shared.formatting.begin(id),
            Pending::Symbols => shared.symbols.begin(id),
            Pending::Signature => shared.signature.begin(id),
            Pending::CodeAction => shared.code_action.begin(id),
            Pending::Initialize => {}
        }
    }

    fn pos(line: usize, character: usize) -> Position {
        Position::new(line, character)
    }

    fn rng(sl: usize, sc: usize, el: usize, ec: usize) -> Range {
        Range::new(pos(sl, sc), pos(el, ec))
    }

    fn item(label: &str) -> CompletionItem {
        CompletionItem {
            label: label.to_string(),
            insert_text: label.to_string(),
            ..CompletionItem::default()
        }
    }

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
        pend(&shared, 1, Pending::Initialize);
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

    // =======================================================================
    // UTF-16 ↔ byte 変換
    // =======================================================================

    #[test]
    fn utf16_byte_ascii_and_lines() {
        let t = "abc\ndef";
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 0)), 0);
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 3)), 3); // 行末
        assert_eq!(lsp_pos_to_byte_index(t, pos(1, 0)), 4);
        assert_eq!(lsp_pos_to_byte_index(t, pos(1, 3)), 7); // ファイル末尾
        assert_eq!(byte_index_to_lsp_pos(t, 0), pos(0, 0));
        assert_eq!(byte_index_to_lsp_pos(t, 4), pos(1, 0));
        assert_eq!(byte_index_to_lsp_pos(t, 7), pos(1, 3));
    }

    #[test]
    fn utf16_byte_japanese() {
        // "あ" = 3 byte / 1 UTF-16 unit
        let t = "あいう\nかき";
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 0)), 0);
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 1)), 3);
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 3)), 9);
        assert_eq!(lsp_pos_to_byte_index(t, pos(1, 1)), 13); // 9+1(改行)+3
        assert_eq!(byte_index_to_lsp_pos(t, 6), pos(0, 2));
        assert_eq!(byte_index_to_lsp_pos(t, 10), pos(1, 0));
        // 往復
        for c in 0..=3 {
            let b = lsp_pos_to_byte_index(t, pos(0, c));
            assert_eq!(byte_index_to_lsp_pos(t, b), pos(0, c), "col {c}");
        }
    }

    #[test]
    fn utf16_byte_emoji_surrogate_pair() {
        // "🎉" = 4 byte / 2 UTF-16 unit
        let t = "a🎉b";
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 0)), 0);
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 1)), 1); // 絵文字の先頭
        // サロゲートペアの途中 (col=2) は文字の先頭へ丸める = byte 1
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 2)), 1);
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 3)), 5); // 'b'
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 4)), 6); // 行末
        assert_eq!(byte_index_to_lsp_pos(t, 5), pos(0, 3));
        // 丸めた byte index は必ず char 境界 (String の操作で panic しない)
        assert!(t.is_char_boundary(lsp_pos_to_byte_index(t, pos(0, 2))));
    }

    #[test]
    fn utf16_byte_combining_marks() {
        // 結合文字は独立した code point: "が" (か + 濁点) は 2 文字 / 2 UTF-16 unit
        let t = "か\u{3099}ぎ";
        assert_eq!(t.chars().count(), 3);
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 1)), 3); // 濁点の先頭
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 2)), 6); // "ぎ" の先頭
        assert_eq!(byte_index_to_lsp_pos(t, 3), pos(0, 1));
    }

    #[test]
    fn utf16_byte_clamps_out_of_range() {
        let t = "あ🎉\nx";
        // 行末を超える col → 改行の直前
        assert_eq!(lsp_pos_to_byte_index(t, pos(0, 99)), 7); // 3 + 4
        // 存在しない行 → テキスト末尾
        assert_eq!(lsp_pos_to_byte_index(t, pos(9, 0)), t.len());
        // 空テキスト
        assert_eq!(lsp_pos_to_byte_index("", pos(0, 0)), 0);
        assert_eq!(lsp_pos_to_byte_index("", pos(3, 3)), 0);
        assert_eq!(byte_index_to_lsp_pos("", 5), pos(0, 0));
        // 末尾の改行だけの行 (EOF が新しい行になるケース)
        assert_eq!(lsp_pos_to_byte_index("a\n", pos(1, 0)), 2);
    }

    // =======================================================================
    // TextEdit の適用
    // =======================================================================

    #[test]
    fn apply_edits_back_to_front() {
        let text = "let a = 1;\nlet b = 2;\n";
        // わざと「前 → 後」の順で渡す。後ろから適用されないと 2 つ目がずれる
        let edits = vec![
            TextEdit::new(rng(0, 4, 0, 5), "alpha"),
            TextEdit::new(rng(1, 4, 1, 5), "beta"),
        ];
        assert_eq!(
            apply_text_edits(text, &edits),
            "let alpha = 1;\nlet beta = 2;\n"
        );
    }

    #[test]
    fn apply_edits_same_position_inserts_keep_array_order() {
        // 仕様: 同じ位置への挿入は配列の順に並ぶ
        let edits = vec![
            TextEdit::new(rng(0, 1, 0, 1), "X"),
            TextEdit::new(rng(0, 1, 0, 1), "Y"),
        ];
        assert_eq!(apply_text_edits("AB", &edits), "AXYB");
    }

    #[test]
    fn apply_edits_on_japanese_uses_utf16_columns() {
        let text = "こんにちは世界";
        // UTF-16 col 5..7 = "世界"
        let edits = vec![TextEdit::new(rng(0, 5, 0, 7), "せかい")];
        assert_eq!(apply_text_edits(text, &edits), "こんにちはせかい");
    }

    #[test]
    fn apply_edits_overlapping_is_clamped_not_panicking() {
        // 仕様違反の重なり。panic せず決定的な結果になること
        let edits = vec![
            TextEdit::new(rng(0, 0, 0, 5), "X"),
            TextEdit::new(rng(0, 3, 0, 8), "Y"),
        ];
        let out = apply_text_edits("0123456789", &edits);
        assert!(!out.is_empty());
        assert_eq!(out, "XY89");
    }

    #[test]
    fn apply_edits_empty_and_reversed_range() {
        assert_eq!(apply_text_edits("abc", &[]), "abc");
        // start > end のサーバーバグ: 入れ替えて扱う
        let e = vec![TextEdit::new(Range::new(pos(0, 3), pos(0, 1)), "Z")];
        assert_eq!(apply_text_edits("abcd", &e), "aZd");
    }

    // =======================================================================
    // 補完: パース / 絞り込み / 打ち切り / 適用
    // =======================================================================

    #[test]
    fn parse_completion_all_item_shapes() {
        let v = json!({
            "isIncomplete": true,
            "items": [
                { "label": "plain" },
                { "label": "with_insert", "insertText": "with_insert()", "kind": 3,
                  "detail": "fn()", "documentation": "説明" },
                { "label": "edited", "textEdit": {
                      "range": {"start":{"line":1,"character":2},"end":{"line":1,"character":5}},
                      "newText": "edited!" },
                  "additionalTextEdits": [{
                      "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":0}},
                      "newText": "use foo::Bar;\n" }],
                  "sortText": "0001", "filterText": "edt", "preselect": true },
                { "label": "snip", "insertTextFormat": 2, "insertText": "snip($1)",
                  "documentation": {"kind":"markdown","value":"**md**"}, "tags": [1] },
                { "label": "ir", "textEdit": {
                      "insert": {"start":{"line":0,"character":1},"end":{"line":0,"character":1}},
                      "replace": {"start":{"line":0,"character":1},"end":{"line":0,"character":9}},
                      "newText": "ir" } }
            ]
        });
        let list = parse_completion_list(&v);
        assert!(list.is_incomplete);
        assert_eq!(list.items.len(), 5);

        assert_eq!(list.items[0].insert_text, "plain"); // label へフォールバック
        assert_eq!(list.items[0].filter_key(), "plain");

        assert_eq!(list.items[1].insert_text, "with_insert()");
        assert_eq!(list.items[1].detail, "fn()");
        assert_eq!(list.items[1].documentation, "説明");
        assert_eq!(list.items[1].kind, 3);

        let e = &list.items[2];
        assert_eq!(e.text_edit.as_ref().unwrap().range, rng(1, 2, 1, 5));
        assert_eq!(e.insert_text, "edited!"); // textEdit.newText へフォールバック
        assert_eq!(e.additional_text_edits.len(), 1);
        assert_eq!(e.sort_key(), "0001");
        assert_eq!(e.filter_key(), "edt");
        assert!(e.preselect);

        assert!(list.items[3].is_snippet);
        assert!(list.items[3].deprecated);
        assert_eq!(list.items[3].documentation, "**md**");

        // InsertReplaceEdit は insert 側を採る
        assert_eq!(list.items[4].text_edit.as_ref().unwrap().range, rng(0, 1, 0, 1));

        // 配列形式 (CompletionItem[]) も読める
        let arr = parse_completion_list(&json!([{"label":"a"}]));
        assert!(!arr.is_incomplete);
        assert_eq!(arr.items.len(), 1);
        // null / 壊れた形でも panic しない
        assert!(parse_completion_list(&Value::Null).items.is_empty());
        assert!(parse_completion_list(&json!({"items": 3})).items.is_empty());
    }

    #[test]
    fn completion_state_filters_and_sorts() {
        let mut st = CompletionState::new();
        let list = CompletionList {
            is_incomplete: false,
            items: vec![
                item("format!"),
                item("from_str"),
                item("Foo"),
                CompletionItem {
                    sort_text: Some("0000".into()),
                    ..item("fold")
                },
                item("zzz"),
            ],
        };
        st.mark_sent(RequestStatus::Sent(7), pos(0, 0));
        assert!(st.apply_response(7, list));
        assert_eq!(st.len(), 5);

        st.set_filter("fo");
        let vis: Vec<&str> = st.visible().iter().map(|i| i.label.as_str()).collect();
        // "zzz" は落ちる。前方一致が先、その中で sortText 順
        assert!(!vis.contains(&"zzz"));
        assert_eq!(vis[0], "fold"); // sortText "0000"
        assert!(vis.contains(&"format!") && vis.contains(&"from_str"));
        // 大小無視の前方一致 "Foo" は完全一致組より後ろ
        assert!(
            vis.iter().position(|s| *s == "Foo").unwrap()
                > vis.iter().position(|s| *s == "format!").unwrap()
        );

        // 選択の巡回
        st.select_next();
        assert_eq!(st.selected_index(), 1);
        st.select_prev();
        st.select_prev();
        assert_eq!(st.selected_index(), st.len() - 1);

        // 一致 0 件なら閉じる
        st.set_filter("qqqq");
        assert!(!st.is_open());
    }

    #[test]
    fn completion_state_debounce_and_trigger_chars() {
        let caps = ServerCaps {
            completion: true,
            completion_trigger_chars: vec!['.'],
            ..ServerCaps::default()
        };
        let t0 = std::time::Instant::now();
        let mut st = CompletionState::new();

        // 識別子入力 → 予約されるがデバウンス前は返らない
        st.on_typed('a', &caps, t0);
        assert!(st.due_request(t0, COMPLETION_DEBOUNCE).is_none());
        assert_eq!(
            st.due_request(t0 + COMPLETION_DEBOUNCE, COMPLETION_DEBOUNCE),
            Some(CompletionTrigger::Invoked)
        );
        // 一度返したら消える
        assert!(st.due_request(t0 + COMPLETION_DEBOUNCE, COMPLETION_DEBOUNCE).is_none());

        // トリガ文字は語を仕切り直して必ず再要求
        st.on_typed('.', &caps, t0);
        assert_eq!(st.filter(), "");
        assert_eq!(
            st.due_request(t0 + COMPLETION_DEBOUNCE, COMPLETION_DEBOUNCE),
            Some(CompletionTrigger::TriggerChar('.'))
        );

        // 区切り文字は閉じる
        st.on_typed('a', &caps, t0);
        st.on_typed(' ', &caps, t0);
        assert!(!st.is_open());
        assert!(st.due_request(t0 + COMPLETION_DEBOUNCE, COMPLETION_DEBOUNCE).is_none());

        // 能力の無いサーバーでは何も予約しない
        let mut st2 = CompletionState::new();
        st2.on_typed('a', &ServerCaps::default(), t0);
        assert!(st2.due_request(t0 + COMPLETION_DEBOUNCE, COMPLETION_DEBOUNCE).is_none());
    }

    #[test]
    fn completion_state_open_list_filters_locally_but_incomplete_refetches() {
        let caps = ServerCaps {
            completion: true,
            ..ServerCaps::default()
        };
        let t0 = std::time::Instant::now();
        let late = t0 + COMPLETION_DEBOUNCE;

        // is_incomplete=false → ローカル絞り込みだけ (再要求しない)
        let mut st = CompletionState::new();
        st.mark_sent(RequestStatus::Sent(1), pos(0, 0));
        st.apply_response(
            1,
            CompletionList {
                is_incomplete: false,
                items: vec![item("foo"), item("foobar")],
            },
        );
        st.on_typed('f', &caps, t0);
        assert!(st.due_request(late, COMPLETION_DEBOUNCE).is_none());
        assert_eq!(st.len(), 2);

        // is_incomplete=true → 入力のたびに再要求
        let mut st2 = CompletionState::new();
        st2.mark_sent(RequestStatus::Sent(1), pos(0, 0));
        st2.apply_response(
            1,
            CompletionList {
                is_incomplete: true,
                items: vec![item("foo")],
            },
        );
        st2.on_typed('f', &caps, t0);
        assert_eq!(st2.due_request(late, COMPLETION_DEBOUNCE), Some(CompletionTrigger::Invoked));
    }

    #[test]
    fn completion_superseded_response_is_dropped() {
        // 状態層: 古い id の応答は取り込まない
        let mut st = CompletionState::new();
        st.mark_sent(RequestStatus::Sent(1), pos(0, 0));
        st.mark_sent(RequestStatus::Sent(2), pos(0, 1)); // 2 が最新
        assert!(!st.apply_response(1, CompletionList {
            is_incomplete: false,
            items: vec![item("old")],
        }));
        assert!(st.is_empty());
        assert!(st.apply_response(2, CompletionList {
            is_incomplete: false,
            items: vec![item("new")],
        }));
        assert_eq!(st.visible()[0].label, "new");

        // 受信側: Slot も最新 id 以外を捨てる
        let shared = Arc::new(Shared::new());
        shared.completion.begin(10);
        shared.completion.begin(11); // 10 は superseded
        let (tx, _rx) = mpsc::channel();
        pend(&shared, 10, Pending::Completion);
        handle_message(
            &json!({"jsonrpc":"2.0","id":10,"result":[{"label":"stale"}]}).to_string(),
            &shared,
            &tx,
        );
        assert!(shared.completion.take().is_none(), "古い応答は捨てられる");
        pend(&shared, 11, Pending::Completion);
        handle_message(
            &json!({"jsonrpc":"2.0","id":11,"result":[{"label":"fresh"}]}).to_string(),
            &shared,
            &tx,
        );
        assert_eq!(shared.completion.take().unwrap().items[0].label, "fresh");
    }

    #[test]
    fn completion_accept_applies_text_edit_and_additional_edits() {
        let text = "use std::io;\n\nfn main() { HashM }\n";
        // "HashM" は行 2 の col 12..17
        let it = CompletionItem {
            label: "HashMap".into(),
            insert_text: "HashMap".into(),
            text_edit: Some(TextEdit::new(rng(2, 12, 2, 17), "HashMap")),
            additional_text_edits: vec![TextEdit::new(
                rng(1, 0, 1, 0),
                "use std::collections::HashMap;\n",
            )],
            ..CompletionItem::default()
        };
        let edits = completion_edits(&it, rng(2, 12, 2, 17));
        assert_eq!(edits.len(), 2);
        let out = apply_text_edits(text, &edits);
        assert_eq!(
            out,
            "use std::io;\nuse std::collections::HashMap;\n\nfn main() { HashMap }\n"
        );

        // textEdit が無い候補は fallback 範囲を使う
        let plain = item("push_str");
        let out2 = apply_text_edits("s.pu", &completion_edits(&plain, rng(0, 2, 0, 4)));
        assert_eq!(out2, "s.push_str");

        // CompletionState 経由 (選択中の候補を確定)
        let mut st = CompletionState::new();
        st.mark_sent(RequestStatus::Sent(1), pos(2, 12));
        st.apply_response(
            1,
            CompletionList {
                is_incomplete: false,
                items: vec![it],
            },
        );
        let edits = st.accept(rng(2, 12, 2, 17)).expect("選択候補がある");
        assert_eq!(apply_text_edits(text, &edits), out);
    }

    // =======================================================================
    // ホバー
    // =======================================================================

    #[test]
    fn hover_normalizes_all_content_shapes() {
        // MarkupContent
        let h = parse_hover(&json!({
            "contents": {"kind":"markdown","value":"# 見出し"},
            "range": {"start":{"line":1,"character":2},"end":{"line":1,"character":6}}
        }));
        assert_eq!(h.contents, "# 見出し");
        assert_eq!(h.range, Some(rng(1, 2, 1, 6)));

        // MarkedString (string)
        assert_eq!(parse_hover(&json!({"contents": "plain"})).contents, "plain");

        // MarkedString {language, value} → コードフェンス
        assert_eq!(
            parse_hover(&json!({"contents": {"language":"rust","value":"fn f()"}})).contents,
            "```rust\nfn f()\n```"
        );

        // 配列 (混在)
        let mixed = parse_hover(&json!({"contents": [
            {"language":"rust","value":"fn f()"},
            "docs",
            {"kind":"markdown","value":""}
        ]}));
        assert_eq!(mixed.contents, "```rust\nfn f()\n```\n\ndocs");

        // null / 欠損でも panic しない
        assert_eq!(parse_hover(&Value::Null).contents, "");
        assert_eq!(parse_hover(&json!({"contents": 42})).contents, "");
    }

    #[test]
    fn hover_state_debounces_and_cancels_on_move() {
        let t0 = std::time::Instant::now();
        let mut hs = HoverState::new();
        hs.on_move(pos(1, 1), t0);
        assert!(hs.due_request(t0, HOVER_DEBOUNCE).is_none());
        assert_eq!(hs.due_request(t0 + HOVER_DEBOUNCE, HOVER_DEBOUNCE), Some(pos(1, 1)));
        // 同じ位置で二度は要求しない
        assert!(hs.due_request(t0 + HOVER_DEBOUNCE * 2, HOVER_DEBOUNCE).is_none());

        hs.mark_sent(RequestStatus::Sent(5));
        // 移動したら応答を捨てる
        hs.on_move(pos(1, 2), t0 + HOVER_DEBOUNCE);
        assert!(!hs.apply_response(
            5,
            HoverInfo {
                contents: "old".into(),
                range: None
            }
        ));
        assert!(hs.shown().is_none());

        // 新しい要求は通る
        assert_eq!(
            hs.due_request(t0 + HOVER_DEBOUNCE * 3, HOVER_DEBOUNCE),
            Some(pos(1, 2))
        );
        hs.mark_sent(RequestStatus::Sent(6));
        assert!(hs.apply_response(
            6,
            HoverInfo {
                contents: "new".into(),
                range: None
            }
        ));
        assert_eq!(hs.shown().unwrap().contents, "new");
        assert_eq!(hs.shown_at(), Some(pos(1, 2)));

        // 空本文は表示しない
        hs.on_move(pos(2, 0), t0);
        hs.due_request(t0 + HOVER_DEBOUNCE, HOVER_DEBOUNCE);
        hs.mark_sent(RequestStatus::Sent(7));
        hs.apply_response(7, HoverInfo::default());
        assert!(hs.shown().is_none());
    }

    // =======================================================================
    // 参照検索 / ハイライト
    // =======================================================================

    #[test]
    fn references_group_by_file_sorted() {
        let v = json!([
            {"uri":"file:///w/b.rs","range":{"start":{"line":9,"character":0},"end":{"line":9,"character":3}}},
            {"uri":"file:///w/a.rs","range":{"start":{"line":5,"character":4},"end":{"line":5,"character":7}}},
            {"uri":"file:///w/a.rs","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":3}}},
            {"uri":"file:///w/a.rs","range":{"start":{"line":1,"character":0},"end":{"line":1,"character":3}}}
        ]);
        let groups = group_locations(parse_locations(&v));
        assert_eq!(groups.len(), 2);
        assert!(groups[0].path.ends_with("a.rs"));
        // 位置順・重複は畳まれる
        assert_eq!(groups[0].locations, vec![rng(1, 0, 1, 3), rng(5, 4, 5, 7)]);
        assert_eq!(groups[1].locations.len(), 1);

        // LocationLink 形式 / 単体 Location / null
        let link = parse_locations(&json!([{
            "targetUri":"file:///w/c.rs",
            "targetRange":{"start":{"line":0,"character":0},"end":{"line":9,"character":0}},
            "targetSelectionRange":{"start":{"line":2,"character":3},"end":{"line":2,"character":8}}
        }]));
        assert_eq!(link[0].range, rng(2, 3, 2, 8)); // selection を優先
        assert_eq!(parse_locations(&Value::Null).len(), 0);
        assert_eq!(group_locations(Vec::new()).len(), 0);
    }

    #[test]
    fn document_highlights_parse() {
        let hl = parse_document_highlights(&json!([
            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":2}},"kind":2},
            {"range":{"start":{"line":3,"character":1},"end":{"line":3,"character":3}}}
        ]));
        assert_eq!(hl.len(), 2);
        assert_eq!(hl[0].kind, 2);
        assert_eq!(hl[1].kind, 1); // 既定は Text
        assert!(parse_document_highlights(&Value::Null).is_empty());
    }

    // =======================================================================
    // リネーム
    // =======================================================================

    #[test]
    fn rename_changes_form_normalized_and_applied_back_to_front() {
        // "changes" 形式。同一行に 2 箇所 (前から適用するとずれる)
        let v = json!({"changes": {
            "file:///w/a.rs": [
                {"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":7}},"newText":"newname"},
                {"range":{"start":{"line":0,"character":13},"end":{"line":0,"character":16}},"newText":"newname"}
            ]
        }});
        let plan = parse_workspace_edit(&v);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.edit_count(), 2);
        // 後ろから適用できる順 = 開始位置の降順
        assert_eq!(plan.files[0].edits[0].range.start, pos(0, 13));
        assert_eq!(plan.files[0].edits[1].range.start, pos(0, 4));
        let text = "let old = 1; old + 1";
        assert_eq!(
            apply_file_edits(text, &plan.files[0]),
            "let newname = 1; newname + 1"
        );
    }

    #[test]
    fn rename_document_changes_form_and_resource_ops() {
        let v = json!({"documentChanges": [
            {"textDocument": {"uri":"file:///w/a.rs","version":3},
             "edits": [
                {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":3}},"newText":"AAA"},
                {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"BBB"}
             ]},
            {"textDocument": {"uri":"file:///w/b.rs","version":1},
             "edits": [{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"Z"}]},
            {"kind":"rename","oldUri":"file:///w/a.rs","newUri":"file:///w/c.rs"}
        ]});
        let plan = parse_workspace_edit(&v);
        assert!(plan.has_resource_ops);
        assert_eq!(plan.files.len(), 2);
        let a = plan.files.iter().find(|f| f.path.ends_with("a.rs")).unwrap();
        assert_eq!(a.edits[0].range.start, pos(1, 0)); // 降順に整列済み
        assert_eq!(apply_file_edits("foo\nbar\n", a), "BBB\nAAA\n");

        // 空 / 壊れた WorkspaceEdit
        assert!(parse_workspace_edit(&Value::Null).is_empty());
        assert!(parse_workspace_edit(&json!({"changes": 7})).is_empty());
    }

    #[test]
    fn rename_adjacent_edits_do_not_corrupt_each_other() {
        // 隣接する編集 (端が接する) を後ろから適用しても互いを壊さない
        let v = json!({"changes": {"file:///w/a.rs": [
            {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":3}},"newText":"XXXX"},
            {"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":6}},"newText":"Y"}
        ]}});
        let plan = parse_workspace_edit(&v);
        assert_eq!(apply_file_edits("abcdef", &plan.files[0]), "XXXXY");
    }

    #[test]
    fn prepare_rename_shapes() {
        assert_eq!(
            parse_prepare_rename(&json!({"start":{"line":1,"character":2},
                                         "end":{"line":1,"character":5}})),
            Some(rng(1, 2, 1, 5))
        );
        assert_eq!(
            parse_prepare_rename(&json!({"range":{"start":{"line":0,"character":0},
                                                  "end":{"line":0,"character":4}},
                                         "placeholder":"foo"})),
            Some(rng(0, 0, 0, 4))
        );
        assert_eq!(
            parse_prepare_rename(&json!({"defaultBehavior": true})),
            Some(Range::default())
        );
        assert_eq!(parse_prepare_rename(&Value::Null), None);
        assert_eq!(parse_prepare_rename(&json!({"nonsense": 1})), None);
    }

    // =======================================================================
    // 整形
    // =======================================================================

    #[test]
    fn formatting_edits_applied_in_order() {
        // rustfmt 風: 複数行にまたがる編集がばらばらの順で返る
        let v = json!([
            {"range":{"start":{"line":2,"character":0},"end":{"line":2,"character":6}},"newText":"    "},
            {"range":{"start":{"line":0,"character":3},"end":{"line":0,"character":5}},"newText":" "},
            {"range":{"start":{"line":1,"character":0},"end":{"line":1,"character":2}},"newText":""}
        ]);
        let edits = parse_text_edits(&v);
        assert_eq!(edits.len(), 3);
        let text = "let  x=1;\n  second\n      third\n";
        assert_eq!(
            apply_text_edits(text, &edits),
            "let x=1;\nsecond\n    third\n"
        );
        assert!(parse_text_edits(&Value::Null).is_empty());
    }

    #[test]
    fn format_options_serialize() {
        let o = FormatOptions::default();
        let j = o.to_json();
        assert_eq!(j.get("tabSize").and_then(|v| v.as_u64()), Some(4));
        assert_eq!(j.get("insertSpaces").and_then(|v| v.as_bool()), Some(true));
        assert!(j.get("insertFinalNewline").is_some());
    }

    // =======================================================================
    // ドキュメントシンボル
    // =======================================================================

    /// 階層形式とフラット形式で **同じ木** になること。
    #[test]
    fn document_symbols_both_shapes_yield_same_tree() {
        let hierarchical = json!([
            {"name":"Foo","kind":5,"detail":"struct",
             "range":{"start":{"line":0,"character":0},"end":{"line":9,"character":1}},
             "selectionRange":{"start":{"line":0,"character":0},"end":{"line":9,"character":1}},
             "children":[
                {"name":"bar","kind":6,"detail":"",
                 "range":{"start":{"line":1,"character":2},"end":{"line":4,"character":3}},
                 "selectionRange":{"start":{"line":1,"character":2},"end":{"line":4,"character":3}},
                 "children":[
                    {"name":"inner","kind":12,"detail":"",
                     "range":{"start":{"line":2,"character":4},"end":{"line":3,"character":5}},
                     "selectionRange":{"start":{"line":2,"character":4},"end":{"line":3,"character":5}}}
                 ]},
                {"name":"baz","kind":6,"detail":"",
                 "range":{"start":{"line":5,"character":2},"end":{"line":6,"character":3}},
                 "selectionRange":{"start":{"line":5,"character":2},"end":{"line":6,"character":3}}}
             ]},
            {"name":"top","kind":12,"detail":"",
             "range":{"start":{"line":11,"character":0},"end":{"line":12,"character":1}},
             "selectionRange":{"start":{"line":11,"character":0},"end":{"line":12,"character":1}}}
        ]);
        let flat = json!([
            {"name":"top","kind":12,
             "location":{"uri":"file:///w/a.rs",
                "range":{"start":{"line":11,"character":0},"end":{"line":12,"character":1}}}},
            {"name":"inner","kind":12,"containerName":"bar",
             "location":{"uri":"file:///w/a.rs",
                "range":{"start":{"line":2,"character":4},"end":{"line":3,"character":5}}}},
            {"name":"Foo","kind":5,"detail":"struct",
             "location":{"uri":"file:///w/a.rs",
                "range":{"start":{"line":0,"character":0},"end":{"line":9,"character":1}}}},
            {"name":"baz","kind":6,"containerName":"Foo",
             "location":{"uri":"file:///w/a.rs",
                "range":{"start":{"line":5,"character":2},"end":{"line":6,"character":3}}}},
            {"name":"bar","kind":6,"containerName":"Foo",
             "location":{"uri":"file:///w/a.rs",
                "range":{"start":{"line":1,"character":2},"end":{"line":4,"character":3}}}}
        ]);

        let a = parse_document_symbols(&hierarchical);
        let b = parse_document_symbols(&flat);

        // 形 (深さ・種別・名前の並び) が一致すること
        fn shape(ns: &[SymbolNode], depth: usize, out: &mut Vec<String>) {
            for n in ns {
                out.push(format!("{depth}:{}:{}", n.kind, n.name));
                shape(&n.children, depth + 1, out);
            }
        }
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        shape(&a, 0, &mut sa);
        shape(&b, 0, &mut sb);
        assert_eq!(sa, sb);
        assert_eq!(sa, ["0:5:Foo", "1:6:bar", "2:12:inner", "1:6:baz", "0:12:top"]);
        assert_eq!(a.len(), 2);
        assert_eq!(a[0].name, "Foo");
        assert_eq!(a[0].children.len(), 2);
        assert_eq!(a[0].children[0].children[0].name, "inner");
        assert_eq!(a[1].name, "top");

        // 階層形式は selectionRange を保つ。平坦形式は range で代用する
        assert_eq!(b[0].selection_range, b[0].range);
        assert_eq!(a[0].detail, "struct");

        // 壊れた入力
        assert!(parse_document_symbols(&Value::Null).is_empty());
        assert!(parse_document_symbols(&json!([{"name":"x"}])).is_empty());
    }

    // =======================================================================
    // シグネチャヘルプ / コードアクション
    // =======================================================================

    #[test]
    fn signature_help_parses_label_offsets() {
        let v = json!({
            "signatures": [{
                "label": "fn 挿入(値: i32, 名: &str)",
                "documentation": {"kind":"markdown","value":"doc"},
                "parameters": [
                    {"label": [6, 12]},
                    {"label": "名: &str", "documentation": "名前"}
                ]
            }],
            "activeSignature": 0,
            "activeParameter": 1
        });
        let sh = parse_signature_help(&v);
        assert_eq!(sh.signatures.len(), 1);
        assert_eq!(sh.active_parameter, Some(1));
        assert_eq!(sh.signatures[0].documentation, "doc");
        // UTF-16 オフセットで切り出す (日本語を含む label でもずれない)
        assert_eq!(sh.signatures[0].parameters[0].label, "値: i32");
        assert_eq!(sh.signatures[0].parameters[1].label, "名: &str");
        assert_eq!(parse_signature_help(&Value::Null), SignatureHelp::default());
    }

    #[test]
    fn code_actions_parse_both_union_shapes() {
        let v = json!([
            {"title":"Fix it","kind":"quickfix","isPreferred":true,
             "edit":{"changes":{"file:///w/a.rs":[
                {"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"newText":"X"}]}}},
            {"title":"Organize imports","command":"editor.organizeImports","arguments":[1]},
            {"title":"Lazy","kind":"refactor"}
        ]);
        let acts = parse_code_actions(&v);
        assert_eq!(acts.len(), 3);
        assert!(acts[0].is_preferred);
        assert_eq!(acts[0].edit.edit_count(), 1);
        // Command 形式
        assert_eq!(acts[1].command.as_ref().unwrap().command, "editor.organizeImports");
        // edit も command も無い = resolve が要る
        assert!(acts[2].needs_resolve);
        assert!(parse_code_actions(&Value::Null).is_empty());
    }

    // =======================================================================
    // 見せ方の純関数 (並べ替え / 切り詰め / 塗る場所)
    // =======================================================================

    fn act(title: &str, kind: &str, preferred: bool) -> CodeAction {
        CodeAction {
            title: title.into(),
            kind: kind.into(),
            is_preferred: preferred,
            ..CodeAction::default()
        }
    }

    #[test]
    fn one_line_label_は改行を潰して文字単位で切る() {
        // (入力, 上限, 期待)
        let cases: &[(&str, usize, &str)] = &[
            ("", 10, ""),
            ("hello", 10, "hello"),
            ("  a \n b\t\tc  ", 20, "a b c"),
            ("\n\n\n", 20, ""),
            ("abcdefghij", 5, "abcd…"),
            // 日本語は char 単位で切る (byte で切ると壊れる)
            ("あいうえおかきくけこ", 4, "あいう…"),
            ("あいう", 3, "あいう"),
            ("なんでも", 0, ""),
            ("絵文字🙂🙂🙂🙂", 3, "絵文…"),
        ];
        for (src, max, want) in cases {
            assert_eq!(&one_line_label(src, *max), want, "src={src:?} max={max}");
        }
        // 巨大入力でも上限を必ず守る
        let huge = "x".repeat(100_000);
        assert_eq!(one_line_label(&huge, ACTION_TITLE_MAX).chars().count(), ACTION_TITLE_MAX);
    }

    #[test]
    fn rank_code_actions_は押したい順に安定整列して上限で切る() {
        let v = vec![
            act("refactor", "refactor.extract", false),
            act("", "quickfix", false),          // タイトル空 = 落とす
            act("   ", "quickfix", false),       // 空白だけ = 落とす
            act("command", "", false),
            act("fixAll", "source.fixAll.eslint", false),
            act("qf1", "quickfix", false),
            act("qf2", "quickfix", false),       // 同順位は元の順序のまま
            act("preferred", "refactor", true),
            act("organize", "source.organizeImports", false),
        ];
        let out = rank_code_actions(v);
        let titles: Vec<&str> = out.iter().map(|a| a.title.as_str()).collect();
        assert_eq!(
            titles,
            ["preferred", "qf1", "qf2", "fixAll", "refactor", "organize", "command"]
        );
        // 空応答
        assert!(rank_code_actions(Vec::new()).is_empty());
        // 巨大な配列は上限で切る (同名アクションが大量に並んでも壊れない)
        let many: Vec<CodeAction> = (0..500).map(|_| act("同じ名前", "quickfix", false)).collect();
        let cut = rank_code_actions(many);
        assert_eq!(cut.len(), MAX_CODE_ACTIONS);
        assert!(cut.iter().all(|a| a.title == "同じ名前"));
    }

    #[test]
    fn action_is_actionable_は解決待ちを弾く() {
        let mut with_edit = act("e", "quickfix", false);
        with_edit.edit = WorkspaceEditPlan {
            files: vec![FileEdits {
                path: PathBuf::from("a.rs"),
                edits: vec![TextEdit::new(rng(0, 0, 0, 1), "X")],
            }],
            has_resource_ops: false,
        };
        assert!(action_is_actionable(&with_edit));

        let mut with_cmd = act("c", "", false);
        with_cmd.command = Some(CommandRef {
            title: "c".into(),
            command: "do.it".into(),
            arguments: vec![],
        });
        assert!(action_is_actionable(&with_cmd));

        // edit も command も無い (needs_resolve) は押せない
        assert!(!action_is_actionable(&act("lazy", "refactor", false)));
        // files はあるが編集が 0 件でも押せない
        let mut empty_edit = act("z", "quickfix", false);
        empty_edit.edit = WorkspaceEditPlan {
            files: vec![FileEdits { path: PathBuf::from("a.rs"), edits: vec![] }],
            has_resource_ops: false,
        };
        assert!(!action_is_actionable(&empty_edit));
    }

    #[test]
    fn action_range_は選択優先でなければ行全体() {
        let text = "let a = 1;\nlet 日本語 = 2;\n";
        // 選択なし → キャレット行の全体 (UTF-16 桁で行末まで)
        assert_eq!(
            action_range(text, None, pos(0, 4)),
            rng(0, 0, 0, 10)
        );
        // 日本語行: "let 日本語 = 2;" は UTF-16 で 12 単位 (char 数と同じ = BMP のみ)
        assert_eq!(action_range(text, None, pos(1, 0)).end.character, 12);
        // 空選択は「選択なし」と同じ扱い
        assert_eq!(
            action_range(text, Some((pos(0, 3), pos(0, 3))), pos(0, 3)),
            rng(0, 0, 0, 10)
        );
        // 選択あり → そのまま
        assert_eq!(
            action_range(text, Some((pos(0, 2), pos(1, 5))), pos(1, 5)),
            rng(0, 2, 1, 5)
        );
        // 逆向きの選択は正規化される
        assert_eq!(
            action_range(text, Some((pos(1, 5), pos(0, 2))), pos(0, 2)),
            rng(0, 2, 1, 5)
        );
        // 行が足りない (末尾より後ろ) → character はキャレットのまま、panic しない
        assert_eq!(action_range("", None, pos(9, 3)), rng(9, 0, 9, 3));
    }

    #[test]
    fn diagnostics_in_range_は重なりだけ拾う() {
        let d = |l: usize, c: usize, el: usize, ec: usize| Diagnostic {
            line: l,
            col: c,
            end_line: el,
            end_col: ec,
            severity: 1,
            message: format!("{l}:{c}"),
        };
        let all = vec![
            d(0, 0, 0, 5),  // 行 0
            d(2, 1, 2, 1),  // 空範囲 (端点だけ触れる)
            d(5, 0, 5, 3),  // 行 5
            d(3, 9, 3, 2),  // 逆転した壊れた診断 (正規化して判定)
        ];
        let got = diagnostics_in_range(&all, &rng(0, 0, 0, 10));
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].message, "0:0");
        // 空範囲の診断は端点一致でも拾う
        assert_eq!(diagnostics_in_range(&all, &rng(2, 1, 2, 1)).len(), 1);
        // 逆転診断も行 3 の範囲で拾える
        assert_eq!(diagnostics_in_range(&all, &rng(3, 0, 3, 20)).len(), 1);
        // どこにも重ならない
        assert!(diagnostics_in_range(&all, &rng(9, 0, 9, 1)).is_empty());
        assert!(diagnostics_in_range(&[], &rng(0, 0, 0, 1)).is_empty());
    }

    #[test]
    fn char_span_to_range_は日本語でも桁が合う() {
        let text = "abc\nあいう\n";
        assert_eq!(char_span_to_range(text, 0, 3), rng(0, 0, 0, 3));
        // "あいう" は 1 文字 1 UTF-16 単位
        assert_eq!(char_span_to_range(text, 4, 7), rng(1, 0, 1, 3));
        // 逆順でも正規化される
        assert_eq!(char_span_to_range(text, 7, 4), rng(1, 0, 1, 3));
        // 範囲外はクランプ (panic しない)
        let end = char_span_to_range(text, 0, 9999);
        assert!(end.end.line >= 2);
    }

    #[test]
    fn signature_display_は壊れた応答でも落ちない() {
        let mk = |sigs: Vec<SignatureInfo>, a: usize, p: Option<usize>| SignatureHelp {
            signatures: sigs,
            active_signature: a,
            active_parameter: p,
        };
        // 空応答
        assert!(signature_display(&mk(vec![], 0, None), 60).is_none());
        // ラベルが空白だけ
        assert!(signature_display(
            &mk(vec![SignatureInfo { label: "  \n ".into(), ..Default::default() }], 0, None),
            60
        )
        .is_none());

        let sig = SignatureInfo {
            label: "fn push(&mut self,\n  value: T)".into(),
            documentation: "要素を\n末尾へ追加する".into(),
            parameters: vec![
                ParameterInfo { label: "&mut self".into(), documentation: String::new() },
                ParameterInfo { label: "value: T".into(), documentation: String::new() },
            ],
        };
        let d = signature_display(&mk(vec![sig.clone()], 0, Some(1)), 60).unwrap();
        assert_eq!(d.label, "fn push(&mut self, value: T)");
        assert_eq!(d.active_param, "value: T");
        assert_eq!(d.doc, "要素を 末尾へ追加する");
        assert_eq!((d.index, d.total), (1, 1));

        // activeSignature が範囲外 → 最後へクランプ
        let two = mk(vec![sig.clone(), sig.clone()], 99, Some(0));
        let d = signature_display(&two, 60).unwrap();
        assert_eq!((d.index, d.total), (2, 2));
        // activeParameter が範囲外 → 強調しないだけ
        let d = signature_display(&mk(vec![sig], 0, Some(42)), 60).unwrap();
        assert_eq!(d.active_param, "");
    }

    #[test]
    fn highlight_char_spans_は整列重複除去して上限で切る() {
        let text = "aa bb aa\nあ aa\n";
        let h = |sl, sc, el, ec| DocumentHighlight { range: rng(sl, sc, el, ec), kind: 1 };
        let got = highlight_char_spans(
            text,
            &[
                h(1, 2, 1, 4), // "aa" (2 行目、日本語の後ろ)
                h(0, 6, 0, 8), // "aa"
                h(0, 0, 0, 2), // "aa"
                h(0, 0, 0, 2), // 重複
                h(0, 5, 0, 3), // 逆転 = 捨てる
                h(0, 4, 0, 4), // 空 = 捨てる
                h(99, 0, 99, 5), // 本文外 = 捨てる
            ],
        );
        assert_eq!(got, vec![(0, 2), (6, 8), (11, 13)]);
        assert!(highlight_char_spans(text, &[]).is_empty());
        assert!(highlight_char_spans("", &[h(0, 0, 0, 3)]).is_empty());
        // 上限で切る
        let many: Vec<DocumentHighlight> =
            (0..MAX_HIGHLIGHTS + 50).map(|i| h(0, i % 8, 0, (i % 8) + 1)).collect();
        assert!(highlight_char_spans(text, &many).len() <= MAX_HIGHLIGHTS);
    }

    #[test]
    fn highlight_state_はデバウンスして同じ位置を二度要求しない() {
        let t0 = std::time::Instant::now();
        let mut st = HighlightState::new();
        assert!(st.due_request(t0, HIGHLIGHT_DEBOUNCE).is_none(), "動く前は要求しない");
        assert!(st.due_in(t0, HIGHLIGHT_DEBOUNCE).is_none(), "予定が無ければ再描画も予約しない");

        st.on_move(pos(1, 1), t0);
        assert!(st.due_request(t0, HIGHLIGHT_DEBOUNCE).is_none(), "満了前は要求しない");
        assert!(st.due_in(t0, HIGHLIGHT_DEBOUNCE).is_some(), "満了までの残りを返す");
        // 同じ位置で on_move を連打しても時計は進まない
        st.on_move(pos(1, 1), t0 + Duration::from_millis(200));
        let t1 = t0 + HIGHLIGHT_DEBOUNCE;
        assert_eq!(st.due_request(t1, HIGHLIGHT_DEBOUNCE), Some(pos(1, 1)));
        assert!(st.due_request(t1, HIGHLIGHT_DEBOUNCE).is_none(), "同じ位置で二度は要求しない");
        assert!(st.due_in(t1, HIGHLIGHT_DEBOUNCE).is_none(), "要求済みなら再描画を予約しない");

        // 応答: 飛行中の id 以外は捨てる
        st.mark_sent(RequestStatus::Sent(7));
        let hl = vec![DocumentHighlight { range: rng(0, 0, 0, 2), kind: 2 }];
        assert!(!st.apply_response(6, hl.clone()), "古い応答は捨てる");
        assert!(st.shown().is_empty());
        assert!(st.apply_response(7, hl.clone()));
        assert_eq!(st.shown().len(), 1);

        // キャレットが動いても表示は消さない (点滅させない)
        st.on_move(pos(2, 0), t1);
        assert_eq!(st.shown().len(), 1);
        // 送れなかったら再挑戦できるよう requested_for を戻す
        st.mark_sent(RequestStatus::Unsupported);
        let t2 = t1 + HIGHLIGHT_DEBOUNCE;
        assert_eq!(st.due_request(t2, HIGHLIGHT_DEBOUNCE), Some(pos(2, 0)));
        // clear で全部消える
        st.clear();
        assert!(st.shown().is_empty());
        assert!(st.due_request(t2, HIGHLIGHT_DEBOUNCE).is_none());
    }

    // =======================================================================
    // 能力ゲート / 堅牢性
    // =======================================================================

    #[test]
    fn server_caps_parse_all_provider_shapes() {
        let caps = parse_server_caps(&json!({
            "completionProvider": {"triggerCharacters": [".", "::"]},
            "hoverProvider": true,
            "definitionProvider": true,
            "referencesProvider": true,
            "renameProvider": {"prepareProvider": true},
            "documentFormattingProvider": true,
            "documentRangeFormattingProvider": false,
            "signatureHelpProvider": {"triggerCharacters": ["(", ","]},
            "codeActionProvider": {"codeActionKinds": ["quickfix"]},
            "documentSymbolProvider": true
        }));
        assert!(caps.completion && caps.hover && caps.rename && caps.prepare_rename);
        assert_eq!(caps.completion_trigger_chars, vec!['.', ':']);
        assert_eq!(caps.signature_trigger_chars, vec!['(', ',']);
        assert!(caps.formatting && !caps.range_formatting);
        assert!(caps.code_action && caps.document_symbol);
        assert!(!caps.document_highlight); // 未宣言

        // rename が bool true のときは prepare は無し
        let c2 = parse_server_caps(&json!({"renameProvider": true}));
        assert!(c2.rename && !c2.prepare_rename);
        // 空 / null
        assert_eq!(parse_server_caps(&Value::Null), ServerCaps::default());
        assert_eq!(parse_server_caps(&json!({"renameProvider": false})), ServerCaps::default());
    }

    /// 能力を宣言しないサーバーでは各リクエストが Unsupported (エラーでも panic でもない)。
    #[test]
    fn capability_gating_makes_requests_noop() {
        let client = fake_client();
        // initialize 応答前 = NotReady
        assert_eq!(client.request_rename(Path::new("/w/a.rs"), pos(0, 0), "x"), RequestStatus::NotReady);

        // renameProvider を持たないサーバーが initialize を返した
        deliver(&client, 1, Pending::Initialize, json!({"capabilities": {"hoverProvider": true}}));
        assert!(client.is_ready());
        assert!(!client.caps().rename);

        let p = Path::new("/w/a.rs");
        assert_eq!(client.request_rename(p, pos(0, 0), "x"), RequestStatus::Unsupported);
        assert_eq!(client.request_prepare_rename(p, pos(0, 0)), RequestStatus::Unsupported);
        assert_eq!(client.request_references(p, pos(0, 0), true), RequestStatus::Unsupported);
        assert_eq!(client.request_formatting(p, &FormatOptions::default()), RequestStatus::Unsupported);
        assert_eq!(client.request_document_symbols(p), RequestStatus::Unsupported);
        assert_eq!(client.request_code_actions(p, &Range::default(), &[]), RequestStatus::Unsupported);
        assert_eq!(client.request_signature_help(p, pos(0, 0)), RequestStatus::Unsupported);
        assert_eq!(client.request_completion(p, 0, 0), RequestStatus::Unsupported);
        // 宣言済みのものは送られる
        assert!(client.request_hover(p, 0, 0).is_sent());
        // no-op なのでスロットは空のまま (UI は何も起きない)
        assert!(client.poll_rename().is_none());
        assert!(client.poll_document_symbols().is_none());
    }

    #[test]
    fn malformed_and_truncated_payloads_never_panic() {
        let shared = Arc::new(Shared::new());
        let (tx, _rx) = mpsc::channel();
        for raw in [
            "",
            "{",
            "null",
            "[]",
            r#"{"jsonrpc":"2.0"}"#,
            r#"{"jsonrpc":"2.0","id":"str-id","result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"Unhandled"}}"#,
            r#"{"jsonrpc":"2.0","id":1,"result":{"items":"not-an-array"}}"#,
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":"textDocument/publishDiagnostics","params":{"uri":"file:///a","diagnostics":[{"bad":1}]}}"#,
        ] {
            pend(&shared, 1, Pending::Completion);
            handle_message(raw, &shared, &tx);
        }
        // エラー応答は「空の結果」として届く (待ちっぱなしにならない)
        shared.completion.begin(2);
        pend(&shared, 2, Pending::Completion);
        handle_message(
            r#"{"jsonrpc":"2.0","id":2,"error":{"code":-32601,"message":"no"}}"#,
            &shared,
            &tx,
        );
        assert_eq!(shared.completion.take(), Some(CompletionList::default()));

        // 途中で切れたフレームはデコーダが握り潰す
        let mut dec = FrameDecoder::new();
        dec.push(b"Content-Length: 100\r\n\r\n{\"partial\":");
        assert!(dec.next_message().is_none());
    }

    #[test]
    fn dead_server_abandons_pending_requests() {
        let shared = Arc::new(Shared::new());
        shared.completion.begin(1);
        shared.rename.begin(2);
        shared.symbols.begin(3);
        shared.remember(1, Pending::Completion);
        shared.remember(2, Pending::Rename);
        shared.remember(3, Pending::Symbols);

        shared.alive.store(false, Ordering::SeqCst);
        shared.abandon_all();

        // 待ちは全部「空の結果」で解ける (UI のスピナーが止まる)
        assert_eq!(shared.completion.take(), Some(CompletionList::default()));
        assert_eq!(shared.rename.take(), Some(WorkspaceEditPlan::default()));
        assert_eq!(shared.symbols.take(), Some(Vec::new()));
        assert_eq!(lock_ok(&shared.pending).len(), 0);
        assert_eq!(shared.abandoned.load(Ordering::SeqCst), 3);

        // 死んだ後の応答は取り込まれない (id が 0 に戻っているため)
        let (tx, _rx) = mpsc::channel();
        handle_message(
            &json!({"jsonrpc":"2.0","id":1,"result":[{"label":"late"}]}).to_string(),
            &shared,
            &tx,
        );
        assert!(shared.completion.take().is_none());
    }

    #[test]
    fn sweep_timeouts_releases_stuck_requests() {
        let client = fake_client();
        deliver(
            &client,
            1,
            Pending::Initialize,
            json!({"capabilities": {"documentSymbolProvider": true}}),
        );
        let st = client.request_document_symbols(Path::new("/w/a.rs"));
        assert!(st.is_sent());
        assert_eq!(client.pending_count(), 1);

        // まだ新しいので掃除されない
        assert_eq!(client.sweep_timeouts(Duration::from_secs(60)), 0);
        assert!(client.poll_document_symbols().is_none());

        // 期限切れ (timeout=0) で打ち切られ、空結果が届く
        assert_eq!(client.sweep_timeouts(Duration::ZERO), 1);
        assert_eq!(client.pending_count(), 0);
        assert_eq!(client.poll_document_symbols(), Some(Vec::new()));
        assert_eq!(client.abandoned_count(), 1);

        // 打ち切った後に遅れて来た応答は無視される
        let (tx, _rx) = mpsc::channel();
        let id = st.id().unwrap();
        handle_message(
            &json!({"jsonrpc":"2.0","id":id,"result":[]}).to_string(),
            &client.shared,
            &tx,
        );
        assert!(client.poll_document_symbols().is_none());
    }

    #[test]
    fn restart_policy_backs_off_and_gives_up() {
        let t0 = std::time::Instant::now();
        let mut p = RestartPolicy::new(Duration::from_millis(100), Duration::from_millis(400), 3);
        assert!(p.should_restart(t0)); // 初回は即時

        p.record_exit(t0);
        assert_eq!(p.backoff(), Duration::from_millis(100));
        assert!(!p.should_restart(t0));
        assert!(p.should_restart(t0 + Duration::from_millis(100)));

        p.record_exit(t0);
        assert_eq!(p.backoff(), Duration::from_millis(200)); // 倍
        p.record_exit(t0);
        assert_eq!(p.backoff(), Duration::from_millis(400)); // max で頭打ち
        assert!(p.gave_up()); // limit=3 に到達
        assert!(!p.should_restart(t0 + Duration::from_secs(3600)));

        // 起動に成功したらリセット
        p.record_ready();
        assert_eq!(p.failures(), 0);
        assert!(!p.gave_up());
        assert!(p.should_restart(t0));
    }

    // ---- テスト用の LspClient (実プロセス無し) ----

    /// 子プロセスを持たない LspClient。送信は握り潰され、応答はテストが直接流し込む。
    fn fake_client() -> LspClient {
        // 「必ず即終了する」子プロセスを 1 つだけ使う。stdio は奪わないので
        // reader/writer スレッドは動かず、handle_message をテストから直接叩ける。
        let child = crate::shellenv::shell_command("exit 0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn trivial child");
        let (tx, rx) = mpsc::channel::<Value>();
        // 受信端を保持しないと send がエラーになるのでリークさせておく
        std::mem::forget(rx);
        LspClient {
            child,
            tx,
            shared: Arc::new(Shared::new()),
            next_id: AtomicU64::new(1),
            versions: Mutex::new(HashMap::new()),
        }
    }

    /// サーバー応答を直接流し込む。
    fn deliver(client: &LspClient, id: u64, kind: Pending, result: Value) {
        client.remember_pending(id, kind);
        begin_slot(&client.shared, kind, id);
        let (tx, _rx) = mpsc::channel();
        handle_message(
            &json!({"jsonrpc":"2.0","id":id,"result":result}).to_string(),
            &client.shared,
            &tx,
        );
    }

    #[test]
    fn end_to_end_synthetic_rename_flow() {
        let client = fake_client();
        deliver(
            &client,
            1,
            Pending::Initialize,
            json!({"capabilities": {"renameProvider": {"prepareProvider": true}}}),
        );
        let p = Path::new("/w/a.rs");
        let st = client.request_prepare_rename(p, pos(0, 4));
        deliver(
            &client,
            st.id().unwrap(),
            Pending::PrepareRename,
            json!({"start":{"line":0,"character":4},"end":{"line":0,"character":7}}),
        );
        assert_eq!(client.poll_prepare_rename(), Some(Some(rng(0, 4, 0, 7))));

        let st = client.request_rename(p, pos(0, 4), "新しい名前");
        deliver(
            &client,
            st.id().unwrap(),
            Pending::Rename,
            json!({"changes": {"file:///w/a.rs": [
                {"range":{"start":{"line":0,"character":4},"end":{"line":0,"character":7}},
                 "newText":"新しい名前"}]}}),
        );
        let plan = client.poll_rename().expect("応答あり");
        assert_eq!(plan.edit_count(), 1);
        assert_eq!(apply_file_edits("let old = 1;", &plan.files[0]), "let 新しい名前 = 1;");
        // 2 度目の poll は空 (消費済み)
        assert!(client.poll_rename().is_none());
    }

    // ---- 統合スモーク: rust-analyzer ----

    #[test]
    fn smoke_rust_analyzer() {
        use std::process::Command;
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
