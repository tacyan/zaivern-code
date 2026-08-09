//! **構造化プロトコル段** — 状態ラダーの最上段 (CLAUDE.md 設計原則 #4)。
//!
//! > エージェントの状態を画面 (ピクセル) から推測しない。
//! > 構造化プロトコル > ベンダー提供フック > 状態ファイル > 画面スクレイプ。
//!
//! ここはその **1 段目**。ベンダー CLI が自前で吐く 1 行 1 JSON (JSONL) の
//! イベント列をそのまま読み、「いま何をしているか」を**画面に一切触れずに**決める。
//!
//! ## 実機で確認できたもの (2026-08 時点)
//!
//! | CLI | 構造化出力 | 確認方法 |
//! |---|---|---|
//! | `claude` 2.1.226 | `--print --verbose --output-format stream-json` | `claude --help` + 実行して JSONL を採取 |
//! | `codex` 0.147.0 | `codex exec --json` (「Print events to stdout as JSONL」) | `codex exec --help` + 実行して JSONL を採取 |
//! | `gemini` 0.51.0 | `--output-format stream-json` は **フラグの存在だけ**確認 | `gemini --help` の choices。実行はアカウント制限で不可 |
//!
//! **gemini は方言表に入れていない。** フラグがあることは判ったが、イベントの
//! 語彙 (種別名・フィールド名) を 1 件も観測できていないため、書けば憶測になる。
//! 観測できたら [`crate::agents::STREAM_DIALECTS`] へ 1 行足すだけで有効になる。
//!
//! ## エージェント固有値はここに置かない
//! フラグ名・イベント種別名・ツール名は**すべて** [`crate::agents`] のカタログ
//! ([`crate::agents::STREAM_DIALECTS`]) にデータとして持つ。このモジュールは
//! 「JSONL を行に切って、表を引いて、状態を持つ」機構だけを提供する。
//!
//! ## 沈黙したら降りる
//! [`ProtoTracker::read`] は最後のイベントから `stale_ms` 以上経つと `None` を
//! 返す。上位段が黙ったらラダーは自動的に下位段 (フック → 見張り → 画面) へ降り、
//! UI にはその段位が出る ([`crate::kanban::Source`])。

use serde_json::Value;

// ---------------------------------------------------------------------------
// 状態
// ---------------------------------------------------------------------------

/// 構造化イベントから**画面に依らずに**読み取れる状態。
///
/// 画面推定 ([`crate::kanban::classify_screen`]) より粗いが、こちらは事実である。
/// 粗さは `detail` (ツール名・コマンド) が補う。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProtoState {
    /// セッションが立ち上がった
    Starting,
    /// モデルが考えている / 応答を生成している
    Thinking,
    /// ツール (コマンド) を実行している
    Running,
    /// ファイルを編集している
    Editing,
    /// 承認・入力待ち
    Approval,
    /// 手番が終わって待っている
    Idle,
    /// 正常に終わった
    Done,
    /// 失敗して終わった
    Failed,
}

impl ProtoState {
    /// UI に出す短い名前 (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            ProtoState::Starting => "起動中",
            ProtoState::Thinking => "思考中",
            ProtoState::Running => "実行中",
            ProtoState::Editing => "編集中",
            ProtoState::Approval => "承認待ち",
            ProtoState::Idle => "待機",
            ProtoState::Done => "完了",
            ProtoState::Failed => "異常終了",
        }
    }
}

// ---------------------------------------------------------------------------
// 方言 (カタログが持つデータの型)
// ---------------------------------------------------------------------------

/// イベント 1 件を状態へ写す規則。表は上から順に見て**最初に当たったもの**を採る。
///
/// パスは `.` 区切り。セグメントの末尾に `[]` を付けると「その配列の
/// **いずれかの要素**」を意味する (`message.content[].type` など)。
pub struct EventRule {
    /// `StreamDialect::kind_path` の値。`""` は「種別を問わない」。
    pub kind: &'static str,
    /// 追加の絞り込みフィールドのパス。`""` なら絞り込み無し。
    pub sub_path: &'static str,
    /// `sub_path` に期待する値。`""` なら「そのパスに値が在ればよい」。
    pub sub_value: &'static str,
    /// 一致したときの状態。
    pub state: ProtoState,
    /// 補足 (ツール名・コマンド) を拾うパス。要らなければ `""`。
    pub detail_path: &'static str,
}

