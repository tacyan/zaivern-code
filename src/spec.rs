//! spec 駆動開発 — **差分 (delta) で書き、コードとの乖離を git で見張る**。
//!
//! ## なぜこの形なのか (競合の不満点を 3 つとも潰す)
//!
//! 同種のツール (Spec Kit / OpenSpec / BMAD / Kiro) に対して繰り返し出る
//! 不満は 3 つで、どれも「文書を作る機能」を足しても消えない:
//!
//! 1. **仕様が腐る。** ベンダー自身が未解決と認めている
//!    (Kiro のチームメンバー曰く「仕様はほぼ静的な文書で、コードだけ
//!    書き換わっても仕様は更新されない」)。→ [`assess`] が
//!    **統べているファイルが動いたのに要件の文が動いていない**ことを
//!    git から見つけて「陳腐化の疑い」を出す。**このモジュールの本体はここ。**
//! 2. **一段ギアしかない。** 1 行の修正に 35 タスク・3 フェーズが生成される。
//!    → [`Gear`] の 2 段。軽量パスは `deltas/<能力>.md` **1 枚だけ**を作る。
//! 3. **レビュー負荷。** 「方針を変えるたびに文書が丸ごと再生成され、
//!    変わった箇所だけの差分が出てこない」。→ 変更を **delta で書く** ので、
//!    レビュー対象がそのまま差分になる ([`Delta`])。
//!
//! ## 置き場所 (どの環境でも同じ導出。絶対パスは 1 つも書かない)
//!
//! ```text
//! <ワークスペース>/spec/            ← spec ルート ([`spec_root`])
//!   specs/<能力>/spec.md            ← 唯一の真実 (source of truth)
//!   changes/<変更 ID>/
//!     proposal.toml                 ← ギアと表題 (**コードが書く**)
//!     deltas/<能力>.md              ← 差分。ADDED / MODIFIED / REMOVED
//!   changes/archive/<変更 ID>/      ← 統合済み
//!   state.toml                      ← 決定的なタスク/要件の状態 (**コードが書く**)
//! ```
//!
//! `openspec/` が既にあればそちらを spec ルートとして読む
//! (OpenSpec 利用者がそのまま乗れるように)。
//!
//! ## 守っている約束
//!
//! - **描画スレッドで git を待たない。** 走査は裏のスレッドで、UI は
//!   *いま手元にある値* を描く ([`SpecPanel::poll`])。間隔は
//!   [`crate::git::scan_interval`] で適応的に空ける。
//! - **アイドルのコストはゼロ。** 走査するのは**パネルを出している間だけ**。
//! - **狼少年をやらない。** 空白だけ・コメントだけの変更、ファイルが移動した
//!   だけ、は乖離ではない ([`meaningful_line`] / [`resolve_missing`])。
//!   判定できないときは黙る (`Unknown`) — 疑いを水増ししない。
//! - **state.toml はコードが書く。** LLM に書かせない。書き込みは検証 →
//!   一時ファイル → rename の順で、途中で失敗したら**元のまま**残る
//!   ([`write_state_atomic`])。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use eframe::egui::{self, RichText};
use serde::{Deserialize, Serialize};

use crate::i18n::{tr, trf};
use crate::panels::space;
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// 置き場所 (パスは全部ここから導出する。区切り文字は書かない)
// ---------------------------------------------------------------------------

/// spec ルートの候補。**先に見つかったものを使う。**
/// 1 つも無ければ先頭 (`spec`) を「これから作る場所」として返す。
pub const SPEC_DIR_CANDIDATES: [&str; 2] = ["spec", "openspec"];

const SPECS_DIR: &str = "specs";
const CHANGES_DIR: &str = "changes";
const ARCHIVE_DIR: &str = "archive";
const DELTAS_DIR: &str = "deltas";
const SPEC_FILE: &str = "spec.md";
const PROPOSAL_FILE: &str = "proposal.toml";
const STATE_FILE: &str = "state.toml";

/// `state.toml` / `proposal.toml` の形式版。上げたら [`migrate_state`] に段を足す。
pub const STATE_VERSION: u32 = 1;

/// 走査の下限間隔。実際の間隔は直近の所要時間の 4 倍まで自動で伸びる。
const SCAN_BASE: Duration = Duration::from_secs(4);

/// spec ルートを決める (純関数に近い — 存在確認だけする)。
pub fn spec_root(workspace: &Path) -> PathBuf {
    for name in SPEC_DIR_CANDIDATES {
        let p = workspace.join(name);
        if p.is_dir() {
            return p;
        }
    }
    workspace.join(SPEC_DIR_CANDIDATES[0])
}

// ---------------------------------------------------------------------------
// 型 — 能力 (capability) と要件 (requirement)
// ---------------------------------------------------------------------------

/// 1 つのシナリオ (`#### Scenario: …` と `- GIVEN/WHEN/THEN` の並び)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scenario {
    pub title: String,
    pub steps: Vec<String>,
}

/// 1 つの要件。RFC-2119 の MUST / SHOULD / MAY を本文に書く。
///
/// `targets` / `tests` が **この要件が統べているもの** で、
/// 陳腐化の判定はここに書かれた範囲だけを見る。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Requirement {
    pub name: String,
    pub text: String,
    pub scenarios: Vec<Scenario>,
    /// `[@code] <パス or glob>`
    pub targets: Vec<String>,
    /// `[@test] <パス or glob>`
    pub tests: Vec<String>,
}

impl Requirement {
    /// 統べている対象を 1 本にまとめる (コードとテストを区別しない)。
    pub fn governs(&self) -> Vec<String> {
        let mut v = self.targets.clone();
        v.extend(self.tests.iter().cloned());
        v
    }

    /// 要件の文の指紋。**空白の揺れは無視する** — 折り返しを直しただけで
    /// 「人が仕様を直した」と誤認しないため。
    pub fn fingerprint(&self) -> String {
        let mut buf = String::new();
        buf.push_str(&normalize_text(&self.name));
        buf.push('\n');
        buf.push_str(&normalize_text(&self.text));
        for s in &self.scenarios {
            buf.push('\n');
            buf.push_str(&normalize_text(&s.title));
            for st in &s.steps {
                buf.push('\n');
                buf.push_str(&normalize_text(st));
            }
        }
        format!("{:016x}", fnv1a64(buf.as_bytes()))
    }
}

/// 1 つの能力 = `specs/<名前>/spec.md` 1 枚。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capability {
    pub name: String,
    pub path: PathBuf,
    /// frontmatter の `targets:` — 能力全体が統べる範囲。
    pub targets: Vec<String>,
    pub requirements: Vec<Requirement>,
}

impl Capability {
    fn index_of(&self, name: &str) -> Option<usize> {
        self.requirements.iter().position(|r| r.name == name)
    }
}

/// 差分の動詞。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verb {
    Added,
    Modified,
    Removed,
}

impl Verb {
    /// 見出しに書く語 (`## ADDED Requirements`)。
    pub fn keyword(self) -> &'static str {
        match self {
            Verb::Added => "ADDED",
            Verb::Modified => "MODIFIED",
            Verb::Removed => "REMOVED",
        }
    }

    /// 画面に出す記号 + 語。
    pub fn chip(self) -> &'static str {
        match self {
            Verb::Added => "＋ ADDED",
            Verb::Modified => "～ MODIFIED",
            Verb::Removed => "－ REMOVED",
        }
    }
}

/// 1 つの能力に対する差分 1 枚 (`changes/<ID>/deltas/<能力>.md`)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    pub added: Vec<Requirement>,
    pub modified: Vec<Requirement>,
    /// 消す要件の名前だけ。
    pub removed: Vec<String>,
    /// 解釈できなかった `## …` 見出し。**黙って捨てない** (画面に出す)。
    pub unknown: Vec<String>,
}

impl Delta {
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }

    /// 触っている要件の総数。
    pub fn touched(&self) -> usize {
        self.added.len() + self.modified.len() + self.removed.len()
    }
}

// ---------------------------------------------------------------------------
// 解析 (純関数) — Markdown → 型
// ---------------------------------------------------------------------------

/// 空白の揺れを潰す (行頭行末を落とし、空行を捨て、`\n` で繋ぐ)。
fn normalize_text(s: &str) -> String {
    s.replace("\r\n", "\n")
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// FNV-1a 64bit。**自前で持つ理由**: `DefaultHasher` は Rust の版で値が
/// 変わり得る。この指紋は `state.toml` に書いて別のマシンと突き合わせるので、
/// 未来永劫同じ値でなければならない。
fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

/// `#` の数と見出し文字列。見出しでなければ `None`。
fn heading(line: &str) -> Option<(usize, &str)> {
    let t = line.trim_end_matches('\r');
    let n = t.chars().take_while(|c| *c == '#').count();
    if n == 0 || n > 6 {
        return None;
    }
    let rest = &t[n..];
    if !rest.starts_with(' ') {
        return None;
    }
    Some((n, rest.trim()))
}

/// 指定の深さの見出しで本文を割る (純関数)。返り値は `(前置き, [(見出し, 本文)])`。
///
/// **より浅い見出しは区切りにしない** — 呼び出し側が段を絞って使うため。
fn split_sections(text: &str, level: usize) -> (String, Vec<(String, String)>) {
    let mut preamble = String::new();
    let mut out: Vec<(String, String)> = Vec::new();
    let mut cur: Option<(String, String)> = None;
    for line in text.replace("\r\n", "\n").lines() {
        match heading(line) {
            Some((n, title)) if n == level => {
                if let Some(sec) = cur.take() {
                    out.push(sec);
                }
                cur = Some((title.to_string(), String::new()));
            }
            _ => {
                let sink = match cur.as_mut() {
                    Some((_, body)) => body,
                    None => &mut preamble,
                };
                sink.push_str(line);
                sink.push('\n');
            }
        }
    }
    if let Some(sec) = cur.take() {
        out.push(sec);
    }
    (preamble, out)
}

/// `[@code] path` / `[@test] path` を 1 行から取り出す (純関数)。
///
/// `path::symbol` のような後置きは**パスの部分だけ**を返す
/// (`[@test] src/foo.rs::tests::bar` → `src/foo.rs`)。
fn link_line(line: &str) -> Option<(String, String)> {
    let t = line.trim();
    let rest = t.strip_prefix("[@")?;
    let (tag, rest) = rest.split_once(']')?;
    let value = rest.trim_start_matches(':').trim();
    if value.is_empty() {
        return None;
    }
    Some((tag.trim().to_ascii_lowercase(), value.to_string()))
}

/// リンクの値からパス部分だけを取り出す (純関数)。
pub fn target_path(t: &str) -> &str {
    t.split("::").next().unwrap_or(t).trim()
}

/// 要件 1 つ分の本文を解析する (純関数)。
fn parse_requirement(name: &str, block: &str) -> Requirement {
    let (body, scns) = split_sections(block, 4);
    let mut req = Requirement {
        name: name.to_string(),
        ..Requirement::default()
    };
    let mut text_lines: Vec<&str> = Vec::new();
    for line in body.lines() {
        match link_line(line) {
            Some((tag, value)) if tag == "code" || tag == "target" => req.targets.push(value),
            Some((tag, value)) if tag == "test" => req.tests.push(value),
            // 未知のタグは本文として残す (黙って落とさない)
            _ => text_lines.push(line),
        }
    }
    req.text = normalize_text(&text_lines.join("\n"));
    for (title, sbody) in scns {
        let title = title
            .strip_prefix("Scenario:")
            .map(str::trim)
            .unwrap_or(title.as_str())
            .to_string();
        let steps: Vec<String> = sbody
            .lines()
            .map(str::trim)
            .filter_map(|l| l.strip_prefix("- ").or_else(|| l.strip_prefix("* ")))
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect();
        req.scenarios.push(Scenario { title, steps });
    }
    req
}

/// `### Requirement: <名前>` を全部拾う (純関数)。深さ 3 で固定。
fn parse_requirements(text: &str) -> Vec<Requirement> {
    let (_, secs) = split_sections(text, 3);
    secs.into_iter()
        .filter_map(|(title, body)| {
            let name = title.strip_prefix("Requirement:")?.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some(parse_requirement(&name, &body))
        })
        .collect()
}

/// frontmatter の `targets:` を取り出し、本文を返す (純関数)。
///
/// `targets: a, b` と YAML のリスト (`- a`) の両方を受ける。閉じていない /
/// 無い場合は「frontmatter は無かった」ことにして**全文を本文**にする。
fn split_targets_front_matter(text: &str) -> (Vec<String>, String) {
    let src = text
        .strip_prefix('\u{feff}')
        .unwrap_or(text)
        .replace("\r\n", "\n");
    let mut lines = src.lines();
    if lines.next().map(str::trim) != Some("---") {
        return (Vec::new(), src);
    }
    let mut fm: Vec<&str> = Vec::new();
    let mut closed = false;
    let mut consumed = 1usize;
    for line in lines {
        consumed += 1;
        let t = line.trim();
        if t == "---" || t == "..." {
            closed = true;
            break;
        }
        if fm.len() >= 200 {
            break;
        }
        fm.push(line);
    }
    if !closed {
        return (Vec::new(), src);
    }
    let mut targets: Vec<String> = Vec::new();
    let mut in_list = false;
    for line in &fm {
        let t = line.trim();
        if in_list {
            if let Some(v) = t.strip_prefix("- ") {
                push_targets(&mut targets, v);
                continue;
            }
            in_list = false;
        }
        let Some((k, v)) = t.split_once(':') else {
            continue;
        };
        if !k.trim().eq_ignore_ascii_case("targets") {
            continue;
        }
        if v.trim().is_empty() {
            in_list = true;
        } else {
            push_targets(&mut targets, v);
        }
    }
    let body: String = src
        .split('\n')
        .skip(consumed)
        .collect::<Vec<_>>()
        .join("\n");
    (targets, body)
}

/// `a, b c` をカンマ / 空白で割って積む。引用符は 1 組だけ外す。
fn push_targets(out: &mut Vec<String>, raw: &str) {
    for tok in raw.split([',', ' ', '\t']) {
        let t = tok.trim().trim_matches(['"', '\'']);
        if !t.is_empty() && t != "[" && t != "]" {
            out.push(t.trim_matches(['[', ']']).to_string());
        }
    }
}

/// 能力 1 枚を解析する (純関数)。
pub fn parse_capability(name: &str, path: PathBuf, text: &str) -> Capability {
    let (targets, body) = split_targets_front_matter(text);
    Capability {
        name: name.to_string(),
        path,
        targets,
        requirements: parse_requirements(&body),
    }
}

/// 差分 1 枚を解析する (純関数)。
///
/// 見出しは `## ADDED Requirements` の形。大文字小文字は問わず、
/// `Requirements` の有無も問わない。解釈できない `##` は [`Delta::unknown`] へ。
pub fn parse_delta(text: &str) -> Delta {
    let (_, targets) = split_targets_front_matter(text);
    let (_, secs) = split_sections(&targets, 2);
    let mut d = Delta::default();
    for (title, body) in secs {
        let verb = title
            .split_whitespace()
            .next()
            .map(|w| w.trim_end_matches(':').to_ascii_uppercase());
        match verb.as_deref() {
            Some("ADDED") => d.added.extend(parse_requirements(&body)),
            Some("MODIFIED") | Some("CHANGED") => d.modified.extend(parse_requirements(&body)),
            Some("REMOVED") | Some("DELETED") => {
                d.removed
                    .extend(parse_requirements(&body).into_iter().map(|r| r.name));
            }
            _ => d.unknown.push(title),
        }
    }
    d
}

// ---------------------------------------------------------------------------
// 適用 (純関数) — delta を真実へ畳む
// ---------------------------------------------------------------------------

/// [`apply_delta`] の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ApplyReport {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub removed: Vec<String>,
    /// 当てられなかったもの (既にある名前の ADDED / 無い名前の MODIFIED・REMOVED)。
    /// **黙って上書き・黙って無視をしない**ので、ここが空でなければ人が読む。
    pub conflicts: Vec<String>,
    /// 最後の 1 件を消したので、この能力は引退する。
    pub retired: bool,
}

