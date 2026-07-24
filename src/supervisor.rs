//! スーパーバイザーエージェント — 走っている全 CLI エージェントセッションを見張り、
//! 「途中で静かにおかしくなる」のを防ぐメタエージェント層。
//!
//! 設計方針:
//! - 監視ロジックは純関数 (`detect_*`) に切り出し、単体テストで偽陽性まで検証する。
//! - 介入は必ず「段階的なはしご」を昇る。破壊的な操作は既定で提案(要確認)止まり。
//! - LLM への相談は完全に任意。既定 OFF で、無くても監視は成立する
//!   (見張り役自身が不安定であってはならない)。
//! - メモリは全て上限付き。長時間走っても増え続けない。
//! - UI スレッドをブロックしない。1 秒に 1 回だけ画面文字列をハッシュする程度。

#![allow(dead_code)]

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::time::Instant;

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::agents::Approval;

// ---------------------------------------------------------------------------
// 設定
// ---------------------------------------------------------------------------

/// スーパーバイザーの設定。既定値は全て保守的 (誤爆より見逃しを選ぶ)。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct SupervisorConfig {
    /// 監視そのものの ON/OFF。
    pub enabled: bool,
    /// 画面をサンプリングする間隔 (ms)。毎フレームは走らせない。
    pub sample_interval_ms: u64,
    /// セッションごとに保持するサンプル数の上限 (リングバッファ)。
    pub sample_capacity: usize,
    /// セッションごとに保持する状態遷移履歴の上限。
    pub history_capacity: usize,

    // --- 検出器の ON/OFF ---
    pub detect_stall: bool,
    pub detect_loop: bool,
    pub detect_error_storm: bool,
    pub detect_crash: bool,
    pub detect_silent_wait: bool,
    pub detect_runaway: bool,

    // --- 停滞 (Stall) ---
    /// 意味的な進捗が無いまま何秒続いたら停滞とみなすか。
    pub stall_secs: u64,
    /// スピナー等でカウンタだけ動いている場合の猶予倍率。
    /// 「動いてはいるが中身が進んでいない」状態を即断しないための係数。
    /// ただし無制限には待たない (最終的には stall_secs * この倍率で発火する)。
    pub spinner_grace_factor: u64,

    // --- ループ / 振動 ---
    /// 同一ブロックの再出現を数える窓 (秒)。
    pub loop_window_secs: u64,
    /// 窓内で同じブロックが何回再出現したらループとみなすか。
    pub loop_repeats: usize,
    /// ブロックハッシュに使う末尾行数。
    pub loop_block_lines: usize,

    // --- エラー嵐 ---
    pub error_window_secs: u64,
    /// 窓内の最低エラー行数 (これ未満なら率を見ない)。
    pub error_min_count: u32,
    /// 1 秒あたりの新規エラー行数のしきい値。
    pub error_rate_per_sec: f32,
    /// エラー判定パターン (小文字化した正規化行に対する部分一致)。多言語対応のためデータ駆動。
    pub error_patterns: Vec<String>,
    /// エラー判定から除外するパターン ("0 errors" 等の誤爆防止)。
    pub error_exclude_patterns: Vec<String>,

    // --- 沈黙した承認待ち ---
    /// 承認待ちがこの秒数を超えて放置されたら通知する。
    pub silent_wait_secs: u64,

    // --- 暴走出力 ---
    pub runaway_sustain_secs: u64,
    /// 平常時のベースラインに対する何倍を暴走とみなすか。
    pub runaway_factor: f32,
    /// 絶対値の下限 (B/s)。これを下回るなら倍率が出ても暴走扱いしない。
    pub runaway_floor_bps: f32,

    // --- 介入のはしご ---
    /// 異常が継続してから通知するまでの秒数。
    pub notify_after_secs: u64,
    /// 異常が継続してから通知の上の段へ昇るまでの秒数。
    pub escalate_after_secs: u64,
    /// Nudge (短い文面を送って詰まりを解く) を許すか。
    pub allow_nudge: bool,
    /// Nudge の文面。
    pub nudge_text: String,
    /// 再起動を自動で撃ってよいか。**既定 false**。
    /// true にしても Approval::Ask のときは決して自動発火しない。
    pub allow_auto_restart: bool,
    /// 停止を自動で撃ってよいか。**既定 false**。
    pub allow_auto_halt: bool,

    // --- レート制限 ---
    /// 同種の介入を再度撃つまでの最短間隔 (秒)。
    pub cooldown_notify_secs: u64,
    pub cooldown_auto_answer_secs: u64,
    pub cooldown_nudge_secs: u64,
    pub cooldown_restart_secs: u64,
    /// 1 セッションあたり 1 時間に許す介入の総数。
    pub max_interventions_per_hour: usize,

    // --- LLM 相談 (既定 OFF) ---
    /// ルールベース検出が発火したとき LLM に診断を仰ぐか。**既定 false**。
    pub llm_escalation: bool,
    /// LLM に渡す出力抜粋の最大文字数。
    pub llm_excerpt_chars: usize,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            sample_interval_ms: 1000,
            sample_capacity: 240, // 1秒間隔で約4分ぶん
            history_capacity: 20,

            detect_stall: true,
            detect_loop: true,
            detect_error_storm: true,
            detect_crash: true,
            detect_silent_wait: true,
            detect_runaway: true,

            stall_secs: 180,
            spinner_grace_factor: 2,

            loop_window_secs: 300,
            loop_repeats: 3,
            loop_block_lines: 4,

            error_window_secs: 60,
            error_min_count: 12,
            error_rate_per_sec: 0.5,
            error_patterns: default_error_patterns(),
            error_exclude_patterns: default_error_excludes(),

            silent_wait_secs: 120,

            runaway_sustain_secs: 15,
            runaway_factor: 8.0,
            runaway_floor_bps: 40_000.0,

            notify_after_secs: 30,
            escalate_after_secs: 180,
            allow_nudge: true,
            nudge_text: "作業が止まっているようです。今の状況を1行で報告し、続行できるなら続けてください。"
                .into(),
            allow_auto_restart: false,
            allow_auto_halt: false,

            cooldown_notify_secs: 120,
            cooldown_auto_answer_secs: 15,
            cooldown_nudge_secs: 300,
            cooldown_restart_secs: 900,
            max_interventions_per_hour: 12,

            llm_escalation: false,
            llm_excerpt_chars: 2000,
        }
    }
}

