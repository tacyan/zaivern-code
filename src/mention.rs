//! `@` によるコンテキスト参照 — **決定的な**コンテキスト添付。
//!
//! ## なぜ要るのか
//!
//! 競合 (Cursor) は 2.0 で `@Code` / `@Definitions` / `@Lint Errors` などを
//! **削除**し、「エージェントが自分で集めるから手で添付しなくてよい」と説明した。
//! そこで失われたのは**決定性**である。モデルが「たぶんこれだろう」と探しに行く
//! 経路は、当たることもあれば外れることもあり、外れたぶんだけトークンを焼く。
//!
//! このモジュールの約束はただ一つ:
//!
//! > **選んだものが、そのままエージェントへ渡る。** 間にモデルの裁量を挟まない。
//!
//! そのために守っている設計上の決まり:
//!
//! * **黙って切らない。** 検索結果も本文も上限を持つが、切ったら必ず
//!   [`gap_marker`] を**画面と挿入テキストの両方**へ残す
//!   (CLAUDE.md 設計原則 2「捨てた箇所には明示的なギャップ標識を入れる」)。
//! * **挿す前に解決先を見せる。** `@parse_osc` が `src/terminal.rs:1447-1470` の
//!   どこに解決されたのかを、確定する前に一覧へ出す。
//! * **1 件ごとのコストを出す。** 文字数と概算トークン数を添付ごとに表示する。
//!   文脈肥大 (context bloat) はこの製品分野で最も放置されている不満で、
//!   1 件ごとの費用を出している競合は無い。
//! * **階層で降りる。** `@app/` と打てば `app` の**直下**が出る。Cursor は
//!   ここが平坦な fuzzy 検索で、同社スタッフ自身が弱点と認めている。
//! * **UI スレッドを止めない。** ファイル走査・git・本文の読み出しはすべて
//!   裏のスレッドへ逃がし、UI は**いま手元にある値**を描く。
//!
//! ## 全体の流れ
//!
//! ```text
//!   本文 + キャレット ──parse_query──▶ Query ──candidates──▶ Ranked
//!                                                              │ Enter
//!                                        apply_pick ◀──────────┘
//!                                            │
//!                                            ├─ ディレクトリなら「降りる」だけ
//!                                            └─ 確定なら Ledger へ Attachment を積む
//!                                                        │ 送信時
//!                                                    expand()
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::fuzzy::PreparedQuery;
use crate::i18n::{tr, trf};

// ---------------------------------------------------------------------------
// 種別のカタログ
// ---------------------------------------------------------------------------

/// 参照できるものの種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Kind {
    /// シンボル (関数・型・クラス)。**Cursor が消して最も惜しまれたもの**。
    Symbol,
    /// ファイル 1 本。
    File,
    /// フォルダ (選ぶと降りる / Tab でその階層ごと添付)。
    Folder,
    /// 端末の末尾出力。
    Terminal,
    /// 差分 (作業ツリー / ブランチ比較)。
    Diff,
    /// いまの診断 (Cursor が `@Lint Errors` として消したもの)。
    Problems,
}

/// 種別ごとの静的メタデータ。**名前空間・アイコン・並び順の唯一の出所**。
struct KindMeta {
    kind: Kind,
    /// `@ns:` で明示するときの名前空間。
    ns: &'static str,
    icon: &'static str,
    /// 名前空間を指定しないときの優先順位 (小さいほど上)。
    rank: i32,
}

/// 種別カタログ。順序がそのまま既定の優先順位になる。
const KINDS: &[KindMeta] = &[
    KindMeta {
        kind: Kind::Symbol,
        ns: "sym",
        icon: "◈",
        rank: 0,
    },
    KindMeta {
        kind: Kind::File,
        ns: "file",
        icon: "📄",
        rank: 1,
    },
    KindMeta {
        kind: Kind::Folder,
        ns: "dir",
        icon: "📁",
        rank: 2,
    },
    KindMeta {
        kind: Kind::Terminal,
        ns: "term",
        icon: "▤",
        rank: 3,
    },
    KindMeta {
        kind: Kind::Diff,
        ns: "diff",
        icon: "±",
        rank: 4,
    },
    KindMeta {
        kind: Kind::Problems,
        ns: "prob",
        icon: "⚠",
        rank: 5,
    },
];

fn meta(kind: Kind) -> &'static KindMeta {
    // カタログは全種を必ず持つ (テスト `種別カタログは全種を覆う` が番人)。
    KINDS
        .iter()
        .find(|m| m.kind == kind)
        .unwrap_or(&KINDS[0])
}

impl Kind {
    /// 一覧に出すアイコン。
    pub fn icon(self) -> &'static str {
        meta(self).icon
    }

    /// `@ns:` の名前空間 (そのまま挿入テキストにも出る)。
    pub fn ns(self) -> &'static str {
        meta(self).ns
    }

    /// 並び順 (小さいほど上)。
    fn rank(self) -> i32 {
        meta(self).rank
    }

    /// 人が読むラベル。
    pub fn label(self) -> String {
        match self {
            Kind::Symbol => tr("シンボル"),
            Kind::File => tr("ファイル"),
            Kind::Folder => tr("フォルダ"),
            Kind::Terminal => tr("端末"),
            Kind::Diff => tr("差分"),
            Kind::Problems => tr("診断"),
        }
    }
}

// ---------------------------------------------------------------------------
// クエリの切り出し (純粋関数)
// ---------------------------------------------------------------------------

/// `@` の後ろに許す最大文字数。これを超えたら「もう `@` ではない」と見なす。
///
/// 打ち切りではなく**起動判定**の窓なので、ここで検索結果が減ることはない
/// (Cursor の「50 文字で黙って検索が止まる」とは別物)。
pub const MAX_TERM_CHARS: usize = 160;

/// 本文から切り出した `@` クエリ。位置は**文字**単位 (バイトではない)。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Query {
    /// `@` そのものの文字位置。
    pub at: usize,
    /// キャレットの文字位置。置換範囲は `at..caret`。
    pub caret: usize,
    /// `@` の直後からキャレットまでの生文字列 (名前空間を含む)。
    pub raw: String,
    /// 明示された名前空間。無指定なら `None` (全種を混ぜる)。
    pub ns: Option<Kind>,
    /// 名前空間を除いた検索語。
    pub term: String,
}

/// `@` の左に来てよい文字か。**単語の途中の `@` では起動しない**
/// (`foo@example.com` を候補一覧で潰さないため)。
fn is_open_boundary(c: char) -> bool {
    c.is_whitespace() || matches!(c, '(' | '[' | '{' | '<' | '"' | '\'' | '`' | ',' | ';')
}

/// 本文とキャレット位置から `@` クエリを切り出す。
///
/// 起動しない条件 (どれも表でテストしてある):
///
/// * キャレットまでの間に空白がある (`@foo bar` の `bar` の後ろ)
/// * `@` の左が単語文字 (`foo@bar`, メールアドレス)
/// * `@` の左がもう一つの `@` (`@@` はエスケープ)
/// * `@` からキャレットまでが [`MAX_TERM_CHARS`] を超える
pub fn parse_query(text: &str, caret: usize) -> Option<Query> {
    let chars: Vec<char> = text.chars().collect();
    let caret = caret.min(chars.len());
    let mut i = caret;
    let mut steps = 0usize;
    let at = loop {
        if i == 0 {
            return None;
        }
        i -= 1;
        let c = chars[i];
        if c == '@' {
            break i;
        }
        if c.is_whitespace() {
            return None;
        }
        steps += 1;
        if steps > MAX_TERM_CHARS {
            return None;
        }
    };
    if at > 0 {
        let prev = chars[at - 1];
        // `@@` は打ち消し。`foo@` も起動しない。
        if prev == '@' || !is_open_boundary(prev) {
            return None;
        }
    }
    let raw: String = chars[at + 1..caret].iter().collect();
    let (ns, term) = split_ns(&raw);
    let term = term.to_string();
    Some(Query {
        at,
        caret,
        raw,
        ns,
        term,
    })
}

/// `sym:foo` → `(Some(Symbol), "foo")`。表に無い頭は名前空間ではない
/// (Windows の `C:/…` を名前空間と読み違えないため、表引きだけで判断する)。
fn split_ns(raw: &str) -> (Option<Kind>, &str) {
    if let Some((head, rest)) = raw.split_once(':') {
        if let Some(k) = KINDS.iter().find(|m| m.ns == head).map(|m| m.kind) {
            return (Some(k), rest);
        }
    }
    (None, raw)
}

