//! **ベンダー提供フック段** — 状態ラダーの 2 段目 (CLAUDE.md 設計原則 #4)。
//!
//! ベンダー CLI が「いま何が起きたか」を自分から知らせてくる口。画面を読まない。
//!
//! ## 受け口はファイルの投函箱 (ポートを開かない)
//! `remote.rs` が 8899〜 を使っているので、フックのためにポートを増やさない。
//! フックは Zaivern の実行ファイル自身を `zai hook --zaivern <agent> <event>`
//! として呼び、標準入力で渡された JSON を `~/.zaivern/hooks/` へ 1 ファイル置く。
//! GUI 側は見張りのサンプリング周期 (既定 1 秒) にだけ投函箱を空にする
//! ([`drain`]) ので、**アイドル時の追加コストは readdir 1 回**で済む
//! (設計原則 3)。
//!
//! ## 設置はユーザーの同意を取ってから
//! [`install`] はボタンを押したときにだけ走る。ユーザーの設定ファイルは
//! **消さずに併合**し、初回だけ `<settings>.zaivern.bak` を残す。
//! [`uninstall`] は自分が足した項目**だけ**を外して元へ戻す。
//!
//! ## エージェント固有値はここに置かない
//! 設定ファイルの場所・フックイベント名・イベント → 状態の対応は
//! [`crate::agents::HOOK_TARGETS`] のカタログにデータとして持つ。
//!
//! ## 実機で確認できたこと (2026-08 時点)
//! - `claude` 2.1.226 は `--output-format stream-json` の出力に
//!   `{"type":"system","subtype":"hook_started","hook_event":"SessionStart",…}` を
//!   流す = フックが**実際に発火している**ことを観測した。
//! - 設定の形は実在する `~/.claude/settings.json` から確認した:
//!   `hooks.<Event>[] = { matcher, hooks: [ { type:"command", command, timeout, async } ] }`。
//!   その環境で有効だったイベント名は Elicitation / Notification /
//!   PermissionRequest / PostCompact / PostToolUse / PostToolUseFailure /
//!   PreCompact / PreToolUse / SessionEnd / SessionStart / Stop / StopFailure /
//!   SubagentStart / SubagentStop / UserPromptSubmit。
//!   このうち**意味が名前から一意に決まるものだけ**をカタログへ入れている。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use eframe::egui;
use serde_json::{Map, Value};

use super::protocol::{ProtoRead, ProtoState};

// ---------------------------------------------------------------------------
// カタログが持つデータの型
// ---------------------------------------------------------------------------

/// 1 エージェント分のフック設定。実データは [`crate::agents::HOOK_TARGETS`]。
pub struct HookTarget {
    /// カタログ上の実行ファイル名 (`AgentSpec::bin`)。
    pub bin: &'static str,
    /// 設定ファイルの、プロジェクト直下 (または各種ルート) からの相対パス。
    /// パス区切りは `/` で書く — [`Path::join`] が OS ごとに解決する。
    pub settings_rel: &'static str,
    /// `(イベント名, 状態, ツール名で細分してよいか)`。
    pub events: &'static [(&'static str, ProtoState, bool)],
    /// **リースの強制に使うイベント名**。
    ///
    /// claude / codex は `PreToolUse`、gemini は `BeforeTool` と**名前が違う**。
    /// ここを決め打ちにしていたせいで、gemini ではフックを設置しても
    /// `lease::gate` が入口で降りて 1 件も止まらなかった。
    /// 必ず [`HookTarget::events`] に載っているイベントであること
    /// (カタログ整合テストが強制する)。
    pub gate_event: &'static str,
    /// ツール名 → 状態の細分表。
    pub tools: &'static [(&'static str, ProtoState)],
    /// 書き込み系ツールのペイロードから**対象ファイルのパス**を取り出すキー
    /// (優先順)。`tool_input.<key>` を順に見る。
    ///
    /// ファイル所有リース ([`crate::lease`]) の強制がここを読む。
    /// **キー名はベンダーごとに違うのでカタログに置く** — 機構側
    /// (`lease::target_path`) はリテラルを 1 つも持たない。
    pub write_path_keys: &'static [&'static str],
    /// `(ツール名, コマンド文字列が載る `tool_input` のキー)`。
    ///
    /// パスを持たず**シェルのコマンド行**を持つツール (`Bash` /
    /// `run_shell_command` 等)。[`crate::agents::cmdwrite`] が解析する。
    pub command_tools: &'static [(&'static str, &'static str)],
    /// `(ツール名, パッチ本文が載る `tool_input` のキー)`。
    ///
    /// codex の `apply_patch` は**パス欄を持たず**、対象が本文の
    /// `*** Update File: <path>` に書かれている。
    /// [`crate::agents::patchpath`] が解析する。
    pub patch_tools: &'static [(&'static str, &'static str)],
    /// **設置しただけでは効かない**ベンダーのための、もう一段の確認。
    ///
    /// 空なら「設置＝有効」。空でなければ [`activation_gaps`] が実際に調べ、
    /// 満たされていない間は [`HookStatus::Inactive`] を返す。
    pub activation: &'static [Activation],
    /// 拒否の返し方 (ベンダーごとに JSON の形も終了コードも違う)。
    pub deny: DenyShape,
    /// **実機で確認した方法**。空は禁止 (カタログ整合テストが落とす)。
    pub verified: &'static str,
}

// ---------------------------------------------------------------------------
// 拒否の返し方 — ひな形をカタログに置く
// ---------------------------------------------------------------------------

/// ひな形の中で**理由**に差し替わる文字列。
pub const DENY_REASON: &str = "{reason}";
/// ひな形の中で**イベント名** ([`HookTarget::gate_event`]) に差し替わる文字列。
pub const DENY_EVENT: &str = "{event}";

/// 拒否をベンダーへ返す形。
///
/// ## なぜひな形なのか
/// 形はベンダーごとに違う (claude/codex は `hookSpecificOutput` の入れ子、
/// gemini は top-level の `decision`/`reason`)。分岐を機構側に書くと
/// **ベンダーが増えるたびに `lease.rs` を触る**ことになるので、
/// **JSON そのものをカタログのデータとして持つ**。
///
/// 差し替えは「文字列値がちょうど [`DENY_REASON`] / [`DENY_EVENT`] と
/// 等しい箇所」だけで、**JSON として組み立て直す**。
/// 文字列連結にすると、理由に `"` や改行が入った瞬間に壊れた JSON を
/// 吐いて**拒否が黙って無視される** (= 止まっていると思わせて止まらない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DenyShape {
    /// 拒否 JSON のひな形。**それ自体が正しい JSON であること**
    /// (カタログ整合テストが強制する)。
    pub template: &'static str,
    /// 終了コード。claude は「JSON は exit 0 のときだけ読まれる」。
    pub exit: i32,
}