impl ApplyReport {
    pub fn is_clean(&self) -> bool {
        self.conflicts.is_empty()
    }
}

/// delta を能力へ当てる (純関数)。ADDED は追記・MODIFIED は置換・REMOVED は削除。
pub fn apply_delta(cap: &mut Capability, d: &Delta) -> ApplyReport {
    let mut rep = ApplyReport::default();
    for r in &d.added {
        if cap.index_of(&r.name).is_some() {
            rep.conflicts
                .push(format!("ADDED {}: 同じ名前の要件が既にある", r.name));
            continue;
        }
        cap.requirements.push(r.clone());
        rep.added.push(r.name.clone());
    }
    for r in &d.modified {
        match cap.index_of(&r.name) {
            Some(i) => {
                cap.requirements[i] = r.clone();
                rep.modified.push(r.name.clone());
            }
            None => rep
                .conflicts
                .push(format!("MODIFIED {}: 元の要件が無い", r.name)),
        }
    }
    for name in &d.removed {
        match cap.index_of(name) {
            Some(i) => {
                cap.requirements.remove(i);
                rep.removed.push(name.clone());
            }
            None => rep
                .conflicts
                .push(format!("REMOVED {name}: 元の要件が無い")),
        }
    }
    rep.retired = cap.requirements.is_empty();
    rep
}

/// 能力を Markdown へ書き戻す (純関数)。[`parse_capability`] と往復する。
pub fn render_capability(cap: &Capability) -> String {
    let mut out = String::new();
    if !cap.targets.is_empty() {
        out.push_str("---\n");
        out.push_str(&format!("targets: {}\n", cap.targets.join(", ")));
        out.push_str("---\n\n");
    }
    out.push_str(&format!("# {}\n\n## Requirements\n", cap.name));
    for r in &cap.requirements {
        out.push_str(&format!("\n### Requirement: {}\n", r.name));
        if !r.text.is_empty() {
            out.push_str(&r.text);
            out.push('\n');
        }
        for t in &r.targets {
            out.push_str(&format!("[@code] {t}\n"));
        }
        for t in &r.tests {
            out.push_str(&format!("[@test] {t}\n"));
        }
        for s in &r.scenarios {
            out.push_str(&format!("\n#### Scenario: {}\n", s.title));
            for st in &s.steps {
                out.push_str(&format!("- {st}\n"));
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 2 段ギア — 小さな変更に大袈裟な儀式をさせない
// ---------------------------------------------------------------------------

/// 変更の進め方。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Gear {
    /// **軽量パス** — 差分 1 枚だけ。要件/設計/タスクの三部作を作らない。
    #[default]
    Light,
    /// **完全パス** — 差分 + 設計メモ + タスク表。
    Full,
}

impl Gear {
    pub fn label(self) -> String {
        match self {
            Gear::Light => tr("軽量"),
            Gear::Full => tr("完全"),
        }
    }
}

/// 変更の大きさ。ギアはこれだけで決まる (純関数の入力)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Footprint {
    /// 触る要件の数
    pub reqs: usize,
    /// シナリオの総数
    pub scenarios: usize,
    /// REMOVED の数 (後方互換を壊すので必ず重い)
    pub removes: usize,
    /// 統べているファイルの数
    pub files: usize,
}

/// 軽量パスで通す上限。**1 要件・2 シナリオ・3 ファイルまで。**
/// これを超えると人が差分を一目で読めなくなる。
const LIGHT_MAX_REQS: usize = 1;
const LIGHT_MAX_SCENARIOS: usize = 2;
const LIGHT_MAX_FILES: usize = 3;

/// 差分の大きさを測る (純関数)。
pub fn footprint(d: &Delta) -> Footprint {
    let mut f = Footprint {
        reqs: d.touched(),
        removes: d.removed.len(),
        ..Footprint::default()
    };
    let mut files: Vec<String> = Vec::new();
    for r in d.added.iter().chain(d.modified.iter()) {
        f.scenarios += r.scenarios.len();
        for t in r.governs() {
            let p = target_path(&t).to_string();
            if !files.contains(&p) {
                files.push(p);
            }
        }
    }
    f.files = files.len();
    f
}

/// ギアの推奨 (純関数)。**既定は軽量**で、超えたときだけ完全へ倒す。
pub fn suggest_gear(f: Footprint) -> Gear {
    if f.removes > 0
        || f.reqs > LIGHT_MAX_REQS
        || f.scenarios > LIGHT_MAX_SCENARIOS
        || f.files > LIGHT_MAX_FILES
    {
        Gear::Full
    } else {
        Gear::Light
    }
}

// ---------------------------------------------------------------------------
// 陳腐化の判定 (純関数) — **狼少年をやらない**
// ---------------------------------------------------------------------------

/// 統べているファイル 1 枚の変化。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FileDrift {
    pub path: String,
    /// **意味のある**変更行の数 (空白だけ・コメントだけは数えない)。
    pub meaningful: usize,
    /// 移動しただけなら移動元。
    pub moved_from: Option<String>,
}

/// 拡張子から行コメントの前置きを引く。
///
/// **判らない拡張子には空を返す。** 判らない言語のコメントを剥がそうとすると
/// 本物のコードを落としかねないので、そこは諦めて空白判定だけに任せる。
pub fn comment_prefixes(path: &str) -> &'static [&'static str] {
    let ext = Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "rs" | "go" | "js" | "jsx" | "ts" | "tsx" | "c" | "h" | "cc" | "cpp" | "hpp" | "cs"
        | "java" | "kt" | "kts" | "swift" | "scala" | "dart" | "php" | "zig" | "proto" => &["//"],
        "py" | "rb" | "sh" | "bash" | "zsh" | "fish" | "pl" | "r" | "toml" | "yaml" | "yml"
        | "cfg" | "conf" | "ini" | "tf" | "dockerfile" | "mk" | "nix" | "ex" | "exs" => &["#"],
        "sql" | "lua" | "hs" | "elm" | "adb" | "ads" => &["--"],
        "el" | "clj" | "cljs" | "lisp" | "scm" => &[";"],
        _ => &[],
    }
}

/// この 1 行は「意味のある変更」か (純関数)。
///
/// * 空白だけ → 違う
/// * 行コメントだけ → 違う
/// * C 系のブロックコメントの中身らしい行 (`/*` `*` `*/` 始まり) → 違う
///
/// 判らない言語ではコメントを剥がさない。**過小報告に倒す**方針だが、
/// ここだけは剥がしすぎるほうが危ないので保守的にする。
pub fn meaningful_line(path: &str, text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return false;
    }
    let pre = comment_prefixes(path);
    if pre.iter().any(|p| t.starts_with(p)) {
        return false;
    }
    if pre.contains(&"//") && (t.starts_with("/*") || t.starts_with('*') || t.starts_with("*/")) {
        return false;
    }
    true
}

/// unified diff の 1 ファイル分から乖離を測る (純関数)。
pub fn file_drift(fd: &crate::diff::FileDiff) -> FileDrift {
    let path = if fd.new_path.is_empty() || fd.new_path == "/dev/null" {
        fd.old_path.clone()
    } else {
        fd.new_path.clone()
    };
    let mut meaningful = 0usize;
    for h in &fd.hunks {
        for l in &h.lines {
            if l.kind == crate::diff::LineKind::Context {
                continue;
            }
            if meaningful_line(&path, &l.text) {
                meaningful += 1;
            }
        }
    }
    FileDrift {
        moved_from: (fd.is_rename && fd.old_path != fd.new_path).then(|| fd.old_path.clone()),
        path,
        meaningful,
    }
}

/// `git diff` の出力を丸ごと乖離の一覧へ (純関数)。
pub fn drifts_of(diff_text: &str) -> Vec<FileDrift> {
    crate::diff::parse_unified(diff_text)
        .iter()
        .map(file_drift)
        .collect()
}

/// リンク切れの判定 (純関数)。**移動しただけのファイルは切れていない。**
pub fn resolve_missing(missing: &[String], drifts: &[FileDrift]) -> Vec<String> {
    missing
        .iter()
        .filter(|m| {
            !drifts
                .iter()
                .any(|d| d.moved_from.as_deref() == Some(m.as_str()))
        })
        .cloned()
        .collect()
}

/// 要件 1 つの状態。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum Staleness {
    /// 判定材料が無い (基準となるコミットを記録していない / git が無い)。
    /// **疑いには数えない。**
    #[default]
    Unknown,
    /// 追いついている。
    InSync,
    /// 基準の後で**仕様のほうが動いている** = 人が既に手当てした、と見なす。
    SpecAhead,
    /// **陳腐化の疑い** — 統べているコードが動いたのに要件の文が動いていない。
    Suspect { files: Vec<String>, lines: usize },
    /// リンク切れ — 統べる先が無い。
    Dangling { missing: Vec<String> },
}

impl Staleness {
    /// 疑いとして数えるか (タブのバッジはこれだけ数える)。
    pub fn is_suspect(&self) -> bool {
        matches!(self, Staleness::Suspect { .. } | Staleness::Dangling { .. })
    }

    /// `state.toml` に書く語彙。
    pub fn key(&self) -> &'static str {
        match self {
            Staleness::Unknown => "unknown",
            Staleness::InSync => "in-sync",
            Staleness::SpecAhead => "spec-ahead",
            Staleness::Suspect { .. } => "suspect",
            Staleness::Dangling { .. } => "dangling",
        }
    }

    /// 画面に出す記号 + 語。
    pub fn chip(&self) -> String {
        match self {
            Staleness::Unknown => tr("－ 未判定"),
            Staleness::InSync => tr("✔ 同期"),
            Staleness::SpecAhead => tr("✎ 仕様が先行"),
            Staleness::Suspect { .. } => tr("⚠ 陳腐化の疑い"),
            Staleness::Dangling { .. } => tr("🔗 リンク切れ"),
        }
    }

    fn color(&self, theme: &Theme) -> egui::Color32 {
        match self {
            Staleness::Unknown => theme.text_dim,
            Staleness::InSync => theme.ok,
            Staleness::SpecAhead => theme.accent,
            Staleness::Suspect { .. } => theme.warn,
            Staleness::Dangling { .. } => theme.err,
        }
    }

    /// 根拠を 1 行で (ホバーに出す)。
    pub fn detail(&self) -> String {
        match self {
            Staleness::Suspect { files, lines } => trf(
                "{n} 行の実質変更: {files}",
                &[("n", lines.to_string()), ("files", files.join(", "))],
            ),
            Staleness::Dangling { missing } => {
                trf("見つからない対象: {m}", &[("m", missing.join(", "))])
            }
            _ => String::new(),
        }
    }
}

/// **陳腐化の判定 (純関数)。** ここが競合に無い部分なので、規則を明文化する。
///
/// | 入力 | 出す答え | 理由 |
/// |---|---|---|
/// | 統べる先が無い (移動でもない) | `Dangling` | 推測ではなく事実。直せる |
/// | 実質変更が 0 行 | `InSync` | 空白/コメント/移動だけは乖離ではない |
/// | 基準の後で要件の文が動いた | `SpecAhead` | 人が既に触っている。**疑わない** |
/// | それ以外 | `Suspect` | コードだけ動いた |
///
/// 迷ったら黙る (過小報告)。「エラーの部分一致で稼働中のエージェントを
/// 異常と判定した」のと同じ失敗を繰り返さないため。
pub fn assess(spec_changed: bool, drifts: &[FileDrift], missing: &[String]) -> Staleness {
    let missing = resolve_missing(missing, drifts);
    if !missing.is_empty() {
        return Staleness::Dangling { missing };
    }
    let hits: Vec<&FileDrift> = drifts.iter().filter(|d| d.meaningful > 0).collect();
    if hits.is_empty() {
        return Staleness::InSync;
    }
    if spec_changed {
        return Staleness::SpecAhead;
    }
    Staleness::Suspect {
        lines: hits.iter().map(|d| d.meaningful).sum(),
        files: hits.iter().map(|d| d.path.clone()).collect(),
    }
}

// ---------------------------------------------------------------------------
// 状態ファイル (state.toml) — **コードが書く。LLM に書かせない**
// ---------------------------------------------------------------------------

/// 要件 1 件の記録。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReqState {
    pub capability: String,
    pub name: String,
    /// 最後に突き合わせた時点の [`Requirement::fingerprint`]。
    #[serde(default)]
    pub fingerprint: String,
    /// 最後に突き合わせた時点の HEAD (空なら未判定)。
    #[serde(default)]
    pub baseline: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub targets: Vec<String>,
}

/// タスク 1 件の記録。文脈が飛んでもここを読めば続きから戻れる。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskState {
    pub change: String,
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub status: String,
}

/// `state.toml` 全体。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    #[serde(default)]
    pub version: u32,
    #[serde(default, rename = "requirement")]
    pub requirements: Vec<ReqState>,
    #[serde(default, rename = "task")]
    pub tasks: Vec<TaskState>,
}

/// 要件の状態として許す語彙。
pub const REQ_STATUSES: [&str; 5] = ["unknown", "in-sync", "spec-ahead", "suspect", "dangling"];
/// タスクの状態として許す語彙。
pub const TASK_STATUSES: [&str; 4] = ["todo", "doing", "done", "blocked"];

impl State {
    fn find(&self, cap: &str, name: &str) -> Option<&ReqState> {
        self.requirements
            .iter()
            .find(|r| r.capability == cap && r.name == name)
    }
}

