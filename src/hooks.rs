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
    /// ツール名 → 状態の細分表。
    pub tools: &'static [(&'static str, ProtoState)],
    /// **実機で確認した方法**。空は禁止 (カタログ整合テストが落とす)。
    pub verified: &'static str,
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
    /// 全イベントが入っている
    Installed,
}

impl HookStatus {
    /// UI に出す短い名前 (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            HookStatus::Missing => "未設置",
            HookStatus::Partial => "一部のみ",
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
        n if n == plan.events.len() => HookStatus::Installed,
        _ => HookStatus::Partial,
    }
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
