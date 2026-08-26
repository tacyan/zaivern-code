//! トークン見積もりと、最適化の効き目を残す軽量な台帳。
//!
//! ## なぜ「見積もり」なのか
//!
//! 正確なトークン数はモデルごとの tokenizer にしか出せず、それを積むと
//! **どのモデルの数字なのか**という問いが増える (Claude / GPT / Gemini で
//! 違う)。ここが要るのは「渡す前に減らせたか」を測ることだけなので、
//! モデル非依存の近似 (ascii/4 + 非 ascii) で足りる。誤差 ±20% を
//! 名前と出力の両方で明示して、正確な値だと誤解させない。
//!
//! ## 台帳の大きさは構造で有界にする
//!
//! 1 回ごとの記録を追記すると際限なく育つので、**日ごとの合計だけ**を持つ。
//! 1 年で 365 行にしかならないので、丸ごと読んで丸ごと書ける
//! (「巨大な Analytics 基盤を作らない」という要件そのもの)。
//!
//! 日付は **UTC の epoch 日番号**で持つ。暦へ直さないのは、暦へ直した
//! 瞬間に「どのタイムゾーンの今日か」という別の問題を抱え込むため
//! (表示側が必要なら暦へ直せばよく、記録の側は連番のままでよい)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// ASCII 何文字で 1 トークンと見るか。GPT / Claude / Gemini の
/// tokenizer はいずれも英文でこの辺りに落ち着く。
const ASCII_PER_TOKEN: f64 = 4.0;

/// モデル非依存のトークン見積もり (誤差 ±20%)。
///
/// 非 ASCII は 1 文字 1 トークンとして数える。CJK はどの tokenizer でも
/// おおむねそうなるので、日本語のファイルを「小さい」と誤って判断しない。
pub fn estimate_tokens(s: &str) -> usize {
    let mut ascii = 0usize;
    let mut other = 0usize;
    for c in s.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            other += 1;
        }
    }
    (ascii as f64 / ASCII_PER_TOKEN + other as f64).ceil() as usize
}

/// 削減率 (%)。**増えたときと元が空のときは 0.0** を返す。
///
/// 「増えた」を負の削減率として出すと、合計したときに他の削減と打ち消し合って
/// 台帳が嘘をつく。増減は `original` と `optimized` の生値が持っているので、
/// 率のほうは 0 で床を作る。
pub fn reduction_percent(original: usize, optimized: usize) -> f32 {
    if original == 0 || optimized >= original {
        return 0.0;
    }
    ((original - optimized) as f64 / original as f64 * 100.0) as f32
}

/// `max` トークン相当へ畳む。先頭 70% ・末尾 20% を残し、間に印を入れる。
///
/// 返すのは `(本文, 畳んだか)`。**途中を落としたことを本文に書く**のが要点で、
/// 黙って切ると受け取った側が「これで全部だ」と読んでしまう。
pub fn truncate_tokens(text: &str, max: usize) -> (String, bool) {
    if max == 0 || estimate_tokens(text) <= max {
        return (text.to_string(), false);
    }
    let lines: Vec<&str> = text.lines().collect();
    let head_budget = max * 7 / 10;
    let tail_budget = max * 2 / 10;

    let mut head_end = 0usize;
    let mut used = 0usize;
    for (i, l) in lines.iter().enumerate() {
        let t = estimate_tokens(l) + 1;
        if used + t > head_budget {
            break;
        }
        used += t;
        head_end = i + 1;
    }

    let mut tail_start = lines.len();
    used = 0;
    for (i, l) in lines.iter().enumerate().rev() {
        if i < head_end {
            break;
        }
        let t = estimate_tokens(l) + 1;
        if used + t > tail_budget {
            break;
        }
        used += t;
        tail_start = i;
    }

    let snipped = tail_start.saturating_sub(head_end);
    if snipped == 0 {
        return (text.to_string(), false);
    }
    let snipped_tok: usize = lines[head_end..tail_start]
        .iter()
        .map(|l| estimate_tokens(l) + 1)
        .sum();
    let mut out = String::new();
    out.push_str(&lines[..head_end].join("\n"));
    out.push_str(&format!(
        "\n… [context: {snipped} lines / ~{snipped_tok} tok snipped — narrow with offset/limit or grep] …\n"
    ));
    out.push_str(&lines[tail_start..].join("\n"));
    (out, true)
}

