//! CLI コーディングエージェントを「診断役」として使う `Diagnostician` 実装。
//!
//! スーパーバイザー (`supervisor.rs`) は決定的なルールだけで完結して動く。
//! ここはその上に載る**任意**の相談相手で、詰まったセッションの様子を
//! ユーザーが選んだ CLI エージェント (Claude Code / Codex / Goose ...) に
//! ヘッドレス 1 発実行で見せて、次の一手を 1 行で答えてもらう。
//!
//! 設計上の原則 (見張り役自身が事故の原因になってはならない):
//!
//! 1. **黙るほうに倒す** — 応答が少しでも曖昧なら `None`。推測で介入を捏造しない。
//! 2. **必ず終わる** — 子プロセスにはハード期限があり、超えたら kill する。
//!    stdout/stderr は別スレッドで読み切る (パイプ満杯によるデッドロック回避)。
//! 3. **上限は二重にかける** — ここで異常種別ごとの上限まで丸めたうえで、
//!    `supervisor::intent_from_diagnosis` が同じ `gate` を再度通す。
//!    どちらか一方が壊れても破壊的操作は素通りしない。
//! 4. **自分自身は診ない** — 診断役の CLI は普通の作業セッションとしても使える。
//!    そのセッションが詰まった状態で自分に診断を頼むと永久に返らないため、
//!    自セッション ID を登録しておき、一致したら即 `None`。
//! 5. **すべて有界** — 抜粋も応答も固定長で打ち切る。無制限のバッファは持たない。

use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::agents::{spec_for_command, AgentSpec};
use crate::supervisor::{
    redact, Anomaly, Diagnosis, DiagnosisRequest, Diagnostician, Intervention,
};

/// 子プロセスの既定ハード期限。
///
/// NOTE: 配線は app.rs/config.rs 側で行う。まだ呼び出し元が無い項目だけ
/// 個別に dead_code を許可している (モジュールまるごとの許可はしない)。
#[allow(dead_code)]
pub const DEFAULT_TIMEOUT_SECS: u64 = 60;

/// プロンプトへ載せる出力抜粋の上限 (文字数)。直近の末尾だけを残す。
pub const MAX_EXCERPT_CHARS: usize = 2000;

/// 子プロセスから読み取る応答の上限 (バイト)。これを超えた分は捨てる。
pub const MAX_RESPONSE_BYTES: usize = 16 * 1024;

/// `WHY:` 行として採用する最大文字数。
pub const MAX_WHY_CHARS: usize = 200;

/// 状態開始時刻を覚えておくセッション数の上限 (超えたら丸ごと捨てて数え直す)。
const MAX_TRACKED_SESSIONS: usize = 256;

/// 応答で受け付ける行動名。**この表に無い綴りは一切認めない** (大文字小文字も含めて厳密)。
const ACTION_TABLE: &[(&str, Intervention)] = &[
    ("observe", Intervention::Observe),
    ("notify", Intervention::Notify),
    ("auto_answer", Intervention::AutoAnswer),
    ("nudge", Intervention::Nudge),
    ("restart", Intervention::Restart),
    ("halt", Intervention::Halt),
];

// ---------------------------------------------------------------------------
// 応答の解析 (純関数)
// ---------------------------------------------------------------------------

/// 厳密形式の応答から取り出した助言。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedAdvice {
    pub action: Intervention,
    /// 理由 (日本語 1 行、`MAX_WHY_CHARS` で打ち切り済み)。
    pub why: String,
}

/// 行動名を厳密に引く。表記ゆれ (大文字・前後の記号) は認めない。
fn action_from_name(name: &str) -> Option<Intervention> {
    ACTION_TABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, a)| *a)
}

/// 応答テキストを解析する。**曖昧なら必ず `None`**。
///
/// 受け付けるのは次の 2 行がちょうど 1 組だけ現れる場合に限る:
///
/// ```text
/// ACTION: nudge
/// WHY: テストが同じ箇所で失敗し続けており、方針の再指示が要る
/// ```
///
/// `None` にする条件 (いずれも「介入を捏造しない」ため意図的に厳しくしている):
/// - 空 / 空白のみ
/// - `ACTION:` 行が無い (散文だけ)
/// - `ACTION:` 行が 2 つ以上 (矛盾していても一致していても不採用)
/// - 未知の行動名、または大文字小文字が違う (`Restart` など)
/// - `WHY:` 行が無い / 空 / 2 つ以上
/// - プロンプトをそのまま返してきた場合 (雛形の `<observe|...>` は未知名、
///   さらに本物の行と併せて 2 行になるのでどちらの経路でも弾かれる)
pub fn parse_response(raw: &str) -> Option<ParsedAdvice> {
    if raw.trim().is_empty() {
        return None;
    }

    let mut actions: Vec<&str> = Vec::new();
    let mut whys: Vec<&str> = Vec::new();

    for line in raw.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("ACTION:") {
            actions.push(rest.trim());
        } else if let Some(rest) = line.strip_prefix("WHY:") {
            whys.push(rest.trim());
        }
    }

    // 0 個は「答えていない」、2 個以上は「どれが答えか決められない」。どちらも黙る。
    if actions.len() != 1 || whys.len() != 1 {
        return None;
    }

    let action = action_from_name(actions[0])?;

    let why_src = whys[0];
    if why_src.is_empty() {
        return None;
    }
    let why: String = why_src.chars().take(MAX_WHY_CHARS).collect();

    Some(ParsedAdvice { action, why })
}