/// 検索語を「確定したディレクトリ部分」と「その中での絞り込み語」に割る。
///
/// `"app/ui"` → `("app/", "ui")` / `"app/"` → `("app/", "")` / `"ui"` → `("", "ui")`。
/// 区切りは `/` と `\` の両方を受ける (Windows で打った区切りも通す)。
/// 返すディレクトリ部分は**末尾の区切りを含む**。
pub fn split_dir(term: &str) -> (&str, &str) {
    match term.rfind(['/', '\\']) {
        Some(i) => (&term[..=i], &term[i + 1..]),
        None => ("", term),
    }
}

/// 表示・保存に使う正規形へ直す (区切りは常に `/`、末尾は `/` 1 個)。
fn normalize_dir(dir: &str) -> String {
    let d = dir.replace('\\', "/");
    let d = d.trim_start_matches('/').to_string();
    if d.is_empty() || d.ends_with('/') {
        d
    } else {
        format!("{d}/")
    }
}

// ---------------------------------------------------------------------------
// 階層ブラウズ (純粋関数)
// ---------------------------------------------------------------------------

/// あるディレクトリの直下 1 段。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Child {
    /// 直下の名前 (ディレクトリでも末尾に `/` は付けない)。
    pub name: String,
    pub is_dir: bool,
    /// ディレクトリのとき、その配下にある索引済みファイル数。
    pub count: usize,
}

/// 索引 (スラッシュ区切りの相対パス一覧) から `dir` の**直下だけ**を数え上げる。
///
/// Cursor の `@` は平坦な fuzzy 検索で、`@app/` と打っても app の中身が
/// 出てこない (「hierarchical path browser ではない」と同社スタッフが明言)。
/// ここは**必ず直下だけ**を返し、フォルダを先に置く。
pub fn children_of(rels: &[String], dir: &str) -> Vec<Child> {
    let dir = normalize_dir(dir);
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    let mut files: Vec<String> = Vec::new();
    for rel in rels {
        let Some(rest) = rel.strip_prefix(dir.as_str()) else {
            continue;
        };
        if rest.is_empty() {
            continue;
        }
        match rest.find('/') {
            Some(i) => *dirs.entry(rest[..i].to_string()).or_default() += 1,
            None => files.push(rest.to_string()),
        }
    }
    files.sort();
    files.dedup();
    let mut out: Vec<Child> = dirs
        .into_iter()
        .map(|(name, count)| Child {
            name,
            is_dir: true,
            count,
        })
        .collect();
    out.extend(files.into_iter().map(|name| Child {
        name,
        is_dir: false,
        count: 0,
    }));
    out
}

// ---------------------------------------------------------------------------
// 候補と順位付け
// ---------------------------------------------------------------------------

/// 解決先の実体。**ここに書かれたものがそのままエージェントへ渡る**。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Target {
    /// ファイル全体 (ワークスペース相対、スラッシュ区切り)。
    File { rel: String },
    /// ディレクトリ。Enter では**降りるだけ**で確定しない。
    Folder { rel: String },
    /// シンボル。行は 0 起点・両端を含む。
    Symbol {
        rel: String,
        start_line: usize,
        end_line: usize,
        /// LSP が出した範囲か (false は本文走査の近似)。
        exact: bool,
    },
    /// 端末の末尾出力。
    Terminal { id: u64, title: String },
    /// 差分。
    Diff(DiffScope),
    /// いまの診断すべて。
    Problems,
}

/// どの差分を取るか。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum DiffScope {
    /// 作業ツリー (HEAD との差)。
    WorkTree,
    /// ブランチ比較。`rev` には `main...HEAD` のような**三点記法**を入れる。
    /// マージベースは git 自身が解決するので、こちらでは計算しない
    /// (リポジトリの既定ブランチ名を推測しないで済む)。
    Branch {
        rev: String,
        /// 印に使う短い名前 (空白を含まないこと)。
        name: String,
    },
}

impl DiffScope {
    fn slug(&self) -> String {
        match self {
            DiffScope::WorkTree => "worktree".to_string(),
            DiffScope::Branch { name, .. } => name.clone(),
        }
    }
}

/// 「上流ブランチとの分岐点から」を表す rev。既定ブランチ名を知らなくても
/// マージベース比較ができる唯一の可搬な書き方 (`@{u}` は git の標準記法)。
const UPSTREAM_REV: &str = "@{u}...HEAD";

/// 一覧に出す候補 1 件。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Candidate {
    pub kind: Kind,
    /// 一覧の主表示 (ディレクトリは末尾 `/` 付き)。
    pub label: String,
    /// **解決先の説明**。確定する前に必ずここを見せる。
    pub detail: String,
    pub target: Target,
    /// 並べ替え用。[`rank`] が埋める。
    pub score: i32,
}

/// 順位付け済みの一覧。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct Ranked {
    pub items: Vec<Candidate>,
    /// 上限で落とした件数。**0 でなければ UI に必ず出す** (黙って切らない)。
    pub omitted: usize,
}

/// ディレクトリを持ち上げる加点。階層を降りる操作を素早くするため。
const FOLDER_BONUS: i32 = 2_000;
/// 完全一致の持ち上げ。`@main` と打って `main.rs` が最上位に来るように。
const EXACT_BONUS: i32 = 40_000;
/// 前方一致の持ち上げ。
const PREFIX_BONUS: i32 = 20_000;
/// 種別 1 段ぶんの目減り (fuzzy の素点より大きく、一致段より小さい)。
const KIND_STEP: i32 = 500;

/// 候補を絞り込んで並べる。マッチャは [`crate::fuzzy`] を使い回す
/// (2 つ目の fuzzy を書かない)。
pub fn rank(mut cands: Vec<Candidate>, term: &str, limit: usize) -> Ranked {
    let pq = PreparedQuery::new(term);
    let lower = term.to_lowercase();
    cands.retain_mut(|c| {
        let Some(base) = pq.score(&c.label) else {
            return false;
        };
        let l = c.label.to_lowercase();
        let tier = if !lower.is_empty() && l == lower {
            EXACT_BONUS
        } else if !lower.is_empty() && l.starts_with(&lower) {
            PREFIX_BONUS
        } else {
            0
        };
        let folder = if c.kind == Kind::Folder {
            FOLDER_BONUS
        } else {
            0
        };
        c.score = base
            .saturating_add(tier)
            .saturating_add(folder)
            .saturating_sub(c.kind.rank().saturating_mul(KIND_STEP));
        true
    });
    cands.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
    let omitted = cands.len().saturating_sub(limit);
    cands.truncate(limit);
    Ranked {
        items: cands,
        omitted,
    }
}

// ---------------------------------------------------------------------------
// 切り詰めと「捨てた印」
// ---------------------------------------------------------------------------

/// 本文のどちら側を残すか。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Keep {
    /// 先頭を残す (ファイル・差分)。
    Head,
    /// 末尾を残す (端末の出力)。
    Tail,
}

/// 切り詰めた結果。**捨てた量を必ず持ち回る**。
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Trimmed {
    /// 標識を含んだ本文。
    pub text: String,
    pub omitted_lines: usize,
    pub omitted_chars: usize,
}

impl Trimmed {
    pub fn is_trimmed(&self) -> bool {
        self.omitted_lines > 0 || self.omitted_chars > 0
    }
}

/// 捨てた箇所へ入れる標識。**画面にも挿入テキストにも同じ文言が出る**。
pub fn gap_marker(lines: usize, chars: usize) -> String {
    trf(
        "⟪省略: {l} 行 / {c} 文字 — Zaivern が上限で切りました⟫",
        &[("l", lines.to_string()), ("c", chars.to_string())],
    )
}

/// 行境界で切り詰め、捨てた箇所に [`gap_marker`] を残す。
///
/// **文字境界で切らない** — CJK でも壊れないよう、必ず行単位で落とす。
pub fn trim_lines(body: &str, max_chars: usize, keep: Keep) -> Trimmed {
    let total: usize = body.chars().count();
    if total <= max_chars {
        return Trimmed {
            text: body.to_string(),
            omitted_lines: 0,
            omitted_chars: 0,
        };
    }
    let lines: Vec<&str> = body.lines().collect();
    let mut taken: Vec<&str> = Vec::new();
    let mut used = 0usize;
    let idx: Vec<usize> = match keep {
        Keep::Head => (0..lines.len()).collect(),
        Keep::Tail => (0..lines.len()).rev().collect(),
    };
    let mut kept = vec![false; lines.len()];
    for i in idx {
        let n = lines[i].chars().count() + 1;
        if used + n > max_chars {
            break;
        }
        used += n;
        kept[i] = true;
    }
    for (i, line) in lines.iter().enumerate() {
        if kept[i] {
            taken.push(line);
        }
    }
    let omitted_lines = lines.len() - taken.len();
    let omitted_chars = total.saturating_sub(used);
    let marker = gap_marker(omitted_lines, omitted_chars);
    let text = match keep {
        Keep::Head => format!("{}\n{marker}", taken.join("\n")),
        Keep::Tail => format!("{marker}\n{}", taken.join("\n")),
    };
    Trimmed {
        text,
        omitted_lines,
        omitted_chars,
    }
}

