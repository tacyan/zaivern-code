//! 🧩 **行域オーナーシップ** — 「同じファイルでも、違う行なら競合しない」の芯。
//!
//! ## なぜ要るのか (ファイル単位のリースが払っている代償)
//!
//! [`crate::lease`] はファイル単位で所有を主張し、他人が持つファイルへの書き込みを
//! **断る**ことで衝突ゼロを買っている。実測 (`docs/conflict-zero.md`) では
//! 64 体・1536 書込でマージ衝突 0 件を達成したが、その 0 件は
//! **971 回の書き込みを断って**買ったものだった。つまり:
//!
//! > 衝突ゼロは達成できているが、**並列度がファイル数で頭打ちになる**。
//! > `src/app.rs` のような大きなファイルを 1 人が持つと、他の 63 体は
//! > そのファイルに 1 バイトも書けない。
//!
//! これは製品としての天井そのものである。**同じファイルの違う行なら 2 人が
//! 同時に書けて、しかも後のマージが一撃で済む**なら、並列度の上限は
//! ファイル数ではなく**行域の数**になる。桁が 2 つ変わる。
//!
//! ## 不変条件はたった 1 つ
//!
//! > 稼働中の 2 つの行域は、同じファイル内では [`SAFE_BAND`] 行以上離れている。
//!
//! これが保たれている限り、**マージは常に一撃で通る**。「後で衝突するかも
//! しれない」ではなく「衝突し得ない」が構造的に言える。
//!
//! ## 行番号は動く — だからアンカーを持つ
//!
//! 他人が自分より上の行を書き換えると、自分の行域は下へずれる。行番号だけを
//! 持っていると、次の書き込みで**別人の領域を自分のものだと思い込む**。
//! [`Anchor`] は域の先頭行・末尾行の内容と行数を持ち、[`resolve`] が現在の
//! テキストから域を取り直す。追従 ([`follow`]) は [`crate::marks::map_lines`] を
//! 再利用する (2 実装を持つとズレるため、行対応の計算はここで再発明しない)。
//!
//! ## 決定性
//!
//! `HashMap` / `HashSet` を 1 つも使わない。同じ入力からは、どの OS の
//! どのプロセスでも 1 バイト違わない結果が出る。

// TODO(統合担当): 行域オーナーシップが lease / coedit / mesh へ全部繋がった
// 時点でこの allow を外す。CLAUDE.md の「never used 警告は繋いでいない検出器」に
// 従い、外せない項目が残ったらそれは未完成として報告する。
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

// ═══════════════════════════════════════════════════════════════════════════
//  1. 安全帯 — この定数がこの機能の全ての主張を支えている
// ═══════════════════════════════════════════════════════════════════════════

/// 2 つの行域の間に必要な**未変更行の数**。
///
/// # この値は実測で決めた (推測ではない)
///
/// 合成ファイルに両側の変更を置き、間隔を 0〜5 行と変えながら**実際に git を
/// 起こして**下限を測った。[`tests::実gitで安全帯の下限を測る`] が毎回同じ計測を
/// やり直すので、git の挙動が変われば即座に赤くなる。
///
/// 計測環境: git 2.47.1 / macOS 15 (darwin 25.5.0)。間隔 = 両側の変更行の間に
/// 挟まる**未変更行の数** ([`spans_too_close`] の `gap` と同じ定義)。自分側は
/// 置換に固定し、相手側の**変更の種類を変えて**下限を測った (幅 1 行と 3 行の両方)。
///
/// | 経路 | 相手=置換 | 相手=削除 | 相手=挿入 | 下限 |
/// |---|---|---|---|---|
/// | `git merge-file -p` (三方向マージ) | 1 | 1 | 1 | **1** |
/// | `git merge-tree --write-tree` / `git merge` | 1 | 1 | 1 | **1** |
/// | `git apply` (パッチ適用・文脈 3 行) | 3 | 3 | 3 | **3** |
///
/// **相手の変更の種類で下限は変わらなかった** (幅を 1→3 行に変えても同じ)。
/// 唯一の例外は**自分側が純粋な挿入**のときで、行 `p` の手前への挿入は
/// **行 `p` を書き換えない**ぶん実効距離が 1 行広く、間隔 0 行でも通る
/// (実測: `gap = 0` の 9 通りのうち「自分側が挿入」の 3 通りだけが綺麗に通った)。
/// ただし**同じ位置**への挿入どうしは必ず衝突するので、「挿入なら 0 でよい」という
/// 緩和は入れていない。差は [`tests::近すぎる判定が実際の衝突より何倍多いかを数える`]
/// が件数で固定している。
///
/// ## 元の理由付けは間違っていた
///
/// 以前ここには「git の diff は既定で 3 行の文脈を付ける。両側の変更が 3 行未満
/// しか離れていないと xdiff がハンクを 1 つに畳んで衝突にする」と書いてあったが、
/// **三方向マージではそうならない**。`xdl_merge` が衝突を出すのは変更ハンクが
/// **重なるか隣接するとき**だけで、3 行の文脈は diff の**表示**の話でしかない。
/// 実測でも `myers` / `minimal` / `patience` / `histogram` の 4 アルゴリズム
/// すべてで間隔 1 行あれば綺麗に通った (挿入同士なら 0 行でも通る)。
///
/// ## それでも 3 を採る理由
///
/// パッチ適用の経路が混ざると 3 行が要る。`git apply` / `git am` /
/// `git rebase --apply` / `git format-patch` 経由のレビューはハンクの
/// **前後 3 行の文脈が一致すること**を要求するので、間隔 2 行では
/// **置換・削除・挿入のいずれでも必ず落ちる** (実測: gap=2 は FAIL、gap=3 は ok)。
/// [`tests::実gitでパッチ適用の下限を測る`] が変更種別ごとに確かめている。
///
/// **いちばん厳しい経路に合わせる**のがこの定数の役目なので 3 のまま据え置く。
/// 下げてよいのは「三方向マージしか通らない」と保証できる場合だけで、その値は
/// [`MERGE_ONLY_BAND`] に分けてある。
pub const SAFE_BAND: u32 = 3;

/// **三方向マージだけ**を通す前提でよいときの下限 (実測値)。
///
/// [`SAFE_BAND`] の表のとおり `git merge-file` / `git merge-tree` / `git merge` は
/// 間隔 1 行あれば衝突しない。パッチ適用の経路が一切無いワークフロー
/// (worktree + `git merge` のみ) ならこちらを使うと確保できる行域が
/// **3 倍近くに増える**。既定にしないのは、`git am` が 1 回でも混ざった瞬間に
/// 保証が壊れるため — **保証の強さを既定にする**。
pub const MERGE_ONLY_BAND: u32 = 1;

// ═══════════════════════════════════════════════════════════════════════════
//  2. 型
// ═══════════════════════════════════════════════════════════════════════════

/// 行域 `[start, end]`。**1 始まり・両端を含む**。
///
/// `end` が [`Span::EOF`] のときは「ファイル末尾まで」を意味する。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    /// 「ファイル末尾まで」を表す番兵。
    pub const EOF: u32 = u32::MAX;

    /// 1 行だけの域。
    pub fn line(n: u32) -> Span {
        Span { start: n, end: n }
    }

    /// 行数 (EOF 込みの域は [`u32::MAX`] を返す)。
    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    /// 空 (start > end) か。壊れた入力の検出に使う。
    pub fn is_empty(&self) -> bool {
        self.start > self.end || self.start == 0
    }

    /// この域が行 `n` を含むか。
    pub fn contains(&self, n: u32) -> bool {
        n >= self.start && n <= self.end
    }
}

/// 行番号が動いても同じ場所を取り直すための錨。
///
/// 先頭行と末尾行の**内容**を覚えておき、[`resolve`] が現在のテキストから
/// 探し直す。内容が消えていたら `None` を返す — 「たぶんここだろう」で
/// 別人の領域を掴むより、**取り直せないと正直に言う**ほうが安全。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anchor {
    /// 域の先頭行の内容 (前後の空白を落としたもの)。
    #[serde(default)]
    pub head: String,
    /// 域の末尾行の内容 (前後の空白を落としたもの)。
    #[serde(default)]
    pub tail: String,
    /// 確保した時点の行数。探索の当たりを付けるのに使う。
    #[serde(default)]
    pub len: u32,
}

impl Anchor {
    /// 中身が空 = 錨として使えない。
    pub fn is_blank(&self) -> bool {
        self.head.is_empty() && self.tail.is_empty()
    }
}

/// 担当の単位。**ファイル全体**か、**ファイル内の行域**か。
///
/// `path` は [`crate::lease::normalize_path`] を通した相対パスまたは glob。
/// `span` が `None` ならファイル全体 (従来のファイル単位リースと等価)。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Region {
    pub path: String,
    #[serde(default)]
    pub span: Option<Span>,
    #[serde(default)]
    pub anchor: Anchor,
}

impl Region {
    /// ファイル全体を指す域。
    pub fn whole(path: &str) -> Region {
        Region {
            path: path.to_string(),
            span: None,
            anchor: Anchor::default(),
        }
    }

    /// ファイル全体か。
    pub fn is_whole(&self) -> bool {
        self.span.is_none()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. 表記 — 台帳にもコマンドラインにも同じ文字列で載る
// ═══════════════════════════════════════════════════════════════════════════

/// 指定の中身。[`Spec`] の一部。
///
/// 行域は行番号で書けるが、**人にもエージェントにも扱いやすいのは記号名**
/// (`#fn:draw_toolbar`)。行番号は他人の編集で動くのに対し、記号名は動かない。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sel {
    /// ファイル全体。
    Whole,
    /// 行番号で指した域。
    Lines(Span),
    /// Rust の記号で指した域 (`kind` は `fn` / `struct` など)。
    Symbol { kind: String, name: String },
}

/// パスと指定の組。[`parse_spec`] / [`render_spec`] が往復する。
///
/// [`Region`] と分けてあるのは、記号指定が**テキストを見るまで行域に落ちない**ため。
/// [`resolve_spec`] にテキストを渡すと [`Region`] になる。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spec {
    pub path: String,
    pub sel: Sel,
}