// ---------------------------------------------------------------------------
// 安全上限 (防御の一段目)
// ---------------------------------------------------------------------------

/// 異常種別ごとに「決定的な層が最終的に狙う介入」= LLM に許す上限。
///
/// これは `supervisor::Anomaly::desired_action` の写しである (向こうは非公開)。
/// 意図的な二重管理: 診断役は supervisor の内部に踏み込まずに自前で上限を持ち、
/// 万一どちらかがずれても**厳しいほうが勝つ** (min を取るため)。
pub fn ceiling_for(anomaly: Anomaly) -> Intervention {
    match anomaly {
        Anomaly::SilentWait => Intervention::AutoAnswer,
        Anomaly::Stall => Intervention::Nudge,
        Anomaly::Crash => Intervention::Restart,
        Anomaly::Looping | Anomaly::ErrorStorm | Anomaly::Runaway => Intervention::Halt,
    }
}

/// 助言を上限まで丸める。`Intervention` は重い順に `Ord` なので min でよい。
pub fn clamp_action(action: Intervention, anomaly: Anomaly, max_action: Intervention) -> Intervention {
    action.min(ceiling_for(anomaly)).min(max_action)
}

/// この介入はユーザー確認を要するか。
///
/// 破壊的 (`Restart` / `Halt`) は LLM 由来である以上、設定に関わらず必ず確認を取る。
/// 実際の確認フラグ立ては `supervisor::intent_from_diagnosis` が行うが、
/// UI 側が診断単体を扱うときのためにここでも同じ判定を公開しておく。
pub fn requires_confirmation(action: Intervention) -> bool {
    action.destructive()
}

// ---------------------------------------------------------------------------
// 起動コマンドの組み立て (純関数)
// ---------------------------------------------------------------------------

/// プリセットのコマンドとカタログ定義から、ヘッドレス 1 発実行の
/// (実行ファイル, プロンプト直前までの引数) を作る。
///
/// `headless` には 2 形式ある:
/// - **フラグ型** (`-p` / `--print` / `-x` / `--no-interactive` ...) → ユーザー引数の後ろに付ける
/// - **サブコマンド型** (`codex exec` / `goose run -t` / `acli rovodev run` ...)
///   → 先頭トークンが実行ファイル名なのでそれを落とし、残りを引数の**先頭**に置く
///
/// `headless` が空の CLI は非対話実行の手段が無い。そのまま起動すると対話 TUI が
/// 立ち上がって永久に返らないので、ここで拒否する (握りつぶさずエラーにする)。
#[allow(dead_code)]
pub fn build_invocation(command: &str, spec: &AgentSpec) -> Result<(String, Vec<String>), String> {
    let mut toks = command.split_whitespace();
    let program = toks
        .next()
        .ok_or_else(|| "コマンドが空です".to_string())?
        .to_string();
    let user_args: Vec<String> = toks.map(|s| s.to_string()).collect();

    let headless = spec.headless.trim();
    if headless.is_empty() {
        return Err(format!(
            "{} は非対話(ヘッドレス)実行に対応していないため診断役にできません",
            spec.label
        ));
    }

    let mut hl: Vec<&str> = headless.split_whitespace().collect();
    let mut args: Vec<String> = Vec::new();

    // サブコマンド型なら先頭の実行ファイル名を落とし、サブコマンドを最前に置く。
    if hl.first().map(|t| *t == spec.bin).unwrap_or(false) {
        hl.remove(0);
        args.extend(hl.iter().map(|s| s.to_string()));
        args.extend(user_args);
    } else {
        // フラグ型はユーザー引数の後ろ。プロンプトはさらにその後ろに置かれる。
        args.extend(user_args);
        args.extend(hl.iter().map(|s| s.to_string()));
    }

    Ok((program, args))
}

// ---------------------------------------------------------------------------
// プロンプト組み立て (純関数)
// ---------------------------------------------------------------------------

