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
//!    **承認プロンプトが出ていないときだけ**、入力欄に本文が見えている
//!    あいだ ([`GIVE_UP`] まで)。
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
/// Enter を送り続けて**勝手に承認してしまう**。上限で必ず止める
/// (承認プロンプト中は `attention` で止まるが、上限は最後の砦として残す)。
///
/// **3 回では足りなかった。** 起動に手間取る相手 (MCP を立ち上げている
/// Codex 等) では 3 回とも起動中に当たって終わる — 実機で、同じ Codex でも
/// 起動が速かった側は動き出し、`⚠ MCP startup incomplete` が出ていた側は
/// 本文を入力欄に抱えたまま止まっていた。間隔は回を追うごとに広がるので
/// (`VERIFY_DELAY * 回数`、上限 [`COMMIT_RETRY_MAX`])、12 回で
/// おおよそ 30 秒ぶん粘る。
pub const MAX_COMMIT_TRIES: u8 = 12;

/// 「落ち着くまで待つ」指示 (Issue 着手 / レース / 失敗切替の引き継ぎ) が
/// Idle を待つ上限。これを過ぎたら承認待ちでない限り入れてしまう
/// (スピナーの誤検知で永遠に待たないため)。
pub const READY_WAIT: Duration = Duration::from_secs(45);

/// 諦める上限。ここを過ぎたらユーザーへ知らせて捨てる。
pub const GIVE_UP: Duration = Duration::from_secs(120);

/// 確定キーを撃つ前に「相手が静かになる」のを待つ上限。
///
/// **忙しい CLI は確定キーを飲み込む** (実機の Codex)。かといって永久に
/// 待つと届かないので、ここを過ぎたら撃って `Verify` で確かめる。
/// `GIVE_UP` より十分短くする — 待ちで予算を使い切ると、撃ち直す機会が
/// 1 度も来ない。
pub const COMMIT_IDLE_WAIT: Duration = Duration::from_secs(20);

/// 確定キーを撃ち直す間隔の上限。
///
/// 回を追うごとに間隔を広げる (`VERIFY_DELAY * 回数`) が、開きすぎると
/// 相手が受け取れるようになってから届くまでが遅くなる。
pub const COMMIT_RETRY_MAX: Duration = Duration::from_secs(3);

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
impl Peek {
    /// **本文が入力欄に見えているか** (= こちらの書き込みが届いた証拠)。
    ///
    /// 畳まれた貼り付け (`[Pasted Content 2329 chars]`) も「見えている」と
    /// 数える — 中身は本文そのものなので、確定キーを撃ってよい。
    ///
    /// **読み取れないときは真を返す。** 入力欄を読めない相手で書き直しを
    /// 繰り返すと、同じ本文が何度も入ることになる。証拠が無いなら、
    /// 従来どおり 1 回書いて進む。
    pub fn input_seen(&self, tail: &str) -> bool {
        match self.input.as_deref() {
            None => true,
            Some(t) => still_pending(Some(t), tail),
        }
    }
}