// ── 何をした 1 回なのか ───────────────────────────────────────────

/// Context Engine が行った操作の種類。**メトリクスの分類軸**であって、
/// 分岐の材料ではない (どの操作でも扱いは同じ)。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum ContextOperation {
    Read,
    Search,
    Refs,
    Directory,
    Json,
    Text,
    Count,
}

impl ContextOperation {
    /// 台帳と出力に載る安定 ID。**訳さない** (機械が読む値なので)。
    pub fn id(self) -> &'static str {
        match self {
            ContextOperation::Read => "read",
            ContextOperation::Search => "search",
            ContextOperation::Refs => "refs",
            ContextOperation::Directory => "directory",
            ContextOperation::Json => "json",
            ContextOperation::Text => "text",
            ContextOperation::Count => "count",
        }
    }

    /// 安定 ID からの逆引き。台帳を読み戻すときに使う。
    pub fn from_id(s: &str) -> Option<Self> {
        Some(match s {
            "read" => ContextOperation::Read,
            "search" => ContextOperation::Search,
            "refs" => ContextOperation::Refs,
            "directory" => ContextOperation::Directory,
            "json" => ContextOperation::Json,
            "text" => ContextOperation::Text,
            "count" => ContextOperation::Count,
            _ => return None,
        })
    }

    /// 台帳の集計で回す全種。
    pub const ALL: [ContextOperation; 7] = [
        ContextOperation::Read,
        ContextOperation::Search,
        ContextOperation::Refs,
        ContextOperation::Directory,
        ContextOperation::Json,
        ContextOperation::Text,
        ContextOperation::Count,
    ];
}

/// この 1 回が誰のためのものだったか。**全て任意の不透明な文字列**。
///
/// ここが Provider 非依存の要になっている: エンジンはこの値を**分類のラベル
/// としてしか触らない**。`agent` に何が入っていても処理が変わってはいけない
/// (`engine::tests::出自は挙動を変えない` と
///  `mod tests::コアはエージェント名を知らない` が番人)。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct ContextOrigin {
    /// どのエージェント向けか (`"claude"` 等)。分岐には**使わない**。
    pub agent: Option<String>,
    /// セッション ID。
    pub session: Option<String>,
    /// タスク ID。
    pub task: Option<String>,
}

impl ContextOrigin {
    /// 何も分かっていない出自。
    pub fn unknown() -> Self {
        Self::default()
    }
}

/// 1 回の最適化の結果。
#[derive(Clone, Debug)]
pub struct ContextMetrics {
    pub operation: ContextOperation,
    pub original_tokens: usize,
    pub optimized_tokens: usize,
    pub origin: ContextOrigin,
    /// かかった時間 (ms)。エンジン自身がボトルネックになっていないかを見る。
    pub elapsed_ms: u64,
}

impl ContextMetrics {
    /// 減らせたトークン数。**増えたときは 0** ([`reduction_percent`] と同じ床)。
    pub fn saved_tokens(&self) -> usize {
        self.original_tokens.saturating_sub(self.optimized_tokens)
    }

    /// 削減率 (%)。
    pub fn reduction_percent(&self) -> f32 {
        reduction_percent(self.original_tokens, self.optimized_tokens)
    }
}

// ── 日ごとの合計 ─────────────────────────────────────────────────

/// UTC の 1 日ぶんの合計。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct DayTotals {
    pub operations: u64,
    pub original_tokens: u64,
    pub optimized_tokens: u64,
}

impl DayTotals {
    fn add(&mut self, m: &ContextMetrics) {
        self.operations += 1;
        self.original_tokens += m.original_tokens as u64;
        self.optimized_tokens += m.optimized_tokens as u64;
    }