/// 診断依頼のプロンプトを作る。
///
/// 抜粋は `supervisor::redact` を**もう一度**通す。呼び出し側 (supervisor) が既に
/// 秘匿化しているが、ここは外部プロセスへ文字列を渡す最後の関門なので二重にかける。
/// `redact` は末尾 `limit` 文字を残す実装なので、自動的に「直近だけ」になる。
pub fn build_prompt(req: &DiagnosisRequest, state_secs: Option<u64>, limit: usize) -> String {
    let limit = limit.min(MAX_EXCERPT_CHARS);
    let excerpt = redact(&req.excerpt, limit);
    let since = match state_secs {
        Some(s) if s >= 60 => format!("約 {} 分", s / 60),
        Some(s) => format!("約 {s} 秒"),
        None => "不明".to_string(),
    };

    let mut p = String::with_capacity(excerpt.len() + 1024);
    p.push_str("あなたは、別の CLI コーディングエージェントを見張る監督役です。\n");
    p.push_str("以下は、行き詰まっている可能性のあるセッションの状況です。\n\n");
    p.push_str(&format!("セッション名: {}\n", req.session_title));
    p.push_str(&format!("検出された異常: {}\n", req.anomaly.label()));
    p.push_str(&format!("この状態が続いている時間: {since}\n"));
    p.push_str(&format!(
        "直近の出力 (末尾 {limit} 文字まで、秘匿化済み):\n---\n{excerpt}\n---\n\n"
    ));
    p.push_str("取れる行動は次の 6 つだけです。\n");
    p.push_str("  observe     … 記録するだけ (まだ様子を見る)\n");
    p.push_str("  notify      … 利用者に通知する\n");
    p.push_str("  auto_answer … 承認待ちに Enter を送る\n");
    p.push_str("  nudge       … 続行を促すメッセージを送る\n");
    p.push_str("  restart     … セッションを再起動する (作業内容を失う恐れ)\n");
    p.push_str("  halt        … セッションを停止する (作業内容を失う恐れ)\n\n");
    p.push_str("回答は、次の 2 行**だけ**を、この綴りのまま出力してください。\n");
    p.push_str("前置き・後書き・コードブロック・箇条書きは一切付けないでください。\n");
    p.push_str("行動名は小文字のみです。上の 6 つ以外は書かないでください。\n\n");
    p.push_str("ACTION: <observe|notify|auto_answer|nudge|restart|halt>\n");
    p.push_str("WHY: <日本語 1 行の理由>\n");
    p
}

// ---------------------------------------------------------------------------
// 本体
// ---------------------------------------------------------------------------

/// CLI エージェントをヘッドレスで叩いて診断してもらう `Diagnostician`。
pub struct CliDiagnostician {
    /// 表示用の元コマンド文字列。
    #[allow(dead_code)]
    command: String,
    /// UI 表示名 (カタログ由来)。
    #[allow(dead_code)]
    label: String,
    program: String,
    /// プロンプト直前までの固定引数。
    args: Vec<String>,
    cwd: Option<PathBuf>,
    timeout: Duration,
    /// ここより重い介入は返さない (異常種別ごとの上限とあわせて min を取る)。
    max_action: Intervention,
    /// 診断役自身のセッション ID。ここと一致する依頼は必ず断る。
    self_session_id: Mutex<Option<u64>>,
    /// 「この異常をいつから見ているか」。プロンプトの継続時間表示に使う。
    state_since: Mutex<HashMap<u64, (Anomaly, Instant)>>,
    /// 直近に `None` を返した理由 (UI から見せる用)。
    last_error: Mutex<Option<String>>,
}

impl CliDiagnostician {
    /// プリセットのコマンド文字列から作る。
    ///
    /// カタログに無い CLI、または `headless` が空の CLI では `Err` を返し、
    /// **診断役として構築させない**。対話 TUI を起動してしまうと、期限まで
    /// ぶら下がったうえに何も返さないという最悪の挙動になるため。
    #[allow(dead_code)]
    pub fn new(command: &str, cwd: Option<PathBuf>) -> Result<Self, String> {
        let spec = spec_for_command(command)
            .ok_or_else(|| format!("`{command}` は既知のエージェント CLI ではありません"))?;
        let (program, args) = build_invocation(command, spec)?;
        Ok(Self {
            command: command.to_string(),
            label: spec.label.to_string(),
            program,
            args,
            cwd,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            max_action: Intervention::Halt,
            self_session_id: Mutex::new(None),
            state_since: Mutex::new(HashMap::new()),
            last_error: Mutex::new(None),
        })
    }