// ---------------------------------------------------------------------------
// 「設置済みだが有効化されていない」の検出
// ---------------------------------------------------------------------------

/// 設置しただけでは効かないベンダーの、もう一段の条件。
///
/// ## これが無いと何が起きるか
/// codex は設定ファイルへ書いただけでは**黙ってフックを飛ばす**
/// (実機で確認: 未信頼のプロジェクトフックは発火せず、警告も出ない)。
/// その状態で UI に「強制」と出すと、ユーザーは守られていると思って
/// 並列エージェントを走らせる。`lease.rs` の doc が書いている通り
/// **「効いていると思わせて実は勧告」は無いより悪い**。
pub struct Activation {
    /// 調べ方 (ベンダー固有の値はこの中に持つ)。
    pub kind: ActivationKind,
    /// ホーム位置を上書きする環境変数名。無ければ空。
    pub home_env: &'static str,
    /// ホーム位置の既定 (ユーザーのホームからの相対。`/` 区切り)。
    pub home_rel: &'static str,
    /// ホームからの、見に行くファイルの相対パス。
    pub file_rel: &'static str,
    /// UI に出す「何が足りないか」(tr のキーになる日本語原文)。
    pub missing: &'static str,
    /// UI に出す「どう直すか」(tr のキーになる日本語原文)。
    pub how: &'static str,
}

/// 有効化の調べ方。**ベンダー固有のキー名・値はすべてここへ**入れる
/// (機構側 = [`activation_gaps`] はリテラルを 1 つも持たない)。
pub enum ActivationKind {
    /// TOML の設定で、フック 1 件ずつの信頼を管理する形 (codex)。
    ///
    /// 実在の `~/.codex/config.toml` から採取した形:
    /// ```toml
    /// [features]
    /// hooks = true
    ///
    /// [hooks.state."/…/hooks.json:pre_tool_use:0:0"]
    /// enabled = false
    /// trusted_hash = "sha256:…"
    /// ```
    /// 節の名前は `<hooks.json の絶対パス>:<イベント名の snake_case>:<群>:<番号>`。
    HookTrustToml {
        /// 機能フラグの `(テーブル名, キー名)`。`false` なら全部止まる。
        feature: (&'static str, &'static str),
        /// 信頼の記録が並ぶテーブル名 (`.` で入れ子を辿る)。
        state_table: &'static str,
        /// 信頼済みを示すキー。無ければ**未信頼**。
        trusted_key: &'static str,
        /// 明示的に無効化されたことを示すキー (`false` なら止まる)。
        enabled_key: &'static str,
    },
    /// JSON の「信頼したフォルダ」表で、作業ツリーごとに可否が決まる形 (gemini)。
    ///
    /// 信頼されていないフォルダでは**プロジェクトの設定ファイルごと読まれない**
    /// ので、フックも当然効かない。
    TrustedFolderJson {
        /// そのフォルダ自身が信頼されていることを示す値。
        trusted: &'static [&'static str],
        /// **配下も**信頼されることを示す値 (祖先に在れば足りる)。
        trusted_parent: &'static str,
    },
}

/// 満たされていない有効化条件 1 件 (UI に出すための材料)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActivationGap {
    /// 何が足りないか。
    pub missing: String,
    /// どう直すか。
    pub how: String,
    /// 見に行ったファイル (ユーザーが自分で確かめられるように出す)。
    pub file: PathBuf,
}

// ---------------------------------------------------------------------------
// 投函箱
// ---------------------------------------------------------------------------

/// フック本体を呼ぶときの目印。ユーザーが書いた他のフックと取り違えないための
/// 唯一の識別子 (実行ファイルのパスは更新で変わるので当てにしない)。
pub const HOOK_MARK: &str = "hook --zaivern";

/// 1 回の [`drain`] で読む上限。フックが暴走しても UI を止めない。
const DRAIN_MAX: usize = 256;

/// 投函箱に置きっぱなしのファイルを捨てるまでの時間 (ミリ秒)。
/// GUI が落ちている間に溜まったものを、次の起動で古い状態として採らない。
const INBOX_TTL_MS: u64 = 10 * 60 * 1000;

/// フックの投函箱 (`~/.zaivern/hooks/`)。
pub fn inbox_dir() -> PathBuf {
    crate::config::zaivern_dir().join("hooks")
}

/// フックが投函する 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookEvent {
    /// カタログ上の実行ファイル名 (`AgentSpec::bin`)。
    pub agent: String,
    /// ベンダーが振ったセッション ID。
    pub session: String,
    /// フックが走ったときの作業ディレクトリ。
    pub cwd: String,
    /// フックイベント名 (`PreToolUse` 等)。
    pub event: String,
    /// ツール名 (取れたときだけ)。
    pub tool: String,
}

/// 標準入力で渡された JSON (+ 引数) から 1 件を組み立てる **純関数**。
///
/// ペイロードの形はベンダー次第なので、**在るものだけ**拾って残りは空にする。
/// JSON として読めなくても、引数から判る `agent` / `event` は失わない。
pub fn event_from_payload(agent: &str, event: &str, payload: &str) -> HookEvent {
    let v: Value = serde_json::from_str(payload).unwrap_or(Value::Null);
    let get = |keys: &[&str]| -> String {
        for k in keys {
            if let Some(s) = v.get(*k).and_then(Value::as_str) {
                return s.to_string();
            }
        }
        String::new()
    };
    HookEvent {
        agent: agent.to_string(),
        // session_id / sessionId のどちらでも拾えるようにする (表記揺れ対策)。
        session: get(&["session_id", "sessionId"]),
        cwd: get(&["cwd", "workspace"]),
        // 引数が空でもペイロードに在れば拾う。
        event: if event.is_empty() {
            get(&["hook_event_name", "hookEventName"])
        } else {
            event.to_string()
        },
        tool: get(&["tool_name", "toolName"]),
    }
}