// ---------------------------------------------------------------------------
// コスト表示
// ---------------------------------------------------------------------------

/// 添付 1 件ぶんの費用。
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Cost {
    pub chars: usize,
    pub lines: usize,
    /// **概算**トークン数。UI では必ず `~` を付けて出す。
    pub tokens: usize,
}

impl Cost {
    fn add(self, o: Cost) -> Cost {
        Cost {
            chars: self.chars + o.chars,
            lines: self.lines + o.lines,
            tokens: self.tokens + o.tokens,
        }
    }

    /// `1.2k` のような短い表記 (幅の狭いチップに収めるため)。
    pub fn short_tokens(self) -> String {
        if self.tokens >= 10_000 {
            format!("{}k", self.tokens / 1_000)
        } else if self.tokens >= 1_000 {
            format!("{:.1}k", self.tokens as f32 / 1_000.0)
        } else {
            self.tokens.to_string()
        }
    }
}

/// ASCII 4 文字 ≒ 1 トークン、非 ASCII 1 文字 ≒ 1 トークン として数える。
///
/// **厳密なトークナイザではない。** BPE はモデルごとに違い、CLI 経由では
/// 実測できない。ここで欲しいのは「この添付は他より一桁重いか」が分かる
/// 精度なので、CJK が 1 文字 1 トークン前後になる点だけ外さないようにしてある。
pub fn cost_of(text: &str) -> Cost {
    let mut ascii = 0usize;
    let mut wide = 0usize;
    for c in text.chars() {
        if c.is_ascii() {
            ascii += 1;
        } else {
            wide += 1;
        }
    }
    Cost {
        chars: ascii + wide,
        lines: text.lines().count(),
        tokens: ascii.div_ceil(4) + wide,
    }
}

// ---------------------------------------------------------------------------
// バイト範囲の解決 (CJK / タブ混在でも壊れない)
// ---------------------------------------------------------------------------

/// 0 起点の行番号 → **バイト**オフセット。行が足りなければ末尾を返す。
pub fn line_start_byte(text: &str, line: usize) -> usize {
    if line == 0 {
        return 0;
    }
    let mut seen = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            seen += 1;
            if seen == line {
                return i + 1;
            }
        }
    }
    text.len()
}

/// 0 起点の行 + **LSP の UTF-16 列** → バイトオフセット。
///
/// LSP の列は UTF-16 コード単位なので、CJK (1 単位) と絵文字 (2 単位) で
/// 進み方が違う。タブは 1 文字として数える (LSP も桁を展開しない)。
pub fn byte_offset(text: &str, line: usize, utf16_col: usize) -> usize {
    let start = line_start_byte(text, line);
    let rest = &text[start..];
    let mut units = 0usize;
    for (off, c) in rest.char_indices() {
        if units >= utf16_col {
            return start + off;
        }
        if c == '\n' {
            return start + off;
        }
        units += c.len_utf16();
    }
    text.len()
}

/// 0 起点・両端を含む行範囲の**バイト範囲**。
pub fn line_range(text: &str, start_line: usize, end_line: usize) -> std::ops::Range<usize> {
    let s = line_start_byte(text, start_line);
    let e = if end_line + 1 == usize::MAX {
        text.len()
    } else {
        line_start_byte(text, end_line + 1)
    };
    s..e.max(s)
}

// ---------------------------------------------------------------------------
// 本文への差し込み (純粋関数)
// ---------------------------------------------------------------------------

/// `@クエリ` を `insert` で置き換える。返り値は `(新しい本文, 新しいキャレット)`。
///
/// 文字単位で扱うので日本語でも壊れない ([`crate::panels::insert_at_caret`] と同じ流儀)。
pub fn apply_pick(text: &str, q: &Query, insert: &str) -> (String, usize) {
    let chars: Vec<char> = text.chars().collect();
    let at = q.at.min(chars.len());
    let caret = q.caret.clamp(at, chars.len());
    let mut out: String = chars[..at].iter().collect();
    out.push_str(insert);
    out.extend(chars[caret..].iter());
    (out, at + insert.chars().count())
}

// ---------------------------------------------------------------------------
// 添付台帳
// ---------------------------------------------------------------------------

/// 添付の本文。**裏で取りに行っている間も UI は待たない**。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Body {
    /// 裏で取得中。
    Pending,
    Ready(Trimmed),
    /// 取れなかった理由 (これも黙らせない)。
    Failed(String),
}

/// 本文に置いた印 1 つと、それが指す実体。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Attachment {
    /// 本文へ書かれる印 (`@src/foo.rs:10-20`)。**これが台帳との鍵**。
    pub token: String,
    pub kind: Kind,
    /// 解決先の説明 (挿入前に見せたものと同じ)。
    pub detail: String,
    pub body: Body,
}

impl Attachment {
    /// この添付がプロンプトへ足す費用。未解決なら 0。
    pub fn cost(&self) -> Cost {
        match &self.body {
            Body::Ready(t) => cost_of(&t.text),
            _ => Cost::default(),
        }
    }
}

/// 本文に置かれた印と本文の対応表。
///
/// **自己修復する**: ユーザーが本文から印を消したら、その添付も落ちる
/// ([`Ledger::prune`])。台帳と本文がずれて「送ったつもりの無い文脈が飛ぶ」
/// 事故を構造的に潰す。
#[derive(Clone, Default, Debug)]
pub struct Ledger {
    items: Vec<Attachment>,
}

impl Ledger {
    pub fn items(&self) -> &[Attachment] {
        &self.items
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// 同じ印が既にあれば差し替える (二重添付を作らない)。
    pub fn add(&mut self, a: Attachment) {
        match self.items.iter_mut().find(|x| x.token == a.token) {
            Some(slot) => *slot = a,
            None => self.items.push(a),
        }
    }

    /// 裏で取れた本文を流し込む。印が既に本文から消えていたら捨てる。
    pub fn resolve(&mut self, token: &str, body: Body) {
        if let Some(slot) = self.items.iter_mut().find(|x| x.token == token) {
            slot.body = body;
        }
    }

    /// 本文に残っていない印の添付を落とす。
    pub fn prune(&mut self, text: &str) {
        self.items.retain(|a| text.contains(&a.token));
    }

    pub fn clear(&mut self) {
        self.items.clear();
    }

    /// まだ裏で取っている最中の件数。
    pub fn pending(&self) -> usize {
        self.items
            .iter()
            .filter(|a| a.body == Body::Pending)
            .count()
    }

    /// 全添付の合計コスト。
    pub fn total(&self) -> Cost {
        self.items.iter().fold(Cost::default(), |a, x| a.add(x.cost()))
    }
}

// ---------------------------------------------------------------------------
// 送信直前の展開
// ---------------------------------------------------------------------------

/// 本文に含まれるバッククォートの最長連を避けたフェンスを作る。
///
/// 本文にコードフェンスが入っていると 3 個では閉じてしまうので、
/// **必ず本文より 1 個長い**フェンスを使う。
fn fence_for(body: &str) -> String {
    let mut longest = 0usize;
    let mut run = 0usize;
    for c in body.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat(longest.max(2) + 1)
}

/// 送信直前に本文へ添付を展開する (純粋関数)。
///
/// * 本文中の印はそのまま残す — 人が読んでも文脈が分かるため。
/// * `native_at` はそのエージェントが `@パス` を**自分で**読み込む記法。
///   [`crate::agents::AgentSpec::file_ref_syntax`] から来るデータで、
///   空なら常に本文を同梱する。ファイル全体の参照だけに効かせる
///   (範囲・端末・差分・診断を自分で読める CLI は無い)。
pub fn expand(text: &str, items: &[Attachment], native_at: bool) -> String {
    let live: Vec<&Attachment> = items.iter().filter(|a| text.contains(&a.token)).collect();
    if live.is_empty() {
        return text.to_string();
    }
    let mut out = text.trim_end().to_string();
    let mut blocks: Vec<String> = Vec::new();
    for a in live {
        // `@パス` を CLI 自身が展開できるファイル参照は、本文を付けない。
        if native_at && a.kind == Kind::File {
            continue;
        }
        let head = format!("▼ {} — {}", a.token, a.detail);
        match &a.body {
            Body::Ready(t) => {
                let f = fence_for(&t.text);
                blocks.push(format!("{head}\n{f}\n{}\n{f}", t.text));
            }
            Body::Pending => blocks.push(format!(
                "{head}\n{}",
                tr("⟪未解決: 取得が終わる前に送信されました⟫")
            )),
            Body::Failed(why) => blocks.push(format!(
                "{head}\n{}",
                trf("⟪取得できませんでした: {why}⟫", &[("why", why.clone())])
            )),
        }
    }
    if blocks.is_empty() {
        return out;
    }
    out.push_str("\n\n");
    out.push_str(&tr("--- 添付コンテキスト (Zaivern が確定して同梱) ---"));
    out.push('\n');
    out.push_str(&blocks.join("\n\n"));
    out
}

// ---------------------------------------------------------------------------
// 候補の材料 (App から借りてくるもの)
// ---------------------------------------------------------------------------

/// 見つかったシンボル 1 件。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct SymbolHit {
    pub name: String,
    /// 種類のラベル (`関数` / `Function` など。LSP の SymbolKind か走査の推定)。
    pub kind_label: String,
    /// ワークスペース相対 (スラッシュ区切り)。
    pub rel: String,
    /// 0 起点・両端を含む。
    pub start_line: usize,
    pub end_line: usize,
    /// LSP が出した範囲か。false は本文走査の**近似**で、UI にもそう出す。
    pub exact: bool,
}

