//! **エージェントへ送った指示を「必ず 1 ターンとして実行させる」送信手順。**
//!
//! ## なぜ専用のモジュールが要るのか
//!
//! 以前は送信地点ごとに `format!("{text}\r")` を組み立てて PTY へ 1 回で
//! 書いていた (Cockpit の指名送信・一斉送信・かんばん・リモート・失敗切替の
//! 引き継ぎで合計 6 箇所)。これには**エージェントによっては指示が実行されない**
//! という致命的な欠陥がある。
//!
//! Claude Code / Codex / Gemini CLI はいずれも Ink (React for CLI) 製で、
//! stdin を「短時間にまとまって届いたバイト列 = ペースト」として扱う。
//! 本文と確定用の CR が **同じ 1 回の write** で届くと、CR まで込みで
//! ペーストと判定され、**CR が改行として入力欄に入るだけで送信されない**。
//! 症状は「送ったのに、エージェントが入力欄に文字を抱えたまま待機している」。
//! 素のシェルは 1 回の write でも動くため、**エージェントによって効いたり
//! 効かなかったりする**という見え方になっていた。
//!
//! ## 対策 (この順序が全部必要)
//!
//! 1. **本文と確定キーを別々の write に分ける。** 間に [`COMMIT_DELAY`] を置いて
//!    相手が本文を消化し終えてから CR を送る。これが本丸。
//! 2. **相手が承認プロンプトで止まっている間は送らない。** 送ると本文ではなく
//!    「承認への回答」になってしまう (既定が "No, exit" のプロンプトもある)。
//! 3. **bracketed paste が有効なら本文をそれで包む。** 複数行の指示が途中の改行で
//!    分割送信されるのを防ぐ。
//! 4. **入力欄に本文が残っていたら CR を撃ち直す。** 1 と 2 で大半は消えるが、
//!    起動直後の再描画と重なると 1 発目が落ちることがある。ただし撃ち直しは
//!    **承認プロンプトが出ていないときだけ** ([`MAX_COMMIT_TRIES`] 回まで)。
//!
//! ## 判断はすべて純粋関数
//!
//! [`decide`] は時刻も PTY も知らない。呼び出し側が集めた [`Peek`] と
//! 経過時間だけで次の一手を返すので、実 PTY 無しでテーブルテストできる。

use std::time::{Duration, Instant};

/// 本文を書いてから確定キー (CR) を送るまでの待ち。
///
/// Ink の貼り付け判定は「直前の chunk から数十 ms 以内」で走る。ここを
/// 短くすると本文と CR が同じペーストへ吸われて**送信されない**。
/// 逆に長くしても体感は変わらない (人間の反応より速い) ので、
/// 安全側に倒して 120ms を採る。
pub const COMMIT_DELAY: Duration = Duration::from_millis(120);

/// 確定キーを送ってから「入力欄が空になったか」を確かめるまでの待ち。
pub const VERIFY_DELAY: Duration = Duration::from_millis(450);

/// 確定キーを送る最大回数 (初回 + 撃ち直し)。
///
/// 無制限に撃つと、相手がたまたま承認プロンプトを出した瞬間に
/// Enter を送り続けて**勝手に承認してしまう**。上限で必ず止める。
pub const MAX_COMMIT_TRIES: u8 = 3;

/// 「落ち着くまで待つ」指示 (Issue 着手 / レース / 失敗切替の引き継ぎ) が
/// Idle を待つ上限。これを過ぎたら承認待ちでない限り入れてしまう
/// (スピナーの誤検知で永遠に待たないため)。
pub const READY_WAIT: Duration = Duration::from_secs(45);

/// 諦める上限。ここを過ぎたらユーザーへ知らせて捨てる。
pub const GIVE_UP: Duration = Duration::from_secs(120);

/// 送れない状態のときに次を見に行く間隔。
///
/// アイドル時のコストをゼロに保つため、呼び出し側はこの間隔で
/// `request_repaint_after` する (常時再描画はしない)。
pub const POLL: Duration = Duration::from_millis(150);

/// PTY へ送る確定キー。CR (`\r`) — LF ではない。
///
/// 端末の行入力は CR で確定する。`\n` を送るとアプリによっては
/// 「改行の挿入」として扱われ、やはり送信されない。
pub const COMMIT: &[u8] = b"\r";

/// bracketed paste の開始/終了シーケンス。
const PASTE_BEGIN: &[u8] = b"\x1b[200~";
const PASTE_END: &[u8] = b"\x1b[201~";