    /// ハード期限を変える (0 秒は許さず最低 5 秒に丸める)。
    #[allow(dead_code)]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout.max(Duration::from_secs(5));
        self
    }

    /// 全体の安全上限を下げる (既定は `Halt` = 異常種別ごとの上限のみ)。
    #[allow(dead_code)]
    pub fn with_max_action(mut self, max_action: Intervention) -> Self {
        self.max_action = max_action;
        self
    }

    /// 診断役自身が動いているセッション ID を登録する。
    ///
    /// 診断役の CLI は専用枠ではなく普通の作業にも使える。そのセッションが
    /// 詰まったときに自分自身へ診断を頼むと、返事を書けるはずのプロセスが
    /// まさに固まっているので永久に返らない。ここに入れておけば必ず断る。
    #[allow(dead_code)]
    pub fn set_self_session(&self, id: Option<u64>) {
        if let Ok(mut g) = self.self_session_id.lock() {
            *g = id;
        }
    }

    /// 登録済みの自セッション ID。
    pub fn self_session_id(&self) -> Option<u64> {
        self.self_session_id.lock().ok().and_then(|g| *g)
    }

    /// 表示用の情報 (コマンド, 表示名)。
    #[allow(dead_code)]
    pub fn describe(&self) -> (&str, &str) {
        (&self.command, &self.label)
    }

    /// 直近に診断を見送った理由。成功時は `None`。
    #[allow(dead_code)]
    pub fn last_error(&self) -> Option<String> {
        self.last_error.lock().ok().and_then(|g| g.clone())
    }

    fn note_error(&self, msg: impl Into<String>) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = Some(msg.into());
        }
    }

    fn clear_error(&self) {
        if let Ok(mut g) = self.last_error.lock() {
            *g = None;
        }
    }

    /// この異常が続いている秒数。初回は 0 秒として記録する。
    fn state_secs(&self, id: u64, anomaly: Anomaly) -> Option<u64> {
        let mut g = self.state_since.lock().ok()?;
        // 有界化: 覚えすぎたら丸ごと捨てる (精度より上限を優先する)。
        if g.len() >= MAX_TRACKED_SESSIONS {
            g.clear();
        }
        let e = g.entry(id).or_insert((anomaly, Instant::now()));
        if e.0 != anomaly {
            *e = (anomaly, Instant::now());
        }
        Some(e.1.elapsed().as_secs())
    }

    /// 子プロセスを起動してプロンプトを渡し、stdout を返す。
    ///
    /// `github.rs::capture` と同じ作法: stdin は null、stdout/stderr は
    /// 読み取りスレッドで読み切り、期限を過ぎたら kill して wait する。
    fn run(&self, prompt: &str) -> Result<String, String> {
        let mut cmd = Command::new(&self.program);
        cmd.args(&self.args);
        cmd.arg(prompt);
        if let Some(dir) = &self.cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .env("NO_COLOR", "1")
            .env("CLICOLOR", "0")
            .env("TERM", "dumb")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("診断役 `{}` を起動できません: {e}", self.program))?;

        let out_rx = child.stdout.take().map(spawn_capped_reader);
        let err_rx = child.stderr.take().map(spawn_capped_reader);

        let deadline = Instant::now() + self.timeout;
        let status = loop {
            match child.try_wait() {
                Ok(Some(st)) => break st,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(format!(
                            "診断役の応答が {} 秒を超えたため中断しました",
                            self.timeout.as_secs()
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(e) => return Err(format!("診断役の終了待ちに失敗しました: {e}")),
            }
        };

        // kill 済みでもパイプが閉じるので join は必ず戻る。
        let stdout = out_rx.and_then(|h| h.join().ok()).unwrap_or_default();
        let stderr = err_rx.and_then(|h| h.join().ok()).unwrap_or_default();

        if !status.success() {
            let tail: String = stderr
                .trim()
                .chars()
                .rev()
                .take(200)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            return Err(format!(
                "診断役が異常終了しました (code={:?}): {tail}",
                status.code()
            ));
        }
        Ok(stdout)
    }
}

/// 上限付きで読み切るリーダースレッド。**保持は有界、読み取りは EOF まで**。
///
/// 上限を超えた分は捨てるが、読むのはやめない。途中で読むのをやめると
/// パイプが詰まって子プロセスが write でブロックし、期限まで無駄に待つことになる
/// (暴走して大量出力する相手ほどこの罠にはまる)。
fn spawn_capped_reader<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf: Vec<u8> = Vec::with_capacity(4096);
        let mut chunk = [0u8; 4096];
        loop {
            match r.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) => {
                    if buf.len() < MAX_RESPONSE_BYTES {
                        let room = MAX_RESPONSE_BYTES - buf.len();
                        buf.extend_from_slice(&chunk[..n.min(room)]);
                    }
                    // 上限到達後も読み捨てて相手を進ませる。
                }
                Err(_) => break,
            }
        }
        // 打ち切りで壊れた UTF-8 が末尾に残りうるので lossy で受ける。
        String::from_utf8_lossy(&buf).to_string()
    })
}

