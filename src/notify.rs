//! OS ネイティブ通知モジュール。
//!
//! 依存クレートを使わず `std::process::Command` でシェルアウトする。
//! 通知は非同期(spawn のみ、wait しない)で送り、失敗はすべて無視する。

use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};

/// body の最大文字数(char 単位、マルチバイト安全)。
const MAX_BODY_CHARS: usize = 200;

/// 通知を出してよいか。**既定はオン**(設定を持たない版・欄の無い
/// `config.toml` から起動しても、いままでと同じ挙動になる)。
///
/// この層は依存を持たない設計なので、**設定を読む経路をここへ持ち込まない**。
/// 値を入れるのは設定を持っている側 (`config::apply_runtime_flags`) の仕事で、
/// ここは旗を見るだけにする。旗にするのは、通知の呼び出しが 16 箇所あり
/// **入口で止めないと必ず取りこぼす**ため (呼び出し側を 16 箇所直す形にすると、
/// 次に足された 17 箇所目が黙って鳴る)。
const ENABLED_DEFAULT: bool = true;
static ENABLED: AtomicBool = AtomicBool::new(ENABLED_DEFAULT);

/// 通知のオン/オフを切り替える。上位 (設定) から呼ぶ。
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

/// いま通知を出す設定か。
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 通知**音**を鳴らしてよいか。**既定はオン** (欄の無い `config.toml` から
/// 起動しても、いままでと同じ「通知オン・音あり」になる)。
///
/// **3 段 (オフ/無音/オン) ではなく独立した旗にした理由**: `feature::Setting`
/// が持てる型は Bool/Int/Float/Text だけで、3 段にすると設定画面が数値欄か
/// 自由入力になり、既存の `notifications.enabled` からの移行も要る。
/// 旗を 2 つに割ると生まれる「通知オフなのに音オン」という無意味な組は、
/// [`plan_args`] が**オフの時点で計画ごと `None`** を返すので観測できない
/// (= 組み合わせ爆発は型ではなく判定側で潰してある)。
const SOUND_DEFAULT: bool = true;
static SOUND: AtomicBool = AtomicBool::new(SOUND_DEFAULT);

/// 通知音のオン/オフを切り替える。上位 (設定) から呼ぶ。
pub fn set_sound(on: bool) {
    SOUND.store(on, Ordering::Relaxed);
}

/// いま通知音を鳴らす設定か。
pub fn sound() -> bool {
    SOUND.load(Ordering::Relaxed)
}

/// OS のネイティブ通知を非同期(spawn、wait しない)で送る。失敗は無視。
///
/// **オフのときは 1 プロセスも起こさない** (`osascript` / `notify-send` /
/// `powershell` のどれも spawn しない = 音も鳴らない)。
/// 通知は出したいが音は要らない場合は [`set_sound`] を false にする
/// (プロセスは起こすが、音の指定を組み立てない)。
pub fn notify(title: &str, body: &str) {
    if let Some(mut cmd) = notify_plan(title, body) {
        spawn_and_reap(&mut cmd);
    }
}

/// 通知 1 回で**起こすことになるコマンド**。オフなら `None`。
///
/// spawn する前にここで全部決まるので、テストは
/// **実際に通知を飛ばさずに**「1 プロセスも起こさない」ことを確かめられる
/// (`Command::get_program` で何を起こすつもりだったかまで見える)。
fn notify_plan(title: &str, body: &str) -> Option<Command> {
    plan_for(
        Prefs {
            on: enabled(),
            sound: sound(),
        },
        title,
        body,
    )
}

/// 通知の設定 2 つ。**位置引数の `bool` 2 つ**にすると呼び違えても型で
/// 気付けないので、名前つきの値で運ぶ。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Prefs {
    /// 通知そのものを出すか (`notifications.enabled`)。
    on: bool,
    /// 出すとき音を鳴らすか (`notifications.sound`)。
    /// `on` が false のときは意味を持たない (計画ごと消えるため)。
    sound: bool,
}

/// 通知を出す先の OS。`cfg!` は**実行中の OS の値しか返さない**ので、
/// 3 OS ぶんの計画を 1 台で表にして固定するためだけに切り出した型。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Target {
    MacOs,
    Linux,
    Windows,
    /// 通知の出し方を知らない OS。何も起こさない。
    Other,
}