/// 1 件を投函箱へ書く (フック本体 = `zai hook` から呼ぶ)。
///
/// ファイル名は衝突しないよう「時刻 + PID + 連番」で作る。
pub fn post(dir: &Path, ev: &HookEvent) -> Result<PathBuf, String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    std::fs::create_dir_all(dir).map_err(|e| format!("投函箱を作成できません: {e}"))?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let name = format!(
        "{nanos}-{}-{}.json",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::SeqCst)
    );
    let path = dir.join(name);
    let json = serde_json::json!({
        "agent": ev.agent,
        "session": ev.session,
        "cwd": ev.cwd,
        "event": ev.event,
        "tool": ev.tool,
    });
    std::fs::write(&path, json.to_string()).map_err(|e| format!("投函できません: {e}"))?;
    Ok(path)
}

/// 投函箱を空にして中身を返す。読んだファイルは消す (二重計上しない)。
///
/// 古すぎるファイル ([`INBOX_TTL_MS`] 超) は**読まずに捨てる** — GUI が落ちて
/// いた間に溜まったものを「いまの状態」として採らないため。
pub fn drain(dir: &Path, now_ms: u64) -> Vec<HookEvent> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(_) => return Vec::new(),
    };
    let mut files: Vec<PathBuf> = rd
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "json"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files.into_iter().take(DRAIN_MAX) {
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_millis() as u64 > INBOX_TTL_MS);
        if !stale {
            if let Ok(raw) = std::fs::read_to_string(&path) {
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    let s = |k: &str| {
                        v.get(k)
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    out.push(HookEvent {
                        agent: s("agent"),
                        session: s("session"),
                        cwd: s("cwd"),
                        event: s("event"),
                        tool: s("tool"),
                    });
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
    let _ = now_ms; // 時刻は呼び出し側の都合。TTL は mtime で見る。
    out
}

// ---------------------------------------------------------------------------
// セッションへの割り当て
// ---------------------------------------------------------------------------

/// ルーティングの相手 (Zaivern 側のセッション 1 件)。
#[derive(Clone, Debug)]
pub struct HookTargetSession {
    pub id: u64,
    /// カタログ上の実行ファイル名。
    pub bin: String,
    pub cwd: PathBuf,
}

/// 2 つのパスが同じディレクトリを指すか。
///
/// Windows は大文字小文字を区別しないファイルシステムが既定なので畳んで比べる。
/// unix はそのまま比べる (両方を実装する — 片側だけの分岐は書かない)。
fn same_dir(a: &Path, b: &Path) -> bool {
    if cfg!(windows) {
        let f = |p: &Path| p.to_string_lossy().to_lowercase().replace('\\', "/");
        f(a) == f(b)
    } else {
        a == b
    }
}

/// フックの通知を Zaivern のセッションへ割り当てて保持する。
///
/// ベンダーのセッション ID は初回だけ cwd + エージェント種別で結びつけ、
/// 以後はその ID で引く (同じフォルダに複数セッションが居ても取り違えない)。
#[derive(Default)]
pub struct HookRouter {
    /// ベンダー session_id → Zaivern の session id
    bound: HashMap<String, u64>,
    /// Zaivern の session id → (読み取り, 受信時刻)
    reads: HashMap<u64, (ProtoRead, u64)>,
}

impl HookRouter {
    /// 受け取った通知を割り当てる。割り当て先が無いものは捨てる。
    pub fn route(&mut self, evs: &[HookEvent], sessions: &[HookTargetSession], now_ms: u64) {
        for ev in evs {
            let Some((state, refine)) = crate::agents::hook_event_state(&ev.agent, &ev.event)
            else {
                continue; // カタログに無いイベント = 意味が確定していない
            };
            let id = match self.bound.get(&ev.session) {
                Some(id) if !ev.session.is_empty() => Some(*id),
                _ => {
                    let cwd = PathBuf::from(&ev.cwd);
                    let cand: Vec<&HookTargetSession> = sessions
                        .iter()
                        .filter(|s| s.bin == ev.agent && same_dir(&s.cwd, &cwd))
                        .collect();
                    // まだどのベンダー ID とも結びついていないものを優先する。
                    let bound_ids: Vec<u64> = self.bound.values().copied().collect();
                    let pick = cand
                        .iter()
                        .find(|s| !bound_ids.contains(&s.id))
                        .or(cand.first())
                        .map(|s| s.id);
                    if let (Some(id), false) = (pick, ev.session.is_empty()) {
                        self.bound.insert(ev.session.clone(), id);
                    }
                    pick
                }
            };
            let Some(id) = id else { continue };
            // ツールを使う直前のイベントだけ、ツール名で状態を細分する (データ駆動)。
            let state = if refine {
                crate::agents::hook_tool_state(&ev.agent, &ev.tool).unwrap_or(state)
            } else {
                state
            };
            self.reads.insert(
                id,
                (
                    ProtoRead {
                        state,
                        detail: ev.tool.clone(),
                    },
                    now_ms,
                ),
            );
        }
    }

    /// いまの判定。`stale_ms` 以上黙っていたら `None` (= 下位段へ降りる)。
    pub fn read(&self, id: u64, now_ms: u64, stale_ms: u64) -> Option<ProtoRead> {
        let (read, at) = self.reads.get(&id)?;
        if now_ms.saturating_sub(*at) > stale_ms {
            return None;
        }
        Some(read.clone())
    }

    /// 1 セッション分を捨てる (`Supervisor::forget`)。
    pub fn forget(&mut self, id: u64) {
        self.reads.remove(&id);
        self.bound.retain(|_, v| *v != id);
    }

    /// 消えたセッションの分を捨てる (無制限に増やさない)。
    pub fn retain(&mut self, alive: &[u64]) {
        self.reads.retain(|k, _| alive.contains(k));
        let live: Vec<u64> = self.reads.keys().copied().collect();
        self.bound.retain(|_, v| live.contains(v));
    }
}

// ---------------------------------------------------------------------------
// 設置 / 解除
// ---------------------------------------------------------------------------

/// 1 エージェント分の設置計画。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookPlan {
    pub bin: &'static str,
    /// 書き換える設定ファイル。
    pub settings: PathBuf,
    /// 仕掛けるイベント名。
    pub events: Vec<&'static str>,
    /// 仕掛けるコマンド行。
    pub command: String,
}

/// 設置状態。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HookStatus {
    /// 1 件も入っていない
    Missing,
    /// 一部だけ入っている (別バージョンの残骸など)
    Partial,
    /// **設定ファイルには全部入っているが、ベンダー側で有効になっていない。**
    ///
    /// codex の未信頼フックのように、**書いてあるのに黙って飛ばされる**状態。
    /// [`HookStatus::Installed`] と分けているのは、ここを一緒にすると
    /// `lease::current_tier` が「強制」を名乗ってしまい、
    /// **止まらないのに止まると表示する**という最悪の嘘になるため。
    Inactive,
    /// 全イベントが入っていて、実際に発火する
    Installed,
}