/// 入力欄の残留判定に使う「本文の末尾」の文字数。
///
/// 先頭ではなく末尾を見るのは、入力欄が狭いと先頭が折り返しで隠れる一方、
/// カーソル手前 (= 末尾) は必ず見えているため。
const TAIL_CHARS: usize = 24;

// ── 本文の整形 ───────────────────────────────────────────────────────

/// PTY へ流してよい形へ整える (純関数)。
///
/// - `\r\n` / `\r` は `\n` へ揃える (CRLF がそのまま行くと二重確定になる)
/// - タブと改行以外の制御文字は落とす (ESC が混ざると端末の状態を壊す)
/// - 末尾の空白と改行は落とす (末尾の改行は「余計な 1 回の確定」になる)
pub fn sanitize(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                // CRLF は 1 個の LF へ畳む
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(ch),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    while out.ends_with([' ', '\t', '\n']) {
        out.pop();
    }
    out
}

/// 本文として PTY へ書くバイト列 (純関数)。
///
/// `bracketed` が真なら bracketed paste で包む — 相手が「これは 1 回の
/// 貼り付けである」と解釈するので、**本文中の改行で分割送信されない**。
/// 偽のときは素の端末へ貼ったのと同じ意味 (改行はその場で行として渡る) に
/// なるが、これは素のシェルでは望ましい挙動なのでそのまま流す。
///
/// 空文字は空のバイト列を返す (呼び出し側が送信そのものを取り止める)。
pub fn body_bytes(text: &str, bracketed: bool) -> Vec<u8> {
    let body = sanitize(text);
    if body.is_empty() {
        return Vec::new();
    }
    if !bracketed {
        return body.into_bytes();
    }
    let mut out = Vec::with_capacity(body.len() + PASTE_BEGIN.len() + PASTE_END.len());
    out.extend_from_slice(PASTE_BEGIN);
    out.extend_from_slice(body.as_bytes());
    out.extend_from_slice(PASTE_END);
    out
}

/// 入力欄の残留判定に使う末尾断片 (純関数)。
///
/// 空白を 1 個へ潰してから末尾 [`TAIL_CHARS`] 文字を取る。入力欄側も同じ
/// 正規化を通してから包含を見るので、折り返しや桁揃えの空白でズレない。
pub fn tail_key(text: &str) -> String {
    let norm = normalize_ws(&sanitize(text));
    let n = norm.chars().count();
    norm.chars().skip(n.saturating_sub(TAIL_CHARS)).collect()
}

/// 空白 (改行・タブ含む) を 1 個の半角空白へ潰す。
fn normalize_ws(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut sp = false;
    for ch in s.chars() {
        if ch.is_whitespace() {
            sp = true;
            continue;
        }
        if sp && !out.is_empty() {
            out.push(' ');
        }
        sp = false;
        out.push(ch);
    }
    out
}

/// 入力欄に本文がまだ残っているか (純関数)。
///
/// `input` は端末画面から拾った「入力欄に見えている文字列」。取れなかった
/// (`None`) ときは **残っていない扱い** にする — 見えないものを根拠に
/// Enter を撃ち直すと、承認プロンプトへ誤爆する危険の方が大きい。
pub fn still_pending(input: Option<&str>, tail: &str) -> bool {
    if tail.is_empty() {
        return false;
    }
    match input {
        Some(t) => normalize_ws(t).contains(tail),
        None => false,
    }
}

// ── 状態機械 ─────────────────────────────────────────────────────────

/// 送信 1 通の進行段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 相手が受け取れる状態になるのを待っている (まだ何も書いていない)
    Ready,
    /// 本文を書いた。確定キーを送るのを待っている
    Commit,
    /// 確定キーを送った。効いたかを確かめている
    Verify,
}

/// 相手のいまの様子。呼び出し側が毎フレーム集める。
#[derive(Debug, Clone, Default)]
pub struct Peek {
    /// PTY が生きているか
    pub running: bool,
    /// 見張りが Idle と判定しているか
    pub idle: bool,
    /// 承認プロンプト等で止まっているか。**真の間は絶対に送らない**
    pub attention: bool,
    /// アプリが bracketed paste を有効にしているか
    pub bracketed: bool,
    /// 入力欄に見えている文字列 (拾えなければ None)
    pub input: Option<String>,
}