/// 候補を組むために App が渡すもの。**mention.rs は App を知らない**。
#[derive(Clone, Copy)]
pub struct Source<'a> {
    /// 索引の基準になるルート (相対パスはここからの相対)。
    pub root: &'a Path,
    /// 索引済みの相対パス (スラッシュ区切り)。
    pub files: &'a [String],
    /// 索引が上限で打ち切られているか (黙って隠さない)。
    pub files_truncated: bool,
    /// シンボル (LSP と走査の混在)。
    pub symbols: &'a [SymbolHit],
    /// シンボル走査がまだ走っているか。
    pub symbols_busy: bool,
    /// 起動中の端末 `(id, 表示名)`。
    pub terminals: &'a [(u64, String)],
    /// 診断の件数。0 なら `@problems` は出さない (空の項目を並べない)。
    pub problems: usize,
    /// git の作業ツリー。無ければ `@diff` は出さない。
    pub repo: Option<&'a Path>,
}

/// 一覧の上限。超えたぶんは [`Ranked::omitted`] として**必ず表示する**。
pub const LIST_LIMIT: usize = 40;

/// クエリから候補一覧を組む (純粋関数)。
pub fn candidates(q: &Query, src: &Source<'_>) -> Ranked {
    let (dir, leaf) = split_dir(&q.term);
    // `@diff:origin/main` の `/` はパスではないので、階層モードへ入れない。
    let path_mode = !dir.is_empty() && q.ns != Some(Kind::Diff);
    let mut out: Vec<Candidate> = Vec::new();

    let want = |k: Kind| q.ns.is_none_or(|n| n == k);

    // ── ファイル / フォルダ: 必ず**直下 1 段**を出す (階層で降りる) ──
    if want(Kind::File) || want(Kind::Folder) {
        let base = normalize_dir(dir);
        for c in children_of(src.files, &base) {
            let rel = format!("{base}{}", c.name);
            if c.is_dir {
                if !want(Kind::Folder) {
                    continue;
                }
                out.push(Candidate {
                    kind: Kind::Folder,
                    label: format!("{}/", c.name),
                    detail: trf(
                        "{rel}/ — 直下 {n} 件 (Enter で降りる / Tab でこの階層を添付)",
                        &[("rel", rel.clone()), ("n", c.count.to_string())],
                    ),
                    target: Target::Folder { rel },
                    score: 0,
                });
            } else {
                if !want(Kind::File) {
                    continue;
                }
                out.push(Candidate {
                    kind: Kind::File,
                    label: c.name.clone(),
                    detail: rel.clone(),
                    target: Target::File { rel },
                    score: 0,
                });
            }
        }
    }

    // ── シンボル: 解決先の行範囲まで見せる ──
    if want(Kind::Symbol) && !path_mode {
        for s in src.symbols {
            out.push(Candidate {
                kind: Kind::Symbol,
                label: s.name.clone(),
                detail: format!(
                    "{} {}:{}-{} ({})",
                    if s.exact { "=" } else { "≈" },
                    s.rel,
                    s.start_line + 1,
                    s.end_line + 1,
                    s.kind_label
                ),
                target: Target::Symbol {
                    rel: s.rel.clone(),
                    start_line: s.start_line,
                    end_line: s.end_line,
                    exact: s.exact,
                },
                score: 0,
            });
        }
    }

    if !path_mode {
        // ── 端末 ──
        if want(Kind::Terminal) {
            for (id, title) in src.terminals {
                out.push(Candidate {
                    kind: Kind::Terminal,
                    label: title.clone(),
                    detail: trf(
                        "端末 #{id} の末尾出力を添付します",
                        &[("id", id.to_string())],
                    ),
                    target: Target::Terminal {
                        id: *id,
                        title: title.clone(),
                    },
                    score: 0,
                });
            }
        }
        // ── 差分 ──
        if want(Kind::Diff) && src.repo.is_some() {
            out.push(Candidate {
                kind: Kind::Diff,
                label: tr("作業ツリーの差分"),
                detail: tr("HEAD との差 (ステージ済み + 未ステージ)"),
                target: Target::Diff(DiffScope::WorkTree),
                score: 0,
            });
            out.push(Candidate {
                kind: Kind::Diff,
                label: tr("上流との分岐点からの差分"),
                detail: trf(
                    "{rev} — マージベースからの差 (基点は git が解決します)",
                    &[("rev", UPSTREAM_REV.to_string())],
                ),
                target: Target::Diff(DiffScope::Branch {
                    rev: UPSTREAM_REV.to_string(),
                    name: "upstream".to_string(),
                }),
                score: 0,
            });
            // `@diff:<ブランチ>` と打てば任意の基点と比べられる。
            // 名前の検査は git_panel と同じ 1 本を通す (別の規則を作らない)。
            if q.ns == Some(Kind::Diff) {
                if let Ok(name) = crate::git_panel::validate_rev(&q.term) {
                    let rev = format!("{name}...HEAD");
                    out.push(Candidate {
                        kind: Kind::Diff,
                        label: trf("{n} との差分", &[("n", name.clone())]),
                        detail: trf(
                            "{rev} — マージベースからの差",
                            &[("rev", rev.clone())],
                        ),
                        target: Target::Diff(DiffScope::Branch { rev, name }),
                        score: 0,
                    });
                }
            }
        }
        // ── 診断 (Cursor が @Lint Errors として消したもの) ──
        if want(Kind::Problems) && src.problems > 0 {
            out.push(Candidate {
                kind: Kind::Problems,
                label: tr("いまの診断"),
                detail: trf(
                    "{n} 件のエラー・警告を添付します",
                    &[("n", src.problems.to_string())],
                ),
                target: Target::Problems,
                score: 0,
            });
        }
    }

    rank(out, leaf, LIST_LIMIT)
}

/// 候補を確定したときに本文へ入る印。
///
/// ディレクトリだけは「降りる」ための中間形 (末尾 `/`) を返し、確定しない。
pub fn token_for(t: &Target) -> String {
    match t {
        Target::File { rel } => format!("@{rel}"),
        Target::Folder { rel } => format!("@{rel}/"),
        Target::Symbol {
            rel,
            start_line,
            end_line,
            ..
        } => format!("@{rel}:{}-{}", start_line + 1, end_line + 1),
        Target::Terminal { id, .. } => format!("@{}:{id}", Kind::Terminal.ns()),
        Target::Diff(scope) => format!("@{}:{}", Kind::Diff.ns(), scope.slug()),
        Target::Problems => format!("@{}", Kind::Problems.ns()),
    }
}

/// フォルダそのものを添付するときの印 (Tab で確定したとき)。
pub fn folder_token(rel: &str) -> String {
    format!("@{}:{rel}/", Kind::Folder.ns())
}

// ---------------------------------------------------------------------------
// シンボル走査 (裏のスレッドで走る純粋な処理)
// ---------------------------------------------------------------------------

