//! シェル統合 (OSC 633 / OSC 133) — 端末レイヤでの「画面スクレイプ」を廃止する。
//!
//! CLAUDE.md 設計原則 4 は判定の出どころに優先順位を付けている:
//! **構造化プロトコル > ベンダー提供フック > 状態ファイル > 画面スクレイプ**。
//! このモジュールはそのうち「端末そのものが持つ構造化プロトコル」を担う。
//!
//! # 何が変わるか
//!
//! 「いま走っているコマンドは何で、終わったのか、終了コードはいくつか」を、
//! 描画済みのグリッドから**推測しない**。シェルが OSC で直接教えてくる。
//! これは単なる高速化ではなく、**バグの一群が構造的に消える**変更である:
//!
//! - 端末の桁数が変わると判定結果が変わる (行が折り返されて正規表現が外れる)。
//!   VS Code の problem-matcher 系バグの上位は全部これで、
//!   「端末パネルを**隠している**ときだけ 10 件全部拾える」という報告まである。
//!   こちらはバイト列 (PTY の生ストリーム) を読むので、桁数に一切依存しない。
//! - `error` / `failed` の部分一致で `Read(src/error_handling.rs)` を
//!   「エラー」に数える事故 (CLAUDE.md の傷)。終了コード 0 という**事実**が
//!   来るなら、文字列ヒューリスティクスの誤判定をその場で否定できる。
//!
//! # 段 (Tier)
//!
//! VS Code と同じ 3 段で、**必ず UI に出す**。黙って劣化するのが一番たちが悪い。
//!
//! | 段 | 分かること |
//! |----|-----------|
//! | [`Tier::Rich`]  | コマンド行 (OSC 633 `E`) + 境界 + 終了コード + cwd |
//! | [`Tier::Basic`] | 境界 (`A`/`B`/`C`) と終了コード (`D`) だけ。コマンド行は不明 |
//! | [`Tier::None`]  | 何も来ていない。従来どおり画面推定へ降りる |
//!
//! # 非破壊であること
//!
//! - 注入は**オプトイン** ([`set_enabled`])。既定は off で、そのときの起動経路は
//!   1 バイトも変わらない。
//! - パースは常時 on だが、これは**受け身**なので副作用が無い。iTerm2 / kitty /
//!   starship を使っている人のシェルは既に OSC 133 を出しているので、
//!   注入しないまま [`Tier::Basic`] 以上になることがある (それでよい)。
//! - 注入する側 (シム) も、ユーザーの設定が既に OSC を出していれば**何もしない**。
//!   二重発行は「1 コマンドが 2 件に見える」形で壊れるので、必ず避ける。

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

use crate::i18n::{tr, trf};
use crate::supervisor::protocol::{ProtoRead, ProtoState};

// ---------------------------------------------------------------------------
// 上限 — 壊れた / 悪意ある発行元にメモリを食わせない
// ---------------------------------------------------------------------------

/// 1 セッションが覚えるコマンド件数の上限。
///
/// 超えたぶんは古い順に捨てるが、**捨てたことは必ず見せる**
/// (設計原則 2 の「捨てた箇所には明示的なギャップ標識を入れる」)。
pub const MAX_COMMANDS: usize = 200;

/// コマンド行として受け取る最大文字数。ヒアドキュメントを丸ごと 1 行で
/// 渡してくるシェルがあるので、表示にも記録にも要らない長さで切る。
pub const MAX_COMMAND_CHARS: usize = 2048;

/// `P;Cwd=` で受け取るパスの最大バイト数。
const MAX_CWD_BYTES: usize = 4096;

/// 段の変化ログの保持件数 (UI に出すだけなので少しでよい)。
const MAX_TIER_LOG: usize = 16;

/// マーカーが途切れたと見なすまでの時間 (ミリ秒)。
///
/// シェル統合は「プロンプトが出た」「コマンドが終わった」の瞬間しか喋らない。
/// 人が席を外していれば何時間も無言なので、構造化ストリーム
/// ([`crate::supervisor::PROTO_STALE_MS`] = 120 秒) よりずっと長く効かせる。
/// ただし無限ではない — セッションが死んで無言になったものを信じ続けないため。
pub const SHELL_STALE_MS: u64 = 30 * 60 * 1000;

// ---------------------------------------------------------------------------
// マーカー (パース結果)
// ---------------------------------------------------------------------------

/// OSC 633 / OSC 133 の 1 マーカー。
///
/// 由来 (633 か 133 か) は [`Marker::rich`] でだけ区別する。段の判定以外では
/// 同じ意味なので、上位は区別せずに扱ってよい。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Marker {
    /// `A` — プロンプトの開始。
    PromptStart,
    /// `B` — プロンプトの終わり (= ユーザーが打ち始める位置)。
    PromptEnd,
    /// `C` — コマンド出力の開始 (実行直前)。
    PreExec,
    /// `D[;<exit>]` — 実行終了。終了コードが付かない発行元もある。
    Finished(Option<i32>),
    /// `633;E;<commandline>[;<nonce>]` — **シェルが知っているコマンド行**。
    /// 画面から拾い直す必要が無くなる、この機能の中核。
    CommandLine { line: String, nonce: String },
    /// `633;P;Cwd=<path>` — 権威のある作業ディレクトリ。
    Cwd(PathBuf),
    /// `633;P;<key>=<value>` のうち Cwd 以外。
    Property { key: String, value: String },
}

impl Marker {
    /// このマーカーが「上の段 (Rich)」の証拠になるか。
    ///
    /// コマンド行が分かる = 画面を 1 文字も読まずに「何を実行したか」が言える。
    fn rich_evidence(&self) -> bool {
        match self {
            Marker::CommandLine { .. } => true,
            Marker::Property { key, value } => {
                key.eq_ignore_ascii_case("HasRichCommandDetection")
                    && value.eq_ignore_ascii_case("True")
            }
            _ => false,
        }
    }
}

/// OSC の本体を [`Marker`] に読む **純関数**。
///
/// `ps` は OSC の先頭数値 (`b"633"` / `b"133"`)、`rest` は最初の `;` より後ろ。
/// terminal.rs の既存 OSC パーサ (`on_osc`) からそのまま呼べる形にしてある
/// (パーサを 2 つ持たない)。
///
/// # 寛容さの方針
///
/// kitty は `133;A;k=s`、WezTerm は `133;D;1` のように**余分なパラメータを足す**。
/// 知らないパラメータは黙って捨て、先頭 1 文字だけで種類を決める。
/// 逆に `E` / `P` は 633 の拡張なので 133 では受け付けない
/// (`133;E` を名乗る発行元は存在せず、受けると誤読の温床になるだけ)。
pub fn parse_osc(ps: &[u8], rest: &[u8]) -> Option<Marker> {
    let is633 = match ps {
        b"633" => true,
        b"133" => false,
        _ => return None,
    };
    let mut parts = rest.split(|c| *c == b';');
    let letter = parts.next()?;
    // 種類は 1 文字ちょうど。`Debug` のような長い語は別物なので受けない。
    if letter.len() != 1 {
        return None;
    }
    match letter[0] {
        b'A' => Some(Marker::PromptStart),
        b'B' => Some(Marker::PromptEnd),
        b'C' => Some(Marker::PreExec),
        b'D' => {
            // 終了コードは 10 進の ASCII。付いていない / 数字でないなら None。
            let code = parts.next().and_then(|p| {
                let s = std::str::from_utf8(p).ok()?;
                s.trim().parse::<i32>().ok()
            });
            Some(Marker::Finished(code))
        }
        b'E' if is633 => {
            // 仕様上コマンド行は必ず逃がされている (`;` は `\x3b`)。よって
            // 最初の `;` より後ろは nonce であって、コマンド行の一部ではない。
            let raw = parts.next().unwrap_or(b"");
            let nonce = parts.next().unwrap_or(b"");
            let mut line = unescape_value(raw);
            truncate_chars(&mut line, MAX_COMMAND_CHARS);
            Some(Marker::CommandLine {
                line,
                nonce: String::from_utf8_lossy(nonce).into_owned(),
            })
        }
        b'P' if is633 => {
            let kv = parts.next()?;
            let eq = kv.iter().position(|c| *c == b'=')?;
            let key = String::from_utf8_lossy(&kv[..eq]).into_owned();
            let value_raw = &kv[eq + 1..];
            if key.eq_ignore_ascii_case("Cwd") {
                if value_raw.len() > MAX_CWD_BYTES {
                    return None;
                }
                let v = unescape_value(value_raw);
                if v.is_empty() {
                    return None;
                }
                return Some(Marker::Cwd(PathBuf::from(v)));
            }
            Some(Marker::Property {
                key,
                value: unescape_value(value_raw),
            })
        }
        _ => None,
    }
}