/// 送信 1 通。
#[derive(Debug, Clone)]
pub struct Job {
    /// 宛先セッション ID
    pub session: u64,
    /// 送る本文 (整形前)
    pub text: String,
    /// 確定キーまで送るか。偽なら**入力欄へ入れるだけ**で人の Enter を待つ
    pub submit: bool,
    /// Idle になるまで待つか。
    ///
    /// ユーザーが自分で送信ボタンを押した分は **待たない** (偽) —
    /// 押した瞬間に届かないと「効かなかった」と受け取られるため。
    /// 起動直後の自動配達 (Issue 着手・レース・失敗切替) だけが真。
    pub wait_idle: bool,
    /// いまの段
    pub stage: Stage,
    /// 確定キーを送った回数
    pub tries: u8,
    /// 表示用のラベル (トースト)
    pub title: String,
    /// **配達の結果を知りたい呼び出し元の目印** (無ければ `None`)。
    ///
    /// 積めたこと (`queue_submit` が真) と、実際に届いたこと
    /// (`Act::Done`) は**別の時刻に決まる**。積んだ時点で「送った」と
    /// 記録すると、その後に相手が消えても (`Act::Gone`)、入力欄が空かない
    /// まま上限に達しても (`Act::GaveUp`)、呼び出し元は永久に気付けない。
    /// ここに目印を入れておくと、終わり方が呼び出し元へ 1 回だけ返る。
    ///
    /// **送信経路は増やさない。** 目印を運ぶだけで、書き込みはこれまで
    /// どおり `submit_tick` の 1 か所しかない。
    pub tag: Option<String>,
}

impl Job {
    /// ユーザーが送信操作をした分 (待たない・確定まで送る)。
    pub fn user(session: u64, text: impl Into<String>) -> Self {
        Self {
            session,
            text: text.into(),
            submit: true,
            wait_idle: false,
            stage: Stage::Ready,
            tries: 0,
            title: String::new(),
            tag: None,
        }
    }

    /// 起動直後の自動配達分 (Idle を待つ)。
    pub fn deferred(session: u64, text: impl Into<String>, submit: bool) -> Self {
        Self {
            session,
            text: text.into(),
            submit,
            wait_idle: true,
            stage: Stage::Ready,
            tries: 0,
            title: String::new(),
            tag: None,
        }
    }
}

/// [`decide`] が返す次の一手。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Act {
    /// 宛先が消えた / 終了した。黙って捨てる
    Gone,
    /// 本文を PTY へ書く
    WriteBody,
    /// 確定キーを送る
    WriteCommit,
    /// 完了
    Done,
    /// 上限まで待っても届かなかった。ユーザーへ知らせる
    GaveUp,
    /// まだ。この時間だけ待ってからもう一度見る
    Wait(Duration),
}

/// 次の一手を決める (**純粋関数**)。
///
/// * `since_stage` — いまの段に入ってからの経過
/// * `since_queued` — 積まれてからの経過 (待ちの上限判定に使う)
///
/// 判断順序が意味を持つ:
/// 1. 生きていなければ何もしない
/// 2. **承認待ちなら絶対に送らない** (本文が承認への回答になってしまう)
/// 3. それから段ごとの進行
pub fn decide(job: &Job, peek: &Peek, since_stage: Duration, since_queued: Duration) -> Act {
    if !peek.running {
        return Act::Gone;
    }
    match job.stage {
        Stage::Ready => {
            if since_queued >= GIVE_UP {
                return Act::GaveUp;
            }
            // 承認プロンプトで止まっている間は何があっても送らない。
            if peek.attention {
                return Act::Wait(POLL);
            }
            if !job.wait_idle {
                return Act::WriteBody;
            }
            // 自動配達は Idle を待つ。待ちすぎたら入れてしまう
            // (承認待ちでないことは上で確かめてある)。
            if peek.idle || since_queued >= READY_WAIT {
                Act::WriteBody
            } else {
                Act::Wait(POLL)
            }
        }
        Stage::Commit => {
            if !job.submit {
                return Act::Done;
            }
            if since_stage < COMMIT_DELAY {
                return Act::Wait(COMMIT_DELAY - since_stage);
            }
            // 本文を書いてから確定までの間に承認プロンプトが出たら、
            // Enter は承認への回答になる。撃たずに落ち着くのを待つ。
            if peek.attention {
                return Act::Wait(POLL);
            }
            Act::WriteCommit
        }
        Stage::Verify => {
            if since_stage < VERIFY_DELAY {
                return Act::Wait(VERIFY_DELAY - since_stage);
            }
            if job.tries >= MAX_COMMIT_TRIES {
                return Act::Done;
            }
            // 撃ち直しは「承認プロンプトが出ていない」かつ
            // 「入力欄に本文がまだ見えている」ときだけ。
            if peek.attention {
                return Act::Done;
            }
            if still_pending(peek.input.as_deref(), &tail_key(&job.text)) {
                Act::WriteCommit
            } else {
                Act::Done
            }
        }
    }
}