impl Diagnostician for CliDiagnostician {
    fn diagnose(&self, req: &DiagnosisRequest) -> Option<Diagnosis> {
        // 1. 自己診断の禁止 — 詰まっている本人に「なぜ詰まったか」は訊けない。
        if self.self_session_id() == Some(req.session_id) {
            self.note_error("診断役自身のセッションのため診断を見送りました");
            return None;
        }

        // 2. 念のための保険: 引数が組み立てられていない (= ヘッドレス不可) なら動かさない。
        if self.program.is_empty() {
            self.note_error("ヘッドレス実行の指定が無いため診断できません");
            return None;
        }

        let secs = self.state_secs(req.session_id, req.anomaly);
        let prompt = build_prompt(req, secs, MAX_EXCERPT_CHARS);

        let raw = match self.run(&prompt) {
            Ok(s) => s,
            Err(e) => {
                self.note_error(e);
                return None;
            }
        };

        // 3. 解析できないものは全部 None。介入を捏造するくらいなら黙る。
        let Some(advice) = parse_response(&raw) else {
            self.note_error("診断役の応答が所定の形式ではないため採用しませんでした");
            return None;
        };

        // 4. 上限まで丸める (supervisor 側の gate でもう一度丸められる)。
        let recommended = clamp_action(advice.action, req.anomaly, self.max_action);
        let mut summary = advice.why;
        if recommended != advice.action {
            summary.push_str(&format!(
                " (推奨『{}』は安全上限により『{}』へ引き下げ)",
                advice.action.label(),
                recommended.label()
            ));
        }
        if requires_confirmation(recommended) {
            summary.push_str(" ※実行前に確認が必要です");
        }

        self.clear_error();
        Some(Diagnosis {
            session_id: req.session_id,
            anomaly: req.anomaly,
            summary,
            recommended,
        })
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Approval;
    use crate::supervisor::{gate, GateResult, SessionState, SupervisorConfig};

    /// テスト用: カタログを通さず任意のコマンドで組み立てる。
    fn raw(program: &str, args: &[&str], timeout_secs: u64) -> CliDiagnostician {
        CliDiagnostician {
            command: program.to_string(),
            label: program.to_string(),
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            timeout: Duration::from_secs(timeout_secs),
            max_action: Intervention::Halt,
            self_session_id: Mutex::new(None),
            state_since: Mutex::new(HashMap::new()),
            last_error: Mutex::new(None),
        }
    }

    fn req(anomaly: Anomaly, excerpt: &str) -> DiagnosisRequest {
        DiagnosisRequest {
            session_id: 7,
            session_title: "テスト用セッション".to_string(),
            anomaly,
            state: SessionState::Stalled,
            excerpt: excerpt.to_string(),
        }
    }

    const SPEC_FLAG: AgentSpec = AgentSpec {
        bin: "claude",
        label: "Claude Code",
        icon: "",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "-p",
        model_flag: "",
        install: "",
        note: "",
        switch_keys: "",
        switch_hint: "",
    };

    const SPEC_SUB: AgentSpec = AgentSpec {
        bin: "codex",
        label: "Codex",
        icon: "",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "codex exec",
        model_flag: "",
        install: "",
        note: "",
        switch_keys: "",
        switch_hint: "",
    };

    const SPEC_SUB_MULTI: AgentSpec = AgentSpec {
        bin: "goose",
        label: "Goose",
        icon: "",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "goose run -t",
        model_flag: "",
        install: "",
        note: "",
        switch_keys: "",
        switch_hint: "",
    };

    const SPEC_EMPTY: AgentSpec = AgentSpec {
        bin: "nohead",
        label: "ヘッドレス非対応 CLI",
        icon: "",
        auto_flag: "",
        auto_env: &[],
        strip: &[],
        headless: "",
        model_flag: "",
        install: "",
        note: "",
        switch_keys: "",
        switch_hint: "",
    };

    // --- 応答解析: 正常系 ---

    #[test]
    fn parses_strict_two_line_form() {
        let a = parse_response("ACTION: nudge\nWHY: 同じテストで止まっている\n").unwrap();
        assert_eq!(a.action, Intervention::Nudge);
        assert_eq!(a.why, "同じテストで止まっている");
    }

    #[test]
    fn parses_all_known_action_names() {
        for (name, want) in ACTION_TABLE {
            let s = format!("ACTION: {name}\nWHY: 理由");
            assert_eq!(parse_response(&s).unwrap().action, *want, "{name}");
        }
    }

    #[test]
    fn tolerates_surrounding_whitespace_only() {
        let a = parse_response("   ACTION:   halt   \n  WHY:   暴走中   ").unwrap();
        assert_eq!(a.action, Intervention::Halt);
        assert_eq!(a.why, "暴走中");
    }

    // --- 応答解析: 異常系 (ここが本体。全部 None でなければならない) ---

    #[test]
    fn rejects_empty_output() {
        assert!(parse_response("").is_none());
        assert!(parse_response("   \n\n \t ").is_none());
    }

    #[test]
    fn rejects_prose_without_action_line() {
        let s = "セッションを見た限り、テストが失敗し続けているようです。\
                 おそらく再起動すればよいと思います。";
        assert!(parse_response(s).is_none());
    }

    #[test]
    fn rejects_two_conflicting_action_lines() {
        let s = "ACTION: nudge\nWHY: 理由A\nACTION: halt\nWHY: 理由B";
        assert!(parse_response(s).is_none());
    }

    #[test]
    fn rejects_duplicate_identical_action_lines() {
        // 同じ答えが 2 回でも「どちらを採るか」を機械が決められないので不採用。
        let s = "ACTION: nudge\nWHY: 理由\nACTION: nudge";
        assert!(parse_response(s).is_none());
    }

    #[test]
    fn rejects_unknown_action_name() {
        assert!(parse_response("ACTION: reboot\nWHY: 理由").is_none());
        assert!(parse_response("ACTION: kill -9\nWHY: 理由").is_none());
        assert!(parse_response("ACTION: \nWHY: 理由").is_none());
    }

    #[test]
    fn rejects_wrong_case_action_name() {
        assert!(parse_response("ACTION: Restart\nWHY: 理由").is_none());
        assert!(parse_response("ACTION: HALT\nWHY: 理由").is_none());
        assert!(parse_response("action: nudge\nwhy: 理由").is_none());
    }

    #[test]
    fn rejects_missing_or_empty_why() {
        assert!(parse_response("ACTION: nudge").is_none());
        assert!(parse_response("ACTION: nudge\nWHY:").is_none());
        assert!(parse_response("ACTION: nudge\nWHY:   ").is_none());
        assert!(parse_response("ACTION: nudge\nWHY: 理由A\nWHY: 理由B").is_none());
    }

    #[test]
    fn rejects_echoed_prompt() {
        // プロンプトをそのまま返してきたケース: 雛形の <observe|...> は未知名。
        let r = req(Anomaly::Stall, "なんらかの出力");
        let echoed = build_prompt(&r, Some(120), MAX_EXCERPT_CHARS);
        assert!(parse_response(&echoed).is_none());

        // プロンプト全文のあとに本物の答えを付けてきたケースも、ACTION 行が 2 本になり不採用。
        let echoed_plus = format!("{echoed}\nACTION: nudge\nWHY: 理由");
        assert!(parse_response(&echoed_plus).is_none());
    }

    #[test]
    fn caps_why_length() {
        let long = "あ".repeat(1000);
        let a = parse_response(&format!("ACTION: notify\nWHY: {long}")).unwrap();
        assert_eq!(a.why.chars().count(), MAX_WHY_CHARS);
    }

    // --- 起動コマンドの組み立て ---

    #[test]
    fn refuses_agent_without_headless_form() {
        let e = build_invocation("nohead --flag", &SPEC_EMPTY).unwrap_err();
        assert!(e.contains("非対話"), "{e}");
        // カタログ経由でも同じ: 未知コマンドは構築させない。
        assert!(CliDiagnostician::new("definitely-not-an-agent", None).is_err());
    }

    #[test]
    fn builds_flag_style_invocation() {
        let (p, a) = build_invocation("claude --model opus", &SPEC_FLAG).unwrap();
        assert_eq!(p, "claude");
        assert_eq!(a, vec!["--model", "opus", "-p"]);
    }

    #[test]
    fn builds_subcommand_style_invocation() {
        let (p, a) = build_invocation("codex --yolo", &SPEC_SUB).unwrap();
        assert_eq!(p, "codex");
        // サブコマンドは必ず最前。ユーザー引数はその後ろ。
        assert_eq!(a, vec!["exec", "--yolo"]);

        let (p2, a2) = build_invocation("goose", &SPEC_SUB_MULTI).unwrap();
        assert_eq!(p2, "goose");
        assert_eq!(a2, vec!["run", "-t"]);
    }

    #[test]
    fn real_catalog_agents_all_build() {
        // カタログ全 CLI がヘッドレス形を持ち、組み立てに成功すること。
        for spec in crate::agents::AGENT_CATALOG {
            let (p, a) = build_invocation(spec.bin, spec).expect(spec.bin);
            assert_eq!(p, spec.bin);
            assert!(!a.is_empty(), "{}: 引数が空", spec.bin);
            assert_ne!(a[0], spec.bin, "{}: 実行ファイル名が引数に残っている", spec.bin);
        }
    }

    // --- プロンプト ---

    #[test]
    fn prompt_redacts_and_caps_excerpt() {
        let secret = "token sk-abcdefghijklmnopqrstuvwxyz0123 と user@example.com";
        let noise = "ログ行\n".repeat(5000);
        let r = req(Anomaly::ErrorStorm, &format!("{noise}{secret}"));
        let p = build_prompt(&r, Some(180), MAX_EXCERPT_CHARS);

        // 秘匿化されている
        assert!(!p.contains("sk-abcdefghijklmnopqrstuvwxyz0123"), "トークンが漏れている");
        assert!(!p.contains("user@example.com"), "メールアドレスが漏れている");
        assert!(p.contains("***"));

        // 抜粋は上限まで。全文 (5000 行) は載らない。
        assert!(
            p.len() < 12_000,
            "プロンプトが長すぎる: {} バイト",
            p.len()
        );
        assert!(p.matches("ログ行").count() < 2000);

        // 直近側 (末尾) が残っている
        assert!(p.contains("と"), "末尾側の抜粋が失われている");

        // 必要な情報が入っている
        assert!(p.contains("エラー多発"));
        assert!(p.contains("約 3 分"));
        assert!(p.contains("ACTION:"));
        assert!(p.contains("WHY:"));
    }

    #[test]
    fn prompt_handles_unknown_duration() {
        let r = req(Anomaly::Stall, "x");
        let p = build_prompt(&r, None, MAX_EXCERPT_CHARS);
        assert!(p.contains("不明"));
        let p2 = build_prompt(&r, Some(45), MAX_EXCERPT_CHARS);
        assert!(p2.contains("約 45 秒"));
    }

    // --- 安全上限 ---

    #[test]
    fn clamps_action_to_anomaly_ceiling() {
        // 停滞に対して halt を勧められても nudge までしか上げない。
        assert_eq!(
            clamp_action(Intervention::Halt, Anomaly::Stall, Intervention::Halt),
            Intervention::Nudge
        );
        // 異常終了に対する halt は restart まで。
        assert_eq!(
            clamp_action(Intervention::Halt, Anomaly::Crash, Intervention::Halt),
            Intervention::Restart
        );
        // 承認待ちは auto_answer が上限。
        assert_eq!(
            clamp_action(Intervention::Restart, Anomaly::SilentWait, Intervention::Halt),
            Intervention::AutoAnswer
        );
        // 上限より軽い助言はそのまま。
        assert_eq!(
            clamp_action(Intervention::Observe, Anomaly::Runaway, Intervention::Halt),
            Intervention::Observe
        );
        // 全体上限も効く。
        assert_eq!(
            clamp_action(Intervention::Halt, Anomaly::Runaway, Intervention::Notify),
            Intervention::Notify
        );
    }

    #[test]
    fn destructive_recommendation_still_needs_confirmation() {
        // 二層とも確認を要求することを示す。
        // 一層目 (ここ):
        assert!(requires_confirmation(Intervention::Restart));
        assert!(requires_confirmation(Intervention::Halt));
        assert!(!requires_confirmation(Intervention::Nudge));

        // 二層目 (supervisor の gate): 既定設定ではどの承認モードでも確認になる。
        let cfg = SupervisorConfig::default();
        for ap in [Approval::Ask, Approval::Auto, Approval::Agent] {
            for act in [Intervention::Restart, Intervention::Halt] {
                assert!(
                    matches!(gate(act, ap, &cfg), GateResult::NeedConfirm(_)),
                    "{act:?} が無確認で通った"
                );
            }
        }
    }

    #[test]
    fn e2e_destructive_response_is_clamped_and_flagged() {
        // 暴走に対して halt を勧める応答 → halt のまま返るが確認必須の印が付く。
        let d = raw("/bin/sh", &["-c", "printf 'ACTION: halt\\nWHY: 出力が止まらない\\n'; :"], 10);
        // /bin/sh -c <script> <prompt> の形になるので prompt は $0 に入る (無害)。
        let out = d.diagnose(&req(Anomaly::Runaway, "loop loop loop")).unwrap();
        assert_eq!(out.recommended, Intervention::Halt);
        assert!(out.summary.contains("確認が必要"), "{}", out.summary);
        assert!(requires_confirmation(out.recommended));

        // 停滞に対して restart を勧める応答 → nudge へ引き下げ。
        let d2 = raw("/bin/sh", &["-c", "printf 'ACTION: restart\\nWHY: 反応が無い\\n'"], 10);
        let out2 = d2.diagnose(&req(Anomaly::Stall, "...")).unwrap();
        assert_eq!(out2.recommended, Intervention::Nudge);
        assert!(out2.summary.contains("引き下げ"), "{}", out2.summary);
    }

    // --- 自己診断ガード ---

    #[test]
    fn refuses_to_diagnose_itself() {
        // 実行したら 30 秒固まるコマンドを設定しておく。ガードが効けば起動されない。
        let d = raw("/bin/sh", &["-c", "sleep 30"], 60);
        d.set_self_session(Some(7));
        assert_eq!(d.self_session_id(), Some(7));

        let t0 = Instant::now();
        assert!(d.diagnose(&req(Anomaly::Stall, "自分が止まっている")).is_none());
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "自己診断ガードが子プロセスを起動してしまった"
        );
        assert!(d.last_error().unwrap().contains("診断役自身"));

        // 別セッションなら通常どおり動く。
        let d2 = raw("/bin/sh", &["-c", "printf 'ACTION: notify\\nWHY: 通知だけ\\n'"], 10);
        d2.set_self_session(Some(999));
        assert!(d2.diagnose(&req(Anomaly::Stall, "x")).is_some());
    }

