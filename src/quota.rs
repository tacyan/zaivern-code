//! プラン使用量 (クォータ) の横断集約と枯渇予測 — **通信ゼロ・完全オフライン**。
//!
//! ## 何を解決するか
//!
//! 競合製品は「アカウント 1 つ分の使用率とリセットまでの時間」をベンダーが
//! ローカルへ書いたファイルから読んで見せる。こちらは **N 本の並列エージェント
//! を横断した合算ビュー**と、**現在の燃焼速度から見た枯渇予測**を出す。
//! 壁にぶつかってから気付くのではなく、ぶつかる前に絞れるようにするのが目的。
//!
//! ## 設計の方針
//!
//! - **ネットワークを一切叩かない**。ベンダー CLI が既にローカルへ書いた
//!   ファイルを読むだけ。API 呼び出しが増えないので課金も増えない。
//! - **エージェント固有の知識は全てデータ** ([`AGENT_QUOTAS`] の記述子表)。
//!   ロジック側にベンダー名やパスの文字列を散らさない。新しい CLI は表に
//!   1 行足すだけで対応できる。
//! - **パスは全て `dirs::home_dir()` 起点**。環境・OS・ユーザー名を焼き込まない。
//!   ファイル探索は「注入されたルート」に対して動くので、テストは本物の
//!   ホームディレクトリを一切触らない。
//! - **パーサは内容 (`&str`) だけを受け取る純関数**。パスを渡しても
//!   ファイルは読まない (= UI スレッドから呼んでも I/O が起きない)。
//! - **推測を事実として出さない**。実測値 (ベンダーファイル) と推定値
//!   (燃焼速度からの外挿) は [`Confidence`] で必ず区別し、材料が足りなければ
//!   [`Projection::InsufficientData`] を返す。
//! - **勝手に止めない**。出すのは助言 ([`Advice`]) だけで、実際に絞るか
//!   止めるかは人が決める。
//!
//! ## ローカルに実際に何があるか (調査結果)
//!
//! | CLI | ローカルの使用量情報 | 種別 |
//! |---|---|---|
//! | codex | `~/.codex/sessions/**/rollout-*.jsonl` の `payload.rate_limits.primary.{used_percent, window_minutes, resets_at}` | 実測 |
//! | claude | プラン使用率を持つファイルは**無い** (`rate_limits.five_hour` は statusline へ stdin で渡されるだけで永続化されない) | 観測のみ |
//! | その他 | 同上 | 観測のみ |
//!
//! 実測が無いエージェントは「自前の観測」へ落とす。すなわち
//! [`crate::terminal::detect_rate_limit`] が拾った上限警告 (時刻付き) と、
//! 走っている本数。これは既存機能の再利用で、検知ロジックは複製しない。

// 公開 API 一式を先に用意し、パネル側の配線は後から行うため未使用警告を抑える
// (coordinator.rs / keybinds.rs と同じ扱い)。
#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::i18n::{tr, trf};

// ── 記述子表 (エージェント固有の知識はここだけ) ──────────────────────────

/// 使用量の在り処。
pub enum QuotaSource {
    /// ベンダーがローカルへ使用率を書いている。`locator` で探し、`parser` で読む。
    VendorFile {
        locator: FileLocator,
        /// **内容だけ**を受け取る純関数 (パスを渡してもファイルは読まない)。
        parser: fn(&str) -> Option<VendorUsage>,
    },
    /// ローカルに使用率が無い。自前の観測 (上限検知イベント) だけを使う。
    ObservedOnly,
}

/// ベンダーファイルの探し方 (ホーム起点。絶対パスは一切書かない)。
pub struct FileLocator {
    /// ホームからの相対ディレクトリ (例: `[".codex", "sessions"]`)。
    pub base: &'static [&'static str],
    /// ファイル名の前方一致 (空文字なら不問)。
    pub file_prefix: &'static str,
    /// 拡張子 (`.` は含めない。空文字なら不問)。
    pub file_ext: &'static str,
    /// 何階層まで潜るか (0 = base 直下のみ)。
    pub max_depth: usize,
    /// 走査するエントリ数の上限 (巨大なセッション置き場でも止まらないため)。
    pub max_entries: usize,
    /// 末尾から何バイトだけ読むか (JSONL 全体を読まない)。
    pub tail_bytes: u64,
}

/// エージェント 1 種類分の記述子。
pub struct AgentQuota {
    /// CLI の実行ファイル名 (agents.rs のカタログと同じキー)。
    pub bin: &'static str,
    /// 表示名。
    pub label: &'static str,
    /// プラン/アカウントの共有鍵。**同じ鍵のエージェントは同じ枠を食い合う**。
    pub account: &'static str,
    /// 使用量の在り処。
    pub source: QuotaSource,
    /// トークン消費の在り処 (プラン使用率とは別のファイルにあることが多い)。
    pub tokens: TokenSource,
}

/// 対応エージェントの表。行を足すだけで新しい CLI に対応できる。
pub static AGENT_QUOTAS: &[AgentQuota] = &[
    AgentQuota {
        bin: "claude",
        label: "Claude Code",
        account: "anthropic",
        // Claude Code はプラン使用率をローカルへ永続化しない (statusline へ
        // stdin で渡すだけ)。よって観測のみ。
        source: QuotaSource::ObservedOnly,
        // ただしトランスクリプトには 1 ターンごとの `usage` が残る。
        // 置き場は `~/.claude/projects/<cwd をエンコードした名前>/<sessionId>.jsonl`
        // なので base 直下 + 1 階層。
        tokens: TokenSource::Transcript {
            locator: FileLocator {
                base: &[".claude", "projects"],
                file_prefix: "",
                file_ext: "jsonl",
                max_depth: 2,
                max_entries: 8192,
                tail_bytes: 512 * 1024,
            },
            parser: parse_claude_transcript,
        },
    },
    AgentQuota {
        bin: "codex",
        label: "Codex CLI",
        account: "openai",
        source: QuotaSource::VendorFile {
            locator: FileLocator {
                base: &[".codex", "sessions"],
                file_prefix: "rollout-",
                file_ext: "jsonl",
                max_depth: 4,
                max_entries: 4096,
                tail_bytes: 256 * 1024,
            },
            parser: parse_codex_rollout,
        },
        // 使用率と同じロールアウトに `token_count` イベントが混ざっている。
        tokens: TokenSource::Transcript {
            locator: FileLocator {
                base: &[".codex", "sessions"],
                file_prefix: "rollout-",
                file_ext: "jsonl",
                max_depth: 4,
                max_entries: 4096,
                tail_bytes: 512 * 1024,
            },
            parser: parse_codex_tokens,
        },
    },
    AgentQuota {
        bin: "gemini",
        label: "Gemini CLI",
        account: "google",
        source: QuotaSource::ObservedOnly,
        // `~/.gemini/tmp/*/chats/*.jsonl` に本文は残るが、トークン数を書いた
        // ファイルは見つからなかった (`totalTokenCount` 等を全走査して 0 件)。
        tokens: TokenSource::None,
    },
    AgentQuota {
        bin: "cursor-agent",
        label: "Cursor Agent",
        account: "cursor",
        source: QuotaSource::ObservedOnly,
        // 未確認。
        tokens: TokenSource::None,
    },
];

/// 記述子を bin 名で引く。
pub fn descriptor(bin: &str) -> Option<&'static AgentQuota> {
    AGENT_QUOTAS.iter().find(|a| a.bin == bin)
}

// ── 値の型 ─────────────────────────────────────────────────────────────

/// ベンダーファイルから読めた 1 件分の使用量。
#[derive(Debug, Clone, PartialEq)]
pub struct VendorUsage {
    /// 0.0..=1.0 に丸めた使用率。
    pub used_fraction: f32,
    /// 枠の窓幅 (5 時間枠なら 5h)。
    pub window: Option<Duration>,
    /// 枠がリセットされる時刻。
    pub resets_at: Option<SystemTime>,
    /// プラン名 (ベンダーが書いていれば)。
    pub plan: Option<String>,
    /// その行が書かれた時刻。
    pub observed_at: Option<SystemTime>,
}

/// 自前で観測した「上限に当たった」イベント。
#[derive(Debug, Clone, PartialEq)]
pub struct RateLimitEvent {
    /// どのエージェントか (bin 名)。
    pub agent: String,
    /// 検知した時刻。
    pub at: SystemTime,
    /// 検知した行 (terminal::detect_rate_limit の戻り)。
    pub line: String,
}

/// 情報の出どころ。UI はこれで「実測」と「観測どまり」を必ず描き分ける。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    /// ベンダーがローカルへ書いた使用率を読んだ (実測)。
    Vendor,
    /// 自前の観測 (上限検知イベント) だけがある。
    Observed,
    /// 何も無い。
    Unavailable,
}

/// 確度。**推定を実測として出さない**ための札。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Confidence {
    /// ベンダーの数字 / 十分な標本数による測定。
    Measured,
    /// 外挿・少ない標本による推定。
    Estimated,
    /// 判定材料が無い。
    Unknown,
}

/// エージェント 1 本分のスナップショット。
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaSnapshot {
    /// bin 名。
    pub agent: String,
    /// 表示名。
    pub label: String,
    /// プラン共有鍵。
    pub account: String,
    /// 0.0..=1.0。ベンダーファイルが無ければ None。
    pub used_fraction: Option<f32>,
    /// 枠のリセット時刻。
    pub resets_at: Option<SystemTime>,
    /// 枠の窓幅。
    pub window: Option<Duration>,
    /// プラン名。
    pub plan: Option<String>,
    /// 自前で観測した上限イベント (新しい順ではなく時刻昇順)。
    pub observed_events: Vec<RateLimitEvent>,
    /// 出どころ。
    pub source: SourceKind,
    /// 使用率が測られた時刻 (ベンダー行の時刻)。
    pub measured_at: Option<SystemTime>,
}

impl QuotaSnapshot {
    fn empty(d: &AgentQuota) -> Self {
        Self {
            agent: d.bin.to_string(),
            label: d.label.to_string(),
            account: d.account.to_string(),
            used_fraction: None,
            resets_at: None,
            window: None,
            plan: None,
            observed_events: Vec::new(),
            source: SourceKind::Unavailable,
            measured_at: None,
        }
    }

    /// 残り枠 (0.0..=1.0)。使用率が無ければ None。
    pub fn remaining_fraction(&self) -> Option<f32> {
        self.used_fraction.map(|u| (1.0 - u).clamp(0.0, 1.0))
    }
}

// ── 取得 (ここだけがファイルを触る) ────────────────────────────────────

/// 全エージェントのスナップショット。**背景スレッドから呼ぶこと**。
pub fn snapshot_all() -> Vec<QuotaSnapshot> {
    match dirs::home_dir() {
        Some(h) => snapshot_all_in(&h),
        None => AGENT_QUOTAS.iter().map(QuotaSnapshot::empty).collect(),
    }
}

/// ホーム相当のルートを注入する版 (テストはこれを使い、本物のホームを触らない)。
pub fn snapshot_all_in(home: &Path) -> Vec<QuotaSnapshot> {
    AGENT_QUOTAS
        .iter()
        .map(|d| {
            let mut snap = QuotaSnapshot::empty(d);
            if let QuotaSource::VendorFile { locator, parser } = &d.source {
                if let Some(path) = newest_file(home, locator) {
                    if let Some(content) = read_tail(&path, locator.tail_bytes) {
                        if let Some(u) = parser(&content) {
                            snap.used_fraction = Some(u.used_fraction);
                            snap.resets_at = u.resets_at;
                            snap.window = u.window;
                            snap.plan = u.plan;
                            snap.measured_at = u.observed_at;
                            snap.source = SourceKind::Vendor;
                        }
                    }
                }
            }
            snap
        })
        .collect()
}

/// 観測イベントをスナップショットへ合流させる (純関数)。
///
/// 実測が無いエージェントは、イベントが 1 つでもあれば
/// [`SourceKind::Observed`] へ格上げする。
pub fn merge_observed(snaps: &mut [QuotaSnapshot], events: &[RateLimitEvent]) {
    for s in snaps.iter_mut() {
        s.observed_events = events
            .iter()
            .filter(|e| e.agent == s.agent)
            .cloned()
            .collect();
        s.observed_events.sort_by_key(|e| e.at);
        if s.source == SourceKind::Unavailable && !s.observed_events.is_empty() {
            s.source = SourceKind::Observed;
        }
    }
}

/// `base` 配下で条件に合う**最も新しい**ファイルを 1 つ返す。
///
/// ルートは注入され、走査は `max_depth` / `max_entries` で有界。
pub fn newest_file(root: &Path, loc: &FileLocator) -> Option<PathBuf> {
    let mut dir = root.to_path_buf();
    for seg in loc.base {
        dir.push(seg);
    }
    if !dir.is_dir() {
        return None;
    }
    let mut best: Option<(SystemTime, PathBuf)> = None;
    let mut seen = 0usize;
    let mut stack = vec![(dir, 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            seen += 1;
            if seen > loc.max_entries {
                return best.map(|(_, p)| p);
            }
            let path = ent.path();
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if depth < loc.max_depth {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !matches_name(&path, loc) {
                continue;
            }
            let mtime = ent
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                best = Some((mtime, path));
            }
        }
    }
    best.map(|(_, p)| p)
}

fn matches_name(path: &Path, loc: &FileLocator) -> bool {
    let name = match path.file_name().and_then(|n| n.to_str()) {
        Some(n) => n,
        None => return false,
    };
    if !loc.file_prefix.is_empty() && !name.starts_with(loc.file_prefix) {
        return false;
    }
    if !loc.file_ext.is_empty() {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != loc.file_ext {
            return false;
        }
    }
    true
}

/// ファイル末尾を最大 `max_bytes` 読む。途中で切れた先頭行は捨てる。
pub fn read_tail(path: &Path, max_bytes: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let truncated = len > max_bytes;
    if truncated {
        f.seek(SeekFrom::Start(len - max_bytes)).ok()?;
    }
    let mut buf = Vec::with_capacity(max_bytes.min(len) as usize + 1);
    f.take(max_bytes).read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    if truncated {
        // 先頭は行の途中かもしれないので落とす (壊れた JSON を食わせない)。
        match text.find('\n') {
            Some(i) => Some(text[i + 1..].to_string()),
            None => Some(String::new()),
        }
    } else {
        Some(text)
    }
}

// ── パーサ (純関数。内容だけを受け取る) ────────────────────────────────

/// codex のロールアウト JSONL から**最後の**使用率行を読む。
///
/// 想定する形 (キーが欠けても壊れない):
/// `{"timestamp":"…","payload":{"rate_limits":{"primary":{"used_percent":…,
///  "window_minutes":…,"resets_at":…},"plan_type":"…"}}}`
/// `resets_at` (エポック秒) が無ければ `resets_in_seconds` + 行の時刻で補う。
pub fn parse_codex_rollout(content: &str) -> Option<VendorUsage> {
    /// 末尾から見る行数の上限 (壊れた巨大ファイルでも止まらないため)。
    const MAX_LINES: usize = 8192;
    content
        .lines()
        .rev()
        .take(MAX_LINES)
        .filter(|l| l.contains("rate_limits"))
        .find_map(|l| codex_line_usage(l.trim()))
}