impl HookStatus {
    /// UI に出す短い名前 (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            HookStatus::Missing => "未設置",
            HookStatus::Partial => "一部のみ",
            HookStatus::Inactive => "設置済み (未承認 — まだ止まりません)",
            HookStatus::Installed => "設置済み",
        }
    }
}

/// `root` (プロジェクト直下 or ホーム) に対する設置計画。
///
/// カタログにフック設定を持たないエージェントでは `None`。
pub fn plan_for(bin: &str, root: &Path, exe: &Path) -> Option<HookPlan> {
    let target = crate::agents::hook_target(bin)?;
    let events: Vec<&'static str> = target.events.iter().map(|(e, _, _)| *e).collect();
    Some(HookPlan {
        bin: target.bin,
        settings: root.join(target.settings_rel),
        events,
        command: format!("\"{}\" {HOOK_MARK} {}", exe.display(), target.bin),
    })
}

/// 設定ファイルを読む。無ければ空のオブジェクト。壊れていれば `Err`。
fn read_settings(path: &Path) -> Result<Map<String, Value>, String> {
    let raw = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Ok(Map::new()),
    };
    if raw.trim().is_empty() {
        return Ok(Map::new());
    }
    match serde_json::from_str::<Value>(&raw) {
        Ok(Value::Object(m)) => Ok(m),
        Ok(_) => Err("設定ファイルの中身が JSON オブジェクトではありません".into()),
        Err(e) => Err(format!("設定ファイルを読めません: {e}")),
    }
}

/// この計画で 1 イベントに入れるフック項目。
fn hook_entry(plan: &HookPlan, event: &str) -> Value {
    serde_json::json!({
        "matcher": "",
        "hooks": [ { "type": "command", "command": format!("{} {event}", plan.command) } ]
    })
}

/// 与えられたグループ配列が「自分の項目」を含むか。
fn group_is_ours(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hs| {
            hs.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|c| c.contains(HOOK_MARK))
            })
        })
}

/// いまの設置状態。
///
/// 全イベントが入っていても、ベンダー側の有効化が済んでいなければ
/// [`HookStatus::Inactive`] を返す ([`activation_gaps`] を参照)。
pub fn status(plan: &HookPlan) -> HookStatus {
    let Ok(root) = read_settings(&plan.settings) else {
        return HookStatus::Missing;
    };
    let hooks = root.get("hooks").and_then(Value::as_object);
    let mut have = 0usize;
    for ev in &plan.events {
        let ours = hooks
            .and_then(|h| h.get(*ev))
            .and_then(Value::as_array)
            .is_some_and(|arr| arr.iter().any(group_is_ours));
        if ours {
            have += 1;
        }
    }
    match have {
        0 => HookStatus::Missing,
        n if n == plan.events.len() => {
            // 追加の I/O は**全部入っているときだけ**払う。
            if activation_gaps(plan).is_empty() {
                HookStatus::Installed
            } else {
                HookStatus::Inactive
            }
        }
        _ => HookStatus::Partial,
    }
}

/// ユーザーのホーム。取れない環境でも落とさない (相対で扱う)。
fn home_dir() -> PathBuf {
    dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
}

/// このベンダーのホーム (`CODEX_HOME` 等の環境変数で上書きできる)。
fn vendor_home(a: &Activation) -> PathBuf {
    if !a.home_env.is_empty() {
        if let Some(v) = std::env::var_os(a.home_env) {
            if !v.is_empty() {
                return PathBuf::from(v);
            }
        }
    }
    // `/` 区切りの相対を OS ごとに解決する (区切りを直書きしない)。
    a.home_rel
        .split('/')
        .filter(|s| !s.is_empty())
        .fold(home_dir(), |p, s| p.join(s))
}

/// `CamelCase` → `snake_case` (`PreToolUse` → `pre_tool_use`)。
///
/// codex の信頼記録の節名がこの形。**機構側の変換**であって
/// ベンダー固有の値ではないので、ここに置いてよい。
fn snake_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for (i, c) in s.char_indices() {
        if c.is_ascii_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.push(c.to_ascii_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// `a.b.c` を辿って値を引く (TOML / JSON 共通)。
fn dotted<'a>(v: &'a toml::Value, path: &str) -> Option<&'a toml::Value> {
    path.split('.')
        .filter(|s| !s.is_empty())
        .try_fold(v, |cur, k| cur.get(k))
}

/// 2 つのパスを「同じ場所」として比べるための正規化。
///
/// macOS / Windows の既定のファイルシステムは**大文字小文字を区別しない**ので
/// 畳んで比べる (実際に `~/.gemini/trustedFolders.json` は `/Users/…` を
/// `/users/…` と小文字で持っていた)。Linux は区別するのでそのまま。
fn fold_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/').to_string();
    if cfg!(target_os = "macos") || cfg!(windows) {
        s.to_lowercase()
    } else {
        s
    }
}

/// **設置してあるのに効かない**理由を全部挙げる。空なら実際に発火する。
///
/// I/O をするので UI スレッドから毎フレーム呼ぶ相手ではない
/// ([`status`] は全イベントが揃っているときだけ呼ぶ)。
///
/// 読めない・見つからないファイルは**「足りない」側に倒す**。
/// ここを fail-open にすると、確かめられないまま「強制」と表示してしまう。
pub fn activation_gaps(plan: &HookPlan) -> Vec<ActivationGap> {
    activation_gaps_in(plan, &vendor_home)
}