/// OSC 633 の値エスケープを戻す。
///
/// 規則 (VS Code のドキュメントより): `\` → `\\`、それ以外は `\xNN` の 16 進。
/// `;` (0x3b) と 0x20 以下は**必ず**逃がされている。
///
/// 壊れた入力 (`\` で終わる / `\xZZ`) はその 1 バイトをそのまま通す —
/// ここで諦めるとコマンド行ごと落ちるので、読める範囲は読む。
pub fn unescape_value(raw: &[u8]) -> String {
    fn hex(c: u8) -> Option<u8> {
        match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\' && i + 1 < raw.len() {
            match raw[i + 1] {
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                    continue;
                }
                b'x' | b'X' if i + 3 < raw.len() => {
                    if let (Some(h), Some(l)) = (hex(raw[i + 2]), hex(raw[i + 3])) {
                        out.push((h << 4) | l);
                        i += 4;
                        continue;
                    }
                }
                _ => {}
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// 文字境界を壊さずに `max` 文字で切る (切ったら省略記号を足す)。
fn truncate_chars(s: &mut String, max: usize) {
    if s.chars().count() <= max {
        return;
    }
    let cut = s.char_indices().nth(max).map(|(i, _)| i).unwrap_or(s.len());
    s.truncate(cut);
    s.push('…');
}

// ---------------------------------------------------------------------------
// 段
// ---------------------------------------------------------------------------

/// シェル統合の劣化段。**必ず UI に出す** (CLAUDE.md 設計原則 4)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum Tier {
    /// マーカーが 1 つも来ていない。判定は従来どおり画面推定へ降りる。
    #[default]
    None,
    /// 境界と終了コードは分かるが、コマンド行は分からない (OSC 133 だけ)。
    Basic,
    /// コマンド行まで分かる (OSC 633 `E`)。
    Rich,
}

impl Tier {
    /// UI に出す短い名前 (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            Tier::None => "無効",
            Tier::Basic => "基本",
            Tier::Rich => "完全",
        }
    }

    /// 「いま何段目に居るか」を 1 文字で。kanban の `Source::mark` と同じ思想。
    pub fn mark(self) -> &'static str {
        match self {
            Tier::None => "·",
            Tier::Basic => "◇",
            Tier::Rich => "◆",
        }
    }
}

// ---------------------------------------------------------------------------
// コマンドモデル
// ---------------------------------------------------------------------------

/// シェルが報告した 1 コマンド。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Command {
    /// コマンド行。[`Tier::Basic`] では空 (シェルが教えてくれない)。
    pub command_line: String,
    /// 終了コード。`D` に付いていなければ `None`。
    pub exit_code: Option<i32>,
    /// 実行時の作業ディレクトリ (`P;Cwd=`)。
    pub cwd: Option<PathBuf>,
    /// 実行開始 (`C`) の時刻 — トラッカー起点からのミリ秒。
    pub started_ms: u64,
    /// 実行終了 (`D`) の時刻 — 同上。
    pub finished_ms: u64,
}

impl Command {
    /// 実行に要した時間 (ミリ秒)。
    pub fn duration_ms(&self) -> u64 {
        self.finished_ms.saturating_sub(self.started_ms)
    }

    /// 成功したか。終了コード不明のときは `None` (「たぶん成功」にしない)。
    pub fn ok(&self) -> Option<bool> {
        self.exit_code.map(|c| c == 0)
    }

    /// 一覧に出す 1 行 (コマンド行が無い段でも意味のある文字列になる)。
    pub fn summary(&self) -> String {
        let head = if self.command_line.is_empty() {
            tr("(コマンド行は不明)")
        } else {
            self.command_line.replace('\n', " ")
        };
        match self.exit_code {
            Some(0) => trf("✓ {cmd}", &[("cmd", head)]),
            Some(c) => trf(
                "✕ {cmd} (code {code})",
                &[("cmd", head), ("code", c.to_string())],
            ),
            None => trf("· {cmd}", &[("cmd", head)]),
        }
    }
}

/// 実行中のコマンド (まだ `D` が来ていないもの)。
#[derive(Clone, Debug, Default)]
struct Pending {
    command_line: String,
    cwd: Option<PathBuf>,
    /// `B` を見た時刻。`C` が来たらそちらで上書きする。
    started_ms: u64,
    /// `C` を見たか = 本当に実行が始まったか。
    executing: bool,
}

// ---------------------------------------------------------------------------
// トラッカー
// ---------------------------------------------------------------------------

/// 1 端末ぶんのシェル統合状態。
///
/// PTY 読取スレッドから [`Tracker::feed`] され、UI スレッドから読まれる。
/// **読取スレッドを絶対に止めない**ため、ここでの仕事は
/// 「小さな構造体を 1 つ push するだけ」に保つ (設計原則 2)。
pub struct Tracker {
    origin: Instant,
    tier: Tier,
    ring: VecDeque<Command>,
    /// 上限超えで捨てた件数。**0 でない限り UI に出す** (黙って消さない)。
    dropped: u64,
    cur: Option<Pending>,
    /// 最後に何らかのマーカーを見た時刻 (段の鮮度判定に使う)。
    last_ms: Option<u64>,
    /// 直近に判明した cwd。次のコマンドの既定値になる。
    last_cwd: Option<PathBuf>,
    /// 直近の完了コマンドが失敗していたか (次のコマンドが始まるまで保持)。
    last_failure: Option<(String, i32)>,
    /// 段が変わった履歴 (「名前を付けて記録する」の実体)。
    tier_log: VecDeque<String>,
}

impl Default for Tracker {
    fn default() -> Self {
        Self::new()
    }
}