/// 1 行分の解析。壊れていれば None (呼び出し側は次の行へ進む)。
fn codex_line_usage(line: &str) -> Option<VendorUsage> {
    // 途中で切れた行・非 JSON はここで None になる
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let rl = v
        .get("payload")
        .and_then(|p| p.get("rate_limits"))
        .or_else(|| v.get("rate_limits"))?;
    let observed_at = v
        .get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_rfc3339);
    // primary が空なら secondary へ落ちる (ベンダー側の版差を吸収)
    let bucket = ["primary", "secondary"]
        .iter()
        .filter_map(|k| rl.get(*k))
        .find(|b| {
            b.get("used_percent")
                .and_then(|p| p.as_f64())
                .map(|p| p.is_finite())
                .unwrap_or(false)
        })?;
    let percent = bucket.get("used_percent").and_then(|p| p.as_f64())?;
    let window = bucket
        .get("window_minutes")
        .and_then(|w| w.as_u64())
        .filter(|m| *m > 0)
        .map(|m| Duration::from_secs(m.saturating_mul(60)));
    let resets_at = bucket
        .get("resets_at")
        .and_then(|r| r.as_i64())
        .filter(|s| *s > 0)
        .map(|s| UNIX_EPOCH + Duration::from_secs(s as u64))
        .or_else(|| {
            let rel = bucket.get("resets_in_seconds").and_then(|r| r.as_i64())?;
            if rel < 0 {
                return None;
            }
            Some(observed_at? + Duration::from_secs(rel as u64))
        });
    Some(VendorUsage {
        used_fraction: ((percent as f32) / 100.0).clamp(0.0, 1.0),
        window,
        resets_at,
        plan: rl
            .get("plan_type")
            .and_then(|p| p.as_str())
            .map(|s| s.to_string()),
        observed_at,
    })
}

/// RFC3339 (`2026-07-25T03:36:12.345Z` / `…+09:00`) を [`SystemTime`] へ。
/// 外部クレートを足さずに済ませるための最小実装。壊れていれば None。
pub fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    if b[10] != b'T' && b[10] != b't' && b[10] != b' ' {
        return None;
    }
    let num = |a: usize, z: usize| -> Option<i64> { s.get(a..z)?.parse::<i64>().ok() };
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let mut epoch = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + sec.min(59);
    // 小数秒を飛ばしてタイムゾーン指定を読む
    let rest = &s[19..];
    let tz = rest.trim_start_matches(|c: char| c == '.' || c.is_ascii_digit());
    if !(tz.is_empty() || tz == "Z" || tz == "z") {
        let sign: i64 = match tz.as_bytes()[0] {
            b'+' => -1,
            b'-' => 1,
            _ => return None,
        };
        let hh: i64 = tz.get(1..3)?.parse().ok()?;
        let mm: i64 = tz.get(4..6).unwrap_or("00").parse().unwrap_or(0);
        epoch += sign * (hh * 3600 + mm * 60);
    }
    if epoch < 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(epoch as u64))
}

/// 民間暦 → エポックからの日数 (Howard Hinnant の days_from_civil)。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// 端末出力から上限警告を拾う。**検知ロジックは既存の
/// [`crate::terminal::detect_rate_limit`] を再利用**し、複製しない。
pub fn observe_output(agent: &str, text: &str, at: SystemTime) -> Option<RateLimitEvent> {
    crate::terminal::detect_rate_limit(text).map(|line| RateLimitEvent {
        agent: agent.to_string(),
        at,
        line,
    })
}

// ── 集約 (同じプランを共有するエージェントをまとめる) ──────────────────

/// アカウント (= プラン枠) 1 つ分の合算ビュー。
#[derive(Debug, Clone, PartialEq)]
pub struct AccountUsage {
    /// プラン共有鍵。
    pub account: String,
    /// この枠を食っているエージェント (bin 名, 昇順)。
    pub agents: Vec<String>,
    /// 使用率。**合計ではなく最大**を採る (各エージェントが報告するのは
    /// 「そのアカウント全体の使用率」であり、足すと二重計上になる)。
    pub used_fraction: Option<f32>,
    /// 使用率の確度。
    pub confidence: Confidence,
    /// 最も早いリセット時刻。
    pub resets_at: Option<SystemTime>,
    /// この枠で走っている本数の合計 (燃焼速度の倍率)。
    pub running_agents: usize,
    /// 観測した上限イベント (時刻昇順)。
    pub events: Vec<RateLimitEvent>,
    /// 枯渇予測 ([`attach_projection`] で埋まる。初期値は InsufficientData)。
    pub projection: Projection,
}

impl AccountUsage {
    /// 残り枠。
    pub fn remaining_fraction(&self) -> Option<f32> {
        self.used_fraction.map(|u| (1.0 - u).clamp(0.0, 1.0))
    }

    /// 指定した窓の中に入った上限イベントの数。
    pub fn recent_events(&self, window: Duration, now: SystemTime) -> usize {
        self.events
            .iter()
            .filter(|e| {
                now.duration_since(e.at)
                    .map(|d| d <= window)
                    .unwrap_or(true)
            })
            .count()
    }
}

/// スナップショットをアカウント単位へ畳む (純関数)。
///
/// `running_by_agent` は「bin 名 → いま走っている本数」。
pub fn aggregate(
    snaps: &[QuotaSnapshot],
    running_by_agent: &[(String, usize)],
) -> Vec<AccountUsage> {
    let mut order: Vec<String> = Vec::new();
    let mut by: HashMap<String, AccountUsage> = HashMap::new();
    for s in snaps {
        let e = by.entry(s.account.clone()).or_insert_with(|| {
            order.push(s.account.clone());
            AccountUsage {
                account: s.account.clone(),
                agents: Vec::new(),
                used_fraction: None,
                confidence: Confidence::Unknown,
                resets_at: None,
                running_agents: 0,
                events: Vec::new(),
                projection: Projection::InsufficientData,
            }
        });
        if !e.agents.contains(&s.agent) {
            e.agents.push(s.agent.clone());
        }
        if let Some(u) = s.used_fraction {
            e.used_fraction = Some(e.used_fraction.map_or(u, |cur| cur.max(u)));
            if s.source == SourceKind::Vendor {
                e.confidence = Confidence::Measured;
            } else if e.confidence == Confidence::Unknown {
                e.confidence = Confidence::Estimated;
            }
        }
        if let Some(r) = s.resets_at {
            e.resets_at = Some(e.resets_at.map_or(r, |cur| cur.min(r)));
        }
        e.running_agents += running_by_agent
            .iter()
            .find(|(a, _)| *a == s.agent)
            .map(|(_, n)| *n)
            .unwrap_or(0);
        e.events.extend(s.observed_events.iter().cloned());
    }
    let mut out: Vec<AccountUsage> = order
        .into_iter()
        .filter_map(|k| by.remove(&k))
        .map(|mut u| {
            u.agents.sort();
            u.events.sort_by_key(|e| e.at);
            u
        })
        .collect();
    out.sort_by(|a, b| a.account.cmp(&b.account));
    out
}

// ── 燃焼速度と枯渇予測 ─────────────────────────────────────────────────

/// 使用率の観測点 1 つ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurnSample {
    pub at: SystemTime,
    /// 0.0..=1.0。
    pub used_fraction: f32,
}

/// 燃焼速度 (枠/秒)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BurnRate {
    /// 1 秒あたりに減る枠 (0.0 = 燃えていない)。
    pub per_sec: f32,
    /// 使った標本数。
    pub samples: usize,
    /// 標本が張る時間幅。
    pub span: Duration,
    /// 確度。
    pub confidence: Confidence,
}

/// [`Confidence::Measured`] を名乗るのに要る最小標本数。
pub const MEASURED_MIN_SAMPLES: usize = 3;
/// [`Confidence::Measured`] を名乗るのに要る最小時間幅。
pub const MEASURED_MIN_SPAN: Duration = Duration::from_secs(120);

/// 直近 `window` の標本から燃焼速度を出す (純関数)。
///
/// - 標本 0/1 個、時間幅 0 → None (**材料不足**をはっきり返す)
/// - 途中で使用率が下がっていたら「枠がリセットされた」と見なし、
///   **下がった後の標本だけ**を使う (リセットを跨いだ平均は嘘になる)
pub fn burn_rate(samples: &[BurnSample], window: Duration, now: SystemTime) -> Option<BurnRate> {
    let mut win: Vec<&BurnSample> = samples
        .iter()
        .filter(|s| match now.duration_since(s.at) {
            Ok(age) => age <= window,
            Err(_) => true, // 未来の時刻 (時計のズレ) は直近扱い
        })
        .collect();
    win.sort_by_key(|s| s.at);
    if win.len() < 2 {
        return None;
    }
    // 直近のリセット位置を探す (使用率が明確に下がった点)
    let mut start = 0usize;
    for i in 1..win.len() {
        if win[i].used_fraction + 1e-4 < win[i - 1].used_fraction {
            start = i;
        }
    }
    let win = &win[start..];
    if win.len() < 2 {
        return None;
    }
    let first = win[0];
    let last = win[win.len() - 1];
    let span = last.at.duration_since(first.at).ok()?;
    if span.as_secs_f32() <= 0.0 {
        return None;
    }
    let delta = (last.used_fraction - first.used_fraction).max(0.0);
    let per_sec = delta / span.as_secs_f32();
    let confidence = if win.len() >= MEASURED_MIN_SAMPLES && span >= MEASURED_MIN_SPAN {
        Confidence::Measured
    } else {
        Confidence::Estimated
    };
    Some(BurnRate {
        per_sec,
        samples: win.len(),
        span,
        confidence,
    })
}

/// 枯渇予測の結果。**「分からない」を握り潰さない**ための型。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Projection {
    /// この時間で枠を使い切る見込み (推定)。
    Exhaustion(Duration),
    /// 使い切る前にリセットが来る (この時間で枠が戻る)。
    ResetFirst(Duration),
    /// 燃えていない (速度がほぼ 0)。
    NotBurning,
    /// 材料不足で判定できない。
    InsufficientData,
}

impl Projection {
    /// 「この時間で尽きる」場合だけ Some。
    pub fn exhaustion(self) -> Option<Duration> {
        match self {
            Projection::Exhaustion(d) => Some(d),
            _ => None,
        }
    }
}

/// 予測できる上限 (これを超える見積りは実用上「燃えていない」と同じ)。
pub const PROJECTION_MAX: Duration = Duration::from_secs(30 * 24 * 3600);

/// 残り枠と燃焼速度から枯渇までの時間を出す (純関数)。
///
/// `burn` か `remaining` が無ければ [`Projection::InsufficientData`]。
/// リセットの方が先に来るなら [`Projection::ResetFirst`]。
pub fn projected_exhaustion(
    now: SystemTime,
    burn: Option<&BurnRate>,
    remaining: Option<f32>,
    resets_at: Option<SystemTime>,
) -> Projection {
    let (burn, remaining) = match (burn, remaining) {
        (Some(b), Some(r)) => (b, r),
        _ => return Projection::InsufficientData,
    };
    if remaining <= 0.0 {
        return Projection::Exhaustion(Duration::ZERO);
    }
    let secs = match exhaustion_after(burn.per_sec, remaining) {
        Some(d) => d,
        None => return Projection::NotBurning,
    };
    if let Some(reset) = resets_at {
        if let Ok(until) = reset.duration_since(now) {
            if until < secs {
                return Projection::ResetFirst(until);
            }
        }
    }
    Projection::Exhaustion(secs)
}