/// [`activation_gaps`] の本体。**ホームの求め方を差し替えられる**ようにして
/// あるので、テストは実ユーザーの `~/.codex` / `~/.gemini` に触らずに済む。
fn activation_gaps_in(
    plan: &HookPlan,
    home_of: &dyn Fn(&Activation) -> PathBuf,
) -> Vec<ActivationGap> {
    let Some(target) = crate::agents::hook_target(plan.bin) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for a in target.activation {
        let file = a
            .file_rel
            .split('/')
            .filter(|s| !s.is_empty())
            // OS ごとの区切りは Path::join に任せる (直書きしない)。
            .fold(home_of(a), |p, s| p.join(s));
        let gap = |why: &str| ActivationGap {
            missing: why.to_string(),
            how: a.how.to_string(),
            file: file.clone(),
        };
        let raw = std::fs::read_to_string(&file).unwrap_or_default();
        match &a.kind {
            ActivationKind::HookTrustToml {
                feature,
                state_table,
                trusted_key,
                enabled_key,
            } => {
                let cfg: toml::Value = raw.parse().unwrap_or(toml::Value::Table(
                    // 読めない = まだ何も承認されていない、と同じ扱い。
                    toml::map::Map::new(),
                ));
                // 1. 機能フラグそのものが切られていないか。
                let feat_on = dotted(&cfg, &format!("{}.{}", feature.0, feature.1))
                    .and_then(toml::Value::as_bool)
                    // 既定は有効 (実機の `codex features list` で hooks は stable/true)。
                    .unwrap_or(true);
                if !feat_on {
                    out.push(gap(a.missing));
                    continue;
                }
                // 2. 自分が入れたフックが 1 件ずつ信頼されているか。
                //    節名は `<設定ファイルの絶対パス>:<snake イベント>:<群>:<番号>`。
                let abs =
                    std::fs::canonicalize(&plan.settings).unwrap_or_else(|_| plan.settings.clone());
                let state = dotted(&cfg, state_table);
                let idx = our_group_index(plan);
                let all_ok = plan.events.iter().all(|ev| {
                    let Some(i) = idx.get(*ev) else { return false };
                    // 正規化前後のどちらの綴りでも引けるようにする
                    // (macOS の /tmp → /private/tmp のような差を吸収する)。
                    [abs.as_path(), plan.settings.as_path()].iter().any(|p| {
                        let key = format!("{}:{}:{}:0", p.display(), snake_case(ev), i);
                        let Some(node) = state.and_then(|s| s.get(&key)) else {
                            return false;
                        };
                        let trusted = node.get(trusted_key).is_some();
                        let enabled = node
                            .get(enabled_key)
                            .and_then(toml::Value::as_bool)
                            .unwrap_or(true);
                        trusted && enabled
                    })
                });
                if !all_ok {
                    out.push(gap(a.missing));
                }
            }
            ActivationKind::TrustedFolderJson {
                trusted,
                trusted_parent,
            } => {
                let map: Map<String, Value> = serde_json::from_str(&raw).unwrap_or_default();
                // 設定ファイルの置き場所の親 = 作業ツリー。
                let tree = plan
                    .settings
                    .parent()
                    .and_then(Path::parent)
                    .unwrap_or(Path::new("."));
                let want = fold_path(tree);
                let ok = map.iter().any(|(k, v)| {
                    let Some(v) = v.as_str() else { return false };
                    let k = fold_path(Path::new(k));
                    if k == want {
                        trusted.contains(&v) || v == *trusted_parent
                    } else {
                        // 祖先が「配下も信頼」なら足りる。
                        v == *trusted_parent && want.starts_with(&format!("{k}/"))
                    }
                });
                if !ok {
                    out.push(gap(a.missing));
                }
            }
        }
    }
    out
}

/// イベント名 → **自分の項目が入っている群の番号**。
///
/// codex の信頼記録は群と番号で引くので、実際のファイルから数える
/// (`install` は自分の項目を末尾へ足すが、ユーザーが並べ替えた後でも当たるように)。
fn our_group_index(plan: &HookPlan) -> HashMap<&'static str, usize> {
    let mut out = HashMap::new();
    let Ok(root) = read_settings(&plan.settings) else {
        return out;
    };
    let hooks = root.get("hooks").and_then(Value::as_object);
    for ev in &plan.events {
        if let Some(arr) = hooks.and_then(|h| h.get(*ev)).and_then(Value::as_array) {
            if let Some(i) = arr.iter().position(group_is_ours) {
                out.insert(*ev, i);
            }
        }
    }
    out
}

/// 書き戻し (親ディレクトリを作り、初回だけバックアップを残す)。
fn write_settings(path: &Path, root: &Map<String, Value>, backup: bool) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("設定フォルダを作れません: {e}"))?;
    }
    if backup {
        let bak = backup_path(path);
        if !bak.exists() && path.exists() {
            std::fs::copy(path, &bak).map_err(|e| format!("バックアップを作れません: {e}"))?;
        }
    }
    let text = serde_json::to_string_pretty(&Value::Object(root.clone()))
        .map_err(|e| format!("JSON 化に失敗: {e}"))?;
    std::fs::write(path, format!("{text}\n")).map_err(|e| format!("設定を書けません: {e}"))
}

/// バックアップの置き場所 (`<settings>.zaivern.bak`)。
pub fn backup_path(settings: &Path) -> PathBuf {
    let mut s = settings.as_os_str().to_os_string();
    s.push(".zaivern.bak");
    PathBuf::from(s)
}

/// フックを設置する。**冪等** — 2 回撃っても増えない。
///
/// ユーザーの既存フックは 1 件も消さない (同じイベントに並べて足す)。
pub fn install(plan: &HookPlan) -> Result<(), String> {
    let mut root = read_settings(&plan.settings)?;
    let mut hooks = root
        .get("hooks")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for ev in &plan.events {
        let mut arr = hooks
            .get(*ev)
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        // 既に自分の項目が居るなら足さない (冪等)。中身は最新へ差し替える。
        arr.retain(|g| !group_is_ours(g));
        arr.push(hook_entry(plan, ev));
        hooks.insert((*ev).to_string(), Value::Array(arr));
    }
    root.insert("hooks".into(), Value::Object(hooks));
    write_settings(&plan.settings, &root, true)
}

// ---------------------------------------------------------------------------
// UI (同意はここでだけ取る)
// ---------------------------------------------------------------------------

