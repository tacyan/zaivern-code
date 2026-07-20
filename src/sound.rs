//! 効果音プレイヤー — OS 標準のシステムサウンドを fire-and-forget で鳴らす。
//!
//! clawd-on-desk (デスクトップの clawd) がタスク完了・確認・エラー時に
//! 短い効果音を鳴らすための極小モジュール。外部クレートには依存せず、
//! OS 付属のコマンドを子プロセスとして spawn するだけ。
//!
//! - 非ブロッキング: 子プロセスを spawn したら wait せずに切り離す。
//!   stdin/stdout/stderr はすべて null に接続する。
//! - 失敗はすべて黙って無視する (コマンド不在・ファイル不在でも panic しない)。
//! - 種類ごとに 10 秒のクールダウンを持ち、同じ音の連打を防ぐ。
//!
//! プラットフォーム別の再生手段:
//! - macOS:   `afplay` + /System/Library/Sounds/*.aiff
//! - Linux:   `paplay` + freedesktop サウンドテーマ (無ければ spawn 失敗 → 無音)
//! - Windows: `powershell` + [System.Media.SystemSounds]

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// 同一種類の効果音を再び鳴らせるようになるまでのクールダウン時間。
const COOLDOWN: Duration = Duration::from_secs(10);

/// 効果音の種類。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum SoundKind {
    /// タスク完了 (macOS: Glass / Linux: complete / Windows: Asterisk)
    Complete,
    /// 確認・通知 (macOS: Ping / Linux: message / Windows: Beep)
    Confirm,
    /// エラー (macOS: Basso / Linux: dialog-error / Windows: Hand)
    Error,
}

/// 効果音プレイヤー。種類ごとのクールダウン管理を持つ。
#[derive(Default)]
pub struct SoundPlayer {
    /// 種類ごとの最終再生時刻。
    last_played: HashMap<SoundKind, Instant>,
}

impl SoundPlayer {
    /// 効果音を鳴らす。種類ごとに10秒クールダウン。
    /// 非ブロッキング (子プロセスを spawn して切り離す)。エラーはすべて無視。
    pub fn play(&mut self, kind: SoundKind) {
        if !self.should_play(kind, Instant::now()) {
            return;
        }
        spawn_sound(kind);
    }

    /// クールダウン判定。鳴らしてよければ最終再生時刻を `now` に更新して true。
    /// 前回再生から [`COOLDOWN`] 未満なら false (時刻は更新しない)。
    fn should_play(&mut self, kind: SoundKind, now: Instant) -> bool {
        if let Some(&last) = self.last_played.get(&kind) {
            // Instant::duration_since は now < last でも 0 に飽和する (panic しない)。
            if now.duration_since(last) < COOLDOWN {
                return false;
            }
        }
        self.last_played.insert(kind, now);
        true
    }
}

/// macOS: システムサウンドの AIFF を `afplay` で再生する。
#[cfg(target_os = "macos")]
fn spawn_sound(kind: SoundKind) {
    use std::process::{Command, Stdio};
    let path = match kind {
        SoundKind::Complete => "/System/Library/Sounds/Glass.aiff",
        SoundKind::Confirm => "/System/Library/Sounds/Ping.aiff",
        SoundKind::Error => "/System/Library/Sounds/Basso.aiff",
    };
    // spawn の結果 (Child) は保持せず即 drop = 切り離し。エラーも無視。
    let _ = Command::new("afplay")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Linux: freedesktop サウンドテーマを `paplay` (PulseAudio/PipeWire) で再生する。
/// paplay が無い環境では spawn が失敗するだけで、そのまま無音で続行する。
#[cfg(target_os = "linux")]
fn spawn_sound(kind: SoundKind) {
    use std::process::{Command, Stdio};
    let path = match kind {
        SoundKind::Complete => "/usr/share/sounds/freedesktop/stereo/complete.oga",
        SoundKind::Confirm => "/usr/share/sounds/freedesktop/stereo/message.oga",
        SoundKind::Error => "/usr/share/sounds/freedesktop/stereo/dialog-error.oga",
    };
    let _ = Command::new("paplay")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Windows: PowerShell 経由で [System.Media.SystemSounds] を再生する。
/// プロセス終了が早すぎると音が切れるため Start-Sleep で 500ms だけ生かす。
#[cfg(target_os = "windows")]
fn spawn_sound(kind: SoundKind) {
    use std::os::windows::process::CommandExt;
    use std::process::{Command, Stdio};
    let name = match kind {
        SoundKind::Complete => "Asterisk",
        SoundKind::Confirm => "Beep",
        SoundKind::Error => "Hand",
    };
    // CREATE_NO_WINDOW: GUI アプリからコンソール窓を出さないため。
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let script = format!(
        "[System.Media.SystemSounds]::{}.Play(); Start-Sleep -m 500",
        name
    );
    let _ = Command::new("powershell")
        .args(["-NoProfile", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .spawn();
}

/// その他の OS: 何もしない (無音)。
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn spawn_sound(_kind: SoundKind) {}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1回目は鳴り、10秒未満は抑制され、10秒経過で再び鳴る。
    #[test]
    fn cooldown_suppresses_within_10s() {
        let mut player = SoundPlayer::default();
        let t0 = Instant::now();

        // 初回は必ず鳴る。
        assert!(player.should_play(SoundKind::Complete, t0));
        // 直後・9.999秒後まではクールダウン中。
        assert!(!player.should_play(SoundKind::Complete, t0));
        assert!(!player.should_play(SoundKind::Complete, t0 + Duration::from_secs(5)));
        assert!(!player.should_play(SoundKind::Complete, t0 + Duration::from_millis(9_999)));
        // ちょうど10秒経過したら再び鳴る。
        assert!(player.should_play(SoundKind::Complete, t0 + Duration::from_secs(10)));
        // 再生した時点からクールダウンが再スタートする。
        assert!(!player.should_play(SoundKind::Complete, t0 + Duration::from_secs(15)));
    }

    /// クールダウンは種類ごとに独立している。
    #[test]
    fn cooldown_is_per_kind() {
        let mut player = SoundPlayer::default();
        let t0 = Instant::now();

        assert!(player.should_play(SoundKind::Complete, t0));
        // Complete がクールダウン中でも Confirm / Error は鳴る。
        assert!(player.should_play(SoundKind::Confirm, t0 + Duration::from_secs(1)));
        assert!(player.should_play(SoundKind::Error, t0 + Duration::from_secs(1)));
        // それぞれが独立にクールダウンする。
        assert!(!player.should_play(SoundKind::Confirm, t0 + Duration::from_secs(2)));
    }
}