    // --- 実行系 ---

    #[test]
    fn nonzero_exit_yields_none_with_reason() {
        let d = raw("/bin/sh", &["-c", "echo 'boom' >&2; exit 3"], 10);
        assert!(d.diagnose(&req(Anomaly::Stall, "x")).is_none());
        let e = d.last_error().unwrap();
        assert!(e.contains("異常終了"), "{e}");
        assert!(e.contains("3"), "{e}");
    }

    #[test]
    fn missing_program_yields_none_with_reason() {
        let d = raw("/nonexistent/zaivern-diag-bin", &[], 10);
        assert!(d.diagnose(&req(Anomaly::Stall, "x")).is_none());
        assert!(d.last_error().unwrap().contains("起動できません"));
    }

    #[test]
    fn hard_timeout_kills_child() {
        let d = raw("/bin/sh", &["-c", "sleep 60"], 5);
        let t0 = Instant::now();
        assert!(d.diagnose(&req(Anomaly::Stall, "x")).is_none());
        let took = t0.elapsed();
        assert!(took < Duration::from_secs(20), "期限で切れていない: {took:?}");
        assert!(d.last_error().unwrap().contains("中断"));
    }

    #[test]
    fn huge_response_is_bounded_and_rejected() {
        // 大量出力でもバッファは MAX_RESPONSE_BYTES で止まり、形式不一致で None。
        let d = raw("/bin/sh", &["-c", "yes ぐるぐる | head -c 2000000"], 30);
        assert!(d.diagnose(&req(Anomaly::Runaway, "x")).is_none());
        assert!(d.last_error().unwrap().contains("形式"));
    }