impl Tracker {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            tier: Tier::None,
            ring: VecDeque::new(),
            dropped: 0,
            cur: None,
            last_ms: None,
            last_cwd: None,
            last_failure: None,
            tier_log: VecDeque::new(),
        }
    }

    /// 起点からの経過ミリ秒。
    pub fn now_ms(&self) -> u64 {
        self.origin.elapsed().as_millis() as u64
    }

    /// 実時刻でマーカーを 1 つ流し込む (読取スレッドから呼ぶ)。
    pub fn feed(&mut self, m: Marker) {
        let now = self.now_ms();
        self.feed_at(m, now);
    }

    /// 時刻を明示してマーカーを流し込む (テスト用の決定的な入口)。
    pub fn feed_at(&mut self, m: Marker, now_ms: u64) {
        self.last_ms = Some(now_ms);
        if m.rich_evidence() {
            self.set_tier(Tier::Rich, now_ms);
        } else {
            // 何であれマーカーが来た時点で「基本」までは上がる。
            self.set_tier(Tier::Basic.max(self.tier), now_ms);
        }
        match m {
            Marker::PromptStart => {
                // `D` を取りこぼしたまま次のプロンプトが出た。実行中だった
                // ものは終了コード不明で畳む (画面に取り残さない)。
                if self.cur.as_ref().is_some_and(|p| p.executing) {
                    self.finish(None, now_ms);
                }
                self.cur = None;
            }
            Marker::PromptEnd => {
                // プロンプトが終わった = ここから先がユーザーの入力。
                self.cur = Some(Pending {
                    command_line: String::new(),
                    cwd: self.last_cwd.clone(),
                    started_ms: now_ms,
                    executing: false,
                });
            }
            Marker::CommandLine { line, .. } => {
                // pwsh は `E` を `B` の後 (PSConsoleHostReadLine) で出し、
                // bash/zsh は preexec で `C` の直前に出す。どちらでも拾えるよう
                // 「実行中のものがあればそれに、無ければ作って」当てる。
                let p = self.cur.get_or_insert_with(|| Pending {
                    cwd: self.last_cwd.clone(),
                    started_ms: now_ms,
                    ..Pending::default()
                });
                p.command_line = line;
            }
            Marker::PreExec => {
                let p = self.cur.get_or_insert_with(|| Pending {
                    cwd: self.last_cwd.clone(),
                    started_ms: now_ms,
                    ..Pending::default()
                });
                p.executing = true;
                p.started_ms = now_ms;
                // 新しいコマンドが始まったので、直前の失敗はもう「いま」ではない。
                self.last_failure = None;
            }
            Marker::Finished(code) => self.finish(code, now_ms),
            Marker::Cwd(path) => {
                self.last_cwd = Some(path.clone());
                if let Some(p) = self.cur.as_mut() {
                    p.cwd = Some(path);
                }
            }
            Marker::Property { .. } => {}
        }
    }

    fn set_tier(&mut self, t: Tier, now_ms: u64) {
        // 段は上げるだけ。一度コマンド行が取れたシェルが次から取れなくなる、
        // という劣化は起こらない (起こるなら発行元が壊れている)。
        if t <= self.tier {
            return;
        }
        let from = self.tier;
        self.tier = t;
        let line = trf(
            "[{ms}ms] シェル統合の段: {from} → {to}",
            &[
                ("ms", now_ms.to_string()),
                ("from", tr(from.label())),
                ("to", tr(t.label())),
            ],
        );
        if self.tier_log.len() >= MAX_TIER_LOG {
            self.tier_log.pop_front();
        }
        self.tier_log.push_back(line);
    }

    fn finish(&mut self, code: Option<i32>, now_ms: u64) {
        let Some(p) = self.cur.take() else {
            return;
        };
        // 何も実行していない Enter (プロンプトで改行しただけ) は記録しない。
        // pwsh は毎プロンプトで境界を出すので、これが無いと空行で埋まる。
        if !p.executing && p.command_line.is_empty() {
            return;
        }
        let cmd = Command {
            command_line: p.command_line,
            exit_code: code,
            cwd: p.cwd,
            started_ms: p.started_ms,
            finished_ms: now_ms,
        };
        self.last_failure = match cmd.exit_code {
            Some(c) if c != 0 => Some((cmd.command_line.clone(), c)),
            _ => None,
        };
        if self.ring.len() >= MAX_COMMANDS {
            self.ring.pop_front();
            self.dropped += 1;
        }
        self.ring.push_back(cmd);
    }

    /// 現在の段。
    pub fn tier(&self) -> Tier {
        self.tier
    }

    /// 段の変化ログを 1 つの文字列に畳む (何も変わっていなければ `None`)。
    /// ツールチップ 1 枚で「どうやってこの段になったか」を出すための形。
    pub fn tier_log_text(&self) -> Option<String> {
        (!self.tier_log.is_empty())
            .then(|| self.tier_log.iter().cloned().collect::<Vec<_>>().join("\n"))
    }

    /// **ギャップ標識** — 古い記録を捨てたことを明示する 1 行。
    ///
    /// 設計原則 2: 「捨てた箇所には明示的なギャップ標識を入れる」。
    /// 捨てていなければ `None` (空の行を作らない = UI の原則「空白は作らない」)。
    pub fn gap_note(&self) -> Option<String> {
        (self.dropped > 0).then(|| {
            trf(
                "⋯ 古い {n} 件は上限 ({max}) を超えたため捨てました",
                &[
                    ("n", self.dropped.to_string()),
                    ("max", MAX_COMMANDS.to_string()),
                ],
            )
        })
    }

    /// 直近のコマンド (新しい順、最大 `n` 件)。
    pub fn recent(&self, n: usize) -> Vec<&Command> {
        self.ring.iter().rev().take(n).collect()
    }

    /// 記録できているコマンド件数 (捨てたぶんは含まない)。
    pub fn recorded(&self) -> usize {
        self.ring.len()
    }

    /// 直近コマンドの終了コード。**毎フレームの描画経路から呼ばれる**ので、
    /// [`Command`] を写さず数値だけ返す (1 フレームに 1 アロケーションを作らない)。
    pub fn last_exit(&self) -> Option<i32> {
        self.ring.back()?.exit_code
    }

    /// いま実行中のコマンド行 (実行中でなければ `None`)。
    pub fn running_command(&self) -> Option<&str> {
        self.cur
            .as_ref()
            .filter(|p| p.executing)
            .map(|p| p.command_line.as_str())
    }

    /// **状態ラダーへの供給** — 画面を 1 文字も読まずに得られた判定。
    ///
    /// `stale_ms` を超えて無言なら `None` を返し、呼び出し側は下の段へ降りる。
    pub fn read(&self, now_ms: u64, stale_ms: u64) -> Option<ProtoRead> {
        let last = self.last_ms?;
        if now_ms.saturating_sub(last) > stale_ms {
            return None;
        }
        if self.tier == Tier::None {
            return None;
        }
        if let Some(p) = self.cur.as_ref().filter(|p| p.executing) {
            return Some(ProtoRead {
                state: ProtoState::Running,
                detail: p.command_line.clone(),
            });
        }
        // 直前のコマンドが落ちている間は「異常終了」。次のコマンドが始まれば
        // 消える (`PreExec` で last_failure を落としている)。
        // これが**文字列一致より強い**根拠: 終了コードは事実であって推定ではない。
        if let Some((cmd, code)) = self.last_failure.as_ref() {
            return Some(ProtoRead {
                state: ProtoState::Failed,
                detail: trf(
                    "{cmd} → code {code}",
                    &[("cmd", cmd.clone()), ("code", code.to_string())],
                ),
            });
        }
        Some(ProtoRead {
            state: ProtoState::Idle,
            detail: String::new(),
        })
    }

    /// 実時刻での [`Tracker::read`]。
    pub fn read_now(&self) -> Option<ProtoRead> {
        self.read(self.now_ms(), SHELL_STALE_MS)
    }
}

// ---------------------------------------------------------------------------
// シェルの種類とシム
// ---------------------------------------------------------------------------

/// 起動するシェルの種類。実行ファイル名から決める。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellKind {
    Bash,
    Zsh,
    Fish,
    /// pwsh (7+) / powershell.exe (5.1)
    PowerShell,
    /// cmd.exe など。シェル統合の手段が無い (VS Code も対応していない)。
    Unsupported,
}

impl ShellKind {
    /// 実行ファイルのパスから種類を決める。
    ///
    /// 大文字小文字と `.exe` を無視する — Windows の `Pwsh.EXE` も、
    /// macOS の `/opt/homebrew/bin/fish` も、同じ 1 本の規則で当てる。
    pub fn from_program(program: &str) -> Self {
        let stem = Path::new(program)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(program)
            .to_ascii_lowercase();
        match stem.as_str() {
            "bash" | "sh" => ShellKind::Bash,
            "zsh" => ShellKind::Zsh,
            "fish" => ShellKind::Fish,
            "pwsh" | "powershell" => ShellKind::PowerShell,
            _ => ShellKind::Unsupported,
        }
    }

    /// シムのファイル名 (拡張子込み)。
    fn shim_name(self) -> Option<&'static str> {
        match self {
            ShellKind::Bash => Some("zaivern.bash"),
            ShellKind::Zsh => Some("zdotdir"),
            ShellKind::Fish => Some("zaivern.fish"),
            ShellKind::PowerShell => Some("zaivern.ps1"),
            ShellKind::Unsupported => None,
        }
    }
}

/// シムの置き場。`~/.zaivern/shellint/` — ハードコードせず
/// [`crate::config::zaivern_dir`] から導出する (どのユーザー名・OS でも動く)。
pub fn install_dir() -> PathBuf {
    crate::config::zaivern_dir().join("shellint")
}

/// シムを `dir` へ書き出す。既に同じ内容なら書かない (毎起動の write を避ける)。
///
/// zsh だけはファイル 1 枚では足りない — `ZDOTDIR` を差し替える方式なので、
/// zsh が読む 4 つの起動ファイルすべてを用意して、それぞれが**本人の設定を
/// 先に読む**必要がある。
pub fn write_shims(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    write_if_changed(&dir.join("zaivern.bash"), BASH_SHIM)?;
    write_if_changed(&dir.join("zaivern.fish"), FISH_SHIM)?;
    write_if_changed(&dir.join("zaivern.ps1"), PWSH_SHIM)?;
    let zdot = dir.join("zdotdir");
    std::fs::create_dir_all(&zdot)?;
    write_if_changed(&zdot.join(".zshenv"), ZSH_ZSHENV)?;
    write_if_changed(&zdot.join(".zprofile"), ZSH_ZPROFILE)?;
    write_if_changed(&zdot.join(".zshrc"), ZSH_ZSHRC)?;
    write_if_changed(&zdot.join(".zlogin"), ZSH_ZLOGIN)?;
    Ok(())
}

fn write_if_changed(path: &Path, body: &str) -> std::io::Result<()> {
    if std::fs::read_to_string(path).is_ok_and(|cur| cur == body) {
        return Ok(());
    }
    std::fs::write(path, body)
}

/// 起動計画 — 「どのプログラムを、どの引数と環境変数で起動するか」の差分。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LaunchPlan {
    /// シェルへ渡す引数 (既存の `-l` などを**置き換える**)。
    pub args: Vec<String>,
    /// 追加で渡す環境変数。
    pub env: Vec<(String, String)>,
}