/// 「定義らしい行」を見つけるためのキーワード表。
///
/// **言語ごとの分岐を持たない。** どの言語でも「定義のキーワード + 名前」という
/// 形は共通なので、キーワードだけをデータとして持つ。ここに無い言語でも
/// LSP が動いていれば `documentSymbol` の側 (`exact = true`) で拾える。
const DEF_KEYWORDS: &[&str] = &[
    "fn",
    "func",
    "function",
    "def",
    "defn",
    "sub",
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "impl",
    "type",
    "const",
    "static",
    "let",
    "var",
    "val",
    "module",
    "namespace",
    "package",
    "record",
    "object",
    "protocol",
    "extension",
    "macro_rules",
];

/// 走査するファイル数の上限。超えたぶんは [`ScanResult::skipped_files`] に出す。
const SCAN_MAX_FILES: usize = 4_000;
/// 1 ファイルあたりに読む上限バイト数 (生成物・ミニファイ済みを読み切らない)。
const SCAN_MAX_BYTES: usize = 1_500_000;
/// ヒット件数の上限。
const SCAN_MAX_HITS: usize = 200;
/// シンボル 1 件として切り出す最大行数。
pub const SYMBOL_MAX_LINES: usize = 400;

/// シンボル走査の結果。**打ち切りを黙って隠さない**。
#[derive(Clone, Default, PartialEq, Eq, Debug)]
pub struct ScanResult {
    pub hits: Vec<SymbolHit>,
    /// 上限で落としたヒット数。
    pub omitted_hits: usize,
    /// 読まなかったファイル数 (上限・巨大・非 UTF-8)。
    pub skipped_files: usize,
}

/// 行頭の字下げを桁数で測る (タブは 4 桁として数える)。
fn indent_of(line: &str) -> usize {
    let mut n = 0usize;
    for c in line.chars() {
        match c {
            '\t' => n += 4,
            ' ' => n += 1,
            _ => break,
        }
    }
    n
}

/// 定義行から本体の終わりの行 (0 起点・両端を含む) を推定する。
///
/// **字下げだけを見る言語非依存の近似**。ブレース言語では閉じ括弧だけの行を
/// 取り込み、Python のような字下げ言語では字下げが戻った手前で切る。
/// 近似であることは UI で `≈` として明示するので、外れても嘘にはならない。
pub fn def_end_line(lines: &[&str], start: usize, max_lines: usize) -> usize {
    let Some(first) = lines.get(start) else {
        return start;
    };
    let base = indent_of(first);
    let limit = (start + max_lines).min(lines.len().saturating_sub(1));
    let mut end = start;
    for i in (start + 1)..=limit {
        let l = lines[i];
        if l.trim().is_empty() {
            continue;
        }
        let ind = indent_of(l);
        if ind <= base {
            // 閉じ括弧だけの行は本体の一部として取り込む。
            if ind == base && matches!(l.trim(), "}" | "};" | ")" | ");" | "}," | "end") {
                end = i;
            }
            break;
        }
        end = i;
    }
    end
}

/// 定義行を探す正規表現。`regex` は線形時間なので、ユーザーが打った語を
/// そのまま埋めても破滅的バックトラックにならない (自前の照合は書かない)。
fn def_regex(term: &str) -> Option<regex::Regex> {
    if term.trim().is_empty() {
        return None;
    }
    let pat = format!(
        r"(?im)^[^\n]*\b(?:{kw})\b[^\n]*?\b(\w*{name}\w*)\b",
        kw = DEF_KEYWORDS.join("|"),
        name = regex::escape(term)
    );
    regex::Regex::new(&pat).ok()
}

/// ワークスペースを走査してシンボルらしい定義を集める。**裏のスレッドで呼ぶこと**。
///
/// LSP が動いていない言語でも効く保険であり、`workspace/symbol` を持たない
/// サーバーでも効く。Cursor が `@code` を消して失われた「明示的にこの関数を
/// 指す」操作を、サーバーの有無に依らず成立させるのが狙い。
pub fn scan_symbols(root: &Path, files: &[String], term: &str) -> ScanResult {
    let Some(re) = def_regex(term) else {
        return ScanResult::default();
    };
    let mut out = ScanResult::default();
    for rel in files.iter().take(SCAN_MAX_FILES) {
        let abs = root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR));
        let Ok(meta) = std::fs::metadata(&abs) else {
            out.skipped_files += 1;
            continue;
        };
        if meta.len() as usize > SCAN_MAX_BYTES {
            out.skipped_files += 1;
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&abs) else {
            out.skipped_files += 1;
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        // 行頭オフセットの表を 1 回だけ作り、マッチ位置から行番号を引く。
        for m in re.captures_iter(&text) {
            let Some(name) = m.get(1) else { continue };
            let start = text[..name.start()].matches('\n').count();
            let end = def_end_line(&lines, start, SYMBOL_MAX_LINES);
            if out.hits.len() >= SCAN_MAX_HITS {
                out.omitted_hits += 1;
                continue;
            }
            out.hits.push(SymbolHit {
                name: name.as_str().to_string(),
                kind_label: tr("走査"),
                rel: rel.clone(),
                start_line: start,
                end_line: end,
                exact: false,
            });
        }
    }
    if files.len() > SCAN_MAX_FILES {
        out.skipped_files += files.len() - SCAN_MAX_FILES;
    }
    out
}

// ---------------------------------------------------------------------------
// 本文の取得 (裏のスレッドで走る)
// ---------------------------------------------------------------------------

/// 添付 1 件の本文の上限文字数。超えたぶんは [`gap_marker`] で明示する。
pub const BODY_MAX_CHARS: usize = 6_000;
/// 端末から取る行数と 1 行の桁数。
pub const TERM_TAIL_ROWS: usize = 120;
pub const TERM_TAIL_COLS: usize = 400;

/// 裏で取りに行く仕事。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Fetch {
    /// ファイル全体。
    File { abs: PathBuf },
    /// 行範囲 (0 起点・両端を含む)。
    Span {
        abs: PathBuf,
        start_line: usize,
        end_line: usize,
    },
    /// 差分。git は**必ず裏**で撃つ (UI スレッドで待つと数秒止まる)。
    Diff { repo: PathBuf, scope: DiffScope },
    /// フォルダ直下の一覧。
    Listing { rel: String, names: Vec<String> },
}

/// 仕事を実行する。**UI スレッドから呼ばない**こと。
pub fn run_fetch(job: &Fetch) -> Result<Trimmed, String> {
    match job {
        Fetch::File { abs } => {
            let text = std::fs::read_to_string(abs).map_err(|e| e.to_string())?;
            Ok(trim_lines(&text, BODY_MAX_CHARS, Keep::Head))
        }
        Fetch::Span {
            abs,
            start_line,
            end_line,
        } => {
            let text = std::fs::read_to_string(abs).map_err(|e| e.to_string())?;
            let r = line_range(&text, *start_line, *end_line);
            Ok(trim_lines(&text[r], BODY_MAX_CHARS, Keep::Head))
        }
        Fetch::Diff { repo, scope } => {
            let args = match scope {
                DiffScope::WorkTree => {
                    return crate::git::working_tree_diff(repo)
                        .map(|d| trim_lines(&d, BODY_MAX_CHARS, Keep::Head))
                }
                DiffScope::Branch { rev } => crate::git_panel::review_diff_args(
                    &crate::git_panel::ReviewBase::Rev(rev.clone()),
                    crate::git_panel::ContextLines::Three,
                    false,
                ),
            };
            let out = crate::git::run_git_at(repo, &args)?;
            Ok(trim_lines(&out, BODY_MAX_CHARS, Keep::Head))
        }
        Fetch::Listing { rel, names } => {
            let body = names
                .iter()
                .map(|n| format!("{rel}{n}"))
                .collect::<Vec<_>>()
                .join("\n");
            Ok(trim_lines(&body, BODY_MAX_CHARS, Keep::Head))
        }
    }
}

// ---------------------------------------------------------------------------
// ポップアップの配置 (純粋関数・テーブルテスト対象)
// ---------------------------------------------------------------------------

/// ポップアップの矩形一式。**すべて `area` の中に収まり、互いに重ならない**。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct PopupLayout {
    pub frame: egui::Rect,
    /// 選択中の解決先を出す 1 行 (**確定前に見せる**ための行)。
    pub header: egui::Rect,
    pub list: egui::Rect,
    /// 件数・打ち切り・合計コストを出す 1 行。
    pub footer: egui::Rect,
    /// 実際に描ける行数。
    pub rows: usize,
    /// 入力欄の**上**へ出したか。
    pub above: bool,
}

