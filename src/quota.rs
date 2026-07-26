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

use crate::i18n::trf;

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
    },
    AgentQuota {
        bin: "gemini",
        label: "Gemini CLI",
        account: "google",
        source: QuotaSource::ObservedOnly,
    },
    AgentQuota {
        bin: "cursor-agent",
        label: "Cursor Agent",
        account: "cursor",
        source: QuotaSource::ObservedOnly,
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
        s.observed_events = events.iter().filter(|e| e.agent == s.agent).cloned().collect();
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
            .filter(|e| now.duration_since(e.at).map(|d| d <= window).unwrap_or(true))
            .count()
    }
}

/// スナップショットをアカウント単位へ畳む (純関数)。
///
/// `running_by_agent` は「bin 名 → いま走っている本数」。
pub fn aggregate(snaps: &[QuotaSnapshot], running_by_agent: &[(String, usize)]) -> Vec<AccountUsage> {
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
        self.per_account.get(account).map(|v| v.as_slice()).unwrap_or(&[])
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
            AdviceReason::AlreadyLimited { agent } => {
                trf("{agent} が使用上限に当たっています", &[("agent", agent.clone())])
            }
            AdviceReason::HighUsage { percent } => {
                trf("プラン枠を {percent}% 使っています", &[("percent", percent.to_string())])
            }
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
                &[("running", running.to_string()), ("percent", percent.to_string())],
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
pub fn advise(usage: &AccountUsage, running_agents: usize, policy: &Policy, now: SystemTime) -> Advice {
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
                reason: AdviceReason::HighUsage {
                    percent: pct(u),
                },
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
        assert_eq!(u.resets_at, Some(parse_rfc3339("2026-07-25T00:10:00Z").unwrap()));
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
        assert_eq!(parse_rfc3339("2026-07-25T03:36:12.345Z"), parse_rfc3339("2026-07-25T03:36:12Z"));
        // +09:00 は UTC より 9 時間進んでいる → エポックは 9 時間手前
        let jst = parse_rfc3339("2026-07-25T09:00:00+09:00").unwrap();
        assert_eq!(jst, parse_rfc3339("2026-07-25T00:00:00Z").unwrap());
        for bad in ["", "abc", "2026-07-25", "2026/07/25T00:00:00Z", "2026-13-01T00:00:00Z"] {
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
        assert!(burn_rate(&samples(&[(900, 0.5)]), w, now).is_none(), "標本 1 つ");
        assert!(
            burn_rate(&samples(&[(900, 0.5), (900, 0.7)]), w, now).is_none(),
            "時間幅ゼロ"
        );
        assert!(
            burn_rate(&samples(&[(0, 0.1), (10, 0.2)]), Duration::from_secs(60), now).is_none(),
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
        let s = samples(&[(200, 0.80), (400, 0.90), (600, 0.05), (800, 0.15), (1000, 0.25)]);
        let r = burn_rate(&s, Duration::from_secs(3600), now).unwrap();
        assert_eq!(r.samples, 3, "リセット後の 3 点だけ");
        assert!((r.per_sec - 0.0005).abs() < 1e-6, "0.2 / 400s");
    }

    /// 減っている (=燃えていない) 場合は速度 0。
    #[test]
    fn burn_rate_flat_is_zero() {
        let now = t(1_000);
        let r = burn_rate(&samples(&[(400, 0.5), (700, 0.5), (1000, 0.5)]), Duration::from_secs(3600), now)
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
        assert_eq!(h.samples("openai").len(), BurnHistory::CAP, "同時刻は上書き");
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
        let mut u = aggregate(&[snap("codex", "openai", Some(0.7), SourceKind::Vendor)], &[])
            .pop()
            .unwrap();
        attach_projection(&mut u, Some(&burn(0.001)), t(0));
        assert_eq!(u.projection, Projection::Exhaustion(Duration::from_secs(300)));
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
            advise(&account(Some(0.10), Projection::NotBurning, vec![]), 1, &p, now),
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
            advise(&account(Some(0.96), Projection::NotBurning, vec![]), 1, &p, now),
            Advice::Stop {
                reason: AdviceReason::HighUsage { percent: 96 }
            }
        ));
        // 予測枯渇が 5 分以内 → 止める
        assert!(matches!(
            advise(
                &account(Some(0.30), Projection::Exhaustion(Duration::from_secs(120)), vec![]),
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
                &account(Some(0.30), Projection::Exhaustion(Duration::from_secs(1200)), vec![]),
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
            advise(&account(Some(0.85), Projection::NotBurning, vec![]), 1, &p, now),
            Advice::SlowDown {
                reason: AdviceReason::HighUsage { percent: 85 }
            }
        ));
        // 少し前の上限警告が積み上がっている (窓の外だが 4 倍窓の中)
        assert!(matches!(
            advise(
                &account(None, Projection::InsufficientData, vec![ev(98_000), ev(98_500)]),
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
            advise(&account(Some(0.60), Projection::NotBurning, vec![]), 3, &p, now),
            Advice::SlowDown {
                reason: AdviceReason::Crowded {
                    running: 3,
                    percent: 60
                }
            }
        ));
        // 情報が無ければ黙る (推測で騒がない)
        assert_eq!(
            advise(&account(None, Projection::InsufficientData, vec![]), 5, &p, now),
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