/// シェル統合つきの起動計画を組む **純関数**。
///
/// `shell` は実行ファイルのパス、`dir` はシムの置き場。
/// 対応していないシェル (cmd.exe 等) では `None` — 呼び出し側は従来どおり起動する。
///
/// # なぜ「コマンド指定なし」のときだけなのか
///
/// `$SHELL -lc "claude"` はプロンプトを 1 回も描かない。シェル統合が喋る
/// 契機 (プロンプトの前後) がそもそも存在しないので、注入しても何も起きず、
/// 起動経路だけが変わる = 損しかない。エージェント側の PTY には
/// 代わりに `ZAIVERN_AGENT=1` を渡す ([`agent_env`])。
pub fn launch_plan_for(shell: &str, dir: &Path, nonce: &str) -> Option<LaunchPlan> {
    let kind = ShellKind::from_program(shell);
    let name = kind.shim_name()?;
    let path = dir.join(name);
    let mut env = vec![
        ("ZAIVERN_SHELL_NONCE".to_string(), nonce.to_string()),
        // シム側が自分の居場所を知る必要はないが、ユーザーが `env` で
        // 「統合が効いているか」を確かめられるようにしておく。
        (
            "ZAIVERN_SHELL_INTEGRATION_DIR".to_string(),
            dir.display().to_string(),
        ),
    ];
    let args = match kind {
        ShellKind::Bash => {
            // `--init-file` は **対話かつ非ログイン**のときだけ読まれる。
            // つまり `-l` とは同時に使えない。PATH は build_command が
            // shellenv::user_path() で渡しているので、ログインシェルを
            // やめても `claude` が command not found にはならない。
            vec![
                "--init-file".to_string(),
                path.display().to_string(),
                "-i".to_string(),
            ]
        }
        ShellKind::Zsh => {
            // zsh は起動ファイルの場所を `ZDOTDIR` で丸ごと差し替えられる。
            // 本人の `ZDOTDIR` (未設定なら `$HOME`) は別名で渡し、
            // こちらの .zshrc 等が**先に本人の設定を読む**。
            let user = std::env::var("ZDOTDIR").unwrap_or_default();
            env.push(("ZAIVERN_ZDOTDIR_USER".to_string(), user));
            env.push(("ZDOTDIR".to_string(), path.display().to_string()));
            vec!["-l".to_string()]
        }
        ShellKind::Fish => {
            vec![
                "-l".to_string(),
                "-C".to_string(),
                format!("source {}", fish_quote(&path.display().to_string())),
            ]
        }
        ShellKind::PowerShell => {
            vec![
                "-NoExit".to_string(),
                "-Command".to_string(),
                format!(". {}", pwsh_quote(&path.display().to_string())),
            ]
        }
        ShellKind::Unsupported => return None,
    };
    Some(LaunchPlan { args, env })
}

/// fish の単一引用符で括る。fish では `'` と `\` だけがエスケープ対象。
fn fish_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('\'');
    out
}

/// PowerShell の単一引用符で括る。PowerShell では `'` を `''` に重ねる。
fn pwsh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// ユーザーの設定が**既に** OSC 133/633 を出しているか (二重発行の門番)。
///
/// 判定材料は 2 つ:
/// 1. 既知の統合が立てる環境変数 (iTerm2 / kitty / VS Code / WezTerm)。
/// 2. rc ファイルの本文に `133;` / `633;` が書かれていること
///    (starship や手書きの統合はここで見つかる)。
///
/// 判定は**シム側でも実行時にもう一度**行う (環境変数は起動してみないと
/// 分からないものがあるため)。ここは「そもそも注入する意味があるか」の
/// 事前判断で、UI に理由を出すために使う。
pub fn already_integrated(env: &dyn Fn(&str) -> Option<String>, rc_files: &[PathBuf]) -> bool {
    const MARKERS: [&str; 4] = [
        "ITERM_SHELL_INTEGRATION_INSTALLED",
        "KITTY_SHELL_INTEGRATION",
        "VSCODE_SHELL_INTEGRATION",
        "WEZTERM_SHELL_INTEGRATION",
    ];
    if MARKERS
        .iter()
        .any(|k| env(k).is_some_and(|v| !v.trim().is_empty()))
    {
        return true;
    }
    rc_files.iter().any(|p| {
        std::fs::read_to_string(p)
            .map(|s| s.contains("133;") || s.contains("633;"))
            .unwrap_or(false)
    })
}

/// 既定で調べる rc ファイル。ホームは `dirs` から導出する (直書き禁止)。
pub fn default_rc_files() -> Vec<PathBuf> {
    let Some(home) = dirs::home_dir() else {
        return Vec::new();
    };
    let zdot = std::env::var("ZDOTDIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(|| home.clone());
    vec![
        home.join(".bashrc"),
        home.join(".bash_profile"),
        zdot.join(".zshrc"),
        home.join(".config").join("fish").join("config.fish"),
    ]
}

// ---------------------------------------------------------------------------
// 有効/無効 (オプトイン)
// ---------------------------------------------------------------------------

/// 注入が有効か。既定 **false** — 入れただけでは起動経路が変わらない。
static ENABLED: AtomicBool = AtomicBool::new(false);

/// 注入の有効/無効を切り替える。有効化した時点でシムを書き出す
/// (起動のたびに書くとホームへの write が毎回増える)。
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
    if on {
        let _ = write_shims(&install_dir());
    }
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// 今回の起動につかう nonce。
///
/// 「その `E` は本当にこちらが仕込んだシェルが出したものか」を確かめる印。
/// 端末に流れてくるバイト列は誰でも書けるので、無いと偽のコマンド行を
/// 信じ込まされる。プロセス起動ごとに 1 つで足りる。
pub fn nonce() -> &'static str {
    use std::sync::OnceLock;
    static NONCE: OnceLock<String> = OnceLock::new();
    NONCE.get_or_init(|| {
        // 依存を増やさない範囲で十分に散らす: 起動時刻 (ns) と PID。
        let ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let pid = std::process::id();
        format!("{ns:x}{pid:x}")
    })
}

/// `build_command` から呼ぶ入口。無効 / 未対応シェル / コマンド指定ありなら `None`。
pub fn launch_plan(shell: &str, has_command: bool) -> Option<LaunchPlan> {
    if !enabled() || has_command {
        return None;
    }
    // 対応していないシェル (cmd.exe 等) はディスクに触れる前に帰る。
    // シムの書き出しは `~/.zaivern` への write なので、無駄撃ちしない。
    ShellKind::from_program(shell).shim_name()?;
    let dir = install_dir();
    // 有効化時に書いているが、`~/.zaivern` を消された後でも復活できるようにする。
    if !dir.join("zaivern.bash").exists() {
        write_shims(&dir).ok()?;
    }
    launch_plan_for(shell, &dir, nonce())
}

/// エージェント PTY へ渡す印。
///
/// VS Code は 120 コメントの issue の末に `VSCODE_AGENT` を出した。
/// 原因は「プロンプトフレームワーク (bash-preexec / 自作 PROMPT_COMMAND /
/// bash-git-prompt) がエージェント端末を固める」で、**エージェントのシェルだけ**
/// それらを切る手段が要る、というものだった。同じ issue には
/// **狭い端末での固まり**も報告されている (39 桁、次いで 2 分割で 25 桁。
/// 「2 行目へ折り返す文字列なら何でも」)。横並びのエージェント操縦席は
/// まさにその形なので、こちらでも同じ逃げ道を用意する。
pub fn agent_env() -> [(&'static str, &'static str); 1] {
    [("ZAIVERN_AGENT", "1")]
}

// ---------------------------------------------------------------------------
// シム本体
// ---------------------------------------------------------------------------
//
// 共通の約束:
//  - 二重発行しない。ユーザーの設定が既に OSC を出していたら**何もしない**。
//  - 値は OSC 633 の規則で逃がす (`\` → `\\`、`;` → `\x3b`、制御文字 → `\xNN`)。
//  - 失敗しても対話シェルとしては普通に使える (統合が無いだけ)。

const BASH_SHIM: &str = r#"# Zaivern Code — bash シェル統合 (OSC 633 / OSC 133)
# 生成物です。直接編集しても再生成で上書きされます。
#
# `bash --init-file <このファイル> -i` から読まれる。--init-file は
# ~/.bashrc の**代わり**に読まれるので、まず本人の設定を読み直す。

if [ -n "${ZAIVERN_SHELL_INTEGRATION:-}" ]; then
	return
fi
ZAIVERN_SHELL_INTEGRATION=1

if [ -r "$HOME/.bashrc" ]; then
	. "$HOME/.bashrc"
fi

# ── 二重発行の門番 ──────────────────────────────────────────────
# iTerm2 / kitty / VS Code / 手書きの統合が既に居るなら、こちらは足さない。
# 1 コマンドが 2 件に見える壊れ方をするので、迷ったら降りる。
case "${PROMPT_COMMAND:-}${PS1:-}" in
	*'133;'*|*'633;'*) ZAIVERN_SHELL_INTEGRATION=external; return ;;
esac
if [ -n "${ITERM_SHELL_INTEGRATION_INSTALLED:-}" ] \
	|| [ -n "${KITTY_SHELL_INTEGRATION:-}" ] \
	|| [ -n "${VSCODE_SHELL_INTEGRATION:-}" ]; then
	ZAIVERN_SHELL_INTEGRATION=external
	return
fi

__zv_nonce="${ZAIVERN_SHELL_NONCE:-}"
__zv_executing=""
__zv_ps1_mark='\[\e]633;B\a\]'