// ── 待ち行列の 1 通 (時刻つき) ────────────────────────────────────────

/// 送信待ちの 1 通。時刻の記帳だけを持ち、判断は [`decide`] に任せる。
#[derive(Debug, Clone)]
pub struct Pending {
    pub job: Job,
    /// 積まれた時刻 (待ちの上限判定に使う)
    queued: Instant,
    /// いまの段に入った時刻
    stage_at: Instant,
}

impl Pending {
    pub fn new(job: Job, now: Instant) -> Self {
        Self {
            job,
            queued: now,
            stage_at: now,
        }
    }

    /// 次の一手を尋ねる。
    pub fn act(&self, peek: &Peek, now: Instant) -> Act {
        decide(
            &self.job,
            peek,
            now.saturating_duration_since(self.stage_at),
            now.saturating_duration_since(self.queued),
        )
    }

    /// 段を進める (経過時間の起点も一緒に打ち直す)。
    pub fn advance(&mut self, stage: Stage, now: Instant) {
        self.job.stage = stage;
        self.stage_at = now;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peek_ready() -> Peek {
        Peek {
            running: true,
            idle: true,
            attention: false,
            bracketed: true,
            input: None,
        }
    }

    #[test]
    fn 改行コードは_lf_へ揃えて末尾の空白を落とす() {
        assert_eq!(sanitize("a\r\nb\r\n"), "a\nb");
        assert_eq!(sanitize("a\rb"), "a\nb");
        assert_eq!(sanitize("  a  \n\n"), "  a");
    }

    #[test]
    fn 制御文字は落とすがタブと改行は残す() {
        assert_eq!(sanitize("a\x07b\tc\nd"), "ab\tc\nd");
        assert!(!sanitize("\x1b[31mred").contains('\x1b'));
    }

    /// **本文と確定キーは絶対に 1 回の write にしない** — これを混ぜると
    /// Ink 系 TUI がまとめてペーストと判定し、CR が改行として飲まれる。
    #[test]
    fn 本文のバイト列に確定キーを含めない() {
        for bracketed in [false, true] {
            let b = body_bytes("やること", bracketed);
            assert!(!b.ends_with(COMMIT), "bracketed={bracketed}");
            assert!(!b.contains(&b'\r'), "bracketed={bracketed}");
        }
    }

    #[test]
    fn bracketed_なら本文を包む() {
        let b = body_bytes("一行目\n二行目", true);
        let s = String::from_utf8(b).unwrap();
        assert_eq!(s, "\x1b[200~一行目\n二行目\x1b[201~");
    }

    #[test]
    fn bracketed_でなければ素のまま送る() {
        let b = body_bytes("ls -la", false);
        assert_eq!(String::from_utf8(b).unwrap(), "ls -la");
    }

    #[test]
    fn 空文字は空のバイト列() {
        assert!(body_bytes("   \n ", true).is_empty());
    }

    /// 承認プロンプトが出ている間は、どの段でも絶対に書かない。
    /// (本文が「承認への回答」になると、既定が "No, exit" の
    ///  プロンプトではセッションごと終了する)
    #[test]
    fn 承認待ちの間は何も送らない() {
        let peek = Peek {
            attention: true,
            ..peek_ready()
        };
        for stage in [Stage::Ready, Stage::Commit] {
            let job = Job {
                stage,
                ..Job::user(1, "やって")
            };
            assert!(
                matches!(
                    decide(&job, &peek, Duration::from_secs(5), Duration::from_secs(5)),
                    Act::Wait(_)
                ),
                "stage={stage:?} で送ろうとした"
            );
        }
    }

    /// ユーザーが押した送信は Idle を待たない (待つと「効かない」に見える)。
    #[test]
    fn ユーザー送信は_idle_を待たずに書く() {
        let peek = Peek {
            idle: false,
            ..peek_ready()
        };
        let job = Job::user(1, "やって");
        assert_eq!(
            decide(&job, &peek, Duration::ZERO, Duration::ZERO),
            Act::WriteBody
        );
    }

    /// 自動配達は Idle を待つが、待ちすぎたら入れる。
    #[test]
    fn 自動配達は_idle_を待ち上限で諦めずに入れる() {
        let peek = Peek {
            idle: false,
            ..peek_ready()
        };
        let job = Job::deferred(1, "やって", true);
        assert!(matches!(
            decide(&job, &peek, Duration::ZERO, Duration::ZERO),
            Act::Wait(_)
        ));
        assert_eq!(
            decide(&job, &peek, Duration::ZERO, READY_WAIT),
            Act::WriteBody
        );
    }

    #[test]
    fn 本文の直後には確定キーを送らず待つ() {
        let job = Job {
            stage: Stage::Commit,
            ..Job::user(1, "やって")
        };
        assert!(matches!(
            decide(&job, &peek_ready(), Duration::ZERO, Duration::ZERO),
            Act::Wait(_)
        ));
        assert_eq!(
            decide(&job, &peek_ready(), COMMIT_DELAY, COMMIT_DELAY),
            Act::WriteCommit
        );
    }

    #[test]
    fn 確定キー不要の配達は本文だけで完了する() {
        let job = Job {
            stage: Stage::Commit,
            ..Job::deferred(1, "やって", false)
        };
        assert_eq!(
            decide(&job, &peek_ready(), Duration::ZERO, Duration::ZERO),
            Act::Done
        );
    }

    /// 入力欄に本文が残っていたら撃ち直す (これが「待機したまま」の救済)。
    #[test]
    fn 入力欄に残っていたら確定キーを撃ち直す() {
        let text = "テストを通してからコミットしてください";
        let peek = Peek {
            input: Some(format!("> {text}")),
            ..peek_ready()
        };
        let job = Job {
            stage: Stage::Verify,
            tries: 1,
            ..Job::user(1, text)
        };
        assert_eq!(
            decide(&job, &peek, VERIFY_DELAY, VERIFY_DELAY),
            Act::WriteCommit
        );
    }

    #[test]
    fn 入力欄が空になっていれば完了() {
        let peek = Peek {
            input: Some("> ".into()),
            ..peek_ready()
        };
        let job = Job {
            stage: Stage::Verify,
            tries: 1,
            ..Job::user(1, "やって")
        };
        assert_eq!(decide(&job, &peek, VERIFY_DELAY, VERIFY_DELAY), Act::Done);
    }

    /// 撃ち直しは上限で必ず止まる (勝手に承認し続けない)。
    #[test]
    fn 撃ち直しは上限で止まる() {
        let text = "やってください";
        let peek = Peek {
            input: Some(text.into()),
            ..peek_ready()
        };
        let job = Job {
            stage: Stage::Verify,
            tries: MAX_COMMIT_TRIES,
            ..Job::user(1, text)
        };
        assert_eq!(decide(&job, &peek, VERIFY_DELAY, VERIFY_DELAY), Act::Done);
    }

    /// 入力欄が読めないときは撃ち直さない (誤爆より取りこぼしを選ぶ)。
    #[test]
    fn 入力欄が読めないときは撃ち直さない() {
        let job = Job {
            stage: Stage::Verify,
            tries: 1,
            ..Job::user(1, "やって")
        };
        assert_eq!(
            decide(&job, &peek_ready(), VERIFY_DELAY, VERIFY_DELAY),
            Act::Done
        );
    }

    #[test]
    fn 終了したセッションへは送らない() {
        let peek = Peek {
            running: false,
            ..peek_ready()
        };
        assert_eq!(
            decide(
                &Job::user(1, "やって"),
                &peek,
                Duration::ZERO,
                Duration::ZERO
            ),
            Act::Gone
        );
    }

    #[test]
    fn 待ちの上限を超えたら諦める() {
        let peek = Peek {
            idle: false,
            ..peek_ready()
        };
        let job = Job::deferred(1, "やって", true);
        assert_eq!(decide(&job, &peek, Duration::ZERO, GIVE_UP), Act::GaveUp);
    }

    #[test]
    fn 末尾断片は空白の揺れを吸収する() {
        let t = tail_key("あ  い\nう");
        assert!(!t.contains('\n'));
        assert!(still_pending(Some("│ あ い う   │"), &t));
        assert!(!still_pending(Some("│           │"), &t));
    }

    #[test]
    fn 空の本文は残留判定に使わない() {
        assert!(!still_pending(Some("なんでも"), ""));
    }
}
