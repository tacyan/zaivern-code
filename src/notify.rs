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

/// Webhook へイベントを POST する (curl にシェルアウト、非同期・失敗は無視)。
///
/// 形式は URL のドメインから自動判別する:
/// - Slack (`hooks.slack.com`) / Discord (`discord.com` / `discordapp.com`)
///   → JSON。`text` と `content` の両キーを入れるので、どちらのサービスでも読める。
/// - それ以外 (ntfy のトピック URL など) → プレーンテキスト本文 + `Title:` ヘッダ
///   (ntfy の標準的な受け口。Title 非対応のサービスでも本文は届く)。
///
/// curl は macOS / Windows 10+ / ほとんどの Linux に同梱されている。
/// 無い環境では spawn が失敗して黙って何もしない (通知は常にベストエフォート)。
pub fn webhook(url: &str, title: &str, body: &str) {
    let url = url.trim();
    if url.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
        return;
    }
    let body = truncate_chars(body, MAX_BODY_CHARS);
    let is_json = url.contains("hooks.slack.com")
        || url.contains("discord.com/api/webhooks")
        || url.contains("discordapp.com/api/webhooks");
    let mut cmd = Command::new("curl");
    cmd.args(["-fsS", "-m", "10", "-o", if cfg!(windows) { "NUL" } else { "/dev/null" }]);
    if is_json {
        let payload = format!(
            "{{\"text\":{t},\"content\":{t}}}",
            t = json_string(&format!("{title}\n{body}"))
        );
        cmd.args(["-H", "Content-Type: application/json", "-d", &payload]);
    } else {
        // ntfy 形式: 本文はプレーンテキスト、タイトルはヘッダで渡す。
        // ヘッダは latin-1 しか通らない実装があるため、日本語タイトルは
        // ntfy の UTF-8 拡張 (RFC 2047 は使わず X-Title に生 UTF-8) に任せる。
        cmd.args(["-H", &format!("X-Title: {}", sanitize_header(title)), "-d", &body]);
    }
    cmd.arg(url);
    let _ = cmd.spawn();
}

/// JSON 文字列リテラルへのエスケープ(純関数)。
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// HTTP ヘッダ値に入れられない改行類を落とす(ヘッダインジェクション防止)。
fn sanitize_header(s: &str) -> String {
    s.chars().filter(|c| *c != '\r' && *c != '\n').collect()
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

    // ── Webhook ──────────────────────────────────────────────────────

    #[test]
    fn json_string_escapes_specials() {
        assert_eq!(json_string(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(json_string("line1\nline2"), "\"line1\\nline2\"");
        assert_eq!(json_string("タブ\tと制御\u{1}"), "\"タブ\\tと制御\\u0001\"");
    }

    #[test]
    fn header_strips_newlines() {
        // ヘッダインジェクション対策: 改行は落ちる
        assert_eq!(sanitize_header("題名\r\nX-Evil: 1"), "題名X-Evil: 1");
    }

    #[test]
    fn webhook_rejects_non_http_urls() {
        // URL でないもの・空は何もしない (spawn さえしない)。パニックしないことの確認。
        webhook("", "t", "b");
        webhook("ftp://example.com", "t", "b");
        webhook("javascript:alert(1)", "t", "b");
    }
}