/// 「速度 × 残り」だけの素の計算 (時計に触らない)。
/// 速度 0・非有限・上限超えは None。
pub fn exhaustion_after(burn_per_sec: f32, remaining: f32) -> Option<Duration> {
    if !burn_per_sec.is_finite() || !remaining.is_finite() || burn_per_sec <= 1e-9 {
        return None;
    }
    let secs = (remaining.max(0.0) / burn_per_sec) as f64;
    if !secs.is_finite() || secs > PROJECTION_MAX.as_secs_f64() {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

/// アカウントへ予測を貼る。
pub fn attach_projection(u: &mut AccountUsage, burn: Option<&BurnRate>, now: SystemTime) {
    u.projection = projected_exhaustion(now, burn, u.remaining_fraction(), u.resets_at);
}

/// アカウント別の使用率履歴。燃焼速度はここから出す。
#[derive(Debug, Default)]
pub struct BurnHistory {
    per_account: HashMap<String, Vec<BurnSample>>,
}

impl BurnHistory {
    /// 1 アカウントあたりの保持数。
    pub const CAP: usize = 256;
    /// 既定の集計窓。
    pub const WINDOW: Duration = Duration::from_secs(3600);

    pub fn new() -> Self {
        Self::default()
    }

    /// 観測点を 1 つ足す (同時刻の重複は上書き)。
    pub fn record(&mut self, account: &str, at: SystemTime, used_fraction: f32) {
        let v = self.per_account.entry(account.to_string()).or_default();
        if let Some(last) = v.last_mut() {
            if last.at == at {
                last.used_fraction = used_fraction;
                return;
            }
        }
        v.push(BurnSample {
            at,
            used_fraction: used_fraction.clamp(0.0, 1.0),
        });
        if v.len() > Self::CAP {
            let cut = v.len() - Self::CAP;
            v.drain(..cut);
        }
    }

    pub fn samples(&self, account: &str) -> &[BurnSample] {
        self.per_account
            .get(account)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// 燃焼速度 (窓は [`BurnHistory::WINDOW`])。
    pub fn rate(&self, account: &str, now: SystemTime) -> Option<BurnRate> {
        burn_rate(self.samples(account), Self::WINDOW, now)
    }
}

// ── 助言 (止めるのは人。ここは言うだけ) ────────────────────────────────

/// 助言のしきい値。設定から差し替えられるようフィールドは全て公開。
#[derive(Debug, Clone, PartialEq)]
pub struct Policy {
    /// この時間内に尽きる見込みなら「絞れ」。
    pub horizon: Duration,
    /// この時間内に尽きる見込みなら「止めろ」。
    pub stop_horizon: Duration,
    /// この使用率で「絞れ」。
    pub slow_fraction: f32,
    /// この使用率で「止めろ」。
    pub stop_fraction: f32,
    /// 上限イベントを「今まさに当たっている」と見なす窓。
    pub event_window: Duration,
    /// 並列本数がこれ以上なら混雑扱い。
    pub crowd_agents: usize,
    /// 混雑時に「絞れ」を出し始める使用率。
    pub crowd_fraction: f32,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            horizon: Duration::from_secs(30 * 60),
            stop_horizon: Duration::from_secs(5 * 60),
            slow_fraction: 0.80,
            stop_fraction: 0.95,
            event_window: Duration::from_secs(10 * 60),
            crowd_agents: 3,
            crowd_fraction: 0.50,
        }
    }
}

/// 助言の理由。UI は [`AdviceReason::label`] を出せばよい。
#[derive(Debug, Clone, PartialEq)]
pub enum AdviceReason {
    /// 既に上限に当たっている (観測イベントが直近にある)。
    AlreadyLimited { agent: String },
    /// 使用率がしきい値を超えた。
    HighUsage { percent: u32 },
    /// 予測枯渇が近い。
    ExhaustsSoon { in_secs: u64, running: usize },
    /// 直近に上限イベントが複数。
    RepeatedLimits { count: usize },
    /// 並列本数が多く、使用率も上がってきた。
    Crowded { running: usize, percent: u32 },
}

impl AdviceReason {
    /// 日本語の一言 (辞書があれば翻訳される)。
    pub fn label(&self) -> String {
        match self {
            AdviceReason::AlreadyLimited { agent } => trf(
                "{agent} が使用上限に当たっています",
                &[("agent", agent.clone())],
            ),
            AdviceReason::HighUsage { percent } => trf(
                "プラン枠を {percent}% 使っています",
                &[("percent", percent.to_string())],
            ),
            AdviceReason::ExhaustsSoon { in_secs, running } => trf(
                "この調子だと約 {mins} 分で枠が尽きます ({running} 本並列)",
                &[
                    ("mins", ((*in_secs + 59) / 60).to_string()),
                    ("running", running.to_string()),
                ],
            ),
            AdviceReason::RepeatedLimits { count } => trf(
                "直近に上限警告が {count} 回出ています",
                &[("count", count.to_string())],
            ),
            AdviceReason::Crowded { running, percent } => trf(
                "{running} 本を並列で走らせたまま枠を {percent}% 使っています",
                &[
                    ("running", running.to_string()),
                    ("percent", percent.to_string()),
                ],
            ),
        }
    }
}

/// UI へ渡す助言。**自動で殺さない**。判断するのは人。
#[derive(Debug, Clone, PartialEq)]
pub enum Advice {
    /// 余裕あり。
    Ok,
    /// 並列本数を減らす等、絞った方がよい。
    SlowDown { reason: AdviceReason },
    /// これ以上流すと壁に当たる。
    Stop { reason: AdviceReason },
}

impl Advice {
    /// 深刻さ (0 = Ok, 1 = SlowDown, 2 = Stop)。UI の色分け・並べ替え用。
    pub fn severity(&self) -> u8 {
        match self {
            Advice::Ok => 0,
            Advice::SlowDown { .. } => 1,
            Advice::Stop { .. } => 2,
        }
    }

    /// 表示用の本文 (Ok は空文字)。
    pub fn message(&self) -> String {
        match self {
            Advice::Ok => String::new(),
            Advice::SlowDown { reason } | Advice::Stop { reason } => reason.label(),
        }
    }
}

/// 助言を決める (純関数)。
///
/// 判定は上から順に見て**最初に当たったもの**を返す。
pub fn advise(
    usage: &AccountUsage,
    running_agents: usize,
    policy: &Policy,
    now: SystemTime,
) -> Advice {
    // 1) 既に当たっている — 予測より観測が強い
    let recent = usage.recent_events(policy.event_window, now);
    if recent > 0 {
        let agent = usage
            .events
            .last()
            .map(|e| e.agent.clone())
            .unwrap_or_else(|| usage.account.clone());
        return Advice::Stop {
            reason: AdviceReason::AlreadyLimited { agent },
        };
    }
    let used = usage.used_fraction;
    // 2) 使用率が停止しきい値超え (実測)
    if let Some(u) = used {
        if u >= policy.stop_fraction {
            return Advice::Stop {
                reason: AdviceReason::HighUsage { percent: pct(u) },
            };
        }
    }
    // 3) 予測枯渇が目前
    if let Projection::Exhaustion(d) = usage.projection {
        if d <= policy.stop_horizon {
            return Advice::Stop {
                reason: AdviceReason::ExhaustsSoon {
                    in_secs: d.as_secs(),
                    running: running_agents,
                },
            };
        }
        if d <= policy.horizon {
            return Advice::SlowDown {
                reason: AdviceReason::ExhaustsSoon {
                    in_secs: d.as_secs(),
                    running: running_agents,
                },
            };
        }
    }
    // 4) 使用率が警戒しきい値超え
    if let Some(u) = used {
        if u >= policy.slow_fraction {
            return Advice::SlowDown {
                reason: AdviceReason::HighUsage { percent: pct(u) },
            };
        }
    }
    // 5) 少し前の上限警告が積み重なっている
    let older = usage.recent_events(policy.event_window.saturating_mul(4), now);
    if older >= 2 {
        return Advice::SlowDown {
            reason: AdviceReason::RepeatedLimits { count: older },
        };
    }
    // 6) 並列が多く、枠も半分以上使っている
    if running_agents >= policy.crowd_agents {
        if let Some(u) = used {
            if u >= policy.crowd_fraction {
                return Advice::SlowDown {
                    reason: AdviceReason::Crowded {
                        running: running_agents,
                        percent: pct(u),
                    },
                };
            }
        }
    }
    Advice::Ok
}

/// 0.0..=1.0 → 0..=100 (表示用。四捨五入)。
pub fn pct(f: f32) -> u32 {
    (f.clamp(0.0, 1.0) * 100.0).round() as u32
}

// ── トークン消費とコスト推定 ──────────────────────────────────────────
//
// ## ローカルに実際に何があるか (実測。2026-08 時点)
//
// | CLI | ファイル | 取れるもの |
// |---|---|---|
// | claude | `~/.claude/projects/<cwd をエンコードした名前>/<sessionId>.jsonl` | `type:"assistant"` の行の `message.usage` (`input_tokens` / `output_tokens` / `cache_creation_input_tokens` / `cache_read_input_tokens`) と `message.model` |
// | codex | `~/.codex/sessions/YYYY/MM/DD/rollout-*.jsonl` | `payload.type=="token_count"` の `info.last_token_usage` (`input_tokens` / `cached_input_tokens` / `output_tokens`) |
// | gemini | **無し**。`~/.gemini/tmp/*/chats/*.jsonl` に本文はあるが、トークン数を書いたファイルは見つからなかった | — |
// | その他 | 未確認 | — |
//
// **本文は一切読み出さない。** 拾うのは数値・時刻・モデル名だけで、
// プロンプト本文やツール出力は構造体にも画面にも一切載せない。

/// 1 行として受け付ける最大バイト数。超える行は飛ばす。
/// 壊れた/巨大な 1 行に JSON パーサを噛ませて固まらせないための上限。
pub const TOKEN_MAX_LINE: usize = 2 * 1024 * 1024;

/// 集計で見るファイル数の上限 (新しい順)。
///
/// 1 ファイルあたり [`FileLocator::tail_bytes`] しか読まないので、
/// 1 回の集計で読むのは最大 `TOKEN_MAX_FILES × tail_bytes`。
/// **この積が背景 I/O の上限**なので、増やすときは [`TOKEN_TTL`] と一緒に見る。
pub const TOKEN_MAX_FILES: usize = 64;

/// 集計の既定の窓。「いま並列度をどうするか」を決めるための直近ぶん。
pub const TOKEN_WINDOW: Duration = Duration::from_secs(24 * 3600);

/// トークン集計を読み直す最短間隔。
///
/// 使用率 ([`crate::coordinator::QUOTA_TTL`]) より**ずっと長い**。
/// 使用率は 1 ファイルの末尾数百 KB で済むが、トークン集計は
/// 何十本ものトランスクリプトを舐めるので、同じ間隔で回すと
/// アイドル時に無視できない背景 I/O になる (設計原則 3)。
pub const TOKEN_TTL: Duration = Duration::from_secs(120);

/// 1 回のやり取り (= 1 API リクエスト) 分のトークン。
///
/// 加算は全て飽和 (`saturating_*`)。桁あふれで負や 0 に化けさせない。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TokenUsage {
    /// キャッシュに当たらなかった入力。
    pub input: u64,
    /// 出力 (推論トークンを含む)。
    pub output: u64,
    /// キャッシュ**書き込み** (ベンダーが区別しないなら 0)。
    pub cache_write: u64,
    /// キャッシュ**読み出し**。
    pub cache_read: u64,
}

impl TokenUsage {
    /// 全部の合計。
    pub fn total(&self) -> u64 {
        self.input
            .saturating_add(self.output)
            .saturating_add(self.cache_write)
            .saturating_add(self.cache_read)
    }

    /// 1 つも消費していないか。
    pub fn is_zero(&self) -> bool {
        self.total() == 0
    }

    /// 足し込む (飽和)。
    pub fn add(&mut self, o: &TokenUsage) {
        self.input = self.input.saturating_add(o.input);
        self.output = self.output.saturating_add(o.output);
        self.cache_write = self.cache_write.saturating_add(o.cache_write);
        self.cache_read = self.cache_read.saturating_add(o.cache_read);
    }
}

/// トランスクリプト 1 行分。**本文は持たない**。
#[derive(Debug, Clone, PartialEq)]
pub struct TurnUsage {
    /// その行が書かれた時刻 (取れなければ None)。
    pub at: Option<SystemTime>,
    /// モデル名 (取れなければ None)。単価の引き当てに使う。
    pub model: Option<String>,
    /// 消費。
    pub usage: TokenUsage,
}

/// トークン消費の在り処。
pub enum TokenSource {
    /// ローカルのトランスクリプトを読む。パーサは**内容だけ**を受け取る純関数。
    Transcript {
        locator: FileLocator,
        parser: fn(&str) -> Vec<TurnUsage>,
    },
    /// ローカルに残らない (調査して確認できなかったものも含む)。
    None,
}

// ── パーサ (純関数。内容だけを受け取る) ────────────────────────────────

/// Claude Code のトランスクリプト JSONL から `message.usage` を拾う。
///
/// 1 行 1 レコード。`type:"assistant"` の行だけが `message.usage` を持つ。
/// 途中で切れた行・非 JSON・`usage` の無い行・空行は**黙って飛ばす**
/// (壊れた 1 行のためにファイル全体を捨てない)。
pub fn parse_claude_transcript(content: &str) -> Vec<TurnUsage> {
    content.lines().filter_map(claude_line_usage).collect()
}

fn claude_line_usage(line: &str) -> Option<TurnUsage> {
    let line = line.trim();
    // 安いふるいを先に通す。全行を JSON パースすると巨大な履歴で重すぎる。
    if line.len() > TOKEN_MAX_LINE || !line.contains("\"usage\"") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let msg = v.get("message")?;
    let u = msg.get("usage")?;
    let n = |k: &str| u.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let usage = TokenUsage {
        input: n("input_tokens"),
        output: n("output_tokens"),
        cache_write: n("cache_creation_input_tokens"),
        cache_read: n("cache_read_input_tokens"),
    };
    if usage.is_zero() {
        return None;
    }
    Some(TurnUsage {
        at: v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_rfc3339),
        model: msg
            .get("model")
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        usage,
    })
}

/// codex のロールアウト JSONL から `token_count` イベントを拾う。
///
/// **`total_token_usage` ではなく `last_token_usage` を足し上げる。**
/// 前者はセッション内の累計なので、窓で切ると二重計上になる
/// (実測: total が 19594 → 44626 と伸びるのに対し last は 19594 / 25032 で、
/// last の総和が total に一致する)。
pub fn parse_codex_tokens(content: &str) -> Vec<TurnUsage> {
    content.lines().filter_map(codex_line_tokens).collect()
}

fn codex_line_tokens(line: &str) -> Option<TurnUsage> {
    let line = line.trim();
    if line.len() > TOKEN_MAX_LINE || !line.contains("token_count") {
        return None;
    }
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let info = v.get("payload").and_then(|p| p.get("info"))?;
    let lu = info
        .get("last_token_usage")
        .or_else(|| info.get("total_token_usage"))?;
    let n = |k: &str| lu.get(k).and_then(|x| x.as_u64()).unwrap_or(0);
    let cached = n("cached_input_tokens");
    // codex (OpenAI 系) の input_tokens はキャッシュ読み出しを**含む**合計。
    // Claude 側と揃えるためにここで引いておく。
    let usage = TokenUsage {
        input: n("input_tokens").saturating_sub(cached),
        output: n("output_tokens"),
        // 書き込み側の課金区分をベンダーが出さないので 0 のまま。
        cache_write: 0,
        cache_read: cached,
    };
    if usage.is_zero() {
        return None;
    }
    Some(TurnUsage {
        at: v
            .get("timestamp")
            .and_then(|t| t.as_str())
            .and_then(parse_rfc3339),
        model: info
            .get("model")
            .and_then(|m| m.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string()),
        usage,
    })
}

// ── 集計 ───────────────────────────────────────────────────────────────

/// エージェント 1 種類分のトークン集計。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentTokens {
    /// bin 名。
    pub agent: String,
    /// 表示名。
    pub label: String,
    /// プラン共有鍵。
    pub account: String,
    /// 合計。
    pub total: TokenUsage,
    /// モデル別内訳 (合計の降順)。モデル名が取れなかった分のキーは空文字。
    pub by_model: Vec<(String, TokenUsage)>,
    /// 数えたやり取りの本数。
    pub turns: usize,
    /// 上限に当たって読み切れなかった (= 実際はこれ以上)。
    pub truncated: bool,
}

/// `base` 配下で条件に合うファイルを**新しい順**に最大 `max_files` 件返す。
///
/// `newer_than` より古いものは落とす。走査はルート注入 + `max_depth` /
/// `max_entries` で有界 (巨大なセッション置き場でも止まらない)。
/// 第 2 戻り値は「上限に当たって取りこぼした」印。
pub fn recent_files(
    root: &Path,
    loc: &FileLocator,
    newer_than: SystemTime,
    max_files: usize,
) -> (Vec<PathBuf>, bool) {
    let mut dir = root.to_path_buf();
    for seg in loc.base {
        dir.push(seg);
    }
    if !dir.is_dir() {
        return (Vec::new(), false);
    }
    let mut found: Vec<(SystemTime, PathBuf)> = Vec::new();
    let mut seen = 0usize;
    let mut budget_hit = false;
    let mut stack = vec![(dir, 0usize)];
    while let Some((d, depth)) = stack.pop() {
        let rd = match std::fs::read_dir(&d) {
            Ok(rd) => rd,
            Err(_) => continue,
        };
        for ent in rd.flatten() {
            seen += 1;
            if seen > loc.max_entries {
                budget_hit = true;
                break;
            }
            let path = ent.path();
            let ft = match ent.file_type() {
                Ok(ft) => ft,
                Err(_) => continue,
            };
            if ft.is_dir() {
                if depth < loc.max_depth {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            if !matches_name(&path, loc) {
                continue;
            }
            let mtime = ent
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);
            if mtime < newer_than {
                continue;
            }
            found.push((mtime, path));
        }
        if budget_hit {
            break;
        }
    }
    found.sort_by(|a, b| b.0.cmp(&a.0));
    let truncated = budget_hit || found.len() > max_files;
    found.truncate(max_files);
    (found.into_iter().map(|(_, p)| p).collect(), truncated)
}

/// 全エージェントのトークン集計。**背景スレッドから呼ぶこと**。
pub fn scan_tokens(since: SystemTime) -> Vec<AgentTokens> {
    match dirs::home_dir() {
        Some(h) => scan_tokens_in(&h, since),
        None => Vec::new(),
    }
}

/// ホーム相当のルートを注入する版 (テストはこれを使い、本物のホームを触らない)。
///
/// 消費がゼロのエージェントは**返さない**。「変化していないものは 1px も
/// 出さない」を、UI ではなくデータの側で担保する。
/// 戻りは消費の多い順 (どのエージェントが高いかが一目で分かるように)。
pub fn scan_tokens_in(home: &Path, since: SystemTime) -> Vec<AgentTokens> {
    scan_tokens_multi_in(home, &[since])
        .pop()
        .unwrap_or_default()
}

/// 起点を複数まとめて **1 パス**で集計する (戻りは `sinces` と同じ並び)。
///
/// 上限判定には「窓ぶん (24h)」「その日ぶん」「このセッションぶん」と別々の
/// 起点が要る。起点ごとに [`scan_tokens_in`] を呼ぶと同じファイルを何度も
/// 読むことになり、アイドル時の背景 I/O が起点の本数だけ増える (設計原則 3)。
/// ファイルの絞り込みは**最も古い起点**で 1 回だけ行い、以降は読み込んだ行を
/// 起点ごとの積み上げへ振り分けるだけにする。
///
/// 時刻が読めなかった行は [`scan_tokens_in`] と同じくどの起点にも数える。
/// 予算の見張りとしては「多め」へ倒すほうが安全なため (少なく見せない)。
pub fn scan_tokens_multi_in(home: &Path, sinces: &[SystemTime]) -> Vec<Vec<AgentTokens>> {
    /// 起点 1 つぶんの積み上げ。
    #[derive(Default)]
    struct Acc {
        by_model: HashMap<String, TokenUsage>,
        total: TokenUsage,
        turns: usize,
    }
    let mut out: Vec<Vec<AgentTokens>> = sinces.iter().map(|_| Vec::new()).collect();
    if sinces.is_empty() {
        return out;
    }
    let oldest = sinces.iter().copied().min().unwrap_or(UNIX_EPOCH);
    for d in AGENT_QUOTAS {
        let TokenSource::Transcript { locator, parser } = &d.tokens else {
            continue;
        };
        let (files, mut truncated) = recent_files(home, locator, oldest, TOKEN_MAX_FILES);
        let mut acc: Vec<Acc> = sinces.iter().map(|_| Acc::default()).collect();
        for path in files {
            // 末尾しか読まないので、それより長いファイルは先頭を取りこぼす。
            // 「実際はこれ以上」と正直に印を立てる (少なく見せない)。
            if std::fs::metadata(&path)
                .map(|m| m.len() > locator.tail_bytes)
                .unwrap_or(false)
            {
                truncated = true;
            }
            let Some(content) = read_tail(&path, locator.tail_bytes) else {
                continue;
            };
            for t in parser(&content) {
                let key = t.model.unwrap_or_default();
                for (i, since) in sinces.iter().enumerate() {
                    // 時刻が読めた行は窓で切る。読めない行は落とさない
                    // (ファイルの mtime で既に一番古い窓に入っているため)。
                    if t.at.map(|a| a < *since).unwrap_or(false) {
                        continue;
                    }
                    acc[i]
                        .by_model
                        .entry(key.clone())
                        .or_default()
                        .add(&t.usage);
                    acc[i].total.add(&t.usage);
                    acc[i].turns += 1;
                }
            }
        }
        for (i, a) in acc.into_iter().enumerate() {
            if a.turns == 0 {
                continue;
            }
            // 飽和した = これ以上は数えられていない
            let truncated = truncated || a.total.total() == u64::MAX;
            let mut by_model: Vec<(String, TokenUsage)> = a.by_model.into_iter().collect();
            by_model.sort_by(|x, y| y.1.total().cmp(&x.1.total()).then(x.0.cmp(&y.0)));
            out[i].push(AgentTokens {
                agent: d.bin.to_string(),
                label: d.label.to_string(),
                account: d.account.to_string(),
                total: a.total,
                by_model,
                turns: a.turns,
                truncated,
            });
        }
    }
    for v in out.iter_mut() {
        v.sort_by(|a, b| b.total.total().cmp(&a.total.total()));
    }
    out
}

// ── コスト推定 ─────────────────────────────────────────────────────────

/// 100 万トークンあたりの単価。
///
/// **モデル名も金額もこの module には無い。** 価格は変わるので、表は
/// `config.rs` が設定として持ち、ここは [`PriceLookup`] 越しに引くだけ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModelRate {
    pub input: f64,
    pub output: f64,
    pub cache_write: f64,
    pub cache_read: f64,
}