    /// 減らせたトークン数。
    pub fn saved_tokens(&self) -> u64 {
        self.original_tokens.saturating_sub(self.optimized_tokens)
    }

    /// 削減率 (%)。
    pub fn reduction_percent(&self) -> f32 {
        if self.original_tokens == 0 || self.optimized_tokens >= self.original_tokens {
            return 0.0;
        }
        ((self.original_tokens - self.optimized_tokens) as f64 / self.original_tokens as f64
            * 100.0) as f32
    }
}

/// 1970-01-01 からの日数 (UTC)。時計が壊れていても panic しない。
pub fn epoch_day_at(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() / 86_400)
        .unwrap_or(0)
}

/// 今日 (UTC) の日番号。
pub fn today() -> u64 {
    epoch_day_at(std::time::SystemTime::now())
}

/// 台帳。**日ごとの合計と、操作ごとの内訳だけ**を持つ。
///
/// 1 回ごとの明細は持たない — 明細を持った瞬間に「いつ消すか」を決めねば
/// ならず、それは軽量な構造では答えられない問いになる。
#[derive(Clone, Default, Debug)]
pub struct Ledger {
    days: BTreeMap<u64, DayTotals>,
    ops: BTreeMap<&'static str, DayTotals>,
    /// 出自のエージェント別の合計。**これが [`ContextOrigin::agent`] の行き先。**
    ///
    /// session / task まで別々に積むと際限が無いので積まない
    /// (1 回ごとの値は [`ContextMetrics`] が持っていて、呼び出し側が
    ///  受け取れる — 溜めるかどうかはその層の判断)。
    agents: BTreeMap<String, DayTotals>,
}

/// 台帳に残す最大日数。これを超えた古い日は落とす (ファイルを有界にする)。
const MAX_DAYS: usize = 400;

/// 台帳に残すエージェント名の最大数。**名前は外から来る文字列**なので、
/// 上限を置かないとファイルが際限なく育つ。
const MAX_AGENTS: usize = 32;

impl Ledger {
    /// 1 回ぶんを積む。
    pub fn record(&mut self, day: u64, m: &ContextMetrics) {
        self.days.entry(day).or_default().add(m);
        self.ops.entry(m.operation.id()).or_default().add(m);
        if let Some(agent) = m
            .origin
            .agent
            .as_deref()
            .map(str::trim)
            .filter(|a| !a.is_empty())
        {
            // 既にある名前は常に積む。新しい名前は上限まで
            // (溢れた分を捨てるのは、名前の数が外からいくらでも増やせるため)。
            if self.agents.contains_key(agent) || self.agents.len() < MAX_AGENTS {
                self.agents.entry(agent.to_string()).or_default().add(m);
            }
        }
        while self.days.len() > MAX_DAYS {
            let Some(oldest) = self.days.keys().next().copied() else {
                break;
            };
            self.days.remove(&oldest);
        }
    }

    /// ある日の合計 (無い日は 0)。
    pub fn day(&self, day: u64) -> DayTotals {
        self.days.get(&day).copied().unwrap_or_default()
    }

    /// 全期間の合計。
    pub fn total(&self) -> DayTotals {
        let mut t = DayTotals::default();
        for d in self.days.values() {
            t.operations += d.operations;
            t.original_tokens += d.original_tokens;
            t.optimized_tokens += d.optimized_tokens;
        }
        t
    }

    /// 操作ごとの内訳 (件数のある種類だけ、[`ContextOperation::ALL`] の順)。
    pub fn by_operation(&self) -> Vec<(ContextOperation, DayTotals)> {
        ContextOperation::ALL
            .iter()
            .filter_map(|op| {
                self.ops
                    .get(op.id())
                    .filter(|t| t.operations > 0)
                    .map(|t| (*op, *t))
            })
            .collect()
    }

    /// 出自のエージェント別の内訳 (名前順)。
    pub fn by_agent(&self) -> Vec<(&str, DayTotals)> {
        self.agents.iter().map(|(k, v)| (k.as_str(), *v)).collect()
    }