/// 語彙と必須項目の検証 (純関数)。**書く前に必ず通す。**
pub fn validate_state(st: &State) -> Result<(), Vec<String>> {
    let mut bad: Vec<String> = Vec::new();
    if st.version == 0 || st.version > STATE_VERSION {
        bad.push(format!("version={} が範囲外", st.version));
    }
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for r in &st.requirements {
        if r.capability.is_empty() || r.name.is_empty() {
            bad.push(format!("要件の識別子が空: {:?}/{:?}", r.capability, r.name));
        }
        if !REQ_STATUSES.contains(&r.status.as_str()) {
            bad.push(format!(
                "{}/{}: status={:?} は語彙外",
                r.capability, r.name, r.status
            ));
        }
        let key = (r.capability.as_str(), r.name.as_str());
        if seen.contains(&key) {
            bad.push(format!("{}/{} が二重に載っている", r.capability, r.name));
        }
        seen.push(key);
    }
    for t in &st.tasks {
        if t.change.is_empty() || t.id.is_empty() {
            bad.push(format!("タスクの識別子が空: {:?}/{:?}", t.change, t.id));
        }
        if !TASK_STATUSES.contains(&t.status.as_str()) {
            bad.push(format!(
                "{}#{}: status={:?} は語彙外",
                t.change, t.id, t.status
            ));
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad)
    }
}

/// 形式版の引き上げ (純関数)。
///
/// * `version` が無い (= 0) → 版が付く前の書式。そのまま読んで今の版を打つ。
/// * 未来の版 → **読まない**。古いアプリが新しい書式を潰さないため。
pub fn migrate_state(raw: &str) -> Result<State, String> {
    let mut st: State = toml::from_str(raw).map_err(|e| e.to_string())?;
    if st.version > STATE_VERSION {
        return Err(trf(
            "state.toml の形式版 {v} はこのアプリより新しい (対応は {c} まで)",
            &[
                ("v", st.version.to_string()),
                ("c", STATE_VERSION.to_string()),
            ],
        ));
    }
    if st.version == 0 {
        st.version = STATE_VERSION;
        for r in &mut st.requirements {
            if r.status.is_empty() {
                r.status = "unknown".into();
            }
        }
        for t in &mut st.tasks {
            if t.status.is_empty() {
                t.status = "todo".into();
            }
        }
    }
    Ok(st)
}

/// `--dry-run` に相当する突き合わせ結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DriftReport {
    /// 実体と記録が一致している
    pub in_sync: Vec<String>,
    /// 実体にあって記録に無い
    pub new_entries: Vec<String>,
    /// 記録にあって実体に無い (孤児)
    pub orphans: Vec<String>,
    /// 語彙外の値 (項目, 値)
    pub illegal: Vec<(String, String)>,
}

impl DriftReport {
    /// 直すところがあるか。
    pub fn dirty(&self) -> bool {
        !self.new_entries.is_empty() || !self.orphans.is_empty() || !self.illegal.is_empty()
    }
}

fn req_id(cap: &str, name: &str) -> String {
    format!("{cap}/{name}")
}

fn task_id(change: &str, id: &str) -> String {
    format!("{change}#{id}")
}

/// 記録と実体を突き合わせる (純関数)。**何も書かない。**
pub fn drift_report(st: &State, caps: &[Capability], changes: &[Change]) -> DriftReport {
    let mut rep = DriftReport::default();
    let mut live_reqs: Vec<String> = Vec::new();
    for c in caps {
        for r in &c.requirements {
            live_reqs.push(req_id(&c.name, &r.name));
        }
    }
    let mut live_tasks: Vec<String> = Vec::new();
    for ch in changes {
        for t in &ch.tasks {
            live_tasks.push(task_id(&ch.id, t));
        }
    }
    let recorded_reqs: Vec<String> = st
        .requirements
        .iter()
        .map(|r| req_id(&r.capability, &r.name))
        .collect();
    let recorded_tasks: Vec<String> = st.tasks.iter().map(|t| task_id(&t.change, &t.id)).collect();

    for id in &live_reqs {
        if recorded_reqs.contains(id) {
            rep.in_sync.push(id.clone());
        } else {
            rep.new_entries.push(id.clone());
        }
    }
    for id in &live_tasks {
        if recorded_tasks.contains(id) {
            rep.in_sync.push(id.clone());
        } else {
            rep.new_entries.push(id.clone());
        }
    }
    for (i, id) in recorded_reqs.iter().enumerate() {
        if !live_reqs.contains(id) {
            rep.orphans.push(id.clone());
        }
        let s = &st.requirements[i].status;
        if !REQ_STATUSES.contains(&s.as_str()) {
            rep.illegal.push((id.clone(), s.clone()));
        }
    }
    for (i, id) in recorded_tasks.iter().enumerate() {
        if !live_tasks.contains(id) {
            rep.orphans.push(id.clone());
        }
        let s = &st.tasks[i].status;
        if !TASK_STATUSES.contains(&s.as_str()) {
            rep.illegal.push((id.clone(), s.clone()));
        }
    }
    rep
}

/// 実体から `state.toml` を組み直す (純関数)。**LLM ではなくここが唯一の書き手。**
///
/// * 既にある記録は基準 (`baseline` / `fingerprint`) を引き継ぐ
/// * 実体に無くなったものは落とす (孤児を残さない)
/// * 語彙外の値は既定へ倒す
/// * `statuses` (`<能力>/<要件>` → [`Staleness::key`]) があればそれを書く。
///   空なら記録済みの値を保つ (判定材料が無いのに上書きしない)
pub fn sync_state(
    old: &State,
    caps: &[Capability],
    changes: &[Change],
    statuses: &BTreeMap<String, String>,
) -> State {
    let mut st = State {
        version: STATE_VERSION,
        ..State::default()
    };
    for c in caps {
        for r in &c.requirements {
            let prev = old.find(&c.name, &r.name);
            let mut targets: Vec<String> = c.targets.clone();
            targets.extend(r.governs());
            st.requirements.push(ReqState {
                capability: c.name.clone(),
                name: r.name.clone(),
                fingerprint: prev
                    .map(|p| p.fingerprint.clone())
                    .unwrap_or_else(|| r.fingerprint()),
                baseline: prev.map(|p| p.baseline.clone()).unwrap_or_default(),
                status: statuses
                    .get(&req_id(&c.name, &r.name))
                    .cloned()
                    .or_else(|| prev.map(|p| p.status.clone()))
                    .filter(|s| REQ_STATUSES.contains(&s.as_str()))
                    .unwrap_or_else(|| "unknown".into()),
                targets,
            });
        }
    }
    for ch in changes {
        for t in &ch.tasks {
            let prev = old.tasks.iter().find(|x| x.change == ch.id && x.id == *t);
            st.tasks.push(TaskState {
                change: ch.id.clone(),
                id: t.clone(),
                title: prev.map(|p| p.title.clone()).unwrap_or_else(|| t.clone()),
                status: prev
                    .map(|p| p.status.clone())
                    .filter(|s| TASK_STATUSES.contains(&s.as_str()))
                    .unwrap_or_else(|| "todo".into()),
            });
        }
    }
    st
}

/// 検証 → 一時ファイル → rename の順で書く。
///
/// **失敗したら元のファイルは 1 バイトも変わらない。**
/// 検証で弾かれた時点で書き始めないし、書けても読み直して同じ物にならなければ
/// 一時ファイルを捨てる (半端な state.toml を本番の名前に置かない)。
pub fn write_state_atomic(path: &Path, st: &State) -> Result<(), String> {
    validate_state(st).map_err(|bad| bad.join(" / "))?;
    let text = toml::to_string_pretty(st).map_err(|e| e.to_string())?;
    // 書いたものが読み戻せるか (往復) をここで確かめる。
    let back = migrate_state(&text).map_err(|e| trf("書き戻せない: {e}", &[("e", e)]))?;
    if &back != st {
        return Err(tr(
            "書き戻した内容が一致しない (state.toml は変更していません)",
        ));
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = tmp_sibling(path);
    std::fs::write(&tmp, text.as_bytes()).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    // rename は同じディレクトリ内なので、Windows でも既存を置き換える
    // (std の rename は MOVEFILE_REPLACE_EXISTING 相当)。
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e.to_string());
    }
    Ok(())
}

/// 同じディレクトリに作る一時ファイル名 (プロセスと時刻で衝突しない)。
fn tmp_sibling(path: &Path) -> PathBuf {
    let dir = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let stem = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "state".into());
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".{stem}.zv-{}-{nanos}.tmp", std::process::id()))
}

// ---------------------------------------------------------------------------
// 進行中の変更
// ---------------------------------------------------------------------------

/// `changes/<ID>/proposal.toml`。**コードが書く**。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    #[serde(default)]
    pub version: u32,
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub gear: Gear,
    /// 完全パスのタスク ID (軽量パスでは空)。
    #[serde(default)]
    pub tasks: Vec<String>,
}

/// 進行中の変更 1 件。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Change {
    pub id: String,
    pub dir: PathBuf,
    pub title: String,
    pub gear: Gear,
    pub tasks: Vec<String>,
    /// (能力名, 差分, 差分ファイルのパス)
    pub deltas: Vec<(String, Delta, PathBuf)>,
}

impl Change {
    /// 変更全体の大きさ。
    pub fn footprint(&self) -> Footprint {
        let mut f = Footprint::default();
        for (_, d, _) in &self.deltas {
            let g = footprint(d);
            f.reqs += g.reqs;
            f.scenarios += g.scenarios;
            f.removes += g.removes;
            f.files += g.files;
        }
        f
    }

    /// 選んだギアが差分の大きさに合っていないか。
    pub fn gear_mismatch(&self) -> bool {
        self.gear == Gear::Light && suggest_gear(self.footprint()) == Gear::Full
    }
}

/// 完全パスで作るタスクの ID。**コードが決める** (LLM に採番させない)。
const FULL_TASKS: [&str; 4] = ["delta", "impl", "test", "archive"];

// ---------------------------------------------------------------------------
// 走査 (裏のスレッド専用) — ここだけが git とファイルを触る
// ---------------------------------------------------------------------------

/// 1 回の走査の結果。
#[derive(Clone, Debug, Default)]
pub struct Scan {
    pub root: PathBuf,
    pub caps: Vec<Capability>,
    pub changes: Vec<Change>,
    pub state: State,
    pub report: DriftReport,
    /// `<能力>/<要件>` → 判定
    pub status: BTreeMap<String, Staleness>,
    /// 読み込みで起きた問題 (画面に 1 行で出す)
    pub notes: Vec<String>,
}