/// 単価の引き当て。実装は `config.rs` の設定側。
pub trait PriceLookup {
    /// モデル名 → 単価。表に無ければ None (**0 円にしない**)。
    fn rate(&self, model: &str) -> Option<ModelRate>;
    /// 通貨の表示記号。
    fn currency(&self) -> &str;
}

/// 推定コスト。**必ず「推定」として出すこと。**
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CostEstimate {
    /// 単価が引けたぶんの合計。
    pub amount: f64,
    /// 単価が引けなかったモデル名 (重複なし・昇順)。
    pub unknown_models: Vec<String>,
    /// 単価が引けなかったぶんのトークン数。
    pub unknown_tokens: u64,
}

impl CostEstimate {
    /// 全てのモデルの単価が引けたか。
    pub fn is_complete(&self) -> bool {
        self.unknown_models.is_empty()
    }

    /// 表示用の文字列。**必ず「推定」と明示し、不明ぶんは 0 円にしない**。
    pub fn label(&self, currency: &str) -> String {
        let money = format!("{currency}{:.2}", self.amount);
        if self.is_complete() {
            return trf("推定 {money}", &[("money", money)]);
        }
        if self.amount <= 0.0 {
            return trf(
                "推定不可 (単価未設定: {models})",
                &[("models", self.unknown_models.join(", "))],
            );
        }
        trf(
            "推定 {money} 以上 (単価未設定: {models})",
            &[("money", money), ("models", self.unknown_models.join(", "))],
        )
    }
}

/// 単価 1 つ分のコスト (通貨単位)。
///
/// `u64::MAX` を入れても `f64` の範囲 (約 1.8e19 × 単価 / 1e6) に収まるので
/// 桁あふれしない。
pub fn rate_cost(u: &TokenUsage, r: &ModelRate) -> f64 {
    (u.input as f64 * r.input
        + u.output as f64 * r.output
        + u.cache_write as f64 * r.cache_write
        + u.cache_read as f64 * r.cache_read)
        / 1_000_000.0
}

/// エージェント 1 本分の推定コスト。単価が引けないモデルは合計に入れず、
/// 名前を [`CostEstimate::unknown_models`] へ残す (**0 円にしない**)。
pub fn estimate_cost(t: &AgentTokens, prices: &dyn PriceLookup) -> CostEstimate {
    let mut est = CostEstimate::default();
    for (model, u) in &t.by_model {
        match prices.rate(model) {
            Some(r) => est.amount += rate_cost(u, &r),
            None => {
                est.unknown_tokens = est.unknown_tokens.saturating_add(u.total());
                let name = if model.is_empty() {
                    tr("(モデル不明)")
                } else {
                    model.clone()
                };
                if !est.unknown_models.contains(&name) {
                    est.unknown_models.push(name);
                }
            }
        }
    }
    est.unknown_models.sort();
    est
}

/// 数を「12.3k / 4.5M」へ丸める。
///
/// **桁が上がっても表示幅を暴れさせない**のが目的なので、`u64` の全域を
/// 覆う接尾辞まで用意する (実際のトークン数が E に届くことは無いが、
/// 幅の上限を型で保証しておく)。
pub fn short_tokens(n: u64) -> String {
    const UNITS: &[(u64, &str)] = &[
        (1_000_000_000_000_000_000, "E"),
        (1_000_000_000_000_000, "P"),
        (1_000_000_000_000, "T"),
        (1_000_000_000, "G"),
        (1_000_000, "M"),
        (1_000, "k"),
    ];
    for (scale, suffix) in UNITS {
        if n >= *scale {
            return format!("{:.1}{suffix}", n as f64 / *scale as f64);
        }
    }
    n.to_string()
}

// ── ステータスバーの並べ方 (純粋関数) ──────────────────────────────────

/// バッジ 1 列の配置。矩形は必ず可用領域に収まり、互いに重ならない。
#[derive(Debug, Clone, PartialEq)]
pub struct BadgeLayout {
    /// 描くか。消費ゼロ・幅不足なら false = **1px も出さない**。
    pub visible: bool,
    /// 詳細 (エージェント別) を諦めて合算 1 個へ縮退したか。
    pub compact: bool,
    /// 各要素の矩形 `(x, y, w, h)`。
    pub rects: Vec<(f32, f32, f32, f32)>,
}

impl BadgeLayout {
    /// 何も描かない。
    pub fn hidden() -> Self {
        Self {
            visible: false,
            compact: false,
            rects: Vec::new(),
        }
    }
}

/// ステータスバーのトークン/コストバッジをどう並べるかを決める。
///
/// - `avail`: 使ってよい領域 `(幅, 高さ)`
/// - `items`: 詳細表示のときの各バッジの希望幅 (エージェント別)
/// - `compact_w`: 合算 1 個へ縮退したときの幅
/// - `row_h` / `gap`: 行の高さと要素間の隙間
/// - `want_detail`: 利用者が詳細表示を選んでいるか
///
/// 詳細が入りきらなければ compact へ、compact も入らなければ非表示へ、と
/// 段階的に縮退する。**行は必ず `avail.0` に収まる** (見切れさせない)。
pub fn token_badge_layout(
    avail: (f32, f32),
    items: &[f32],
    compact_w: f32,
    row_h: f32,
    gap: f32,
    want_detail: bool,
) -> BadgeLayout {
    let (aw, ah) = avail;
    if items.is_empty() || aw <= 0.0 || ah <= 0.0 || row_h <= 0.0 || row_h > ah {
        return BadgeLayout::hidden();
    }
    let gap = gap.max(0.0);
    let place = |widths: &[f32]| -> Vec<(f32, f32, f32, f32)> {
        let mut x = 0.0f32;
        let mut out = Vec::with_capacity(widths.len());
        for w in widths {
            out.push((x, 0.0, *w, row_h));
            x += *w + gap;
        }
        out
    };
    let span = |widths: &[f32]| -> f32 {
        widths.iter().copied().sum::<f32>() + gap * (widths.len().saturating_sub(1)) as f32
    };
    if want_detail && items.iter().all(|w| *w > 0.0) && span(items) <= aw {
        return BadgeLayout {
            visible: true,
            compact: false,
            rects: place(items),
        };
    }
    if compact_w > 0.0 && compact_w <= aw {
        return BadgeLayout {
            visible: true,
            compact: true,
            rects: place(&[compact_w]),
        };
    }
    BadgeLayout::hidden()
}

// ── コスト上限とアラート ───────────────────────────────────────────────
//
// 「エージェントを N 本走らせて、気付いたら $200」を止めるための見張り。
// **金額も通貨もこの module には無い** — 上限は設定 (`config.rs`) から
// [`CostLimits`] として渡され、ここは比較と分類しかしない。

/// 1 日の長さ (秒)。日次集計の境界に使う。
pub const DAY_SECS: u64 = 24 * 3600;

/// `t` が属する「日」の通し番号 (UTC。1970-01-01 = 0)。
///
/// ## なぜローカル時刻ではなく UTC で切るのか
///
/// 1. **依存を増やさない。** `std` にはタイムゾーン DB が無い。ローカルの
///    暦日を正しく出すには `chrono` / `time` が要るが、この製品は依存を
///    増やさない方針 (うるう秒・夏時間・歴史的な UTC オフセット変更まで
///    抱え込むことになる)。
/// 2. **境界が動かない。** ローカル時刻で切ると、出張・OS のタイムゾーン
///    更新・夏時間の切り替えで「今日」の始まりが黙って前後する。夏時間の
///    移行日は 23 時間や 25 時間の「日」が生まれ、上限の意味が日によって
///    変わってしまう。UTC の通し番号は**どの環境でも同じ結果**になり、
///    テストに固定の epoch 秒を渡すだけで再現できる。
/// 3. **ロケールに依存しない。** 週の始まり・暦 (和暦/イスラム暦) ・
///    カレンダー設定のどれにも左右されない。
///
/// 表示では「今日 (UTC)」と明示すること。ユーザーの深夜 0 時とはズレる。
pub fn utc_day_index(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() / DAY_SECS
}

/// `t` が属する UTC の日の始まり。
pub fn utc_day_start(t: SystemTime) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(utc_day_index(t) * DAY_SECS)
}

/// 上限に対する現在の状態。深刻な順に大きい (`max` で最悪を取れる)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BudgetState {
    /// 余裕あり (上限が未設定のときもここ)。
    Normal,
    /// 警告割合を超えた。
    Warn,
    /// 上限に達した / 超えた。
    Over,
}

/// 上限に達したときの動作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LimitAction {
    /// 知らせるだけ。**既定** — 勝手に止めない。
    #[default]
    Notify,
    /// 新規の送信を止める。
    Stop,
}

impl LimitAction {
    /// 設定の文字列から。未知の値は既定 (`Notify`) — 打ち間違いで
    /// 勝手に止まるほうが害が大きい。
    ///
    /// 毎フレーム呼ばれる経路 (送信の門) にいるので**確保を作らない**
    /// (`to_ascii_lowercase` は String を確保する)。
    pub fn from_key(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("stop") {
            Self::Stop
        } else {
            Self::Notify
        }
    }

    /// 設定へ書き戻す文字列。
    pub fn key(self) -> &'static str {
        match self {
            Self::Notify => "notify",
            Self::Stop => "stop",
        }
    }
}

/// どの上限か。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetKind {
    /// このアプリを起動してからの消費。
    Session,
    /// 今日 (UTC) の消費。
    Daily,
}

impl BudgetKind {
    /// 画面に出す名前。
    pub fn label(self) -> String {
        match self {
            Self::Session => tr("このセッション"),
            Self::Daily => tr("今日 (UTC)"),
        }
    }
}

/// **判定の中心にある純粋関数**: (消費, 上限, 警告割合) → 状態。
///
/// 規則 (この順で判定する):
///
/// 1. 上限が有限の正数でなければ `Normal` (0 / 負 / NaN / ∞ = 無制限)
/// 2. 消費が NaN なら `Normal` (数えられていないものを咎めない)
/// 3. 消費 ≥ 上限 なら `Over` (**ちょうど上限は Over**。「上限まで使える」の
///    「まで」を含む側に倒す — 超えてから止めるのでは遅い)
/// 4. 消費 > 0 かつ 消費 ≥ 上限 × 警告割合 なら `Warn`
///    (割合は 0.0..=1.0 へ丸める。消費 0 は割合 0 でも `Normal`)
/// 5. それ以外は `Normal`
pub fn budget_state(spent: f64, limit: f64, warn_ratio: f32) -> BudgetState {
    if !limit.is_finite() || limit <= 0.0 {
        return BudgetState::Normal;
    }
    if spent.is_nan() {
        return BudgetState::Normal;
    }
    if spent >= limit {
        return BudgetState::Over;
    }
    // `f32` の 0.8 は正確な 0.8 ではない (0.800000011920929…)。そのまま掛けると
    // 上限 50 の 8 割が 40.0000005 になり、「ちょうど 40」が警告に入らない。
    // 設定に書くのは 10 進の割合なので、6 桁で丸めて意図した値へ戻してから比べる
    // (f32 の有効桁は約 7 桁なので、これで打ち込んだ小数がそのまま戻る)。
    // NaN は clamp も round も NaN のままなので、比較が false = Normal になる。
    let ratio = (f64::from(warn_ratio.clamp(0.0, 1.0)) * 1e6).round() / 1e6;
    if spent > 0.0 && spent >= limit * ratio {
        return BudgetState::Warn;
    }
    BudgetState::Normal
}

/// 上限 1 件の判定結果。
#[derive(Debug, Clone, PartialEq)]
pub struct BudgetStatus {
    pub kind: BudgetKind,
    pub state: BudgetState,
    /// 推定消費 (通貨単位)。
    pub spent: f64,
    /// 上限 (通貨単位)。必ず正。
    pub limit: f64,
}

impl BudgetStatus {
    /// 上限に対する割合 (0.0..)。表示用。
    pub fn fraction(&self) -> f64 {
        if self.limit > 0.0 {
            (self.spent / self.limit).max(0.0)
        } else {
            0.0
        }
    }

    /// ステータスバーに出す短い文字列 (`$12.34 / $50.00`)。
    /// 通貨記号は設定から渡す — ここに書かない。
    pub fn short_label(&self, currency: &str) -> String {
        format!("{currency}{:.2} / {currency}{:.2}", self.spent, self.limit)
    }
}

/// コスト上限の設定 (金額は `config.rs` から来る)。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CostLimits {
    /// セッション単位の上限。0 以下 = 無制限。
    pub session: f64,
    /// 1 日 (UTC) の上限。0 以下 = 無制限。
    pub daily: f64,
    /// 上限の何割で警告するか。
    pub warn_ratio: f32,
    /// 上限に達したときの動作。
    pub action: LimitAction,
}

impl Default for CostLimits {
    /// **上限なし** が既定。設定しない限り 1px も出さないし、何も止めない。
    fn default() -> Self {
        Self {
            session: 0.0,
            daily: 0.0,
            warn_ratio: 0.0,
            action: LimitAction::Notify,
        }
    }
}

impl CostLimits {
    /// 上限が 1 つでも設定されているか。false なら**画面に何も出さない**。
    pub fn any(&self) -> bool {
        (self.session.is_finite() && self.session > 0.0)
            || (self.daily.is_finite() && self.daily > 0.0)
    }

