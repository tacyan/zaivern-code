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
        spawn_and_reap(Command::new("osascript").args(["-e", &script]));
    } else if cfg!(target_os = "linux") {
        // notify-send が存在しなければ spawn が Err になるだけで、黙って何もしない。
        spawn_and_reap(Command::new("notify-send").args([title, &body]));
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
        spawn_and_reap(Command::new("powershell").args([
            "-NoProfile",
            "-NonInteractive",
            "-WindowStyle",
            "Hidden",
            "-Command",
            &script,
        ]));
    }
}

/// spawn した子を別スレッドで wait して回収する。Child を即 drop すると
/// Unix では reap されず、通知 1 回ごとにゾンビが 1 個アプリ終了まで残る
/// (長時間セッションでプロセス数上限に達すると PTY 起動まで失敗し得る)。
fn spawn_and_reap(cmd: &mut Command) {
    // Windows: GUI アプリからコンソールアプリを起動しても窓を出さない
    // (powershell の -WindowStyle Hidden だけでは一瞬窓が出る)
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    if let Ok(mut child) = cmd.spawn() {
        let _ = std::thread::Builder::new()
            .name("zv-notify-reap".into())
            .spawn(move || {
                let _ = child.wait();
            });
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
    cmd.args([
        "-fsS",
        "-m",
        "10",
        "-o",
        if cfg!(windows) { "NUL" } else { "/dev/null" },
    ]);
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
        cmd.args([
            "-H",
            &format!("X-Title: {}", sanitize_header(title)),
            "-d",
            &body,
        ]);
    }
    cmd.arg(url);
    spawn_and_reap(&mut cmd);
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

// ---------------------------------------------------------------------------
// 通知の「遷移エッジ」門番
// ---------------------------------------------------------------------------
//
// 競合実装 (orca) は通知スパムを未修正バグとして抱えている。原因は
// 「状態が続いている間ずっと鳴らす」設計で、ここはそれを構造的に禁じる。
// **鳴らしてよいのは状態が変わった瞬間だけ**。

use std::collections::HashMap;

/// セッションの「稼働の段」。通知の遷移判定にだけ使う粗い 3 値。
///
/// 承認待ちやレート制限のような**要対応イベント**はここへ写さない
/// (`None` を渡す) — それらは遷移ではなく事件なので、専用の通知が持つ。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum WorkPhase {
    /// まだ観測できていない (起動直後)。ここからは**絶対に鳴らさない**。
    #[default]
    Unknown,
    /// 動いている
    Working,
    /// 生きているが手が空いている (プロンプトへ戻っている)
    Idle,
}

/// 「稼働中 → 待機」の**遷移した瞬間 1 回だけ**を通す門番。
///
/// * 起動直後 (`Unknown` → `Idle`) では鳴らない — 誤爆防止。
/// * `Idle` が続く間は鳴らない。
/// * `Idle` → `Working` → `Idle` なら 2 回鳴る。
/// * 段が曖昧なフレーム (承認待ち等) は [`WorkGate::note`] に `None` を渡す。
///   **前の段を保ったまま**素通しするので、
///   「作業中 → 承認待ち → 待機」でもちゃんと 1 回鳴る。
#[derive(Default)]
pub struct WorkGate {
    seen: HashMap<u64, WorkPhase>,
}

impl WorkGate {
    /// セッション 1 本の今の段を入れて「今 鳴らすべきか」を返す。
    pub fn note(&mut self, id: u64, now: Option<WorkPhase>) -> bool {
        let Some(now) = now.filter(|p| *p != WorkPhase::Unknown) else {
            return false; // 曖昧なフレームは段を動かさない
        };
        let prev = self.seen.insert(id, now).unwrap_or_default();
        prev == WorkPhase::Working && now == WorkPhase::Idle
    }

    /// 生きているセッションだけ残す (終了したセッションの段は忘れる)。
    pub fn retain(&mut self, alive: &[u64]) {
        self.seen.retain(|id, _| alive.contains(id));
    }

    /// 1 本だけ忘れる。「未読に戻す」= 後回し宣言のとき、
    /// 次に待機へ戻ったら**もう一度**鳴らしてほしいので段を捨てる。
    pub fn forget(&mut self, id: u64) {
        self.seen.remove(&id);
    }

    /// いま覚えている段 (テストと表示用)。
    pub fn phase(&self, id: u64) -> WorkPhase {
        self.seen.get(&id).copied().unwrap_or_default()
    }
}

/// 「同じ内容が続く間は鳴らさない」汎用の門番。
///
/// [`WorkGate`] と違い**初回は鳴る** — 新しい異常は news だが、
/// 同じ異常の再掲は news ではない、という区別。
#[derive(Default)]
pub struct EdgeGate {
    seen: HashMap<u64, String>,
}

impl EdgeGate {
    /// 直前と違う内容のときだけ true。
    pub fn changed(&mut self, id: u64, now: &str) -> bool {
        match self.seen.get(&id) {
            Some(prev) if prev == now => false,
            _ => {
                self.seen.insert(id, now.to_string());
                true
            }
        }
    }