/// ポップアップの最小幅 / 最大幅。狭い窓では可用幅に従って縮む。
pub const POPUP_MIN_W: f32 = 280.0;
pub const POPUP_MAX_W: f32 = 560.0;
const POPUP_MARGIN: f32 = 6.0;
const POPUP_GAP: f32 = 4.0;

/// 入力欄 (`anchor`) の近くにポップアップを置く。
///
/// 下に入らなければ上へ回し、どちらでも `area` からはみ出させない。
/// 行数は入る分しか返さない — 入らない行を描いて見切れさせない。
pub fn popup_layout(
    area: egui::Rect,
    anchor: egui::Rect,
    items: usize,
    row_h: f32,
    line_h: f32,
) -> PopupLayout {
    let m = POPUP_MARGIN;
    let row_h = row_h.max(1.0);
    let line_h = line_h.max(1.0);
    let inner_w = (area.width() - 2.0 * m).max(1.0);
    let w = anchor
        .width()
        .clamp(POPUP_MIN_W.min(inner_w), POPUP_MAX_W.min(inner_w));
    let below = (area.bottom() - m - (anchor.bottom() + POPUP_GAP)).max(0.0);
    let above_space = ((anchor.top() - POPUP_GAP) - (area.top() + m)).max(0.0);
    let above = above_space > below;
    let space = if above { above_space } else { below };
    let chrome = 2.0 * line_h;
    let list_space = (space - chrome).max(0.0);
    let rows = ((list_space / row_h).floor() as usize).min(items.max(1));
    let h = (chrome + rows as f32 * row_h).min(space).max(0.0);
    let x_max = (area.right() - m - w).max(area.left() + m);
    let x = anchor.left().clamp(area.left() + m, x_max);
    let y = if above {
        (anchor.top() - POPUP_GAP - h).max(area.top() + m)
    } else {
        (anchor.bottom() + POPUP_GAP).min((area.bottom() - m - h).max(area.top() + m))
    };
    let frame = egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h));
    let head_h = line_h.min(h);
    let header = egui::Rect::from_min_size(frame.min, egui::vec2(w, head_h));
    let foot_h = (h - head_h).min(line_h).max(0.0);
    let footer = egui::Rect::from_min_size(
        egui::pos2(x, frame.bottom() - foot_h),
        egui::vec2(w, foot_h),
    );
    let list = egui::Rect::from_min_max(
        egui::pos2(x, header.bottom()),
        egui::pos2(x + w, footer.top().max(header.bottom())),
    );
    PopupLayout {
        frame,
        header,
        list,
        footer,
        rows,
        above,
    }
}

// ---------------------------------------------------------------------------
// 状態と配線
// ---------------------------------------------------------------------------

use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::agent_input::{AgentInputBuffer, ComposerTarget};
use crate::theme::Theme;

/// 打鍵を待つ間隔。1 打ごとに走査スレッドを起こさないための間引き。
const SCAN_DEBOUNCE: Duration = Duration::from_millis(220);
/// 走査を始める最短の語長 (1 文字でワークスペース全体を読まない)。
const SCAN_MIN_TERM: usize = 2;

/// キー操作の結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Nav {
    None,
    Up,
    Down,
    /// Enter — フォルダなら降りる、それ以外は確定。
    Accept,
    /// Tab — フォルダでも降りずにその場で添付する。
    Attach,
    Close,
}

/// App しか持っていない本文の要求。呼び出し側が [`Mention::provide`] で返す。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Need {
    Terminal { token: String, id: u64 },
    Problems { token: String },
}

/// 空の台帳 (宛先にまだ添付が無いときに貸す)。
static EMPTY_LEDGER: Ledger = Ledger { items: Vec::new() };

/// `@` ピッカーの全状態。App が 1 つだけ持つ。
#[derive(Default)]
pub struct Mention {
    /// 本文に置かれた印と本文の対応表。**宛先ごと**に分ける
    /// (下書きが宛先ごとに分かれているので、添付も分けないと
    ///  宛先を切り替えた瞬間に他方の添付が消える)。
    ledgers: std::collections::HashMap<ComposerTarget, Ledger>,
    /// いま編集中の宛先。
    cur: ComposerTarget,
    open: bool,
    sel: usize,
    /// 候補を組み直す鍵 (これが変わったときだけ組み直す)。
    key: String,
    ranked: Ranked,
    /// 一覧の上に出す注記。**0 件でも必ず何か言う**。
    notes: Vec<String>,
    // ── シンボル走査 ──
    scan_term: String,
    scan_rx: Option<mpsc::Receiver<(String, ScanResult)>>,
    scan_want: Option<(String, Instant)>,
    hits: Vec<SymbolHit>,
    scan_note: Option<String>,
    scan_busy: bool,
    // ── 本文取得 ──
    fetches: Vec<(String, mpsc::Receiver<Result<Trimmed, String>>)>,
    /// 一覧をクリックされた (次のフレームで Enter と同じ扱いにする)。
    clicked: bool,
    /// App しか持っていない本文の要求 (呼び出し側が `take_need` で引き取る)。
    need: Option<Need>,
}

impl Mention {
    /// ポップアップが出ているか (コンポーザが Esc/Enter を横取りしないための判定)。
    pub fn is_open(&self) -> bool {
        self.open
    }

    /// いまの宛先の台帳。
    pub fn ledger(&self) -> &Ledger {
        self.ledgers.get(&self.cur).unwrap_or(&EMPTY_LEDGER)
    }

    /// 指定した宛先の台帳 (送信直前の展開に使う)。
    fn ledger_of(&self, t: ComposerTarget) -> &Ledger {
        self.ledgers.get(&t).unwrap_or(&EMPTY_LEDGER)
    }

    /// 届いた本文を**どの宛先の台帳にでも**流し込む (印は一意)。
    fn resolve_any(&mut self, token: &str, body: Body) {
        for l in self.ledgers.values_mut() {
            l.resolve(token, body.clone());
        }
    }

    /// 送信直前に本文へ添付を展開する。`command` は宛先エージェントの起動コマンド
    /// (パス付き・サブコマンド付きでも `spec_for_command` が吸収する)。
    ///
    /// エージェントごとの差は [`crate::agents::AgentSpec::file_ref_syntax`] という
    /// **カタログのデータ**から来る (ここに CLI 名を書かない)。
    pub fn expand_for(&self, text: &str, command: Option<&str>, to: ComposerTarget) -> String {
        let native = command
            .and_then(crate::agents::spec_for_command)
            .is_some_and(|s| !s.file_ref_syntax().is_empty());
        expand(text, self.ledger_of(to).items(), native)
    }

    /// 端末・診断のように App しか持っていない本文を渡す。
    pub fn provide(&mut self, token: &str, body: String) {
        let t = trim_lines(&body, BODY_MAX_CHARS, Keep::Tail);
        self.resolve_any(token, Body::Ready(t));
    }

