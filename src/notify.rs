//! OS ネイティブ通知モジュール。
//!
//! 依存クレートを使わず `std::process::Command` でシェルアウトする。
//! 通知は非同期(spawn のみ、wait しない)で送り、失敗はすべて無視する。

#![allow(dead_code)]

use std::process::Command;

/// body の最大文字数(char 単位、マルチバイト安全)。
const MAX_BODY_CHARS: usize = 200;

/// OS のネイティブ通知を非同期(spawn、wait しない)で送る。失敗は無視。
pub fn notify(title: &str, body: &str) {
    let body = truncate_chars(body, MAX_BODY_CHARS);

    if cfg!(target_os = "macos") {
        // AppleScript 文字列リテラルに埋め込むためエスケープし、
        // 引数は配列渡し(シェル文字列連結しない)でインジェクションを防ぐ。
        let script = format!(
            "display notification \"{}\" with title \"{}\" sound name \"Ping\"",
            escape_applescript(&body),
            escape_applescript(title),
        );
        let _ = Command::new("osascript").args(["-e", &script]).spawn();
    } else if cfg!(target_os = "linux") {
        // notify-send が存在しなければ spawn が Err になるだけで、黙って何もしない。
        let _ = Command::new("notify-send").args([title, &body]).spawn();
    } else if cfg!(target_os = "windows") {
        // ベストエフォート: PowerShell の WinRT トースト通知。
        // シングルクォート文字列に埋め込むため ' を '' に二重化する。
        let script = format!(
            concat!(
                "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;",
                "$t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02);",
                "$n = $t.GetElementsByTagName('text');",
                "$n.Item(0).AppendChild($t.CreateTextNode('{title}')) | Out-Null;",
                "$n.Item(1).AppendChild($t.CreateTextNode('{body}')) | Out-Null;",
                "[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Zaivern Code').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
            ),
            title = escape_powershell_single_quoted(title),
            body = escape_powershell_single_quoted(&body),
        );
        let _ = Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-WindowStyle", "Hidden", "-Command", &script])
            .spawn();
    }
}

/// AppleScript の二重引用符リテラル用エスケープ(純関数)。
/// `\` → `\\`、`"` → `\"`。char 単位で処理するためマルチバイト安全。
fn escape_applescript(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(c),
        }
    }
    out
}

/// PowerShell シングルクォート文字列用エスケープ(純関数)。`'` → `''`。
fn escape_powershell_single_quoted(s: &str) -> String {
    s.replace('\'', "''")
}

/// 先頭 `max` 文字まで切り詰める(char 単位、マルチバイト安全)。
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_applescript_quotes() {
        assert_eq!(escape_applescript(r#"say "hello" now"#), r#"say \"hello\" now"#);
    }

    #[test]
    fn escape_applescript_backslash() {
        assert_eq!(escape_applescript(r"C:\path\to"), r"C:\\path\\to");
        // バックスラッシュ→引用符の順でも二重エスケープにならないこと
        assert_eq!(escape_applescript("\\\""), "\\\\\\\"");
    }

    #[test]
    fn escape_applescript_japanese_passthrough() {
        assert_eq!(escape_applescript("テスト通知です。改行なし"), "テスト通知です。改行なし");
    }

    #[test]
    fn escape_applescript_emoji_passthrough() {
        assert_eq!(escape_applescript("完了 🚀✨👍"), "完了 🚀✨👍");
    }

    #[test]
    fn escape_applescript_mixed_injection_attempt() {
        assert_eq!(
            escape_applescript(r#"日本語"引用\パス🚀"#),
            "日本語\\\"引用\\\\パス🚀"
        );
    }

    #[test]
    fn truncate_chars_multibyte_safe() {
        let long = "あ🚀".repeat(300); // 600 chars
        let t = truncate_chars(&long, 200);
        assert_eq!(t.chars().count(), 200);
        // char 境界で切れている(パニックしない・不正 UTF-8 にならない)こと
        assert!(t.is_char_boundary(t.len()));
    }

    #[test]
    fn truncate_chars_short_string_unchanged() {
        assert_eq!(truncate_chars("short", 200), "short");
        assert_eq!(truncate_chars("", 200), "");
    }

    #[test]
    fn escape_powershell_single_quotes() {
        assert_eq!(escape_powershell_single_quoted("it's a 'test'"), "it''s a ''test''");
    }
}