/// 1 エージェント分の構造化出力の**方言**。実データは `agents.rs` のカタログ。
pub struct StreamDialect {
    /// カタログ上の実行ファイル名 (`AgentSpec::bin`)。
    pub bin: &'static str,
    /// 構造化出力を有効にする引数 (`bin` の後ろに足す並び)。
    pub args: &'static str,
    /// イベント種別が入るフィールドのパス。
    pub kind_path: &'static str,
    /// 規則表 (上から順)。
    pub rules: &'static [EventRule],
    /// ツール名 → 状態の細分。`detail` がここに在れば規則の状態を上書きする。
    pub tools: &'static [(&'static str, ProtoState)],
    /// **この方言を実機で確認した方法**。空は禁止 (カタログ整合テストが落とす)。
    pub verified: &'static str,
}

impl StreamDialect {
    /// そのまま端末へ打てる、構造化出力つきの起動コマンド。
    pub fn command(&self) -> String {
        format!("{} {}", self.bin, self.args)
    }
}

// ---------------------------------------------------------------------------
// JSON パス
// ---------------------------------------------------------------------------

/// スカラー値の文字列表現。オブジェクト・配列・null は `None`。
fn scalar(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// `path` の指す値をすべて集める (`[]` は配列の全要素へ枝分かれする)。
fn collect_at<'a>(v: &'a Value, path: &str, out: &mut Vec<&'a Value>) {
    if path.is_empty() {
        out.push(v);
        return;
    }
    let (seg, rest) = match path.split_once('.') {
        Some((a, b)) => (a, b),
        None => (path, ""),
    };
    if let Some(name) = seg.strip_suffix("[]") {
        let arr = if name.is_empty() {
            v.as_array()
        } else {
            v.get(name).and_then(Value::as_array)
        };
        if let Some(arr) = arr {
            for e in arr {
                collect_at(e, rest, out);
            }
        }
        return;
    }
    if let Some(next) = v.get(seg) {
        collect_at(next, rest, out);
    }
}

/// `path` に `want` が在るか。`want` が空なら「値が在るか」。
fn path_matches(v: &Value, path: &str, want: &str) -> bool {
    let mut found = Vec::new();
    collect_at(v, path, &mut found);
    if want.is_empty() {
        return found.iter().any(|x| !x.is_null());
    }
    found.iter().filter_map(|x| scalar(x)).any(|s| s == want)
}

/// `path` の最初のスカラー値。
fn path_str(v: &Value, path: &str) -> Option<String> {
    let mut found = Vec::new();
    collect_at(v, path, &mut found);
    found.iter().find_map(|x| scalar(x))
}

// ---------------------------------------------------------------------------
// 行の切り出し (チャンク境界に強い)
// ---------------------------------------------------------------------------

/// 1 行の上限 (バイト)。これを超える行は**捨てて数える** (溜め込まない)。
///
/// ツール結果を丸ごと積んだイベントは容易に MB 級になる。上限が無いと
/// 1 行のために際限なくメモリを食う (設計原則 3: アイドル時のコストはゼロ)。
pub const MAX_LINE_BYTES: usize = 256 * 1024;

/// バイトチャンク列 → 完全な行。**UTF-8 の途中で割れても壊れない**。
///
/// 復号は「改行で切り出した完全な行」に対してだけ行うので、マルチバイト文字が
/// チャンク境界をまたいでもバッファに残って次のチャンクと繋がる。
pub struct StreamDecoder {
    buf: Vec<u8>,
    max_line: usize,
    /// 上限超過で「次の改行まで捨てている」最中か。
    dropping: bool,
    /// 上限超過で捨てた行数 (健全性の目安として UI/ログへ出せる)。
    pub dropped_lines: u64,
}

impl StreamDecoder {
    pub fn new(max_line: usize) -> Self {
        Self {
            buf: Vec::new(),
            max_line: max_line.max(1),
            dropping: false,
            dropped_lines: 0,
        }
    }

    /// 途中まで溜まっている (= まだ改行が来ていない) バイト数。
    pub fn pending(&self) -> usize {
        self.buf.len()
    }