/// フック設置の 1 行 UI。**押されたときにしか書き換えない**。
///
/// 直前に「何のファイルをどう書き換えるか」をホバーで全部見せる。
/// `root` はプロジェクト直下 (= ワークスペース)。戻り値は直近の結果メッセージ。
pub fn ui(ui: &mut egui::Ui, root: &Path, log: &mut String) {
    use crate::i18n::{tr, trf};
    let Ok(exe) = std::env::current_exe() else {
        ui.label(tr(
            "実行ファイルの場所が判らないため、フックを設置できません",
        ));
        return;
    };
    ui.label(
        egui::RichText::new(tr(
            "ベンダー提供フック — エージェントの状態を画面ではなく通知から知る",
        ))
        .strong(),
    );
    ui.label(
        egui::RichText::new(tr(
            "設定ファイルはボタンを押したときだけ書き換えます (初回は .zaivern.bak に控えを残します)",
        ))
        .weak(),
    );
    for target in crate::agents::HOOK_TARGETS {
        let Some(plan) = plan_for(target.bin, root, &exe) else {
            continue;
        };
        let st = status(&plan);
        ui.horizontal_wrapped(|ui| {
            ui.label(format!("{} — {}", plan.bin, tr(st.label())))
                .on_hover_text(trf(
                    "書き換え先: {path}\nイベント: {events}\nコマンド: {cmd}\n確認方法: {ver}",
                    &[
                        ("path", plan.settings.display().to_string()),
                        ("events", plan.events.join(", ")),
                        ("cmd", plan.command.clone()),
                        ("ver", target.verified.to_string()),
                    ],
                ));
            if st != HookStatus::Installed && ui.button(tr("設置")).clicked() {
                *log = match install(&plan) {
                    Ok(()) => trf(
                        "フックを設置しました: {path}",
                        &[("path", plan.settings.display().to_string())],
                    ),
                    Err(e) => e,
                };
            }
            if st != HookStatus::Missing && ui.button(tr("解除")).clicked() {
                *log = match uninstall(&plan) {
                    Ok(()) => trf(
                        "フックを解除しました: {path}",
                        &[("path", plan.settings.display().to_string())],
                    ),
                    Err(e) => e,
                };
            }
        });
        // **設置しただけでは効かない**ベンダーは、その場で何が足りないかを出す。
        // ここを黙っていると「設置済みと出ているのに止まらない」になる。
        if st == HookStatus::Inactive {
            for g in activation_gaps(&plan) {
                ui.label(
                    egui::RichText::new(format!("  ⚠ {} — {}", tr(&g.missing), tr(&g.how)))
                        .color(ui.visuals().warn_fg_color),
                )
                .on_hover_text(trf(
                    "確認したファイル: {path}",
                    &[("path", g.file.display().to_string())],
                ));
            }
        }
    }
    if !log.is_empty() {
        ui.label(egui::RichText::new(log.as_str()).weak());
    }
}

