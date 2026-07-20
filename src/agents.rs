use std::path::{Path, PathBuf};

use eframe::egui;

use crate::config::AgentPreset;
use crate::terminal::{Session, SpawnSpec};

pub enum SessionEvent {
    /// (title) — セッションがユーザーの承認待ちになった
    NeedsApproval(String),
    /// (title, 説明) — 全自動YESモードが承認プロンプトへ自動応答した
    AutoApproved(String, &'static str),
    /// (title, exit code) — セッションが終了した
    Exited(String, u32),
}

/// 既定の承認モード (config.approval_mode に対応)。
/// Agent = Agent欄(プリセット)優先: コマンドに書かれたフラグをそのまま使う。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Approval {
    Ask,
    Auto,
    Agent,
}

impl Approval {
    pub fn from_mode(mode: &str) -> Self {
        match mode {
            "auto" => Approval::Auto,
            "agent" => Approval::Agent,
            _ => Approval::Ask,
        }
    }
}

/// 承認モードを自動適用できる CLI: (プログラム名, 全自動フラグ, ask時に除去する単独フラグ群)。
/// claude の `--permission-mode bypassPermissions` だけは 2 トークン形式のため別処理。
const KNOWN_AGENTS: &[(&str, &str, &[&str])] = &[
    (
        "claude",
        "--dangerously-skip-permissions",
        &["--dangerously-skip-permissions"],
    ),
    (
        "codex",
        "--dangerously-bypass-approvals-and-sandbox",
        &[
            "--dangerously-bypass-approvals-and-sandbox",
            "--yolo",
            "--full-auto",
        ],
    ),
    // Antigravity CLI (Google)。全自動フラグは claude と同名。
    (
        "agy",
        "--dangerously-skip-permissions",
        &["--dangerously-skip-permissions", "--yolo"],
    ),
];