    /// 記録の入っている日数。
    pub fn days_recorded(&self) -> usize {
        self.days.len()
    }

    /// JSON へ写す。
    pub fn to_json(&self) -> serde_json::Value {
        let days: serde_json::Map<String, serde_json::Value> = self
            .days
            .iter()
            .map(|(k, v)| (k.to_string(), totals_json(v)))
            .collect();
        let ops: serde_json::Map<String, serde_json::Value> = self
            .ops
            .iter()
            .map(|(k, v)| ((*k).to_string(), totals_json(v)))
            .collect();
        let agents: serde_json::Map<String, serde_json::Value> = self
            .agents
            .iter()
            .map(|(k, v)| (k.clone(), totals_json(v)))
            .collect();
        serde_json::json!({ "version": 1, "days": days, "operations": ops, "agents": agents })
    }

    /// JSON から読む。壊れていたら**空の台帳**を返す (panic しない)。
    ///
    /// 台帳は統計であって真実の在り処ではないので、読めないときに
    /// 機能ごと止める理由が無い。
    pub fn from_json(v: &serde_json::Value) -> Self {
        let mut out = Ledger::default();
        if let Some(days) = v.get("days").and_then(|d| d.as_object()) {
            for (k, t) in days {
                if let (Ok(day), Some(tot)) = (k.parse::<u64>(), totals_from_json(t)) {
                    out.days.insert(day, tot);
                }
            }
        }
        if let Some(ops) = v.get("operations").and_then(|d| d.as_object()) {
            for (k, t) in ops {
                if let (Some(op), Some(tot)) = (ContextOperation::from_id(k), totals_from_json(t)) {
                    out.ops.insert(op.id(), tot);
                }
            }
        }
        if let Some(agents) = v.get("agents").and_then(|d| d.as_object()) {
            for (k, t) in agents.iter().take(MAX_AGENTS) {
                if let Some(tot) = totals_from_json(t) {
                    out.agents.insert(k.clone(), tot);
                }
            }
        }
        out
    }

    /// ファイルから読む。無い / 壊れているなら空の台帳。
    pub fn load(path: &Path) -> Self {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ledger::default();
        };
        match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(v) => Ledger::from_json(&v),
            Err(_) => Ledger::default(),
        }
    }

    /// ファイルへ書く。**同じディレクトリの一時ファイル経由で差し替える**
    /// (途中で落ちても半端な台帳を残さない)。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let tmp = path.with_extension(format!("tmp{}", std::process::id()));
        let body = serde_json::to_string(&self.to_json()).map_err(|e| e.to_string())?;
        std::fs::write(&tmp, body).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{}: {e}", path.display())
        })
    }
}

fn totals_json(t: &DayTotals) -> serde_json::Value {
    serde_json::json!({
        "operations": t.operations,
        "original_tokens": t.original_tokens,
        "optimized_tokens": t.optimized_tokens,
        "saved_tokens": t.saved_tokens(),
        "reduction_percent": (t.reduction_percent() * 10.0).round() / 10.0,
    })
}

fn totals_from_json(v: &serde_json::Value) -> Option<DayTotals> {
    Some(DayTotals {
        operations: v.get("operations")?.as_u64()?,
        original_tokens: v.get("original_tokens")?.as_u64()?,
        optimized_tokens: v.get("optimized_tokens")?.as_u64()?,
    })
}

// ── プロセスに 1 つの台帳 ────────────────────────────────────────

fn shared() -> &'static Mutex<Ledger> {
    static L: OnceLock<Mutex<Ledger>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(Ledger::default()))
}

/// プロセス内の台帳へ 1 回ぶん積む。**永続化はしない** (呼び出し側の判断)。
pub fn record(m: &ContextMetrics) {
    if let Ok(mut l) = shared().lock() {
        l.record(today(), m);
    }
}

/// プロセス内の台帳の写し。
pub fn snapshot() -> Ledger {
    shared().lock().map(|l| l.clone()).unwrap_or_default()
}