/// 記号指定で受け付ける種別。**Rust だけ**を意図的に対象にしている。
///
/// 他言語まで広げると誤検出が増え、ユーザーが機能ごと切ってしまう
/// (このリポジトリの流儀)。
pub const SYMBOL_KINDS: &[&str] = &[
    "fn", "struct", "enum", "trait", "impl", "mod", "union", "type", "const", "static",
];

/// 仕様文字列を [`Spec`] へ分解する。[`parse`] の上位互換。
///
/// | 書き方 | 意味 |
/// |---|---|
/// | `src/a.rs` | ファイル全体 |
/// | `src/a.rs#L10-40` | 10〜40 行目 (両端含む) |
/// | `src/a.rs#L10+30` | 10 行目から 30 行 |
/// | `src/a.rs#L10-` | 10 行目から末尾まで |
/// | `src/a.rs#L10` | 10 行目だけ |
/// | `src/a.rs#fn:draw_toolbar` | `draw_toolbar` 関数の全体 |
/// | `src/a.rs#struct:Region` | `Region` 構造体の全体 |
pub fn parse_spec(spec: &str) -> Result<Spec, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("空の指定です".into());
    }
    let Some((path, frag)) = spec.rsplit_once('#') else {
        return Ok(Spec {
            path: spec.to_string(),
            sel: Sel::Whole,
        });
    };
    // `#kind:name` — 記号指定
    if let Some((kind, name)) = frag.split_once(':') {
        if SYMBOL_KINDS.contains(&kind) && !name.is_empty() {
            if path.is_empty() {
                return Err(format!("パスがありません: {spec}"));
            }
            return Ok(Spec {
                path: path.to_string(),
                sel: Sel::Symbol {
                    kind: kind.to_string(),
                    name: name.to_string(),
                },
            });
        }
    }
    let Some(body) = frag.strip_prefix('L').or_else(|| frag.strip_prefix('l')) else {
        // `#` があっても `L` でも記号でもないならパスの一部とみなす
        return Ok(Spec {
            path: spec.to_string(),
            sel: Sel::Whole,
        });
    };
    if path.is_empty() {
        return Err(format!("パスがありません: {spec}"));
    }
    let span = parse_span_body(body).ok_or_else(|| format!("行域を読めません: {frag}"))?;
    if span.is_empty() {
        return Err(format!("行域が空です: {frag}"));
    }
    Ok(Spec {
        path: path.to_string(),
        sel: Sel::Lines(span),
    })
}

/// [`parse_spec`] の逆。
pub fn render_spec(s: &Spec) -> String {
    match &s.sel {
        Sel::Whole => s.path.clone(),
        Sel::Lines(sp) => render_lines(&s.path, *sp),
        Sel::Symbol { kind, name } => format!("{}#{}:{}", s.path, kind, name),
    }
}

/// 仕様文字列を行域へ分解する。
///
/// 受け付ける書き方は [`parse_spec`] の表のうち**行番号で書けるものだけ**。
/// 記号指定 (`#fn:name`) は**テキストを見ないと行域に落ちない**ので、ここでは
/// `Err` で断る (黙ってファイル全体として扱うと、確保した本人が
/// 「関数 1 つだけ取った」と思っているのに他の 63 体を締め出す)。
/// 記号指定を扱うときは [`parse_spec`] + [`resolve_spec`] を使う。
pub fn parse(spec: &str) -> Result<Region, String> {
    let s = parse_spec(spec)?;
    match s.sel {
        Sel::Whole => Ok(Region::whole(&s.path)),
        Sel::Lines(sp) => Ok(Region {
            path: s.path,
            span: Some(sp),
            anchor: Anchor::default(),
        }),
        Sel::Symbol { kind, name } => Err(format!(
            "記号指定はテキストが要ります (resolve_spec を使ってください): {kind}:{name}"
        )),
    }
}

/// `10-40` / `10+30` / `10-` / `10` を [`Span`] にする。
fn parse_span_body(body: &str) -> Option<Span> {
    let body = body.trim();
    if let Some((a, b)) = body.split_once('+') {
        let start: u32 = a.trim().parse().ok()?;
        let count: u32 = b.trim().parse().ok()?;
        if count == 0 {
            return None;
        }
        return Some(Span {
            start,
            end: start.saturating_add(count - 1),
        });
    }
    if let Some((a, b)) = body.split_once('-') {
        let start: u32 = a.trim().parse().ok()?;
        let b = b.trim();
        let end = if b.is_empty() {
            Span::EOF
        } else {
            b.parse().ok()?
        };
        return Some(Span { start, end });
    }
    let n: u32 = body.parse().ok()?;
    Some(Span::line(n))
}

fn render_lines(path: &str, s: Span) -> String {
    if s.end == Span::EOF {
        format!("{path}#L{}-", s.start)
    } else if s.start == s.end {
        format!("{path}#L{}", s.start)
    } else {
        format!("{path}#L{}-{}", s.start, s.end)
    }
}

/// [`parse`] の逆。台帳へ書くときはこれを通す (表記を 1 つに保つ)。
pub fn render(r: &Region) -> String {
    match r.span {
        None => r.path.clone(),
        Some(s) => render_lines(&r.path, s),
    }
}