# OSC 633 の値エスケープ。bash 3.2 (macOS 同梱) でも動く置換だけを使う。
# 逃がす順は `\` が先 — 後にすると自分が入れた `\x3b` の `\` まで二重化する。
__zv_escape() {
	local s="$1"
	s="${s//\\/\\\\}"
	s="${s//;/\\x3b}"
	s="${s//$'\n'/\\x0a}"
	s="${s//$'\r'/\\x0d}"
	s="${s//$'\t'/\\x09}"
	printf '%s' "$s"
}

# 打った行**そのもの**を取る。DEBUG トラップの $BASH_COMMAND は
# 「いま実行しようとしている 1 個の単純コマンド」なので、
# `echo a; echo b` が `echo a` に化ける (実測)。履歴の先頭なら丸ごと取れる。
# 履歴が無効な環境では空になるので、そのときだけ $BASH_COMMAND へ落ちる。
__zv_last_history() {
	local h
	h=$(builtin history 1 2>/dev/null)
	# "  123  echo a; echo b" → "echo a; echo b" (bash 3.2 でも動く剥がし方)
	h="${h#"${h%%[!0-9 ]*}"}"
	printf '%s' "$h"
}

__zv_preexec() {
	[ -n "$__zv_executing" ] && return
	__zv_executing=1
	local line
	line=$(__zv_last_history)
	[ -z "$line" ] && line="$1"
	printf '\033]633;E;%s;%s\007' "$(__zv_escape "$line")" "$__zv_nonce"
	printf '\033]633;C\007'
}

__zv_precmd() {
	local ec=$?
	if [ -n "$__zv_executing" ]; then
		printf '\033]633;D;%s\007' "$ec"
	fi
	__zv_executing=""
	printf '\033]633;P;Cwd=%s\007' "$(__zv_escape "$PWD")"
	printf '\033]633;A\007'
	# プロンプト本体の直後に B を出す。フレームワークが PS1 を毎回
	# 組み直しても付け直せるよう、毎回「入っているか」を見る。
	case "$PS1" in
		*'633;B'*) ;;
		*) PS1="$PS1$__zv_ps1_mark" ;;
	esac
}

# bash-preexec が既に居るならその枠組みへ相乗りする。DEBUG トラップを
# 上書きすると bash-preexec 側が壊れ、端末が固まる (VS Code の issue の主因)。
if [ -n "${bash_preexec_imported:-}${__bp_imported:-}" ]; then
	preexec_functions+=(__zv_preexec)
	precmd_functions+=(__zv_precmd)
else
	# 「プロンプトの処理が終わった」印。DEBUG トラップは PROMPT_COMMAND の
	# 中で走る内部コマンドにも反応するので、これが無いとユーザーの
	# PROMPT_COMMAND の断片が「1 件目のコマンド」として記録される。
	# 印を立てるのは PROMPT_COMMAND の**最後**なので、内部コマンドの時点では
	# まだ立っていない = 数えられない。
	__zv_arm() { __zv_armed=1; }
	__zv_debug_trap() {
		[ -z "${__zv_armed:-}" ] && return
		# 補完中 (COMP_LINE あり) は実行ではない。
		[ -n "${COMP_LINE:-}" ] && return
		__zv_armed=""
		__zv_preexec "$BASH_COMMAND"
	}
	if [ -n "${PROMPT_COMMAND:-}" ]; then
		PROMPT_COMMAND="__zv_precmd; $PROMPT_COMMAND; __zv_arm"
	else
		PROMPT_COMMAND="__zv_precmd; __zv_arm"
	fi
	# DEBUG トラップは**この初期化ファイルの最後**に仕掛ける。先に仕掛けると
	# 直後の自分自身の行が「1 つ目のコマンド」として記録される
	# (実測: `E;[ -n "${PROMPT_COMMAND:-}" ]` という幽霊が 1 件残った)。
	trap '__zv_debug_trap' DEBUG
fi
"#;

const ZSH_ZSHENV: &str = r#"# Zaivern Code — zsh シェル統合 (ZDOTDIR 差し替え)。生成物です。
# 本人の起動ファイルを先に読む。未設定なら $HOME が本来の ZDOTDIR。
if [ -z "${ZAIVERN_ZDOTDIR_USER:-}" ]; then
	ZAIVERN_ZDOTDIR_USER="$HOME"
fi
ZAIVERN_ZDOTDIR_SELF="$ZDOTDIR"
[ -f "$ZAIVERN_ZDOTDIR_USER/.zshenv" ] && . "$ZAIVERN_ZDOTDIR_USER/.zshenv"
# 本人の .zshenv が ZDOTDIR を動かしたら、それを「本来の場所」として覚え直し、
# 残りの起動ファイルはこちらへ戻す (そうしないと以降を乗っ取れない)。
if [ "$ZDOTDIR" != "$ZAIVERN_ZDOTDIR_SELF" ]; then
	ZAIVERN_ZDOTDIR_USER="$ZDOTDIR"
	ZDOTDIR="$ZAIVERN_ZDOTDIR_SELF"
fi
"#;

const ZSH_ZPROFILE: &str = r#"# Zaivern Code — zsh シェル統合。生成物です。
[ -f "$ZAIVERN_ZDOTDIR_USER/.zprofile" ] && . "$ZAIVERN_ZDOTDIR_USER/.zprofile"
"#;

const ZSH_ZLOGIN: &str = r#"# Zaivern Code — zsh シェル統合。生成物です。
[ -f "$ZAIVERN_ZDOTDIR_USER/.zlogin" ] && . "$ZAIVERN_ZDOTDIR_USER/.zlogin"
"#;

const ZSH_ZSHRC: &str = r#"# Zaivern Code — zsh シェル統合 (OSC 633)。生成物です。
[ -f "$ZAIVERN_ZDOTDIR_USER/.zshrc" ] && . "$ZAIVERN_ZDOTDIR_USER/.zshrc"

# ── 二重発行の門番 ──────────────────────────────────────────────
if [[ -n "${ITERM_SHELL_INTEGRATION_INSTALLED:-}" \
	|| -n "${KITTY_SHELL_INTEGRATION:-}" \
	|| -n "${VSCODE_SHELL_INTEGRATION:-}" \
	|| "$PS1" == *'133;'* || "$PS1" == *'633;'* \
	|| "${precmd_functions[*]}" == *iterm2* ]]; then
	ZAIVERN_SHELL_INTEGRATION=external
	return
fi
ZAIVERN_SHELL_INTEGRATION=1

__zv_nonce="${ZAIVERN_SHELL_NONCE:-}"
__zv_executing=""

__zv_escape() {
	local s="$1"
	s="${s//\\/\\\\}"
	s="${s//;/\\x3b}"
	s="${s//$'\n'/\\x0a}"
	s="${s//$'\r'/\\x0d}"
	s="${s//$'\t'/\\x09}"
	print -nr -- "$s"
}

__zv_preexec() {
	__zv_executing=1
	print -nr -- $'\033]633;E;'"$(__zv_escape "$1")"$';'"$__zv_nonce"$'\007'
	print -nr -- $'\033]633;C\007'
}

__zv_precmd() {
	local ec=$?
	if [[ -n "$__zv_executing" ]]; then
		print -nr -- $'\033]633;D;'"$ec"$'\007'
	fi
	__zv_executing=""
	print -nr -- $'\033]633;P;Cwd='"$(__zv_escape "$PWD")"$'\007'
	print -nr -- $'\033]633;A\007'
	if [[ "$PS1" != *'633;B'* ]]; then
		PS1="$PS1"$'%{\033]633;B\007%}'
	fi
}

autoload -Uz add-zsh-hook
add-zsh-hook precmd __zv_precmd
add-zsh-hook preexec __zv_preexec

[ -f "$ZAIVERN_ZDOTDIR_USER/.zshrc.local" ] && . "$ZAIVERN_ZDOTDIR_USER/.zshrc.local"
"#;

const FISH_SHIM: &str = r#"# Zaivern Code — fish シェル統合 (OSC 633)。生成物です。
# `fish -l -C 'source <このファイル>'` から読まれる。

if set -q ZAIVERN_SHELL_INTEGRATION
	exit 0
end

# 二重発行の門番。fish の config.fish は -C の**後**に読まれるので、
# 判定は最初のプロンプトまで遅らせる (--on-event fish_prompt の中で見る)。
set -g ZAIVERN_SHELL_INTEGRATION 1
set -g __zv_wrapped 0

function __zv_escape --argument-names value
	set -l s (string replace --all -- '\\' '\\\\' $value)
	set s (string replace --all -- ';' '\x3b' $s)
	set s (string replace --all -- \n '\x0a' $s)
	set s (string replace --all -- \t '\x09' $s)
	printf '%s' $s
end

function __zv_preexec --on-event fish_preexec --argument-names cmd
	printf '\033]633;E;%s;%s\007' (__zv_escape $cmd) "$ZAIVERN_SHELL_NONCE"
	printf '\033]633;C\007'
end