fn read_dirs(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<(String, PathBuf)> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            (!name.starts_with('.')).then_some((name, e.path()))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn load_capabilities(sroot: &Path) -> Vec<Capability> {
    read_dirs(&sroot.join(SPECS_DIR))
        .into_iter()
        .filter_map(|(name, dir)| {
            let path = dir.join(SPEC_FILE);
            let text = std::fs::read_to_string(&path).ok()?;
            Some(parse_capability(&name, path, &text))
        })
        .collect()
}

fn load_changes(sroot: &Path) -> Vec<Change> {
    read_dirs(&sroot.join(CHANGES_DIR))
        .into_iter()
        .filter(|(name, _)| name != ARCHIVE_DIR)
        .map(|(id, dir)| {
            let prop: Proposal = std::fs::read_to_string(dir.join(PROPOSAL_FILE))
                .ok()
                .and_then(|s| toml::from_str(&s).ok())
                .unwrap_or_default();
            let mut deltas: Vec<(String, Delta, PathBuf)> = Vec::new();
            if let Ok(rd) = std::fs::read_dir(dir.join(DELTAS_DIR)) {
                let mut files: Vec<PathBuf> = rd
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("md"))
                    .collect();
                files.sort();
                for p in files {
                    let Ok(text) = std::fs::read_to_string(&p) else {
                        continue;
                    };
                    let cap = p
                        .file_stem()
                        .map(|s| s.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    deltas.push((cap, parse_delta(&text), p));
                }
            }
            Change {
                title: if prop.title.is_empty() {
                    id.clone()
                } else {
                    prop.title.clone()
                },
                gear: prop.gear,
                tasks: prop.tasks.clone(),
                id,
                dir,
                deltas,
            }
        })
        .collect()
}

/// git の管理下にあるファイルの一覧 (repo 相対、`/` 区切り)。
fn tracked_files(repo: &Path) -> Vec<String> {
    crate::git::run_git_at(repo, &["ls-files".to_string()])
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

/// glob / 実パスを実体へ解決する (純関数)。返り値は `(当たったパス, 見つからない指定)`。
///
/// **1 つも当たらない glob は「見つからない」に数えない。** glob は *絞り込み* で
/// あって *リンク* ではないので、まだ 1 枚も書いていない `src/auth/**` を
/// リンク切れと呼ぶと、機能を作り始めた瞬間に赤くなる。
/// リンク切れとして数えるのは**実パスで書かれた指定**だけ — こちらは
/// 「そこにあると宣言した」ものなので、無ければ事実として誤り。
pub fn resolve_targets(patterns: &[String], files: &[String]) -> (Vec<String>, Vec<String>) {
    let mut hit: Vec<String> = Vec::new();
    let mut missing: Vec<String> = Vec::new();
    for pat in patterns {
        let p = target_path(pat);
        if p.is_empty() {
            continue;
        }
        let is_glob = p.contains(['*', '?', '[']);
        let mut found = false;
        for f in files {
            let ok = if is_glob {
                crate::file_search::glob_match(p, f)
            } else {
                f == p
            };
            if ok {
                found = true;
                if !hit.contains(f) {
                    hit.push(f.clone());
                }
            }
        }
        if !found && !is_glob {
            missing.push(p.to_string());
        }
    }
    (hit, missing)
}

/// 1 回の走査。**裏のスレッドからしか呼ばない** (git を待つため)。
pub fn scan(workspace: &Path) -> Scan {
    let root = spec_root(workspace);
    let caps = load_capabilities(&root);
    let changes = load_changes(&root);
    let state = match std::fs::read_to_string(root.join(STATE_FILE)) {
        Ok(raw) => migrate_state(&raw),
        Err(_) => Ok(State {
            version: STATE_VERSION,
            ..State::default()
        }),
    };
    let mut notes: Vec<String> = Vec::new();
    let state = state.unwrap_or_else(|e| {
        notes.push(e);
        State {
            version: STATE_VERSION,
            ..State::default()
        }
    });
    let report = drift_report(&state, &caps, &changes);

    // ── 陳腐化の判定。git は**ここ (裏のスレッド) でだけ**走らせる ──
    let files = tracked_files(workspace);
    let mut status: BTreeMap<String, Staleness> = BTreeMap::new();
    // 基準コミットごとにまとめて 1 回だけ `git diff` を撃つ
    // (要件ごとに撃つと、要件の数だけ index を取り合って遅くなる)。
    let mut by_baseline: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in &caps {
        for r in &c.requirements {
            let Some(rec) = state.find(&c.name, &r.name) else {
                status.insert(req_id(&c.name, &r.name), Staleness::Unknown);
                continue;
            };
            if rec.baseline.is_empty() {
                status.insert(req_id(&c.name, &r.name), Staleness::Unknown);
                continue;
            }
            let mut pats = c.targets.clone();
            pats.extend(r.governs());
            let (hit, _) = resolve_targets(&pats, &files);
            by_baseline
                .entry(rec.baseline.clone())
                .or_default()
                .extend(hit);
        }
    }
    let mut diffs: BTreeMap<String, Vec<FileDrift>> = BTreeMap::new();
    for (base, mut paths) in by_baseline {
        paths.sort();
        paths.dedup();
        if paths.is_empty() {
            diffs.insert(base, Vec::new());
            continue;
        }
        let mut args: Vec<String> = vec![
            "diff".into(),
            "-U0".into(),
            "-M".into(),
            "--ignore-all-space".into(),
            base.clone(),
            "--".into(),
        ];
        args.extend(paths);
        let text = crate::git::run_git_at(workspace, &args).unwrap_or_default();
        diffs.insert(base, drifts_of(&text));
    }
    for c in &caps {
        for r in &c.requirements {
            let key = req_id(&c.name, &r.name);
            if status.contains_key(&key) {
                continue;
            }
            let Some(rec) = state.find(&c.name, &r.name) else {
                continue;
            };
            let mut pats = c.targets.clone();
            pats.extend(r.governs());
            let (hit, missing) = resolve_targets(&pats, &files);
            let empty: Vec<FileDrift> = Vec::new();
            let mine: Vec<FileDrift> = diffs
                .get(&rec.baseline)
                .unwrap_or(&empty)
                .iter()
                .filter(|d| {
                    hit.contains(&d.path)
                        || d.moved_from.as_ref().is_some_and(|m| hit.contains(m))
                        || missing.contains(&d.path)
                        || d.moved_from.as_ref().is_some_and(|m| missing.contains(m))
                })
                .cloned()
                .collect();
            let spec_changed = rec.fingerprint != r.fingerprint();
            status.insert(key, assess(spec_changed, &mine, &missing));
        }
    }

    Scan {
        root,
        caps,
        changes,
        state,
        report,
        status,
        notes,
    }
}

// ---------------------------------------------------------------------------
// 書き込み (描画の外でだけ呼ぶ)
// ---------------------------------------------------------------------------

/// パネルが app へ返す「書いてほしいこと」。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteReq {
    /// 新しい変更の骨組みを作る
    Scaffold {
        id: String,
        capability: String,
        gear: Gear,
    },
    /// 差分を真実へ畳んで、変更を archive へ移す
    Archive(String),
    /// この要件を「いま追認した」ことにする (指紋と HEAD を記録し直す)
    Confirm { capability: String, name: String },
    /// `state.toml` を実体から組み直す。`statuses` は**画面がいま見ている判定**
    /// (`<能力>/<要件>` → [`Staleness::key`])。書き手はコードのままで、
    /// 判定の結果だけを載せる。
    SyncState { statuses: BTreeMap<String, String> },
}

/// パネルが app へ返す要求。**I/O は 1 つも描画中に行わない。**
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SpecAction {
    #[default]
    None,
    /// 走査し直す
    Rescan,
    /// エディタで開く
    Open(PathBuf),
    /// エージェントへ渡す文脈 (送信経路は app が持っている)
    Hand(String),
    /// spec ルート配下への書き込み
    Write(WriteReq),
}

/// 変更 ID として使える形に均す (純関数)。
///
/// パスの一部になるので、区切り文字や `..` を**構造的に**作れないようにする。
pub fn slug(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for ch in raw.trim().chars() {
        let c = if ch.is_alphanumeric() {
            ch.to_ascii_lowercase()
        } else {
            '-'
        };
        if c == '-' {
            if prev_dash || out.is_empty() {
                continue;
            }
            prev_dash = true;
        } else {
            prev_dash = false;
        }
        out.push(c);
        if out.chars().count() >= 60 {
            break;
        }
    }
    out.trim_matches('-').to_string()
}

/// 軽量パスの差分テンプレート。**1 要件 1 シナリオだけ**の短い雛形。
fn light_template(capability: &str, id: &str) -> String {
    format!(
        "# {id}\n\n\
         ## ADDED Requirements\n\n\
         ### Requirement: {capability} の新しい振る舞い\n\
         The system MUST …\n\
         [@code] src/\n\n\
         #### Scenario: 代表的な 1 本\n\
         - GIVEN …\n\
         - WHEN …\n\
         - THEN …\n"
    )
}

/// 書き込み要求を実行する (I/O)。返り値はトーストに出す 1 行。
pub fn apply_write(req: WriteReq, workspace: &Path) -> Result<String, String> {
    let root = spec_root(workspace);
    match req {
        WriteReq::Scaffold {
            id,
            capability,
            gear,
        } => {
            let id = slug(&id);
            let capability = {
                let c = slug(&capability);
                if c.is_empty() {
                    "new-capability".to_string()
                } else {
                    c
                }
            };
            if id.is_empty() {
                return Err(tr("変更 ID を入れてください"));
            }
            let dir = root.join(CHANGES_DIR).join(&id);
            if dir.exists() {
                return Err(trf("{id} は既にあります", &[("id", id)]));
            }
            std::fs::create_dir_all(dir.join(DELTAS_DIR)).map_err(|e| e.to_string())?;
            let delta = dir.join(DELTAS_DIR).join(format!("{capability}.md"));
            std::fs::write(&delta, light_template(&capability, &id)).map_err(|e| e.to_string())?;
            let prop = Proposal {
                version: STATE_VERSION,
                id: id.clone(),
                title: id.clone(),
                gear,
                tasks: if gear == Gear::Full {
                    FULL_TASKS.iter().map(|s| (*s).to_string()).collect()
                } else {
                    Vec::new()
                },
            };
            std::fs::write(
                dir.join(PROPOSAL_FILE),
                toml::to_string_pretty(&prop).map_err(|e| e.to_string())?,
            )
            .map_err(|e| e.to_string())?;
            Ok(trf(
                "{id} を作りました ({gear}パス)",
                &[("id", id), ("gear", gear.label())],
            ))
        }
        WriteReq::Archive(id) => archive(workspace, &id),
        WriteReq::Confirm { capability, name } => {
            let caps = load_capabilities(&root);
            let cap = caps
                .iter()
                .find(|c| c.name == capability)
                .ok_or_else(|| trf("{c} が見つかりません", &[("c", capability.clone())]))?;
            let req = cap
                .requirements
                .iter()
                .find(|r| r.name == name)
                .ok_or_else(|| trf("{n} が見つかりません", &[("n", name.clone())]))?;
            let head = head_sha(workspace)?;
            let changes = load_changes(&root);
            let path = root.join(STATE_FILE);
            let old = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| migrate_state(&raw).ok())
                .unwrap_or_default();
            // 追認は 1 件だけを動かす。他の要件の判定は記録済みの値を保つ
            // (見えていない要件の状態を巻き添えで書き換えない)。
            let mut st = sync_state(&old, &caps, &changes, &BTreeMap::new());
            let Some(rec) = st
                .requirements
                .iter_mut()
                .find(|r| r.capability == capability && r.name == name)
            else {
                return Err(tr("state.toml に載せられませんでした"));
            };
            rec.fingerprint = req.fingerprint();
            rec.baseline = head;
            rec.status = "in-sync".into();
            write_state_atomic(&path, &st)?;
            Ok(trf("{n} を追認しました", &[("n", name)]))
        }
        WriteReq::SyncState { statuses } => {
            let caps = load_capabilities(&root);
            let changes = load_changes(&root);
            let path = root.join(STATE_FILE);
            let old = std::fs::read_to_string(&path)
                .ok()
                .and_then(|raw| migrate_state(&raw).ok())
                .unwrap_or_default();
            let st = sync_state(&old, &caps, &changes, &statuses);
            let (nr, nt) = (st.requirements.len(), st.tasks.len());
            write_state_atomic(&path, &st)?;
            Ok(trf(
                "state.toml を作り直しました (要件 {r} / タスク {t})",
                &[("r", nr.to_string()), ("t", nt.to_string())],
            ))
        }
    }
}

fn head_sha(repo: &Path) -> Result<String, String> {
    crate::git::run_git_at(repo, &["rev-parse".to_string(), "HEAD".to_string()])
        .map(|s| s.trim().to_string())
        .and_then(|s| {
            if s.is_empty() {
                Err(tr("HEAD が取れません (コミットが 1 つもない?)"))
            } else {
                Ok(s)
            }
        })
}

/// 差分を真実へ畳み、変更を `changes/archive/` へ移す。
///
/// 衝突が 1 つでもあれば**何も書かずに**止める (半分だけ当たった状態を作らない)。
fn archive(workspace: &Path, id: &str) -> Result<String, String> {
    let root = spec_root(workspace);
    let changes = load_changes(&root);
    let ch = changes
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| trf("{id} が見つかりません", &[("id", id.to_string())]))?;
    if ch.deltas.is_empty() {
        return Err(tr("差分が 1 枚もありません"));
    }
    let mut caps = load_capabilities(&root);
    let mut written: Vec<(PathBuf, String)> = Vec::new();
    let mut retired: Vec<String> = Vec::new();
    let mut summary = ApplyReport::default();
    for (cap_name, delta, _) in &ch.deltas {
        let mut cap = match caps.iter().position(|c| &c.name == cap_name) {
            Some(i) => caps.remove(i),
            None => Capability {
                name: cap_name.clone(),
                path: root.join(SPECS_DIR).join(cap_name).join(SPEC_FILE),
                ..Capability::default()
            },
        };
        let rep = apply_delta(&mut cap, delta);
        if !rep.is_clean() {
            return Err(trf(
                "衝突があるので何も書いていません: {c}",
                &[("c", rep.conflicts.join(" / "))],
            ));
        }
        summary.added.extend(rep.added);
        summary.modified.extend(rep.modified);
        summary.removed.extend(rep.removed);
        if rep.retired {
            retired.push(cap.name.clone());
        }
        written.push((cap.path.clone(), render_capability(&cap)));
    }
    // ここから書き込み。検証は上で済んでいる。
    for (path, text) in &written {
        if retired
            .iter()
            .any(|r| path.ends_with(Path::new(r).join(SPEC_FILE)))
        {
            continue;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(path, text).map_err(|e| e.to_string())?;
    }
    for name in &retired {
        // 最後の 1 件が消えた能力は引退させる (空の spec.md を残さない)。
        let _ = std::fs::remove_dir_all(root.join(SPECS_DIR).join(name));
    }
    let dest_parent = root.join(CHANGES_DIR).join(ARCHIVE_DIR);
    std::fs::create_dir_all(&dest_parent).map_err(|e| e.to_string())?;
    let dest = dest_parent.join(id);
    if dest.exists() {
        std::fs::remove_dir_all(&dest).map_err(|e| e.to_string())?;
    }
    std::fs::rename(&ch.dir, &dest).map_err(|e| e.to_string())?;
    Ok(trf(
        "{id} を統合しました (＋{a} ～{m} －{r}{extra})",
        &[
            ("id", id.to_string()),
            ("a", summary.added.len().to_string()),
            ("m", summary.modified.len().to_string()),
            ("r", summary.removed.len().to_string()),
            (
                "extra",
                if retired.is_empty() {
                    String::new()
                } else {
                    format!(" / 引退: {}", retired.join(", "))
                },
            ),
        ],
    ))
}

// ---------------------------------------------------------------------------
// エージェントへの引き継ぎ (設計原則 5 — 文脈は 1 面で作る)
// ---------------------------------------------------------------------------

/// 要件 1 つをエージェントへ渡す文脈 (純関数)。
pub fn requirement_context(cap: &Capability, r: &Requirement, st: &Staleness) -> String {
    let mut s = String::new();
    s.push_str(&trf(
        "次の仕様に従って作業してください。能力: {c}\n",
        &[("c", cap.name.clone())],
    ));
    s.push_str(&format!("\n### Requirement: {}\n{}\n", r.name, r.text));
    for sc in &r.scenarios {
        s.push_str(&format!("\n#### Scenario: {}\n", sc.title));
        for step in &sc.steps {
            s.push_str(&format!("- {step}\n"));
        }
    }
    let governs = {
        let mut g = cap.targets.clone();
        g.extend(r.governs());
        g
    };
    if !governs.is_empty() {
        s.push_str(&trf(
            "\n統べている対象: {g}\n",
            &[("g", governs.join(", "))],
        ));
    }
    match st {
        Staleness::Suspect { .. } | Staleness::Dangling { .. } => {
            s.push_str(&trf(
                "\n注意: この要件は {chip} です ({detail})。\
                 コードに合わせて要件の文を直すか、要件に合わせてコードを直すか、\
                 **どちらが正しいかを先に述べてから**作業してください。\n",
                &[("chip", st.chip()), ("detail", st.detail())],
            ));
        }
        _ => {}
    }
    s
}

/// 変更 1 件をエージェントへ渡す文脈 (純関数)。
pub fn change_context(ch: &Change) -> String {
    let mut s = trf(
        "変更 {id} ({gear}パス) の差分を実装してください。\
         差分に書かれていないことはしないでください。\n",
        &[("id", ch.id.clone()), ("gear", ch.gear.label())],
    );
    for (cap, d, _) in &ch.deltas {
        s.push_str(&format!("\n## capability: {cap}\n"));
        for (verb, list) in [(Verb::Added, &d.added), (Verb::Modified, &d.modified)] {
            for r in list {
                s.push_str(&format!(
                    "\n### {} Requirement: {}\n{}\n",
                    verb.keyword(),
                    r.name,
                    r.text
                ));
                for sc in &r.scenarios {
                    s.push_str(&format!("#### Scenario: {}\n", sc.title));
                    for step in &sc.steps {
                        s.push_str(&format!("- {step}\n"));
                    }
                }
                let g = r.governs();
                if !g.is_empty() {
                    s.push_str(&format!("対象: {}\n", g.join(", ")));
                }
            }
        }
        for name in &d.removed {
            s.push_str(&format!(
                "\n### {} Requirement: {name}\n",
                Verb::Removed.keyword()
            ));
        }
    }
    if !ch.tasks.is_empty() {
        s.push_str(&trf("\nタスク: {t}\n", &[("t", ch.tasks.join(" → "))]));
    }
    s
}

// ---------------------------------------------------------------------------
// レイアウト (純関数) — どの幅でも見切れない / 重ならない
// ---------------------------------------------------------------------------

/// 一覧と詳細を左右に並べられる下限幅。これを割ったら縦に積む。
const SPLIT_MIN_W: f32 = 680.0;
/// 一覧の幅 (左右に並べるとき)。
const LIST_W: f32 = 240.0;
/// 一覧の高さ (縦に積むとき) の割合。
const STACK_LIST_RATIO: f32 = 0.38;