    /// 設定されている上限だけを判定する (未設定の上限は結果に入らない)。
    /// 並びは深刻な順 → 同率なら日次を先に (期間の長いほうが重い)。
    pub fn evaluate(&self, session_spent: f64, daily_spent: f64) -> Vec<BudgetStatus> {
        let mut out = Vec::new();
        for (kind, spent, limit) in [
            (BudgetKind::Daily, daily_spent, self.daily),
            (BudgetKind::Session, session_spent, self.session),
        ] {
            if !limit.is_finite() || limit <= 0.0 {
                continue;
            }
            out.push(BudgetStatus {
                kind,
                state: budget_state(spent, limit, self.warn_ratio),
                spent,
                limit,
            });
        }
        out.sort_by(|a, b| b.state.cmp(&a.state));
        out
    }

    /// 最も深刻な 1 件 (上限が 1 つも無ければ None)。
    pub fn worst(&self, session_spent: f64, daily_spent: f64) -> Option<BudgetStatus> {
        self.evaluate(session_spent, daily_spent).into_iter().next()
    }

    /// 新規の送信を止めるべきか。`Stop` を選んでいて、かつ `Over` のときだけ。
    pub fn blocks(&self, session_spent: f64, daily_spent: f64) -> Option<BudgetStatus> {
        if self.action != LimitAction::Stop {
            return None;
        }
        self.worst(session_spent, daily_spent)
            .filter(|s| s.state == BudgetState::Over)
    }
}