function __zv_postexec --on-event fish_postexec
	printf '\033]633;D;%s\007' $status
end

function __zv_install_prompt --on-event fish_prompt
	test $__zv_wrapped -eq 1; and return
	set -g __zv_wrapped 1
	# 既に誰かが 133/633 を出しているなら降りる。
	if functions -q fish_prompt; and string match -q '*133;*' -- (functions fish_prompt)
		set -g ZAIVERN_SHELL_INTEGRATION external
		functions -e __zv_preexec __zv_postexec
		return
	end
	functions --copy fish_prompt __zv_original_fish_prompt
	function fish_prompt
		printf '\033]633;P;Cwd=%s\007' (__zv_escape "$PWD")
		printf '\033]633;A\007'
		__zv_original_fish_prompt
		printf '\033]633;B\007'
	end
end
"#;

const PWSH_SHIM: &str = r#"# Zaivern Code — PowerShell シェル統合 (OSC 633)。生成物です。
# `pwsh -NoExit -Command ". '<このファイル>'"` から読まれる。

if ($env:ZAIVERN_SHELL_INTEGRATION) { return }
if ($env:VSCODE_SHELL_INTEGRATION) {
	$env:ZAIVERN_SHELL_INTEGRATION = "external"
	return
}
$env:ZAIVERN_SHELL_INTEGRATION = "1"

$Global:__ZvNonce = $env:ZAIVERN_SHELL_NONCE
$Global:__ZvOriginalPrompt = $function:Prompt
$Global:__ZvFirstPrompt = $true