/// パネル本体の 2 枚の矩形。**必ず `avail` の中に収まり、重ならない。**
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PanelLayout {
    pub list: egui::Rect,
    pub detail: egui::Rect,
    /// 縦に積んだか
    pub stacked: bool,
}

/// 一覧 / 詳細の配置を決める (純関数)。
pub fn panel_layout(avail: egui::Rect) -> PanelLayout {
    let w = avail.width().max(0.0);
    let h = avail.height().max(0.0);
    if w >= SPLIT_MIN_W {
        let lw = LIST_W.min((w - space::SM) * 0.5).max(0.0);
        let list = egui::Rect::from_min_size(avail.min, egui::vec2(lw, h));
        // 幅が 0 に近いところでも **avail の外へ出さない** (右端で止める)。
        let dx = (avail.left() + lw + space::SM).min(avail.right());
        let dw = (avail.right() - dx).max(0.0);
        return PanelLayout {
            list,
            detail: egui::Rect::from_min_size(egui::pos2(dx, avail.top()), egui::vec2(dw, h)),
            stacked: false,
        };
    }
    let lh = (h * STACK_LIST_RATIO)
        .min((h - space::SM).max(0.0))
        .max(0.0);
    let list = egui::Rect::from_min_size(avail.min, egui::vec2(w, lh));
    // 高さが 0 に近いところでも **avail の外へ出さない** (下端で止める)。
    let dy = (avail.top() + lh + space::SM).min(avail.bottom());
    let dh = (avail.bottom() - dy).max(0.0);
    PanelLayout {
        list,
        detail: egui::Rect::from_min_size(egui::pos2(avail.left(), dy), egui::vec2(w, dh)),
        stacked: true,
    }
}

/// 空状態カードの最大幅 / 高さ。
const EMPTY_CARD_MAX_W: f32 = 520.0;
const EMPTY_CARD_H: f32 = 196.0;

/// 空状態カードの矩形 (純関数)。**常に `avail` の中央 1 枚**で、必ず収まる。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let aw = avail.width().max(0.0);
    let ah = avail.height().max(0.0);
    let w = (aw - space::LG * 2.0).clamp(0.0, EMPTY_CARD_MAX_W).min(aw);
    let h = EMPTY_CARD_H.min(ah);
    let x = avail.left() + (aw - w) * 0.5;
    let y = avail.top() + (ah - h) * 0.5;
    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h))
}

// ---------------------------------------------------------------------------
// パネル (状態 + 描画)
// ---------------------------------------------------------------------------

/// 一覧で選んでいるもの。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Sel {
    Capability(String),
    Change(String),
}

/// パネルの表示状態。app が所有する。
#[derive(Default)]
pub struct SpecPanel {
    /// spec ルート (表示用)
    pub root: PathBuf,
    pub caps: Vec<Capability>,
    pub changes: Vec<Change>,
    pub state: State,
    pub report: DriftReport,
    pub status: BTreeMap<String, Staleness>,
    pub notes: Vec<String>,
    pub selected: Option<Sel>,
    /// 「＋ 変更を起こす」の入力欄を出しているか
    pub new_open: bool,
    pub new_id: String,
    pub new_cap: String,
    pub new_gear: Gear,
    /// **疑いのある要件だけに絞る。** 能力が数十本あると、⚠ の 3 件が
    /// 埋もれて見つからない。バッジと同じ数だけを画面に残す絞り込み。
    pub only_stale: bool,
    /// 1 度でも走査したか (空状態と「まだ読んでいない」を区別する)
    pub scanned: bool,
    pending: Option<Receiver<Scan>>,
    started: Option<Instant>,
    cost: Option<Duration>,
    last: Option<Instant>,
}

impl SpecPanel {
    /// タブに添える件数 = **疑いの数**。0 のときは `None` (常に 0 のバッジを作らない)。
    pub fn badge(&self) -> Option<usize> {
        match self.stale_count() {
            0 => None,
            n => Some(n),
        }
    }

    pub fn stale_count(&self) -> usize {
        self.status.values().filter(|s| s.is_suspect()).count()
    }

    /// 次の [`poll`](Self::poll) で必ず取り直す。
    pub fn invalidate(&mut self) {
        self.last = None;
    }

    /// **疑いのある要件だけを見る。** 絞り込みを立て、最初に疑いのある能力へ
    /// 選択を移す。疑いが 1 件も無いときは**何も変えない**
    /// (押しても空の画面になるだけ、という操作を作らない)。
    /// 走査前で判定がまだ無いときも、絞り込みだけ立てて次の走査を待つ。
    pub fn focus_stale(&mut self) {
        if self.scanned && self.stale_count() == 0 {
            return;
        }
        self.only_stale = true;
        if let Some(c) = self.first_stale_capability() {
            self.selected = Some(Sel::Capability(c));
        }
    }

    /// 疑いのある要件を持つ最初の能力。
    fn first_stale_capability(&self) -> Option<String> {
        self.caps
            .iter()
            .find(|c| {
                c.requirements
                    .iter()
                    .any(|r| self.status_of(&c.name, &r.name).is_suspect())
            })
            .map(|c| c.name.clone())
    }

    fn status_of(&self, cap: &str, req: &str) -> Staleness {
        self.status
            .get(&req_id(cap, req))
            .cloned()
            .unwrap_or_default()
    }

    /// **毎フレーム呼んでよい。決して待たない。**
    ///
    /// 走査は裏のスレッドで、間隔は [`crate::git::scan_interval`] が
    /// 直近の所要時間から決める (遅いリポジトリでは自動で空く)。
    /// 呼ぶのは**パネルを出している間だけ** — アイドルのコストはゼロ。
    pub fn poll(&mut self, workspace: &Path) {
        if let Some(rx) = &self.pending {
            match rx.try_recv() {
                Ok(scan) => {
                    self.cost = self.started.take().map(|t| t.elapsed());
                    self.pending = None;
                    self.last = Some(Instant::now());
                    self.absorb(scan);
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.pending = None;
                    self.started = None;
                    self.last = Some(Instant::now());
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let wait = crate::git::scan_interval(SCAN_BASE, self.cost);
        let due = self.last.is_none_or(|t| t.elapsed() >= wait);
        if due && self.pending.is_none() {
            let ws = workspace.to_path_buf();
            let (tx, rx) = mpsc::channel();
            let spawned = std::thread::Builder::new()
                .name("zv-spec-scan".into())
                .spawn(move || {
                    let _ = tx.send(scan(&ws));
                });
            if spawned.is_ok() {
                self.pending = Some(rx);
                self.started = Some(Instant::now());
            } else {
                // スレッドを起こせない環境。**同期実行へは落とさない**
                // (落とすと描画が数秒止まる)。次の間隔でまた挑む。
                self.last = Some(Instant::now());
            }
        }
    }

    fn absorb(&mut self, s: Scan) {
        self.root = s.root;
        self.caps = s.caps;
        self.changes = s.changes;
        self.state = s.state;
        self.report = s.report;
        self.status = s.status;
        self.notes = s.notes;
        self.scanned = true;
        // 選択が消えていたら外す (無い物の詳細を描かない)
        let gone = match &self.selected {
            Some(Sel::Capability(n)) => !self.caps.iter().any(|c| &c.name == n),
            Some(Sel::Change(n)) => !self.changes.iter().any(|c| &c.id == n),
            None => false,
        };
        if gone {
            self.selected = None;
        }
        if self.selected.is_none() {
            self.selected = self
                .changes
                .first()
                .map(|c| Sel::Change(c.id.clone()))
                .or_else(|| self.caps.first().map(|c| Sel::Capability(c.name.clone())));
        }
    }
}

/// spec パネルを描く。**I/O は 1 つも行わない** — 要求は返り値で返す。
pub fn ui(ui: &mut egui::Ui, theme: &Theme, panel: &mut SpecPanel) -> SpecAction {
    let mut action = SpecAction::None;
    header(ui, theme, panel, &mut action);
    if panel.new_open {
        new_change_form(ui, theme, panel, &mut action);
    }
    if panel.caps.is_empty() && panel.changes.is_empty() {
        empty_state(ui, theme, panel, &mut action);
        return action;
    }
    ui.add_space(space::XS);
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let lay = panel_layout(avail);
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(lay.list), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("zv-spec-list")
            .auto_shrink([false, false])
            .show(ui, |ui| list_ui(ui, theme, panel));
    });
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(lay.detail), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("zv-spec-detail")
            .auto_shrink([false, false])
            .show(ui, |ui| detail_ui(ui, theme, panel, &mut action));
    });
    action
}

fn header(ui: &mut egui::Ui, theme: &Theme, panel: &mut SpecPanel, action: &mut SpecAction) {
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(tr("📐 Spec")).size(13.0).color(theme.text));
        let stale = panel.stale_count();
        if stale > 0 {
            // 0 のときは 1 ピクセルも描かない (常に 0 のバッジを作らない)
            ui.label(
                RichText::new(trf("⚠ {n}", &[("n", stale.to_string())]))
                    .size(11.5)
                    .color(theme.warn),
            )
            .on_hover_text(tr("陳腐化の疑い / リンク切れのある要件の数"));
        }
        if !panel.caps.is_empty() || !panel.changes.is_empty() {
            ui.label(
                RichText::new(trf(
                    "能力 {c} / 変更 {h}",
                    &[
                        ("c", panel.caps.len().to_string()),
                        ("h", panel.changes.len().to_string()),
                    ],
                ))
                .size(11.5)
                .color(theme.text_dim),
            );
        }
        if panel.report.dirty() {
            ui.label(
                RichText::new(trf(
                    "state.toml: 新規 {n} / 孤児 {o} / 語彙外 {i}",
                    &[
                        ("n", panel.report.new_entries.len().to_string()),
                        ("o", panel.report.orphans.len().to_string()),
                        ("i", panel.report.illegal.len().to_string()),
                    ],
                ))
                .size(11.0)
                .color(theme.warn),
            )
            .on_hover_text(tr("「🧮 state を作り直す」で実体に合わせます"));
        }
        for note in &panel.notes {
            ui.label(RichText::new(note).size(11.0).color(theme.err));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("⟳").on_hover_text(tr("読み直す")).clicked() {
                *action = SpecAction::Rescan;
            }
            if ui
                .button(tr("🧮 state"))
                .on_hover_text(tr("state.toml を実体から組み直します (書くのはコードで、\
                     検証してから一時ファイル経由で置き換えます)"))
                .clicked()
            {
                *action = SpecAction::Write(WriteReq::SyncState {
                    statuses: panel
                        .status
                        .iter()
                        .map(|(k, v)| (k.clone(), v.key().to_string()))
                        .collect(),
                });
            }
            // 疑いが 1 件も無いときはボタン自体を出さない
            // (押しても何も起きない操作を画面へ置かない)
            if stale > 0
                && ui
                    .selectable_label(panel.only_stale, tr("⚠ 疑いだけ"))
                    .on_hover_text(tr("陳腐化の疑い / リンク切れのある要件だけに絞ります"))
                    .clicked()
            {
                if panel.only_stale {
                    panel.only_stale = false;
                } else {
                    // 絞り込みの入口は 1 本だけ (パレットからもここを通る)
                    panel.focus_stale();
                }
            }
            if ui
                .button(tr("＋ 変更"))
                .on_hover_text(tr("新しい変更 (delta) の骨組みを作ります"))
                .clicked()
            {
                panel.new_open = !panel.new_open;
            }
        });
    });
}

fn new_change_form(
    ui: &mut egui::Ui,
    theme: &Theme,
    panel: &mut SpecPanel,
    action: &mut SpecAction,
) {
    egui::Frame::none()
        .fill(theme.panel_alt)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(space::SM, space::XS))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(tr("ID")).size(11.5).color(theme.text_dim));
                let w = (ui.available_width() * 0.3).clamp(80.0, 200.0);
                ui.add_sized(
                    [w, 22.0],
                    egui::TextEdit::singleline(&mut panel.new_id).hint_text(tr("add-2fa")),
                );
                ui.label(RichText::new(tr("能力")).size(11.5).color(theme.text_dim));
                let w = (ui.available_width() * 0.3).clamp(80.0, 200.0);
                ui.add_sized(
                    [w, 22.0],
                    egui::TextEdit::singleline(&mut panel.new_cap).hint_text(tr("auth")),
                );
                // ギアは既定が軽量。1 行の修正に三部作を作らせない
                for g in [Gear::Light, Gear::Full] {
                    if ui
                        .selectable_label(panel.new_gear == g, g.label())
                        .on_hover_text(match g {
                            Gear::Light => tr(
                                "軽量パス — 差分 1 枚だけ。要件/設計/タスクの三部作を作りません",
                            ),
                            Gear::Full => {
                                tr("完全パス — 差分 + タスク表 (delta→impl→test→archive)")
                            }
                        })
                        .clicked()
                    {
                        panel.new_gear = g;
                    }
                }
                if ui.button(tr("作る")).clicked() {
                    *action = SpecAction::Write(WriteReq::Scaffold {
                        id: panel.new_id.clone(),
                        capability: panel.new_cap.clone(),
                        gear: panel.new_gear,
                    });
                    panel.new_open = false;
                    panel.new_id.clear();
                }
            });
        });
}

fn empty_state(ui: &mut egui::Ui, theme: &Theme, panel: &mut SpecPanel, action: &mut SpecAction) {
    let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
    let card = empty_card(avail);
    ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
        egui::Frame::none()
            .fill(theme.panel_alt)
            .stroke(egui::Stroke::new(1.0_f32, theme.border))
            .rounding(egui::Rounding::same(10.0))
            .inner_margin(egui::Margin::same(space::MD))
            .show(ui, |ui| {
                ui.set_width((card.width() - space::MD * 2.0).max(0.0));
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("📐").size(38.0));
                    ui.label(
                        RichText::new(if panel.scanned {
                            tr("まだ仕様がありません")
                        } else {
                            tr("読み込んでいます…")
                        })
                        .size(16.0)
                        .color(theme.text),
                    );
                    ui.label(
                        RichText::new(tr("変更は差分 (ADDED / MODIFIED / REMOVED) で書き、\
                             統合したときだけ真実の spec.md へ畳みます。\
                             小さな修正には軽量パス — 差分 1 枚で終わります"))
                        .size(11.0)
                        .color(theme.text_dim),
                    );
                    ui.add_space(space::SM);
                    ui.horizontal(|ui| {
                        for g in [Gear::Light, Gear::Full] {
                            if ui
                                .button(trf("{g}パスで始める", &[("g", g.label())]))
                                .clicked()
                            {
                                panel.new_open = true;
                                panel.new_gear = g;
                            }
                        }
                    });
                    if panel.scanned {
                        ui.label(
                            RichText::new(trf(
                                "置き場所: {p}",
                                &[("p", panel.root.display().to_string())],
                            ))
                            .size(10.5)
                            .color(theme.text_dim),
                        );
                    }
                });
            });
    });
    let _ = action;
}