/// 既定のエラー検出パターン。英語だけに閉じないようデータ駆動で持つ。
pub fn default_error_patterns() -> Vec<String> {
    [
        "error",
        "fatal",
        "panic",
        "traceback",
        "exception",
        "failed",
        "failure",
        "no such file",
        "permission denied",
        "command not found",
        "connection refused",
        "timed out",
        "エラー",
        "失敗",
        "例外",
        "致命的",
        "権限がありません",
        "見つかりません",
        "错误",
        "失败",
        "오류",
        "실패",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

/// エラー扱いしない行 ("0 errors" 等の集計行)。
/// 正規化後は数字が `#` に潰れるため `# error` の形で書く。
pub fn default_error_excludes() -> Vec<String> {
    [
        "# error",
        "no error",
        "エラーなし",
        "# failure",
        "# failed",
        "# 件のエラー",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

// ---------------------------------------------------------------------------
// 状態機械
// ---------------------------------------------------------------------------

/// セッションの状態。PTY 出力とプロセス状態から導出する。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum SessionState {
    /// 進捗が出ている
    Working,
    /// 承認プロンプトで止まっている
    WaitingApproval,
    /// 生きているが特に動いていない (プロンプト待ち等)
    Idle,
    /// 動いているはずなのに進んでいない
    Stalled,
    /// 同じことを繰り返している
    Looping,
    /// エラーが噴き出している
    Errored,
    /// 作業中に落ちた
    Crashed,
    /// 正常終了
    Done,
}

impl SessionState {
    pub fn label(self) -> &'static str {
        match self {
            SessionState::Working => "作業中",
            SessionState::WaitingApproval => "承認待ち",
            SessionState::Idle => "待機",
            SessionState::Stalled => "停滞",
            SessionState::Looping => "ループ",
            SessionState::Errored => "エラー多発",
            SessionState::Crashed => "異常終了",
            SessionState::Done => "完了",
        }
    }

    /// 注意を要する状態か。
    pub fn is_trouble(self) -> bool {
        matches!(
            self,
            SessionState::Stalled
                | SessionState::Looping
                | SessionState::Errored
                | SessionState::Crashed
        )
    }
}

/// 状態遷移 1 件。履歴は上限付きで保持する。
#[derive(Clone, Debug)]
pub struct StateTransition {
    pub at_ms: u64,
    pub from: SessionState,
    pub to: SessionState,
    pub reason: String,
}

// ---------------------------------------------------------------------------
// 異常
// ---------------------------------------------------------------------------

/// 検出できる異常の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Anomaly {
    Stall,
    Looping,
    ErrorStorm,
    Crash,
    SilentWait,
    Runaway,
}

impl Anomaly {
    pub fn label(self) -> &'static str {
        match self {
            Anomaly::Stall => "停滞",
            Anomaly::Looping => "同じ処理の繰り返し",
            Anomaly::ErrorStorm => "エラー多発",
            Anomaly::Crash => "異常終了",
            Anomaly::SilentWait => "承認待ちの放置",
            Anomaly::Runaway => "出力の暴走",
        }
    }

    /// 緊急か (即座に通知し、段階を飛ばして提案まで出す)。
    fn urgent(self) -> bool {
        matches!(self, Anomaly::Crash | Anomaly::Runaway)
    }

    /// この異常に対して最終的に狙う介入。
    fn desired_action(self) -> Intervention {
        match self {
            // 承認待ちの放置は「Auto のときだけ」自動応答する。Ask では gate が拒否する。
            Anomaly::SilentWait => Intervention::AutoAnswer,
            Anomaly::Stall => Intervention::Nudge,
            Anomaly::Crash => Intervention::Restart,
            Anomaly::Looping | Anomaly::ErrorStorm | Anomaly::Runaway => Intervention::Halt,
        }
    }

    /// 対応する状態 (状態機械への反映用)。
    fn state(self) -> Option<SessionState> {
        match self {
            Anomaly::Stall => Some(SessionState::Stalled),
            Anomaly::Looping => Some(SessionState::Looping),
            Anomaly::ErrorStorm => Some(SessionState::Errored),
            Anomaly::Crash => Some(SessionState::Crashed),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// 介入のはしご
// ---------------------------------------------------------------------------

/// 介入。厳密に重い順へ昇る。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Intervention {
    /// 記録するだけ
    Observe,
    /// トースト + (非フォーカス時) OS 通知
    Notify,
    /// 承認プロンプトに自動応答する
    AutoAnswer,
    /// 短い文面を送って詰まりを解く
    Nudge,
    /// 停止して再起動する (**破壊的**: 作業中の内容を失う可能性がある)
    Restart,
    /// 停止してユーザーへエスカレーションする
    Halt,
}

impl Intervention {
    /// 重さ。0 が最も軽い。
    pub fn severity(self) -> u8 {
        match self {
            Intervention::Observe => 0,
            Intervention::Notify => 1,
            Intervention::AutoAnswer => 2,
            Intervention::Nudge => 3,
            Intervention::Restart => 4,
            Intervention::Halt => 5,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Intervention::Observe => "記録",
            Intervention::Notify => "通知",
            Intervention::AutoAnswer => "自動応答",
            Intervention::Nudge => "促し",
            Intervention::Restart => "再起動",
            Intervention::Halt => "停止",
        }
    }

    /// 破壊的か (作業中の内容を失う可能性がある)。
    pub fn destructive(self) -> bool {
        matches!(self, Intervention::Restart | Intervention::Halt)
    }
}

/// ゲート判定の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GateResult {
    /// そのまま実行してよい
    Allow,
    /// ユーザー確認が要る (UI が確認ダイアログにする)
    NeedConfirm(&'static str),
    /// 実行してはいけない
    Refuse(&'static str),
}

/// 介入を許すかどうかの判定。**安全性の中核**なので純関数にして必ずテストする。
///
/// 絶対規則:
/// - `Approval::Ask` のとき、`Notify` より上は無確認では絶対に通さない。
/// - `AutoAnswer` は `Approval::Auto` のときだけ。`Ask` では拒否 (承認の意味が消えるため)。
/// - `Restart` / `Halt` は既定で常に「提案」。設定で明示的に許可された場合のみ自動発火。
pub fn gate(action: Intervention, approval: Approval, cfg: &SupervisorConfig) -> GateResult {
    match action {
        Intervention::Observe | Intervention::Notify => GateResult::Allow,

        Intervention::AutoAnswer => match approval {
            // 承認モードでの自動応答はユーザーの承認権を奪うので、確認でも通さず拒否する。
            Approval::Ask => {
                GateResult::Refuse("承認モードが「都度確認」のため自動応答しません")
            }
            Approval::Auto => GateResult::Allow,
            // Agent モードはプリセットのコマンド任せ。勝手には答えず確認を挟む。
            Approval::Agent => GateResult::NeedConfirm("Agent モードのため確認します"),
        },

        Intervention::Nudge => {
            if !cfg.allow_nudge {
                return GateResult::Refuse("促しが設定で無効です");
            }
            match approval {
                Approval::Ask => GateResult::NeedConfirm("承認モードが「都度確認」のため確認します"),
                Approval::Auto | Approval::Agent => GateResult::Allow,
            }
        }

        Intervention::Restart => {
            if approval == Approval::Ask {
                return GateResult::NeedConfirm(
                    "承認モードが「都度確認」のため再起動は確認が必要です",
                );
            }
            if cfg.allow_auto_restart {
                GateResult::Allow
            } else {
                GateResult::NeedConfirm("再起動は作業内容を失う可能性があるため確認が必要です")
            }
        }

        Intervention::Halt => {
            if approval == Approval::Ask {
                return GateResult::NeedConfirm("承認モードが「都度確認」のため停止は確認が必要です");
            }
            if cfg.allow_auto_halt {
                GateResult::Allow
            } else {
                GateResult::NeedConfirm("停止は作業内容を失う可能性があるため確認が必要です")
            }
        }
    }
}

/// スーパーバイザーが UI へ渡す介入の意図。実行するのは app.rs 側。
#[derive(Clone, Debug)]
pub struct InterventionIntent {
    pub session_id: u64,
    pub session_title: String,
    pub action: Intervention,
    pub anomaly: Anomaly,
    /// 日本語の理由 (トースト / 確認ダイアログにそのまま出せる)。
    pub reason: String,
    /// true なら UI は確認を取ってから実行すること。
    pub needs_confirmation: bool,
    /// AutoAnswer なら送るキー列、Nudge なら送る文面。
    pub payload: Option<String>,
    pub at_ms: u64,
}

impl InterventionIntent {
    /// トースト用の 1 行。
    pub fn toast_line(&self) -> String {
        format!(
            "🛡 {} — {}: {}",
            self.session_title,
            self.anomaly.label(),
            self.reason
        )
    }

    /// 確認ダイアログ用の本文。
    pub fn confirm_body(&self) -> String {
        format!(
            "{} で「{}」を検出しました。\n{}\n\n「{}」を実行しますか?{}",
            self.session_title,
            self.anomaly.label(),
            self.reason,
            self.action.label(),
            if self.action.destructive() {
                "\n※ 実行中の作業内容が失われる可能性があります。"
            } else {
                ""
            }
        )
    }
}

// ---------------------------------------------------------------------------
// サンプリング
// ---------------------------------------------------------------------------

/// app.rs が毎ティック作る、セッション 1 件のスナップショット。
/// スーパーバイザーは `Session` を所有しない (端末側の実装に依存しない)。
#[derive(Clone, Debug)]
pub struct SessionSnapshot {
    pub id: u64,
    pub title: String,
    /// vt100 画面の可視テキスト。
    pub screen_text: String,
    pub running: bool,
    /// 承認プロンプトで止まっているか (`Session::attention`)。
    pub waiting_approval: bool,
    pub exit_code: Option<u32>,
    /// 直近にユーザーが手入力したか (`Session::take_user_typed`)。
    pub user_typed: bool,
    /// PTY の累積出力バイト数が取れるなら渡す。無ければ画面から推定する。
    pub total_output_bytes: Option<u64>,
}

impl SessionSnapshot {
    /// `crate::terminal::Session` から作る補助関数 (読み取りのみ)。
    pub fn from_session(s: &crate::terminal::Session, user_typed: bool) -> Self {
        let screen_text = s
            .parser
            .lock()
            .map(|p| p.screen().contents())
            .unwrap_or_default();
        let exit_code = s.exit_code.lock().ok().and_then(|c| *c);
        Self {
            id: s.id,
            title: s.title.clone(),
            screen_text,
            running: s.running(),
            waiting_approval: s.attention,
            exit_code,
            user_typed,
            total_output_bytes: None,
        }
    }
}

/// 1 回ぶんのサンプル。検出器はこれの列だけを見る (純粋・テスト可能)。
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    pub t_ms: u64,
    /// 意味的な内容のハッシュ (数字とスピナー記号を除去済み)。変化 = 本当の進捗。
    pub content_hash: u64,
    /// 数字を残したハッシュ。変化 = カウンタが動いている (生きてはいる)。
    pub volatile_hash: u64,
    /// 末尾数行のブロックハッシュ。ループ検出に使う。
    pub block_hash: u64,
    /// 前回サンプル以降に新しく現れたエラー行数。
    pub new_error_lines: u32,
    /// 前回サンプル以降の出力バイト数 (推定可)。
    pub bytes_delta: u64,
    /// この時点で承認待ちだったか。
    pub waiting: bool,
}

/// 画面文字列の解析結果。純関数 `analyze_screen` の戻り値。
#[derive(Clone, Debug, Default)]
pub struct ScreenAnalysis {
    pub content_hash: u64,
    pub volatile_hash: u64,
    pub block_hash: u64,
    /// 非空行のハッシュ列 (数字を残した表現 = 行の同一性判定用)。
    pub line_hashes: Vec<u64>,
    /// エラーとみなした行のハッシュ列 (`line_hashes` と同じ表現)。
    pub error_line_hashes: Vec<u64>,
    /// 非空文字数 (バイト数推定に使う)。
    pub char_count: u64,
}

fn hash_of(s: &str) -> u64 {
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

fn hash_slice(v: &[u64]) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

/// ANSI エスケープを落とす (vt100 経由なら基本入っていないが、生ログでも使えるように)。
/// ターミナル生ログの「📜 前回ログ」表示 (app.rs) からも使う。
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            // CSI / OSC などをざっくり読み飛ばす
            if chars.peek() == Some(&'[') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else if chars.peek() == Some(&']') {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2 == '\u{7}' || c2 == '\u{1b}' {
                        break;
                    }
                }
            } else {
                chars.next();
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// スピナー / プログレス表示に使われる記号か。
/// 点字 (⠋⠙…)、ブロック (▁▂▃)、幾何図形 (●○◐)、装飾記号 (✻✽✢) など。
fn is_spinner_glyph(c: char) -> bool {
    matches!(c as u32,
        0x2500..=0x257F   // 罫線
        | 0x2580..=0x259F // ブロック要素
        | 0x25A0..=0x25FF // 幾何図形
        | 0x2600..=0x27BF // 記号・装飾
        | 0x2800..=0x28FF // 点字 (定番のスピナー)
        | 0x1F300..=0x1FAFF // 絵文字
    )
}

/// ASCII スピナー (`|`, `/`, `-`, `\`) 単独トークンか。
fn is_ascii_spinner_token(tok: &str) -> bool {
    matches!(tok, "|" | "/" | "-" | "\\" | "*" | "+" | "..." | "…")
}

/// 行を正規化する。
/// `keep_digits=false` なら数字を `#` に潰す (経過秒・トークン数・進捗% を無効化)。
pub fn normalize_line(line: &str, keep_digits: bool) -> String {
    let line = strip_ansi(line);
    let mut out = String::with_capacity(line.len());
    let mut last_was_hash = false;
    for tok in line.split_whitespace() {
        if is_ascii_spinner_token(tok) {
            continue;
        }
        let mut buf = String::with_capacity(tok.len());
        for c in tok.chars() {
            if is_spinner_glyph(c) {
                continue;
            }
            if c.is_ascii_digit() && !keep_digits {
                if !last_was_hash {
                    buf.push('#');
                    last_was_hash = true;
                }
                continue;
            }
            last_was_hash = false;
            for lc in c.to_lowercase() {
                buf.push(lc);
            }
        }
        let buf = buf.trim();
        if buf.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(buf);
        last_was_hash = false;
    }
    out
}

/// エラーっぽい行か。パターンはデータ駆動 (英語決め打ちにしない)。
pub fn is_error_line(normalized: &str, cfg: &SupervisorConfig) -> bool {
    if normalized.is_empty() {
        return false;
    }
    let hit = cfg
        .error_patterns
        .iter()
        .any(|p| normalized.contains(p.as_str()));
    if !hit {
        return false;
    }
    // 集計行 ("# errors", "no errors") は除外する。
    // ただし行頭がエラー語なら本物のエラー行なので除外しない
    // (例: "error: failed due to # errors")。
    let starts_with_error = cfg
        .error_patterns
        .iter()
        .any(|p| normalized.starts_with(p.as_str()));
    if !starts_with_error
        && cfg
            .error_exclude_patterns
            .iter()
            .any(|p| normalized.contains(p.as_str()))
    {
        return false;
    }
    true
}

/// 画面 1 枚を解析する。純関数。
pub fn analyze_screen(text: &str, cfg: &SupervisorConfig) -> ScreenAnalysis {
    let mut a = ScreenAnalysis::default();
    let mut norm_lines: Vec<String> = Vec::new();
    let mut volatile_lines: Vec<String> = Vec::new();
    // ブロックハッシュ (ループ検出) 用は数字を潰した表現を使う。
    // 「行の同一性」(新規行/新規エラー行の判定) は数字を残した表現を使う
    // — でないと "error[E0001]" と "error[E0002]" が同一視され、
    // エラーの噴出を 1 行と数えてしまう。
    let mut norm_hashes: Vec<u64> = Vec::new();

    for raw in text.lines() {
        let n = normalize_line(raw, false);
        if n.is_empty() {
            continue;
        }
        let v = normalize_line(raw, true);
        a.char_count += v.chars().count() as u64;
        let vh = hash_of(&v);
        a.line_hashes.push(vh);
        norm_hashes.push(hash_of(&n));
        if is_error_line(&n, cfg) {
            a.error_line_hashes.push(vh);
        }
        volatile_lines.push(v);
        norm_lines.push(n);
    }

    a.content_hash = hash_of(&norm_lines.join("\n"));
    a.volatile_hash = hash_of(&volatile_lines.join("\n"));

    let n = cfg.loop_block_lines.max(1);
    let tail: Vec<u64> = norm_hashes.iter().rev().take(n).rev().copied().collect();
    a.block_hash = if tail.is_empty() { 0 } else { hash_slice(&tail) };
    a
}

// ---------------------------------------------------------------------------
// 検出器 — すべて純関数。偽陽性テストが本体。
// ---------------------------------------------------------------------------

fn window(samples: &[Sample], from_ms: u64) -> &[Sample] {
    let idx = samples.partition_point(|s| s.t_ms < from_ms);
    &samples[idx..]
}

/// 最後に「意味的な進捗」があった時刻。変化が一度も無ければ観測開始時刻。
fn last_semantic_progress_ms(samples: &[Sample]) -> Option<u64> {
    let first = samples.first()?;
    for i in (1..samples.len()).rev() {
        if samples[i].content_hash != samples[i - 1].content_hash {
            return Some(samples[i].t_ms);
        }
    }
    Some(first.t_ms)
}

fn changes_in(samples: &[Sample], from_ms: u64, volatile: bool) -> usize {
    let mut n = 0;
    let mut prev: Option<u64> = None;
    for s in samples.iter() {
        let h = if volatile { s.volatile_hash } else { s.content_hash };
        if s.t_ms >= from_ms {
            if let Some(p) = prev {
                if p != h {
                    n += 1;
                }
            }
        }
        prev = Some(h);
    }
    n
}

/// **停滞**: 作業中のはずなのに意味的な進捗が無い。
///
/// - 承認待ちの間は絶対に発火しない (それは `SilentWait` の担当)。
/// - 「最後の出力からの経過」ではなく窓内の**進捗レート**で判定するので、
///   周期的に出力が出る長いビルドは誤検出しない。
/// - スピナーが回っているだけ (バイトは出るが中身が進まない) の場合は
///   `spinner_grace_factor` 倍まで待つ。ただし無限には待たない。
pub fn detect_stall(samples: &[Sample], cfg: &SupervisorConfig, now_ms: u64) -> Option<String> {
    if !cfg.detect_stall || samples.len() < 2 {
        return None;
    }
    let last = samples.last()?;
    if last.waiting {
        return None; // 承認待ちは停滞ではない
    }
    let win_ms = cfg.stall_secs.saturating_mul(1000);
    let from = now_ms.saturating_sub(win_ms);

    // 窓内に一度でも実進捗があれば停滞ではない (長時間ビルドの誤検出回避)
    if changes_in(samples, from, false) > 0 {
        return None;
    }

    let progress_at = last_semantic_progress_ms(samples)?;
    let quiet_ms = now_ms.saturating_sub(progress_at);

    // カウンタ (経過秒など) だけ動いているならスピナー中。猶予を伸ばす。
    let counter_alive = changes_in(samples, from, true) > 0;
    let threshold = if counter_alive {
        win_ms.saturating_mul(cfg.spinner_grace_factor.max(1))
    } else {
        win_ms
    };
    if quiet_ms < threshold {
        return None;
    }
    Some(if counter_alive {
        format!(
            "表示は動いていますが {} 秒間まったく進捗がありません",
            quiet_ms / 1000
        )
    } else {
        format!("{} 秒間出力が止まっています", quiet_ms / 1000)
    })
}

/// **ループ / 振動**: 同じ出力ブロックが窓内で繰り返し再出現する。
///
/// 静止画面 (スピナーだけ) を誤検出しないため、連続する同一ハッシュは 1 つに潰す。
/// つまり `A,A,A` は 1 回、`A,B,A,B,A` は A が 3 回。
/// 「似た行を大量に出す」正常系 (cargo build 等) は各行が別ハッシュになるので出現 1 回止まり。
pub fn detect_loop(samples: &[Sample], cfg: &SupervisorConfig, now_ms: u64) -> Option<String> {
    if !cfg.detect_loop {
        return None;
    }
    let from = now_ms.saturating_sub(cfg.loop_window_secs.saturating_mul(1000));
    let mut seq: Vec<u64> = Vec::new();
    for s in window(samples, from) {
        if s.block_hash == 0 {
            continue;
        }
        if seq.last() != Some(&s.block_hash) {
            seq.push(s.block_hash);
        }
    }
    if seq.len() < 2 {
        return None; // 変化していない = ループではない (停滞の担当)
    }
    let mut counts: HashMap<u64, usize> = HashMap::new();
    for h in &seq {
        *counts.entry(*h).or_insert(0) += 1;
    }
    let max = counts.values().copied().max().unwrap_or(0);
    if max >= cfg.loop_repeats.max(2) {
        Some(format!(
            "同じ出力が {} 分以内に {} 回繰り返されています",
            cfg.loop_window_secs / 60,
            max
        ))
    } else {
        None
    }
}

/// **エラー嵐**: 新規エラー行の発生率がしきい値を超えた。
pub fn detect_error_storm(
    samples: &[Sample],
    cfg: &SupervisorConfig,
    now_ms: u64,
) -> Option<String> {
    if !cfg.detect_error_storm {
        return None;
    }
    let from = now_ms.saturating_sub(cfg.error_window_secs.saturating_mul(1000));
    let win = window(samples, from);
    if win.len() < 2 {
        return None;
    }
    let total: u32 = win.iter().map(|s| s.new_error_lines).sum();
    if total < cfg.error_min_count {
        return None;
    }
    let span_ms = win.last()?.t_ms.saturating_sub(win.first()?.t_ms);
    if span_ms == 0 {
        return None;
    }
    let rate = total as f32 / (span_ms as f32 / 1000.0);
    if rate >= cfg.error_rate_per_sec {
        Some(format!(
            "エラー行が {:.1} 行/秒 (直近 {} 秒で {} 行) 出ています",
            rate,
            span_ms / 1000,
            total
        ))
    } else {
        None
    }
}

/// **異常終了**: 作業中 / 承認待ちのままプロセスが落ちた。
pub fn detect_crash(
    running: bool,
    exit_code: Option<u32>,
    prev: SessionState,
    cfg: &SupervisorConfig,
) -> Option<String> {
    if !cfg.detect_crash || running {
        return None;
    }
    let was_active = matches!(
        prev,
        SessionState::Working
            | SessionState::WaitingApproval
            | SessionState::Stalled
            | SessionState::Looping
            | SessionState::Errored
    );
    if !was_active {
        return None;
    }
    match exit_code {
        Some(0) | None => None,
        Some(c) => Some(format!("作業中に終了コード {c} で終了しました")),
    }
}

/// **沈黙した承認待ち**: 承認待ちのまま誰も反応していない。
pub fn detect_silent_wait(
    waiting_since_ms: Option<u64>,
    last_user_input_ms: Option<u64>,
    cfg: &SupervisorConfig,
    now_ms: u64,
) -> Option<String> {
    if !cfg.detect_silent_wait {
        return None;
    }
    let since = waiting_since_ms?;
    let waited = now_ms.saturating_sub(since);
    if waited < cfg.silent_wait_secs.saturating_mul(1000) {
        return None;
    }
    // 承認待ちに入ってから後にユーザーが触っているなら放置ではない
    if let Some(t) = last_user_input_ms {
        if t >= since {
            return None;
        }
    }
    Some(format!(
        "{} 秒間、承認待ちのまま誰も応答していません",
        waited / 1000
    ))
}

/// **暴走出力**: 出力バイトレートがベースラインを大きく超えた状態が続いている。
pub fn detect_runaway(samples: &[Sample], cfg: &SupervisorConfig, now_ms: u64) -> Option<String> {
    if !cfg.detect_runaway || samples.len() < 6 {
        return None;
    }
    let sustain_ms = cfg.runaway_sustain_secs.saturating_mul(1000);
    let from = now_ms.saturating_sub(sustain_ms);
    let recent = window(samples, from);
    if recent.len() < 3 {
        return None;
    }
    let split = samples.len() - recent.len();
    let base = &samples[..split];
    if base.len() < 3 {
        return None;
    }

    let rate_of = |w: &[Sample]| -> f32 {
        let span = w
            .last()
            .map(|l| l.t_ms)
            .unwrap_or(0)
            .saturating_sub(w.first().map(|f| f.t_ms).unwrap_or(0));
        if span == 0 {
            return 0.0;
        }
        let bytes: u64 = w.iter().skip(1).map(|s| s.bytes_delta).sum();
        bytes as f32 / (span as f32 / 1000.0)
    };

    let recent_span = recent
        .last()?
        .t_ms
        .saturating_sub(recent.first()?.t_ms);
    if recent_span < sustain_ms / 2 {
        return None; // 十分な持続がない
    }

    let recent_rate = rate_of(recent);
    if recent_rate < cfg.runaway_floor_bps {
        return None;
    }
    // ベースラインは中央値 (瞬間的なスパイクに引きずられないように)
    let mut base_rates: Vec<f32> = base
        .windows(2)
        .filter_map(|w| {
            let dt = w[1].t_ms.saturating_sub(w[0].t_ms);
            if dt == 0 {
                None
            } else {
                Some(w[1].bytes_delta as f32 / (dt as f32 / 1000.0))
            }
        })
        .collect();
    if base_rates.is_empty() {
        return None;
    }
    base_rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let baseline = base_rates[base_rates.len() / 2].max(1.0);

    if recent_rate >= baseline * cfg.runaway_factor {
        Some(format!(
            "出力が平常の {:.0} 倍 ({:.0} KB/秒) に膨れ上がっています",
            recent_rate / baseline,
            recent_rate / 1024.0
        ))
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// リングバッファ
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Ring<T> {
    buf: VecDeque<T>,
    cap: usize,
}

impl<T> Ring<T> {
    fn new(cap: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(cap.min(1024)),
            cap: cap.max(1),
        }
    }
    fn push(&mut self, v: T) {
        if self.buf.len() >= self.cap {
            self.buf.pop_front();
        }
        self.buf.push_back(v);
    }
    fn len(&self) -> usize {
        self.buf.len()
    }
    fn iter(&self) -> impl Iterator<Item = &T> {
        self.buf.iter()
    }
    fn last(&self) -> Option<&T> {
        self.buf.back()
    }
    fn retain_from(&mut self, keep: impl Fn(&T) -> bool) {
        while let Some(f) = self.buf.front() {
            if keep(f) {
                break;
            }
            self.buf.pop_front();
        }
    }
}

// ---------------------------------------------------------------------------
// セッションごとの監視状態
// ---------------------------------------------------------------------------

struct SessionMonitor {
    title: String,
    state: SessionState,
    samples: Ring<Sample>,
    history: Ring<StateTransition>,
    /// 直近に見た行ハッシュ (新規エラー行の差分計算用)。上限付き。
    seen_lines: HashSet<u64>,
    seen_order: Ring<u64>,
    last_bytes_total: Option<u64>,
    last_char_count: u64,
    waiting_since_ms: Option<u64>,
    last_user_input_ms: Option<u64>,
    /// 異常ごとの継続開始時刻とはしごの段。
    escalation: HashMap<Anomaly, Escalation>,
    /// 直近の介入 (レート制限用)。
    recent_actions: Ring<(u64, Intervention, Anomaly)>,
    /// 最終スクリーン (LLM 抜粋用)。上限付き。
    last_screen: String,
}

#[derive(Clone, Copy, Debug)]
struct Escalation {
    since_ms: u64,
    /// 0 = Observe 済み, 1 = Notify 済み, 2 = 上位介入 済み
    step: u8,
    last_seen_ms: u64,
}

const SEEN_LINES_CAP: usize = 600;
const LAST_SCREEN_CAP: usize = 8192;

impl SessionMonitor {
    fn new(title: String, cfg: &SupervisorConfig) -> Self {
        Self {
            title,
            state: SessionState::Idle,
            samples: Ring::new(cfg.sample_capacity),
            history: Ring::new(cfg.history_capacity),
            seen_lines: HashSet::new(),
            seen_order: Ring::new(SEEN_LINES_CAP),
            last_bytes_total: None,
            last_char_count: 0,
            waiting_since_ms: None,
            last_user_input_ms: None,
            escalation: HashMap::new(),
            recent_actions: Ring::new(64),
            last_screen: String::new(),
        }
    }

    fn note_lines(&mut self, hashes: &[u64]) {
        for h in hashes {
            if self.seen_lines.insert(*h) {
                if self.seen_order.len() >= SEEN_LINES_CAP {
                    if let Some(old) = self.seen_order.buf.pop_front() {
                        self.seen_lines.remove(&old);
                    }
                }
                self.seen_order.push(*h);
            }
        }
    }

    fn transition(&mut self, to: SessionState, reason: String, at_ms: u64) {
        if self.state == to {
            return;
        }
        let from = self.state;
        self.state = to;
        self.history.push(StateTransition {
            at_ms,
            from,
            to,
            reason,
        });
    }

    /// 直近 1 時間の介入件数。
    fn actions_last_hour(&self, now_ms: u64) -> usize {
        let from = now_ms.saturating_sub(3_600_000);
        self.recent_actions
            .iter()
            .filter(|(t, _, _)| *t >= from)
            .count()
    }

    fn last_action_at(&self, action: Intervention) -> Option<u64> {
        self.recent_actions
            .iter()
            .filter(|(_, a, _)| *a == action)
            .map(|(t, _, _)| *t)
            .max()
    }
}

// ---------------------------------------------------------------------------
// LLM エスカレーション (既定 OFF)
// ---------------------------------------------------------------------------

/// LLM 診断への入力。出力抜粋は必ず秘匿化してから渡す。
#[derive(Clone, Debug)]
pub struct DiagnosisRequest {
    pub session_id: u64,
    pub session_title: String,
    pub anomaly: Anomaly,
    pub state: SessionState,
    /// 秘匿化済みの出力抜粋。
    pub excerpt: String,
}

/// LLM からの診断結果。推奨はそのまま実行せず必ず `gate` を通す。
#[derive(Clone, Debug)]
pub struct Diagnosis {
    pub session_id: u64,
    pub anomaly: Anomaly,
    /// 日本語 1〜2 行の所見。
    pub summary: String,
    pub recommended: Intervention,
}

/// LLM 診断の実装を差し替えるためのフック。
/// スーパーバイザーはこれが無くても完全に機能する (見張り役自身が不安定であってはならない)。
pub trait Diagnostician: Send + Sync {
    fn diagnose(&self, req: &DiagnosisRequest) -> Option<Diagnosis>;
}

/// 出力抜粋から秘密になりそうな文字列を落とす。
pub fn redact(text: &str, max_chars: usize) -> String {
    let home = dirs::home_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default();
    let mut out = String::with_capacity(text.len().min(max_chars * 2));
    for line in text.lines() {
        let line = if !home.is_empty() {
            line.replace(&home, "~")
        } else {
            line.to_string()
        };
        let mut buf = String::with_capacity(line.len());
        for tok in line.split_whitespace() {
            let secretish = tok.starts_with("sk-")
                || tok.starts_with("ghp_")
                || tok.starts_with("gho_")
                || tok.starts_with("github_pat_")
                || tok.starts_with("xox")
                || tok.contains('@') && tok.contains('.')
                || (tok.len() >= 32
                    && tok
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'));
            if !buf.is_empty() {
                buf.push(' ');
            }
            buf.push_str(if secretish { "***" } else { tok });
        }
        out.push_str(&buf);
        out.push('\n');
    }
    // 末尾 max_chars 文字を残す (直近の出力のほうが診断に効く)
    let n = out.chars().count();
    if n > max_chars {
        out.chars().skip(n - max_chars).collect()
    } else {
        out
    }
}

// ---------------------------------------------------------------------------
// スーパーバイザー本体
// ---------------------------------------------------------------------------

pub struct Supervisor {
    cfg: SupervisorConfig,
    origin: Instant,
    monitors: HashMap<u64, SessionMonitor>,
    last_sample_ms: u64,
    diagnostician: Option<Arc<dyn Diagnostician>>,
    diag_tx: Sender<Diagnosis>,
    diag_rx: Receiver<Diagnosis>,
}

impl Supervisor {
    pub fn new(cfg: SupervisorConfig) -> Self {
        let (diag_tx, diag_rx) = channel();
        Self {
            cfg,
            origin: Instant::now(),
            monitors: HashMap::new(),
            last_sample_ms: 0,
            diagnostician: None,
            diag_tx,
            diag_rx,
        }
    }

    pub fn config(&self) -> &SupervisorConfig {
        &self.cfg
    }

    pub fn set_config(&mut self, cfg: SupervisorConfig) {
        self.cfg = cfg;
    }

    /// LLM 診断の実装を差し込む (任意)。差し込んでも `llm_escalation` が false なら使われない。
    pub fn set_diagnostician(&mut self, d: Arc<dyn Diagnostician>) {
        self.diagnostician = Some(d);
    }

    pub fn elapsed_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    pub fn state_of(&self, id: u64) -> Option<SessionState> {
        self.monitors.get(&id).map(|m| m.state)
    }

    /// 状態遷移履歴 (新しい順ではなく古い順、上限付き)。
    pub fn history_of(&self, id: u64) -> Vec<StateTransition> {
        self.monitors
            .get(&id)
            .map(|m| m.history.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn forget(&mut self, id: u64) {
        self.monitors.remove(&id);
    }

    /// UI スレッドから毎フレーム呼ぶ。内部で間引くので毎フレームで安い。
    pub fn tick(&mut self, snaps: &[SessionSnapshot], approval: Approval) -> Vec<InterventionIntent> {
        let now = self.elapsed_ms();
        self.tick_ms(snaps, approval, now)
    }

    /// テスト可能な本体。時刻を明示的に渡す。
    pub fn tick_ms(
        &mut self,
        snaps: &[SessionSnapshot],
        approval: Approval,
        now_ms: u64,
    ) -> Vec<InterventionIntent> {
        if !self.cfg.enabled {
            return Vec::new();
        }
        if now_ms.saturating_sub(self.last_sample_ms) < self.cfg.sample_interval_ms
            && self.last_sample_ms > 0
        {
            return Vec::new();
        }
        self.last_sample_ms = now_ms;

        // 消えたセッションの監視状態を捨てる (無制限に増やさない)
        let alive: HashSet<u64> = snaps.iter().map(|s| s.id).collect();
        self.monitors.retain(|k, _| alive.contains(k));

        let mut intents = Vec::new();
        for snap in snaps {
            intents.extend(self.observe_one(snap, approval, now_ms));
        }
        intents
    }

    fn observe_one(
        &mut self,
        snap: &SessionSnapshot,
        approval: Approval,
        now_ms: u64,
    ) -> Vec<InterventionIntent> {
        let cfg = self.cfg.clone();
        let mon = self
            .monitors
            .entry(snap.id)
            .or_insert_with(|| SessionMonitor::new(snap.title.clone(), &cfg));
        mon.title = snap.title.clone();

        // --- サンプル生成 ---
        let a = analyze_screen(&snap.screen_text, &cfg);
        let new_errors = a
            .error_line_hashes
            .iter()
            .filter(|h| !mon.seen_lines.contains(*h))
            .count() as u32;
        let bytes_delta = match (snap.total_output_bytes, mon.last_bytes_total) {
            (Some(t), Some(p)) => t.saturating_sub(p),
            (Some(t), None) => {
                mon.last_bytes_total = Some(t);
                0
            }
            _ => {
                // PTY のバイト数が無いときは「新しく現れた行数 × 平均行長」で推定する
                let new_lines = a
                    .line_hashes
                    .iter()
                    .filter(|h| !mon.seen_lines.contains(*h))
                    .count() as u64;
                let avg = if a.line_hashes.is_empty() {
                    0
                } else {
                    a.char_count / a.line_hashes.len() as u64
                };
                new_lines * avg.max(1)
            }
        };
        if let Some(t) = snap.total_output_bytes {
            mon.last_bytes_total = Some(t);
        }
        mon.note_lines(&a.line_hashes);
        mon.last_char_count = a.char_count;
        mon.last_screen = if snap.screen_text.len() > LAST_SCREEN_CAP {
            snap.screen_text
                .chars()
                .rev()
                .take(LAST_SCREEN_CAP)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect()
        } else {
            snap.screen_text.clone()
        };

        mon.samples.push(Sample {
            t_ms: now_ms,
            content_hash: a.content_hash,
            volatile_hash: a.volatile_hash,
            block_hash: a.block_hash,
            new_error_lines: new_errors,
            bytes_delta,
            waiting: snap.waiting_approval,
        });

        if snap.user_typed {
            mon.last_user_input_ms = Some(now_ms);
        }
        if snap.waiting_approval {
            if mon.waiting_since_ms.is_none() {
                mon.waiting_since_ms = Some(now_ms);
            }
        } else {
            mon.waiting_since_ms = None;
        }

        let samples: Vec<Sample> = mon.samples.iter().copied().collect();
        let prev_state = mon.state;
        let waiting_since = mon.waiting_since_ms;
        let last_input = mon.last_user_input_ms;

        // --- 検出 ---
        let mut found: Vec<(Anomaly, String)> = Vec::new();
        if let Some(r) = detect_crash(snap.running, snap.exit_code, prev_state, &cfg) {
            found.push((Anomaly::Crash, r));
        }
        if snap.running {
            if let Some(r) = detect_loop(&samples, &cfg, now_ms) {
                found.push((Anomaly::Looping, r));
            }
            if let Some(r) = detect_error_storm(&samples, &cfg, now_ms) {
                found.push((Anomaly::ErrorStorm, r));
            }
            if let Some(r) = detect_runaway(&samples, &cfg, now_ms) {
                found.push((Anomaly::Runaway, r));
            }
            if let Some(r) = detect_stall(&samples, &cfg, now_ms) {
                found.push((Anomaly::Stall, r));
            }
            if let Some(r) = detect_silent_wait(waiting_since, last_input, &cfg, now_ms) {
                found.push((Anomaly::SilentWait, r));
            }
        }

        // --- 状態機械の更新 ---
        let (next_state, reason) = derive_state(prev_state, snap, &samples, &found, &cfg, now_ms);
        let mon = self.monitors.get_mut(&snap.id).expect("monitor exists");
        mon.transition(next_state, reason, now_ms);

        // --- はしごを昇る ---
        let mut intents = Vec::new();
        let active: HashSet<Anomaly> = found.iter().map(|(a, _)| *a).collect();
        mon.escalation.retain(|k, _| active.contains(k));

        for (anomaly, reason) in &found {
            // 借用を跨がないよう、いったん値として取り出してから書き戻す
            let mut esc = *mon.escalation.entry(*anomaly).or_insert(Escalation {
                since_ms: now_ms,
                step: 0,
                last_seen_ms: now_ms,
            });
            esc.last_seen_ms = now_ms;
            let held = now_ms.saturating_sub(esc.since_ms);

            // 段の決定: 緊急なら即通知＋即提案、そうでなければ継続時間で昇る
            let notify_at = if anomaly.urgent() {
                0
            } else {
                cfg.notify_after_secs.saturating_mul(1000)
            };
            let escalate_at = if anomaly.urgent() {
                0
            } else {
                cfg.escalate_after_secs.saturating_mul(1000)
            };

            if esc.step == 0 && held >= notify_at {
                esc.step = 1;
                if let Some(i) = Self::make_intent(
                    mon,
                    snap,
                    *anomaly,
                    Intervention::Notify,
                    reason.clone(),
                    approval,
                    &cfg,
                    now_ms,
                ) {
                    intents.push(i);
                }
            }
            if esc.step == 1 && held >= escalate_at {
                esc.step = 2;
                let action = anomaly.desired_action();
                let payload = match action {
                    Intervention::Nudge => Some(cfg.nudge_text.clone()),
                    Intervention::AutoAnswer => Some("\r".to_string()),
                    _ => None,
                };
                if let Some(mut i) = Self::make_intent(
                    mon,
                    snap,
                    *anomaly,
                    action,
                    reason.clone(),
                    approval,
                    &cfg,
                    now_ms,
                ) {
                    i.payload = payload;
                    intents.push(i);
                }
            }
            mon.escalation.insert(*anomaly, esc);
        }
        intents
    }

    /// ゲート + レート制限を通して意図を作る。通らなければ None。
    #[allow(clippy::too_many_arguments)]
    fn make_intent(
        mon: &mut SessionMonitor,
        snap: &SessionSnapshot,
        anomaly: Anomaly,
        action: Intervention,
        reason: String,
        approval: Approval,
        cfg: &SupervisorConfig,
        now_ms: u64,
    ) -> Option<InterventionIntent> {
        // --- ゲート (安全性) ---
        let needs_confirmation = match gate(action, approval, cfg) {
            GateResult::Allow => false,
            GateResult::NeedConfirm(_) => true,
            GateResult::Refuse(_) => return None,
        };

        // --- レート制限 ---
        if mon.actions_last_hour(now_ms) >= cfg.max_interventions_per_hour {
            return None;
        }
        let cooldown_ms = 1000
            * match action {
                Intervention::Observe => 0,
                Intervention::Notify => cfg.cooldown_notify_secs,
                Intervention::AutoAnswer => cfg.cooldown_auto_answer_secs,
                Intervention::Nudge => cfg.cooldown_nudge_secs,
                Intervention::Restart | Intervention::Halt => cfg.cooldown_restart_secs,
            };
        if let Some(last) = mon.last_action_at(action) {
            if now_ms.saturating_sub(last) < cooldown_ms {
                return None;
            }
        }

        mon.recent_actions.push((now_ms, action, anomaly));
        mon.recent_actions
            .retain_from(|(t, _, _)| now_ms.saturating_sub(*t) <= 3_600_000);

        Some(InterventionIntent {
            session_id: snap.id,
            session_title: snap.title.clone(),
            action,
            anomaly,
            reason,
            needs_confirmation,
            payload: None,
            at_ms: now_ms,
        })
    }

    // --- LLM エスカレーション (任意) ---

    /// LLM 診断を非同期で依頼する。`llm_escalation` が false、または実装未設定なら何もしない。
    /// 既存の作法どおり thread + mpsc + request_repaint。
    pub fn request_diagnosis(&self, session_id: u64, anomaly: Anomaly, ctx: &egui::Context) {
        if !self.cfg.llm_escalation {
            return;
        }
        let Some(d) = self.diagnostician.clone() else {
            return;
        };
        let Some(mon) = self.monitors.get(&session_id) else {
            return;
        };
        let req = DiagnosisRequest {
            session_id,
            session_title: mon.title.clone(),
            anomaly,
            state: mon.state,
            excerpt: redact(&mon.last_screen, self.cfg.llm_excerpt_chars),
        };
        let tx = self.diag_tx.clone();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            if let Some(out) = d.diagnose(&req) {
                let _ = tx.send(out);
            }
            ctx.request_repaint();
        });
    }

    /// 届いた診断を回収する (update() から try_recv で排出)。
    pub fn poll_diagnoses(&mut self) -> Vec<Diagnosis> {
        let mut out = Vec::new();
        while let Ok(d) = self.diag_rx.try_recv() {
            out.push(d);
        }
        out
    }

    /// LLM の推奨を意図に変換する。**必ず同じゲートを通す** (LLM に安全弁は任せない)。
    pub fn intent_from_diagnosis(
        &mut self,
        d: &Diagnosis,
        approval: Approval,
    ) -> Option<InterventionIntent> {
        let cfg = self.cfg.clone();
        if !cfg.llm_escalation {
            return None;
        }
        let mon = self.monitors.get_mut(&d.session_id)?;
        let snap = SessionSnapshot {
            id: d.session_id,
            title: mon.title.clone(),
            screen_text: String::new(),
            running: true,
            waiting_approval: false,
            exit_code: None,
            user_typed: false,
            total_output_bytes: None,
        };
        let now = self.origin.elapsed().as_millis() as u64;
        let mut i = Self::make_intent(
            mon,
            &snap,
            d.anomaly,
            d.recommended,
            format!("AI 診断: {}", d.summary),
            approval,
            &cfg,
            now,
        )?;
        // LLM 由来の破壊的操作は設定に関わらず必ず確認を取る
        if i.action.destructive() {
            i.needs_confirmation = true;
        }
        Some(i)
    }
}

/// 状態を導出する。異常が出ていればそれを優先。
fn derive_state(
    prev: SessionState,
    snap: &SessionSnapshot,
    samples: &[Sample],
    found: &[(Anomaly, String)],
    cfg: &SupervisorConfig,
    now_ms: u64,
) -> (SessionState, String) {
    if !snap.running {
        if found.iter().any(|(a, _)| *a == Anomaly::Crash) {
            return (SessionState::Crashed, "作業中にプロセスが終了".into());
        }
        return match snap.exit_code {
            Some(0) | None => (SessionState::Done, "正常終了".into()),
            Some(c) => (SessionState::Errored, format!("終了コード {c}")),
        };
    }
    if snap.waiting_approval {
        return (SessionState::WaitingApproval, "承認プロンプト検出".into());
    }
    for want in [Anomaly::Looping, Anomaly::ErrorStorm, Anomaly::Stall] {
        if let Some((a, r)) = found.iter().find(|(x, _)| *x == want) {
            if let Some(s) = a.state() {
                return (s, r.clone());
            }
        }
    }
    // 直近に意味的な進捗があれば作業中
    let idle_win = now_ms.saturating_sub(cfg.sample_interval_ms.saturating_mul(8).max(10_000));
    if changes_in(samples, idle_win, false) > 0 {
        return (SessionState::Working, "出力に進捗あり".into());
    }
    let _ = prev;
    (SessionState::Idle, "出力なし".into())
}

// ---------------------------------------------------------------------------
// テスト — 偽陽性のほうが本命。
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> SupervisorConfig {
        SupervisorConfig::default()
    }

    /// `Approval` は Debug を派生していないのでテスト用の名前関数を持つ。
    fn ap_name(a: Approval) -> &'static str {
        match a {
            Approval::Ask => "Ask",
            Approval::Auto => "Auto",
            Approval::Agent => "Agent",
        }
    }

    /// 画面テキスト列からサンプル列を作るテストヘルパ (実運用と同じ経路を通す)。
    fn samples_from(screens: &[(u64, &str)], cfg: &SupervisorConfig, waiting: bool) -> Vec<Sample> {
        let mut seen: HashSet<u64> = HashSet::new();
        let mut out = Vec::new();
        for (t, text) in screens {
            let a = analyze_screen(text, cfg);
            let new_err = a
                .error_line_hashes
                .iter()
                .filter(|h| !seen.contains(*h))
                .count() as u32;
            let new_lines = a.line_hashes.iter().filter(|h| !seen.contains(*h)).count() as u64;
            for h in &a.line_hashes {
                seen.insert(*h);
            }
            let avg = if a.line_hashes.is_empty() {
                1
            } else {
                (a.char_count / a.line_hashes.len() as u64).max(1)
            };
            out.push(Sample {
                t_ms: *t,
                content_hash: a.content_hash,
                volatile_hash: a.volatile_hash,
                block_hash: a.block_hash,
                new_error_lines: new_err,
                bytes_delta: new_lines * avg,
                waiting,
            });
        }
        out
    }

    // ---------------- 正規化 ----------------

    #[test]
    fn spinner_glyphs_and_counters_normalize_away() {
        let a = normalize_line("⠋ Thinking… (12s · 340 tokens)", false);
        let b = normalize_line("⠙ Thinking… (13s · 355 tokens)", false);
        assert_eq!(a, b, "スピナーと数字は同じ内容に潰れるべき: {a} / {b}");
        // 数字を残せば別物になる (カウンタ生存の判定に使える)
        assert_ne!(
            normalize_line("⠋ Thinking… (12s)", true),
            normalize_line("⠙ Thinking… (13s)", true)
        );
    }

    #[test]
    fn similar_but_distinct_lines_stay_distinct() {
        // 正規化しても別の行は別のまま (ループ誤検出の防止)
        assert_ne!(
            normalize_line("   Compiling serde v1.0.200", false),
            normalize_line("   Compiling toml v0.8.19", false)
        );
    }

    #[test]
    fn ansi_is_stripped() {
        assert_eq!(
            normalize_line("\u{1b}[32mok\u{1b}[0m done", false),
            "ok done"
        );
    }

    // ---------------- 停滞 ----------------

    #[test]
    fn stall_fires_when_output_frozen() {
        let mut c = cfg();
        c.stall_secs = 60;
        let screens: Vec<(u64, &str)> = (0..40)
            .map(|i| (i * 5_000u64, "building the thing\nplease wait"))
            .collect();
        let s = samples_from(&screens, &c, false);
        assert!(
            detect_stall(&s, &c, 195_000).is_some(),
            "完全に固まっていれば停滞になるべき"
        );
    }

    #[test]
    fn spinner_does_not_trigger_stall() {
        // 偽陽性テスト: スピナーが回っている間は既定の猶予内で発火しない
        let mut c = cfg();
        c.stall_secs = 60; // 猶予込みで 120s
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧"];
        let texts: Vec<String> = (0..20)
            .map(|i| format!("{} Thinking… ({}s · {} tokens)", frames[i % 8], i * 5, 100 + i * 7))
            .collect();
        let screens: Vec<(u64, &str)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (i as u64 * 5_000, t.as_str()))
            .collect();
        let s = samples_from(&screens, &c, false);
        assert!(
            detect_stall(&s, &c, 95_000).is_none(),
            "スピナー animation で停滞にしてはいけない"
        );
    }

    #[test]
    fn spinner_eventually_stalls_not_fooled_forever() {
        // 「スピナーに騙されない」テスト: 猶予を超えれば必ず発火する
        let mut c = cfg();
        c.stall_secs = 60;
        c.spinner_grace_factor = 2;
        let frames = ["⠋", "⠙", "⠹", "⠸"];
        let texts: Vec<String> = (0..80)
            .map(|i| format!("{} Thinking… ({}s)", frames[i % 4], i * 5))
            .collect();
        let screens: Vec<(u64, &str)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (i as u64 * 5_000, t.as_str()))
            .collect();
        let s = samples_from(&screens, &c, false);
        assert!(
            detect_stall(&s, &c, 395_000).is_some(),
            "猶予(120s)を大きく超えたら停滞と判定すべき"
        );
    }

    #[test]
    fn long_compile_with_periodic_output_is_not_stalled() {
        // 偽陽性テスト: 30 秒おきに実進捗が出る長いビルド
        let mut c = cfg();
        c.stall_secs = 60;
        let mut screens: Vec<(u64, String)> = Vec::new();
        let mut body = String::new();
        for i in 0..40u64 {
            if i % 6 == 0 {
                body.push_str(&format!("   Compiling crate_{i} v1.0\n"));
            }
            screens.push((i * 5_000, body.clone()));
        }
        let refs: Vec<(u64, &str)> = screens.iter().map(|(t, s)| (*t, s.as_str())).collect();
        let s = samples_from(&refs, &c, false);
        assert!(
            detect_stall(&s, &c, 195_000).is_none(),
            "周期的に実進捗が出るビルドを停滞にしてはいけない"
        );
    }

    #[test]
    fn waiting_approval_never_stalls() {
        // 偽陽性テスト: 承認待ちは停滞ではない
        let mut c = cfg();
        c.stall_secs = 10;
        let screens: Vec<(u64, &str)> = (0..40)
            .map(|i| (i * 5_000u64, "Do you want to proceed?\n❯ 1. Yes\n  2. No"))
            .collect();
        let s = samples_from(&screens, &c, true);
        assert!(
            detect_stall(&s, &c, 195_000).is_none(),
            "承認待ちで停滞を出してはいけない"
        );
    }

    // ---------------- ループ ----------------

    #[test]
    fn oscillating_retry_is_detected_as_loop() {
        let c = cfg();
        let a = "$ cargo build\nerror: linker failed\nretrying...";
        let b = "$ cargo build\nlinking...\n";
        let mut screens: Vec<(u64, &str)> = Vec::new();
        for i in 0..12u64 {
            screens.push((i * 10_000, if i % 2 == 0 { a } else { b }));
        }
        let s = samples_from(&screens, &c, false);
        assert!(
            detect_loop(&s, &c, 115_000).is_some(),
            "同じ失敗の往復はループとして検出すべき"
        );
    }

    #[test]
    fn spinner_does_not_trigger_loop() {
        // 偽陽性テスト: 静止した画面 (スピナーのみ) はループではない
        let c = cfg();
        let frames = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴"];
        let texts: Vec<String> = (0..30)
            .map(|i| format!("{} working ({}s)", frames[i % 6], i))
            .collect();
        let screens: Vec<(u64, &str)> = texts
            .iter()
            .enumerate()
            .map(|(i, t)| (i as u64 * 5_000, t.as_str()))
            .collect();
        let s = samples_from(&screens, &c, false);
        assert!(
            detect_loop(&s, &c, 145_000).is_none(),
            "スピナーをループ扱いしてはいけない"
        );
    }

    #[test]
    fn many_similar_lines_are_not_a_loop() {
        // 偽陽性テスト: 似ているが別内容の行を大量に出すのは正常
        let c = cfg();
        let mut body = String::new();
        let mut screens: Vec<(u64, String)> = Vec::new();
        for i in 0..40u64 {
            body.push_str(&format!("test module_{i}::case_{i} ... ok\n"));
            screens.push((i * 2_000, body.clone()));
        }
        let refs: Vec<(u64, &str)> = screens.iter().map(|(t, s)| (*t, s.as_str())).collect();
        let s = samples_from(&refs, &c, false);
        assert!(
            detect_loop(&s, &c, 80_000).is_none(),
            "似た行の大量出力をループ扱いしてはいけない"
        );
    }

    // ---------------- エラー嵐 ----------------

    #[test]
    fn error_storm_is_detected() {
        let c = cfg();
        let mut body = String::new();
        let mut screens: Vec<(u64, String)> = Vec::new();
        for i in 0..30u64 {
            body.push_str(&format!("error[E{:04}]: mismatched types at line {}\n", i, i));
            screens.push((i * 1_000, body.clone()));
        }
        let refs: Vec<(u64, &str)> = screens.iter().map(|(t, s)| (*t, s.as_str())).collect();
        let s = samples_from(&refs, &c, false);
        assert!(detect_error_storm(&s, &c, 29_000).is_some());
    }

    #[test]
    fn japanese_error_lines_count() {
        let c = cfg();
        let mut body = String::new();
        let mut screens: Vec<(u64, String)> = Vec::new();
        for i in 0..30u64 {
            body.push_str(&format!("エラー: モジュール {i} の読み込みに失敗\n"));
            screens.push((i * 1_000, body.clone()));
        }
        let refs: Vec<(u64, &str)> = screens.iter().map(|(t, s)| (*t, s.as_str())).collect();
        let s = samples_from(&refs, &c, false);
        assert!(
            detect_error_storm(&s, &c, 29_000).is_some(),
            "日本語のエラー行も検出できるべき"
        );
    }

    #[test]
    fn zero_errors_summary_is_not_an_error() {
        // 偽陽性テスト: "0 errors" をエラー行にしない
        let c = cfg();
        assert!(!is_error_line(&normalize_line("build finished: 0 errors", false), &c));
        assert!(!is_error_line(&normalize_line("no errors found", false), &c));
        assert!(is_error_line(&normalize_line("error: boom", false), &c));
    }

    #[test]
    fn stable_error_screen_is_not_a_storm() {
        // 偽陽性テスト: 同じエラーが画面に残り続けるだけでは嵐ではない
        let c = cfg();
        let screens: Vec<(u64, &str)> =
            (0..60).map(|i| (i * 1_000u64, "error: one single failure")).collect();
        let s = samples_from(&screens, &c, false);
        assert!(
            detect_error_storm(&s, &c, 59_000).is_none(),
            "同一エラーの残留を嵐扱いしてはいけない"
        );
    }

    // ---------------- クラッシュ / 沈黙 / 暴走 ----------------

    #[test]
    fn crash_detected_only_when_active() {
        let c = cfg();
        assert!(detect_crash(false, Some(1), SessionState::Working, &c).is_some());
        assert!(detect_crash(false, Some(0), SessionState::Working, &c).is_none());
        assert!(detect_crash(true, None, SessionState::Working, &c).is_none());
        // 待機中に終了したのは異常終了ではない
        assert!(detect_crash(false, Some(1), SessionState::Idle, &c).is_none());
    }

    #[test]
    fn silent_wait_fires_and_user_reaction_clears_it() {
        let mut c = cfg();
        c.silent_wait_secs = 60;
        assert!(detect_silent_wait(Some(0), None, &c, 120_000).is_some());
        // 承認待ち開始より後にユーザーが触っていれば放置ではない
        assert!(detect_silent_wait(Some(0), Some(30_000), &c, 120_000).is_none());
        // まだしきい値未満
        assert!(detect_silent_wait(Some(0), None, &c, 30_000).is_none());
    }

    #[test]
    fn runaway_detected_and_normal_output_is_not() {
        let c = cfg();
        let mut s: Vec<Sample> = Vec::new();
        // 平常: 1KB/s を 60 秒
        for i in 0..60u64 {
            s.push(Sample {
                t_ms: i * 1_000,
                bytes_delta: 1_000,
                ..Default::default()
            });
        }
        assert!(
            detect_runaway(&s, &c, 59_000).is_none(),
            "平常出力を暴走にしてはいけない"
        );
        // 暴走: 500KB/s を 20 秒
        for i in 60..80u64 {
            s.push(Sample {
                t_ms: i * 1_000,
                bytes_delta: 500_000,
                ..Default::default()
            });
        }
        assert!(detect_runaway(&s, &c, 79_000).is_some());
    }

    // ---------------- 安全ゲート ----------------

    #[test]
    fn auto_answer_is_refused_under_ask() {
        let c = cfg();
        assert_eq!(
            gate(Intervention::AutoAnswer, Approval::Ask, &c),
            GateResult::Refuse("承認モードが「都度確認」のため自動応答しません")
        );
        assert_eq!(gate(Intervention::AutoAnswer, Approval::Auto, &c), GateResult::Allow);
        assert!(matches!(
            gate(Intervention::AutoAnswer, Approval::Agent, &c),
            GateResult::NeedConfirm(_)
        ));
    }

    #[test]
    fn nothing_above_notify_is_automatic_under_ask() {
        let mut c = cfg();
        // 設定で自動化を許しても Ask では自動発火させない
        c.allow_auto_restart = true;
        c.allow_auto_halt = true;
        for a in [
            Intervention::AutoAnswer,
            Intervention::Nudge,
            Intervention::Restart,
            Intervention::Halt,
        ] {
            assert!(
                !matches!(gate(a, Approval::Ask, &c), GateResult::Allow),
                "Ask で {a:?} を無確認で許してはいけない"
            );
        }
        // Notify 以下は常に通る
        assert_eq!(gate(Intervention::Notify, Approval::Ask, &c), GateResult::Allow);
        assert_eq!(gate(Intervention::Observe, Approval::Ask, &c), GateResult::Allow);
    }

    #[test]
    fn restart_and_halt_need_confirmation_by_default() {
        let c = cfg();
        assert!(!c.allow_auto_restart, "既定で自動再起動は無効であるべき");
        assert!(!c.allow_auto_halt, "既定で自動停止は無効であるべき");
        for ap in [Approval::Ask, Approval::Auto, Approval::Agent] {
            assert!(
                matches!(gate(Intervention::Restart, ap, &c), GateResult::NeedConfirm(_)),
                "既定では {} でも再起動は確認が要る",
                ap_name(ap)
            );
            assert!(matches!(
                gate(Intervention::Halt, ap, &c),
                GateResult::NeedConfirm(_)
            ));
        }
        // 明示的にオプトインし、かつ Ask でない場合のみ自動
        let mut c2 = cfg();
        c2.allow_auto_restart = true;
        assert_eq!(gate(Intervention::Restart, Approval::Auto, &c2), GateResult::Allow);
    }

    #[test]
    fn llm_escalation_is_off_by_default() {
        assert!(!cfg().llm_escalation, "LLM 相談は既定で OFF であるべき");
    }

    // ---------------- 統合 (Supervisor) ----------------

    fn snap(id: u64, text: &str, running: bool, waiting: bool) -> SessionSnapshot {
        SessionSnapshot {
            id,
            title: "claude".into(),
            screen_text: text.into(),
            running,
            waiting_approval: waiting,
            exit_code: None,
            user_typed: false,
            total_output_bytes: None,
        }
    }

    #[test]
    fn restart_does_not_auto_fire_on_crash_under_default_config() {
        let mut sv = Supervisor::new(cfg());
        // 作業中にする (数字だけの違いは正規化で潰れるので語そのものを変える)
        let steps = ["reading src/a.rs", "editing src/b.rs", "running tests", "linking", "checking"];
        for (i, step) in steps.iter().enumerate() {
            sv.tick_ms(&[snap(1, step, true, false)], Approval::Auto, i as u64 * 2_000);
        }
        assert_eq!(sv.state_of(1), Some(SessionState::Working));
        // 異常終了
        let mut s = snap(1, "checking", false, false);
        s.exit_code = Some(137);
        let intents = sv.tick_ms(&[s], Approval::Auto, 20_000);
        assert_eq!(sv.state_of(1), Some(SessionState::Crashed));
        let restart: Vec<_> = intents
            .iter()
            .filter(|i| i.action == Intervention::Restart)
            .collect();
        assert_eq!(restart.len(), 1, "再起動は提案として 1 件出るべき");
        assert!(
            restart[0].needs_confirmation,
            "既定設定で再起動が自動発火してはいけない"
        );
        assert!(intents.iter().any(|i| i.action == Intervention::Notify));
    }

    #[test]
    fn rate_limiting_suppresses_repeat_interventions() {
        let mut c = cfg();
        c.stall_secs = 10;
        c.notify_after_secs = 0;
        c.escalate_after_secs = 0;
        c.cooldown_notify_secs = 600;
        let mut sv = Supervisor::new(c);
        let mut total_notify = 0;
        for i in 0..60u64 {
            let out = sv.tick_ms(&[snap(1, "frozen screen", true, false)], Approval::Auto, i * 5_000);
            total_notify += out.iter().filter(|x| x.action == Intervention::Notify).count();
        }
        assert_eq!(
            total_notify, 1,
            "クールダウン中は通知が繰り返し出てはいけない (出た数: {total_notify})"
        );
    }

    #[test]
    fn auto_answer_never_emitted_under_ask() {
        let mut c = cfg();
        c.silent_wait_secs = 5;
        c.notify_after_secs = 0;
        c.escalate_after_secs = 0;
        let mut sv = Supervisor::new(c);
        let mut any_auto = false;
        for i in 0..40u64 {
            let out = sv.tick_ms(
                &[snap(1, "Do you want to proceed?\n❯ 1. Yes", true, true)],
                Approval::Ask,
                i * 5_000,
            );
            any_auto |= out.iter().any(|x| x.action == Intervention::AutoAnswer);
        }
        assert!(!any_auto, "Ask モードで自動応答を出してはいけない");
        assert_eq!(sv.state_of(1), Some(SessionState::WaitingApproval));
    }

    #[test]
    fn auto_answer_allowed_under_auto_mode() {
        let mut c = cfg();
        c.silent_wait_secs = 5;
        c.notify_after_secs = 0;
        c.escalate_after_secs = 0;
        let mut sv = Supervisor::new(c);
        let mut auto = Vec::new();
        for i in 0..40u64 {
            let out = sv.tick_ms(
                &[snap(1, "Do you want to proceed?\n❯ 1. Yes", true, true)],
                Approval::Auto,
                i * 5_000,
            );
            auto.extend(out.into_iter().filter(|x| x.action == Intervention::AutoAnswer));
        }
        assert!(!auto.is_empty(), "Auto モードでは自動応答が出るべき");
        assert!(!auto[0].needs_confirmation);
        assert_eq!(auto[0].payload.as_deref(), Some("\r"));
    }

    #[test]
    fn history_is_bounded() {
        let mut c = cfg();
        c.history_capacity = 5;
        c.sample_interval_ms = 0;
        let mut sv = Supervisor::new(c);
        for i in 0..100u64 {
            // 承認待ちと作業中を往復させて遷移を量産する
            let waiting = i % 2 == 0;
            sv.tick_ms(
                &[snap(1, &format!("line {i}"), true, waiting)],
                Approval::Auto,
                i * 1_000,
            );
        }
        assert!(sv.history_of(1).len() <= 5, "履歴は上限付きであるべき");
    }

    #[test]
    fn samples_are_bounded() {
        let mut c = cfg();
        c.sample_capacity = 10;
        c.sample_interval_ms = 0;
        let mut sv = Supervisor::new(c);
        for i in 0..500u64 {
            sv.tick_ms(&[snap(1, &format!("line {i}"), true, false)], Approval::Auto, i * 100);
        }
        let mon = sv.monitors.get(&1).unwrap();
        assert_eq!(mon.samples.len(), 10);
        assert!(mon.seen_lines.len() <= SEEN_LINES_CAP);
    }

    #[test]
    fn gone_sessions_are_forgotten() {
        let mut sv = Supervisor::new(cfg());
        sv.tick_ms(&[snap(1, "a", true, false), snap(2, "b", true, false)], Approval::Auto, 0);
        assert_eq!(sv.monitors.len(), 2);
        sv.tick_ms(&[snap(1, "a", true, false)], Approval::Auto, 2_000);
        assert_eq!(sv.monitors.len(), 1, "消えたセッションの監視状態は捨てるべき");
    }

    #[test]
    fn redaction_removes_secrets() {
        let out = redact("token sk-abcdefghijklmnopqrstuvwxyz012345 user a@b.com", 500);
        assert!(!out.contains("sk-abcdef"), "APIキーは秘匿されるべき: {out}");
        assert!(!out.contains("a@b.com"), "メールは秘匿されるべき: {out}");
    }

    #[test]
    fn config_roundtrips_through_toml() {
        let c = cfg();
        let s = toml::to_string(&c).expect("serialize");
        let back: SupervisorConfig = toml::from_str(&s).expect("deserialize");
        assert_eq!(back.stall_secs, c.stall_secs);
        assert!(!back.allow_auto_restart);
        assert!(!back.llm_escalation);
    }

    // ---------------- derive_state (状態導出の優先順位) ----------------

    /// derive_state に渡す異常リストを作るテストヘルパ。
    fn found(list: &[(Anomaly, &str)]) -> Vec<(Anomaly, String)> {
        list.iter().map(|(a, r)| (*a, r.to_string())).collect()
    }

    #[test]
    fn crash_anomaly_wins_over_exit_code() {
        let c = cfg();
        let mut s = snap(1, "", false, false);
        s.exit_code = Some(0);
        let f = found(&[(Anomaly::Crash, "作業中に落ちた")]);
        let (st, reason) = derive_state(SessionState::Working, &s, &[], &f, &c, 10_000);
        assert_eq!(
            st,
            SessionState::Crashed,
            "Crash 異常は exit_code より優先されるべき: {reason}"
        );
        // 同じ exit_code=0 でも Crash 異常が無ければ正常終了
        let (st2, _) = derive_state(SessionState::Working, &s, &[], &[], &c, 10_000);
        assert_eq!(st2, SessionState::Done);
    }

    #[test]
    fn nonzero_exit_code_without_crash_is_errored() {
        let c = cfg();
        let mut s = snap(1, "", false, false);
        s.exit_code = Some(3);
        let (st, reason) = derive_state(SessionState::Working, &s, &[], &[], &c, 10_000);
        assert_eq!(st, SessionState::Errored);
        assert_eq!(reason, "終了コード 3");
        // exit_code 無しの非 running は正常終了扱い
        s.exit_code = None;
        let (st2, _) = derive_state(SessionState::Working, &s, &[], &[], &c, 10_000);
        assert_eq!(st2, SessionState::Done);
    }

    #[test]
    fn waiting_approval_wins_over_loop_and_error_storm() {
        let c = cfg();
        let s = snap(1, "❯ 1. Yes", true, true);
        let f = found(&[
            (Anomaly::Looping, "ループ中"),
            (Anomaly::ErrorStorm, "エラー多発"),
        ]);
        let (st, reason) = derive_state(SessionState::Working, &s, &[], &f, &c, 10_000);
        assert_eq!(
            st,
            SessionState::WaitingApproval,
            "承認待ちは Loop/ErrorStorm より優先されるべき: {reason}"
        );
        assert_eq!(reason, "承認プロンプト検出");
    }

    #[test]
    fn anomaly_priority_is_loop_then_storm_then_stall() {
        let c = cfg();
        let s = snap(1, "working", true, false);
        // 3 つ同時なら Looping が勝つ (found の並び順には依存しない)
        let f = found(&[
            (Anomaly::Stall, "S"),
            (Anomaly::ErrorStorm, "E"),
            (Anomaly::Looping, "L"),
        ]);
        let (st, reason) = derive_state(SessionState::Working, &s, &[], &f, &c, 10_000);
        assert_eq!((st, reason.as_str()), (SessionState::Looping, "L"));
        // Looping が無ければ ErrorStorm
        let f = found(&[(Anomaly::Stall, "S"), (Anomaly::ErrorStorm, "E")]);
        let (st, reason) = derive_state(SessionState::Working, &s, &[], &f, &c, 10_000);
        assert_eq!((st, reason.as_str()), (SessionState::Errored, "E"));
        // Stall 単独なら Stalled
        let f = found(&[(Anomaly::Stall, "S")]);
        let (st, reason) = derive_state(SessionState::Working, &s, &[], &f, &c, 10_000);
        assert_eq!((st, reason.as_str()), (SessionState::Stalled, "S"));
    }

    #[test]
    fn recent_progress_means_working() {
        let c = cfg();
        let s = snap(1, "step three", true, false);
        let samples = samples_from(
            &[(0, "step one"), (2_000, "step two"), (4_000, "step three")],
            &c,
            false,
        );
        let (st, reason) = derive_state(SessionState::Idle, &s, &samples, &[], &c, 5_000);
        assert_eq!(st, SessionState::Working, "{reason}");
    }

    #[test]
    fn stale_progress_means_idle() {
        let c = cfg();
        let s = snap(1, "step two", true, false);
        // 進捗は 1 秒時点まで。既定の窓 (10 秒) の外なので待機扱いになる
        let samples = samples_from(
            &[
                (0, "step one"),
                (1_000, "step two"),
                (20_000, "step two"),
                (30_000, "step two"),
            ],
            &c,
            false,
        );
        let (st, reason) = derive_state(SessionState::Working, &s, &samples, &[], &c, 30_000);
        assert_eq!(st, SessionState::Idle, "{reason}");
        // サンプルが無い場合も待機
        let (st2, _) = derive_state(SessionState::Working, &s, &[], &[], &c, 30_000);
        assert_eq!(st2, SessionState::Idle);
    }

    // ---------------- LLM 診断 → 意図 (intent_from_diagnosis) ----------------

    fn diag(id: u64, anomaly: Anomaly, summary: &str, rec: Intervention) -> Diagnosis {
        Diagnosis {
            session_id: id,
            anomaly,
            summary: summary.into(),
            recommended: rec,
        }
    }

    #[test]
    fn diagnosis_is_ignored_when_llm_escalation_off() {
        let mut sv = Supervisor::new(cfg()); // 既定で llm_escalation=false
        sv.tick_ms(&[snap(1, "working", true, false)], Approval::Auto, 0);
        let d = diag(1, Anomaly::Stall, "止まっている", Intervention::Notify);
        assert!(
            sv.intent_from_diagnosis(&d, Approval::Auto).is_none(),
            "llm_escalation=false のとき診断は意図になってはいけない"
        );
    }

    #[test]
    fn diagnosis_for_unknown_session_is_none() {
        let mut c = cfg();
        c.llm_escalation = true;
        let mut sv = Supervisor::new(c);
        sv.tick_ms(&[snap(1, "working", true, false)], Approval::Auto, 0);
        let d = diag(99, Anomaly::Stall, "止まっている", Intervention::Notify);
        assert!(
            sv.intent_from_diagnosis(&d, Approval::Auto).is_none(),
            "未登録セッション id の診断は無視されるべき"
        );
    }

    #[test]
    fn destructive_llm_recommendation_always_needs_confirmation() {
        let mut c = cfg();
        c.llm_escalation = true;
        // 自動再起動・自動停止を明示的に許可していても、LLM 由来なら必ず確認
        c.allow_auto_restart = true;
        c.allow_auto_halt = true;
        let mut sv = Supervisor::new(c);
        sv.tick_ms(&[snap(1, "working", true, false)], Approval::Auto, 0);
        for rec in [Intervention::Restart, Intervention::Halt] {
            let d = diag(1, Anomaly::Crash, "再起動が必要", rec);
            let i = sv
                .intent_from_diagnosis(&d, Approval::Auto)
                .expect("意図は作られるべき");
            assert!(
                i.needs_confirmation,
                "LLM 由来の {} は設定に関わらず確認必須",
                rec.label()
            );
        }
    }

    #[test]
    fn diagnosis_summary_is_reflected_in_reason() {
        let mut c = cfg();
        c.llm_escalation = true;
        let mut sv = Supervisor::new(c);
        sv.tick_ms(&[snap(1, "working", true, false)], Approval::Auto, 0);
        let d = diag(
            1,
            Anomaly::Looping,
            "同じテストを 5 回やり直している",
            Intervention::Notify,
        );
        let i = sv
            .intent_from_diagnosis(&d, Approval::Ask)
            .expect("Notify は Ask でも許可される");
        assert_eq!(i.action, Intervention::Notify);
        assert!(!i.needs_confirmation);
        assert_eq!(i.reason, "AI 診断: 同じテストを 5 回やり直している");
        assert_eq!(i.session_id, 1);
    }
}