# OSC 633 の値エスケープ。`\` を先に処理する (後だと自分の `\x3b` を壊す)。
function Global:__Zv-Escape([string] $Value) {
	if ($null -eq $Value) { return "" }
	$Value = $Value.Replace('\', '\\')
	$Value = $Value.Replace(';', '\x3b')
	$Value = $Value.Replace("`n", '\x0a')
	$Value = $Value.Replace("`r", '\x0d')
	$Value = $Value.Replace("`t", '\x09')
	return $Value
}

function Global:Prompt() {
	# $? と $LASTEXITCODE は**最初に**取る。以降のどの式でも壊れる。
	$LastSucceeded = $?
	$LastExit = $global:LASTEXITCODE
	$Out = ""
	if (-not $Global:__ZvFirstPrompt) {
		$Code = 0
		if ($null -ne $LastExit) { $Code = $LastExit }
		elseif (-not $LastSucceeded) { $Code = 1 }
		$Out += "$([char]0x1b)]633;D;$Code$([char]0x07)"
	}
	$Global:__ZvFirstPrompt = $false
	$Out += "$([char]0x1b)]633;A$([char]0x07)"
	$Cwd = __Zv-Escape (Get-Location).Path
	$Out += "$([char]0x1b)]633;P;Cwd=$Cwd$([char]0x07)"
	$Out += $Global:__ZvOriginalPrompt.Invoke()
	$Out += "$([char]0x1b)]633;B$([char]0x07)"
	# 元の終了コードを戻す。ここで書き換えると `$LASTEXITCODE` が
	# プロンプトの副作用で汚れる。
	$global:LASTEXITCODE = $LastExit
	return $Out
}

# PowerShell には preexec が無い。PSReadLine が入力を返す関数
# (PSConsoleHostReadLine) を包むのが唯一「打った行そのもの」を取れる場所。
# 入っていなければコマンド行は分からない = 段は「基本」に留まる (それでよい)。
if (Get-Command PSConsoleHostReadLine -ErrorAction SilentlyContinue) {
	$Global:__ZvOriginalReadLine = $function:PSConsoleHostReadLine
	function Global:PSConsoleHostReadLine {
		$Line = $Global:__ZvOriginalReadLine.Invoke()
		$Escaped = __Zv-Escape $Line
		[Console]::Write("$([char]0x1b)]633;E;$Escaped;$($Global:__ZvNonce)$([char]0x07)")
		[Console]::Write("$([char]0x1b)]633;C$([char]0x07)")
		return $Line
	}
}
"#;

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// terminal.rs の OSC パーサと同じ入口を通してマーカー列を得る。
    /// **バイト列**でテストするのが要点 — 描画結果には一切触らない。
    fn markers(bytes: &[u8]) -> Vec<Marker> {
        let mut s = crate::terminal::QueryScanner::default();
        s.scan(bytes)
            .into_iter()
            .filter_map(|e| match e {
                crate::terminal::TermEvent::Shell(m) => Some(m),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn osc133の各文字を読む() {
        assert_eq!(parse_osc(b"133", b"A"), Some(Marker::PromptStart));
        assert_eq!(parse_osc(b"133", b"B"), Some(Marker::PromptEnd));
        assert_eq!(parse_osc(b"133", b"C"), Some(Marker::PreExec));
        assert_eq!(parse_osc(b"133", b"D"), Some(Marker::Finished(None)));
        assert_eq!(parse_osc(b"133", b"D;0"), Some(Marker::Finished(Some(0))));
        assert_eq!(
            parse_osc(b"133", b"D;130"),
            Some(Marker::Finished(Some(130))),
            "SIGINT の 130 も普通の終了コード"
        );
        // 633 の拡張は 133 では受けない (存在しない発行元を想定しない)
        assert_eq!(parse_osc(b"133", b"E;ls"), None);
        assert_eq!(parse_osc(b"133", b"P;Cwd=/x"), None);
    }

    #[test]
    fn osc133の余分なパラメータは捨てる() {
        // kitty は `133;A;k=s`、WezTerm は `133;D;1;aid=…` のように足してくる。
        assert_eq!(parse_osc(b"133", b"A;k=s"), Some(Marker::PromptStart));
        assert_eq!(
            parse_osc(b"133", b"D;3;aid=7"),
            Some(Marker::Finished(Some(3)))
        );
        assert_eq!(
            parse_osc(b"133", b"D;notanumber"),
            Some(Marker::Finished(None)),
            "数字でないものを 0 と読んではいけない"
        );
    }

    #[test]
    fn osc633のeはnonceの有無どちらも読む() {
        assert_eq!(
            parse_osc(b"633", b"E;cargo test"),
            Some(Marker::CommandLine {
                line: "cargo test".into(),
                nonce: String::new()
            })
        );
        assert_eq!(
            parse_osc(b"633", b"E;cargo test;abc123"),
            Some(Marker::CommandLine {
                line: "cargo test".into(),
                nonce: "abc123".into()
            })
        );
    }

    #[test]
    fn osc633のeはセミコロンを含むコマンド行を壊さない() {
        // 仕様どおり `;` は `\x3b` で逃がされて届く。ここを素朴に
        // 「最初の ; で切る」だけにすると、コマンド行が半分になる。
        let m = parse_osc(b"633", b"E;make a\\x3b make b;n0").expect("E");
        assert_eq!(
            m,
            Marker::CommandLine {
                line: "make a; make b".into(),
                nonce: "n0".into()
            }
        );
    }

    #[test]
    fn 値エスケープを戻す() {
        assert_eq!(unescape_value(b"a\\\\b"), "a\\b");
        assert_eq!(unescape_value(b"a\\x0ab"), "a\nb");
        assert_eq!(unescape_value(b"\\x3b"), ";");
        // 壊れた入力でも落とさず、読める範囲は読む
        assert_eq!(unescape_value(b"a\\"), "a\\");
        assert_eq!(unescape_value(b"a\\xZZ"), "a\\xZZ");
    }

    #[test]
    fn 長すぎるコマンド行は切る() {
        let long = "x".repeat(MAX_COMMAND_CHARS + 100);
        let body = format!("E;{long}");
        let Some(Marker::CommandLine { line, .. }) = parse_osc(b"633", body.as_bytes()) else {
            panic!("E が読めない");
        };
        assert_eq!(line.chars().count(), MAX_COMMAND_CHARS + 1, "省略記号ぶん");
        assert!(line.ends_with('…'));
    }

    #[test]
    fn cwdプロパティを読む() {
        // パスは環境ごとに違うので、テストでも直書きしない。
        let dir = crate::test_util::unique_temp_dir("shellint", "cwd");
        let esc = dir.display().to_string().replace(';', "\\x3b");
        let body = format!("P;Cwd={esc}");
        assert_eq!(parse_osc(b"633", body.as_bytes()), Some(Marker::Cwd(dir)));
        assert_eq!(
            parse_osc(b"633", b"P;IsWindows=True"),
            Some(Marker::Property {
                key: "IsWindows".into(),
                value: "True".into()
            })
        );
    }

    #[test]
    fn belとstで終端されたoscをどちらも読む() {
        assert_eq!(markers(b"\x1b]133;A\x07"), vec![Marker::PromptStart]);
        assert_eq!(markers(b"\x1b]133;A\x1b\\"), vec![Marker::PromptStart]);
    }

    #[test]
    fn 通常出力に混ざっても読める() {
        let bytes = b"hello\x1b]633;B\x07world\x1b]633;C\x07out\r\n\x1b]633;D;0\x07";
        assert_eq!(
            markers(bytes),
            vec![
                Marker::PromptEnd,
                Marker::PreExec,
                Marker::Finished(Some(0))
            ]
        );
    }

    #[test]
    fn ptyの読み取り2回に割れても読める() {
        // 8KB のチャンク境界は OSC の途中に平気で落ちる。**必ず起きる**ので
        // ここが割れて欠けると、たまにコマンドが 1 件消える形で壊れる。
        let mut s = crate::terminal::QueryScanner::default();
        let all = b"\x1b]633;E;cargo build;n1\x07\x1b]633;C\x07";
        for split in 1..all.len() {
            let mut s2 = crate::terminal::QueryScanner::default();
            let mut got = Vec::new();
            for ev in s2.scan(&all[..split]) {
                if let crate::terminal::TermEvent::Shell(m) = ev {
                    got.push(m);
                }
            }
            for ev in s2.scan(&all[split..]) {
                if let crate::terminal::TermEvent::Shell(m) = ev {
                    got.push(m);
                }
            }
            assert_eq!(
                got,
                vec![
                    Marker::CommandLine {
                        line: "cargo build".into(),
                        nonce: "n1".into()
                    },
                    Marker::PreExec
                ],
                "{split} バイト目で割れたときに落ちた"
            );
        }
        // 途中で終わったぶんは pending に残るだけで、イベントは出ない
        assert!(s.scan(b"\x1b]633;E;half").is_empty());
    }

    #[test]
    fn 閉じないoscは上限で捨てられる() {
        let mut s = crate::terminal::QueryScanner::default();
        let mut seq = b"\x1b]633;E;".to_vec();
        seq.extend(std::iter::repeat_n(b'x', 128 * 1024));
        assert!(s.scan(&seq).is_empty());
        // 上限を超えた持ち越しは捨てる = 無限に太らない
        assert!(s.scan(b"\x07").is_empty(), "捨てた後の残骸を読み直さない");
    }

    // ── 折り返し ─────────────────────────────────────────────────

    #[test]
    fn 狭い端末で折り返してもコマンド行は完全に取れる() {
        // VS Code の problem-matcher バグ群の正体は「整形済みグリッドを読む」こと。
        // 25 桁 / 39 桁 (実際に固まりが報告された幅) でグリッドは折り返すが、
        // こちらはバイト列を読むので**1 文字も変わらない**。
        let cmd = "cargo test --workspace --all-features -- --nocapture shellint";
        let bytes = format!("\x1b]633;E;{cmd};n\x07\x1b]633;C\x07{cmd}\r\n\x1b]633;D;0\x07");
        for cols in [25u16, 39, 110] {
            let got = markers(bytes.as_bytes());
            assert_eq!(
                got[0],
                Marker::CommandLine {
                    line: cmd.into(),
                    nonce: "n".into()
                },
                "{cols} 桁でコマンド行が変わってはいけない"
            );
            // 同じバイト列をグリッドへ流すと、狭い幅では実際に折り返る。
            // 比べる相手は `rows()` (画面 1 行ずつ = 描画とマッチャが見る形)。
            // `contents()` は折り返しを繋ぎ直して返すので、ここでは使えない。
            let mut p = vt100::Parser::new(10, cols, 100);
            p.process(bytes.as_bytes());
            let rows: Vec<String> = p.screen().rows(0, cols).collect();
            let intact = rows.iter().any(|l| l.trim_end() == cmd);
            assert_eq!(
                intact,
                cols >= 60,
                "{cols} 桁: グリッドの 1 行にコマンドが丸ごと残るか (前提の確認)"
            );
        }
    }

    // ── トラッカー ───────────────────────────────────────────────

    fn run(t: &mut Tracker, cmd: &str, code: i32, base: u64) {
        t.feed_at(Marker::PromptStart, base);
        t.feed_at(Marker::PromptEnd, base + 1);
        t.feed_at(
            Marker::CommandLine {
                line: cmd.into(),
                nonce: "n".into(),
            },
            base + 2,
        );
        t.feed_at(Marker::PreExec, base + 3);
        t.feed_at(Marker::Finished(Some(code)), base + 10);
    }

    #[test]
    fn 一連のマーカーから1件のコマンドを組み立てる() {
        let mut t = Tracker::new();
        assert_eq!(t.tier(), Tier::None);
        run(&mut t, "cargo test", 0, 1000);
        assert_eq!(t.tier(), Tier::Rich);
        assert_eq!(t.recorded(), 1);
        let c = t.recent(1)[0];
        assert_eq!(c.command_line, "cargo test");
        assert_eq!(c.exit_code, Some(0));
        assert_eq!(c.duration_ms(), 7, "C から D までを測る");
        assert_eq!(c.ok(), Some(true));
    }

    #[test]
    fn osc133だけなら段は基本のまま() {
        let mut t = Tracker::new();
        t.feed_at(Marker::PromptStart, 0);
        t.feed_at(Marker::PromptEnd, 1);
        t.feed_at(Marker::PreExec, 2);
        t.feed_at(Marker::Finished(Some(2)), 3);
        assert_eq!(t.tier(), Tier::Basic, "コマンド行が来ないので完全ではない");
        assert_eq!(t.recorded(), 1);
        assert_eq!(t.recent(1)[0].command_line, "", "無いものを捏造しない");
        assert_eq!(t.recent(1)[0].exit_code, Some(2));
    }

    #[test]
    fn 段の変化は記録される() {
        let mut t = Tracker::new();
        assert_eq!(t.tier_log_text(), None, "最初は空 = UI に空欄を作らない");
        t.feed_at(Marker::PromptStart, 5);
        t.feed_at(
            Marker::CommandLine {
                line: "ls".into(),
                nonce: String::new(),
            },
            9,
        );
        let log = t.tier_log_text().expect("2 段ぶんの変化が記録されている");
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines.len(), 2, "無効→基本→完全: {log}");
        assert!(lines[0].contains("基本"), "{log}");
        assert!(lines[1].contains("完全"), "{log}");
    }

    #[test]
    fn プロパティで完全段へ上がる() {
        let mut t = Tracker::new();
        t.feed_at(
            Marker::Property {
                key: "HasRichCommandDetection".into(),
                value: "True".into(),
            },
            0,
        );
        assert_eq!(t.tier(), Tier::Rich);
    }

    #[test]
    fn 空のenterは記録しない() {
        let mut t = Tracker::new();
        t.feed_at(Marker::PromptStart, 0);
        t.feed_at(Marker::PromptEnd, 1);
        t.feed_at(Marker::Finished(Some(0)), 2);
        assert_eq!(t.recorded(), 0, "何も実行していない改行で履歴を埋めない");
    }

    #[test]
    fn dを取りこぼしても次のプロンプトで畳む() {
        let mut t = Tracker::new();
        t.feed_at(Marker::PromptEnd, 0);
        t.feed_at(
            Marker::CommandLine {
                line: "sleep".into(),
                nonce: String::new(),
            },
            1,
        );
        t.feed_at(Marker::PreExec, 2);
        t.feed_at(Marker::PromptStart, 50);
        assert_eq!(t.recorded(), 1);
        assert_eq!(t.recent(1)[0].exit_code, None, "不明を 0 と偽らない");
    }

    #[test]
    fn 上限を超えたらギャップ標識を出す() {
        let mut t = Tracker::new();
        assert_eq!(t.gap_note(), None, "捨てていないなら行を作らない");
        for i in 0..(MAX_COMMANDS + 5) {
            run(&mut t, &format!("cmd{i}"), 0, i as u64 * 100);
        }
        assert_eq!(t.recorded(), MAX_COMMANDS);
        let note = t.gap_note().expect("捨てたなら必ず出す");
        assert!(note.contains('5'), "捨てた 5 件が標識に出ていない: {note}");
        assert_eq!(
            t.recent(1)[0].command_line,
            format!("cmd{}", MAX_COMMANDS + 4),
            "新しい方を残す"
        );
    }

    #[test]
    fn cwdは次のコマンドへ引き継がれる() {
        let dir = crate::test_util::unique_temp_dir("shellint", "carry");
        let mut t = Tracker::new();
        t.feed_at(Marker::Cwd(dir.clone()), 0);
        run(&mut t, "ls", 0, 10);
        assert_eq!(t.recent(1)[0].cwd.as_deref(), Some(dir.as_path()));
    }

    // ── ラダーへの供給 ───────────────────────────────────────────

    #[test]
    fn 実行中は実行中として読める() {
        let mut t = Tracker::new();
        t.feed_at(Marker::PromptEnd, 0);
        t.feed_at(
            Marker::CommandLine {
                line: "cargo build".into(),
                nonce: String::new(),
            },
            1,
        );
        t.feed_at(Marker::PreExec, 2);
        let r = t.read(3, SHELL_STALE_MS).expect("読めるはず");
        assert_eq!(r.state, ProtoState::Running);
        assert_eq!(r.detail, "cargo build");
    }

    #[test]
    fn 失敗は終了コードで確定し次のコマンドで消える() {
        let mut t = Tracker::new();
        run(&mut t, "cargo test", 1, 0);
        let r = t.read(20, SHELL_STALE_MS).expect("読めるはず");
        assert_eq!(r.state, ProtoState::Failed);
        assert!(r.detail.contains("code 1"), "{}", r.detail);
        // 次のコマンドが始まったら「いまの状態」ではなくなる
        t.feed_at(Marker::PromptEnd, 30);
        t.feed_at(Marker::PreExec, 31);
        assert_eq!(
            t.read(32, SHELL_STALE_MS).unwrap().state,
            ProtoState::Running
        );
    }

    #[test]
    fn 成功したコマンドはerrorという文字列があっても失敗にしない() {
        // CLAUDE.md の傷: `Read(src/error_handling.rs)` を「エラー」に数えた。
        // 終了コード 0 は**事実**なので、文字列一致の誤判定をここで否定できる。
        let mut t = Tracker::new();
        run(&mut t, "rg error_handling", 0, 0);
        assert_eq!(t.recent(1)[0].ok(), Some(true));
        assert_eq!(t.read(5, SHELL_STALE_MS).unwrap().state, ProtoState::Idle);
    }

    #[test]
    fn 無言が続けば段を降りる() {
        let mut t = Tracker::new();
        run(&mut t, "ls", 0, 0);
        assert!(t.read(SHELL_STALE_MS, SHELL_STALE_MS).is_some());
        assert!(
            t.read(SHELL_STALE_MS + 1000, SHELL_STALE_MS).is_none(),
            "死んだシェルを信じ続けない"
        );
    }

    #[test]
    fn マーカーが無ければ何も言わない() {
        let t = Tracker::new();
        assert!(t.read(0, SHELL_STALE_MS).is_none());
        assert_eq!(t.recorded(), 0);
        assert_eq!(
            t.tier_log_text(),
            None,
            "変化が無いなら UI に空行を作らない"
        );
    }

    // ── シェル判定と起動計画 ─────────────────────────────────────

    #[test]
    fn シェルの種類を実行ファイル名から決める() {
        // パスは環境依存なので、判定はファイル名だけに依らせる。
        for (p, k) in [
            ("bash", ShellKind::Bash),
            ("/bin/bash", ShellKind::Bash),
            ("/usr/local/bin/zsh", ShellKind::Zsh),
            ("/opt/homebrew/bin/fish", ShellKind::Fish),
            ("pwsh", ShellKind::PowerShell),
            ("powershell.exe", ShellKind::PowerShell),
            ("PowerShell.EXE", ShellKind::PowerShell),
            ("cmd.exe", ShellKind::Unsupported),
            ("nu", ShellKind::Unsupported),
        ] {
            assert_eq!(ShellKind::from_program(p), k, "{p}");
        }
    }

    #[test]
    fn 起動計画は各シェルの正しい入口を使う() {
        let dir = crate::test_util::unique_temp_dir("shellint", "plan");
        let bash = launch_plan_for("/bin/bash", &dir, "n").expect("bash");
        assert_eq!(bash.args[0], "--init-file");
        assert_eq!(bash.args[1], dir.join("zaivern.bash").display().to_string());
        assert!(
            bash.args.contains(&"-i".to_string()),
            "対話でないと読まれない"
        );
        assert!(
            !bash.args.contains(&"-l".to_string()),
            "-l と --init-file は両立しない"
        );

        let zsh = launch_plan_for("/bin/zsh", &dir, "n").expect("zsh");
        let zdot = zsh
            .env
            .iter()
            .find(|(k, _)| k == "ZDOTDIR")
            .expect("ZDOTDIR");
        assert_eq!(zdot.1, dir.join("zdotdir").display().to_string());
        assert!(zsh.env.iter().any(|(k, _)| k == "ZAIVERN_ZDOTDIR_USER"));

        let fish = launch_plan_for("/usr/bin/fish", &dir, "n").expect("fish");
        assert_eq!(fish.args[1], "-C");
        assert!(fish.args[2].starts_with("source '"));

        let ps = launch_plan_for("pwsh", &dir, "n").expect("pwsh");
        assert_eq!(ps.args[0], "-NoExit");
        assert_eq!(ps.args[1], "-Command");
        assert!(ps.args[2].starts_with(". '"));

        assert!(launch_plan_for("cmd.exe", &dir, "n").is_none());
        // nonce はどのシェルでも渡す
        for p in [bash, zsh, fish, ps] {
            assert!(p
                .env
                .iter()
                .any(|(k, v)| k == "ZAIVERN_SHELL_NONCE" && v == "n"));
        }
    }

    #[test]
    fn 引用符を含むパスでも壊れない() {
        // ユーザー名に `'` が入る環境は実在する。エスケープを間違えると
        // その環境だけシェルが起動しない (最悪の壊れ方)。
        assert_eq!(fish_quote("/a'b/c"), "'/a\\'b/c'");
        assert_eq!(fish_quote("/a\\b"), "'/a\\\\b'");
        assert_eq!(pwsh_quote("/a'b/c"), "'/a''b/c'");
    }

    #[test]
    fn シムを書き出して再実行しても書き換えない() {
        let dir = crate::test_util::unique_temp_dir("shellint", "install");
        write_shims(&dir).expect("1 回目");
        let bash = dir.join("zaivern.bash");
        let before = std::fs::metadata(&bash).expect("メタ").modified().ok();
        write_shims(&dir).expect("2 回目");
        let after = std::fs::metadata(&bash).expect("メタ").modified().ok();
        assert_eq!(before, after, "内容が同じなら書かない");
        for f in [".zshenv", ".zprofile", ".zshrc", ".zlogin"] {
            assert!(dir.join("zdotdir").join(f).exists(), "{f} が無い");
        }
        assert!(dir.join("zaivern.fish").exists());
        assert!(dir.join("zaivern.ps1").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 二重発行の門番は環境変数とrcの両方を見る() {
        let dir = crate::test_util::unique_temp_dir("shellint", "guard");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let plain = dir.join("plain.sh");
        std::fs::write(&plain, "export PS1='$ '\n").expect("write");
        let osc = dir.join("osc.sh");
        std::fs::write(&osc, "printf '\\033]133;A\\007'\n").expect("write");

        let none = |_: &str| -> Option<String> { None };
        assert!(!already_integrated(&none, &[plain.clone()]));
        assert!(already_integrated(&none, &[osc.clone()]));
        let iterm = |k: &str| (k == "ITERM_SHELL_INTEGRATION_INSTALLED").then(|| "Yes".to_string());
        assert!(already_integrated(&iterm, &[plain.clone()]));
        // 空文字は「立っていない」扱い
        let empty = |k: &str| (k == "KITTY_SHELL_INTEGRATION").then(String::new);
        assert!(!already_integrated(&empty, &[plain]));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn シムは二重発行の門番を必ず持っている() {
        // シムを書き換えるときに門番を落とすと、iTerm2 利用者の履歴が
        // 全部 2 件になる。構造検査で落ちるようにしておく。
        for (name, body) in [
            ("bash", BASH_SHIM),
            ("zsh", ZSH_ZSHRC),
            ("fish", FISH_SHIM),
            ("pwsh", PWSH_SHIM),
        ] {
            let body = body.replace("\r\n", "\n");
            assert!(
                body.contains("external"),
                "{name} のシムに二重発行の門番が無い"
            );
        }
        // bash / zsh は 3.2 系でも動く記法だけを使う (macOS 同梱は 3.2)。
        assert!(
            !BASH_SHIM.contains("declare -A"),
            "連想配列は bash 4 以降。macOS 同梱の 3.2 で落ちる"
        );
    }

    #[test]
    fn 有効化しない限り起動計画は出ない() {
        // 既定は off。入れただけで起動経路が変わってはいけない。
        // フラグは直接触る — `set_enabled(true)` は実 `~/.zaivern` へシムを
        // 書くので、テストから呼んではいけない。
        let was = ENABLED.swap(false, Ordering::Relaxed);
        assert!(
            launch_plan("/bin/bash", false).is_none(),
            "無効なら注入しない"
        );
        ENABLED.store(true, Ordering::Relaxed);
        assert!(
            launch_plan("/bin/bash", true).is_none(),
            "コマンド指定ありはプロンプトが出ないので注入しない"
        );
        assert!(
            launch_plan("cmd.exe", false).is_none(),
            "未対応シェルはディスクに触れずに帰る"
        );
        ENABLED.store(was, Ordering::Relaxed);
    }

    #[test]
    fn エージェント印を渡す() {
        assert_eq!(agent_env(), [("ZAIVERN_AGENT", "1")]);
    }
}