fn list_ui(ui: &mut egui::Ui, theme: &Theme, panel: &mut SpecPanel) {
    // 中身が無いセクションは**見出しごと出さない** (空白を作らない)。
    // 「⚠ 疑いだけ」の間は、疑いを持たない進行中の変更も畳む。
    if !panel.changes.is_empty() && !panel.only_stale {
        ui.label(
            RichText::new(tr("進行中の変更"))
                .size(11.0)
                .strong()
                .color(theme.text_dim),
        );
        let rows: Vec<(String, String, bool)> = panel
            .changes
            .iter()
            .map(|c| (c.id.clone(), c.gear.label(), c.gear_mismatch()))
            .collect();
        for (id, gear, mismatch) in rows {
            let on = panel.selected == Some(Sel::Change(id.clone()));
            let label = format!("{} {}  ·{}", if mismatch { "⚠" } else { "🧩" }, id, gear);
            if ui
                .add(egui::SelectableLabel::new(
                    on,
                    RichText::new(ellipsize(&label, 30)).size(12.0),
                ))
                .on_hover_text(if mismatch {
                    tr("軽量パスにしては差分が大きい (完全パスを検討)")
                } else {
                    id.clone()
                })
                .clicked()
            {
                panel.selected = Some(Sel::Change(id.clone()));
            }
        }
        ui.add_space(space::SM);
    }
    if !panel.caps.is_empty() {
        ui.label(
            RichText::new(tr("能力 (真実)"))
                .size(11.0)
                .strong()
                .color(theme.text_dim),
        );
        let rows: Vec<(String, usize, usize)> = panel
            .caps
            .iter()
            .map(|c| {
                let stale = c
                    .requirements
                    .iter()
                    .filter(|r| panel.status_of(&c.name, &r.name).is_suspect())
                    .count();
                (c.name.clone(), c.requirements.len(), stale)
            })
            .collect();
        for (name, n, stale) in rows {
            if panel.only_stale && stale == 0 {
                continue;
            }
            let on = panel.selected == Some(Sel::Capability(name.clone()));
            let label = if stale > 0 {
                format!("⚠ {name}  {stale}/{n}")
            } else {
                format!("📘 {name}  {n}")
            };
            if ui
                .add(egui::SelectableLabel::new(
                    on,
                    RichText::new(ellipsize(&label, 30))
                        .size(12.0)
                        .color(if stale > 0 { theme.warn } else { theme.text }),
                ))
                .on_hover_text(name.clone())
                .clicked()
            {
                panel.selected = Some(Sel::Capability(name.clone()));
            }
        }
    }
}

/// 長い文字列を省略する (全文はホバーで見せる)。
fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

fn detail_ui(ui: &mut egui::Ui, theme: &Theme, panel: &mut SpecPanel, action: &mut SpecAction) {
    match panel.selected.clone() {
        Some(Sel::Capability(name)) => {
            let Some(i) = panel.caps.iter().position(|c| c.name == name) else {
                return;
            };
            capability_detail(ui, theme, panel, i, action);
        }
        Some(Sel::Change(id)) => {
            let Some(i) = panel.changes.iter().position(|c| c.id == id) else {
                return;
            };
            change_detail(ui, theme, panel, i, action);
        }
        None => {}
    }
}

fn capability_detail(
    ui: &mut egui::Ui,
    theme: &Theme,
    panel: &SpecPanel,
    idx: usize,
    action: &mut SpecAction,
) {
    let cap = &panel.caps[idx];
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(&cap.name)
                .size(13.5)
                .strong()
                .color(theme.text),
        );
        if !cap.targets.is_empty() {
            ui.label(
                RichText::new(ellipsize(&cap.targets.join(", "), 60))
                    .size(11.0)
                    .color(theme.text_dim),
            )
            .on_hover_text(cap.targets.join("\n"));
        }
        if ui
            .button("📄")
            .on_hover_text(tr("spec.md を開く"))
            .clicked()
        {
            *action = SpecAction::Open(cap.path.clone());
        }
    });
    for r in &cap.requirements {
        let st = panel.status_of(&cap.name, &r.name);
        if panel.only_stale && !st.is_suspect() {
            continue;
        }
        ui.push_id(&r.name, |ui| {
            egui::Frame::none()
                .fill(theme.bg)
                .rounding(egui::Rounding::same(6.0))
                .inner_margin(egui::Margin::symmetric(space::SM, 5.0))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            RichText::new(st.chip())
                                .size(10.5)
                                .monospace()
                                .color(st.color(theme)),
                        )
                        .on_hover_text(if st.detail().is_empty() {
                            st.chip()
                        } else {
                            st.detail()
                        });
                        ui.label(
                            RichText::new(ellipsize(&r.name, 48))
                                .size(12.5)
                                .color(theme.text),
                        )
                        .on_hover_text(&r.name);
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("✅")
                                .on_hover_text(tr("いまのコードで正しいと追認する \
                                         (state.toml の基準を今の HEAD へ)"))
                                .clicked()
                            {
                                *action = SpecAction::Write(WriteReq::Confirm {
                                    capability: cap.name.clone(),
                                    name: r.name.clone(),
                                });
                            }
                            if ui
                                .button("📤")
                                .on_hover_text(tr("この要件をエージェントへ渡す"))
                                .clicked()
                            {
                                *action = SpecAction::Hand(requirement_context(cap, r, &st));
                            }
                            if ui.button("📄").on_hover_text(tr("開く")).clicked() {
                                *action = SpecAction::Open(cap.path.clone());
                            }
                        });
                    });
                    if !r.text.is_empty() {
                        ui.label(
                            RichText::new(ellipsize(&r.text.replace('\n', " "), 160))
                                .size(11.0)
                                .color(theme.text_dim),
                        )
                        .on_hover_text(&r.text);
                    }
                    if !st.detail().is_empty() {
                        ui.label(
                            RichText::new(ellipsize(&st.detail(), 120))
                                .size(10.5)
                                .color(st.color(theme)),
                        )
                        .on_hover_text(st.detail());
                    }
                });
        });
    }
}

fn change_detail(
    ui: &mut egui::Ui,
    theme: &Theme,
    panel: &SpecPanel,
    idx: usize,
    action: &mut SpecAction,
) {
    let ch = &panel.changes[idx];
    ui.horizontal_wrapped(|ui| {
        ui.label(
            RichText::new(&ch.title)
                .size(13.5)
                .strong()
                .color(theme.text),
        );
        let f = ch.footprint();
        ui.label(
            RichText::new(trf(
                "{g}パス · 要件 {r} · シナリオ {s} · ファイル {f}",
                &[
                    ("g", ch.gear.label()),
                    ("r", f.reqs.to_string()),
                    ("s", f.scenarios.to_string()),
                    ("f", f.files.to_string()),
                ],
            ))
            .size(11.0)
            .color(theme.text_dim),
        );
        if ch.gear_mismatch() {
            ui.label(
                RichText::new(tr("⚠ 完全パス相当の大きさです"))
                    .size(11.0)
                    .color(theme.warn),
            )
            .on_hover_text(tr(
                "軽量パスは 1 要件・2 シナリオ・3 ファイルまでを想定しています",
            ));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .button(tr("📦 統合"))
                .on_hover_text(tr(
                    "差分を真実の spec.md へ畳み、この変更を archive へ移します \
                     (衝突があれば何も書きません)",
                ))
                .clicked()
            {
                *action = SpecAction::Write(WriteReq::Archive(ch.id.clone()));
            }
            if ui
                .button("📤")
                .on_hover_text(tr("この差分をエージェントへ渡す"))
                .clicked()
            {
                *action = SpecAction::Hand(change_context(ch));
            }
        });
    });
    if !ch.tasks.is_empty() {
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(tr("タスク")).size(11.0).color(theme.text_dim));
            for t in &ch.tasks {
                let st = panel
                    .state
                    .tasks
                    .iter()
                    .find(|x| x.change == ch.id && &x.id == t)
                    .map(|x| x.status.clone())
                    .unwrap_or_else(|| "todo".into());
                ui.label(
                    RichText::new(format!("{t}:{st}"))
                        .size(10.5)
                        .monospace()
                        .color(if st == "done" {
                            theme.ok
                        } else {
                            theme.text_dim
                        }),
                );
            }
        });
    }
    for (cap, d, path) in &ch.deltas {
        ui.push_id(path, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(trf("capability: {c}", &[("c", cap.clone())]))
                        .size(11.5)
                        .color(theme.accent),
                );
                if ui.button("📄").on_hover_text(tr("差分を開く")).clicked() {
                    *action = SpecAction::Open(path.clone());
                }
            });
            for u in &d.unknown {
                ui.label(
                    RichText::new(trf("解釈できない見出し: {u}", &[("u", u.clone())]))
                        .size(11.0)
                        .color(theme.err),
                );
            }
            if d.is_empty() && d.unknown.is_empty() {
                ui.label(
                    RichText::new(tr("差分がまだ空です"))
                        .size(11.0)
                        .color(theme.text_dim),
                );
            }
            for (verb, list) in [(Verb::Added, &d.added), (Verb::Modified, &d.modified)] {
                for r in list {
                    delta_row(ui, theme, verb, &r.name, &r.text);
                }
            }
            for name in &d.removed {
                delta_row(ui, theme, Verb::Removed, name, "");
            }
        });
    }
}