/// 永続化する台帳の置き場を、与えられた `~/.zaivern` 相当から導く。
///
/// **`config::zaivern_dir()` をここから呼ばない** — このモジュールを
/// 将来 core crate へ切り出すときに、唯一残る crate 依存になるため。
/// 置き場を決めるのは呼び出し側 (アダプタ) の仕事。
pub fn store_path(zaivern_dir: &Path) -> PathBuf {
    zaivern_dir.join("context").join("metrics.json")
}

/// 保存済みの台帳へ 1 回ぶんを足して書き戻す。
///
/// 読み込み → 加算 → 書き出しを 1 回で行う。同時に走った別インスタンスの
/// 記録は**取りこぼしうる** (最後に書いた側が勝つ)。統計なので
/// fail-open にしてあり、失敗しても呼び出し側の処理は続く。
pub fn persist(path: &Path, m: &ContextMetrics) -> Result<(), String> {
    let mut l = Ledger::load(path);
    l.record(today(), m);
    l.save(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(op: ContextOperation, orig: usize, opt: usize) -> ContextMetrics {
        ContextMetrics {
            operation: op,
            original_tokens: orig,
            optimized_tokens: opt,
            origin: ContextOrigin::unknown(),
            elapsed_ms: 0,
        }
    }

    fn from(agent: &str, orig: usize, opt: usize) -> ContextMetrics {
        ContextMetrics {
            origin: ContextOrigin {
                agent: Some(agent.to_string()),
                session: Some("s".into()),
                task: Some("t".into()),
            },
            ..m(ContextOperation::Read, orig, opt)
        }
    }

    /// 出自のエージェント名で積み分けられ、**名前の数は上限で有界**。
    #[test]
    fn 出自のエージェント別に積む() {
        let mut l = Ledger::default();
        l.record(1, &from("claude", 100, 10));
        l.record(1, &from("codex", 200, 50));
        l.record(1, &from("claude", 100, 10));
        l.record(1, &m(ContextOperation::Read, 50, 50));
        let by = l.by_agent();
        assert_eq!(by.len(), 2, "出自不明まで数えている");
        assert_eq!(by[0].0, "claude");
        assert_eq!(by[0].1.operations, 2);
        assert_eq!(by[0].1.saved_tokens(), 180);
        // 空白だけの名前は無いのと同じ
        l.record(1, &from("   ", 10, 1));
        assert_eq!(l.by_agent().len(), 2);
        // 名前は外から来るので上限で止める
        for i in 0..MAX_AGENTS + 20 {
            l.record(1, &from(&format!("agent-{i}"), 10, 1));
        }
        assert_eq!(l.by_agent().len(), MAX_AGENTS);
        // 既にある名前は上限を超えても積み続ける
        let before = l
            .by_agent()
            .iter()
            .find(|(k, _)| *k == "claude")
            .unwrap()
            .1
            .operations;
        l.record(1, &from("claude", 10, 1));
        let after = l
            .by_agent()
            .iter()
            .find(|(k, _)| *k == "claude")
            .unwrap()
            .1
            .operations;
        assert_eq!(after, before + 1);
        // JSON を往復しても残る
        assert_eq!(Ledger::from_json(&l.to_json()).by_agent().len(), MAX_AGENTS);
    }

    #[test]
    fn ascii_and_cjk_estimates() {
        assert_eq!(estimate_tokens(&"a".repeat(40)), 10);
        assert_eq!(estimate_tokens("日本語のテキストです"), 10);
        assert_eq!(estimate_tokens(""), 0);
    }

    /// 削減率は「増えた」「元が空」で必ず 0。負の率は合計で他を打ち消す。
    #[test]
    fn 削減率は増えたときも空のときもゼロ() {
        assert_eq!(reduction_percent(0, 0), 0.0);
        assert_eq!(reduction_percent(0, 10), 0.0);
        assert_eq!(reduction_percent(100, 100), 0.0);
        assert_eq!(reduction_percent(100, 120), 0.0);
        assert!((reduction_percent(100, 25) - 75.0).abs() < 0.01);
        assert_eq!(m(ContextOperation::Read, 100, 120).saved_tokens(), 0);
    }

    #[test]
    fn truncate_keeps_head_and_tail_and_says_so() {
        let text = (0..500)
            .map(|i| format!("line number {i} with some padding text"))
            .collect::<Vec<_>>()
            .join("\n");
        let (out, cut) = truncate_tokens(&text, 300);
        assert!(cut);
        assert!(out.contains("line number 0 "));
        assert!(out.contains("line number 499"));
        assert!(out.contains("snipped"), "落としたことが本文に出ていない");
        assert!(estimate_tokens(&out) < estimate_tokens(&text));
        let (same, cut) = truncate_tokens("hello\nworld", 100);
        assert!(!cut);
        assert_eq!(same, "hello\nworld");
        // 上限 0 は「上限なし」として扱う (0 トークンへ畳んでも意味が無い)
        assert!(!truncate_tokens("hello", 0).1);
    }

    #[test]
    fn 台帳は日ごとと操作ごとに積む() {
        let mut l = Ledger::default();
        l.record(100, &m(ContextOperation::Read, 1000, 200));
        l.record(100, &m(ContextOperation::Search, 500, 100));
        l.record(101, &m(ContextOperation::Read, 200, 200));
        assert_eq!(l.day(100).operations, 2);
        assert_eq!(l.day(100).saved_tokens(), 1200);
        assert_eq!(l.day(999).operations, 0, "無い日は 0");
        assert_eq!(l.total().operations, 3);
        assert_eq!(l.total().original_tokens, 1700);
        let by = l.by_operation();
        assert_eq!(by.len(), 2, "件数 0 の種類は出さない");
        assert_eq!(by[0].0, ContextOperation::Read);
    }

    /// 古い日は落として、ファイルを構造的に有界にする。
    #[test]
    fn 台帳は古い日を落として有界に保つ() {
        let mut l = Ledger::default();
        for d in 0..(MAX_DAYS as u64 + 50) {
            l.record(d, &m(ContextOperation::Read, 10, 1));
        }
        assert_eq!(l.days_recorded(), MAX_DAYS);
        assert_eq!(l.day(0).operations, 0, "いちばん古い日が残っている");
        assert_eq!(l.day(MAX_DAYS as u64 + 49).operations, 1);
    }

    #[test]
    fn 台帳はjsonを往復する() {
        let mut l = Ledger::default();
        l.record(7, &m(ContextOperation::Json, 900, 90));
        let back = Ledger::from_json(&l.to_json());
        assert_eq!(back.day(7), l.day(7));
        assert_eq!(back.by_operation(), l.by_operation());
        // 壊れた入力で panic せず、空を返す
        assert_eq!(
            Ledger::from_json(&serde_json::json!({"days": "壊れている"})).days_recorded(),
            0
        );
        assert_eq!(
            Ledger::from_json(&serde_json::json!(null)).days_recorded(),
            0
        );
    }

    #[test]
    fn 台帳はファイルを往復する() {
        let dir = crate::test_util::unique_temp_dir("zaivern-ctx", "ledger");
        let path = store_path(&dir);
        // 無いファイルは空の台帳 (エラーにしない)
        assert_eq!(Ledger::load(&path).days_recorded(), 0);
        persist(&path, &m(ContextOperation::Text, 400, 100)).expect("書けること");
        persist(&path, &m(ContextOperation::Text, 400, 100)).expect("書けること");
        let l = Ledger::load(&path);
        assert_eq!(l.total().operations, 2);
        assert_eq!(l.total().saved_tokens(), 600);
        // 壊れたファイルでも panic しない
        std::fs::write(&path, "{ これは JSON ではない").unwrap();
        assert_eq!(Ledger::load(&path).days_recorded(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 日番号は時計が壊れていても落ちない() {
        assert_eq!(
            epoch_day_at(std::time::UNIX_EPOCH - std::time::Duration::from_secs(10)),
            0
        );
        assert_eq!(
            epoch_day_at(std::time::UNIX_EPOCH + std::time::Duration::from_secs(86_400 * 3 + 5)),
            3
        );
    }
}