/// 通知を「入った瞬間に一度だけ」にするための鍵。
///
/// [`crate::notify::EdgeGate`] へ渡す。同じ状態が続く限り同じ文字列になるので
/// 毎フレーム鳴らない。`Normal` と「上限なし」は空文字 = 鳴らす対象ではない。
pub fn budget_edge_key(worst: Option<&BudgetStatus>) -> String {
    match worst {
        Some(s) if s.state != BudgetState::Normal => {
            format!("{:?}/{:?}", s.kind, s.state)
        }
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(epoch: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(epoch)
    }

    fn rollout_line(ts: &str, pct: f64, resets_at: i64) -> String {
        format!(
            r#"{{"timestamp":"{ts}","type":"event_msg","payload":{{"type":"token_count","info":{{"total_token_usage":{{"input_tokens":1}}}},"rate_limits":{{"limit_id":"x","primary":{{"used_percent":{pct},"window_minutes":300,"resets_at":{resets_at}}},"secondary":null,"plan_type":"pro"}}}}}}"#
        )
    }

    // ── パーサ (内容だけを受け取る純関数) ───────────────────────────

    /// codex のロールアウト行から使用率・窓・リセット時刻・プランを読む。
    #[test]
    fn codex_rollout_parses_primary_bucket() {
        let c = rollout_line("2026-07-25T03:36:12.345Z", 42.5, 1_784_000_000);
        let u = parse_codex_rollout(&c).expect("読めるはず");
        assert!((u.used_fraction - 0.425).abs() < 1e-6);
        assert_eq!(u.window, Some(Duration::from_secs(5 * 3600)));
        assert_eq!(u.resets_at, Some(t(1_784_000_000)));
        assert_eq!(u.plan.as_deref(), Some("pro"));
        assert!(u.observed_at.is_some());
    }

    /// 複数行あれば**最後の**行を採る (最新の使用率)。
    #[test]
    fn codex_rollout_takes_last_line() {
        let c = format!(
            "{}\n{}\n",
            rollout_line("2026-07-25T03:00:00Z", 10.0, 1),
            rollout_line("2026-07-25T04:00:00Z", 77.0, 1_784_000_000)
        );
        let u = parse_codex_rollout(&c).unwrap();
        assert!((u.used_fraction - 0.77).abs() < 1e-6);
    }

    /// `resets_at` が無くても `resets_in_seconds` + 行の時刻で補える。
    #[test]
    fn codex_rollout_relative_reset() {
        let c = r#"{"timestamp":"2026-07-25T00:00:00Z","payload":{"rate_limits":{"primary":{"used_percent":50,"window_minutes":300,"resets_in_seconds":600}}}}"#;
        let u = parse_codex_rollout(c).unwrap();
        assert_eq!(
            u.resets_at,
            Some(parse_rfc3339("2026-07-25T00:10:00Z").unwrap())
        );
    }

    /// 壊れた入力でも**絶対に panic せず** None を返す。
    #[test]
    fn codex_rollout_malformed_is_none() {
        let cases = [
            "",
            "   \n\n",
            "not json at all",
            "{",
            r#"{"payload":{}}"#,
            r#"{"payload":{"rate_limits":null}}"#,
            r#"{"payload":{"rate_limits":{"primary":null,"secondary":null}}}"#,
            r#"{"payload":{"rate_limits":{"primary":{"used_percent":"98"}}}}"#,
            r#"{"payload":{"rate_limits":{"primary":{}}}}"#,
        ];
        for c in cases {
            assert_eq!(parse_codex_rollout(c), None, "入力: {c:?}");
        }
    }

    /// 末尾読みで先頭行が切れていても、後続の正しい行を拾える。
    #[test]
    fn codex_rollout_skips_truncated_first_line() {
        let c = format!(
            "imestamp\":\"2026\",\"payload\":{{\"rate_limits\":\n{}",
            rollout_line("2026-07-25T03:00:00Z", 12.0, 1_784_000_000)
        );
        assert!(parse_codex_rollout(&c).is_some());
    }

    /// 100 超え / 負値は 0..1 に丸める (異常値をそのまま出さない)。
    #[test]
    fn codex_rollout_clamps_percent() {
        let hi = rollout_line("2026-07-25T03:00:00Z", 250.0, 1);
        assert_eq!(parse_codex_rollout(&hi).unwrap().used_fraction, 1.0);
        let lo = rollout_line("2026-07-25T03:00:00Z", -5.0, 1);
        assert_eq!(parse_codex_rollout(&lo).unwrap().used_fraction, 0.0);
    }

    /// RFC3339 の読み取り表 (Z / オフセット / 壊れ)。
    #[test]
    fn rfc3339_table() {
        assert_eq!(parse_rfc3339("1970-01-01T00:00:00Z"), Some(t(0)));
        assert_eq!(parse_rfc3339("1970-01-02T00:00:00Z"), Some(t(86_400)));
        assert_eq!(
            parse_rfc3339("2026-07-25T03:36:12.345Z"),
            parse_rfc3339("2026-07-25T03:36:12Z")
        );
        // +09:00 は UTC より 9 時間進んでいる → エポックは 9 時間手前
        let jst = parse_rfc3339("2026-07-25T09:00:00+09:00").unwrap();
        assert_eq!(jst, parse_rfc3339("2026-07-25T00:00:00Z").unwrap());
        for bad in [
            "",
            "abc",
            "2026-07-25",
            "2026/07/25T00:00:00Z",
            "2026-13-01T00:00:00Z",
        ] {
            assert_eq!(parse_rfc3339(bad), None, "入力: {bad:?}");
        }
    }

    /// **純関数はファイルを読まない**証明: パス文字列を渡しても中身を読まず
    /// None を返し、ディスクにも何も作らない。
    #[test]
    fn pure_parsers_never_touch_the_filesystem() {
        let dir = crate::test_util::unique_temp_dir("zaivern-quota-test", "purity");
        let file = dir.join("rollout-2026.jsonl");
        std::fs::write(&file, rollout_line("2026-07-25T03:00:00Z", 88.0, 1)).unwrap();
        // パスを渡す = 「内容」ではないので読めない (= I/O していない証拠)
        assert_eq!(parse_codex_rollout(file.to_str().unwrap()), None);
        assert_eq!(parse_rfc3339(file.to_str().unwrap()), None);
        // 内容を渡せば読める
        let content = std::fs::read_to_string(&file).unwrap();
        assert!(parse_codex_rollout(&content).is_some());
        // 純関数はファイルを増やさない
        let n = std::fs::read_dir(&dir).unwrap().count();
        assert_eq!(n, 1, "純関数がファイルを作っていない");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 探索と末尾読み (ルート注入) ─────────────────────────────────

    fn codex_locator() -> &'static FileLocator {
        match &descriptor("codex").unwrap().source {
            QuotaSource::VendorFile { locator, .. } => locator,
            _ => unreachable!("codex はベンダーファイル持ち"),
        }
    }

    /// 無いディレクトリでも panic せず None。
    #[test]
    fn newest_file_missing_dir_is_none() {
        let dir = crate::test_util::unique_temp_dir("zaivern-quota-test", "missing");
        assert_eq!(newest_file(&dir, codex_locator()), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 再帰的に探して最も新しいものを返し、名前が合わないものは無視する。
    #[test]
    fn newest_file_picks_newest_matching() {
        let home = crate::test_util::unique_temp_dir("zaivern-quota-test", "newest");
        let sess = home.join(".codex").join("sessions").join("2026").join("07");
        std::fs::create_dir_all(&sess).unwrap();
        let old = sess.join("rollout-old.jsonl");
        let new = sess.join("rollout-new.jsonl");
        let other = sess.join("notes.txt");
        std::fs::write(&old, "a").unwrap();
        std::fs::write(&other, "b").unwrap();
        // mtime の前後関係を確実にするため少し待ってから新しい方を書く
        // (std だけで mtime は設定できない)
        std::thread::sleep(Duration::from_millis(30));
        std::fs::write(&new, "c").unwrap();
        let found = newest_file(&home, codex_locator()).expect("見つかる");
        assert_eq!(
            found.file_name().unwrap(),
            new.file_name().unwrap(),
            "拡張子違いは無視し、最も新しい rollout- を選ぶ"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 末尾読みは最大バイト数を守り、切れた先頭行を落とす。
    #[test]
    fn read_tail_drops_partial_first_line() {
        let dir = crate::test_util::unique_temp_dir("zaivern-quota-test", "tail");
        let f = dir.join("x.jsonl");
        std::fs::write(&f, "aaaaaaaaaa\nbbbb\ncccc\n").unwrap();
        let tail = read_tail(&f, 12).unwrap();
        assert!(!tail.contains("aaaa"), "切れた先頭行は落とす");
        assert!(tail.contains("cccc"));
        // 全部入るときはそのまま
        let whole = read_tail(&f, 1024).unwrap();
        assert!(whole.starts_with("aaaaaaaaaa"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── スナップショット ────────────────────────────────────────────

    /// 空のホームでも全エージェント分が出て、実測は付かない。
    #[test]
    fn snapshot_empty_home_is_unavailable() {
        let home = crate::test_util::unique_temp_dir("zaivern-quota-test", "empty-home");
        let snaps = snapshot_all_in(&home);
        assert_eq!(snaps.len(), AGENT_QUOTAS.len());
        for s in &snaps {
            assert_eq!(s.used_fraction, None);
            assert_eq!(s.source, SourceKind::Unavailable);
            assert!(s.observed_events.is_empty());
        }
        let _ = std::fs::remove_dir_all(&home);
    }

    /// ベンダーファイルがあれば実測が付く。
    #[test]
    fn snapshot_reads_vendor_file() {
        let home = crate::test_util::unique_temp_dir("zaivern-quota-test", "vendor");
        let sess = home.join(".codex").join("sessions").join("2026");
        std::fs::create_dir_all(&sess).unwrap();
        std::fs::write(
            sess.join("rollout-a.jsonl"),
            rollout_line("2026-07-25T03:00:00Z", 61.0, 1_784_000_000),
        )
        .unwrap();
        let snaps = snapshot_all_in(&home);
        let codex = snaps.iter().find(|s| s.agent == "codex").unwrap();
        assert_eq!(codex.source, SourceKind::Vendor);
        assert!((codex.used_fraction.unwrap() - 0.61).abs() < 1e-6);
        assert!((codex.remaining_fraction().unwrap() - 0.39).abs() < 1e-6);
        let claude = snaps.iter().find(|s| s.agent == "claude").unwrap();
        assert_eq!(claude.source, SourceKind::Unavailable, "claude は観測のみ");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 観測イベントは該当エージェントにだけ付き、出どころが格上げされる。
    #[test]
    fn merge_observed_attaches_events() {
        let home = crate::test_util::unique_temp_dir("zaivern-quota-test", "merge");
        let mut snaps = snapshot_all_in(&home);
        let ev = RateLimitEvent {
            agent: "claude".into(),
            at: t(1000),
            line: "5-hour limit reached".into(),
        };
        merge_observed(&mut snaps, &[ev.clone()]);
        let claude = snaps.iter().find(|s| s.agent == "claude").unwrap();
        assert_eq!(claude.observed_events, vec![ev]);
        assert_eq!(claude.source, SourceKind::Observed);
        let codex = snaps.iter().find(|s| s.agent == "codex").unwrap();
        assert!(codex.observed_events.is_empty());
        assert_eq!(codex.source, SourceKind::Unavailable);
        let _ = std::fs::remove_dir_all(&home);
    }

    /// 既存のレート制限検知を再利用している (複製していない)。
    #[test]
    fn observe_output_reuses_terminal_detector() {
        let e = observe_output("claude", "5-hour limit reached ∙ resets 3am\n", t(5)).unwrap();
        assert_eq!(e.agent, "claude");
        assert_eq!(e.at, t(5));
        assert!(e.line.contains("limit reached"));
        assert!(observe_output("claude", "ふつうの出力です\n", t(5)).is_none());
    }

    // ── 集約 ────────────────────────────────────────────────────────

    fn snap(agent: &str, account: &str, used: Option<f32>, src: SourceKind) -> QuotaSnapshot {
        QuotaSnapshot {
            agent: agent.into(),
            label: agent.into(),
            account: account.into(),
            used_fraction: used,
            resets_at: None,
            window: None,
            plan: None,
            observed_events: Vec::new(),
            source: src,
            measured_at: None,
        }
    }

    /// 同じアカウントを共有するエージェントは 1 行に畳まれ、走行本数が合算される。
    #[test]
    fn aggregate_merges_shared_account() {
        let snaps = vec![
            snap("codex", "openai", Some(0.4), SourceKind::Vendor),
            snap("codex-mini", "openai", Some(0.6), SourceKind::Vendor),
        ];
        let running = vec![("codex".to_string(), 2), ("codex-mini".to_string(), 3)];
        let acc = aggregate(&snaps, &running);
        assert_eq!(acc.len(), 1);
        assert_eq!(acc[0].agents, vec!["codex", "codex-mini"]);
        // 合計 (1.0) ではなく最大 (0.6) — 各報告はアカウント全体の値だから
        assert!((acc[0].used_fraction.unwrap() - 0.6).abs() < 1e-6);
        assert_eq!(acc[0].running_agents, 5);
        assert_eq!(acc[0].confidence, Confidence::Measured);
    }

    /// アカウントが違えば混ざらない。
    #[test]
    fn aggregate_keeps_accounts_separate() {
        let snaps = vec![
            snap("claude", "anthropic", None, SourceKind::Unavailable),
            snap("codex", "openai", Some(0.9), SourceKind::Vendor),
        ];
        let acc = aggregate(&snaps, &[]);
        assert_eq!(acc.len(), 2);
        let anth = acc.iter().find(|a| a.account == "anthropic").unwrap();
        assert_eq!(anth.used_fraction, None);
        assert_eq!(anth.confidence, Confidence::Unknown);
        let oai = acc.iter().find(|a| a.account == "openai").unwrap();
        assert_eq!(oai.confidence, Confidence::Measured);
    }

    /// リセット時刻は最も早いものを採る。
    #[test]
    fn aggregate_takes_earliest_reset() {
        let mut a = snap("codex", "openai", Some(0.1), SourceKind::Vendor);
        a.resets_at = Some(t(2000));
        let mut b = snap("codex-mini", "openai", Some(0.2), SourceKind::Vendor);
        b.resets_at = Some(t(1000));
        let acc = aggregate(&[a, b], &[]);
        assert_eq!(acc[0].resets_at, Some(t(1000)));
    }

    // ── 燃焼速度 ────────────────────────────────────────────────────

    fn samples(pairs: &[(u64, f32)]) -> Vec<BurnSample> {
        pairs
            .iter()
            .map(|(s, u)| BurnSample {
                at: t(*s),
                used_fraction: *u,
            })
            .collect()
    }

    /// 材料不足のケースは None (推測を返さない)。
    #[test]
    fn burn_rate_insufficient_data() {
        let now = t(1_000);
        let w = Duration::from_secs(3600);
        assert!(burn_rate(&[], w, now).is_none(), "標本ゼロ");
        assert!(
            burn_rate(&samples(&[(900, 0.5)]), w, now).is_none(),
            "標本 1 つ"
        );
        assert!(
            burn_rate(&samples(&[(900, 0.5), (900, 0.7)]), w, now).is_none(),
            "時間幅ゼロ"
        );
        assert!(
            burn_rate(
                &samples(&[(0, 0.1), (10, 0.2)]),
                Duration::from_secs(60),
                now
            )
            .is_none(),
            "全部が窓の外"
        );
    }

    /// 2 点なら推定、標本と時間幅が揃えば実測。
    #[test]
    fn burn_rate_confidence_levels() {
        let now = t(1_000);
        let w = Duration::from_secs(3600);
        let r = burn_rate(&samples(&[(900, 0.10), (1000, 0.30)]), w, now).unwrap();
        assert!((r.per_sec - 0.002).abs() < 1e-6, "0.2 / 100s");
        assert_eq!(r.confidence, Confidence::Estimated, "2 点は推定どまり");
        let r = burn_rate(
            &samples(&[(400, 0.10), (600, 0.20), (800, 0.30), (1000, 0.40)]),
            w,
            now,
        )
        .unwrap();
        assert!((r.per_sec - 0.0005).abs() < 1e-6, "0.3 / 600s");
        assert_eq!(r.confidence, Confidence::Measured);
        assert_eq!(r.samples, 4);
        assert_eq!(r.span, Duration::from_secs(600));
    }

    /// 枠のリセット (使用率の下落) を跨いだ平均は取らない。
    #[test]
    fn burn_rate_restarts_after_reset() {
        let now = t(1_000);
        let s = samples(&[
            (200, 0.80),
            (400, 0.90),
            (600, 0.05),
            (800, 0.15),
            (1000, 0.25),
        ]);
        let r = burn_rate(&s, Duration::from_secs(3600), now).unwrap();
        assert_eq!(r.samples, 3, "リセット後の 3 点だけ");
        assert!((r.per_sec - 0.0005).abs() < 1e-6, "0.2 / 400s");
    }

    /// 減っている (=燃えていない) 場合は速度 0。
    #[test]
    fn burn_rate_flat_is_zero() {
        let now = t(1_000);
        let r = burn_rate(
            &samples(&[(400, 0.5), (700, 0.5), (1000, 0.5)]),
            Duration::from_secs(3600),
            now,
        )
        .unwrap();
        assert_eq!(r.per_sec, 0.0);
    }

    /// 履歴は上限で刈られ、同時刻は上書きされる。
    #[test]
    fn burn_history_is_bounded() {
        let mut h = BurnHistory::new();
        for i in 0..(BurnHistory::CAP + 50) as u64 {
            h.record("openai", t(i), (i as f32) / 10_000.0);
        }
        assert_eq!(h.samples("openai").len(), BurnHistory::CAP);
        h.record("openai", t((BurnHistory::CAP + 49) as u64), 0.9);
        assert_eq!(
            h.samples("openai").len(),
            BurnHistory::CAP,
            "同時刻は上書き"
        );
        assert_eq!(h.samples("openai").last().unwrap().used_fraction, 0.9);
        assert!(h.samples("unknown-account").is_empty());
    }

    // ── 予測 ────────────────────────────────────────────────────────

    fn burn(per_sec: f32) -> BurnRate {
        BurnRate {
            per_sec,
            samples: 4,
            span: Duration::from_secs(600),
            confidence: Confidence::Measured,
        }
    }

    /// 予測の表 (材料不足・非燃焼・枯渇・リセット先着・既に枯渇)。
    #[test]
    fn projection_table() {
        let now = t(10_000);
        assert_eq!(
            projected_exhaustion(now, None, Some(0.5), None),
            Projection::InsufficientData,
            "速度が無い"
        );
        assert_eq!(
            projected_exhaustion(now, Some(&burn(0.001)), None, None),
            Projection::InsufficientData,
            "残りが分からない"
        );
        assert_eq!(
            projected_exhaustion(now, Some(&burn(0.0)), Some(0.5), None),
            Projection::NotBurning
        );
        assert_eq!(
            projected_exhaustion(now, Some(&burn(0.001)), Some(0.3), None),
            Projection::Exhaustion(Duration::from_secs(300))
        );
        assert_eq!(
            projected_exhaustion(now, Some(&burn(0.001)), Some(0.0), None),
            Projection::Exhaustion(Duration::ZERO),
            "残りゼロは即枯渇"
        );
        // 枯渇 300 秒後 > リセット 100 秒後 → リセットが先
        assert_eq!(
            projected_exhaustion(now, Some(&burn(0.001)), Some(0.3), Some(t(10_100))),
            Projection::ResetFirst(Duration::from_secs(100))
        );
        // リセットが枯渇より後なら枯渇のまま
        assert_eq!(
            projected_exhaustion(now, Some(&burn(0.001)), Some(0.3), Some(t(20_000))),
            Projection::Exhaustion(Duration::from_secs(300))
        );
    }

    /// 素の計算は非有限・極小速度・遠すぎる見積りを弾く。
    #[test]
    fn exhaustion_after_guards() {
        assert_eq!(exhaustion_after(0.0, 0.5), None);
        assert_eq!(exhaustion_after(f32::NAN, 0.5), None);
        assert_eq!(exhaustion_after(0.001, f32::INFINITY), None);
        assert_eq!(exhaustion_after(1e-12, 1.0), None, "遅すぎる = 予測しない");
        assert_eq!(exhaustion_after(0.01, 1.0), Some(Duration::from_secs(100)));
    }

    /// 予測はアカウントへ貼れる。
    #[test]
    fn attach_projection_fills_account() {
        let mut u = aggregate(
            &[snap("codex", "openai", Some(0.7), SourceKind::Vendor)],
            &[],
        )
        .pop()
        .unwrap();
        attach_projection(&mut u, Some(&burn(0.001)), t(0));
        assert_eq!(
            u.projection,
            Projection::Exhaustion(Duration::from_secs(300))
        );
    }

    // ── 助言 ────────────────────────────────────────────────────────

    fn account(used: Option<f32>, proj: Projection, events: Vec<RateLimitEvent>) -> AccountUsage {
        AccountUsage {
            account: "openai".into(),
            agents: vec!["codex".into()],
            used_fraction: used,
            confidence: if used.is_some() {
                Confidence::Measured
            } else {
                Confidence::Unknown
            },
            resets_at: None,
            running_agents: 1,
            events,
            projection: proj,
        }
    }

    fn ev(at: u64) -> RateLimitEvent {
        RateLimitEvent {
            agent: "codex".into(),
            at: t(at),
            line: "usage limit reached".into(),
        }
    }

    /// 助言の決定表。上から順に最初に当たったものが返る。
    #[test]
    fn advise_decision_table() {
        let p = Policy::default();
        let now = t(100_000);
        // 余裕あり
        assert_eq!(
            advise(
                &account(Some(0.10), Projection::NotBurning, vec![]),
                1,
                &p,
                now
            ),
            Advice::Ok
        );
        // 既に上限に当たっている → 止める (予測より観測が強い)
        assert!(matches!(
            advise(
                &account(Some(0.10), Projection::NotBurning, vec![ev(99_700)]),
                1,
                &p,
                now
            ),
            Advice::Stop {
                reason: AdviceReason::AlreadyLimited { .. }
            }
        ));
        // 使用率が停止しきい値超え
        assert!(matches!(
            advise(
                &account(Some(0.96), Projection::NotBurning, vec![]),
                1,
                &p,
                now
            ),
            Advice::Stop {
                reason: AdviceReason::HighUsage { percent: 96 }
            }
        ));
        // 予測枯渇が 5 分以内 → 止める
        assert!(matches!(
            advise(
                &account(
                    Some(0.30),
                    Projection::Exhaustion(Duration::from_secs(120)),
                    vec![]
                ),
                4,
                &p,
                now
            ),
            Advice::Stop {
                reason: AdviceReason::ExhaustsSoon { running: 4, .. }
            }
        ));
        // 予測枯渇が 30 分以内 → 絞る
        assert!(matches!(
            advise(
                &account(
                    Some(0.30),
                    Projection::Exhaustion(Duration::from_secs(1200)),
                    vec![]
                ),
                2,
                &p,
                now
            ),
            Advice::SlowDown {
                reason: AdviceReason::ExhaustsSoon { .. }
            }
        ));
        // 使用率が警戒しきい値超え
        assert!(matches!(
            advise(
                &account(Some(0.85), Projection::NotBurning, vec![]),
                1,
                &p,
                now
            ),
            Advice::SlowDown {
                reason: AdviceReason::HighUsage { percent: 85 }
            }
        ));
        // 少し前の上限警告が積み上がっている (窓の外だが 4 倍窓の中)
        assert!(matches!(
            advise(
                &account(
                    None,
                    Projection::InsufficientData,
                    vec![ev(98_000), ev(98_500)]
                ),
                1,
                &p,
                now
            ),
            Advice::SlowDown {
                reason: AdviceReason::RepeatedLimits { count: 2 }
            }
        ));
        // 並列が多く枠も半分以上
        assert!(matches!(
            advise(
                &account(Some(0.60), Projection::NotBurning, vec![]),
                3,
                &p,
                now
            ),
            Advice::SlowDown {
                reason: AdviceReason::Crowded {
                    running: 3,
                    percent: 60
                }
            }
        ));
        // 情報が無ければ黙る (推測で騒がない)
        assert_eq!(
            advise(
                &account(None, Projection::InsufficientData, vec![]),
                5,
                &p,
                now
            ),
            Advice::Ok
        );
    }

    /// 助言は必ず日本語の本文と深刻度を持つ (UI がそのまま出せる)。
    #[test]
    fn advice_has_message_and_severity() {
        assert_eq!(Advice::Ok.severity(), 0);
        assert!(Advice::Ok.message().is_empty());
        let a = Advice::SlowDown {
            reason: AdviceReason::ExhaustsSoon {
                in_secs: 601,
                running: 3,
            },
        };
        assert_eq!(a.severity(), 1);
        assert!(a.message().contains("11 分"), "601 秒 → 切り上げ 11 分");
        assert!(a.message().contains('3'));
        let s = Advice::Stop {
            reason: AdviceReason::HighUsage { percent: 97 },
        };
        assert_eq!(s.severity(), 2);
        assert!(s.message().contains("97"));
    }

    /// 表示用のパーセント換算。
    #[test]
    fn pct_rounds_and_clamps() {
        assert_eq!(pct(0.0), 0);
        assert_eq!(pct(0.615), 62);
        assert_eq!(pct(1.5), 100);
        assert_eq!(pct(-1.0), 0);
    }

    /// 記述子表の健全性 (bin の重複が無い / claude は観測のみ)。
    #[test]
    fn descriptor_table_is_sane() {
        let mut bins: Vec<&str> = AGENT_QUOTAS.iter().map(|a| a.bin).collect();
        let n = bins.len();
        bins.sort();
        bins.dedup();
        assert_eq!(bins.len(), n, "bin が重複していない");
        assert!(descriptor("codex").is_some());
        assert!(descriptor("nonexistent-cli").is_none());
        assert!(matches!(
            descriptor("claude").unwrap().source,
            QuotaSource::ObservedOnly
        ));
    }
}

// ── トークン消費 / コスト推定のテスト ─────────────────────────────────

#[cfg(test)]
mod token_tests {
    use super::*;

    /// 行の高さ。テストで使う想定値 (ステータスバーの内寸)。
    const TOKEN_ROW_TEST_H: f32 = 18.0;

    /// テスト用の単価表。**本物の設定 (`config.rs`) には触らない。**
    struct Prices(Vec<(&'static str, ModelRate)>);

    impl Prices {
        fn one(name: &'static str, input: f64, output: f64) -> Self {
            Self(vec![(
                name,
                ModelRate {
                    input,
                    output,
                    cache_write: input * 1.25,
                    cache_read: input * 0.1,
                },
            )])
        }
        fn empty() -> Self {
            Self(Vec::new())
        }
    }

    impl PriceLookup for Prices {
        fn rate(&self, model: &str) -> Option<ModelRate> {
            self.0
                .iter()
                .filter(|(k, _)| !k.is_empty() && model.starts_with(k))
                .max_by_key(|(k, _)| k.len())
                .map(|(_, r)| *r)
        }
        fn currency(&self) -> &str {
            "$"
        }
    }

    fn tokens(agent: &str, by_model: &[(&str, TokenUsage)]) -> AgentTokens {
        let mut total = TokenUsage::default();
        for (_, u) in by_model {
            total.add(u);
        }
        AgentTokens {
            agent: agent.into(),
            label: agent.into(),
            account: agent.into(),
            total,
            by_model: by_model
                .iter()
                .map(|(m, u)| ((*m).to_string(), *u))
                .collect(),
            turns: by_model.len(),
            truncated: false,
        }
    }

    // ── パーサ: 正常系 ────────────────────────────────────────────────

    /// Claude Code のトランスクリプトから 4 種類のトークンとモデル名が取れる。
    /// (実ファイルから起こした形。`message.usage` は assistant 行にだけ付く)
    #[test]
    fn claude_のトランスクリプトから_usage_が取れる() {
        let content = concat!(
            r#"{"type":"user","timestamp":"2026-07-20T18:35:10.000Z","message":{"role":"user","content":"hi"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-07-20T18:35:15.437Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":2,"cache_creation_input_tokens":17296,"cache_read_input_tokens":14953,"output_tokens":74}}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-07-20T18:36:00.000Z","message":{"model":"claude-opus-4-8","usage":{"input_tokens":5,"cache_creation_input_tokens":0,"cache_read_input_tokens":32249,"output_tokens":120}}}"#,
            "\n",
        );
        let turns = parse_claude_transcript(content);
        assert_eq!(turns.len(), 2, "assistant 行だけを拾う");
        assert_eq!(
            turns[0].usage,
            TokenUsage {
                input: 2,
                output: 74,
                cache_write: 17296,
                cache_read: 14953,
            }
        );
        assert_eq!(turns[0].model.as_deref(), Some("claude-opus-4-8"));
        assert!(turns[0].at.is_some(), "時刻が読める");
        assert_eq!(turns[1].usage.cache_read, 32249);
    }

    /// codex のロールアウトからは **last_token_usage** を拾う。
    /// total は累計なので足すと二重計上になる (last の総和 = total)。
    #[test]
    fn codex_のロールアウトから_token_count_が取れる() {
        let content = concat!(
            r#"{"timestamp":"2026-08-09T08:22:30.000Z","type":"session_meta","payload":{"id":"x","cwd":"/tmp"}}"#,
            "\n",
            r#"{"timestamp":"2026-08-09T08:23:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"model":"gpt-x","total_token_usage":{"input_tokens":19594,"cached_input_tokens":0,"output_tokens":1000,"total_tokens":19594},"last_token_usage":{"input_tokens":19594,"cached_input_tokens":0,"output_tokens":1000,"total_tokens":19594}}}}"#,
            "\n",
            r#"{"timestamp":"2026-08-09T08:24:00.000Z","type":"event_msg","payload":{"type":"token_count","info":{"model":"gpt-x","total_token_usage":{"input_tokens":44626,"cached_input_tokens":8960,"output_tokens":2000,"total_tokens":44626},"last_token_usage":{"input_tokens":25032,"cached_input_tokens":8960,"output_tokens":1000,"total_tokens":25032}}}}"#,
            "\n",
        );
        let turns = parse_codex_tokens(content);
        assert_eq!(turns.len(), 2);
        // 2 件目: input_tokens はキャッシュを含む合計なので引いてある
        assert_eq!(turns[1].usage.input, 25032 - 8960);
        assert_eq!(turns[1].usage.cache_read, 8960);
        assert_eq!(turns[1].usage.cache_write, 0, "書き込み区分は出ない");
        assert_eq!(turns[1].model.as_deref(), Some("gpt-x"));
        // 足し上げが total と一致する (二重計上していない)
        let sum: u64 = turns
            .iter()
            .map(|t| t.usage.input + t.usage.cache_read)
            .sum();
        assert_eq!(sum, 44626);
    }

    // ── パーサ: 壊れた入力 ────────────────────────────────────────────

    /// 途中で切れた行があっても、その行だけを捨てて残りは読む。
    #[test]
    fn 途中で切れた行は捨てて残りを読む() {
        let content = concat!(
            r#"{"type":"assistant","message":{"model":"m","usage":{"input_tok"#, // ← 切れている
            "\n",
            r#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":10,"output_tokens":20}}}"#,
            "\n",
        );
        let turns = parse_claude_transcript(content);
        assert_eq!(turns.len(), 1, "壊れた 1 行のために全部を捨てない");
        assert_eq!(turns[0].usage.input, 10);
    }

    /// 非 JSON・空行・`usage` の無い行は静かに飛ばす (panic しない)。
    #[test]
    fn 不正な_json_や_usage_無しの行は飛ばす() {
        let content = concat!(
            "\n",
            "これは JSON ではない\n",
            "{}\n",
            "[]\n",
            "null\n",
            r#"{"usage":"文字列でも落ちない"}"#,
            "\n",
            r#"{"type":"assistant","message":{"usage":{}}}"#, // 中身が空 = 0 トークン
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant"}}"#, // usage 無し
            "\n",
            "   \n",
        );
        assert!(parse_claude_transcript(content).is_empty());
        assert!(parse_codex_tokens(content).is_empty());
    }

    /// 巨大な 1 行は上限で弾く (JSON パーサを噛ませて固まらせない)。
    #[test]
    fn 巨大な一行は上限で弾く() {
        let huge = format!(
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":1,"output_tokens":1}},"pad":"{}"}}}}"#,
            "あ".repeat(TOKEN_MAX_LINE) // 1 文字 3 バイト → 必ず上限超え
        );
        assert!(huge.len() > TOKEN_MAX_LINE);
        assert!(
            parse_claude_transcript(&huge).is_empty(),
            "上限超えの行は読まない"
        );
        // 上限のすぐ内側なら普通に読める (弾きすぎていない)
        let ok = r#"{"type":"assistant","message":{"usage":{"input_tokens":7,"output_tokens":8}}}"#;
        assert_eq!(parse_claude_transcript(ok).len(), 1);
    }

    /// 空文字列 (= 空ファイルの内容) は空の結果。
    #[test]
    fn 空の内容は空の結果になる() {
        assert!(parse_claude_transcript("").is_empty());
        assert!(parse_codex_tokens("").is_empty());
        assert!(parse_claude_transcript("\n\n\n").is_empty());
    }

    // ── 実ファイル読み: UTF-8 の割れ / 空ファイル / 無いディレクトリ ──

    /// tail 読みが UTF-8 の途中で割れても panic せず、壊れた先頭行を捨てる。
    #[test]
    fn utf8_の途中で割れても壊れない() {
        let dir = crate::test_util::unique_temp_dir("zv-quota", "utf8");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        // 3 バイト文字を並べた行 + 正しい JSON 行。tail をわざと
        // マルチバイトの途中から始まる位置で切る。
        let head = format!(
            r#"{{"type":"assistant","message":{{"usage":{{"input_tokens":1,"output_tokens":1}},"pad":"{}"}}}}"#,
            "あ".repeat(100)
        );
        let tail_line =
            r#"{"type":"assistant","message":{"usage":{"input_tokens":42,"output_tokens":9}}}"#;
        std::fs::write(&path, format!("{head}\n{tail_line}\n")).unwrap();
        let full = std::fs::metadata(&path).unwrap().len();
        for back in [tail_line.len() as u64 + 3, tail_line.len() as u64 + 4] {
            let content = read_tail(&path, back).expect("読めること");
            let turns = parse_claude_transcript(&content);
            assert_eq!(turns.len(), 1, "back={back} content={content:?}");
            assert_eq!(turns[0].usage.input, 42);
        }
        // 丸ごと読めば 2 行とも取れる
        let all = read_tail(&path, full).unwrap();
        assert_eq!(parse_claude_transcript(&all).len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空ファイルでも落ちず、集計は 0 件。
    #[test]
    fn 空ファイルは空として扱う() {
        let dir = crate::test_util::unique_temp_dir("zv-quota", "empty");
        let proj = dir.join(".claude").join("projects").join("p");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(proj.join("s.jsonl"), "").unwrap();
        let since = SystemTime::now() - Duration::from_secs(3600);
        assert!(
            scan_tokens_in(&dir, since).is_empty(),
            "消費ゼロなら 1 件も返さない"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 存在しないディレクトリでも落ちず、空を返す。
    #[test]
    fn 存在しないディレクトリでも落ちない() {
        // unique_temp_dir は実体を作るので、その中の**作っていない**子を使う
        let base = crate::test_util::unique_temp_dir("zv-quota", "missing");
        let missing = base.join("no-such-home");
        assert!(!missing.exists(), "わざと作らない");
        assert!(scan_tokens_in(&missing, UNIX_EPOCH).is_empty());
        let loc = FileLocator {
            base: &["nope"],
            file_prefix: "",
            file_ext: "jsonl",
            max_depth: 2,
            max_entries: 16,
            tail_bytes: 1024,
        };
        let (files, truncated) = recent_files(&missing, &loc, UNIX_EPOCH, 8);
        assert!(files.is_empty());
        assert!(!truncated);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 注入したホームから集計できる (実 `~/.claude` を触らない)。
    #[test]
    fn 注入したホームから集計できる() {
        let home = crate::test_util::unique_temp_dir("zv-quota", "scan");
        let proj = home.join(".claude").join("projects").join("-tmp-x");
        std::fs::create_dir_all(&proj).unwrap();
        std::fs::write(
            proj.join("s1.jsonl"),
            concat!(
                r#"{"type":"assistant","message":{"model":"mdl-a","usage":{"input_tokens":100,"output_tokens":10,"cache_creation_input_tokens":5,"cache_read_input_tokens":50}}}"#,
                "\n",
                r#"{"type":"assistant","message":{"model":"mdl-b","usage":{"input_tokens":1,"output_tokens":2}}}"#,
                "\n",
            ),
        )
        .unwrap();
        let since = SystemTime::now() - Duration::from_secs(3600);
        let got = scan_tokens_in(&home, since);
        assert_eq!(got.len(), 1, "claude だけが消費している");
        let a = &got[0];
        assert_eq!(a.agent, "claude");
        assert_eq!(a.turns, 2);
        assert_eq!(a.total.input, 101);
        assert_eq!(a.total.output, 12);
        assert_eq!(a.total.cache_write, 5);
        assert_eq!(a.total.cache_read, 50);
        // モデル別は消費の多い順
        assert_eq!(a.by_model[0].0, "mdl-a");
        assert_eq!(a.by_model[1].0, "mdl-b");
        let _ = std::fs::remove_dir_all(&home);
    }

    /// **本文は絶対に集計結果へ載らない。**
    #[test]
    fn 本文は集計結果に載らない() {
        let secret = "SECRET-PROMPT-BODY";
        let content = format!(
            r#"{{"type":"assistant","message":{{"model":"m","content":[{{"type":"text","text":"{secret}"}}],"usage":{{"input_tokens":1,"output_tokens":1}}}}}}"#
        );
        let turns = parse_claude_transcript(&content);
        assert_eq!(turns.len(), 1);
        let dump = format!("{turns:?}");
        assert!(!dump.contains(secret), "本文が混ざっている: {dump}");
    }

    // ── コスト計算 ────────────────────────────────────────────────────

    /// 0 トークンなら 0 円で、不明モデルにもならない。
    #[test]
    fn ゼロトークンはゼロ円() {
        let t = tokens("x", &[("mdl", TokenUsage::default())]);
        let est = estimate_cost(&t, &Prices::one("mdl", 5.0, 25.0));
        assert_eq!(est.amount, 0.0);
        assert!(est.is_complete());
        assert_eq!(est.unknown_tokens, 0);
    }

    /// キャッシュ読み / 書きは別の単価で計算される (同じ扱いにしない)。
    #[test]
    fn キャッシュの読み書きを単価で区別する() {
        let p = Prices::one("mdl", 10.0, 50.0); // 書き 12.5 / 読み 1.0
        let only_write = tokens(
            "x",
            &[(
                "mdl",
                TokenUsage {
                    cache_write: 1_000_000,
                    ..Default::default()
                },
            )],
        );
        let only_read = tokens(
            "x",
            &[(
                "mdl",
                TokenUsage {
                    cache_read: 1_000_000,
                    ..Default::default()
                },
            )],
        );
        let w = estimate_cost(&only_write, &p).amount;
        let r = estimate_cost(&only_read, &p).amount;
        assert!((w - 12.5).abs() < 1e-9, "書き込み {w}");
        assert!((r - 1.0).abs() < 1e-9, "読み出し {r}");
        assert!(w > r, "書き込みの方が高い");
        // 入力 100 万 = 10.0、出力 100 万 = 50.0
        let io = tokens(
            "x",
            &[(
                "mdl",
                TokenUsage {
                    input: 1_000_000,
                    output: 1_000_000,
                    ..Default::default()
                },
            )],
        );
        assert!((estimate_cost(&io, &p).amount - 60.0).abs() < 1e-9);
    }

    /// 単価が設定に無いモデルは **0 円にせず「不明」** として残す。
    #[test]
    fn 単価が無いモデルは不明として残る() {
        let t = tokens(
            "x",
            &[
                (
                    "known",
                    TokenUsage {
                        input: 1_000_000,
                        ..Default::default()
                    },
                ),
                (
                    "unknown-model",
                    TokenUsage {
                        input: 2_000_000,
                        ..Default::default()
                    },
                ),
            ],
        );
        let est = estimate_cost(&t, &Prices::one("known", 3.0, 15.0));
        assert!(!est.is_complete(), "不明があると complete ではない");
        assert_eq!(est.unknown_models, vec!["unknown-model".to_string()]);
        assert_eq!(est.unknown_tokens, 2_000_000);
        assert!((est.amount - 3.0).abs() < 1e-9, "既知ぶんだけ合計する");
        // 表示は「以上」と「単価未設定」を必ず含む (0 円だと嘘をつかない)
        let label = est.label("$");
        assert!(label.contains("以上"), "{label}");
        assert!(label.contains("unknown-model"), "{label}");

        // 全部不明なら「推定不可」
        let all_unknown = estimate_cost(&t, &Prices::empty());
        assert_eq!(all_unknown.amount, 0.0);
        let l2 = all_unknown.label("$");
        assert!(l2.contains("推定不可"), "{l2}");
        assert!(!l2.contains("$0.00"), "0 円と断言しない: {l2}");
    }

    /// モデル名が取れなかった分も「(モデル不明)」として数える。
    #[test]
    fn モデル名が取れない分も不明として数える() {
        let t = tokens(
            "x",
            &[(
                "",
                TokenUsage {
                    input: 10,
                    ..Default::default()
                },
            )],
        );
        let est = estimate_cost(&t, &Prices::one("m", 1.0, 1.0));
        assert_eq!(est.unknown_models.len(), 1);
        assert!(est.unknown_models[0].contains("不明"));
    }

    /// 桁あふれしない: `u64::MAX` を入れても有限の値になる。
    #[test]
    fn 桁あふれしない() {
        let big = TokenUsage {
            input: u64::MAX,
            output: u64::MAX,
            cache_write: u64::MAX,
            cache_read: u64::MAX,
        };
        // total は飽和して u64::MAX で止まる (0 に巻き戻らない)
        assert_eq!(big.total(), u64::MAX);
        let mut acc = big;
        acc.add(&big);
        assert_eq!(acc.input, u64::MAX, "加算も飽和する");
        let t = tokens("x", &[("mdl", big)]);
        let est = estimate_cost(&t, &Prices::one("mdl", 1000.0, 1000.0));
        assert!(est.amount.is_finite(), "{}", est.amount);
        assert!(est.amount > 0.0);
        assert!(!est.label("$").is_empty());
        assert!(!short_tokens(u64::MAX).is_empty());
    }

    /// 短縮表記は桁が上がっても幅が暴れない。
    #[test]
    fn 短縮表記の桁() {
        assert_eq!(short_tokens(0), "0");
        assert_eq!(short_tokens(999), "999");
        assert_eq!(short_tokens(1_000), "1.0k");
        assert_eq!(short_tokens(12_345), "12.3k");
        assert_eq!(short_tokens(1_500_000), "1.5M");
        assert_eq!(short_tokens(2_000_000_000), "2.0G");
        assert_eq!(short_tokens(u64::MAX), "18.4E");
        // 桁が上がっても幅が暴れない (バッジの幅計算が壊れない)
        for n in [0u64, 999, 1_000, 12_345, 1_000_000, 1 << 40, u64::MAX] {
            assert!(short_tokens(n).chars().count() <= 8, "{n}");
        }
    }

    // ── ステータスバーの配置 (純粋関数) ───────────────────────────────

    /// 極端なサイズでも矩形が可用領域に収まり、互いに重ならない。
    #[test]
    fn バッジの矩形は可用領域に収まり重ならない() {
        // 900x700 / 1200x300 / 400x700
        let sizes = [(900.0f32, 700.0f32), (1200.0, 300.0), (400.0, 700.0)];
        let item_sets: Vec<Vec<f32>> = vec![
            vec![120.0],
            vec![120.0, 140.0],
            vec![120.0, 140.0, 160.0, 180.0],
        ];
        for (w, h) in sizes {
            for items in &item_sets {
                for want_detail in [false, true] {
                    // ステータスバーはウィンドウ幅の一部しか使えない
                    let avail = (w * 0.4, TOKEN_ROW_TEST_H.min(h));
                    let lay = token_badge_layout(avail, items, 110.0, 16.0, 8.0, want_detail);
                    if !lay.visible {
                        assert!(lay.rects.is_empty(), "非表示なら矩形も無い");
                        continue;
                    }
                    for (i, (x, y, rw, rh)) in lay.rects.iter().enumerate() {
                        assert!(*x >= 0.0 && *y >= 0.0, "{i}: 原点より外 {:?}", lay.rects[i]);
                        assert!(
                            x + rw <= avail.0 + 0.001,
                            "{w}x{h} detail={want_detail} {i}: 右へ見切れる {} > {}",
                            x + rw,
                            avail.0
                        );
                        assert!(
                            y + rh <= avail.1 + 0.001,
                            "{w}x{h} {i}: 下へ見切れる {} > {}",
                            y + rh,
                            avail.1
                        );
                    }
                    for i in 0..lay.rects.len() {
                        for j in (i + 1)..lay.rects.len() {
                            let (ax, _, aw, _) = lay.rects[i];
                            let (bx, _, bw, _) = lay.rects[j];
                            let overlap = (ax + aw > bx) && (bx + bw > ax);
                            assert!(!overlap, "{i} と {j} が重なっている: {:?}", lay.rects);
                        }
                    }
                }
            }
        }
    }

    /// 幅が足りなければ詳細 → コンパクト → 非表示へ段階的に縮退する。
    #[test]
    fn 幅が足りなければ段階的に縮退する() {
        let items = vec![120.0, 140.0, 160.0];
        // 十分広い: 詳細のまま
        let wide = token_badge_layout((1000.0, 20.0), &items, 110.0, 16.0, 8.0, true);
        assert!(wide.visible && !wide.compact);
        assert_eq!(wide.rects.len(), 3);
        // 詳細は入らないがコンパクトは入る
        let mid = token_badge_layout((200.0, 20.0), &items, 110.0, 16.0, 8.0, true);
        assert!(mid.visible && mid.compact);
        assert_eq!(mid.rects.len(), 1);
        // どちらも入らない: 1px も出さない
        let narrow = token_badge_layout((40.0, 20.0), &items, 110.0, 16.0, 8.0, true);
        assert!(!narrow.visible);
        assert!(narrow.rects.is_empty());
        // 高さが足りなくても出さない
        let short = token_badge_layout((1000.0, 8.0), &items, 110.0, 16.0, 8.0, true);
        assert!(!short.visible);
    }

    /// 消費ゼロ (= 要素なし) なら、どんなに広くても 1px も出さない。
    #[test]
    fn 消費ゼロなら何も出さない() {
        for w in [0.0f32, 400.0, 1200.0, 4000.0] {
            let lay = token_badge_layout((w, 20.0), &[], 110.0, 16.0, 8.0, true);
            assert!(!lay.visible, "w={w}");
            assert!(lay.rects.is_empty());
        }
    }

    /// コンパクト指定なら、詳細が入る幅でも合算 1 個しか出さない。
    #[test]
    fn コンパクト指定なら合算だけを出す() {
        let items = vec![120.0, 140.0];
        let lay = token_badge_layout((4000.0, 20.0), &items, 110.0, 16.0, 8.0, false);
        assert!(lay.visible && lay.compact);
        assert_eq!(lay.rects.len(), 1);
        assert_eq!(lay.rects[0].2, 110.0);
    }

    /// 記述子表: トークンの在り処が調査結果どおりに入っている。
    #[test]
    fn トークンの記述子表が調査結果と合っている() {
        // 実測で取れたもの
        for bin in ["claude", "codex"] {
            assert!(
                matches!(
                    descriptor(bin).unwrap().tokens,
                    TokenSource::Transcript { .. }
                ),
                "{bin} はトランスクリプトから取れる"
            );
        }
        // 取れなかったもの (無いものを在ることにしない)
        for bin in ["gemini", "cursor-agent"] {
            assert!(
                matches!(descriptor(bin).unwrap().tokens, TokenSource::None),
                "{bin} は確認できていない"
            );
        }
    }

    // ── コスト上限とアラート ──────────────────────────────────────────

    /// 日付境界は **UTC の通し番号**で決める。
    /// ローカル時刻・ロケール・夏時間に一切依存しないことを固定する。
    #[test]
    fn 日付境界は_utc_の通し番号で決まる() {
        // 1970-01-01T00:00:00Z = 0 日目
        assert_eq!(utc_day_index(UNIX_EPOCH), 0);
        assert_eq!(utc_day_start(UNIX_EPOCH), UNIX_EPOCH);
        // 同じ日の 23:59:59 までは同じ番号
        let almost = UNIX_EPOCH + Duration::from_secs(DAY_SECS - 1);
        assert_eq!(utc_day_index(almost), 0);
        assert_eq!(utc_day_start(almost), UNIX_EPOCH);
        // 24 時間ちょうどで繰り上がる
        let next = UNIX_EPOCH + Duration::from_secs(DAY_SECS);
        assert_eq!(utc_day_index(next), 1);
        assert_eq!(utc_day_start(next), next);
        // epoch より前でも落ちない (0 に丸める)
        assert_eq!(utc_day_index(UNIX_EPOCH - Duration::from_secs(1)), 0);
        // 起点は必ずその日の 0 時ちょうど (端数が残らない)
        let odd = UNIX_EPOCH + Duration::from_secs(DAY_SECS * 20_000 + 12_345);
        assert_eq!(
            utc_day_start(odd),
            UNIX_EPOCH + Duration::from_secs(DAY_SECS * 20_000)
        );
        // 「今」で呼んでも境界はぴったり 0 時 (端数が残らない)
        assert_eq!(
            utc_day_start(SystemTime::now())
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_secs()
                % DAY_SECS,
            0
        );
    }

    /// 判定の中心。境界・0・負・NaN・∞・極端な値をまとめて固定する。
    #[test]
    fn 上限判定の決定表() {
        use BudgetState::{Normal, Over, Warn};
        // (消費, 上限, 警告割合, 期待, 説明)
        let table: &[(f64, f64, f32, BudgetState, &str)] = &[
            // 上限なし = 何をしても Normal
            (0.0, 0.0, 0.8, Normal, "上限 0 は無制限"),
            (
                1_000_000.0,
                0.0,
                0.8,
                Normal,
                "上限 0 はいくら使っても無制限",
            ),
            (100.0, -5.0, 0.8, Normal, "上限が負なら無制限"),
            (100.0, f64::NAN, 0.8, Normal, "上限が NaN なら無制限"),
            (100.0, f64::INFINITY, 0.8, Normal, "上限が ∞ なら無制限"),
            // 消費が数えられていない
            (f64::NAN, 50.0, 0.8, Normal, "消費が NaN なら咎めない"),
            // 通常域
            (0.0, 50.0, 0.8, Normal, "消費 0"),
            (-1.0, 50.0, 0.8, Normal, "消費が負でも Normal"),
            (39.99, 50.0, 0.8, Normal, "警告のすぐ手前"),
            // 警告のちょうど境界 (50 × 0.8 = 40)
            (40.0, 50.0, 0.8, Warn, "ちょうど警告割合は Warn"),
            (49.99, 50.0, 0.8, Warn, "上限の直前は Warn"),
            // 上限のちょうど境界
            (50.0, 50.0, 0.8, Over, "ちょうど上限は Over"),
            (50.01, 50.0, 0.8, Over, "超過は Over"),
            (1e18, 50.0, 0.8, Over, "極端に大きくても Over"),
            (f64::INFINITY, 50.0, 0.8, Over, "消費が ∞ なら Over"),
            // 警告割合の端
            (0.01, 50.0, 0.0, Warn, "割合 0 は消費した瞬間に Warn"),
            (0.0, 50.0, 0.0, Normal, "割合 0 でも消費 0 は Normal"),
            (49.99, 50.0, 1.0, Normal, "割合 1.0 は上限まで黙る"),
            (50.0, 50.0, 1.0, Over, "割合 1.0 でも上限は Over"),
            // 割合が範囲外でも 0.0..=1.0 へ丸める
            (0.01, 50.0, -3.0, Warn, "負の割合は 0 扱い"),
            (49.99, 50.0, 9.0, Normal, "1 超えの割合は 1 扱い"),
            // clamp は NaN をそのまま返すので、比較は必ず false = Normal
            (25.0, 50.0, f32::NAN, Normal, "割合が NaN なら警告しない"),
            // 極端に小さい上限
            (0.02, 0.01, 0.8, Over, "上限が 1 セントでも効く"),
            (0.009, 0.01, 0.8, Warn, "1 セント上限の 9 割は警告"),
            (0.005, 0.01, 0.8, Normal, "1 セント上限の半分はまだ通常"),
        ];
        for (spent, limit, ratio, want, why) in table {
            assert_eq!(
                budget_state(*spent, *limit, *ratio),
                *want,
                "{why}: spent={spent} limit={limit} ratio={ratio}"
            );
        }
    }

    /// 深刻さは Normal < Warn < Over の順に並ぶ (`max` で最悪を取れる)。
    #[test]
    fn 上限の深刻さは順序を持つ() {
        use BudgetState::{Normal, Over, Warn};
        assert!(Normal < Warn && Warn < Over);
        assert_eq!([Normal, Over, Warn].into_iter().max(), Some(Over));
    }

    /// 上限が未設定なら結果は空 = 画面に 1px も出さない。
    #[test]
    fn 上限が未設定なら判定結果は空() {
        let l = CostLimits::default();
        assert!(!l.any());
        assert!(l.evaluate(999.0, 999.0).is_empty());
        assert_eq!(l.worst(999.0, 999.0), None);
        assert_eq!(l.blocks(999.0, 999.0), None);
        assert_eq!(budget_edge_key(None), "");
    }

    /// 設定した上限だけが判定に載り、深刻な順に並ぶ。
    #[test]
    fn 設定した上限だけが深刻な順に並ぶ() {
        let l = CostLimits {
            session: 10.0,
            daily: 100.0,
            warn_ratio: 0.8,
            action: LimitAction::Notify,
        };
        assert!(l.any());
        // セッションだけ超過
        let got = l.evaluate(12.0, 5.0);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].kind, BudgetKind::Session);
        assert_eq!(got[0].state, BudgetState::Over);
        assert_eq!(got[1].state, BudgetState::Normal);
        assert_eq!(l.worst(12.0, 5.0).unwrap().kind, BudgetKind::Session);
        // 同率なら日次が先 (期間の長いほうを重く見る)
        let both = CostLimits {
            session: 10.0,
            daily: 10.0,
            ..l
        };
        assert_eq!(both.worst(20.0, 20.0).unwrap().kind, BudgetKind::Daily);
        // 片方だけ設定したら 1 件だけ
        let only_daily = CostLimits { session: 0.0, ..l };
        let got = only_daily.evaluate(9999.0, 1.0);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, BudgetKind::Daily);
    }

    /// `stop` を選んだときだけ、しかも `Over` のときだけ止める。
    #[test]
    fn 送信を止めるのは_stop_かつ超過のときだけ() {
        let notify = CostLimits {
            session: 10.0,
            daily: 0.0,
            warn_ratio: 0.8,
            action: LimitAction::Notify,
        };
        // 既定は notify = 何があっても止めない
        assert_eq!(notify.blocks(1_000.0, 0.0), None);
        let stop = CostLimits {
            action: LimitAction::Stop,
            ..notify
        };
        assert_eq!(stop.blocks(8.0, 0.0), None, "Warn では止めない");
        let b = stop.blocks(10.0, 0.0).expect("ちょうど上限で止まる");
        assert_eq!(b.kind, BudgetKind::Session);
        assert_eq!(b.state, BudgetState::Over);
        // 未設定の上限は stop でも止めない
        let none = CostLimits {
            session: 0.0,
            daily: 0.0,
            ..stop
        };
        assert_eq!(none.blocks(1e9, 1e9), None);
    }

    /// 設定文字列 → 動作。**未知の値は既定 (notify)** — 打ち間違いで止めない。
    #[test]
    fn 上限到達時の動作は文字列から引ける() {
        assert_eq!(LimitAction::from_key("stop"), LimitAction::Stop);
        assert_eq!(LimitAction::from_key("  STOP "), LimitAction::Stop);
        assert_eq!(LimitAction::from_key("notify"), LimitAction::Notify);
        assert_eq!(LimitAction::from_key(""), LimitAction::Notify);
        assert_eq!(LimitAction::from_key("halt"), LimitAction::Notify);
        assert_eq!(LimitAction::default(), LimitAction::Notify);
        // 往復する
        for a in [LimitAction::Notify, LimitAction::Stop] {
            assert_eq!(LimitAction::from_key(a.key()), a);
        }
    }

    /// 同じ状態のあいだ鍵は変わらない = 毎フレーム鳴らない。
    #[test]
    fn 上限通知の鍵は状態が変わったときだけ動く() {
        let l = CostLimits {
            session: 10.0,
            daily: 0.0,
            warn_ratio: 0.8,
            action: LimitAction::Notify,
        };
        let key = |spent: f64| budget_edge_key(l.worst(spent, 0.0).as_ref());
        assert_eq!(key(1.0), "", "Normal は鳴らさない");
        let warn = key(8.0);
        assert!(!warn.is_empty());
        assert_eq!(warn, key(9.0), "Warn のあいだは同じ鍵");
        let over = key(10.0);
        assert!(!over.is_empty());
        assert_ne!(warn, over, "段が上がったら鍵が変わる");
        assert_eq!(over, key(999.0), "Over のあいだは同じ鍵");
        // 実際に EdgeGate が 1 度しか通さないこと
        let mut gate = crate::notify::EdgeGate::default();
        assert!(gate.changed(0, &warn));
        assert!(!gate.changed(0, &warn));
        assert!(gate.changed(0, &over));
        assert!(!gate.changed(0, &over));
    }

    /// 表示用の値。通貨記号は引数から来る (コードに埋めない)。
    #[test]
    fn 上限の表示ラベルは通貨記号を引数から取る() {
        let s = BudgetStatus {
            kind: BudgetKind::Daily,
            state: BudgetState::Warn,
            spent: 12.3456,
            limit: 50.0,
        };
        assert_eq!(s.short_label("$"), "$12.35 / $50.00");
        assert_eq!(s.short_label("¥"), "¥12.35 / ¥50.00");
        assert!((s.fraction() - 0.246_912).abs() < 1e-6);
        // 上限 0 は fraction を 0 にする (0 除算を作らない)
        let zero = BudgetStatus {
            limit: 0.0,
            ..s.clone()
        };
        assert_eq!(zero.fraction(), 0.0);
        // 負の消費でも割合は 0 未満にならない
        let neg = BudgetStatus { spent: -5.0, ..s };
        assert_eq!(neg.fraction(), 0.0);
        assert!(!BudgetKind::Session.label().is_empty());
        assert!(!BudgetKind::Daily.label().is_empty());
    }

    /// 複数の起点を **1 パス**で数え分けられる (同じファイルを何度も読まない)。
    #[test]
    fn 複数の起点を一度の走査で数え分ける() {
        let home = crate::test_util::unique_temp_dir("zv-quota", "multi");
        let proj = home.join(".claude").join("projects").join("-tmp-multi");
        std::fs::create_dir_all(&proj).unwrap();
        let line = |ts: &str, out: u64| {
            format!(
                r#"{{"type":"assistant","timestamp":"{ts}","message":{{"model":"m","usage":{{"input_tokens":0,"output_tokens":{out}}}}}}}"#
            )
        };
        // 時刻は固定値 (ローカル時刻・実行日に依存させない)
        std::fs::write(
            proj.join("s.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                line("2020-01-01T00:00:00Z", 100),
                line("2020-01-02T00:00:00Z", 20),
                line("2020-01-03T00:00:00Z", 3),
            ),
        )
        .unwrap();
        let at = |s: &str| parse_rfc3339(s).expect("固定の RFC3339");
        let sinces = [
            at("2019-12-31T00:00:00Z"), // 全部
            at("2020-01-02T00:00:00Z"), // 後ろ 2 本 (境界はその時刻を含む)
            at("2020-01-03T00:00:00Z"), // 最後の 1 本
        ];
        let got = scan_tokens_multi_in(&home, &sinces);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0][0].total.output, 123, "全部");
        assert_eq!(got[1][0].total.output, 23, "後ろ 2 本");
        assert_eq!(got[2][0].total.output, 3, "最後の 1 本");
        // 1 本だけ渡した場合は従来の scan_tokens_in と一致する
        let one = scan_tokens_multi_in(&home, &sinces[..1]);
        assert_eq!(one[0], scan_tokens_in(&home, sinces[0]));
        // 起点ゼロ本は空 (ファイルも読まない)
        assert!(scan_tokens_multi_in(&home, &[]).is_empty());
        // 誰も消費していない窓は 1 件も返さない (= 1px も出さない)
        let future = scan_tokens_multi_in(&home, &[at("2099-01-01T00:00:00Z")]);
        assert_eq!(future.len(), 1);
        assert!(future[0].is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }
}