fn delta_row(ui: &mut egui::Ui, theme: &Theme, verb: Verb, name: &str, text: &str) {
    let col = match verb {
        Verb::Added => theme.ok,
        Verb::Modified => theme.accent,
        Verb::Removed => theme.err,
    };
    egui::Frame::none()
        .fill(theme.bg)
        .rounding(egui::Rounding::same(6.0))
        .inner_margin(egui::Margin::symmetric(space::SM, 4.0))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(verb.chip()).size(10.5).monospace().color(col));
                ui.label(
                    RichText::new(ellipsize(name, 48))
                        .size(12.5)
                        .color(theme.text),
                )
                .on_hover_text(name);
            });
            if !text.is_empty() {
                ui.label(
                    RichText::new(ellipsize(&text.replace('\n', " "), 160))
                        .size(11.0)
                        .color(theme.text_dim),
                )
                .on_hover_text(text);
            }
        });
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn cap_md() -> &'static str {
        "---\ntargets: src/auth/**\n---\n\
         # auth\n\n## Requirements\n\n\
         ### Requirement: Session Expiration\n\
         The system MUST expire sessions after 30 minutes of inactivity.\n\
         [@code] src/session.rs\n\
         [@test] src/session.rs::tests::expiry\n\n\
         #### Scenario: idle user\n\
         - GIVEN a logged-in user\n\
         - WHEN 30 minutes pass\n\
         - THEN the session is rejected\n\n\
         ### Requirement: Remember Me\n\
         The system MAY offer a remember-me checkbox.\n"
    }

    #[test]
    fn 能力を解析して往復できる() {
        let cap = parse_capability("auth", PathBuf::from("x"), cap_md());
        assert_eq!(cap.targets, vec!["src/auth/**"]);
        assert_eq!(cap.requirements.len(), 2);
        let r = &cap.requirements[0];
        assert_eq!(r.name, "Session Expiration");
        assert!(r.text.contains("MUST expire sessions"));
        assert_eq!(r.targets, vec!["src/session.rs"]);
        assert_eq!(r.tests, vec!["src/session.rs::tests::expiry"]);
        assert_eq!(r.scenarios.len(), 1);
        assert_eq!(r.scenarios[0].title, "idle user");
        assert_eq!(r.scenarios[0].steps.len(), 3);
        // 往復: 書き戻して読み直すと同じもの
        let back = parse_capability("auth", PathBuf::from("x"), &render_capability(&cap));
        assert_eq!(back, cap, "render → parse で同じにならない");
    }

    #[test]
    fn 差分の3セクションを解析する() {
        let src = "## ADDED Requirements\n\
                   ### Requirement: Two-Factor Authentication\n\
                   The system MUST support TOTP-based two-factor authentication.\n\
                   #### Scenario: 2FA enrollment\n\
                   - GIVEN a user without 2FA enabled\n\
                   - WHEN the user enables 2FA in settings\n\
                   - THEN a QR code is displayed\n\n\
                   ## MODIFIED Requirements\n\
                   ### Requirement: Session Expiration\n\
                   The system MUST expire sessions after 15 minutes of inactivity.\n\
                   (Previously: 30 minutes)\n\n\
                   ## REMOVED Requirements\n\
                   ### Requirement: Remember Me\n";
        let d = parse_delta(src);
        assert_eq!(d.added.len(), 1);
        assert_eq!(d.added[0].name, "Two-Factor Authentication");
        assert_eq!(d.added[0].scenarios[0].steps.len(), 3);
        assert_eq!(d.modified.len(), 1);
        assert!(d.modified[0].text.contains("15 minutes"));
        assert!(d.modified[0].text.contains("Previously"));
        assert_eq!(d.removed, vec!["Remember Me"]);
        assert!(d.unknown.is_empty());
        assert_eq!(d.touched(), 3);
    }

    #[test]
    fn 壊れた差分でもパニックしない() {
        let cases = [
            "",
            "###",
            "## \n",
            "## ADDED Requirements\n",                 // 中身なし
            "### Requirement:\nname empty\n",          // 名前が空
            "## Whatever\n### Requirement: X\ntext\n", // 未知の見出し
            "---\nnot closed\n## ADDED Requirements\n### Requirement: A\nb\n",
            "#### Scenario: 迷子\n- GIVEN x\n",
        ];
        for src in cases {
            let d = parse_delta(src);
            // 壊れていても「解釈できなかった見出し」として残るか、空になるだけ
            assert!(d.touched() <= 1, "{src:?} → {d:?}");
        }
        let d = parse_delta("## Whatever\n### Requirement: X\ntext\n");
        assert_eq!(d.unknown, vec!["Whatever"], "未知の見出しは捨てない");
        // frontmatter が閉じていなければ本文として読む
        let d = parse_delta("---\nnot closed\n## ADDED Requirements\n### Requirement: A\nb\n");
        assert_eq!(d.added.len(), 1);
    }

    fn req(name: &str, text: &str) -> Requirement {
        Requirement {
            name: name.into(),
            text: text.into(),
            ..Requirement::default()
        }
    }

    #[test]
    fn 差分を真実へ当てる() {
        let mut cap = parse_capability("auth", PathBuf::from("x"), cap_md());
        let d = Delta {
            added: vec![req("Two-Factor Authentication", "MUST support TOTP.")],
            modified: vec![req("Session Expiration", "MUST expire after 15 minutes.")],
            removed: vec!["Remember Me".into()],
            unknown: Vec::new(),
        };
        let rep = apply_delta(&mut cap, &d);
        assert!(rep.is_clean(), "{:?}", rep.conflicts);
        assert!(!rep.retired);
        assert_eq!(cap.requirements.len(), 2);
        assert_eq!(cap.requirements[0].text, "MUST expire after 15 minutes.");
        assert_eq!(cap.requirements[1].name, "Two-Factor Authentication");
        assert!(!cap.requirements.iter().any(|r| r.name == "Remember Me"));
    }

    #[test]
    fn 最後の1件を消したら能力は引退する() {
        let mut cap = Capability {
            name: "tiny".into(),
            requirements: vec![req("Only", "MUST x")],
            ..Capability::default()
        };
        let rep = apply_delta(
            &mut cap,
            &Delta {
                removed: vec!["Only".into()],
                ..Delta::default()
            },
        );
        assert!(rep.retired);
        assert!(cap.requirements.is_empty());
    }

    #[test]
    fn 当てられない差分は衝突として残す() {
        let mut cap = Capability {
            name: "c".into(),
            requirements: vec![req("A", "x")],
            ..Capability::default()
        };
        let rep = apply_delta(
            &mut cap,
            &Delta {
                added: vec![req("A", "重複")],
                modified: vec![req("Nope", "y")],
                removed: vec!["Gone".into()],
                unknown: Vec::new(),
            },
        );
        assert_eq!(rep.conflicts.len(), 3, "{:?}", rep.conflicts);
        assert!(!rep.is_clean());
        // 黙って上書きしていない
        assert_eq!(cap.requirements.len(), 1);
        assert_eq!(cap.requirements[0].text, "x");
    }

    // ── 陳腐化の判定 ──────────────────────────────────────────────

    fn drift(path: &str, meaningful: usize) -> FileDrift {
        FileDrift {
            path: path.into(),
            meaningful,
            moved_from: None,
        }
    }

    #[test]
    fn 陳腐化の規則() {
        // 基準が無い → 判定しない (呼び出し側が Unknown を入れる)
        assert_eq!(assess(false, &[], &[]), Staleness::InSync);
        // 実質変更あり + 仕様は据え置き → 疑い
        match assess(false, &[drift("src/a.rs", 3)], &[]) {
            Staleness::Suspect { files, lines } => {
                assert_eq!(files, vec!["src/a.rs"]);
                assert_eq!(lines, 3);
            }
            other => panic!("{other:?}"),
        }
        // 仕様のほうも動いている → 疑わない (人が手当てした)
        assert_eq!(
            assess(true, &[drift("src/a.rs", 3)], &[]),
            Staleness::SpecAhead
        );
        // 統べる先が無い → リンク切れ
        assert_eq!(
            assess(false, &[], &["src/gone.rs".into()]),
            Staleness::Dangling {
                missing: vec!["src/gone.rs".into()]
            }
        );
    }

    #[test]
    fn 空白だけ_コメントだけの変更は乖離ではない() {
        // (パス, 行, 意味があるか)
        let cases: &[(&str, &str, bool)] = &[
            ("src/a.rs", "    ", false),
            ("src/a.rs", "", false),
            ("src/a.rs", "// コメント", false),
            ("src/a.rs", "/// doc", false),
            ("src/a.rs", "/* block", false),
            ("src/a.rs", " * continued", false),
            ("src/a.rs", " */", false),
            ("src/a.rs", "let x = 1;", true),
            ("src/a.rs", "let s = \"// not a comment\";", true),
            ("run.py", "# コメント", false),
            ("run.py", "x = 1", true),
            ("q.sql", "-- コメント", false),
            ("q.sql", "select 1", true),
            // 判らない拡張子ではコメントを剥がさない (剥がしすぎるほうが危ない)
            ("data.unknownext", "# これは剥がさない", true),
            ("data.unknownext", "   ", false),
        ];
        for (path, line, want) in cases {
            assert_eq!(
                meaningful_line(path, line),
                *want,
                "meaningful_line({path:?}, {line:?})"
            );
        }
    }

    #[test]
    fn 空白だけの差分では疑いを出さない() {
        // 空白だけ / コメントだけの変更行しかないファイル
        let only_noise = FileDrift {
            path: "src/a.rs".into(),
            meaningful: 0,
            moved_from: None,
        };
        assert_eq!(assess(false, &[only_noise], &[]), Staleness::InSync);
    }

    #[test]
    fn 移動しただけのファイルはリンク切れにしない() {
        let moved = FileDrift {
            path: "src/new.rs".into(),
            meaningful: 0,
            moved_from: Some("src/old.rs".into()),
        };
        assert_eq!(
            resolve_missing(&["src/old.rs".into()], &[moved.clone()]),
            Vec::<String>::new()
        );
        assert_eq!(
            assess(false, &[moved], &["src/old.rs".into()]),
            Staleness::InSync,
            "移動だけで陳腐化にもリンク切れにもしない"
        );
        // 本当に消えたものは残す
        assert_eq!(
            resolve_missing(&["src/gone.rs".into()], &[]),
            vec!["src/gone.rs".to_string()]
        );
    }

    #[test]
    fn 実際の_git_diff_出力から乖離を測る() {
        // -U0 の unified diff (空白だけの行と本物の変更が混ざる)
        let text = "diff --git a/src/a.rs b/src/a.rs\n\
                    index 111..222 100644\n\
                    --- a/src/a.rs\n\
                    +++ b/src/a.rs\n\
                    @@ -1,1 +1,2 @@\n\
                    -// 古いコメント\n\
                    +// 新しいコメント\n\
                    +let x = 1;\n";
        let d = drifts_of(text);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].path, "src/a.rs");
        assert_eq!(d[0].meaningful, 1, "コメント 2 行は数えない");
    }

    // ── ギア ──────────────────────────────────────────────────────

    #[test]
    fn ギアは差分の大きさで決まる() {
        let cases: &[(Footprint, Gear)] = &[
            // 1 要件 1 シナリオ 1 ファイル → 軽量
            (
                Footprint {
                    reqs: 1,
                    scenarios: 1,
                    removes: 0,
                    files: 1,
                },
                Gear::Light,
            ),
            // 境界 (1 / 2 / 3) はまだ軽量
            (
                Footprint {
                    reqs: 1,
                    scenarios: 2,
                    removes: 0,
                    files: 3,
                },
                Gear::Light,
            ),
            // 要件が 2 つ → 完全
            (
                Footprint {
                    reqs: 2,
                    scenarios: 1,
                    removes: 0,
                    files: 1,
                },
                Gear::Full,
            ),
            // REMOVED は常に完全 (後方互換を壊す)
            (
                Footprint {
                    reqs: 1,
                    scenarios: 0,
                    removes: 1,
                    files: 1,
                },
                Gear::Full,
            ),
            // ファイルが 4 枚 → 完全
            (
                Footprint {
                    reqs: 1,
                    scenarios: 1,
                    removes: 0,
                    files: 4,
                },
                Gear::Full,
            ),
            // 空の差分は軽量 (儀式を作らない)
            (Footprint::default(), Gear::Light),
        ];
        for (f, want) in cases {
            assert_eq!(suggest_gear(*f), *want, "{f:?}");
        }
    }

    #[test]
    fn 差分から足跡を測る() {
        let d = parse_delta(
            "## ADDED Requirements\n\
             ### Requirement: A\n\
             MUST a\n\
             [@code] src/a.rs\n\
             [@test] src/a.rs::tests::x\n\
             #### Scenario: s1\n- GIVEN g\n",
        );
        let f = footprint(&d);
        assert_eq!(f.reqs, 1);
        assert_eq!(f.scenarios, 1);
        assert_eq!(f.removes, 0);
        // `src/a.rs` と `src/a.rs::tests::x` は同じファイル → 1 枚
        assert_eq!(f.files, 1);
        assert_eq!(suggest_gear(f), Gear::Light);
    }

    // ── state.toml ────────────────────────────────────────────────

    fn tiny_caps() -> Vec<Capability> {
        vec![Capability {
            name: "auth".into(),
            path: PathBuf::from("auth/spec.md"),
            targets: vec![],
            requirements: vec![req("A", "MUST a"), req("B", "MUST b")],
        }]
    }

    fn tiny_changes() -> Vec<Change> {
        vec![Change {
            id: "add-2fa".into(),
            tasks: vec!["impl".into()],
            ..Change::default()
        }]
    }

    #[test]
    fn state_の突き合わせ報告() {
        let caps = tiny_caps();
        let changes = tiny_changes();
        // 空の state → 全部「新規」
        let rep = drift_report(&State::default(), &caps, &changes);
        assert!(rep.in_sync.is_empty());
        assert_eq!(rep.new_entries.len(), 3, "{rep:?}");
        assert!(rep.orphans.is_empty());
        assert!(rep.dirty());

        // 組み直したら一致する
        let st = sync_state(&State::default(), &caps, &changes, &BTreeMap::new());
        let rep = drift_report(&st, &caps, &changes);
        assert_eq!(rep.in_sync.len(), 3);
        assert!(!rep.dirty(), "{rep:?}");

        // 孤児と語彙外
        let mut st2 = st.clone();
        st2.requirements.push(ReqState {
            capability: "auth".into(),
            name: "消えた要件".into(),
            status: "in-sync".into(),
            ..ReqState::default()
        });
        st2.tasks[0].status = "ぐるぐる".into();
        let rep = drift_report(&st2, &caps, &changes);
        assert_eq!(rep.orphans, vec!["auth/消えた要件".to_string()]);
        assert_eq!(rep.illegal.len(), 1);
        assert_eq!(rep.illegal[0].0, "add-2fa#impl");
    }

    #[test]
    fn state_は基準を引き継いで組み直す() {
        let caps = tiny_caps();
        let changes = tiny_changes();
        let mut st = sync_state(&State::default(), &caps, &changes, &BTreeMap::new());
        st.requirements[0].baseline = "deadbeef".into();
        st.requirements[0].status = "in-sync".into();
        // 要件が 1 つ消えた実体で組み直す
        let mut caps2 = caps.clone();
        caps2[0].requirements.pop();
        let st2 = sync_state(&st, &caps2, &changes, &BTreeMap::new());
        assert_eq!(st2.requirements.len(), 1);
        assert_eq!(st2.requirements[0].baseline, "deadbeef", "基準を失わない");
        assert_eq!(st2.requirements[0].status, "in-sync");
    }

    #[test]
    fn 判定した状態を_state_へ書き戻す() {
        let caps = tiny_caps();
        let statuses: BTreeMap<String, String> = [(
            "auth/A".to_string(),
            Staleness::Suspect {
                files: vec!["src/a.rs".into()],
                lines: 2,
            }
            .key()
            .to_string(),
        )]
        .into_iter()
        .collect();
        let st = sync_state(&State::default(), &caps, &[], &statuses);
        assert_eq!(st.requirements[0].status, "suspect");
        // 判定が無い要件は勝手に動かさない
        assert_eq!(st.requirements[1].status, "unknown");
        assert!(validate_state(&st).is_ok());
        // 語彙は全部 `key()` から出る (state.toml に書ける値しか無い)
        for st in [
            Staleness::Unknown,
            Staleness::InSync,
            Staleness::SpecAhead,
            Staleness::Suspect {
                files: vec![],
                lines: 1,
            },
            Staleness::Dangling { missing: vec![] },
        ] {
            assert!(REQ_STATUSES.contains(&st.key()), "{:?}", st.key());
        }
    }

    #[test]
    fn state_の検証は語彙外を弾く() {
        let mut st = sync_state(
            &State::default(),
            &tiny_caps(),
            &tiny_changes(),
            &BTreeMap::new(),
        );
        assert!(validate_state(&st).is_ok());
        st.requirements[0].status = "しらない".into();
        let e = validate_state(&st).expect_err("弾かれるはず");
        assert!(e.iter().any(|m| m.contains("語彙外")), "{e:?}");
        // 版が範囲外
        let bad = State {
            version: STATE_VERSION + 1,
            ..State::default()
        };
        assert!(validate_state(&bad).is_err());
    }

    #[test]
    fn state_の書き込みは原子的で失敗したら元のまま() {
        let dir = unique_temp_dir("zaivern-spec-test", "atomic");
        let path = dir.join(STATE_FILE);
        let good = sync_state(
            &State::default(),
            &tiny_caps(),
            &tiny_changes(),
            &BTreeMap::new(),
        );
        write_state_atomic(&path, &good).expect("最初の書き込み");
        let before = std::fs::read_to_string(&path).expect("読める");

        // 語彙外の値で書こうとする → 弾かれ、ファイルは 1 バイトも変わらない
        let mut bad = good.clone();
        bad.requirements[0].status = "ぐちゃぐちゃ".into();
        assert!(write_state_atomic(&path, &bad).is_err());
        let after = std::fs::read_to_string(&path).expect("読める");
        assert_eq!(before, after, "失敗したのに書き換わっている");

        // 一時ファイルを置き去りにしない
        let leftovers: Vec<String> = std::fs::read_dir(&dir)
            .expect("read_dir")
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");

        // 読み戻すと同じもの
        let back = migrate_state(&after).expect("読める");
        assert_eq!(back, good);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn state_の形式版を引き上げる() {
        // 版が無い (= 版が付く前) → 今の版として読み、既定値を埋める
        let raw = "[[requirement]]\ncapability = \"auth\"\nname = \"A\"\n";
        let st = migrate_state(raw).expect("読める");
        assert_eq!(st.version, STATE_VERSION);
        assert_eq!(st.requirements[0].status, "unknown");
        assert!(validate_state(&st).is_ok());

        // 未来の版は読まない (古いアプリが新しい書式を潰さない)
        let future = format!("version = {}\n", STATE_VERSION + 1);
        assert!(migrate_state(&future).is_err());

        // 壊れた TOML はエラー (panic しない)
        assert!(migrate_state("[[requirement\n").is_err());
    }

    // ── レイアウト ────────────────────────────────────────────────

    #[test]
    fn レイアウトはどの幅でも収まり重ならない() {
        let sizes = [
            (900.0_f32, 700.0_f32),
            (1200.0, 300.0),
            (520.0, 700.0),
            (320.0, 180.0),
            (0.0, 0.0),
        ];
        for (w, h) in sizes {
            let avail = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(w, h));
            let l = panel_layout(avail);
            for (name, r) in [("list", l.list), ("detail", l.detail)] {
                assert!(
                    avail.contains_rect(r),
                    "{name} が可用領域から出ている ({w}x{h}): {r:?} vs {avail:?}"
                );
                assert!(r.width() >= 0.0 && r.height() >= 0.0);
            }
            let inter = l.list.intersect(l.detail);
            assert!(
                inter.width() <= 0.0 || inter.height() <= 0.0,
                "2 枚が重なっている ({w}x{h}): {inter:?}"
            );
            // 空状態カードも必ず中に入る
            let card = empty_card(avail);
            assert!(
                avail.contains_rect(card),
                "空状態カードがはみ出す ({w}x{h}): {card:?}"
            );
        }
        // 広いところは左右、狭いところは上下
        let wide = panel_layout(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(1200.0, 300.0),
        ));
        assert!(!wide.stacked);
        let narrow = panel_layout(egui::Rect::from_min_size(
            egui::pos2(0.0, 0.0),
            egui::vec2(520.0, 700.0),
        ));
        assert!(narrow.stacked);
    }

    // ── パス導出 (ハードコードしない) ─────────────────────────────

    #[test]
    fn spec_ルートはワークスペースから導く() {
        let dir = unique_temp_dir("zaivern-spec-test", "root");
        // 何も無ければ既定 (spec)
        assert_eq!(spec_root(&dir), dir.join("spec"));
        // openspec があればそちらを読む (OpenSpec 利用者がそのまま乗れる)
        std::fs::create_dir_all(dir.join("openspec")).expect("mkdir");
        assert_eq!(spec_root(&dir), dir.join("openspec"));
        // spec が優先
        std::fs::create_dir_all(dir.join("spec")).expect("mkdir");
        assert_eq!(spec_root(&dir), dir.join("spec"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 変更_id_は区切り文字を作れない() {
        let cases: &[(&str, &str)] = &[
            ("Add 2FA", "add-2fa"),
            ("../../etc/passwd", "etc-passwd"),
            ("  hello  ", "hello"),
            ("a//b", "a-b"),
            ("---", ""),
            ("日本語のID", "日本語のid"),
        ];
        for (raw, want) in cases {
            let got = slug(raw);
            assert_eq!(&got, want, "slug({raw:?})");
            assert!(!got.contains('/') && !got.contains('\\') && !got.contains(".."));
        }
    }

    #[test]
    fn 対象のパスは記号を落として取れる() {
        assert_eq!(target_path("src/a.rs::tests::x"), "src/a.rs");
        assert_eq!(target_path(" src/**/*.rs "), "src/**/*.rs");
    }

    #[test]
    fn glob_と実パスを実体へ当てる() {
        let files: Vec<String> = ["src/a.rs", "src/deep/b.rs", "docs/c.md"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let (hit, missing) = resolve_targets(
            &[
                "src/**/*.rs".into(),
                "docs/c.md".into(),
                "src/nope.rs".into(),
                // 1 枚も当たらない glob。**リンク切れに数えない**
                // (まだ書き始めていないだけかもしれない)
                "src/future/**".into(),
            ],
            &files,
        );
        assert_eq!(hit, vec!["src/a.rs", "src/deep/b.rs", "docs/c.md"]);
        assert_eq!(
            missing,
            vec!["src/nope.rs"],
            "実パスだけがリンク切れ (glob は絞り込みであってリンクではない)"
        );
    }

    #[test]
    fn 指紋は空白の揺れを無視して名前と本文で決まる() {
        let a = req("A", "The system MUST x.");
        let b = req("A", "  The system MUST x.  \n\n");
        assert_eq!(a.fingerprint(), b.fingerprint());
        let c = req("A", "The system MUST y.");
        assert_ne!(a.fingerprint(), c.fingerprint());
        // **値そのものを固定する。** `DefaultHasher` は Rust の版で値が変わるが、
        // この指紋は state.toml に書いて別のマシンと突き合わせるので、
        // 未来永劫同じでなければならない (FNV-1a 64 の既知の値)。
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn 疑いだけに絞る操作は空振りしない() {
        let mut panel = SpecPanel {
            caps: tiny_caps(),
            scanned: true,
            ..SpecPanel::default()
        };
        // 疑いが 1 件も無いなら**何も変えない** (空の画面を作らない)
        panel.focus_stale();
        assert!(!panel.only_stale);
        assert_eq!(panel.selected, None);

        // 疑いがあれば、その能力へ選択を移して絞り込む
        panel.status.insert(
            "auth/B".into(),
            Staleness::Suspect {
                files: vec!["src/b.rs".into()],
                lines: 1,
            },
        );
        assert_eq!(panel.stale_count(), 1);
        assert_eq!(panel.badge(), Some(1));
        panel.focus_stale();
        assert!(panel.only_stale);
        assert_eq!(panel.selected, Some(Sel::Capability("auth".into())));

        // 疑いが 0 件ならバッジは出さない (常に 0 のバッジを作らない)
        panel.status.clear();
        assert_eq!(panel.badge(), None);
    }

    // ── 描画スレッドで git を待たない (番人) ──────────────────────

    #[test]
    fn 走査は裏のスレッドで行い描画では待たない() {
        let src = include_str!("spec.rs").replace("\r\n", "\n");
        let body = src
            .split("pub fn poll(&mut self, workspace: &Path) {")
            .nth(1)
            .expect("poll がある");
        let body = body.split("\n    }\n").next().expect("本体の終端");
        assert!(
            body.contains("std::thread::Builder::new()"),
            "poll が走査を裏のスレッドへ逃がしていない"
        );
        assert!(
            body.contains("try_recv()") && !body.contains(".recv()"),
            "poll が受信を待っている (描画が止まる)"
        );
        assert!(
            body.contains("crate::git::scan_interval"),
            "適応的な間隔を使っていない (遅いリポジトリで git が常時走る)"
        );
        // UI 側 (`ui` / `list_ui` / `detail_ui`) が I/O を撃っていないこと
        for name in ["pub fn ui(", "fn list_ui(", "fn detail_ui("] {
            let b = src.split(name).nth(1).expect(name);
            let b = b.split("\n}\n").next().expect("本体の終端");
            for needle in ["std::fs::", "run_git_at", "Command::new"] {
                assert!(!b.contains(needle), "{name} が {needle} を呼んでいる");
            }
        }
    }

    #[test]
    fn app_はパネルを出している間だけ走査する() {
        let src = include_str!("app.rs").replace("\r\n", "\n");
        assert!(
            src.contains("if self.spec_view {\n            self.spec.poll(")
                || src.contains("if self.spec_view {"),
            "app.rs が spec_view で囲って poll していない"
        );
        for needle in [
            "spec_action = spec::ui(ui, &theme, &mut self.spec);",
            "self.apply_spec_action(spec_action);",
        ] {
            assert!(src.contains(needle), "app.rs に {needle} が無い");
        }
    }

    /// **到達経路がレジストリ経由で生きている。**
    ///
    /// 以前は `app.rs` に `Cmd::OpenSpec => self.open_spec_panel(),` が
    /// 直書きされていたが、並列開発で `app.rs` を奪い合わないために
    /// [`crate::feature`] のレジストリへ移した (経緯は `feature.rs` の冒頭)。
    /// 「UI から到達できない実装は未完成」なので、経路が消えていないことを
    /// ここで固定する。ソースではなく**レジストリの実体**を見るので、
    /// 配線が変わっても壊れない。
    #[test]
    fn パレットからの到達経路がレジストリに登録されている() {
        assert_eq!(crate::features::spec::FEATURE.module, "spec");
        let ids: Vec<&str> = crate::features::spec::FEATURE
            .entries
            .iter()
            .map(|e| e.id)
            .collect();
        assert!(ids.contains(&"spec.open"), "spec.open が無い: {ids:?}");
        assert!(ids.contains(&"spec.stale"), "spec.stale が無い: {ids:?}");
        // レジストリ本体に載っていなければパレットに出ない
        assert!(
            crate::feature::REGISTRY
                .iter()
                .any(|f| f.module == crate::features::spec::FEATURE.module),
            "feature::REGISTRY に spec が登録されていない (統合担当が 1 行足す)"
        );
        // app.rs 側の受け口 (メソッド) が残っていること
        let src = include_str!("app.rs").replace("\r\n", "\n");
        for needle in [
            "pub(crate) fn open_spec_panel(&mut self)",
            "pub(crate) fn open_spec_stale(&mut self)",
        ] {
            assert!(src.contains(needle), "app.rs に {needle} が無い");
        }
    }

    // ── 実物のリポジトリで端から端まで ───────────────────────────

    fn git(dir: &Path, args: &[&str]) -> String {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        crate::git::run_git_at(dir, &owned).unwrap_or_default()
    }

    /// 実リポジトリを作る。git が無い環境では `None` (テストを緑のまま飛ばす)。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        let dir = unique_temp_dir("zaivern-spec-test", tag);
        if crate::git::run_git_at(&dir, &["init".to_string()]).is_err() {
            std::fs::remove_dir_all(&dir).ok();
            return None;
        }
        git(&dir, &["config", "user.email", "t@example.invalid"]);
        git(&dir, &["config", "user.name", "t"]);
        git(&dir, &["config", "commit.gpgsign", "false"]);
        Some(dir)
    }

    #[test]
    fn 実リポジトリでコードだけ動いたら疑いが出る() {
        let Some(repo) = temp_repo("e2e") else {
            return;
        };
        // 統べられるコードとテスト
        std::fs::create_dir_all(repo.join("src")).expect("mkdir");
        std::fs::write(
            repo.join("src/session.rs"),
            "// セッション\npub const TTL_MIN: u32 = 30;\n",
        )
        .expect("write");
        // 仕様
        let sdir = repo.join("spec/specs/auth");
        std::fs::create_dir_all(&sdir).expect("mkdir");
        std::fs::write(sdir.join(SPEC_FILE), cap_md()).expect("write");
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "init"]);
        let head = head_sha(&repo).expect("HEAD");

        // 基準を記録する (= 「いまは合っている」)
        let caps = load_capabilities(&spec_root(&repo));
        let mut st = sync_state(&State::default(), &caps, &[], &BTreeMap::new());
        for r in &mut st.requirements {
            r.baseline = head.clone();
            r.status = "in-sync".into();
        }
        write_state_atomic(&spec_root(&repo).join(STATE_FILE), &st).expect("write state");

        // ① まだ何も変えていない → 同期
        let s = scan(&repo);
        assert_eq!(
            s.status.get("auth/Session Expiration"),
            Some(&Staleness::InSync),
            "{:?}",
            s.status
        );

        // ② 空白とコメントだけ変える → **疑いを出さない**
        std::fs::write(
            repo.join("src/session.rs"),
            "// セッション (コメントを書き換えた)\n\npub const TTL_MIN: u32 = 30;\n   \n",
        )
        .expect("write");
        let s = scan(&repo);
        assert_eq!(
            s.status.get("auth/Session Expiration"),
            Some(&Staleness::InSync),
            "空白/コメントだけで狼少年になっている: {:?}",
            s.status
        );

        // ③ 本物のコードを変えて仕様は据え置き → 疑い
        std::fs::write(
            repo.join("src/session.rs"),
            "// セッション\npub const TTL_MIN: u32 = 15;\n",
        )
        .expect("write");
        let s = scan(&repo);
        match s.status.get("auth/Session Expiration") {
            Some(Staleness::Suspect { files, lines }) => {
                assert!(files.iter().any(|f| f.ends_with("session.rs")), "{files:?}");
                assert!(*lines >= 1);
            }
            other => panic!("疑いが出ていない: {other:?}"),
        }

        // ④ 仕様の文も直した → もう疑わない
        let updated = cap_md().replace("30 minutes of inactivity", "15 minutes of inactivity");
        std::fs::write(sdir.join(SPEC_FILE), &updated).expect("write");
        let s = scan(&repo);
        assert_eq!(
            s.status.get("auth/Session Expiration"),
            Some(&Staleness::SpecAhead),
            "{:?}",
            s.status
        );

        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn 骨組みを作って統合するまで通る() {
        let Some(repo) = temp_repo("archive") else {
            return;
        };
        // 軽量パスで変更を起こす
        let msg = apply_write(
            WriteReq::Scaffold {
                id: "Add 2FA".into(),
                capability: "auth".into(),
                gear: Gear::Light,
            },
            &repo,
        )
        .expect("scaffold");
        assert!(msg.contains("add-2fa"), "{msg}");
        let root = spec_root(&repo);
        let delta_path = root.join("changes/add-2fa/deltas/auth.md");
        assert!(delta_path.is_file(), "差分が 1 枚だけ作られる");
        assert!(
            !root.join("changes/add-2fa/design.md").exists(),
            "軽量パスで三部作を作らない"
        );

        // 差分を本物の内容へ書き換えてから統合する
        std::fs::write(
            &delta_path,
            "## ADDED Requirements\n\
             ### Requirement: Two-Factor Authentication\n\
             The system MUST support TOTP-based two-factor authentication.\n\
             [@code] src/auth.rs\n",
        )
        .expect("write");
        let msg = apply_write(WriteReq::Archive("add-2fa".into()), &repo).expect("archive");
        assert!(msg.contains("＋1"), "{msg}");
        let spec_path = root.join("specs/auth").join(SPEC_FILE);
        let text = std::fs::read_to_string(&spec_path).expect("真実が書かれている");
        assert!(text.contains("Two-Factor Authentication"), "{text}");
        assert!(root.join("changes/archive/add-2fa").is_dir());
        assert!(!root.join("changes/add-2fa").exists());

        // 同じ差分をもう一度当てようとしたら衝突で止まる (半分書かない)
        std::fs::create_dir_all(root.join("changes/again/deltas")).expect("mkdir");
        std::fs::write(
            root.join("changes/again/deltas/auth.md"),
            "## ADDED Requirements\n### Requirement: Two-Factor Authentication\nMUST x\n",
        )
        .expect("write");
        let before = std::fs::read_to_string(&spec_path).expect("read");
        let e = apply_write(WriteReq::Archive("again".into()), &repo).expect_err("衝突するはず");
        assert!(e.contains("衝突"), "{e}");
        assert_eq!(
            before,
            std::fs::read_to_string(&spec_path).expect("read"),
            "衝突したのに書き換わっている"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    #[test]
    fn エージェントへ渡す文脈に根拠が入る() {
        let cap = parse_capability("auth", PathBuf::from("x"), cap_md());
        let st = Staleness::Suspect {
            files: vec!["src/session.rs".into()],
            lines: 2,
        };
        let ctx = requirement_context(&cap, &cap.requirements[0], &st);
        assert!(ctx.contains("Session Expiration"));
        assert!(ctx.contains("src/session.rs"));
        assert!(ctx.contains("陳腐化の疑い"), "{ctx}");
        // 疑いが無ければ注意書きは付かない
        let ctx = requirement_context(&cap, &cap.requirements[0], &Staleness::InSync);
        assert!(!ctx.contains("陳腐化の疑い"));

        let ch = Change {
            id: "add-2fa".into(),
            gear: Gear::Light,
            deltas: vec![(
                "auth".into(),
                parse_delta(
                    "## ADDED Requirements\n### Requirement: TOTP\nMUST totp\n[@code] src/a.rs\n",
                ),
                PathBuf::from("d.md"),
            )],
            ..Change::default()
        };
        let ctx = change_context(&ch);
        assert!(ctx.contains("ADDED Requirement: TOTP"), "{ctx}");
        assert!(ctx.contains("src/a.rs"));
    }
}