    /// 生きているセッションだけ残す。
    pub fn retain(&mut self, alive: &[u64]) {
        self.seen.retain(|id, _| alive.contains(id));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 通知の遷移判定 ──────────────────────────────────────────
    // 「働いていたものが手を止めた瞬間」だけ 1 回。

    #[test]
    fn 起動直後の初期状態では鳴らない() {
        let mut g = WorkGate::default();
        // 最初の観測が待機でも鳴らさない (Unknown → Idle)
        assert!(!g.note(1, Some(WorkPhase::Idle)));
        // 段が取れないフレームも同じ
        let mut g2 = WorkGate::default();
        assert!(!g2.note(1, None));
        assert!(!g2.note(1, Some(WorkPhase::Unknown)));
    }

    #[test]
    fn 稼働中から待機への遷移で1回だけ鳴る() {
        let mut g = WorkGate::default();
        assert!(!g.note(1, Some(WorkPhase::Working)));
        assert!(g.note(1, Some(WorkPhase::Idle)), "遷移で鳴らなかった");
        assert!(!g.note(1, Some(WorkPhase::Idle)), "待機のまま鳴り続けた");
        assert!(!g.note(1, Some(WorkPhase::Idle)));
    }

    #[test]
    fn 待機のまま続いても鳴らない() {
        let mut g = WorkGate::default();
        for _ in 0..50 {
            assert!(!g.note(7, Some(WorkPhase::Idle)));
        }
    }

    #[test]
    fn 待機から稼働へ戻ってまた待機なら2回鳴る() {
        let mut g = WorkGate::default();
        g.note(1, Some(WorkPhase::Working));
        assert!(g.note(1, Some(WorkPhase::Idle)));
        assert!(!g.note(1, Some(WorkPhase::Working)));
        assert!(g.note(1, Some(WorkPhase::Idle)));
    }

    #[test]
    fn 曖昧なフレームは段を動かさない() {
        let mut g = WorkGate::default();
        g.note(1, Some(WorkPhase::Working));
        // 承認待ち (= 段が曖昧) を挟んでも「作業中」を覚えたまま
        assert!(!g.note(1, None));
        assert!(!g.note(1, None));
        assert!(
            g.note(1, Some(WorkPhase::Idle)),
            "承認を挟むと鳴らなくなった"
        );
    }

    #[test]
    fn セッションごとに独立している() {
        let mut g = WorkGate::default();
        g.note(1, Some(WorkPhase::Working));
        g.note(2, Some(WorkPhase::Working));
        assert!(g.note(1, Some(WorkPhase::Idle)));
        assert!(!g.note(1, Some(WorkPhase::Idle)));
        assert!(g.note(2, Some(WorkPhase::Idle)));
    }

    #[test]
    fn 終了したセッションの段は忘れる() {
        let mut g = WorkGate::default();
        g.note(1, Some(WorkPhase::Working));
        g.note(2, Some(WorkPhase::Working));
        g.retain(&[2]);
        assert_eq!(g.phase(1), WorkPhase::Unknown);
        assert_eq!(g.phase(2), WorkPhase::Working);
        // 忘れた相手は「初回」に戻るので、次の待機では鳴らない
        assert!(!g.note(1, Some(WorkPhase::Idle)));
    }

    #[test]
    fn 未読に戻すと次の待機でもう一度鳴る() {
        let mut g = WorkGate::default();
        g.note(5, Some(WorkPhase::Working));
        assert!(g.note(5, Some(WorkPhase::Idle)));
        // 「あとで見る」= 段を捨てる → 次に働いて止まればまた鳴る
        g.forget(5);
        assert!(!g.note(5, Some(WorkPhase::Working)));
        assert!(g.note(5, Some(WorkPhase::Idle)));
    }

    #[test]
    fn 同じ内容が続く間は鳴らない汎用門番() {
        let mut g = EdgeGate::default();
        assert!(g.changed(1, "停滞しています"), "初回は鳴ってよい");
        assert!(!g.changed(1, "停滞しています"));
        assert!(g.changed(1, "ループしています"));
        assert!(g.changed(2, "停滞しています"), "別セッションは別勘定");
        g.retain(&[2]);
        assert!(g.changed(1, "ループしています"), "忘れた相手は初回に戻る");
    }

    #[test]
    fn escape_applescript_quotes() {
        assert_eq!(
            escape_applescript(r#"say "hello" now"#),
            r#"say \"hello\" now"#
        );
    }

    #[test]
    fn escape_applescript_backslash() {
        assert_eq!(escape_applescript(r"C:\path\to"), r"C:\\path\\to");
        // バックスラッシュ→引用符の順でも二重エスケープにならないこと
        assert_eq!(escape_applescript("\\\""), "\\\\\\\"");
    }

    #[test]
    fn escape_applescript_japanese_passthrough() {
        assert_eq!(
            escape_applescript("テスト通知です。改行なし"),
            "テスト通知です。改行なし"
        );
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
        assert_eq!(
            escape_powershell_single_quoted("it's a 'test'"),
            "it''s a ''test''"
        );
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