    /// 裏の仕事を取り込む。**待たない** (`try_recv` だけ)。
    fn collect(&mut self) -> bool {
        let mut changed = false;
        if let Some(rx) = &self.scan_rx {
            match rx.try_recv() {
                Ok((term, res)) => {
                    self.scan_rx = None;
                    self.scan_busy = false;
                    self.scan_term = term;
                    self.scan_note = (res.omitted_hits > 0 || res.skipped_files > 0).then(|| {
                        trf(
                            "走査: {h} 件を上限で省略 / {f} ファイルを読めませんでした",
                            &[
                                ("h", res.omitted_hits.to_string()),
                                ("f", res.skipped_files.to_string()),
                            ],
                        )
                    });
                    self.hits = res.hits;
                    self.key.clear();
                    changed = true;
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    self.scan_rx = None;
                    self.scan_busy = false;
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        let mut done: Vec<(String, Body)> = Vec::new();
        self.fetches.retain(|(token, rx)| match rx.try_recv() {
            Ok(Ok(t)) => {
                done.push((token.clone(), Body::Ready(t)));
                false
            }
            Ok(Err(e)) => {
                done.push((token.clone(), Body::Failed(e)));
                false
            }
            Err(mpsc::TryRecvError::Disconnected) => {
                done.push((token.clone(), Body::Failed(tr("取得が中断されました"))));
                false
            }
            Err(mpsc::TryRecvError::Empty) => true,
        });
        for (token, body) in done {
            self.resolve_any(&token, body);
            changed = true;
        }
        changed
    }

    /// 本文の取得を裏で始める。スレッドを起こせなければ理由を添えて失敗にする
    /// (**同期実行へは落とさない** — 落とすと UI が固まる元の挙動が復活する)。
    fn spawn_fetch(&mut self, token: String, job: Fetch, ctx: &egui::Context) {
        let (tx, rx) = mpsc::channel();
        let c = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-mention-fetch".into())
            .spawn(move || {
                let _ = tx.send(run_fetch(&job));
                c.request_repaint();
            });
        if spawned.is_ok() {
            self.fetches.push((token, rx));
        } else {
            self.resolve_any(&token, Body::Failed(tr("走査スレッドを起こせません")));
        }
    }

    /// シンボル走査を (必要なら) 裏で始める。
    fn request_scan(&mut self, term: &str, root: &Path, files: &[String], ctx: &egui::Context) {
        if term.chars().count() < SCAN_MIN_TERM || term == self.scan_term {
            self.scan_want = None;
            return;
        }
        match &self.scan_want {
            Some((want, at)) if want == term => {
                if at.elapsed() < SCAN_DEBOUNCE {
                    crate::perf::repaint_after(ctx, SCAN_DEBOUNCE, "mention-scan");
                    return;
                }
            }
            _ => {
                self.scan_want = Some((term.to_string(), Instant::now()));
                crate::perf::repaint_after(ctx, SCAN_DEBOUNCE, "mention-scan");
                return;
            }
        }
        if self.scan_rx.is_some() {
            return;
        }
        self.scan_want = None;
        let (tx, rx) = mpsc::channel();
        let (term, root, files) = (term.to_string(), root.to_path_buf(), files.to_vec());
        let c = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-mention-scan".into())
            .spawn(move || {
                let res = scan_symbols(&root, &files, &term);
                let _ = tx.send((term, res));
                c.request_repaint();
            });
        if spawned.is_ok() {
            self.scan_rx = Some(rx);
            self.scan_busy = true;
        } else {
            self.scan_note = Some(tr("走査スレッドを起こせません"));
        }
    }

    /// 打鍵をコンポーザより**先に**さらう。ポップアップが出ている間だけ食べる。
    fn grab(ctx: &egui::Context, open: bool) -> Nav {
        if !open {
            return Nav::None;
        }
        let mut nav = Nav::None;
        ctx.input_mut(|i| {
            // IME の確定 Enter を候補確定に使わない (Windows / Linux 対策)。
            let ime = i.events.iter().any(|e| matches!(e, egui::Event::Ime(_)));
            i.events.retain(|e| {
                let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = e
                else {
                    return true;
                };
                // 送信コード (⌘/Ctrl + Enter) には触らない。
                if modifiers.command || modifiers.ctrl || modifiers.alt {
                    return true;
                }
                match key {
                    egui::Key::ArrowDown => {
                        nav = Nav::Down;
                        false
                    }
                    egui::Key::ArrowUp => {
                        nav = Nav::Up;
                        false
                    }
                    egui::Key::Enter if !ime => {
                        nav = Nav::Accept;
                        false
                    }
                    egui::Key::Tab => {
                        nav = Nav::Attach;
                        false
                    }
                    egui::Key::Escape => {
                        nav = Nav::Close;
                        false
                    }
                    _ => true,
                }
            });
        });
        nav
    }
}

/// ワークスペース相対 (スラッシュ) から実パスへ。区切りは OS のものへ直す。
fn abs_of(root: &Path, rel: &str) -> PathBuf {
    root.join(rel.replace('/', std::path::MAIN_SEPARATOR_STR))
}

impl Mention {
    /// **コンポーザの先頭**で呼ぶ。打鍵をさらい、確定したら本文を書き換える。
    ///
    /// 「App しか持っていない本文」(端末の出力・診断) が要るときは
    /// [`Mention::take_need`] に積む。呼び出し側は [`Mention::provide`] で返す。
    pub fn sync(
        &mut self,
        ui: &egui::Ui,
        buf: &mut AgentInputBuffer,
        te_id: egui::Id,
        src: &Source<'_>,
    ) {
        if let Some(n) = self.sync_inner(ui, buf, te_id, src) {
            self.need = Some(n);
        }
    }

    /// 溜まっている要求を引き取る (App 側で本文を作って [`Mention::provide`])。
    pub fn take_need(&mut self) -> Option<Need> {
        self.need.take()
    }

    fn sync_inner(
        &mut self,
        ui: &egui::Ui,
        buf: &mut AgentInputBuffer,
        te_id: egui::Id,
        src: &Source<'_>,
    ) -> Option<Need> {
        let ctx = ui.ctx().clone();
        if self.collect() {
            ctx.request_repaint();
        }
        self.cur = buf.target();
        self.ledgers.entry(self.cur).or_default().prune(buf.text());
        self.ledgers.retain(|_, l| !l.is_empty());
        if !ui.memory(|m| m.has_focus(te_id)) {
            self.open = false;
            return None;
        }
        let st = egui::TextEdit::load_state(&ctx, te_id).unwrap_or_default();
        let caret = st
            .cursor
            .char_range()
            .map(|r| r.primary.index)
            .unwrap_or_else(|| buf.text().chars().count());
        let Some(q) = parse_query(buf.text(), caret) else {
            self.open = false;
            return None;
        };

        // LSP のシンボルと走査のシンボルを混ぜる。同じ場所を指すものは
        // **LSP 側 (exact) を残す** — 近似より正確な方を上に置くため。
        let mut syms: Vec<SymbolHit> = src.symbols.to_vec();
        syms.extend(self.hits.iter().cloned());
        syms.sort_by(|a, b| {
            (&a.rel, a.start_line, &a.name, !a.exact).cmp(&(&b.rel, b.start_line, &b.name, !b.exact))
        });
        syms.dedup_by(|a, b| a.rel == b.rel && a.start_line == b.start_line && a.name == b.name);
        let local = Source {
            symbols: &syms,
            ..*src
        };

        // 候補は**鍵が変わったときだけ**組み直す (毎フレーム索引を舐めない)。
        let key = format!(
            "{}\u{1}{}\u{1}{}\u{1}{}\u{1}{}",
            q.raw,
            src.files.len(),
            syms.len(),
            src.terminals.len(),
            src.problems
        );
        if key != self.key {
            self.key = key;
            self.ranked = candidates(&q, &local);
            self.sel = 0;
        }
        self.open = true;

        // 注記。**0 件でも必ず何か言う。切ったら必ず言う。**
        self.notes.clear();
        if self.ranked.omitted > 0 {
            self.notes.push(trf(
                "上位 {n} 件のみ表示 — {o} 件は出していません (絞り込んでください)",
                &[
                    ("n", self.ranked.items.len().to_string()),
                    ("o", self.ranked.omitted.to_string()),
                ],
            ));
        }
        if src.files_truncated {
            self.notes
                .push(tr("ファイル索引が上限で打ち切られています (一部が出ません)"));
        }
        if let Some(n) = &self.scan_note {
            self.notes.push(n.clone());
        }
        if self.scan_busy || src.symbols_busy {
            self.notes.push(tr("シンボルを走査中…"));
        }

        // シンボル走査は「パス指定でないとき」だけ (`@app/` はファイル閲覧)。
        if q.ns.is_none() || q.ns == Some(Kind::Symbol) {
            let (dir, leaf) = split_dir(&q.term);
            if dir.is_empty() {
                self.request_scan(leaf, src.root, src.files, &ctx);
            }
        }

        let mut nav = Self::grab(&ctx, self.open);
        if std::mem::take(&mut self.clicked) {
            nav = Nav::Accept;
        }
        let n = self.ranked.items.len();
        match nav {
            Nav::Close => {
                self.open = false;
                return None;
            }
            Nav::Down if n > 0 => self.sel = (self.sel + 1) % n,
            Nav::Up if n > 0 => self.sel = (self.sel + n - 1) % n,
            Nav::Accept | Nav::Attach if n > 0 => {
                let c = self.ranked.items[self.sel].clone();
                return self.commit(&ctx, buf, te_id, &q, &c, nav == Nav::Attach, src);
            }
            _ => {}
        }
        None
    }

    /// 候補を確定して本文へ書き込む。フォルダは Enter なら**降りるだけ**。
    fn commit(
        &mut self,
        ctx: &egui::Context,
        buf: &mut AgentInputBuffer,
        te_id: egui::Id,
        q: &Query,
        c: &Candidate,
        force_attach: bool,
        src: &Source<'_>,
    ) -> Option<Need> {
        let descend = matches!(c.target, Target::Folder { .. }) && !force_attach;
        let token = match (&c.target, force_attach) {
            (Target::Folder { rel }, true) => folder_token(rel),
            _ => token_for(&c.target),
        };
        // 確定したら末尾に空白を入れて、続けて打っても再起動しないようにする
        // (`panels::image_mention` と同じ流儀)。降りるときは空白を入れない。
        let insert = if descend {
            token.clone()
        } else {
            format!("{token} ")
        };
        let (text, caret) = apply_pick(buf.text(), q, &insert);
        buf.set_text(text);
        let mut st = egui::TextEdit::load_state(ctx, te_id).unwrap_or_default();
        st.cursor
            .set_char_range(Some(egui::text::CCursorRange::one(
                egui::text::CCursor::new(caret),
            )));
        st.store(ctx, te_id);
        if descend {
            // 階層を 1 段降りただけ。ポップアップは開けたままにする。
            self.key.clear();
            self.sel = 0;
            ctx.request_repaint();
            return None;
        }
        self.open = false;
        self.ledgers.entry(self.cur).or_default().add(Attachment {
            token: token.clone(),
            kind: c.kind,
            detail: c.detail.clone(),
            body: Body::Pending,
        });
        match &c.target {
            Target::File { rel } => {
                let job = Fetch::File {
                    abs: abs_of(src.root, rel),
                };
                self.spawn_fetch(token, job, ctx);
            }
            Target::Symbol {
                rel,
                start_line,
                end_line,
                ..
            } => {
                let job = Fetch::Span {
                    abs: abs_of(src.root, rel),
                    start_line: *start_line,
                    end_line: *end_line,
                };
                self.spawn_fetch(token, job, ctx);
            }
            Target::Folder { rel } => {
                let base = format!("{rel}/");
                let names = children_of(src.files, &base)
                    .into_iter()
                    .map(|c| if c.is_dir { format!("{}/", c.name) } else { c.name })
                    .collect();
                self.spawn_fetch(token, Fetch::Listing { rel: base, names }, ctx);
            }
            Target::Diff(scope) => match src.repo {
                Some(repo) => {
                    let job = Fetch::Diff {
                        repo: repo.to_path_buf(),
                        scope: scope.clone(),
                    };
                    self.spawn_fetch(token, job, ctx);
                }
                None => {
                    self.resolve_any(&token, Body::Failed(tr("git リポジトリがありません")))
                }
            },
            Target::Terminal { id, .. } => {
                return Some(Need::Terminal { token, id: *id });
            }
            Target::Problems => return Some(Need::Problems { token }),
        }
        None
    }
}

// ---------------------------------------------------------------------------
// 描画
// ---------------------------------------------------------------------------

impl Mention {
    /// **テキスト欄の直後**で呼ぶ。候補一覧を描く。
    ///
    /// `anchor` は入力欄そのものの矩形。入らなければ上へ回す ([`popup_layout`])。
    pub fn popup(&mut self, ui: &egui::Ui, theme: &Theme, anchor: egui::Rect) {
        if !self.open {
            return;
        }
        let ctx = ui.ctx().clone();
        let (row_h, line_h) = ctx.fonts(|f| {
            (
                f.row_height(&egui::FontId::proportional(12.5)) + 6.0,
                f.row_height(&egui::FontId::proportional(10.5)) + 6.0,
            )
        });
        let lay = popup_layout(
            ctx.screen_rect(),
            anchor,
            self.ranked.items.len().max(1),
            row_h,
            line_h,
        );
        let sel = self.sel.min(self.ranked.items.len().saturating_sub(1));
        let head = match self.ranked.items.get(sel) {
            // **確定する前に解決先を見せる** — これがこの機能の肝。
            Some(c) => format!("{} {} → {}", c.kind.icon(), c.label, c.detail),
            None => tr("該当なし — 別の語で絞り込んでください"),
        };
        let total = self.ledger().total();
        let mut clicked: Option<usize> = None;
        egui::Area::new(egui::Id::new("zv-mention-popup"))
            .order(egui::Order::Foreground)
            .fixed_pos(lay.frame.min)
            .show(&ctx, |ui| {
                ui.set_width(lay.frame.width());
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border))
                    .rounding(egui::Rounding::same(6.0))
                    .inner_margin(egui::Margin::symmetric(6.0, 4.0))
                    .show(ui, |ui| {
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(head.clone())
                                    .color(theme.accent)
                                    .size(11.0),
                            )
                            .truncate(),
                        )
                        .on_hover_text(head);
                        for note in &self.notes {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(note).color(theme.warn).size(10.0),
                                )
                                .truncate(),
                            );
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("zv-mention-list")
                            .max_height(lay.list.height().max(row_h))
                            .auto_shrink([false, true])
                            .show(ui, |ui| {
                                if self.ranked.items.is_empty() {
                                    ui.label(
                                        egui::RichText::new(tr(
                                            "該当なし (索引にあるファイルとシンボルだけを出します)",
                                        ))
                                        .color(theme.text_dim)
                                        .size(11.0),
                                    );
                                }
                                for (i, c) in self.ranked.items.iter().enumerate() {
                                    let line = format!("{} {}", c.kind.icon(), c.label);
                                    let r = ui.selectable_label(i == sel, line);
                                    if r.clicked() {
                                        clicked = Some(i);
                                    }
                                    if i == sel {
                                        r.scroll_to_me(None);
                                    }
                                    r.on_hover_text(&c.detail);
                                }
                            });
                        ui.add(
                            egui::Label::new(
                                egui::RichText::new(footer_text(&self.ranked, total))
                                    .color(theme.text_dim)
                                    .size(10.0),
                            )
                            .truncate(),
                        );
                    });
            });
        if let Some(i) = clicked {
            self.sel = i;
            self.clicked = true;
            ctx.request_repaint();
        }
    }
}