/// 自分が足した項目**だけ**を外す。ユーザーの項目には触らない。
pub fn uninstall(plan: &HookPlan) -> Result<(), String> {
    let mut root = read_settings(&plan.settings)?;
    let Some(mut hooks) = root.get("hooks").and_then(Value::as_object).cloned() else {
        return Ok(());
    };
    for ev in &plan.events {
        let Some(arr) = hooks.get(*ev).and_then(Value::as_array).cloned() else {
            continue;
        };
        let kept: Vec<Value> = arr.into_iter().filter(|g| !group_is_ours(g)).collect();
        if kept.is_empty() {
            hooks.remove(*ev);
        } else {
            hooks.insert((*ev).to_string(), Value::Array(kept));
        }
    }
    if hooks.is_empty() {
        root.remove("hooks");
    } else {
        root.insert("hooks".into(), Value::Object(hooks));
    }
    write_settings(&plan.settings, &root, false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    /// ユーザーが既に持っている設定 (実在の `~/.claude/settings.json` を縮めたもの)。
    /// **実ファイルには触れない** — 中身の形だけを写している。
    const USER_SETTINGS: &str = r#"{
      "model": "opus",
      "permissions": { "allow": ["Bash(git *)"] },
      "hooks": {
        "SessionStart": [
          { "matcher": "", "hooks": [ { "type": "command", "command": "node /u/my-hook.js SessionStart", "timeout": 5 } ] }
        ],
        "PreCompact": [
          { "matcher": "", "hooks": [ { "type": "command", "command": "node /u/my-hook.js PreCompact" } ] }
        ]
      }
    }"#;

    fn plan_in(dir: &Path) -> HookPlan {
        plan_for("claude", dir, Path::new("zai-test-exe")).expect("claude のフック対象が要る")
    }

    fn read_json(p: &Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(p).expect("設定を読めない")).expect("JSON")
    }

    #[test]
    fn フックの設置は冪等() {
        let dir = unique_temp_dir("zaivern", "hooks-idempotent");
        let plan = plan_in(&dir);
        assert_eq!(status(&plan), HookStatus::Missing);
        install(&plan).expect("1 回目");
        let once = read_json(&plan.settings);
        install(&plan).expect("2 回目");
        let twice = read_json(&plan.settings);
        assert_eq!(once, twice, "2 回設置しても増えない・壊れない");
        assert_eq!(status(&plan), HookStatus::Installed);
        // 仕掛けたイベントの数だけ、自分の項目がちょうど 1 件ずつ在る
        let hooks = twice["hooks"].as_object().expect("hooks");
        assert_eq!(hooks.len(), plan.events.len());
        for ev in &plan.events {
            let arr = hooks[*ev].as_array().expect("配列");
            assert_eq!(arr.iter().filter(|g| group_is_ours(g)).count(), 1);
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 設置してもユーザーの既存設定を消さない() {
        let dir = unique_temp_dir("zaivern", "hooks-keep-user");
        let plan = plan_in(&dir);
        std::fs::create_dir_all(plan.settings.parent().expect("親")).expect("mkdir");
        std::fs::write(&plan.settings, USER_SETTINGS).expect("write");
        let before: Value = serde_json::from_str(USER_SETTINGS).expect("JSON");
        install(&plan).expect("設置");
        let after = read_json(&plan.settings);
        // フック以外のキーはそのまま
        assert_eq!(after["model"], before["model"]);
        assert_eq!(after["permissions"], before["permissions"]);
        // ユーザーのフックは 1 件も消えていない
        let user_start = &before["hooks"]["SessionStart"][0];
        let now_start = after["hooks"]["SessionStart"].as_array().expect("配列");
        assert!(now_start.contains(user_start), "ユーザーのフックが消えた");
        assert_eq!(after["hooks"]["PreCompact"], before["hooks"]["PreCompact"]);
        // 初回はバックアップが残る
        assert!(backup_path(&plan.settings).exists(), "控えが無い");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 解除すると元の設定へ戻る() {
        let dir = unique_temp_dir("zaivern", "hooks-uninstall");
        let plan = plan_in(&dir);
        std::fs::create_dir_all(plan.settings.parent().expect("親")).expect("mkdir");
        std::fs::write(&plan.settings, USER_SETTINGS).expect("write");
        let before: Value = serde_json::from_str(USER_SETTINGS).expect("JSON");
        install(&plan).expect("設置");
        uninstall(&plan).expect("解除");
        assert_eq!(read_json(&plan.settings), before, "解除で元に戻らない");
        assert_eq!(status(&plan), HookStatus::Missing);
        // 2 回解除しても壊れない
        uninstall(&plan).expect("2 回目の解除");
        assert_eq!(read_json(&plan.settings), before);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 一部だけ残っていれば一部のみと出る() {
        let dir = unique_temp_dir("zaivern", "hooks-partial");
        let plan = plan_in(&dir);
        install(&plan).expect("設置");
        // 1 イベントぶんだけ手で消す (別バージョンの残骸を模す)
        let mut v = read_json(&plan.settings);
        let ev = plan.events[0];
        v["hooks"].as_object_mut().expect("hooks").remove(ev);
        std::fs::write(&plan.settings, v.to_string()).expect("write");
        assert_eq!(status(&plan), HookStatus::Partial);
        // 設置し直せば揃う (冪等な回復)
        install(&plan).expect("再設置");
        assert_eq!(status(&plan), HookStatus::Installed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 投函箱は読んだら空になる() {
        let dir = unique_temp_dir("zaivern", "hooks-inbox");
        let ev = event_from_payload(
            "claude",
            "PreToolUse",
            r#"{"session_id":"s1","cwd":"/w","tool_name":"Edit"}"#,
        );
        post(&dir, &ev).expect("投函");
        post(&dir, &ev).expect("投函");
        let got = drain(&dir, 0);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], ev);
        assert!(drain(&dir, 0).is_empty(), "読んだら消えていない");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 壊れたペイロードでも引数から判る分は失わない() {
        let ev = event_from_payload("claude", "Stop", "これは JSON ではない");
        assert_eq!(ev.agent, "claude");
        assert_eq!(ev.event, "Stop");
        assert!(ev.session.is_empty() && ev.cwd.is_empty());
    }

    #[test]
    fn フック通知は_cwd_とエージェントでセッションへ割り当てる() {
        let mut r = HookRouter::default();
        let dir = unique_temp_dir("zaivern", "hooks-route");
        let sessions = vec![
            HookTargetSession {
                id: 7,
                bin: "claude".into(),
                cwd: dir.clone(),
            },
            HookTargetSession {
                id: 9,
                bin: "codex".into(),
                cwd: dir.clone(),
            },
        ];
        let mk = |event: &str, tool: &str| HookEvent {
            agent: "claude".into(),
            session: "s1".into(),
            cwd: dir.to_string_lossy().to_string(),
            event: event.into(),
            tool: tool.into(),
        };
        r.route(&[mk("PreToolUse", "Edit")], &sessions, 0);
        // 同じ cwd に別エージェントが居ても取り違えない
        assert!(r.read(9, 0, 1_000).is_none());
        let got = r.read(7, 0, 1_000).expect("割り当てられていない");
        assert_eq!(got.state, ProtoState::Editing, "ツール名で細分される");
        // ツールを使い終わったイベントはツール名で細分しない
        r.route(&[mk("PostToolUse", "Edit")], &sessions, 10);
        assert_eq!(
            r.read(7, 10, 1_000).expect("読める").state,
            ProtoState::Thinking
        );
        // 沈黙したら降りる
        assert!(r.read(7, 2_000, 1_000).is_none());
        // 消えたセッションは忘れる
        r.forget(7);
        assert!(r.read(7, 10, 1_000).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 全ベンダーで「設置 → 状態 → 撤去」が往復すること。
    ///
    /// claude 決め打ちの往復テストしか無かったので、**書式が違うベンダーを
    /// 足したときに壊れても気付けなかった**。カタログを回して塞ぐ。
    #[test]
    fn どのベンダーでも設置と撤去が往復する() {
        for t in crate::agents::HOOK_TARGETS {
            let dir = unique_temp_dir("zaivern", &format!("hooks-roundtrip-{}", t.bin));
            let plan = plan_for(t.bin, &dir, Path::new("zai-test-exe")).expect("計画を作れない");
            assert_eq!(status(&plan), HookStatus::Missing, "{}", t.bin);
            install(&plan).expect("設置");
            // 設定ファイルには全部入っている (有効化まで済んでいるかは別)。
            let st = status(&plan);
            assert!(
                st == HookStatus::Installed || st == HookStatus::Inactive,
                "{}: 設置したのに {st:?}",
                t.bin
            );
            // **有効化が要るベンダーを「設置済み」と言わない。**
            // 一時ディレクトリはどのベンダーの信頼表にも載っていないので、
            // 有効化条件を持つカタログは必ず Inactive になる。
            assert_eq!(
                st == HookStatus::Inactive,
                !t.activation.is_empty(),
                "{}: 有効化の段が状態に出ていない",
                t.bin
            );
            // 冪等
            install(&plan).expect("2 回目");
            assert_eq!(status(&plan), st, "{}: 2 回目で変わった", t.bin);
            uninstall(&plan).expect("解除");
            assert_eq!(status(&plan), HookStatus::Missing, "{}", t.bin);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    /// 有効化の判定。**実ユーザーの `~/.codex` / `~/.gemini` には触らない**
    /// (ホームの求め方を差し替えられるようにしてある)。
    #[test]
    fn 設置済みでもベンダーが承認していなければ止まらないと判る() {
        let dir = unique_temp_dir("zaivern", "hooks-activation");
        let home = dir.join("vendor-home");
        std::fs::create_dir_all(&home).expect("mkdir");
        let home_of = |_: &Activation| home.clone();

        // ── codex: config.toml の hooks.state に信頼が要る ────────────
        let tree = dir.join("proj");
        let plan = plan_for("codex", &tree, Path::new("zai-test-exe")).expect("計画");
        install(&plan).expect("設置");
        let abs = std::fs::canonicalize(&plan.settings).unwrap_or_else(|_| plan.settings.clone());
        // 自分の項目は 1 件だけなので群は 0 番。
        let trusted: String = plan
            .events
            .iter()
            .map(|ev| {
                format!(
                    "[hooks.state.\"{}:{}:0:0\"]\ntrusted_hash = \"sha256:x\"\n",
                    abs.display(),
                    snake_case(ev)
                )
            })
            .collect();
        let cfg = home.join("config.toml");

        // (設定の中身, 有効か, 何を確かめているか)
        let cases: Vec<(String, bool, &str)> = vec![
            (String::new(), false, "設定が無ければ未承認"),
            (trusted.clone(), true, "全イベントが信頼されていれば有効"),
            (
                format!("[features]\nhooks = false\n\n{trusted}"),
                false,
                "機能フラグが切られていたら止まる",
            ),
            (
                format!("[features]\nhooks = true\n\n{trusted}"),
                true,
                "機能フラグが明示的に有効なら通る",
            ),
            (
                trusted.replace("trusted_hash", "何か別のキー"),
                false,
                "trusted_hash が無ければ未承認",
            ),
            (
                // **他人のフックだけ**が信頼されている状態。
                "[hooks.state.\"/どこか/hooks.json:pre_tool_use:0:0\"]\ntrusted_hash = \"sha256:y\"\n".into(),
                false,
                "別のフックが信頼されていても自分の分にはならない",
            ),
            (
                // 信頼はされているが、明示的に無効化されている
                // (実際にこの環境の pre_tool_use がこの形だった)。
                trusted.replace(
                    "trusted_hash = \"sha256:x\"",
                    "enabled = false\ntrusted_hash = \"sha256:x\"",
                ),
                false,
                "enabled = false なら信頼済みでも止まる",
            ),
        ];
        for (body, want_ok, why) in &cases {
            std::fs::write(&cfg, body).expect("write");
            let gaps = activation_gaps_in(&plan, &home_of);
            assert_eq!(gaps.is_empty(), *want_ok, "codex: {why}");
            if !want_ok {
                let g = &gaps[0];
                assert!(!g.missing.is_empty() && !g.how.is_empty(), "説明が空");
                assert_eq!(g.file, cfg, "確認先が違う");
            }
        }
        // 1 イベントでも欠けたら有効と言わない (部分的な信頼で騙されない)
        let one_missing: String = trusted
            .lines()
            .take(trusted.lines().count() - 2)
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(&cfg, &one_missing).expect("write");
        assert!(
            !activation_gaps_in(&plan, &home_of).is_empty(),
            "codex: 1 件欠けているのに有効と言った"
        );

        // ── gemini: 信頼フォルダ表 ────────────────────────────────
        let gtree = dir.join("gproj");
        let gplan = plan_for("gemini", &gtree, Path::new("zai-test-exe")).expect("計画");
        install(&gplan).expect("設置");
        let tf = home.join("trustedFolders.json");
        let parent = dir.to_string_lossy().to_string();
        let self_dir = gtree.to_string_lossy().to_string();
        let gcases: Vec<(String, bool, &str)> = vec![
            ("{}".into(), false, "表が空なら未信頼"),
            (
                format!("{{\"{self_dir}\":\"TRUST_FOLDER\"}}"),
                true,
                "そのフォルダ自身が信頼されていれば有効",
            ),
            (
                format!("{{\"{self_dir}\":\"DO_NOT_TRUST\"}}"),
                false,
                "明示的に拒否されていれば止まる",
            ),
            (
                format!("{{\"{parent}\":\"TRUST_PARENT\"}}"),
                true,
                "祖先が配下ごと信頼していれば有効",
            ),
            (
                format!("{{\"{parent}\":\"TRUST_FOLDER\"}}"),
                false,
                "祖先が自分だけ信頼でも配下には及ばない",
            ),
            (
                format!("{{\"{}\":\"TRUST_FOLDER\"}}", self_dir.to_uppercase()),
                cfg!(target_os = "macos") || cfg!(windows),
                "大文字小文字を区別しない OS でだけ畳んで一致する",
            ),
        ];
        for (body, want_ok, why) in &gcases {
            std::fs::write(&tf, body).expect("write");
            assert_eq!(
                activation_gaps_in(&gplan, &home_of).is_empty(),
                *want_ok,
                "gemini: {why}"
            );
        }

        // 有効化条件を持たない claude は、この段でつまずかない。
        let cplan =
            plan_for("claude", &dir.join("cproj"), Path::new("zai-test-exe")).expect("計画");
        install(&cplan).expect("設置");
        assert!(activation_gaps_in(&cplan, &home_of).is_empty());
        assert_eq!(status(&cplan), HookStatus::Installed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn イベント名をスネークケースへ畳める() {
        // codex の信頼記録の節名がこの形 (実在の config.toml で確認)
        for (input, want) in [
            ("PreToolUse", "pre_tool_use"),
            ("PostToolUse", "post_tool_use"),
            ("SessionStart", "session_start"),
            ("UserPromptSubmit", "user_prompt_submit"),
            ("Stop", "stop"),
            ("BeforeTool", "before_tool"),
        ] {
            assert_eq!(snake_case(input), want);
        }
    }

    #[test]
    fn カタログに無いフックイベントは捨てる() {
        let mut r = HookRouter::default();
        let dir = unique_temp_dir("zaivern", "hooks-unknown-event");
        let sessions = vec![HookTargetSession {
            id: 1,
            bin: "claude".into(),
            cwd: dir.clone(),
        }];
        r.route(
            &[HookEvent {
                agent: "claude".into(),
                session: "s".into(),
                cwd: dir.to_string_lossy().to_string(),
                event: "Elicitation".into(),
                tool: String::new(),
            }],
            &sessions,
            0,
        );
        assert!(
            r.read(1, 0, 1_000).is_none(),
            "意味が確定していないイベントで状態を作らない"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