pub fn still_pending(input: Option<&str>, tail: &str) -> bool {
    if tail.is_empty() {
        return false;
    }
    match input {
        Some(t) => {
            // **畳まれた貼り付けは「残っている」。**
            //
            // 長い本文を貼ると、入力欄に本文ではなく見出しを出す CLI が
            // ある (`[Pasted Content 2329 chars]`)。本文が消えたように
            // 見えるので、そのまま読むと**送れていないのに「送信済み」**に
            // なる。実機で 6 通中 2 通がこれで落ちた
            // (一覧は `agents::PASTE_PLACEHOLDERS`)。
            if crate::agents::looks_like_pending_paste(t) {
                return true;
            }
            normalize_ws(t).contains(tail)
        }
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
    /// **この相手はもう入力を受け取れるか。**
    ///
    /// 起動直後の数秒は書いても落ちる CLI がある (Claude Code v2)。
    /// 待つ長さは**カタログが持つ** (`agents::input_ready_ms`) ので、
    /// ここは真偽だけを受け取る — 送信側に CLI ごとの分岐を作らない。
    pub input_ready: bool,
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
            // **まだ受け取れない相手には書かない。**
            //
            // 起動直後に書いても画面に 1 文字も出ない CLI がある。書けた
            // つもりで確定キーまで送ると、配達は「完了」と記録されるのに
            // 相手には何も届かない (実機の Claude Code)。待つ長さは
            // カタログが持つ ([`crate::agents::input_ready_ms`])。
            if !peek.input_ready {
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
            // **書けたことを確かめてから確定する。**
            //
            // 起動中の CLI は書き込みを丸ごと捨てる。捨てられたことに
            // 気付かずに確定キーを撃つと、`Verify` は「入力欄に本文が
            // 残っていない」を見て**届いた**と判断する — 1 バイトも
            // 送っていないのに配達完了になる。
            //
            // 実機 (Test6) の実測: 6 体のうち **Claude Code 2 体は
            // 指示の痕跡が 0 件**のまま「作業中」と表示され、9 分間
            // 成果物が 1 つも出来なかった。待ち時間を伸ばしても、
            // 環境が変われば同じことが起きる — **確かめるほうが正しい。**
            //
            // 入力欄に本文が見えないなら、書き直す。見えるようになれば
            // 下へ進んで確定する。
            if !peek.input_seen(&tail_key(&job.text)) {
                if since_queued >= GIVE_UP || job.tries >= MAX_COMMIT_TRIES {
                    return Act::GaveUp;
                }
                return Act::WriteBody;
            }
            // **相手が手を動かしている間は撃たない。**
            //
            // 忙しい CLI は確定キーを飲み込む。実機の Codex では、本文は
            // 入力欄に入っているのに Enter が効かず、2 通目以降が毎回
            // 止まっていた。1 通目が届いていたのは起動直後で待機していた
            // から — **同じ条件を作ってから撃つ**。
            //
            // 待ちっぱなしにはしない。[`COMMIT_IDLE_WAIT`] を過ぎたら
            // 待たずに撃つ (取りこぼすより、撃って確かめるほうがよい。
            // 効かなければ `Verify` が入力欄を見て撃ち直す)。
            if !peek.idle && since_stage < COMMIT_IDLE_WAIT && !peek.attention {
                return Act::Wait(POLL);
            }
            // 本文を書いてから確定までの間に承認プロンプトが出たら、
            // Enter は承認への回答になる。撃たずに落ち着くのを待つ。
            if peek.attention {
                // **待ち続けない。** ここには上限が無かったので、承認待ちが
                // 続く相手では**本文を入力欄に置いたまま永久に止まって**いた
                // (実機: 指示が入力欄に見えているのに何分経っても送られない)。
                // 送ってしまうと承認への回答になるので、送らずに**人へ返す**。
                if since_queued >= GIVE_UP {
                    return Act::GaveUp;
                }
                return Act::Wait(POLL);
            }
            Act::WriteCommit
        }
        Stage::Verify => {
            if since_stage < VERIFY_DELAY {
                return Act::Wait(VERIFY_DELAY - since_stage);
            }
            let stuck = still_pending(peek.input.as_deref(), &tail_key(&job.text));
            // **残っている限り撃ち直す。上限は回数ではなく時間で決める。**
            //
            // 回数で切ると、起動に手間取る相手 (MCP を立ち上げている Codex 等)
            // では**3 回とも起動中に当たって終わる**。実機で、同じ Codex でも
            // 起動が速かった側は動き出し、`⚠ MCP startup incomplete` が出て
            // いた側は本文を入力欄に抱えたまま止まっていた。
            //
            // 入力欄に本文が見えている = まだ届いていない、という**証拠**が
            // あるので、撃ち直しても二重送信にはならない (届けば消える)。
            if !stuck {
                return Act::Done;
            }
            if since_queued >= GIVE_UP || job.tries >= MAX_COMMIT_TRIES {
                // **届かなかったことを黙って完了にしない。** 呼び出し元が
                // 撃ち直せるよう、失敗として返す。
                return Act::GaveUp;
            }
            // 撃ち直しは「承認プロンプトが出ていない」ときだけ
            // (出ていれば Enter は承認への回答になる)。
            if peek.attention {
                return Act::Wait(POLL);
            }
            // **間隔は少しずつ広げる。** 起動中の相手へ 450ms ごとに撃ち続けても
            // 意味が無いので、回を追うごとに待つ (上限 [`COMMIT_RETRY_MAX`])。
            let wait = (VERIFY_DELAY * (1 + u32::from(job.tries))).min(COMMIT_RETRY_MAX);
            if since_stage < wait {
                return Act::Wait(wait - since_stage);
            }
            Act::WriteCommit
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
            input_ready: true,
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
        // 間隔は回を追うごとに広がるので、その待ちを過ぎてから撃つ。
        assert_eq!(
            decide(&job, &peek, COMMIT_RETRY_MAX, VERIFY_DELAY),
            Act::WriteCommit
        );
        // 待ちの途中では撃たない (起動中の相手へ連打しない)。
        assert!(matches!(
            decide(&job, &peek, Duration::ZERO, VERIFY_DELAY),
            Act::Wait(_)
        ));
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
        // まだ余力のある回数 (上限より手前)。
        let job = Job {
            stage: Stage::Verify,
            tries: 1,
            ..Job::user(1, text)
        };
        // **止めるのは回数ではなく時間。**
        //
        // 回数で切ると、起動に手間取る相手 (MCP を立ち上げている Codex 等)
        // では 3 回とも起動中に当たって終わる。入力欄に本文が見えている =
        // まだ届いていない、という**証拠**があるうちは撃ち直す。
        assert_eq!(
            decide(&job, &peek, COMMIT_RETRY_MAX, Duration::from_secs(5)),
            Act::WriteCommit,
            "証拠があるのに撃ち直しをやめている"
        );
        // 時間の上限を過ぎたら**人へ返す** (黙って完了にしない)。
        assert_eq!(decide(&job, &peek, COMMIT_RETRY_MAX, GIVE_UP), Act::GaveUp);
        // 回数の上限でも止まる (**承認プロンプトへ撃ち続けないための最後の砦**)。
        let spent = Job {
            tries: MAX_COMMIT_TRIES,
            ..job.clone()
        };
        assert_eq!(
            decide(&spent, &peek, COMMIT_RETRY_MAX, Duration::from_secs(5)),
            Act::GaveUp,
            "回数の上限を超えて撃ち続けている"
        );
        // 入力欄から消えていれば、届いたので完了でよい。
        let sent = Peek {
            input: Some(String::new()),
            ..peek_ready()
        };
        assert_eq!(decide(&job, &sent, VERIFY_DELAY, VERIFY_DELAY), Act::Done);
    }

    /// **承認待ちのまま止まり続けない。**
    ///
    /// 本文を書いたあと確定キーを送る段には上限が無く、承認プロンプトが
    /// 出ている相手では**入力欄に本文を置いたまま永久に待って**いた
    /// (実機: 指示が見えているのに何分経っても送られない)。
    /// 送ってしまうと承認への回答になるので、送らずに人へ返す。
    #[test]
    fn 承認待ちで止まったままにしない() {
        let text = "やってください";
        let peek = Peek {
            attention: true,
            input: Some(text.into()),
            ..peek_ready()
        };
        for stage in [Stage::Commit, Stage::Verify] {
            let job = Job {
                stage,
                ..Job::user(1, text)
            };
            // 上限までは待つ (承認が終われば送れるので、すぐ諦めない)。
            assert!(
                matches!(
                    decide(&job, &peek, VERIFY_DELAY, Duration::from_secs(1)),
                    Act::Wait(_)
                ),
                "stage={stage:?}: すぐ諦めている"
            );
            // 上限を超えたら**人へ返す** (黙って待ち続けない)。
            assert_eq!(
                decide(&job, &peek, VERIFY_DELAY, GIVE_UP),
                Act::GaveUp,
                "stage={stage:?}: 承認待ちのまま止まり続けている"
            );
        }
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
    /// **忙しい相手には確定キーを撃たない。**
    ///
    /// 実機の Codex は、手を動かしている最中の Enter を飲み込む。本文は
    /// 入力欄に入っているのに送信されず、2 通目以降が毎回止まっていた
    /// (1 通目だけ届いていたのは、起動直後で相手が待機していたから)。
    /// **1 通目と同じ条件を毎回作ってから撃つ。**
    #[test]
    fn 忙しい相手には確定キーを撃たない() {
        let text = "やってください";
        let busy = Peek {
            idle: false,
            input: Some(text.into()),
            ..peek_ready()
        };
        let job = Job {
            stage: Stage::Commit,
            ..Job::deferred(1, text, true)
        };
        // 手が動いている間は待つ。
        assert!(
            matches!(
                decide(&job, &busy, COMMIT_DELAY, Duration::from_secs(1)),
                Act::Wait(_)
            ),
            "忙しい相手へ撃っている"
        );
        // **待ちっぱなしにはしない。** 上限を過ぎたら撃って、効いたかを確かめる。
        assert_eq!(
            decide(&job, &busy, COMMIT_IDLE_WAIT, Duration::from_secs(30)),
            Act::WriteCommit,
            "静かにならない相手へ永久に届かない"
        );
        // 静かなら待たずに撃つ (1 通目と同じ道)。
        let quiet = Peek {
            idle: true,
            ..busy.clone()
        };
        assert_eq!(
            decide(&job, &quiet, COMMIT_DELAY, Duration::from_secs(1)),
            Act::WriteCommit
        );
    }

    /// **起動直後の相手には書かない。**
    ///
    /// 実機で Claude Code は起動から数秒、stdin へ書いても画面に 1 文字も
    /// 出なかった。Zaivern は書けたつもりで確定キーまで送り、配達を「完了」と
    /// 記録するのに、相手には何も届かない (Antigravity のログには指示が
    /// 144 件現れるのに、Claude Code のログは起動表示だけの 6KB で 0 件だった)。
    #[test]
    fn 受け取れない相手には書かない() {
        let job = Job::user(1, "やって");
        let not_yet = Peek {
            input_ready: false,
            ..peek_ready()
        };
        assert!(
            matches!(
                decide(&job, &not_yet, Duration::ZERO, Duration::from_secs(1)),
                Act::Wait(_)
            ),
            "受け取れない相手へ書いている"
        );
        // 受け取れるようになったら書く。
        assert_eq!(
            decide(&job, &peek_ready(), Duration::ZERO, Duration::from_secs(1)),
            Act::WriteBody
        );
        // **待ちっぱなしにはしない。** 上限を過ぎたら人へ返す。
        assert_eq!(decide(&job, &not_yet, Duration::ZERO, GIVE_UP), Act::GaveUp);
    }
}

#[cfg(test)]
mod paste_placeholder_tests {
    use super::*;

    /// **畳まれた貼り付けを「届いた」と読まない。**
    ///
    /// 実機の画面そのもの。Codex は長い本文を
    /// `[Pasted Content 2329 chars]` に畳んで見せるので、本文の末尾を
    /// 探す確認は必ず外れる。外れると `Act::Done` = 送信済みとして
    /// 記録され、**1 文字も届いていない担当が「作業中」と表示される**。
    /// 実機で 6 通中 2 通がこの形で消えた。
    #[test]
    fn 畳まれた貼り付けはまだ届いていない() {
        let tail = "最後まで終わらせて報告してください";
        for shown in [
            "[Pasted Content 2329 chars]",
            "[Pasted Content 2306 chars]",
            "[Pasted text #3 +103 lines]",
            "> [pasted content 12 chars]",
        ] {
            assert!(
                still_pending(Some(shown), tail),
                "畳まれた貼り付けを届いたと読んだ: {shown}"
            );
        }
    }

    /// **本当に消えていれば届いている。** 畳みの検出で、正常な送信を
    /// 「まだ残っている」と読み替えてはいけない (撃ち直しが止まらなくなる)。
    #[test]
    fn 入力欄が空なら届いている() {
        let tail = "最後まで終わらせて報告してください";
        assert!(!still_pending(Some(""), tail));
        assert!(!still_pending(Some("❯ "), tail));
        assert!(!still_pending(None, tail));
        // 人が別のことを打っていても、こちらの本文ではない。
        assert!(!still_pending(Some("git status を見せて"), tail));
    }

    /// **本文がそのまま見えている場合は、これまでどおり残っている扱い。**
    #[test]
    fn 本文が見えていれば残っている() {
        let tail = "最後まで終わらせて報告してください";
        assert!(still_pending(
            Some("… 最後まで終わらせて報告してください"),
            tail
        ));
    }

    /// **畳みの見出しはカタログ 1 か所で持つ。**
    /// 送信側に CLI ごとの分岐を作らない (増えるたびに 2 か所直すことになる)。
    #[test]
    fn 畳みの見出しはカタログが持つ() {
        let src = include_str!("submit.rs").replace("\r\n", "\n");
        let body = src
            .split("pub fn still_pending")
            .nth(1)
            .and_then(|t| t.split("\n}\n").next())
            .expect("still_pending がある");
        assert!(
            body.contains("agents::looks_like_pending_paste"),
            "畳みの判定を送信側に書いている"
        );
        // **送信側に CLI ごとの分岐を作らない**ことは
        // `agents::tests::エージェントごとの癖はカタログにだけ置く` が
        // リポジトリ全体で見張っている (ここで二重に持たない)。
    }
}

#[cfg(test)]
mod write_confirm_tests {
    use super::*;

    fn job(text: &str) -> Job {
        let mut j = Job::user(1, text.to_string());
        j.stage = Stage::Commit;
        j
    }

    fn peek(input: Option<&str>) -> Peek {
        Peek {
            running: true,
            idle: true,
            attention: false,
            input_ready: true,
            input: input.map(str::to_string),
            ..Default::default()
        }
    }

    /// **書けていないのに確定キーを撃たない。**
    ///
    /// 起動中の CLI は書き込みを捨てる。捨てられたことに気付かずに
    /// 確定すると、`Verify` は「入力欄に本文が無い」を見て**届いた**と
    /// 判断し、1 バイトも送っていないのに配達完了になる。
    /// 実機 (Test6) では Claude Code 2 体が**指示の痕跡 0 件**のまま
    /// 「作業中」と表示され、9 分で成果物が 1 つも出来なかった。
    #[test]
    fn 本文が入力欄に無ければ書き直す() {
        let j = job("実装してください。最後まで終わらせて報告してください");
        // 入力欄が空 = 書き込みが捨てられた。
        assert_eq!(
            decide(&j, &peek(Some("")), COMMIT_DELAY, COMMIT_DELAY),
            Act::WriteBody
        );
        assert_eq!(
            decide(&j, &peek(Some("❯ ")), COMMIT_DELAY, COMMIT_DELAY),
            Act::WriteBody
        );
    }

    /// **見えていれば、これまでどおり確定する。**
    #[test]
    fn 本文が見えていれば確定する() {
        let text = "実装してください。最後まで終わらせて報告してください";
        let j = job(text);
        assert_eq!(
            decide(&j, &peek(Some(text)), COMMIT_DELAY, COMMIT_DELAY),
            Act::WriteCommit
        );
        // 畳まれた貼り付けも「見えている」。中身は本文そのもの。
        assert_eq!(
            decide(
                &j,
                &peek(Some("[Pasted Content 2329 chars]")),
                COMMIT_DELAY,
                COMMIT_DELAY
            ),
            Act::WriteCommit
        );
    }

    /// **入力欄を読めない相手では、書き直しを繰り返さない。**
    /// 証拠が無いなら 1 回書いて進む (同じ本文が何度も入るほうが害が大きい)。
    #[test]
    fn 入力欄が読めないなら従来どおり進む() {
        let j = job("実装してください");
        assert_eq!(
            decide(&j, &peek(None), COMMIT_DELAY, COMMIT_DELAY),
            Act::WriteCommit
        );
    }

    /// **書き直しにも上限がある。** 永久に書き続けず、人へ返す。
    #[test]
    fn 書き直しにも上限がある() {
        let mut j = job("実装してください");
        j.tries = MAX_COMMIT_TRIES;
        assert_eq!(
            decide(&j, &peek(Some("")), COMMIT_DELAY, COMMIT_DELAY),
            Act::GaveUp
        );
        let j2 = job("実装してください");
        assert_eq!(
            decide(&j2, &peek(Some("")), COMMIT_DELAY, GIVE_UP),
            Act::GaveUp
        );
    }
}