    // --- 実機確認 (既定では走らせない) ---

    /// 実際に `claude -p` を叩いて 1 往復させる。
    /// 実行: `cargo test diagnostician::tests::live_claude -- --ignored --nocapture`
    #[test]
    #[ignore = "実機の claude CLI が必要"]
    fn live_claude_end_to_end() {
        let d = CliDiagnostician::new("claude", None)
            .unwrap()
            .with_timeout(Duration::from_secs(120));
        let (cmd, label) = d.describe();
        println!("--- 起動: {label} ({cmd}) -> {} {:?}", d.program, d.args);

        let stuck = "$ cargo test\n\
             error[E0308]: mismatched types\n\
             error[E0308]: mismatched types\n\
             error[E0308]: mismatched types\n\
             (以後 20 分、同じエラーで再試行を繰り返している)\n\
             API key sk-ant-0123456789abcdefghijklmnopqrstuvwxyz\n\
             /Users/tacyan/dev/zaivern-code/src/app.rs:120\n";
        let r = req(Anomaly::Looping, stuck);

        println!("--- 送信プロンプト (秘匿化後) ---");
        println!("{}", build_prompt(&r, Some(1200), MAX_EXCERPT_CHARS));
        println!("--- 応答 ---");
        match d.diagnose(&r) {
            Some(out) => println!("recommended={:?}\nsummary={}", out.recommended, out.summary),
            None => println!("None / 理由: {:?}", d.last_error()),
        }
    }

    #[test]
    fn state_duration_resets_when_anomaly_changes() {
        let d = raw("/bin/true", &[], 10);
        let s1 = d.state_secs(1, Anomaly::Stall).unwrap();
        assert_eq!(s1, 0);
        let s2 = d.state_secs(1, Anomaly::Crash).unwrap();
        assert_eq!(s2, 0);
    }

    #[test]
    fn tracked_sessions_are_bounded() {
        let d = raw("/bin/true", &[], 10);
        for i in 0..(MAX_TRACKED_SESSIONS as u64 * 3) {
            let _ = d.state_secs(i, Anomaly::Stall);
        }
        let n = d.state_since.lock().unwrap().len();
        assert!(n <= MAX_TRACKED_SESSIONS, "追跡数が上限を超えた: {n}");
    }
}