    /// チャンクを流し込み、**完全な行だけ**を返す。行末の `\r` は落とす。
    pub fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        let mut out = Vec::new();
        for &b in chunk {
            if b == b'\n' {
                if self.dropping {
                    self.dropping = false;
                    self.buf.clear();
                    continue;
                }
                let line = std::mem::take(&mut self.buf);
                let line = String::from_utf8_lossy(&line);
                let line = line.trim_end_matches('\r');
                if !line.is_empty() {
                    out.push(line.to_string());
                }
                continue;
            }
            if self.dropping {
                continue;
            }
            self.buf.push(b);
            if self.buf.len() > self.max_line {
                // 巨大な 1 行: 溜め込まず、次の改行まで捨てる。
                self.buf.clear();
                self.dropping = true;
                self.dropped_lines += 1;
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// 追跡
// ---------------------------------------------------------------------------

/// 構造化イベントから読んだ 1 件の判定。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ProtoRead {
    pub state: ProtoState,
    /// ツール名・コマンドなどの補足。無ければ空。
    pub detail: String,
}

/// 1 行を方言で読む **純関数**。JSON でない・規則に当たらない行は `None`。
pub fn parse_line(dialect: &StreamDialect, line: &str) -> Option<ProtoRead> {
    let v: Value = serde_json::from_str(line).ok()?;
    parse_value(dialect, &v)
}

/// パース済み JSON を方言で読む **純関数**。
pub fn parse_value(dialect: &StreamDialect, v: &Value) -> Option<ProtoRead> {
    for rule in dialect.rules {
        if !rule.kind.is_empty() && !path_matches(v, dialect.kind_path, rule.kind) {
            continue;
        }
        if !rule.sub_path.is_empty() && !path_matches(v, rule.sub_path, rule.sub_value) {
            continue;
        }
        let detail = if rule.detail_path.is_empty() {
            String::new()
        } else {
            path_str(v, rule.detail_path).unwrap_or_default()
        };
        // ツール名が判るなら、方言のツール表で状態を細分する (データ駆動)。
        let state = dialect
            .tools
            .iter()
            .find(|(name, _)| *name == detail)
            .map(|(_, s)| *s)
            .unwrap_or(rule.state);
        return Some(ProtoRead { state, detail });
    }
    None
}

/// 1 セッション分の構造化ストリーム追跡。
pub struct ProtoTracker {
    dialect: &'static StreamDialect,
    decoder: StreamDecoder,
    last: Option<ProtoRead>,
    last_ms: u64,
    /// JSON として読めなかった行数。
    pub bad_lines: u64,
    /// 規則に当たらなかった (未知の種別の) 行数。
    pub unknown_events: u64,
    /// 状態を更新したイベント数。
    pub events: u64,
}

impl ProtoTracker {
    pub fn new(dialect: &'static StreamDialect) -> Self {
        Self {
            dialect,
            decoder: StreamDecoder::new(MAX_LINE_BYTES),
            last: None,
            last_ms: 0,
            bad_lines: 0,
            unknown_events: 0,
            events: 0,
        }
    }

    /// この追跡が読んでいる方言。
    pub fn dialect(&self) -> &'static StreamDialect {
        self.dialect
    }

    /// 上限超過で捨てた行数 ([`StreamDecoder::dropped_lines`])。
    pub fn dropped_lines(&self) -> u64 {
        self.decoder.dropped_lines
    }

    /// バイトチャンクを流し込む。**画面には一切触れない**。
    pub fn feed(&mut self, chunk: &[u8], now_ms: u64) {
        for line in self.decoder.push(chunk) {
            match serde_json::from_str::<Value>(&line) {
                Ok(v) => match parse_value(self.dialect, &v) {
                    Some(read) => {
                        self.last = Some(read);
                        self.last_ms = now_ms;
                        self.events += 1;
                    }
                    // 未知のイベント種別: **状態は動かさない**。読めた事実は数える。
                    None => self.unknown_events += 1,
                },
                Err(_) => self.bad_lines += 1,
            }
        }
    }

    /// いまの判定。最後のイベントから `stale_ms` 以上経っていたら `None`
    /// (= この段は黙った → ラダーは下位段へ降りる)。
    pub fn read(&self, now_ms: u64, stale_ms: u64) -> Option<ProtoRead> {
        let last = self.last.clone()?;
        if now_ms.saturating_sub(self.last_ms) > stale_ms {
            return None;
        }
        Some(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::{stream_dialect, stream_dialect_for_command};

    // ── 実機で採取した実物のイベント (形はそのまま、値だけ短くしてある) ──
    // claude 2.1.226 `claude -p --output-format stream-json --verbose`
    const C_INIT: &str = r#"{"type":"system","subtype":"init","cwd":"/w","session_id":"a45","tools":["Bash","Edit"]}"#;
    const C_TEXT: &str = r#"{"type":"assistant","message":{"model":"m","type":"message","role":"assistant","content":[{"type":"text","text":"hi"}]}}"#;
    const C_THINK_THEN_TOOL: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"…"},{"type":"tool_use","id":"t1","name":"Bash","input":{}}]}}"#;
    const C_EDIT: &str = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","id":"t2","name":"Edit","input":{}}]}}"#;
    const C_RATE: &str = r#"{"type":"rate_limit_event","rate_limit_info":{"status":"allowed"}}"#;
    const C_OK: &str = r#"{"is_error":false,"num_turns":1,"type":"result","subtype":"success"}"#;
    const C_NG: &str =
        r#"{"is_error":true,"num_turns":1,"type":"result","subtype":"error_during_execution"}"#;
    // codex-cli 0.147.0 `codex exec --json`
    const X_THREAD: &str = r#"{"type":"thread.started","thread_id":"019f"}"#;
    const X_TURN: &str = r#"{"type":"turn.started"}"#;
    const X_CMD_START: &str = r#"{"type":"item.started","item":{"id":"item_1","type":"command_execution","command":"/bin/zsh -lc 'echo z'","status":"in_progress"}}"#;
    const X_MSG: &str =
        r#"{"type":"item.completed","item":{"id":"item_0","type":"agent_message","text":"hi"}}"#;
    const X_DONE: &str = r#"{"type":"turn.completed","usage":{"input_tokens":1}}"#;

    fn claude() -> &'static StreamDialect {
        stream_dialect("claude").expect("claude の方言がカタログに無い")
    }

    fn codex() -> &'static StreamDialect {
        stream_dialect("codex").expect("codex の方言がカタログに無い")
    }

    #[test]
    fn 構造化イベントを画面に触れずに読める() {
        let d = claude();
        let s = |line: &str| parse_line(d, line).map(|r| (r.state, r.detail));
        assert_eq!(s(C_INIT), Some((ProtoState::Starting, String::new())));
        assert_eq!(s(C_TEXT), Some((ProtoState::Thinking, String::new())));
        // thinking ブロックが先に並んでいても tool_use を拾う (配列の全要素を見る)
        assert_eq!(
            s(C_THINK_THEN_TOOL),
            Some((ProtoState::Running, "Bash".to_string()))
        );
        // ツール名の表で「実行中」→「編集中」へ細分される (データ駆動)
        assert_eq!(s(C_EDIT), Some((ProtoState::Editing, "Edit".to_string())));
        assert_eq!(s(C_OK), Some((ProtoState::Done, String::new())));
        // 失敗は成功より先に見る (同じ種別で来るため)
        assert_eq!(s(C_NG), Some((ProtoState::Failed, String::new())));
    }

    #[test]
    fn codex_の_jsonl_も同じ機構で読める() {
        let d = codex();
        let s = |line: &str| parse_line(d, line).map(|r| r.state);
        assert_eq!(s(X_THREAD), Some(ProtoState::Starting));
        assert_eq!(s(X_TURN), Some(ProtoState::Thinking));
        assert_eq!(s(X_CMD_START), Some(ProtoState::Running));
        assert_eq!(s(X_MSG), Some(ProtoState::Thinking));
        assert_eq!(s(X_DONE), Some(ProtoState::Idle));
        assert_eq!(
            parse_line(d, X_CMD_START).map(|r| r.detail),
            Some("/bin/zsh -lc 'echo z'".to_string())
        );
    }

    #[test]
    fn 途中で切れた_json_は状態を動かさない() {
        let mut t = ProtoTracker::new(claude());
        t.feed(format!("{C_INIT}\n").as_bytes(), 0);
        // 改行まで来ていない = まだ行ではない。状態は起動中のまま。
        let half = &C_EDIT[..C_EDIT.len() / 2];
        t.feed(half.as_bytes(), 10);
        assert_eq!(
            t.read(10, 1_000).map(|r| r.state),
            Some(ProtoState::Starting)
        );
        assert_eq!(t.bad_lines, 0, "まだ行になっていないので不正行に数えない");
        // 続きが来て初めて 1 行になる
        t.feed(format!("{}\n", &C_EDIT[C_EDIT.len() / 2..]).as_bytes(), 20);
        assert_eq!(
            t.read(20, 1_000).map(|r| r.state),
            Some(ProtoState::Editing)
        );
    }

    #[test]
    fn 不正な_json_は数えるだけで状態を動かさない() {
        let mut t = ProtoTracker::new(claude());
        t.feed(format!("{C_INIT}\n").as_bytes(), 0);
        t.feed(b"{\"type\": oops\n not json at all\n", 10);
        assert_eq!(t.bad_lines, 2);
        assert_eq!(
            t.read(10, 1_000).map(|r| r.state),
            Some(ProtoState::Starting)
        );
    }

    #[test]
    fn 未知のイベント種別は無視して直前の状態を保つ() {
        let mut t = ProtoTracker::new(claude());
        t.feed(format!("{C_EDIT}\n").as_bytes(), 0);
        // 実機に在るが表に無い種別 (rate_limit_event) と、完全に架空の種別
        t.feed(format!("{C_RATE}\n").as_bytes(), 10);
        t.feed(b"{\"type\":\"quantum_flux\",\"v\":1}\n", 20);
        assert_eq!(t.unknown_events, 2);
        assert_eq!(
            t.read(20, 1_000).map(|r| r.state),
            Some(ProtoState::Editing)
        );
    }

    #[test]
    fn 巨大な_1_行は捨てて次の行から復帰する() {
        let mut t = ProtoTracker::new(claude());
        let huge = format!(
            "{{\"type\":\"assistant\",\"x\":\"{}\"}}\n",
            "x".repeat(MAX_LINE_BYTES + 4_096)
        );
        t.feed(huge.as_bytes(), 0);
        assert_eq!(t.dropped_lines(), 1);
        assert!(t.read(0, 1_000).is_none(), "捨てた行で状態を作らない");
        // 次の行はふつうに読める
        t.feed(format!("{C_INIT}\n").as_bytes(), 10);
        assert_eq!(
            t.read(10, 1_000).map(|r| r.state),
            Some(ProtoState::Starting)
        );
    }

    #[test]
    fn utf8_の途中でチャンクが割れても行は壊れない() {
        let line = r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file":"日本語のパス.rs"}}]}}"#;
        let raw = format!("{line}\n");
        let bytes = raw.as_bytes();
        // 「日」の 3 バイトの途中で割る
        let cut = raw.find('日').expect("マルチバイト文字が要る") + 1;
        assert!(!raw.is_char_boundary(cut), "文字境界でない位置で割る前提");
        let mut dec = StreamDecoder::new(MAX_LINE_BYTES);
        assert!(dec.push(&bytes[..cut]).is_empty());
        let out = dec.push(&bytes[cut..]);
        assert_eq!(out.len(), 1);
        assert!(
            out[0].contains("日本語のパス.rs"),
            "文字が壊れた: {}",
            out[0]
        );
        // 割れても方言はふつうに読める
        assert_eq!(
            parse_line(claude(), &out[0]).map(|r| r.state),
            Some(ProtoState::Editing)
        );
    }

    #[test]
    fn 上位段が沈黙したら読めなくなる() {
        let mut t = ProtoTracker::new(claude());
        t.feed(format!("{C_INIT}\n").as_bytes(), 1_000);
        assert!(t.read(1_500, 1_000).is_some());
        assert!(
            t.read(2_500, 1_000).is_none(),
            "沈黙したら上位段は判定を返さない (下位段へ降りる)"
        );
    }

    #[test]
    fn 構造化フラグが付いていないコマンドは構造化段に入らない() {
        // 素の対話起動では stream-json は出ない (--print 専用)。
        assert!(stream_dialect_for_command("claude").is_none());
        assert!(stream_dialect_for_command("claude --dangerously-skip-permissions").is_none());
        // 全フラグが揃って初めて構造化段になる
        let cmd = format!("claude {}", claude().args);
        assert!(stream_dialect_for_command(&cmd).is_some());
        let cmd = format!("codex {}", codex().args);
        assert!(stream_dialect_for_command(&cmd).is_some());
        // カタログに無いエージェントは常に None
        assert!(stream_dialect_for_command("aider --json").is_none());
    }

    #[test]
    fn 方言のコマンドはそのまま端末へ打てる形になる() {
        assert_eq!(
            claude().command(),
            "claude --print --verbose --output-format stream-json"
        );
        assert_eq!(codex().command(), "codex exec --json");
    }
}