/// 一覧の下に出す 1 行。件数・打ち切り・いままでの合計コスト。
fn footer_text(r: &Ranked, total: Cost) -> String {
    let mut s = trf("{n} 件", &[("n", r.items.len().to_string())]);
    if r.omitted > 0 {
        s.push_str(&trf(" (+{o} 件は非表示)", &[("o", r.omitted.to_string())]));
    }
    s.push_str(&tr("  •  Enter 確定 / Tab フォルダごと添付 / Esc 閉じる"));
    if total.chars > 0 {
        s.push_str(&trf(
            "  •  添付合計 ~{t} tok",
            &[("t", total.short_tokens())],
        ));
    }
    s
}

/// 添付チップの列 (**1 件ごとのコスト付き**)。外された印を返す。
///
/// 文脈肥大はこの製品分野で最も放置されている不満で、1 件ごとの費用を
/// 出している競合は無い。ここが「送る前に気付ける」唯一の場所になる。
pub fn chips_ui(ui: &mut egui::Ui, theme: &Theme, ledger: &Ledger) -> Option<String> {
    if ledger.is_empty() {
        // 空のセクションは高さを取らない。
        return None;
    }
    let mut removed = None;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let total = ledger.total();
        ui.label(
            egui::RichText::new(trf(
                "📎 添付 {n} 件 / ~{t} tok / {c} 文字",
                &[
                    ("n", ledger.items().len().to_string()),
                    ("t", total.short_tokens()),
                    ("c", total.chars.to_string()),
                ],
            ))
            .color(theme.text_dim)
            .size(10.5),
        );
        for a in ledger.items() {
            let cost = a.cost();
            let state = match &a.body {
                Body::Pending => tr("解決中…"),
                Body::Failed(_) => tr("失敗"),
                Body::Ready(t) if t.is_trimmed() => {
                    trf("~{t} tok ✂", &[("t", cost.short_tokens())])
                }
                Body::Ready(_) => trf("~{t} tok", &[("t", cost.short_tokens())]),
            };
            let label = format!("{} {} · {}", a.kind.icon(), a.token, state);
            let hover = match &a.body {
                Body::Failed(e) => format!("{}\n{e}", a.detail),
                Body::Ready(t) if t.is_trimmed() => format!(
                    "{}\n{}",
                    a.detail,
                    gap_marker(t.omitted_lines, t.omitted_chars)
                ),
                _ => a.detail.clone(),
            };
            if ui
                .add(egui::Button::new(egui::RichText::new(label).size(10.5)).small())
                .on_hover_text(hover)
                .clicked()
            {
                removed = Some(a.token.clone());
            }
        }
    });
    removed
}

/// 本文から印を 1 種類まるごと取り除く (チップの ✕ 相当)。
pub fn strip_token(text: &str, token: &str) -> String {
    text.replace(&format!("{token} "), "").replace(token, "")
}