/// 記号指定も含めて、**テキストを見て**行域を確定させる。
///
/// 記号が見つからなければ `Err`。錨も同時に打つので、返った [`Region`] は
/// そのまま [`follow`] / [`resolve`] で追従できる。
pub fn resolve_spec(s: &Spec, text: &str) -> Result<Region, String> {
    match &s.sel {
        Sel::Whole => Ok(Region::whole(&s.path)),
        Sel::Lines(sp) => Ok(Region {
            path: s.path.clone(),
            span: Some(*sp),
            anchor: capture_anchor(text, sp),
        }),
        Sel::Symbol { kind, name } => {
            let sp = symbol_span(text, kind, name)
                .ok_or_else(|| format!("{kind} {name} が見つかりません: {}", s.path))?;
            Ok(Region {
                path: s.path.clone(),
                span: Some(sp),
                anchor: capture_anchor(text, &sp),
            })
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 重なり判定 — 不変条件を守る唯一の関門
// ═══════════════════════════════════════════════════════════════════════════

/// 2 つの行域が、`band` 行の安全帯を挟んでもなお近すぎるか。
///
/// **`band` 行以上離れていれば `false`** (= 同時に持ってよい)。
/// 例: `1-10` と `14-20` は間に 3 行 (11,12,13) あるので `band = 3` では衝突しない。
pub fn spans_too_close(a: &Span, b: &Span, band: u32) -> bool {
    let (lo, hi) = if a.start <= b.start { (a, b) } else { (b, a) };
    // lo が EOF まで伸びているなら必ず重なる
    if lo.end == Span::EOF {
        return true;
    }
    // 間にある未変更行の数
    let gap = hi.start.saturating_sub(lo.end).saturating_sub(1);
    gap < band
}

/// 2 つの担当が同時に持てないか。
///
/// * パスが重ならなければ (glob 同士の交差が無ければ) 常に `false`
/// * 片方がファイル全体なら `true`
/// * どちらも行域なら [`spans_too_close`]
///
/// パスの照合は [`crate::lease::overlaps`] を使う — 3 OS のパス正規化と
/// glob 同士の交差判定は既にそこで実測済みで、**2 実装を持つとズレる**。
pub fn conflicts(a: &Region, b: &Region, band: u32) -> bool {
    if !crate::lease::overlaps(&a.path, &b.path) {
        return false;
    }
    match (a.span, b.span) {
        (None, _) | (_, None) => true,
        (Some(x), Some(y)) => {
            // glob 同士だと「同じファイルを指しているか」が確定しないので、
            // 行域で切り分けられるのは**両方が具体パスのとき**だけ。
            // 片方でも glob なら安全側 (= 衝突扱い) に倒す。
            if is_glob(&a.path) || is_glob(&b.path) {
                return true;
            }
            spans_too_close(&x, &y, band)
        }
    }
}

/// glob 記号を含むか。
fn is_glob(p: &str) -> bool {
    p.contains('*') || p.contains('?') || p.contains('[')
}

/// 一覧の中で同時に持てない組を全部出す (添字の組、`i < j`、辞書順)。
///
/// 出力が空であることが、**そのまま「一撃マージできる」証明**になる。
pub fn conflicting_pairs(list: &[Region], band: u32) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    for i in 0..list.len() {
        for j in (i + 1)..list.len() {
            if conflicts(&list[i], &list[j], band) {
                out.push((i, j));
            }
        }
    }
    out
}

/// 一覧が互いに素か (= この集合なら衝突し得ない)。
pub fn is_disjoint(list: &[Region], band: u32) -> bool {
    conflicting_pairs(list, band).is_empty()
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. 実テキストとの往復 — 錨を打つ / 取り直す / 追従する
// ═══════════════════════════════════════════════════════════════════════════

/// 錨の探索で末尾行を外側へ広げる**最小**距離。
const ANCHOR_MIN_RADIUS: usize = 16;
/// 同じく**最大**距離。ここより大きくずれたら域そのものが作り直されている。
const ANCHOR_MAX_RADIUS: usize = 256;
/// 先頭候補をいくつまで見るか。同じ内容の行が数千ある `}` のようなケースで
/// **O(候補数 × 半径)** に膨らむのを止める上限。
const ANCHOR_CANDIDATES: usize = 32;

/// テキストを行へ割る。**CRLF の `\r` を必ず落とす**。
///
/// Windows のチェックアウトは CRLF なので、落とさないと同じ内容のファイルが
/// OS によって別物に見える (`crate::marks` のソース検査テストが同じ罠を踏んだ)。
/// `str::lines` は `\r\n` の `\r` を落とすが、**最終行に改行が無いとき**の
/// 単独 `\r` は残すので、ここで念のため剥がす。
fn lines_of(text: &str) -> Vec<&str> {
    text.lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect()
}

/// テキストと行域から錨を作る。
pub fn capture_anchor(text: &str, span: &Span) -> Anchor {
    let lines = lines_of(text);
    let idx = |n: u32| -> String {
        if n == 0 || n == Span::EOF {
            return String::new();
        }
        lines
            .get((n - 1) as usize)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let end = if span.end == Span::EOF {
        lines.len() as u32
    } else {
        span.end
    };
    Anchor {
        head: idx(span.start),
        tail: idx(end),
        len: end.saturating_sub(span.start).saturating_add(1),
    }
}

/// 錨から現在の行域を取り直す。**取り直せなければ `None`**。
///
/// 「たぶんここだろう」で近い場所を掴むと、他人の領域を自分のものだと
/// 思い込む事故になる。確信が無ければ持ち主に確保し直させるほうが安い。
///
/// # 曖昧なときは断る
///
/// `}` だけの行・空行・`use std::fmt;` のように**同じ内容の行が複数ある**のは
/// Rust では普通のことなので、素朴に「最初に見つかった行」を採ると簡単に
/// 別人の領域を掴む。ここでは:
///
/// * 元の位置に**いちばん近い**候補を採る
/// * **同じ距離に 2 つ以上**あるなら断る (どちらとも決められない = 曖昧)
/// * 先頭の錨が空 (域が空行から始まっていた) なら断る。空行を探すと
///   ファイル中の全ての空行が候補になり、意味のある答えが出ない
///
/// # 末尾までの域
///
/// `span.end == Span::EOF` の域は、末尾行の内容が**書くたびに変わる**ので
/// 末尾の錨を当てにできない。先頭だけを取り直し、末尾は `EOF` のまま返す。
pub fn resolve(r: &Region, text: &str) -> Option<Span> {
    let span = r.span?;
    if r.anchor.is_blank() {
        return Some(span);
    }
    let lines = lines_of(text);
    if lines.is_empty() {
        return None;
    }
    if r.anchor.head.is_empty() {
        // 末尾だけでは域の始まりが決まらない
        return None;
    }
    let pref = (span.start.max(1) - 1) as usize;

    if span.end == Span::EOF {
        let h = nearest_unique(&lines, &r.anchor.head, pref, usize::MAX)?;
        return Some(Span {
            start: (h + 1) as u32,
            end: Span::EOF,
        });
    }
    let (h, t) = best_pair(&lines, &r.anchor, pref)?;
    Some(Span {
        start: (h + 1) as u32,
        end: (t + 1) as u32,
    })
}

/// 先頭候補と末尾候補の組を、`(先頭のズレ, 行数のズレ)` の辞書順で選ぶ。
/// 最良が 2 つ以上あれば「曖昧」として `None`。
fn best_pair(lines: &[&str], anchor: &Anchor, pref: usize) -> Option<(usize, usize)> {
    let want_len = (anchor.len.max(1) as usize).min(lines.len().max(1));
    let radius = want_len.max(ANCHOR_MIN_RADIUS).min(ANCHOR_MAX_RADIUS);

    let mut cands: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == anchor.head)
        .map(|(i, _)| i)
        .collect();
    if cands.is_empty() {
        return None;
    }
    // 元の位置に近い順。同距離は添字の小さいほうを先に見る (決定的)。
    cands.sort_by_key(|i| (i.abs_diff(pref), *i));
    cands.truncate(ANCHOR_CANDIDATES);

    let mut best: Option<((usize, usize), (usize, usize))> = None;
    let mut tied = false;
    for &h in &cands {
        let t = if anchor.tail.is_empty() || want_len == 1 {
            h
        } else {
            match nearest_unique(lines, &anchor.tail, h + want_len - 1, radius) {
                Some(t) if t >= h => t,
                _ => continue,
            }
        };
        let score = (h.abs_diff(pref), (t + 1 - h).abs_diff(want_len));
        match best {
            None => {
                best = Some((score, (h, t)));
                tied = false;
            }
            Some((bs, _)) if score < bs => {
                best = Some((score, (h, t)));
                tied = false;
            }
            Some((bs, _)) if score == bs => tied = true,
            _ => {}
        }
    }
    if tied {
        return None;
    }
    best.map(|(_, ht)| ht)
}

/// `pref` から半径 `radius` 以内で `want` に**一意に**いちばん近い行を返す。
///
/// 同じ距離に 2 つあれば `None` (曖昧)。比較は `trim()` 済み同士なので、
/// インデントだけが変わった行にも当たる。
fn nearest_unique(lines: &[&str], want: &str, pref: usize, radius: usize) -> Option<usize> {
    if want.is_empty() || lines.is_empty() {
        return None;
    }
    let lo = pref.saturating_sub(radius);
    let hi = pref.saturating_add(radius).min(lines.len() - 1);
    if lo > hi {
        return None;
    }
    let mut best: Option<usize> = None;
    let mut best_d = usize::MAX;
    let mut tied = false;
    for (i, l) in lines.iter().enumerate().take(hi + 1).skip(lo) {
        if l.trim() != want {
            continue;
        }
        let d = i.abs_diff(pref);
        if d < best_d {
            best_d = d;
            best = Some(i);
            tied = false;
        } else if d == best_d {
            tied = true;
        }
    }
    if tied {
        return None;
    }
    best
}

/// 他人の編集で行がずれた後、自分の行域を追従させる。
///
/// 行の対応付けは [`crate::marks::map_lines`] を再利用する。
/// 対応が取れない (自分の域が丸ごと消された) 場合は `false` を返し、
/// 呼び出し側は**確保し直す**。
///
/// # 端が消えても諦めない
///
/// 素朴に「先頭行と末尾行の両方が対応しなければ失敗」にすると、他人が
/// 自分の域の**1 行上**を書き換えただけで域を失う。ここでは域の中で
/// **生き残った最初の行と最後の行**へ縮める。縮む方向は常に安全
/// (持つ行が減るだけで、他人の行を掴むことはない)。
///
/// # 太りすぎたら失ったとみなす
///
/// [`crate::marks::map_lines`] は行数が大きいと LCS を諦めて共通の接頭辞/接尾辞
/// しか返さない。そのとき素朴に「最初と最後の生存行」を採ると、間に挟まった
/// **他人の数千行を自分の域として飲み込む**。域の伸びがファイル全体の増減を
/// 超えたら追従に失敗したとみなす。
pub fn follow(r: &mut Region, old_text: &str, new_text: &str) -> bool {
    let Some(span) = r.span else {
        return true; // ファイル全体は動かない
    };
    let old = lines_of(old_text);
    let new = lines_of(new_text);
    if old.is_empty() || new.is_empty() {
        return false;
    }
    let eof = span.end == Span::EOF;
    let lo = (span.start.max(1) - 1) as usize;
    if lo >= old.len() {
        return false;
    }
    let hi = if eof {
        old.len() - 1
    } else {
        ((span.end as usize).saturating_sub(1)).min(old.len() - 1)
    };
    if hi < lo {
        return false;
    }
    let map = crate::marks::map_lines(&old, &new);
    let mut first: Option<usize> = None;
    let mut last: Option<usize> = None;
    for i in lo..=hi {
        if let Some(Some(j)) = map.get(i).copied() {
            if first.is_none() {
                first = Some(j);
            }
            last = Some(j);
        }
    }
    let (Some(s0), Some(e0)) = (first, last) else {
        return false;
    };
    if e0 < s0 {
        return false;
    }
    let budget = (hi - lo + 1) + new.len().abs_diff(old.len());
    if e0 - s0 + 1 > budget {
        return false;
    }
    let s = (s0 + 1) as u32;
    let e = if eof { Span::EOF } else { (e0 + 1) as u32 };
    r.span = Some(Span { start: s, end: e });
    r.anchor = capture_anchor(new_text, &Span { start: s, end: e });
    true
}

/// 書き込みの前後から「実際に触れた行域」を出す (**新しいファイルの行番号**)。
///
/// リースの関門 ([`crate::lease::gate`]) はこれを使って
/// **「持っている域の中だけを書いたか」**を判定する。持っていない域へ
/// 1 行でもはみ出したら止める。
///
/// # 取りこぼしゼロが要件 — 迷ったら多めに出す
///
/// 触った行を 1 行でも**報告し忘れる**と、関門がその書き込みを通してしまい
/// **衝突が漏れる**。逆に多めに出しても、関門が拒否して確保し直させるだけで
/// 安全は壊れない。よってここは一貫して**安全側 = 多め**に倒す:
///
/// * 行の入れ替えのように [`crate::marks::map_lines`] が片方しか対応付け
///   られないケースでも、**変化したハンク全体**を出す (対応が付いた行の
///   間に挟まった行は全部触ったとみなす)
/// * 純粋な削除は、削除点の**直後の行**を触ったとして出す (git のハンクと同じ)
/// * `band` 行以内に並んだ域は 1 つに畳む (git のハンクと同じ挙動に揃える。
///   畳まないと「別々の小さな域」に見えて判定が甘くなる)
/// * **末尾改行が増減しただけ**でも最終行を触ったとして出す。`str::lines` は
///   `"a\nb\n"` と `"a\nb"` を同じ行列に見せるので、明示的に見ないと落ちる
/// * ファイルを**全消し**したら `1..EOF` を返す。`1..1` だと「1 行しか
///   持っていない人が 100 行のファイルを消せる」ことになり、これは取りこぼし
pub fn touched_spans(old_text: &str, new_text: &str, band: u32) -> Vec<Span> {
    let old = lines_of(old_text);
    let new = lines_of(new_text);

    if new.is_empty() {
        return if old.is_empty() {
            Vec::new()
        } else {
            // 全消し = 全行を触った
            vec![Span {
                start: 1,
                end: Span::EOF,
            }]
        };
    }
    if old.is_empty() {
        // 新規作成 = 全行が新しい
        return vec![Span {
            start: 1,
            end: new.len() as u32,
        }];
    }

    let map = crate::marks::map_lines(&old, &new);
    let mut hit = vec![false; new.len()];

    // 対応が付いた組は old / new の両方で単調増加する。隣り合う 2 組の
    // 「間」が変化したハンクなので、そこを new の行番号で塗る。
    let mut pairs: Vec<(usize, usize)> = map
        .iter()
        .enumerate()
        .filter_map(|(i, to)| to.map(|j| (i, j)))
        .collect();
    pairs.push((old.len(), new.len())); // 番兵 (末尾のハンクを拾う)

    let (mut pi, mut pj) = (-1i64, -1i64);
    for (i, j) in pairs {
        let (i, j) = (i as i64, j as i64);
        let old_gap = i - pi - 1;
        let new_gap = j - pj - 1;
        if new_gap > 0 {
            for k in (pj + 1)..j {
                if let Some(h) = hit.get_mut(k as usize) {
                    *h = true;
                }
            }
        } else if old_gap > 0 {
            // 純粋な削除。削除点の直後 (末尾なら最終行) を触ったとみなす
            let at = (pj + 1).clamp(0, new.len() as i64 - 1) as usize;
            if let Some(h) = hit.get_mut(at) {
                *h = true;
            }
        }
        pi = i;
        pj = j;
    }

    // 末尾改行の増減は行の内容に出ない
    if old_text.ends_with('\n') != new_text.ends_with('\n') {
        if let Some(h) = hit.last_mut() {
            *h = true;
        }
    }

    // 連続 (band 以内) をまとめる
    let mut out: Vec<Span> = Vec::new();
    let mut cur: Option<Span> = None;
    for (j, h) in hit.iter().enumerate() {
        if !*h {
            continue;
        }
        let n = (j + 1) as u32;
        match cur {
            Some(ref mut s) if n.saturating_sub(s.end) <= band => s.end = n,
            Some(s) => {
                out.push(s);
                cur = Some(Span::line(n));
            }
            None => cur = Some(Span::line(n)),
        }
    }
    if let Some(s) = cur {
        out.push(s);
    }
    out
}

/// 触れた行域が、持っている行域の**中に収まっている**か。
///
/// `owned` が空なら「何も持っていない」= 収まらない。
pub fn within(owned: &[Span], touched: &[Span]) -> bool {
    touched.iter().all(|t| {
        owned
            .iter()
            .any(|o| o.start <= t.start && (o.end == Span::EOF || o.end >= t.end))
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. 記号で域を指す (Rust だけ)
// ═══════════════════════════════════════════════════════════════════════════

/// Rust のソースから `kind name` の項目が占める行域を出す (1 始まり・両端含む)。
///
/// # 何を含めるか
///
/// 直上に連続する `#[...]` 属性と `///` doc コメントは**その項目のもの**として
/// 域に含める。持ち主が自分の doc を直せないと使い物にならないため。
/// 直上の素の `//` コメントは**含めない** (前の項目の締めのコメントかもしれず、
/// 誤って他人の行を掴むほうが高くつく)。
///
/// # 終わりの決め方
///
/// 波括弧の深さを数える。**文字列・生文字列・文字リテラル・行コメント・
/// ブロックコメントの中の括弧は数えない** — `println!("}}")` や `'{'` で
/// 深さが狂うと、域が数百行ずれて他人の領域を飲み込む。
/// 本体を持たない項目 (`struct Foo;` / `type X = Y;`) は `;` の行で終わる。
pub fn symbol_span(text: &str, kind: &str, name: &str) -> Option<Span> {
    let lines = lines_of(text);
    let def = lines.iter().position(|l| def_at(l, kind, name))?;
    let end = item_end(&lines, def)?;
    let mut start = def;
    while start > 0 {
        let prev = lines[start - 1].trim_start();
        if prev.starts_with("#[") || prev.starts_with("#![") || prev.starts_with("///") {
            start -= 1;
        } else {
            break;
        }
    }
    Some(Span {
        start: (start + 1) as u32,
        end: (end + 1) as u32,
    })
}

/// 行が `kind name` の定義行か。
///
/// `kind` の手前に置いてよいのは修飾語だけ (`pub` / `pub(crate)` / `async` /
/// `unsafe` / `const` / `extern "C"` / `default`)。これで `pub const fn f` の
/// `fn` にも当たり、`let fn_name = ..` のような**ただの登場**には当たらない。
fn def_at(line: &str, kind: &str, name: &str) -> bool {
    const QUALIFIERS: &[&str] = &["async", "unsafe", "const", "extern", "default", "static"];
    let mut rest = line.trim_start();
    loop {
        if let Some(after) = rest.strip_prefix(kind) {
            if !after.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
                return name_follows(after, name, kind);
            }
        }
        // 修飾語を 1 つ食べる
        let word_end = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let word = &rest[..word_end];
        if word.is_empty() {
            return false;
        }
        let tail = &rest[word_end..];
        let mut tail = tail;
        if word == "pub" {
            // `pub(crate)` / `pub(in path)` の括弧を飛ばす
            if let Some(inner) = tail.strip_prefix('(') {
                match inner.find(')') {
                    Some(k) => tail = &inner[k + 1..],
                    None => return false,
                }
            }
        } else if word == "extern" {
            // `extern "C"` の ABI 文字列を飛ばす
            let t = tail.trim_start();
            if let Some(q) = t.strip_prefix('"') {
                match q.find('"') {
                    Some(k) => tail = &q[k + 1..],
                    None => return false,
                }
            }
        } else if word != "pub" && !QUALIFIERS.contains(&word) {
            return false;
        }
        let next = tail.trim_start();
        if next.len() == tail.len() && !next.starts_with('<') {
            // 空白で区切られていない = 修飾語ではなかった
            return false;
        }
        rest = next;
    }
}

/// 定義キーワードの直後に `name` が来ているか。
fn name_follows(after: &str, name: &str, kind: &str) -> bool {
    let t = after.trim_start();
    if t.len() == after.len() {
        return false; // キーワードと名前がくっついている
    }
    if kind == "impl" {
        // `impl<T> Foo for Bar<T>` — ジェネリクスを飛ばし、以降に名前が
        // **語として**出てくれば当たりとする (`impl Foo` / `impl X for Foo` の両方)
        return t
            .split(|c: char| !(c.is_alphanumeric() || c == '_'))
            .any(|w| w == name);
    }
    let Some(rest) = t.strip_prefix(name) else {
        return false;
    };
    !rest.starts_with(|c: char| c.is_alphanumeric() || c == '_')
}

/// 字句の状態。行をまたいで持ち越すものだけを持つ。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Lex {
    Code,
    Block(u32),
    Str,
    Raw(usize),
}

/// `start` 行から始まる項目が終わる行 (0 始まり)。
fn item_end(lines: &[&str], start: usize) -> Option<usize> {
    let mut st = Lex::Code;
    let mut depth: i32 = 0;
    let mut opened = false;
    for (off, raw) in lines.iter().enumerate().skip(start) {
        let ch: Vec<char> = raw.chars().collect();
        let mut i = 0usize;
        while i < ch.len() {
            match st {
                Lex::Block(d) => {
                    if ch[i] == '*' && ch.get(i + 1) == Some(&'/') {
                        st = if d <= 1 { Lex::Code } else { Lex::Block(d - 1) };
                        i += 2;
                    } else if ch[i] == '/' && ch.get(i + 1) == Some(&'*') {
                        st = Lex::Block(d + 1);
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                Lex::Str => {
                    if ch[i] == '\\' {
                        i += 2;
                    } else {
                        if ch[i] == '"' {
                            st = Lex::Code;
                        }
                        i += 1;
                    }
                    continue;
                }
                Lex::Raw(h) => {
                    if ch[i] == '"' && ch.iter().skip(i + 1).take(h).all(|c| *c == '#') {
                        st = Lex::Code;
                        i += 1 + h;
                    } else {
                        i += 1;
                    }
                    continue;
                }
                Lex::Code => {}
            }
            let c = ch[i];
            if c == '/' && ch.get(i + 1) == Some(&'/') {
                break; // 行コメント: 行末まで無視
            }
            if c == '/' && ch.get(i + 1) == Some(&'*') {
                st = Lex::Block(1);
                i += 2;
                continue;
            }
            if c == '"' {
                st = Lex::Str;
                i += 1;
                continue;
            }
            if (c == 'r' || c == 'b')
                && !i
                    .checked_sub(1)
                    .and_then(|k| ch.get(k))
                    .is_some_and(|p| p.is_alphanumeric() || *p == '_')
            {
                // `r"..."` / `r#"..."#` / `br#"..."#`
                let mut k = i + 1;
                if c == 'b' && ch.get(k) == Some(&'r') {
                    k += 1;
                }
                let hs = ch[k..].iter().take_while(|c| **c == '#').count();
                if ch.get(k + hs) == Some(&'"') {
                    st = Lex::Raw(hs);
                    i = k + hs + 1;
                    continue;
                }
            }
            if c == '\'' {
                // 文字リテラルか、ライフタイム (`'a`) か
                if ch.get(i + 1) == Some(&'\\') {
                    let close = ch.iter().skip(i + 2).position(|c| *c == '\'');
                    i += close.map(|k| k + 3).unwrap_or(2);
                } else if ch.get(i + 2) == Some(&'\'') {
                    i += 3;
                } else {
                    i += 1; // ライフタイム
                }
                continue;
            }
            if c == '{' {
                depth += 1;
                opened = true;
                i += 1;
                continue;
            }
            if c == '}' {
                depth -= 1;
                i += 1;
                if opened && depth <= 0 {
                    return Some(off);
                }
                continue;
            }
            if c == ';' && !opened && depth == 0 {
                return Some(off); // 本体を持たない項目
            }
            i += 1;
        }
    }
    None
}

// ═══════════════════════════════════════════════════════════════════════════
//  テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    // ───────────────────────────────────────────────────────────────────────
    //  表記
    // ───────────────────────────────────────────────────────────────────────

    #[test]
    fn 仕様文字列を往復できる() {
        for (spec, want) in [
            ("src/a.rs", "src/a.rs"),
            ("src/a.rs#L10-40", "src/a.rs#L10-40"),
            ("src/a.rs#L10+3", "src/a.rs#L10-12"),
            ("src/a.rs#L10-", "src/a.rs#L10-"),
            ("src/a.rs#L7", "src/a.rs#L7"),
        ] {
            let r = parse(spec).expect(spec);
            assert_eq!(render(&r), want, "spec={spec}");
        }
    }

    #[test]
    fn 壊れた指定は素直に断る() {
        for bad in ["", "src/a.rs#L0", "src/a.rs#L5-2", "src/a.rs#Lx-3"] {
            assert!(parse(bad).is_err(), "通ってはいけない: {bad}");
        }
    }

    #[test]
    fn 記号指定を往復できる() {
        for spec in [
            "src/a.rs#fn:draw_toolbar",
            "src/a.rs#struct:Region",
            "src/a.rs#impl:Span",
            "src/a.rs#L10-40",
            "src/a.rs",
        ] {
            let s = parse_spec(spec).expect(spec);
            assert_eq!(render_spec(&s), spec, "spec={spec}");
        }
        // 記号は行域に落ちないので parse は断る (黙って全体を掴まない)
        assert!(parse("src/a.rs#fn:draw_toolbar").is_err());
        // 知らない種別は「パスの一部」として素通り (従来どおり)
        let s = parse_spec("src/a.rs#note:x").unwrap();
        assert_eq!(s.sel, Sel::Whole);
        assert_eq!(s.path, "src/a.rs#note:x");
    }

    // ───────────────────────────────────────────────────────────────────────
    //  重なり判定
    // ───────────────────────────────────────────────────────────────────────

    #[test]
    fn 安全帯より離れていれば同時に持てる() {
        let a = Span { start: 1, end: 10 };
        // 11,12,13 が空くので gap = 3
        let ok = Span { start: 14, end: 20 };
        let ng = Span { start: 13, end: 20 };
        assert!(!spans_too_close(&a, &ok, SAFE_BAND));
        assert!(spans_too_close(&a, &ng, SAFE_BAND));
        // 順序を入れ替えても同じ判定
        assert!(!spans_too_close(&ok, &a, SAFE_BAND));
        assert!(spans_too_close(&ng, &a, SAFE_BAND));
    }

    #[test]
    fn 末尾までの域は必ず衝突する() {
        let a = Span {
            start: 10,
            end: Span::EOF,
        };
        assert!(spans_too_close(
            &a,
            &Span {
                start: 9999,
                end: 9999
            },
            SAFE_BAND
        ));
    }

    #[test]
    fn ファイル全体は行域と必ず衝突する() {
        let whole = Region::whole("src/a.rs");
        let part = parse("src/a.rs#L100-110").unwrap();
        assert!(conflicts(&whole, &part, SAFE_BAND));
        assert!(conflicts(&part, &whole, SAFE_BAND));
    }

    #[test]
    fn 別ファイルなら行域が重なっていても衝突しない() {
        let a = parse("src/a.rs#L1-10").unwrap();
        let b = parse("src/b.rs#L1-10").unwrap();
        assert!(!conflicts(&a, &b, SAFE_BAND));
    }

    #[test]
    fn globが混ざったら安全側に倒す() {
        let a = parse("src/*.rs#L1-10").unwrap();
        let b = parse("src/a.rs#L900-910").unwrap();
        assert!(
            conflicts(&a, &b, SAFE_BAND),
            "glob は行域で切り分けられない"
        );
    }

    #[test]
    fn 互いに素な一覧はそのまま証明になる() {
        let list: Vec<Region> = ["src/a.rs#L1-10", "src/a.rs#L14-20", "src/a.rs#L24-30"]
            .iter()
            .map(|s| parse(s).unwrap())
            .collect();
        assert!(is_disjoint(&list, SAFE_BAND));
        assert_eq!(conflicting_pairs(&list, SAFE_BAND), vec![]);
    }

    #[test]
    fn 近すぎる組は組として出る() {
        let list: Vec<Region> = ["src/a.rs#L1-10", "src/a.rs#L12-20"]
            .iter()
            .map(|s| parse(s).unwrap())
            .collect();
        assert_eq!(conflicting_pairs(&list, SAFE_BAND), vec![(0, 1)]);
    }

    // ───────────────────────────────────────────────────────────────────────
    //  実 git で安全帯を測る
    // ───────────────────────────────────────────────────────────────────────

    /// git を起こせるか。無い環境 (CI の最小コンテナ等) ではテストを綺麗に飛ばす。
    ///
    /// **バージョン番号は読まない。** `git merge-tree --write-tree` の可否を
    /// 「2.38 以上か」で決めると、バックポート版・機能削除版を必ず取り違える
    /// (`crate::conflict` が同じ理由で番号判定を撤回した)。ここでは能力の判定を
    /// そもそも持たず、**コマンドを撃って終了コードで分類する** ([`Lab::merge_tree`])。
    fn git_available() -> bool {
        Command::new("git")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// 変更の種類。**種類で下限が変わらないこと**を確かめるために全部回す。
    #[derive(Clone, Copy, Debug)]
    enum Kind {
        /// 行を置き換える
        Replace,
        /// 行を消す
        Delete,
        /// 行の手前に足す
        Insert,
    }

    const KINDS: &[Kind] = &[Kind::Replace, Kind::Delete, Kind::Insert];

    fn base_text(n: u32) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    /// `at` (1 始まり) を起点に `width` 行ぶん `kind` の変更を当てる。
    fn edit(base: &str, kind: Kind, at: u32, width: u32, tag: &str) -> String {
        let mut out = String::new();
        for (i, l) in base.lines().enumerate() {
            let n = (i + 1) as u32;
            let inside = n >= at && n < at + width;
            match kind {
                Kind::Replace if inside => out.push_str(&format!("{tag} {n}\n")),
                Kind::Delete if inside => {}
                Kind::Insert if n == at => {
                    for k in 0..width {
                        out.push_str(&format!("{tag}-new {k}\n"));
                    }
                    out.push_str(l);
                    out.push('\n');
                }
                _ => {
                    out.push_str(l);
                    out.push('\n');
                }
            }
        }
        out
    }

    /// git を起こす実験台。`std::env::temp_dir()` 由来の一意な作業場所を使う
    /// (パス直書き禁止・実 `~/.zaivern` に触れない)。
    struct Lab {
        dir: PathBuf,
    }

    impl Lab {
        fn new(tag: &str) -> Lab {
            let dir = crate::test_util::unique_temp_dir("zai-region", tag);
            for sub in ["a", "b", "w"] {
                std::fs::create_dir_all(dir.join(sub)).expect("create sub dir");
            }
            Lab { dir }
        }

        /// 環境に左右されない git。ユーザーの ~/.gitconfig / システム設定を遮断し、
        /// CRLF 変換も止める (Windows で内容が化けると計測が嘘になる)。
        fn git(&self) -> Command {
            let mut c = Command::new("git");
            c.env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", self.dir.join("no-such-gitconfig"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .arg("-c")
                .arg("core.autocrlf=false")
                .arg("-c")
                .arg("core.safecrlf=false");
            c
        }

        /// 三方向マージ。**衝突したら `true`**。
        fn merge_file(&self, base: &str, ours: &str, theirs: &str) -> bool {
            let (bp, op, tp) = (
                self.dir.join("base.txt"),
                self.dir.join("ours.txt"),
                self.dir.join("theirs.txt"),
            );
            std::fs::write(&bp, base).unwrap();
            std::fs::write(&op, ours).unwrap();
            std::fs::write(&tp, theirs).unwrap();
            let out = self
                .git()
                .args(["merge-file", "-p"])
                .args([&op, &bp, &tp])
                .output()
                .expect("run git merge-file");
            let code = out.status.code().unwrap_or(-1);
            assert!(
                (0..=127).contains(&code),
                "git merge-file が失敗した: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            let text = String::from_utf8_lossy(&out.stdout);
            // 終了コード = 衝突数。マーカの有無でも二重に確かめる
            (code != 0) || text.contains("<<<<<<<")
        }

        /// パッチ適用 (文脈 3 行)。**適用できたら `true`**。
        ///
        /// リポジトリを作らずに測る: base と theirs を別ディレクトリに置いて
        /// `git diff --no-index` でパッチを取り、ours を置いた作業場で
        /// `git apply -p2` する。`git init` + 3 コミットより **8 倍速い**。
        fn apply_patch(&self, base: &str, ours: &str, theirs: &str) -> bool {
            std::fs::write(self.dir.join("a/f.txt"), base).unwrap();
            std::fs::write(self.dir.join("b/f.txt"), theirs).unwrap();
            std::fs::write(self.dir.join("w/f.txt"), ours).unwrap();
            let diff = self
                .git()
                .current_dir(&self.dir)
                .args(["diff", "--no-index", "--", "a/f.txt", "b/f.txt"])
                .output()
                .expect("run git diff --no-index");
            let patch = self.dir.join("p.patch");
            std::fs::write(&patch, &diff.stdout).unwrap();
            self.git()
                .current_dir(self.dir.join("w"))
                .args(["apply", "--check", "-p2"])
                .arg(&patch)
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        }

        /// 実ツリーの三方向マージ。**衝突したら `Some(true)`**。
        ///
        /// `--write-tree` を持たない git では `None` を返して綺麗に降格する。
        /// **可否をバージョン番号で推定しない** — 実際に撃って終了コードで分ける
        /// (0 = 綺麗、1 = 衝突、それ以外 = この git では判断しない)。番号での推定は
        /// バックポート版・機能削除版を必ず取り違える。
        fn merge_tree(&self, base: &str, ours: &str, theirs: &str) -> Option<bool> {
            let repo = self.dir.join("repo");
            let _ = std::fs::remove_dir_all(&repo);
            std::fs::create_dir_all(&repo).unwrap();
            let run = |args: &[&str]| -> bool {
                self.git()
                    .current_dir(&repo)
                    .args([
                        "-c",
                        "user.name=zai",
                        "-c",
                        "user.email=zai@example.invalid",
                        "-c",
                        "commit.gpgsign=false",
                        "-c",
                        "init.defaultBranch=main",
                    ])
                    .args(args)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            };
            if !run(&["init", "-q"]) {
                return None;
            }
            let f = repo.join("f.txt");
            std::fs::write(&f, base).unwrap();
            run(&["add", "-A"]);
            run(&["commit", "-qm", "base"]);
            run(&["checkout", "-qb", "ours"]);
            std::fs::write(&f, ours).unwrap();
            run(&["commit", "-qam", "ours"]);
            run(&["checkout", "-q", "-"]);
            run(&["checkout", "-qb", "theirs"]);
            std::fs::write(&f, theirs).unwrap();
            run(&["commit", "-qam", "theirs"]);
            let out = self
                .git()
                .current_dir(&repo)
                .args(["merge-tree", "--write-tree", "ours", "theirs"])
                .output()
                .ok()?;
            match out.status.code() {
                Some(0) => Some(false),
                Some(1) => Some(true),
                _ => None, // 引数を知らない古い git 等 — 判断しない
            }
        }
    }

    /// 自分側 / 相手側の種類を選んで `gap` 行あけた 3 つ組を作る。
    /// 自分の域は常に `[20, 20+width-1]`、相手は `21+gap` から。
    fn triple(
        ours_kind: Kind,
        theirs_kind: Kind,
        gap: u32,
        width: u32,
    ) -> (String, String, String) {
        let base = base_text(200);
        let ours = edit(&base, ours_kind, 20, width, "OURS");
        let theirs = edit(&base, theirs_kind, 20 + width + gap, width, "THEIRS");
        (base, ours, theirs)
    }

    /// `ours` は常に 20 行目を置換 (幅 1)。`theirs` は種別ごとに `gap` 行あけて当てる。
    fn pair(kind: Kind, gap: u32, width: u32) -> (String, String, String) {
        let base = base_text(200);
        let ours = edit(&base, Kind::Replace, 20, 1, "OURS");
        // ours の域は [20,20]。gap 行あけるので theirs は 21+gap から
        let theirs = edit(&base, kind, 21 + gap, width, "THEIRS");
        (base, ours, theirs)
    }

    /// **この製品の全ての主張を支える定数の実測**。
    ///
    /// 「間隔 `SAFE_BAND-1` では通らず、`SAFE_BAND` では通る」を、実際の git で
    /// 変更種別ごとに確かめる。いちばん厳しい経路 (パッチ適用) が下限を決める。
    #[test]
    fn 実gitで安全帯の下限を測る() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("band");
        let mut worst = 0u32;
        for &kind in KINDS {
            for width in [1u32, 3] {
                // 下限 = 「そこから上は全部通る」いちばん小さい gap
                let mut lower: Option<u32> = None;
                for gap in 0..=6u32 {
                    let (b, o, t) = pair(kind, gap, width);
                    let merge_ok = !lab.merge_file(&b, &o, &t);
                    let apply_ok = lab.apply_patch(&b, &o, &t);
                    if merge_ok && apply_ok {
                        if lower.is_none() {
                            lower = Some(gap);
                        }
                    } else {
                        lower = None; // 途中で落ちたら測り直し
                    }
                }
                let lower = lower.unwrap_or_else(|| panic!("{kind:?}/{width} で下限が出ない"));
                worst = worst.max(lower);
                assert!(
                    lower <= SAFE_BAND,
                    "{kind:?}/{width} の下限 {lower} が SAFE_BAND({SAFE_BAND}) を超えた \
                     — 安全帯を上げること"
                );
            }
        }
        assert_eq!(
            worst, SAFE_BAND,
            "いちばん厳しい変更種別の下限が {worst} なのに SAFE_BAND は {SAFE_BAND} \
             — 一致させること"
        );
    }

    /// 三方向マージだけなら 1 行で足りる ([`MERGE_ONLY_BAND`] の根拠)。
    #[test]
    fn 実gitの三方向マージは1行あれば足りる() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("merge");
        for &kind in KINDS {
            for width in [1u32, 3] {
                // 自分側が置換なら、相手の種類によらず gap = 0 は必ず衝突する
                let (b, o, t) = pair(kind, 0, width);
                assert!(
                    lab.merge_file(&b, &o, &t),
                    "{kind:?}/{width}: gap=0 は衝突するはず"
                );
                // gap >= MERGE_ONLY_BAND なら必ず通る
                for gap in MERGE_ONLY_BAND..=4 {
                    let (b, o, t) = pair(kind, gap, width);
                    assert!(
                        !lab.merge_file(&b, &o, &t),
                        "{kind:?}/{width}: gap={gap} は通るはず"
                    );
                }
            }
        }
        // 唯一の例外: 両側とも挿入なら隣接していても通る (別々の位置なら)。
        // だが**同じ位置**への挿入は必ず衝突するので、緩和は入れていない。
        let base = base_text(200);
        let o = edit(&base, Kind::Insert, 20, 1, "OURS");
        assert!(
            !lab.merge_file(&base, &o, &edit(&base, Kind::Insert, 21, 1, "THEIRS")),
            "別々の位置への挿入どうしは隣接しても通るはず"
        );
        assert!(
            lab.merge_file(&base, &o, &edit(&base, Kind::Insert, 20, 1, "THEIRS")),
            "同じ位置への挿入は衝突するはず"
        );
    }

    /// パッチ適用の下限が 3 であること (= [`SAFE_BAND`] の由来)。
    #[test]
    fn 実gitでパッチ適用の下限を測る() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("apply");
        for &kind in KINDS {
            for gap in 0..SAFE_BAND {
                let (b, o, t) = pair(kind, gap, 1);
                assert!(
                    !lab.apply_patch(&b, &o, &t),
                    "{kind:?}: gap={gap} でパッチが当たってしまった \
                     (文脈 3 行の前提が変わった → SAFE_BAND を測り直すこと)"
                );
            }
            for gap in SAFE_BAND..=5 {
                let (b, o, t) = pair(kind, gap, 1);
                assert!(
                    lab.apply_patch(&b, &o, &t),
                    "{kind:?}: gap={gap} は当たるはず"
                );
            }
        }
    }

    /// 実ツリーの三方向マージでも同じ結論になるか。git 2.38 未満では飛ばす。
    #[test]
    fn 実ツリーのマージでも同じ結論になる() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("tree");
        let (b, o, t) = pair(Kind::Replace, 0, 1);
        let Some(c0) = lab.merge_tree(&b, &o, &t) else {
            eprintln!("git merge-tree --write-tree が使えないので飛ばす");
            return;
        };
        assert!(c0, "gap=0 は実ツリーでも衝突するはず");
        for gap in [MERGE_ONLY_BAND, SAFE_BAND] {
            let (b, o, t) = pair(Kind::Replace, gap, 1);
            assert_eq!(
                lab.merge_tree(&b, &o, &t),
                Some(false),
                "gap={gap} は実ツリーでも通るはず"
            );
        }
    }

    /// 「近すぎる」と「実際に衝突する」の差を、変更の種類と間隔ごとに数える。
    ///
    /// 実測ベンチ (64 体が 1 ファイル 2000 行へぶつかる条件) では
    /// [`spans_too_close`] が真と言った **246 組**のうち、実際に git が衝突したのは
    /// **48 組**だった (約 5.1 倍の過剰報告)。**この乖離は不具合ではなく仕様**である:
    /// 安全帯が引いているのは「衝突する」線ではなく「衝突し**得る**」線だから。
    ///
    /// 乖離の出どころは 2 つで、どちらもここで数えている:
    ///
    /// 1. **間隔 1〜2 行**。`spans_too_close` は `gap < 3` を全部「近すぎる」と言うが、
    ///    三方向マージが実際に衝突するのは `gap = 0` だけ ([`SAFE_BAND`] の表)。
    ///    つまり近すぎ判定 3 段のうち **2 段は空振り**で、これだけで 3 倍になる
    /// 2. **自分側が純粋な挿入**。行 `p` の手前への挿入は**行 `p` を書き換えない**ので、
    ///    実際の距離はモデルより 1 行ぶん広い。実測でも `gap = 0` の 9 通りのうち
    ///    「自分側が挿入」の 3 通りだけが綺麗に通った (相手の種類は問わない)
    ///
    /// **見逃しは 1 件も無い**ことをここで固定する — 実際に衝突した組は必ず
    /// 「近すぎる」と言えていること。過剰報告は確保し直させるだけで済むが、
    /// 見逃しは「一撃マージできる」という主張そのものを嘘にする。
    #[test]
    fn 近すぎる判定が実際の衝突より何倍多いかを数える() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("ratio");
        let (mut flagged, mut actual, mut both) = (0u32, 0u32, 0u32);
        // 空振りの内訳 (間隔で何件・両側挿入で何件)
        let (mut miss_by_gap, mut miss_by_pure_insert) = (0u32, 0u32);
        for &ok in KINDS {
            for &tk in KINDS {
                for gap in 0..=5u32 {
                    let ours_span = Span::line(20);
                    let theirs_span = Span::line(21 + gap);
                    let too_close = spans_too_close(&ours_span, &theirs_span, SAFE_BAND);
                    let (b, o, t) = triple(ok, tk, gap, 1);
                    let conflicted = lab.merge_file(&b, &o, &t);
                    // ★ 見逃しゼロ: 実 git が衝突したなら必ず近すぎと言えていること
                    assert!(
                        !conflicted || too_close,
                        "見逃し: ours={ok:?} theirs={tk:?} gap={gap} が衝突したのに \
                         spans_too_close が false — SAFE_BAND を上げること"
                    );
                    if too_close {
                        flagged += 1;
                        if conflicted {
                            both += 1;
                        } else if gap > 0 {
                            miss_by_gap += 1;
                        } else {
                            miss_by_pure_insert += 1;
                        }
                    }
                    if conflicted {
                        actual += 1;
                    }
                }
            }
        }
        eprintln!(
            "近すぎる={flagged} 実際に衝突={actual} 両方={both} \
             (空振り: 間隔 1〜2 で {miss_by_gap} 件 / 自分側が挿入のみで {miss_by_pure_insert} 件)"
        );
        // 安全帯の外で衝突したものは 1 件も無い (= 見逃しゼロの裏取り)
        assert_eq!(both, actual, "安全帯の外で衝突した組がある");
        // 3 種別 × 3 種別 × 間隔 3 段 = 27 組が「近すぎる」
        assert_eq!(flagged, 27);
        // 実際に衝突するのは間隔 0 の 9 組のうち、自分側が挿入の 3 組を除いた 6 組
        assert_eq!(actual, 6);
        assert_eq!(miss_by_gap, 18, "間隔 1〜2 の空振り");
        assert_eq!(miss_by_pure_insert, 3, "自分側が挿入のみの空振り");
        // 過剰報告は 27 / 6 = 4.5 倍。実測ベンチの 246 / 48 = 5.1 倍と同じ桁で、
        // 「近い = 衝突し得る」であって「近い = 必ず衝突」ではないことを示す。
        assert!(
            flagged >= actual * 4,
            "過剰報告の倍率が落ちた: {flagged}/{actual}"
        );
    }

    /// git の能力をバージョン番号で推定する経路が復活していないこと。
    ///
    /// `crate::conflict` が同じ判定を番号でやって取り違え、**叩いて確かめる**方式へ
    /// 撤回した。ここで同じ誤りをやり直さないよう構造で縛る (バックポート版・
    /// 機能削除版は番号から絶対に見分けられない)。
    ///
    /// なお `merge-tree --write-tree` の可否は、ここでは**予測せず**終了コードで
    /// 分類している ([`Lab::merge_tree`])。判定そのものを持たないので、
    /// `crate::conflict::merge_tree_available` と 2 実装になることもない。
    #[test]
    fn 能力判定にバージョン番号を読む経路が残っていない() {
        // Windows のチェックアウトは CRLF なので正規化してから探す。
        let src = include_str!("region.rs").replace("\r\n", "\n");
        // 探す文字列をそのまま書くと**このテスト自身に当たる**ので分割する。
        assert!(
            !src.contains(concat!("git_ver", "sion")),
            "git のバージョン番号で能力を判定している"
        );
        assert!(
            src.contains("fn git_available()"),
            "存在確認の入口が消えている"
        );
    }

    /// 末尾への追記同士は必ず衝突する (= EOF の域は誰とも同居できない)。
    #[test]
    fn 末尾追記どうしは実gitでも衝突する() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("eof");
        let base = base_text(60);
        let ours = format!("{base}OURS tail\n");
        let theirs = format!("{base}THEIRS tail\n");
        assert!(lab.merge_file(&base, &ours, &theirs));
        // 型のうえでも同じ結論が出る
        let a = Span {
            start: 61,
            end: Span::EOF,
        };
        assert!(spans_too_close(&a, &a, SAFE_BAND));
    }

    // ───────────────────────────────────────────────────────────────────────
    //  錨・追従
    // ───────────────────────────────────────────────────────────────────────

    fn numbered(n: u32) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    fn region_at(text: &str, span: Span) -> Region {
        Region {
            path: "a".into(),
            span: Some(span),
            anchor: capture_anchor(text, &span),
        }
    }

    #[test]
    fn 錨で行域を取り直せる() {
        let text = numbered(50);
        let span = Span { start: 10, end: 20 };
        let mut r = region_at(&text, span);
        // 上に 5 行足す → 15..25 へずれる
        let shifted = format!("x\nx\nx\nx\nx\n{text}");
        let got = resolve(&r, &shifted).expect("取り直せるはず");
        assert_eq!(got, Span { start: 15, end: 25 });
        // follow でも同じ結論になる
        assert!(follow(&mut r, &text, &shifted));
        assert_eq!(r.span, Some(Span { start: 15, end: 25 }));
    }

    #[test]
    fn 同じ内容の行が複数あるなら一番近いものを選ぶ() {
        let old = "a\nb\nDUP\nc\nx\nd\ne\nf\n";
        let r = region_at(old, Span { start: 3, end: 3 });
        assert_eq!(r.anchor.head, "DUP");
        // DUP が 3 行目と 8 行目にある → 元の位置 (3) に近い 3 行目
        let new = "a\nb\nDUP\nc\nx\nd\ne\nDUP\n";
        assert_eq!(resolve(&r, new), Some(Span { start: 3, end: 3 }));
        // 元の位置から消えて 5 行目と 8 行目にある → 距離 2 と 5 で 5 行目
        let new2 = "a\nb\nq\nc\nDUP\nd\ne\nDUP\n";
        assert_eq!(resolve(&r, new2), Some(Span { start: 5, end: 5 }));
    }

    #[test]
    fn 同距離に候補が二つあるなら取り直さない() {
        let old = "a\nb\nc\nd\nDUP\nf\ng\nh\n";
        let r = region_at(old, Span { start: 5, end: 5 });
        // DUP が 3 行目と 7 行目 (どちらも距離 2) → 曖昧なので断る
        let new = "a\nb\nDUP\nd\nz\nf\nDUP\nh\n";
        assert_eq!(resolve(&r, new), None, "曖昧なら掴まずに取り直させる");
    }

    #[test]
    fn 先頭行が消えたら取り直せない() {
        let text = numbered(50);
        let r = region_at(&text, Span { start: 10, end: 20 });
        let gone = text.replace("line 10\n", "");
        assert_eq!(resolve(&r, &gone), None);
    }

    #[test]
    fn 空行から始まる域は錨にならない() {
        let text = "a\n\nb\nc\n";
        let r = region_at(text, Span { start: 2, end: 3 });
        assert_eq!(r.anchor.head, "");
        assert_eq!(resolve(&r, "a\n\nb\nc\n"), None, "空行は錨として使えない");
    }

    #[test]
    fn 末尾までの域は先頭だけで取り直す() {
        let text = numbered(30);
        let span = Span {
            start: 25,
            end: Span::EOF,
        };
        let mut r = region_at(&text, span);
        // 自分で末尾へ追記しても、EOF の域は EOF のまま追従する
        let grown = format!("{text}line 31\nline 32\n");
        assert_eq!(
            resolve(&r, &grown),
            Some(Span {
                start: 25,
                end: Span::EOF
            })
        );
        assert!(follow(&mut r, &text, &grown));
        assert_eq!(
            r.span,
            Some(Span {
                start: 25,
                end: Span::EOF
            })
        );
        // 上に足されればずれる
        let shifted = format!("x\nx\n{grown}");
        assert_eq!(
            resolve(&r, &shifted),
            Some(Span {
                start: 27,
                end: Span::EOF
            })
        );
    }

    #[test]
    fn crlfでも同じ行域になる() {
        let lf = numbered(50);
        let crlf = lf.replace('\n', "\r\n");
        let span = Span { start: 10, end: 20 };
        assert_eq!(capture_anchor(&lf, &span), capture_anchor(&crlf, &span));
        let r = region_at(&lf, span);
        let shifted = format!("x\r\nx\r\n{crlf}");
        assert_eq!(resolve(&r, &shifted), Some(Span { start: 12, end: 22 }));
        // 改行コードを変えただけなら「触っていない」
        assert_eq!(touched_spans(&lf, &crlf, SAFE_BAND), vec![]);
    }

    #[test]
    fn 域の途中が消えても生き残りへ縮む() {
        let text = numbered(50);
        let mut r = region_at(&text, Span { start: 10, end: 20 });
        // 他人が 10 行目を消した (本当は起きてはいけないが、起きたら縮む)
        let cut = text.replace("line 10\n", "");
        assert!(follow(&mut r, &text, &cut));
        assert_eq!(
            r.span,
            Some(Span { start: 10, end: 19 }),
            "11..20 が 10..19 へ"
        );
    }

    #[test]
    fn 域が丸ごと消えたら正直に失敗する() {
        let text = numbered(50);
        let mut r = region_at(&text, Span { start: 10, end: 12 });
        let mut cut = text.clone();
        for i in 10..=12 {
            cut = cut.replace(&format!("line {i}\n"), "");
        }
        assert!(!follow(&mut r, &text, &cut), "取り直させるべき");
    }

    #[test]
    fn 巨大ファイルでも取り直しが速い() {
        // 4 万行。1 万行が `}` だけ、1 万行が空行 — 曖昧さの最悪ケース
        let mut v: Vec<String> = Vec::with_capacity(40_000);
        for i in 0..10_000 {
            v.push(format!("fn f{i}() {{"));
            v.push(format!("    let x = {i};"));
            v.push("}".to_string());
            v.push(String::new());
        }
        let text = v.join("\n") + "\n";
        // 20001..20003 = fn f5000() { / let x / }
        let span = Span {
            start: 20_001,
            end: 20_003,
        };
        let r = region_at(&text, span);
        assert_eq!(r.anchor.head, "fn f5000() {");
        let shifted = format!("// a\n// b\n// c\n{text}");
        let mut best = std::time::Duration::from_secs(3600);
        for _ in 0..5 {
            let t0 = std::time::Instant::now();
            let got = resolve(&r, &shifted);
            best = best.min(t0.elapsed());
            assert_eq!(
                got,
                Some(Span {
                    start: 20_004,
                    end: 20_006
                })
            );
        }
        // O(n) なので 4 万行でも数 ms。負荷で揺れるので**最小値**で判定する
        assert!(
            best < std::time::Duration::from_millis(300),
            "resolve が遅すぎる (4 万行で {best:?}) — 線形を超えていないか疑う"
        );
    }

    // ───────────────────────────────────────────────────────────────────────
    //  触れた行域
    // ───────────────────────────────────────────────────────────────────────

    #[test]
    fn 触れた行域を書き込みの前後から出せる() {
        let old = numbered(30);
        let mut lines: Vec<String> = old.lines().map(|s| s.to_string()).collect();
        lines[14] = "CHANGED".into();
        let new = lines.join("\n") + "\n";
        let spans = touched_spans(&old, &new, SAFE_BAND);
        assert_eq!(
            spans,
            vec![Span { start: 15, end: 15 }],
            "15 行目だけのはず"
        );
        assert!(within(&[Span { start: 10, end: 20 }], &spans));
        assert!(!within(&[Span { start: 1, end: 10 }], &spans));
    }

    #[test]
    fn 純粋な挿入の行域() {
        let old = numbered(30);
        let new = old.replace("line 10\n", "line 10\nNEW\n");
        // 挿入された行は新ファイルの 11 行目
        assert_eq!(
            touched_spans(&old, &new, SAFE_BAND),
            vec![Span { start: 11, end: 11 }]
        );
    }

    #[test]
    fn 純粋な削除の行域() {
        let old = numbered(30);
        let new = old.replace("line 11\n", "");
        // 削除点の直後 = 新ファイルの 11 行目 (中身は元の line 12)
        assert_eq!(
            touched_spans(&old, &new, SAFE_BAND),
            vec![Span { start: 11, end: 11 }]
        );
        // 「1..10 しか持っていない人」は通せない
        assert!(!within(
            &[Span { start: 1, end: 10 }],
            &touched_spans(&old, &new, SAFE_BAND)
        ));
    }

    #[test]
    fn 置換と削除が混ざっても取りこぼさない() {
        let old = numbered(30);
        let new = old
            .replace("line 12\nline 13\nline 14\n", "line 12\nMERGED\n")
            .replace("line 20\n", "TWENTY\n");
        let spans = touched_spans(&old, &new, SAFE_BAND);
        assert!(
            within(&[Span { start: 10, end: 25 }], &spans),
            "10..25 に収まるはず: {spans:?}"
        );
        assert!(
            !within(&[Span { start: 10, end: 17 }], &spans),
            "20 行目を取りこぼした"
        );
    }

    #[test]
    fn 行の入れ替えを取りこぼさない() {
        // 素朴な実装だと片方しか対応が付かず、動いた 2 行の片方を見落とす
        let old = "a\nB\nC\nd\ne\n";
        let new = "a\nC\nB\nd\ne\n";
        // band=0 では畳まないので 2 つの域として出る (どちらも取りこぼさない)
        let spans = touched_spans(old, new, 0);
        assert_eq!(
            spans,
            vec![Span { start: 2, end: 2 }, Span { start: 3, end: 3 }],
            "動いた 2 行を両方出す"
        );
        assert!(!within(&[Span { start: 3, end: 3 }], &spans));
        assert!(!within(&[Span { start: 2, end: 2 }], &spans));
        // 安全帯ぶん畳めば 1 つの域になる
        assert_eq!(
            touched_spans(old, new, SAFE_BAND),
            vec![Span { start: 2, end: 3 }]
        );
    }

    #[test]
    fn 末尾改行の増減を取りこぼさない() {
        // str::lines は "a\nb\n" と "a\nb" を同じ行列に見せる
        assert_eq!(
            touched_spans("a\nb\n", "a\nb", SAFE_BAND),
            vec![Span { start: 2, end: 2 }]
        );
        assert_eq!(
            touched_spans("a\nb", "a\nb\n", SAFE_BAND),
            vec![Span { start: 2, end: 2 }]
        );
        assert_eq!(touched_spans("a\nb\n", "a\nb\n", SAFE_BAND), vec![]);
    }

    #[test]
    fn ファイル新規作成と全消し() {
        // 新規作成 = 全行が新しい
        assert_eq!(
            touched_spans("", "x\ny\nz\n", SAFE_BAND),
            vec![Span { start: 1, end: 3 }]
        );
        // 全消し = 全行を触った。1 行だけ持っている人には通せない
        let all = touched_spans("x\ny\nz\n", "", SAFE_BAND);
        assert_eq!(
            all,
            vec![Span {
                start: 1,
                end: Span::EOF
            }]
        );
        assert!(
            !within(&[Span { start: 1, end: 3 }], &all),
            "全消しは全体の持ち主だけ"
        );
        assert!(within(
            &[Span {
                start: 1,
                end: Span::EOF
            }],
            &all
        ));
        // 何も無い → 何も無い
        assert_eq!(touched_spans("", "", SAFE_BAND), vec![]);
    }

    #[test]
    fn 離れた変更は畳まれない() {
        let old = numbered(40);
        let new = old
            .replace("line 5\n", "FIVE\n")
            .replace("line 30\n", "THIRTY\n");
        assert_eq!(
            touched_spans(&old, &new, SAFE_BAND),
            vec![Span { start: 5, end: 5 }, Span { start: 30, end: 30 }]
        );
    }

    #[test]
    fn 安全帯以内の変更は一つに畳む() {
        let old = numbered(40);
        // 10 行目と 13 行目 (間に 2 行) → gap 2 < SAFE_BAND なので 1 つの域
        let new = old
            .replace("line 10\n", "TEN\n")
            .replace("line 13\n", "THIRTEEN\n");
        assert_eq!(
            touched_spans(&old, &new, SAFE_BAND),
            vec![Span { start: 10, end: 13 }]
        );
    }

    // ───────────────────────────────────────────────────────────────────────
    //  記号で域を指す
    // ───────────────────────────────────────────────────────────────────────

    const SAMPLE: &str = r##"use std::fmt;

/// ツールバーを描く。
#[allow(dead_code)]
pub fn draw_toolbar(ui: &mut Ui) {
    let s = "} not a brace {";
    let c = '{';
    // } これも数えない
    if s.is_empty() {
        ui.label(r#"raw } string"#);
    }
}

pub struct Region {
    pub path: String,
}

impl Span {
    pub fn line(n: u32) -> Span {
        Span { start: n, end: n }
    }
}

pub struct Marker;

const MAX: u32 = 3;
"##;

    #[test]
    fn 記号で域を指せる() {
        // doc コメントと属性まで含める (3..12 行目)
        let f = symbol_span(SAMPLE, "fn", "draw_toolbar").expect("fn が見つからない");
        let lines: Vec<&str> = SAMPLE.lines().collect();
        assert_eq!(
            lines[(f.start - 1) as usize].trim(),
            "/// ツールバーを描く。"
        );
        assert_eq!(lines[(f.end - 1) as usize].trim(), "}");
        // 文字列 / 文字リテラル / コメント / 生文字列の括弧に釣られていない
        assert_eq!(f.end - f.start + 1, 10, "域が {f:?} まで伸びた");

        let s = symbol_span(SAMPLE, "struct", "Region").expect("struct が見つからない");
        assert_eq!(lines[(s.start - 1) as usize].trim(), "pub struct Region {");
        assert_eq!(lines[(s.end - 1) as usize].trim(), "}");

        let i = symbol_span(SAMPLE, "impl", "Span").expect("impl が見つからない");
        assert_eq!(lines[(i.start - 1) as usize].trim(), "impl Span {");
        assert_eq!(lines[(i.end - 1) as usize].trim(), "}");

        // 本体を持たない項目は `;` で終わる
        let u = symbol_span(SAMPLE, "struct", "Marker").expect("unit struct");
        assert_eq!(u.start, u.end);
        assert_eq!(lines[(u.start - 1) as usize].trim(), "pub struct Marker;");
        let c = symbol_span(SAMPLE, "const", "MAX").expect("const");
        assert_eq!(lines[(c.start - 1) as usize].trim(), "const MAX: u32 = 3;");

        assert_eq!(symbol_span(SAMPLE, "fn", "nope"), None);
        // `use std::fmt;` の中の `fmt` を型と誤認しない
        assert_eq!(symbol_span(SAMPLE, "struct", "fmt"), None);
    }

    #[test]
    fn 記号指定からテキストで行域へ落とせる() {
        let spec = parse_spec("src/a.rs#fn:draw_toolbar").unwrap();
        let r = resolve_spec(&spec, SAMPLE).expect("解決できるはず");
        assert_eq!(r.path, "src/a.rs");
        let sp = r.span.expect("行域が付くはず");
        assert_eq!(render(&r), format!("src/a.rs#L{}-{}", sp.start, sp.end));
        // 錨も同時に打たれるので、そのまま追従できる
        assert_eq!(r.anchor.head, "/// ツールバーを描く。");
        let shifted = format!("// top\n{SAMPLE}");
        assert_eq!(
            resolve(&r, &shifted),
            Some(Span {
                start: sp.start + 1,
                end: sp.end + 1
            })
        );
        // 見つからない記号は素直に断る
        let bad = parse_spec("src/a.rs#fn:missing").unwrap();
        assert!(resolve_spec(&bad, SAMPLE).is_err());
    }

    #[test]
    fn 記号の域どうしは安全帯で判定できる() {
        let a = resolve_spec(&parse_spec("a.rs#fn:draw_toolbar").unwrap(), SAMPLE).unwrap();
        let b = resolve_spec(&parse_spec("a.rs#impl:Span").unwrap(), SAMPLE).unwrap();
        // 別々の項目なので同時に持てる
        assert!(!conflicts(&a, &b, SAFE_BAND), "{a:?} と {b:?}");
        let c = resolve_spec(&parse_spec("a.rs#struct:Region").unwrap(), SAMPLE).unwrap();
        // Region と Span の間は 1 行しか空いていない → 安全帯に入らない
        assert!(conflicts(&c, &b, SAFE_BAND));
    }
}