/// コマンドの先頭トークンが承認モード対応 CLI なら (auto フラグ, 除去対象) を返す。
fn known_agent(command: &str) -> Option<(&'static str, &'static [&'static str])> {
    let head = command.split_whitespace().next()?;
    KNOWN_AGENTS
        .iter()
        .find(|(name, _, _)| *name == head)
        .map(|(_, auto_flag, strip)| (*auto_flag, *strip))
}

/// コマンド文字列が bypass 権限フラグを含むか(表示用の判定)。
pub fn command_is_bypass(command: &str) -> bool {
    let lc = command.to_lowercase();
    lc.contains("--dangerously-skip-permissions")
        || lc.contains("--dangerously-bypass-approvals-and-sandbox")
        || lc.contains("--yolo")
        || lc.contains("bypasspermissions")
        || lc.contains("--permission-mode bypass")
}

/// 対応 CLI (claude / codex / agy) のコマンドに承認モードを適用する。
/// Auto = 全自動YES (CLI ごとの bypass フラグを付与)、
/// Ask = 毎回ユーザー承認 (bypass 系フラグを全て除去し CLI 標準の確認に任せる)、
/// Agent = Agent欄優先 (プリセットのコマンドを一切書き換えない)。
///
/// Ask のときは、プリセットのコマンドに bypass フラグが直書きされていても
/// 確実に取り除く(これがユーザー報告「全自動じゃなくても bypass になる」対策)。
pub fn apply_approval(command: &str, approval: Approval) -> String {
    if approval == Approval::Agent {
        return command.to_string();
    }
    let Some((auto_flag, strip_flags)) = known_agent(command) else {
        return command.to_string();
    };
    // bypass 系フラグを一旦すべて除去する。claude は
    // `--permission-mode bypassPermissions`(スペース区切り)も、
    // `--permission-mode=bypassPermissions`(= 区切り)も両方消す。
    let tokens: Vec<&str> = command.split_whitespace().collect();
    let mut parts: Vec<&str> = Vec::with_capacity(tokens.len());
    let mut i = 0;
    while i < tokens.len() {
        let tok = tokens[i];
        if strip_flags.contains(&tok) {
            i += 1;
            continue;
        }
        // `--permission-mode bypassPermissions`(スペース区切り2トークン)を除去
        if tok == "--permission-mode"
            && tokens
                .get(i + 1)
                .map(|v| v.eq_ignore_ascii_case("bypassPermissions"))
                .unwrap_or(false)
        {
            i += 2;
            continue;
        }
        // `--permission-mode=bypassPermissions`(= 区切り1トークン)を除去
        if let Some(v) = tok.strip_prefix("--permission-mode=") {
            if v.eq_ignore_ascii_case("bypassPermissions") {
                i += 1;
                continue;
            }
        }
        parts.push(tok);
        i += 1;
    }
    if approval == Approval::Auto {
        parts.push(auto_flag);
    }
    parts.join(" ")
}

pub struct AgentManager {
    pub sessions: Vec<Session>,
    pub active: usize,
    pub panel_open: bool,
    next_id: u64,
}

fn expand_home(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

impl AgentManager {
    pub fn new() -> Self {
        Self {
            sessions: Vec::new(),
            active: 0,
            panel_open: false,
            next_id: 1,
        }
    }

    pub fn launch(
        &mut self,
        preset: &AgentPreset,
        workspace: &Path,
        approval: Approval,
        ctx: &egui::Context,
    ) -> Result<(), String> {
        let same = self
            .sessions
            .iter()
            .filter(|s| s.preset_name == preset.name)
            .count();
        let title = if same > 0 {
            format!("{} #{}", preset.name, same + 1)
        } else {
            preset.name.clone()
        };
        let cwd = preset
            .cwd
            .as_deref()
            .map(expand_home)
            .filter(|p| p.is_dir())
            .unwrap_or_else(|| workspace.to_path_buf());

        let id = self.next_id;
        self.next_id += 1;
        let session = Session::spawn(
            id,
            SpawnSpec {
                title,
                preset_name: preset.name.clone(),
                icon: preset.icon.clone(),
                command: apply_approval(&preset.command, approval),
                cwd,
                env: preset.env.clone(),
            },
            ctx.clone(),
        )?;
        self.sessions.push(session);
        self.active = self.sessions.len() - 1;
        self.panel_open = true;
        Ok(())
    }

    pub fn restart(&mut self, i: usize, ctx: &egui::Context) -> Result<(), String> {
        let Some(old) = self.sessions.get_mut(i) else {
            return Ok(());
        };
        old.kill();
        let id = self.next_id;
        self.next_id += 1;
        let session = Session::spawn(
            id,
            SpawnSpec {
                title: old.title.clone(),
                preset_name: old.preset_name.clone(),
                icon: old.icon.clone(),
                command: old.command.clone(),
                cwd: old.cwd.clone(),
                env: old.env.clone(),
            },
            ctx.clone(),
        )?;
        self.sessions[i] = session;
        self.active = i;
        Ok(())
    }

    pub fn remove(&mut self, i: usize) {
        if i >= self.sessions.len() {
            return;
        }
        self.sessions.remove(i);
        if self.active >= self.sessions.len() && !self.sessions.is_empty() {
            self.active = self.sessions.len() - 1;
        }
    }

    pub fn active_session(&mut self) -> Option<&mut Session> {
        self.sessions.get_mut(self.active)
    }

    pub fn running_count(&self) -> usize {
        self.sessions.iter().filter(|s| s.running()).count()
    }

    /// 各セッションの状態変化(承認待ち・自動承認・終了)を検知して返す。毎フレーム呼んで良い。
    /// 全自動YESで起動した対応 CLI は承認プロンプトへ自動応答する。
    pub fn poll_events(&mut self) -> Vec<SessionEvent> {
        use crate::terminal::Attention;
        let mut events = Vec::new();
        for s in self.sessions.iter_mut() {
            if s.running() {
                match s.scan_attention(s.auto_yes()) {
                    Some(Attention::NeedsApproval) => {
                        events.push(SessionEvent::NeedsApproval(s.title.clone()));
                    }
                    Some(Attention::AutoReplied(desc)) => {
                        events.push(SessionEvent::AutoApproved(s.title.clone(), desc));
                    }
                    None => {}
                }
            } else if !s.notified_exit {
                s.notified_exit = true;
                s.attention = false;
                let code = s.exit_code.lock().unwrap().unwrap_or(0);
                events.push(SessionEvent::Exited(s.title.clone(), code));
            }
        }
        events
    }

    /// Send text to every running session (cockpit broadcast).
    pub fn broadcast(&mut self, text: &str) {
        let payload = format!("{text}\r");
        for s in &mut self.sessions {
            if s.running() {
                s.write_bytes(payload.as_bytes());
            }
        }
    }

    /// 指定セッションの権限モード切替 UI を開く/切り替える。
    /// Claude/Antigravity は Shift+Tab、Codex は `/permissions` を送る。
    pub fn cycle_permission(&mut self, i: usize) -> Option<&'static str> {
        let s = self.sessions.get_mut(i)?;
        if !s.running() {
            return None;
        }
        let keys = s.permission_switch_keys()?;
        let hint = s.permission_switch_hint()?;
        s.write_bytes(keys);
        Some(hint)
    }

    /// 実行中の対応 CLI セッションへ、それぞれの権限モード切替入力を送る。送った件数を返す。
    pub fn cycle_permission_all(&mut self) -> usize {
        let mut n = 0;
        for s in &mut self.sessions {
            if !s.running() {
                continue;
            }
            if let Some(keys) = s.permission_switch_keys() {
                s.write_bytes(keys);
                n += 1;
            }
        }
        n
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_approval, Approval};

    #[test]
    fn claude_auto_appends_bypass() {
        assert_eq!(
            apply_approval("claude", Approval::Auto),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn claude_ask_strips_dangerous_flag() {
        assert_eq!(
            apply_approval("claude --dangerously-skip-permissions", Approval::Ask),
            "claude"
        );
    }

    #[test]
    fn claude_ask_strips_permission_mode_space() {
        assert_eq!(
            apply_approval("claude --permission-mode bypassPermissions", Approval::Ask),
            "claude"
        );
    }

    #[test]
    fn claude_ask_strips_permission_mode_equals() {
        assert_eq!(
            apply_approval(
                "claude --permission-mode=bypassPermissions --model x",
                Approval::Ask
            ),
            "claude --model x"
        );
    }

    #[test]
    fn non_known_command_untouched() {
        assert_eq!(
            apply_approval("gemini --dangerously-skip-permissions", Approval::Auto),
            "gemini --dangerously-skip-permissions"
        );
    }

    #[test]
    fn ask_does_not_double_add() {
        // ask では付与しない
        assert_eq!(apply_approval("claude", Approval::Ask), "claude");
        // auto でも二重に付与しない
        assert_eq!(
            apply_approval("claude --dangerously-skip-permissions", Approval::Auto),
            "claude --dangerously-skip-permissions"
        );
    }

    #[test]
    fn ask_keeps_non_bypass_permission_mode() {
        // plan など bypass 以外の権限モードは残す
        assert_eq!(
            apply_approval("claude --permission-mode plan", Approval::Ask),
            "claude --permission-mode plan"
        );
    }

    #[test]
    fn agent_mode_keeps_preset_command_verbatim() {
        // Agent欄優先: 既定が何であれプリセットのコマンドを書き換えない
        assert_eq!(
            apply_approval("claude --dangerously-skip-permissions", Approval::Agent),
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(apply_approval("claude", Approval::Agent), "claude");
        assert_eq!(
            apply_approval("codex --dangerously-bypass-approvals-and-sandbox", Approval::Agent),
            "codex --dangerously-bypass-approvals-and-sandbox"
        );
    }

    #[test]
    fn codex_auto_appends_bypass() {
        assert_eq!(
            apply_approval("codex", Approval::Auto),
            "codex --dangerously-bypass-approvals-and-sandbox"
        );
    }

    #[test]
    fn codex_ask_strips_auto_flags() {
        assert_eq!(
            apply_approval("codex --dangerously-bypass-approvals-and-sandbox", Approval::Ask),
            "codex"
        );
        assert_eq!(apply_approval("codex --yolo", Approval::Ask), "codex");
        assert_eq!(apply_approval("codex --full-auto", Approval::Ask), "codex");
    }

    #[test]
    fn agy_auto_and_ask() {
        assert_eq!(
            apply_approval("agy", Approval::Auto),
            "agy --dangerously-skip-permissions"
        );
        assert_eq!(
            apply_approval("agy --dangerously-skip-permissions", Approval::Ask),
            "agy"
        );
    }
}