/// いま動いている OS。`cfg!` は真偽値なので**どの分岐も型検査される**
/// (`#[cfg]` で片側を切り落とすと、その OS でしかコンパイルされない)。
fn host_target() -> Target {
    if cfg!(target_os = "macos") {
        Target::MacOs
    } else if cfg!(target_os = "linux") {
        Target::Linux
    } else if cfg!(target_os = "windows") {
        Target::Windows
    } else {
        Target::Other
    }
}

/// 通知 1 回で起こす **(プログラム, 引数)**。オフなら `None`。
///
/// `Command` を組む前の**純粋な値**にしてあるので、実際に通知を飛ばさずに
/// 「音の指定が入っているか」を文字列として検査できる。OS を引数で受けるのは
/// **手元の 1 台で 3 OS ぶんを表に固定する**ため。
fn plan_args(
    target: Target,
    p: Prefs,
    title: &str,
    body: &str,
) -> Option<(&'static str, Vec<String>)> {
    if !p.on {
        return None;
    }
    let body = truncate_chars(body, MAX_BODY_CHARS);
    match target {
        Target::MacOs => {
            // AppleScript 文字列リテラルに埋め込むためエスケープし、
            // 引数は配列渡し(シェル文字列連結しない)でインジェクションを防ぐ。
            //
            // 音は `sound name` 句の**有無**で決まる。無音側は句ごと落とす
            // (`sound name ""` は構文エラーになり、通知そのものが出ない)。
            let snd = if p.sound { " sound name \"Ping\"" } else { "" };
            let script = format!(
                "display notification \"{}\" with title \"{}\"{snd}",
                escape_applescript(&body),
                escape_applescript(title),
            );
            Some(("osascript", vec!["-e".to_string(), script]))
        }
        Target::Linux => {
            // notify-send が存在しなければ spawn が Err になるだけで、黙って何もしない。
            //
            // **音について正直に書く**: `notify-send` 自体に音の概念は無い。
            // 鳴らしているのは通知デーモンで、freedesktop の通知仕様にある
            // `suppress-sound` ヒント (boolean) を立てると抑制**できることに
            // なっている**。ただし尊重するかはデーモン次第で、
            // **効かない環境が残る** (仕様上ヒントは任意)。
            // 確実に消したいからといって別のコマンドを起こしたりはしない。
            let mut args: Vec<String> = Vec::new();
            if !p.sound {
                args.push("-h".to_string());
                args.push("boolean:suppress-sound:true".to_string());
            }
            args.push(title.to_string());
            args.push(body);
            Some(("notify-send", args))
        }
        Target::Windows => {
            // ベストエフォート: PowerShell の WinRT トースト通知。
            // シングルクォート文字列に埋め込むため ' を '' に二重化する。
            //
            // 音はテンプレートの `<toast>` へ `<audio silent="true"/>` を
            // 1 要素足すと止まる。音ありのときは**要素を足さない** =
            // 既定音のまま (従来の組み立てと 1 バイトも変わらない)。
            let silence = if p.sound {
                ""
            } else {
                "$a = $t.CreateElement('audio'); $a.SetAttribute('silent','true'); $t.DocumentElement.AppendChild($a) | Out-Null;"
            };
            let script = format!(
                concat!(
                    "[Windows.UI.Notifications.ToastNotificationManager, Windows.UI.Notifications, ContentType = WindowsRuntime] | Out-Null;",
                    "$t = [Windows.UI.Notifications.ToastNotificationManager]::GetTemplateContent([Windows.UI.Notifications.ToastTemplateType]::ToastText02);",
                    "$n = $t.GetElementsByTagName('text');",
                    "$n.Item(0).AppendChild($t.CreateTextNode('{title}')) | Out-Null;",
                    "$n.Item(1).AppendChild($t.CreateTextNode('{body}')) | Out-Null;",
                    "{silence}",
                    "[Windows.UI.Notifications.ToastNotificationManager]::CreateToastNotifier('Zaivern Code').Show([Windows.UI.Notifications.ToastNotification]::new($t))",
                ),
                title = escape_powershell_single_quoted(title),
                body = escape_powershell_single_quoted(&body),
                silence = silence,
            );
            Some((
                "powershell",
                vec![
                    "-NoProfile".to_string(),
                    "-NonInteractive".to_string(),
                    "-WindowStyle".to_string(),
                    "Hidden".to_string(),
                    "-Command".to_string(),
                    script,
                ],
            ))
        }
        Target::Other => None,
    }
}

/// [`plan_args`] を実行中の OS 向けに `Command` へ組む。
/// **旗を引数で受ける純粋な形**にしてあるのは、プロセス共通の旗を書き換えずに
/// 検査するため (書き換えると、同時に走っている他のテストの `config::load` が
/// 旗を戻して偽の赤が出る)。
fn plan_for(p: Prefs, title: &str, body: &str) -> Option<Command> {
    let (prog, args) = plan_args(host_target(), p, title, body)?;
    let mut c = Command::new(prog);
    c.args(args);
    Some(c)
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
///
/// **オン/オフは OS 通知と同じ旗で決める。** 設計原則 5
/// 「ハンドラは 1 面、トランスポートは多数」— 鳴らすかどうかの**判断は 1 つ**で、
/// デスクトップと webhook はその配り先の違いでしかない。旗を 2 つに割ると
/// 「通知を切ったのに Slack だけ鳴り続ける」が起きる。
/// webhook だけ止めたい場合は URL を空にすればよい (既に別の切り口がある)。
pub fn webhook(url: &str, title: &str, body: &str) {
    if let Some(mut cmd) = webhook_plan(url, title, body) {
        spawn_and_reap(&mut cmd);
    }
}

/// webhook 1 回で**起こすことになるコマンド**。オフ / URL 不正なら `None`。
/// 切り出す理由は [`notify_plan`] と同じ (spawn せずに検査できる)。
fn webhook_plan(url: &str, title: &str, body: &str) -> Option<Command> {
    webhook_plan_for(enabled(), url, title, body)
}

/// [`webhook_plan`] の実体。旗を引数で受ける理由は [`plan_for`] と同じ。
fn webhook_plan_for(on: bool, url: &str, title: &str, body: &str) -> Option<Command> {
    if !on {
        return None;
    }
    let url = url.trim();
    if url.is_empty() || !(url.starts_with("https://") || url.starts_with("http://")) {
        return None;
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
    Some(cmd)
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

    /// いま覚えている段。**画面に出している場所は無い**ので、
    /// モジュール全体の `allow(dead_code)` を外した機に検査用へ絞った
    /// (表示に使いたくなったら `cfg(test)` を外して呼び出し側を足すこと)。
    #[cfg(test)]
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
        assert!(webhook_plan_for(true, "", "t", "b").is_none());
        assert!(webhook_plan_for(true, "ftp://example.com", "t", "b").is_none());
        assert!(webhook_plan_for(true, "javascript:alert(1)", "t", "b").is_none());
    }

    // ── 通知のオン/オフ ──────────────────────────────────────────────
    //
    // 旗はプロセス共通なので、**この 1 本の中で完結させて必ず戻す**
    // (同時に走っている他のテストへ漏らさない)。
    // 検査は `*_plan` に対して行うので、**実際の通知は 1 度も飛ばない**。

    #[test]
    fn 通知がオフのときはosのコマンドを1つも起こさない() {
        // 旗を書き換えずに、旗の値を引数で与えて検査する
        // (**実際の通知は 1 度も飛ばない**し、他のテストとも干渉しない)。
        assert!(
            plan_for(
                Prefs {
                    on: false,
                    sound: true
                },
                "題名",
                "本文"
            )
            .is_none(),
            "オフなのに OS 通知のプロセスを起こそうとした (macOS は音が鳴る)"
        );
        assert!(
            webhook_plan_for(false, "https://hooks.slack.com/services/x", "題名", "本文").is_none(),
            "オフなのに webhook を送ろうとした"
        );
    }

    #[test]
    fn 通知がオンなら従来どおりosのコマンドを起こす() {
        let plan = plan_for(
            Prefs {
                on: true,
                sound: true,
            },
            "題名",
            "本文",
        );
        if cfg!(any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        )) {
            let p = plan.expect("オンなのに何も起こさない");
            let prog = p.get_program().to_string_lossy().into_owned();
            assert!(
                ["osascript", "notify-send", "powershell"].contains(&prog.as_str()),
                "知らないコマンドを起こそうとした: {prog}"
            );
        } else {
            assert!(plan.is_none(), "対応していない OS では何も起こさない");
        }
        let w = webhook_plan_for(true, "https://hooks.slack.com/services/x", "題名", "本文")
            .expect("オンなら webhook は出る");
        assert_eq!(w.get_program(), "curl");
    }

    // ── 通知音のオン/オフ ────────────────────────────────────────────
    //
    // 検査はすべて `plan_args` / `plan_for` に対して行うので、
    // **この節で実際の通知は 1 度も飛ばない**(音も鳴らない)。

    /// 3 OS ぶんの計画を表で固定する。`cfg!` は実行中の OS しか見ないので、
    /// 手元の 1 台で 3 OS 全部を検査できるのは [`plan_args`] が
    /// OS を引数で受けるため。
    #[test]
    fn 三つのosの通知計画を表で固定する() {
        // (OS, 音, 起こすプログラム, 必ず含む語, 絶対に含まない語)
        let cases: &[(Target, bool, &str, &[&str], &[&str])] = &[
            (
                Target::MacOs,
                true,
                "osascript",
                &["display notification", "sound name \"Ping\""],
                &[],
            ),
            (
                Target::MacOs,
                false,
                "osascript",
                &["display notification", "with title"],
                // 無音側は `sound name` 句ごと落ちる
                &["sound"],
            ),
            (
                Target::Linux,
                true,
                "notify-send",
                &["題名", "本文"],
                &["-h", "suppress-sound"],
            ),
            (
                Target::Linux,
                false,
                "notify-send",
                &["-h", "boolean:suppress-sound:true", "題名", "本文"],
                &[],
            ),
            (
                Target::Windows,
                true,
                "powershell",
                &["ToastNotificationManager", "-Command"],
                // 音ありのときは audio 要素そのものを足さない
                &["audio", "silent"],
            ),
            (
                Target::Windows,
                false,
                "powershell",
                &["CreateElement('audio')", "SetAttribute('silent','true')"],
                &[],
            ),
        ];
        for (target, snd, prog, must, must_not) in cases {
            let (got_prog, args) = plan_args(
                *target,
                Prefs {
                    on: true,
                    sound: *snd,
                },
                "題名",
                "本文",
            )
            .unwrap_or_else(|| panic!("{target:?} で計画が出ない"));
            assert_eq!(got_prog, *prog, "{target:?} が起こすコマンドが違う");
            let joined = args.join(" ");
            for m in *must {
                assert!(
                    joined.contains(m),
                    "{target:?} sound={snd}: {m} が無い\n{joined}"
                );
            }
            for m in *must_not {
                assert!(
                    !joined.contains(m),
                    "{target:?} sound={snd}: {m} が残っている\n{joined}"
                );
            }
        }
        // 通知の出し方を知らない OS では、音の設定に関係なく何も起こさない
        for snd in [true, false] {
            assert!(plan_args(
                Target::Other,
                Prefs {
                    on: true,
                    sound: snd
                },
                "題名",
                "本文"
            )
            .is_none());
        }
        // 通知がオフなら OS に関係なく計画そのものが無い
        // (= 「通知オフなのに音オン」という組は観測できない)
        for t in [Target::MacOs, Target::Linux, Target::Windows, Target::Other] {
            assert!(
                plan_args(
                    t,
                    Prefs {
                        on: false,
                        sound: true
                    },
                    "題名",
                    "本文"
                )
                .is_none(),
                "{t:?}: オフなのに計画が出た"
            );
        }
    }

    /// 実行中の OS の経路 (`plan_for`) でも音の指定が消えること。
    /// 表は `plan_args` を直に突くので、`Prefs` → `Command` の配線が
    /// 抜けていても気付けない。**その配線を見るのがこのテスト**。
    #[test]
    fn 音を切ると組み立てるコマンドから音の指定が消える() {
        let build = |snd: bool| {
            plan_for(
                Prefs {
                    on: true,
                    sound: snd,
                },
                "題名",
                "本文",
            )
            .map(|c| {
                c.get_args()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
        };
        let Some(quiet) = build(false) else {
            // 通知の出し方を知らない OS。3 OS ぶんは上の表で固定済み。
            return;
        };
        for marker in ["sound name", "silent','false", "suppress-sound:false"] {
            assert!(
                !quiet.contains(marker),
                "無音のはずが {marker} が残っている:\n{quiet}"
            );
        }
        let loud = build(true).expect("音ありの計画が出ない");
        assert_ne!(
            quiet, loud,
            "音の設定を変えても組み立てが同じ = 旗が繋がっていない"
        );
    }

    /// 旗の既定が「音あり」であること。設定を持たない版から更新した利用者が
    /// **黙って無音になる**のを防ぐ (旗はプロセス共通なので値は読まない —
    /// 見るのは定数のほうで、同時に走る他のテストと取り合わない)。
    #[test]
    fn 通知音の既定は音あり() {
        assert!(SOUND_DEFAULT, "既定が無音に倒れている");
        assert!(ENABLED_DEFAULT, "既定が通知オフに倒れている");
    }
}
