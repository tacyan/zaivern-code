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
//! テキストから域を取り直す。**行番号は保存せず、必要になった時にだけ
//! 取り直す** (遅延解決)。書き込みのたびに全担当の行番号をずらして回る
//! eager な追従は持たない — 理由は [`resolve`] に書いた。
//!
//! ## 決定性
//!
//! `HashMap` / `HashSet` を 1 つも使わない。同じ入力からは、どの OS の
//! どのプロセスでも 1 バイト違わない結果が出る。

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
///
/// ## ⚠ この帯が保証していないこと (後から実測で見つかった穴)
///
/// 上の表は**変更 2 つを 1 組で**測ったもので、その範囲では今も正しい
/// (周期 1〜12 のどんな反復本文でも、1 行ずつの変更 2 つは間隔 1 行で通る)。
/// 保証していないのは**組み合わせ**のほうで、
///
/// > 「全部の組が帯を満たす」⇒「全部まとめてマージしても綺麗に通る」
///
/// は**成り立たない**。片方の担当がもう片方を上下から挟んでいると (交錯)、
/// 反復的な本文では帯を何行取っても衝突しうる。詳細と対処は
/// [`anchor_lines`] / [`interleave_safe`] / [`interleaved`] に書いた。
/// **帯を広げる方向では直らない**ので、この定数の値は動かしていない。
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

    /// 行数 (EOF 込みの域は [`u32::MAX`] を返す。挿入点は 0)。
    pub fn len(&self) -> u32 {
        if self.end < self.start {
            return 0;
        }
        self.end.saturating_sub(self.start).saturating_add(1)
    }

    /// **挿入点** — 行 `n` の**手前**に書き足す権利。既存の行を 1 行も占有しない。
    ///
    /// ## なぜ要るのか (これが並列度の天井を外す)
    ///
    /// 行域 (`#L100-180`) は**既に在る行**を占有する。ところがエージェントの
    /// 仕事の大半は「関数を足す」「`use` を足す」「一覧に 1 行足す」で、
    /// **既存の行を 1 行も要らない**。それでも行域で確保すると、ファイルの
    /// 行数がそのまま並列度の上限になる — 実測では 2000 行のファイルへ
    /// 64 体が合計 13,160 行を要求し、**供給の 6.6 倍**の需要になって
    /// 53 件が断られた。どんな割り当てでも配れない。
    ///
    /// 挿入点は幅 0 なので、**2000 行あれば安全帯 3 行で約 500 個**取れる。
    /// 64 体なら全員に配って余る。git も、違う場所への挿入どうしは
    /// 綺麗にマージする。
    ///
    /// 表現は `end + 1 == start` (空の半開区間)。`len()` は 0 になる。
    pub fn insert_before(n: u32) -> Span {
        Span {
            start: n.max(1),
            end: n.max(1).saturating_sub(1),
        }
    }

    /// 挿入点か (幅 0)。
    ///
    /// `end + 1 == start` と書くと `end == Span::EOF` で**桁あふれする**
    /// (debug ビルドは panic する)。引き算側で判定する。
    pub fn is_insert(&self) -> bool {
        self.start >= 1 && self.end == self.start - 1
    }

    /// 空 (壊れた入力) か。**挿入点は空ではない** — 幅 0 だが正当な指定である。
    pub fn is_empty(&self) -> bool {
        if self.is_insert() {
            return false;
        }
        self.start > self.end || self.start == 0
    }

    /// 判定に使う「占有区間」。挿入点は `[n, n-1]` のままだと大小比較が
    /// 逆転するので、**点 `n` として扱う**。
    fn probe(&self) -> (u32, u32) {
        if self.is_insert() {
            (self.start, self.start)
        } else {
            (self.start, self.end)
        }
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
    // `#@N` — **挿入点**。行 N の手前に書き足す権利で、既存の行を占有しない。
    // これが並列度の天井を外す指定なので、行域より先に見る。
    if let Some(at) = frag.strip_prefix('@') {
        if path.is_empty() {
            return Err(format!("パスがありません: {spec}"));
        }
        let n: u32 = at
            .trim()
            .parse()
            .map_err(|_| format!("挿入点を読めません: {frag}"))?;
        if n == 0 {
            return Err(format!("挿入点は 1 行目から: {frag}"));
        }
        return Ok(Spec {
            path: path.to_string(),
            sel: Sel::Lines(Span::insert_before(n)),
        });
    }
    let Some(body) = frag.strip_prefix('L').or_else(|| frag.strip_prefix('l')) else {
        // `#` があっても `L` でも記号でも挿入点でもないならパスの一部とみなす
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
    if s.is_insert() {
        return format!("{path}#@{}", s.start);
    }
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
/// そのまま [`resolve`] で取り直せる。
pub fn resolve_spec(s: &Spec, text: &str) -> Result<Region, String> {
    match &s.sel {
        Sel::Whole => Ok(Region::whole(&s.path)),
        Sel::Lines(sp) => Ok(Region {
            path: s.path.clone(),
            span: Some(*sp),
            anchor: capture_anchor(text, sp),
        }),
        Sel::Symbol { kind, name } => {
            // 指定を**そのまま**返す。人は自分が打った文字列で探すので、
            // `src/a.rs` と `fn` と `name` に散らして出すと照合しづらい
            // (`render_spec` が唯一の表記の真実源。ここで組み立て直さない)。
            let sp = symbol_span(text, kind, name)
                .ok_or_else(|| format!("{} が見つかりません", render_spec(s)))?;
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

// ── 判定そのものの費用を「回数」で見るための計数器 ────────────────────────
//
// 絶対時間で線を引くと必ず嘘をつく (Docker の仮想 FS / 他テストとの同時実行 /
// 負荷で実際に 3 件落ちた)。守りたいのは「件数を 2 倍にしても仕事が 2 倍しか
// 増えない」という**構造**なので、番人テストは時間ではなく**呼び出し回数**を見る。
//
// カウンタは**スレッドローカル**。プロセス共通の `static AtomicUsize` にすると
// 同時に走っている他テストの呼び出しまで混ざる (400 回のはずが 800 回になる)。
// `#[cfg(test)]` なので出荷ビルドには 1 バイトも入らない。
#[cfg(test)]
mod count {
    use std::cell::Cell;

    thread_local! {
        // 行域どうしの近さ判定 (`conflicts` の入口) を通った回数
        static PAIRS: Cell<u64> = const { Cell::new(0) };
        // 行に触れた回数 (テキストを行へ割る走査 + 錨の探索での内容比較)
        static LINES: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn pair() {
        PAIRS.with(|c| c.set(c.get().saturating_add(1)));
    }
    pub(super) fn line() {
        LINES.with(|c| c.set(c.get().saturating_add(1)));
    }
    pub(super) fn reset() {
        PAIRS.with(|c| c.set(0));
        LINES.with(|c| c.set(0));
    }
    pub(super) fn pairs() -> u64 {
        PAIRS.with(Cell::get)
    }
    pub(super) fn lines() -> u64 {
        LINES.with(Cell::get)
    }
}

/// 2 つの行域が、`band` 行の安全帯を挟んでもなお近すぎるか。
///
/// **`band` 行以上離れていれば `false`** (= 同時に持ってよい)。
/// 例: `1-10` と `14-20` は間に 3 行 (11,12,13) あるので `band = 3` では衝突しない。
pub fn spans_too_close(a: &Span, b: &Span, band: u32) -> bool {
    // 挿入点は幅 0 だが、判定では「その行の位置にある点」として扱う
    // (`[n, n-1]` のまま引き算すると大小が逆転して必ず衝突になる)。
    let (a0, a1) = a.probe();
    let (b0, b1) = b.probe();
    let ((_lo0, lo1), (hi0, _)) = if a0 <= b0 {
        ((a0, a1), (b0, b1))
    } else {
        ((b0, b1), (a0, a1))
    };
    // lo が EOF まで伸びているなら必ず重なる
    if lo1 == Span::EOF {
        return true;
    }
    // 間にある未変更行の数
    let gap = hi0.saturating_sub(lo1).saturating_sub(1);
    gap < band
}

// ═══════════════════════════════════════════════════════════════════════════
//  2.5 錨 — 帯は**組では正しいが、組み合わせでは足りない**
// ═══════════════════════════════════════════════════════════════════════════

/// ファイル内で**ちょうど 1 回**しか現れない行に印を付ける (添字 `i` は `i+1` 行目)。
///
/// # なぜ要るのか — 帯だけでは足りない実測
///
/// [`SAFE_BAND`] / [`MERGE_ONLY_BAND`] は「**2 つの**変更を何行離せば git が
/// 衝突しないか」を測って決めた値で、その主張は今も正しい (周期 1〜12 の
/// どんな反復本文でも、1 行ずつの変更 2 つは間隔 1 行で綺麗に通る)。
/// 崩れたのは**そこから先の推論**のほうで、
///
/// > 「全部の組が帯を満たす ⇒ 全部まとめてマージしても綺麗に通る」
///
/// は成り立たない。`git merge` の既定戦略 ort は **diff アルゴリズムを
/// histogram に固定している** (`man git-merge`: "ort specifically uses
/// diff-algorithm=histogram")。histogram は本文が反復的だと*同じ側の複数の
/// 変更*を 1 つの巨大なハンクへ畳むので、**片方の担当がもう片方を上下から
/// 挟んでいる**とき (交錯) に、帯を何行取っていても衝突しうる。
///
/// 実測 (`tools/merge-band-probe.sh --mode bracket`、周期 6 の Markdown):
/// A が 17 行目・B が 5/13/25 行目 — どの組も 4 行以上離れているのに衝突する。
/// 帯を広げても直らない (周期 6・16 体・ランダム順では **間隔 16 行でも
/// 9 件衝突**した)。逆に、**隣り合う他人の域の間にこの「唯一の行」が 1 本でも
/// あれば衝突しなかった** — 錨は diff が越えられない壁になる。
///
/// 判定は決定的 (`BTreeMap` のみ。`HashMap` を 1 つも使わない)。
pub fn anchor_lines(text: &str) -> Vec<bool> {
    let lines: Vec<&str> = text
        .lines()
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    let mut seen: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
    for l in &lines {
        *seen.entry(l).or_insert(0) += 1;
    }
    lines.iter().map(|l| seen.get(l) == Some(&1)).collect()
}

/// `lo` と `hi` の**間**に [`anchor_lines`] の錨が 1 本以上あるか。
///
/// 両端の域そのものは含めない (どちらかが書き換える行は壁にならない)。
pub fn anchor_between(anchors: &[bool], lo: &Span, hi: &Span) -> bool {
    let (lo0, lo1) = lo.probe();
    let (hi0, hi1) = hi.probe();
    // 呼び出し側が順序を間違えても答えを変えない
    let (lo_end, hi_start) = if lo0 <= hi0 { (lo1, hi0) } else { (hi1, lo0) };
    if lo_end == Span::EOF || hi_start == Span::EOF {
        return false;
    }
    let from = lo_end as usize;
    let to = (hi_start as usize).saturating_sub(1);
    if from >= to {
        return false;
    }
    anchors
        .get(from..to.min(anchors.len()))
        .map(|s| s.iter().any(|b| *b))
        .unwrap_or(false)
}

/// 複数の行域をまとめて包む最小の域。空なら `None`。
pub fn hull(spans: &[Span]) -> Option<Span> {
    let mut it = spans.iter();
    let first = it.next()?;
    let (mut s, mut e) = first.probe();
    for sp in it {
        let (a, b) = sp.probe();
        s = s.min(a);
        e = if e == Span::EOF || b == Span::EOF {
            Span::EOF
        } else {
            e.max(b)
        };
    }
    Some(Span { start: s, end: e })
}

/// 2 人の担当が同じファイルで**交錯**しているか
/// (= 外接域が重なる = 片方がもう片方を挟んでいる)。
///
/// 交錯していなければ、統合を**行番号の昇順**で流す限り
/// 「累積した側」が「これから混ぜる側」を挟むことは起こり得ない。
pub fn interleaved(a: &[Span], b: &[Span]) -> bool {
    match (hull(a), hull(b)) {
        (Some(x), Some(y)) => !spans_disjoint(&x, &y),
        _ => false,
    }
}

/// 2 つの域がまったく重なっていないか (帯は見ない)。
fn spans_disjoint(a: &Span, b: &Span) -> bool {
    let (a0, a1) = a.probe();
    let (b0, b1) = b.probe();
    let ((_, lo1), (hi0, _)) = if a0 <= b0 {
        ((a0, a1), (b0, b1))
    } else {
        ((b0, b1), (a0, a1))
    };
    lo1 != Span::EOF && hi0 > lo1
}

/// 交錯している 2 人を、それでも「一撃でマージできる」と言い切ってよいか。
///
/// 線の順に並べたとき、**持ち主が変わる境目すべて**に錨 ([`anchor_lines`]) が
/// 1 本以上あることを要求する。錨が 1 つも無い区間が 1 箇所でもあれば `false`
/// (**分からない側へ倒す**)。
///
/// `anchors` が空 (= 元テキストを読めなかった) なら必ず `false` を返す。
pub fn interleave_safe(anchors: &[bool], a: &[Span], b: &[Span]) -> bool {
    if anchors.is_empty() {
        return false;
    }
    let mut all: Vec<(u32, u32, bool)> = Vec::with_capacity(a.len() + b.len());
    for s in a {
        let (x, y) = s.probe();
        all.push((x, y, false));
    }
    for s in b {
        let (x, y) = s.probe();
        all.push((x, y, true));
    }
    all.sort();
    all.windows(2).all(|w| {
        let (lo, hi) = (w[0], w[1]);
        lo.2 == hi.2
            || anchor_between(
                anchors,
                &Span {
                    start: lo.0,
                    end: lo.1,
                },
                &Span {
                    start: hi.0,
                    end: hi.1,
                },
            )
    })
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
    #[cfg(test)]
    count::pair();
    // **同じ文字列は必ず自分自身と重なる。** [`crate::lease::overlaps`] は毎回
    // パスを正規化してセグメントへ割り、セグメントごとに DP の表
    // (`vec![false; (n+1)*(m+1)]`) と `Vec<char>` を確保する。実測 (debug) で
    // **1 回 53µs** — 「同じファイルの違う行」というこの機能で一番多い形が、
    // まるごとそこを通っていた。照合をやめるのではなく、**答えが自明な場合
    // だけ飛ばす**ので実装は 1 本のまま。番人は
    // `tests::同じパスは必ず自分自身と重なる`。
    if a.path != b.path && !crate::lease::overlaps(&a.path, &b.path) {
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
///
/// # 総当たりをやめた (実測して二次だったので直した)
///
/// 素朴な二重ループは `N` 件で `N(N-1)/2` 回 [`conflicts`] を呼ぶ。1 回の
/// [`conflicts`] は [`crate::lease::overlaps`] を通るので、**毎回パスを正規化して
/// セグメントへ割り、セグメントごとに DP の表を確保する**。`tools/region-cost.sh`
/// の実測 (debug) では、互いに素な 800 件で判定そのものに **9.5 秒**かかり、
/// その中身は 319,600 回の呼び出しだった。件数を 2 倍にすると 4 倍に伸びる。
/// 詳しくは `docs/region-cost.md`。
///
/// ここでは**答えを 1 つも変えずに**候補を絞る:
///
/// 1. パスを 1 回だけ正規化して、**具体パス**と「広いパス」(glob / `#` を含む /
///    末尾が区切り) に分ける。具体パスどうしは**正規化した文字列が一致する
///    ときしか重ならない** (`overlaps` のセグメント照合が literal 同士の
///    一致に潰れる) ので、一致するものだけをバケツにまとめる
/// 2. バケツの中は開始行で整列して掃く。「まだ安全帯の内側にいる」予約だけを
///    `active` に残すので、**触る組は実際に衝突する組しか出てこない**
/// 3. 広いパスはパスの照合が要るので従来どおり総当たり (実運用では 0 件)
///
/// 判定そのものは [`conflicts`] のまま呼ぶ — **述語の実装を 2 本持たない**。
/// 絞り込みは「衝突し得る組の**上位集合**」を出すだけで、最終的な可否は
/// 必ず [`conflicts`] が決める。等価性は `cost::総当たりと同じ答えを返す` が
/// 乱択 400 通りで固定している。
///
/// 出力そのものが二次になる入力 (全員が同じ行域に重なる) では、当然二次の
/// ままである。**出力件数に比例する**のが下限で、そこまで落とした。むしろ
/// そこでは総当たりより遅い。**遅い所は測ってある** (debug, 800 件が全員
/// 同じ行域に重なる `cost::crowded`):
///
/// | 段 | 実測 | 中身 |
/// |----|------|------|
/// | 掃引 ([`scan`]) | **127ms** | 判定 319,600 回 (総当たりと同数) |
/// | 並べ替え | **285ms** | 出力 319,600 組を添字の辞書順へ |
/// | (総当たり) | 103ms | 判定 319,600 回、整列は不要 |
///
/// つまり**遅さの 7 割は最後の並べ替え**で、掃引そのものは総当たりの
/// 1.23 倍でしかない。掃引は開始行の順に組を出すので添字の順になっておらず、
/// 出力が二次に膨らむとここだけ `P log P` を払う。
///
/// **数え上げ整列で `P + N` へ落とす手は入れていない。** 全件を欲しがる経路は
/// **互いに素でなかったときの診断** ([`crate::negotiate::Plan::conflict_report`])
/// だけで、そこは既に「実装のバグが起きた後」だから 0.4 秒を惜しむ理由が無い。
/// 普通の入力では `out.is_sorted()` が真になって並べ替えごと省かれる
/// (実測: 互いに素な 800 件で判定 0 回・2.1ms)。
///
/// 「空かどうか」しか要らない [`is_disjoint`] は、同じ走査を
/// **最初の 1 組で降りる**旗付きで呼ぶ (最悪ケースで 319,600 回 → **1 回**)。
///
/// 出荷での呼び出し元は `negotiate::cli_allocate` の「計画が互いに素に
/// なっていません」— **どれとどれがぶつかったのか**を出すためにここを通る
/// ([`crate::negotiate::Plan::conflict_report`])。
pub fn conflicting_pairs(list: &[Region], band: u32) -> Vec<(usize, usize)> {
    let mut out: Vec<(usize, usize)> = Vec::new();
    scan(list, band, false, &mut out);
    // 掃引は開始行の順に出すので、添字の順にはなっていない。
    // **既に整列していれば並べ替えない** — 組がほとんど出ない普通の入力では
    // ここが丸ごと省ける (検査は O(件数)、整列は O(件数 log 件数))。
    if !out.is_sorted() {
        out.sort_unstable();
    }
    out
}

/// 走査の本体。`stop_at_first` が真なら**最初の 1 組を積んだ時点で降りる**。
///
/// [`conflicting_pairs`] と [`is_disjoint`] の違いは**この旗だけ**。絞り込みも
/// 判定 ([`conflicts`]) も 1 実装しかないので、「速い側だけが違う答えを返す」が
/// 構造的に起こらない (`cost::総当たりと同じ答えを返す` が両方を乱択 400 通りで
/// 総当たりと突き合わせている)。
///
/// 降りても答えは変わらない: [`is_disjoint`] が要るのは**存在するかどうか**
/// だけで、1 組でも見つかれば残りを数えても結論は動かない。`band = 0` で
/// 「重なった行域が衝突しない」と出る性質も、判定を [`conflicts`] に任せた
/// ままなので**そのまま**である。
fn scan(list: &[Region], band: u32, stop_at_first: bool, out: &mut Vec<(usize, usize)>) {
    // ① パスを 1 回だけ正規化して分ける。ここが N 回、以前は N² 回だった。
    let mut wide: Vec<usize> = Vec::new();
    let mut plain: Vec<Entry> = Vec::new();
    for (i, r) in list.iter().enumerate() {
        let norm = crate::lease::normalize_path(&r.path);
        // `src/` は正規化で `src/**` になる = 生の文字列だけ見ると取り違える。
        // `#` を含むパスは `overlaps` が仕様として読み直すので、これも広い側へ。
        if is_glob(&r.path) || is_glob(&norm) || r.path.contains('#') {
            wide.push(i);
            continue;
        }
        // ファイル全体は「0 行目から EOF まで」として掃引に混ぜる。どの行域より
        // 手前から始まって永久に生き残るので、同じバケツの全員と組になる
        // (= `conflicts` が `None` に対して返す答えと同じ)。
        let (lo, hi) = match r.span {
            None => (0, Span::EOF),
            Some(s) => s.probe(),
        };
        plain.push(Entry {
            path: norm,
            lo,
            hi,
            idx: i,
        });
    }

    // ② 広いパスは相手が誰であれパスの照合が要る。実運用では 0 件なので、
    //    ここが二次でも全体の速さは変わらない。
    for (k, &i) in wide.iter().enumerate() {
        for &j in wide.iter().skip(k + 1) {
            if push_if_conflicts(list, i, j, band, out) && stop_at_first {
                return;
            }
        }
        for e in &plain {
            if push_if_conflicts(list, i, e.idx, band, out) && stop_at_first {
                return;
            }
        }
    }

    // ③ 具体パスは「同じファイル」ごとに固めて、開始行の順に掃く。
    plain.sort_by(|a, b| (&a.path, a.lo, a.hi, a.idx).cmp(&(&b.path, b.lo, b.hi, b.idx)));
    for bucket in plain.chunk_by(|a, b| a.path == b.path) {
        if sweep_bucket(bucket, list, band, stop_at_first, out) {
            return;
        }
    }
}

/// 一覧が互いに素か (= この集合なら衝突し得ない)。
///
/// **最初の 1 組で降りる。** 欲しいのは「空かどうか」だけなので全件を数える
/// 必要が無い。全員が重なる最悪ケース (800 件) で判定は
/// **319,600 回 → 1 回**、互いに素な 800 件では従来どおり 0 回のまま。
/// 組そのものが要るとき (= 互いに素でなかったときの診断) は
/// [`conflicting_pairs`] を使う。
pub fn is_disjoint(list: &[Region], band: u32) -> bool {
    let mut out: Vec<(usize, usize)> = Vec::new();
    scan(list, band, true, &mut out);
    out.is_empty()
}

/// 掃引に使う 1 件ぶん。`lo`/`hi` は [`Span::probe`] 済み (挿入点は点として入る)。
struct Entry {
    path: String,
    lo: u32,
    hi: u32,
    idx: usize,
}

/// 添字の順を保ったまま [`conflicts`] へ掛けて、真なら組を積む。
/// **積んだら `true`** ([`scan`] の早降りはこれを見る)。
///
/// 総当たりと**同じ向き** (`i < j`) で呼ぶ。`conflicts` は対称だが、
/// 向きを揃えておけば「総当たりと 1 バイト違わない」が自明に言える。
fn push_if_conflicts(
    list: &[Region],
    i: usize,
    j: usize,
    band: u32,
    out: &mut Vec<(usize, usize)>,
) -> bool {
    let (a, b) = if i < j { (i, j) } else { (j, i) };
    if conflicts(&list[a], &list[b], band) {
        out.push((a, b));
        return true;
    }
    false
}

/// 同じファイルの予約を開始行の順に掃いて、衝突し得る組だけを [`conflicts`] へ渡す。
///
/// `active` に残すのは「安全帯の内側にまだ届いている」予約だけ。整列済みなので
/// 一度死んだものが生き返ることはなく、**取り除きは 1 件につき 1 回**しか起きない
/// (= 全体で `O(N + 出力件数)`)。
///
/// `stop_at_first` が真で 1 組見つかったら **`true` を返して降りる**。
fn sweep_bucket(
    bucket: &[Entry],
    list: &[Region],
    band: u32,
    stop_at_first: bool,
    out: &mut Vec<(usize, usize)>,
) -> bool {
    let mut active: Vec<usize> = Vec::new();
    for (k, cur) in bucket.iter().enumerate() {
        // `gap = cur.lo - hi - 1 < band`  ⇔  `cur.lo <= hi + band`。
        // EOF まで伸びている予約は必ず重なるので落とさない。
        // **これは上位集合**であって判定ではない (band = 0 で重なる組を
        // 拾いすぎるぶんは `conflicts` が落とす)。
        active.retain(|&p| {
            let hi = bucket[p].hi;
            hi == Span::EOF || cur.lo <= hi.saturating_add(band)
        });
        for &p in &active {
            if push_if_conflicts(list, bucket[p].idx, cur.idx, band, out) && stop_at_first {
                return true;
            }
        }
        active.push(k);
    }
    false
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
        .map(|l| {
            #[cfg(test)]
            count::line();
            l.strip_suffix('\r').unwrap_or(l)
        })
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
/// # 追従はこれ 1 本 (eager な `follow` は消した)
///
/// 以前は「書き込みが起きるたびに全担当の行番号をずらして回る」eager な
/// `follow` も持っていたが、[`crate::lease`] は**判定のときにここで取り直す**
/// 遅延解決を採ったので、呼ぶ人が 1 人もいなくなった (テストだけが呼んでいて
/// `clippy -D warnings` が `never used` で落ちた)。**2 本持つと必ずズレる**
/// — 行番号を先に動かしておく設計は、更新を 1 回取りこぼした瞬間に
/// 「ずれた行番号」を正しいものとして扱ってしまう。消したのは意図的である。
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
    // **ファイルの行数で頭打ちにしない。** 確保した域が現在の EOF を超えて
    // いるのは「これから書く場所を予約した」正しい状態で、そこを縮めると
    // 予約が消えて別人が同じ場所を取れてしまう (実測: 1 行のファイルに
    // `#L1-10` を確保した直後、`#L5-15` が通った)。
    let want_len = anchor.len.max(1) as usize;
    let radius = want_len.max(ANCHOR_MIN_RADIUS).min(ANCHOR_MAX_RADIUS);

    let mut cands: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            #[cfg(test)]
            count::line();
            l.trim() == anchor.head
        })
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
        let t = if want_len == 1 {
            h
        } else if anchor.tail.is_empty() {
            // 末尾の錨が空 = 確保した時点で末尾が EOF を超えていた
            // (= これから書く場所の予約)。**記録された行数を保って伸ばす。**
            // ここで `h` へ畳むと予約が消える。
            h + want_len - 1
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
        #[cfg(test)]
        count::line();
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

    /// **挿入点は既存の行を 1 行も占有しない。**
    ///
    /// 実測で見えた天井: 2000 行のファイルへ 64 体が合計 13,160 行を要求し、
    /// 供給の 6.6 倍の需要になって 53 件が断られた。エージェントの仕事の
    /// 大半は「足す」なので、幅 0 の予約が取れれば天井そのものが消える。
    #[test]
    fn 挿入点は幅ゼロで多数が同居できる() {
        let r = parse("src/a.rs#@120").expect("解釈");
        let sp = r.span.expect("行域がある");
        assert!(sp.is_insert(), "挿入点として読めていない");
        assert_eq!(sp.len(), 0, "既存の行を占有している");
        assert!(!sp.is_empty(), "挿入点を壊れた入力と見なしている");
        assert_eq!(render(&r), "src/a.rs#@120", "往復しない");

        // 安全帯を挟めば隣り合える
        let a = Span::insert_before(100);
        assert!(!spans_too_close(&a, &Span::insert_before(104), SAFE_BAND));
        assert!(spans_too_close(&a, &Span::insert_before(102), SAFE_BAND));
        assert!(!spans_too_close(&a, &Span::insert_before(104), SAFE_BAND));
        // 順序を入れ替えても同じ
        assert!(spans_too_close(&Span::insert_before(102), &a, SAFE_BAND));

        // 行域との関係も対称
        let range = Span {
            start: 100,
            end: 140,
        };
        assert!(spans_too_close(
            &Span::insert_before(120),
            &range,
            SAFE_BAND
        ));
        assert!(!spans_too_close(
            &Span::insert_before(144),
            &range,
            SAFE_BAND
        ));

        // **2000 行あれば 64 体が全員取れる** (これが天井を外した証拠)
        let pts: Vec<Region> = (0..64)
            .map(|i| parse(&format!("src/a.rs#@{}", 10 + i * 30)).expect("解釈"))
            .collect();
        assert!(
            is_disjoint(&pts, SAFE_BAND),
            "64 個の挿入点が互いに素にならない: {:?}",
            conflicting_pairs(&pts, SAFE_BAND)
        );
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
    fn 同じパスは必ず自分自身と重なる() {
        // `conflicts` は「パスの文字列が同じなら `overlaps` を呼ばない」という
        // 近道を持つ。その前提 (同じ文字列は必ず重なる) をここで固定する。
        for p in [
            "src/a.rs",
            "SRC/A.RS",
            "src/*.rs",
            "src/**/*.rs",
            "src/d/",
            "a[b.rs",
            "a?.rs",
            "src/./a.rs",
            "src/x/../a.rs",
            "src/c.rs#L1-2",
            "",
        ] {
            assert!(
                crate::lease::overlaps(p, p),
                "{p:?} が自分自身と重ならない — conflicts の近道が壊れる"
            );
        }
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

    /// **末尾追加** (EOF への追記) の下限も 3 であること。
    ///
    /// 末尾のハンクは**後ろに文脈が無い**ので前 3 行だけで照合される。他の種別と
    /// 下限がずれていないかを別に確かめる — ファイル末尾は `use` 追加・関数追加・
    /// 一覧への追記で**全エージェントが同時に触りたがる**場所なので、ここが緩いと
    /// 事故が集中する。
    ///
    /// 実測 (base 60 行、相手が 61 行目を追記、自分は `60-gap` 行目を置換):
    /// `gap` 0/1/2 はパッチ適用が FAIL、3/4/5 は ok。**他の種別と同じ 3**。
    #[test]
    fn 実gitで末尾追加の下限を測る() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("append");
        let base = base_text(60);
        // 相手は 61 行目を追記 → 域は [61,61]
        let theirs = format!("{base}THEIRS tail\n");
        for gap in 0..=5u32 {
            let at = 60 - gap; // 自分の域 [at,at] → 間隔は 61-at-1 = gap
            let ours = edit(&base, Kind::Replace, at, 1, "OURS");
            let too_close = spans_too_close(&Span::line(at), &Span::line(61), SAFE_BAND);
            assert_eq!(too_close, gap < SAFE_BAND, "gap={gap} の判定がずれている");
            let applied = lab.apply_patch(&base, &ours, &theirs);
            assert_eq!(
                applied,
                gap >= SAFE_BAND,
                "末尾追加 gap={gap}: パッチ適用が想定と違う \
                 (末尾の下限が動いたら SAFE_BAND を測り直すこと)"
            );
            // 見逃しゼロ: 当てられない = 近すぎると言えている
            assert!(applied || too_close, "末尾追加 gap={gap} で見逃した");
        }
        // 末尾追加どうしは同じ点への追記なので必ず衝突する
        assert!(lab.merge_file(&base, &format!("{base}OURS tail\n"), &theirs));
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

    // ───────────────────────────────────────────────────────────────────────
    //  周期的な本文 — 帯だけでは足りないことの実測と、錨による手当て
    // ───────────────────────────────────────────────────────────────────────

    /// 周期 `p` で繰り返すだけの本文 (末尾の 1 行だけが一意)。
    ///
    /// 末尾を一意にするのは**実在するファイルの形に寄せるため**で、同時に
    /// 判定を安定させる。純粋な周期だけの本文は行数によって git の答えが
    /// 揺れる (周期 6 は 200/300 行で衝突し 400/600 行では通る) が、
    /// 末尾に一意な行が 1 本あると 200〜600 行のどこでも同じ結果になる。
    fn periodic(p: usize, n: u32) -> String {
        const POOL: [&str; 6] = ["```", "code line", "```", "", "---", ""];
        let mut s: String = (0..n)
            .map(|i| format!("{}\n", POOL[(i as usize) % p]))
            .collect();
        s.push_str("tail\n");
        s
    }

    /// `lines` の各行の末尾へ `tag` を足す (置換 1 行ぶんの変更を複数箇所に置く)。
    fn touch(base: &str, lines: &[u32], tag: &str) -> String {
        base.lines()
            .enumerate()
            .map(|(i, l)| {
                if lines.contains(&((i + 1) as u32)) {
                    format!("{l}  <<{tag}>>\n")
                } else {
                    format!("{l}\n")
                }
            })
            .collect()
    }

    /// 本文の `at` 行目を「ファイル内で唯一の行」に差し替える。
    fn plant_anchor(base: &str, at: &[u32]) -> String {
        base.lines()
            .enumerate()
            .map(|(i, l)| {
                let n = (i + 1) as u32;
                if at.contains(&n) {
                    format!("UNIQ-{n}\n")
                } else {
                    format!("{l}\n")
                }
            })
            .collect()
    }

    fn spans(lines: &[u32]) -> Vec<Span> {
        lines.iter().map(|n| Span::line(*n)).collect()
    }

    /// **帯だけのモデルが破れる組を、実 git で固定する。**
    ///
    /// どの組も [`SAFE_BAND`] (3 行) 以上離れているのに `git merge` は衝突する。
    /// 原因は「片方が相手を上下から挟んでいる (交錯) + 本文が反復的」で、
    /// **帯を広げても直らない**。ここが赤くなったら、それは git の側が
    /// 変わったということなので、[`SAFE_BAND`] の doc を測り直すこと。
    #[test]
    fn 実gitで周期的な本文では帯を満たしても衝突することを固定する() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("periodic");
        // (自分の行, 相手の行, 周期)
        let table: &[(&[u32], &[u32], usize)] = &[
            (&[17], &[5, 13, 25], 6),
            (&[17], &[5, 13, 25], 3),
            (&[17], &[5, 13, 25], 1),
            (&[44], &[3, 15, 22, 60], 6),
            (&[50], &[3, 20, 36, 76], 6),
        ];
        let mut broke = 0u32;
        for (ours, theirs, p) in table {
            let base = periodic(*p, 400);
            // 前提: どの組も安全帯を満たしている (= 現行モデルは「素」と言う)
            for a in ours.iter() {
                for b in theirs.iter() {
                    assert!(
                        !spans_too_close(&Span::line(*a), &Span::line(*b), SAFE_BAND),
                        "前提が崩れている: {a} と {b} は帯 {SAFE_BAND} を満たすはず"
                    );
                }
            }
            let o = touch(&base, ours, "OURS");
            let t = touch(&base, theirs, "THEIRS");
            let Some(hit) = lab.merge_tree(&base, &o, &t) else {
                eprintln!("git merge-tree --write-tree が使えないので飛ばす");
                return;
            };
            if hit {
                broke += 1;
                // 帯は満たしているが、錨が 1 本も無いので言い切ってはいけない組
                let anchors = anchor_lines(&base);
                assert!(
                    !interleave_safe(&anchors, &spans(ours), &spans(theirs)),
                    "衝突した組を interleave_safe が通してしまった: {ours:?} / {theirs:?} 周期{p}"
                );
            }
        }
        assert!(
            broke > 0,
            "周期的な本文で 1 件も衝突しないなら、この穴の前提が変わっている"
        );
    }

    /// **錨を 1 本置くと同じ組が綺麗に通る** — 手当てが効くことを実 git で確かめる。
    #[test]
    fn 実gitで錨を一本置けば同じ組が通る() {
        if !git_available() {
            eprintln!("git が無いので飛ばす");
            return;
        }
        let lab = Lab::new("periodic-anchor");
        let table: &[(&[u32], &[u32], &[u32], usize)] = &[
            (&[17], &[5, 13, 25], &[9, 15, 21], 6),
            (&[17], &[5, 13, 25], &[9, 15, 21], 1),
            (&[44], &[3, 15, 22, 60], &[9, 19, 33, 52], 6),
            (&[50], &[3, 20, 36, 76], &[11, 28, 43, 63], 6),
        ];
        for (ours, theirs, planted, p) in table {
            let base = plant_anchor(&periodic(*p, 400), planted);
            let anchors = anchor_lines(&base);
            assert!(
                interleave_safe(&anchors, &spans(ours), &spans(theirs)),
                "錨を置いたのに通していない: {ours:?} / {theirs:?}"
            );
            let o = touch(&base, ours, "OURS");
            let t = touch(&base, theirs, "THEIRS");
            let Some(hit) = lab.merge_tree(&base, &o, &t) else {
                eprintln!("git merge-tree --write-tree が使えないので飛ばす");
                return;
            };
            assert!(!hit, "錨があるのに衝突した: {ours:?} / {theirs:?} 周期{p}");
        }
    }

    #[test]
    fn 錨はファイル内で唯一の行だけ() {
        let a = anchor_lines("alpha\nsame\nbeta\nsame\n");
        assert_eq!(a, vec![true, false, true, false]);
        // CRLF でも同じ答え
        assert_eq!(anchor_lines("alpha\r\nsame\r\nbeta\r\nsame\r\n"), a);
        assert!(anchor_lines("").is_empty());
    }

    #[test]
    fn 錨は域の間にあるものだけを数える() {
        // 1:uniq 2:same 3:uniq2 4:same
        let a = anchor_lines("u1\nsame\nu2\nsame\nu3\nsame\n");
        // [1,1] と [3,3] の間は 2 行目 (same) だけ → 錨なし
        assert!(!anchor_between(&a, &Span::line(1), &Span::line(3)));
        // [1,1] と [5,5] の間は 2,3,4 行目 → 3 行目が錨
        assert!(anchor_between(&a, &Span::line(1), &Span::line(5)));
        // 引数の順を入れ替えても同じ
        assert!(anchor_between(&a, &Span::line(5), &Span::line(1)));
        // 隣接していれば「間」は空
        assert!(!anchor_between(&a, &Span::line(1), &Span::line(2)));
        // EOF まで伸びる域は壁を数えられない
        assert!(!anchor_between(
            &a,
            &Span {
                start: 1,
                end: Span::EOF
            },
            &Span::line(5)
        ));
    }

    #[test]
    fn 外接域と交錯の判定() {
        assert_eq!(hull(&spans(&[10, 3, 25])), Some(Span { start: 3, end: 25 }));
        assert_eq!(hull(&[]), None);
        // EOF が混ざれば外接域も EOF まで
        assert_eq!(
            hull(&[
                Span::line(3),
                Span {
                    start: 9,
                    end: Span::EOF
                }
            ]),
            Some(Span {
                start: 3,
                end: Span::EOF
            })
        );
        // 挟んでいる = 交錯
        assert!(interleaved(&spans(&[5, 25]), &spans(&[17])));
        assert!(interleaved(&spans(&[17]), &spans(&[5, 25])));
        // 上下に分かれていれば交錯ではない
        assert!(!interleaved(&spans(&[5, 9]), &spans(&[17, 25])));
        // 片方が空なら交錯しようがない
        assert!(!interleaved(&[], &spans(&[17])));
    }

    #[test]
    fn 錨が読めなければ交錯は通さない() {
        // 元テキストを読めなかった (= 錨が空) ときは必ず断る (fail-closed)
        assert!(!interleave_safe(&[], &spans(&[5, 25]), &spans(&[17])));
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

    /// **EOF を超える確保は「これから書く場所の予約」なので縮めない。**
    ///
    /// 実測で見つかった回帰: 1 行しかないファイルへ `#L1-10` を確保した直後に
    /// `#L5-15` が通ってしまい、**重なる 2 つの担当が同時に載った**。
    /// 原因は (1) 記録した行数をファイルの行数で頭打ちにしていたこと、
    /// (2) 末尾の錨が空のとき域を先頭 1 行へ畳んでいたこと の 2 つ。
    #[test]
    fn eofを超える予約は縮まない() {
        let text = "x\n";
        let span = Span { start: 1, end: 10 };
        let r = Region {
            path: "a.rs".into(),
            span: Some(span),
            anchor: capture_anchor(text, &span),
        };
        assert_eq!(r.anchor.len, 10, "予約した行数を記録していない");
        assert!(r.anchor.tail.is_empty(), "EOF の先に末尾行は無い");
        assert_eq!(
            resolve(&r, text),
            Some(Span { start: 1, end: 10 }),
            "予約が縮んだ"
        );
        // 重なる要求は必ず衝突と判定される
        let other = parse("a.rs#L5-15").expect("解釈");
        let live = Region {
            span: resolve(&r, text),
            ..r.clone()
        };
        assert!(
            conflicts(&live, &other, SAFE_BAND),
            "重なる 2 つの担当が同時に載る"
        );
        // 予約の先へ足された後も、同じ場所を指し続ける
        let grown = "x\ny\nz\n";
        assert_eq!(resolve(&r, grown), Some(Span { start: 1, end: 10 }));
    }

    #[test]
    fn 錨で行域を取り直せる() {
        let text = numbered(50);
        let span = Span { start: 10, end: 20 };
        let r = region_at(&text, span);
        // 上に 5 行足す → 15..25 へずれる
        let shifted = format!("x\nx\nx\nx\nx\n{text}");
        let got = resolve(&r, &shifted).expect("取り直せるはず");
        assert_eq!(got, Span { start: 15, end: 25 });
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
        let r = region_at(&text, span);
        // 自分で末尾へ追記しても、EOF の域は EOF のまま追従する
        let grown = format!("{text}line 31\nline 32\n");
        assert_eq!(
            resolve(&r, &grown),
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
        count::reset();
        let got = resolve(&r, &shifted);
        let visits = count::lines();
        assert_eq!(
            got,
            Some(Span {
                start: 20_004,
                end: 20_006
            })
        );
        // **絶対時間で線を引かない。** 以前ここは `300ms 未満` だったが、
        // その線は Docker の仮想 FS でも他テストとの同時実行でも簡単に嘘をつく
        // (実際に別のテストで 3 件落ちた)。守りたいのは「行数に比例した仕事しか
        // しない」という**構造**なので、**行の内容を比べた回数**で見る。
        // 内訳は「先頭候補を集める全走査 1 回」+「末尾を半径 256 で探すぶん」。
        let lines = shifted.lines().count() as u64;
        assert!(
            visits <= 4 * lines,
            "resolve が {visits} 回も行を比べた ({lines} 行) — 線形を超えていないか疑う"
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

// ═══════════════════════════════════════════════════════════════════════════
//  行域判定の費用 — ハーネスと分けて測る (プロセス内マイクロベンチ + 番人)
// ═══════════════════════════════════════════════════════════════════════════
//
// ## なぜ別に要るのか
//
// `tools/coedit-bench.sh` / `tools/conflict-zero-bench.sh` は一時リポジトリを
// 作り、git を起こし、`zai` を何十回も起動する。そこで出る数字の**大半は
// ハーネスの費用**で、「行域判定そのものが速いのか遅いのか」は 1 ミリも見えない。
//
// ここは外部プロセス・git・ファイル I/O を 1 つも含まない。**同じ入力の生成を
// 「判定あり」と「空の判定」の両方で回して差を取る**ことで、入力を作る費用と
// 判定そのものの費用を分ける:
//
//   total   = 入力を作る + 判定する
//   harness = 入力を作る + 空 (0 件) を判定する
//   judge   = total - harness      ← これが知りたかった数字
//
// ## 合否は時間で決めない
//
// 表に出す時間は**数字として出すだけ**で、赤にするかどうかは
// [`count`] の**呼び出し回数**で決める。絶対時間の線は Docker の仮想 FS でも
// 他テストとの同時実行でも簡単に嘘をつく。
#[cfg(test)]
mod cost {
    use super::*;
    use std::time::{Duration, Instant};

    // ───────────────────────────────────────────────────────────────────────
    //  入力を作る (決定的。依存を 1 つも増やさない)
    // ───────────────────────────────────────────────────────────────────────

    /// xorshift64*。外部クレートを足さずに決定的な擬似乱数を得る。
    struct Rng(u64);

    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(seed | 1)
        }
        fn next_u64(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        fn upto(&mut self, n: u64) -> u64 {
            if n == 0 {
                0
            } else {
                self.next_u64() % n
            }
        }
    }

    /// 1 ファイルあたりの予約数。64 体が数ファイルを分け合う実運用に近い密度。
    const PER_FILE: usize = 16;

    /// **互いに素**な予約を `n` 件。衝突は 1 件も出ない (出力 0)。
    /// 実運用で `Plan::is_disjoint` が通る形そのもの。
    fn disjoint(n: usize, band: u32) -> Vec<Region> {
        let step = 8 + band as usize + 1;
        (0..n)
            .map(|i| {
                let start = ((i % PER_FILE) * step + 1) as u32;
                Region {
                    path: format!("src/gen/f{}.rs", i / PER_FILE),
                    span: Some(Span {
                        start,
                        end: start + 5,
                    }),
                    anchor: Anchor::default(),
                }
            })
            .collect()
    }

    /// **2 件ずつが必ず衝突する**並べ方。出力は `n/2` 件 = 件数に比例する。
    /// 「出力も判定回数も線形」な、伸びを見るのにいちばん素直な土俵。
    fn couples(n: usize, band: u32) -> Vec<Region> {
        (0..n)
            .map(|i| {
                let k = i % PER_FILE;
                let base = ((k / 2) * (40 + band as usize)) as u32;
                let start = base + 1 + if k.is_multiple_of(2) { 0 } else { 4 };
                Region {
                    path: format!("src/gen/f{}.rs", i / PER_FILE),
                    span: Some(Span {
                        start,
                        end: start + 5,
                    }),
                    anchor: Anchor::default(),
                }
            })
            .collect()
    }

    /// **全員が同じ行域に重なる**最悪ケース。出力そのものが `n(n-1)/2` 件になる。
    /// ここは何をしても二次 — 出力件数が下限だからで、それを正直に出す。
    fn crowded(n: usize) -> Vec<Region> {
        (0..n)
            .map(|i| Region {
                path: "src/gen/hot.rs".to_string(),
                span: Some(Span {
                    start: 1 + (i % 4) as u32,
                    end: 60,
                }),
                anchor: Anchor::default(),
            })
            .collect()
    }

    /// 4 行周期の合成ソース。`}` と空行が全体の半分を占める = 錨の最悪ケース。
    fn make_text(lines: usize) -> String {
        let mut s = String::with_capacity(lines * 20);
        for i in 0..lines {
            match i % 4 {
                0 => s.push_str(&format!("fn f{i}() {{\n")),
                1 => s.push_str(&format!("    let x = {i};\n")),
                2 => s.push_str("}\n"),
                _ => s.push('\n'),
            }
        }
        s
    }

    /// 真ん中あたりの `fn` から 3 行の域を、錨付きで作る。
    fn anchored(text: &str, lines: usize) -> (Region, Span) {
        let k = (lines / 8) * 4; // 4 行周期なので `fn` の行 (0 起点)
        let span = Span {
            start: (k + 1) as u32,
            end: (k + 3) as u32,
        };
        let r = Region {
            path: "src/gen/mid.rs".to_string(),
            span: Some(span),
            anchor: capture_anchor(text, &span),
        };
        (r, span)
    }

    // ───────────────────────────────────────────────────────────────────────
    //  置き換える前の実装 — 数字を「前後とも」出すために残す
    // ───────────────────────────────────────────────────────────────────────

    /// 総当たり版の [`conflicting_pairs`]。`N(N-1)/2` 回 [`conflicts`] を呼ぶ。
    fn naive_pairs(list: &[Region], band: u32) -> Vec<(usize, usize)> {
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

    // ───────────────────────────────────────────────────────────────────────
    //  計測
    // ───────────────────────────────────────────────────────────────────────

    /// `iters` 回のうち**最小**を採る。平均は負荷の山を拾うが、最小は
    /// 「邪魔が入らなかった 1 回」にいちばん近い。
    fn best_of<T>(iters: u32, mut f: impl FnMut() -> T) -> Duration {
        let mut best = Duration::MAX;
        for _ in 0..iters.max(1) {
            let t0 = Instant::now();
            let out = std::hint::black_box(f());
            best = best.min(t0.elapsed());
            drop(out); // 解放は計測の外 (どの経路でも同じだけ払う)
        }
        best
    }

    /// 1 行ぶんの結果。時間はナノ秒、回数は実測。
    struct Row {
        case: String,
        axis: &'static str,
        n: usize,
        band: u32,
        harness_ns: u128,
        total_ns: u128,
        naive_total_ns: u128,
        naive_iters: u32,
        out_pairs: usize,
        judgements: u64,
        naive_judgements: u64,
    }

    impl Row {
        fn judge_ns(&self) -> u128 {
            self.total_ns.saturating_sub(self.harness_ns)
        }
        fn naive_judge_ns(&self) -> u128 {
            self.naive_total_ns.saturating_sub(self.harness_ns)
        }
        fn harness_pct(&self) -> f64 {
            if self.total_ns == 0 {
                return 0.0;
            }
            self.harness_ns as f64 * 100.0 / self.total_ns as f64
        }
    }

    /// 予約一覧の判定を測る。**答えが総当たりと一致することも同時に確かめる**
    /// (速い版だけが走って静かに間違える、をここで潰す)。
    fn measure_pairs(
        case: &str,
        n: usize,
        band: u32,
        iters: u32,
        want_naive: bool,
        build: &dyn Fn() -> Vec<Region>,
    ) -> Row {
        let harness = best_of(iters, || {
            let input = build();
            let p = conflicting_pairs(&[], band);
            (input, p)
        });
        let total = best_of(iters, || {
            let input = build();
            let p = conflicting_pairs(&input, band);
            (input, p)
        });
        // 総当たりは `n(n-1)/2` 回の判定になる。debug ビルドでは 800 件で
        // **17 秒**かかるので、大きいところは繰り返しを 1 回に落とす
        // (最小値を採る意味は薄れるが、桁は動かない)。何回回したかは JSON に出す。
        let naive_iters = if !want_naive {
            0
        } else if n.saturating_mul(n.saturating_sub(1)) / 2 > 50_000 {
            1
        } else {
            iters
        };
        let naive = if naive_iters == 0 {
            Duration::ZERO
        } else {
            best_of(naive_iters, || {
                let input = build();
                let p = naive_pairs(&input, band);
                (input, p)
            })
        };

        let input = build();
        count::reset();
        let fast_out = conflicting_pairs(&input, band);
        let judgements = count::pairs();
        count::reset();
        let naive_out = naive_pairs(&input, band);
        let naive_judgements = count::pairs();
        // **速い版だけが走って静かに間違える**をここで潰す。回数の照合は
        // 時間の計測と別に必ず 1 回やる (計測を飛ばした段でも)。
        assert_eq!(
            fast_out, naive_out,
            "{case} n={n} band={band}: 掃引と総当たりで答えが違う"
        );
        assert_eq!(
            naive_judgements,
            (n * n.saturating_sub(1) / 2) as u64,
            "総当たりの呼び出し回数が N(N-1)/2 でない"
        );

        Row {
            case: case.to_string(),
            axis: "regions",
            n,
            band,
            harness_ns: harness.as_nanos(),
            total_ns: total.as_nanos(),
            naive_total_ns: naive.as_nanos(),
            naive_iters,
            out_pairs: fast_out.len(),
            judgements,
            naive_judgements,
        }
    }

    /// [`is_disjoint`] を測る。
    ///
    /// 対照 (`naive_*` 欄) は**この早降りを入れる前の実装** =
    /// 「全件を列挙してから空かを見る」。前と後が同じ表に並ぶので、
    /// 「消えた」を数字で示せる。
    fn measure_is_disjoint(
        case: &str,
        n: usize,
        band: u32,
        iters: u32,
        build: &dyn Fn() -> Vec<Region>,
    ) -> Row {
        let harness = best_of(iters, || {
            let input = build();
            let v = is_disjoint(&[], band);
            (input, v)
        });
        let total = best_of(iters, || {
            let input = build();
            let v = is_disjoint(&input, band);
            (input, v)
        });
        // 前の実装は最悪ケース (800 件) で debug 0.4 秒。大きいところは
        // 繰り返しを 1 回に落とす (最小値を採る意味は薄れるが桁は動かない)。
        let before_iters = if n > 400 { 1 } else { iters };
        let before = best_of(before_iters, || {
            let input = build();
            let v = conflicting_pairs(&input, band).is_empty();
            (input, v)
        });

        let input = build();
        count::reset();
        let got = is_disjoint(&input, band);
        let judgements = count::pairs();
        count::reset();
        let all = conflicting_pairs(&input, band);
        let before_judgements = count::pairs();
        // **早降りが答えを変えていない**ことを、計測のたびに確かめる。
        assert_eq!(
            got,
            all.is_empty(),
            "{case} n={n} band={band}: 早降りが答えを変えた"
        );

        Row {
            case: case.to_string(),
            axis: "regions",
            n,
            band,
            harness_ns: harness.as_nanos(),
            total_ns: total.as_nanos(),
            naive_total_ns: before.as_nanos(),
            naive_iters: before_iters,
            out_pairs: all.len(),
            judgements,
            naive_judgements: before_judgements,
        }
    }

    /// テキスト側 (錨の取り直し / 錨打ち / 触れた行域) を測る。
    /// `judge` が本番、`empty` が「同じ入力を作って**空を判定**する」対照。
    fn measure_text(
        case: &str,
        lines: usize,
        iters: u32,
        judge: &dyn Fn(&str) -> u64,
        empty: &dyn Fn(&str) -> u64,
    ) -> Row {
        let harness = best_of(iters, || {
            let t = make_text(lines);
            let v = empty(&t);
            (t, v)
        });
        let total = best_of(iters, || {
            let t = make_text(lines);
            let v = judge(&t);
            (t, v)
        });
        let t = make_text(lines);
        count::reset();
        let _ = judge(&t);
        let judgements = count::lines();
        Row {
            case: case.to_string(),
            axis: "lines",
            n: lines,
            band: SAFE_BAND,
            harness_ns: harness.as_nanos(),
            total_ns: total.as_nanos(),
            naive_total_ns: 0,
            naive_iters: 0,
            out_pairs: 0,
            judgements,
            naive_judgements: 0,
        }
    }

    // ───────────────────────────────────────────────────────────────────────
    //  番人 — 合否はすべて「回数」で決める (時間では決めない)
    // ───────────────────────────────────────────────────────────────────────

    #[test]
    fn 総当たりと同じ答えを返す() {
        // glob / `#` 付き / 大小違い / `./` / 末尾スラッシュ / 空パス、
        // 全体・挿入点・EOF まで・普通の域を混ぜて乱択で突き合わせる。
        let mut rng = Rng::new(0x5eed_2024);
        let paths = [
            "src/a.rs",
            "src/b.rs",
            "SRC/A.RS",
            "src/./a.rs",
            "src/x/../a.rs",
            "src/*.rs",
            "src/**/*.rs",
            "src/c.rs#L1-2",
            "src/d/",
            "",
        ];
        for round in 0..400u32 {
            let n = 2 + rng.upto(18) as usize;
            let band = rng.upto(5) as u32;
            let list: Vec<Region> = (0..n)
                .map(|_| {
                    let p = paths[rng.upto(paths.len() as u64) as usize];
                    let start = 1 + rng.upto(40) as u32;
                    let span = match rng.upto(5) {
                        0 => None,
                        1 => Some(Span::insert_before(start)),
                        2 => Some(Span {
                            start,
                            end: Span::EOF,
                        }),
                        3 => Some(Span::line(start)),
                        _ => Some(Span {
                            start,
                            end: start + rng.upto(10) as u32,
                        }),
                    };
                    Region {
                        path: p.to_string(),
                        span,
                        anchor: Anchor::default(),
                    }
                })
                .collect();
            let naive = naive_pairs(&list, band);
            assert_eq!(
                conflicting_pairs(&list, band),
                naive,
                "round={round} band={band} list={list:?}"
            );
            // `is_disjoint` は最初の 1 組で降りる別経路なので、別に突き合わせる。
            assert_eq!(
                is_disjoint(&list, band),
                naive.is_empty(),
                "round={round} band={band} で is_disjoint が食い違う list={list:?}"
            );
        }
    }

    #[test]
    fn 件数を二倍にしても判定回数は二倍までしか増えない() {
        // **時間ではなく回数**を見る。時間は負荷で必ず嘘をつく。
        let band = SAFE_BAND;
        let mut prev: Option<(usize, u64)> = None;
        for n in [100usize, 200, 400, 800] {
            let list = couples(n, band);
            count::reset();
            let out = conflicting_pairs(&list, band);
            let cmp = count::pairs();
            assert_eq!(out.len(), n / 2, "並べ方の前提が崩れている (n={n})");
            assert!(
                cmp <= 4 * n as u64,
                "n={n} で判定 {cmp} 回 (総当たりなら {} 回)",
                n * (n - 1) / 2
            );
            if let Some((pn, pc)) = prev {
                assert!(pc > 0, "判定回数が 0 では伸びを測れない");
                let grow = cmp as f64 / pc as f64;
                assert!(
                    grow <= 2.5,
                    "{pn} → {n} 件で判定回数が {grow:.2} 倍 (総当たりなら 4 倍。二次を疑う)"
                );
            }
            prev = Some((n, cmp));
        }
    }

    #[test]
    fn 互いに素な一覧では総当たりの一パーセントも判定しない() {
        let band = SAFE_BAND;
        let n = 800usize;
        let list = disjoint(n, band);
        count::reset();
        let out = conflicting_pairs(&list, band);
        let cmp = count::pairs();
        assert!(out.is_empty(), "互いに素なのに {} 組出た", out.len());
        let naive = (n * (n - 1) / 2) as u64;
        assert!(
            cmp * 100 < naive,
            "判定 {cmp} 回 / 総当たり {naive} 回 — 絞り込みが効いていない"
        );
    }

    #[test]
    fn 互いに素かの判定は最初の一組で降りる() {
        // **合否は回数で決める。** 時間は負荷で必ず嘘をつく。
        for n in [100usize, 200, 400, 800] {
            // (1) 全員が重なる最悪ケース。全件を数えると N(N-1)/2 回。
            let list = crowded(n);
            count::reset();
            assert!(!is_disjoint(&list, SAFE_BAND), "重なっているのに互いに素");
            let stop = count::pairs();
            count::reset();
            let all = conflicting_pairs(&list, SAFE_BAND);
            let full = count::pairs();
            assert_eq!(
                stop, 1,
                "n={n} で判定 {stop} 回 (最初の 1 組で降りていない。全件版は {full} 回)"
            );
            assert_eq!(full as usize, all.len(), "全件版の回数が出力件数と合わない");

            // (2) 2 件ずつ衝突する並べ方。件数が増えても回数は増えない。
            let list = couples(n, SAFE_BAND);
            count::reset();
            assert!(!is_disjoint(&list, SAFE_BAND));
            let stop = count::pairs();
            assert!(stop <= 2, "n={n} で判定 {stop} 回 (件数に連れて増えている)");

            // (3) 互いに素なら降りる先が無いので、全件版と同じ回数のまま。
            let list = disjoint(n, SAFE_BAND);
            count::reset();
            assert!(is_disjoint(&list, SAFE_BAND));
            let stop = count::pairs();
            count::reset();
            let _ = conflicting_pairs(&list, SAFE_BAND);
            let full = count::pairs();
            assert_eq!(stop, full, "n={n}: 互いに素な入力で回数が変わった");
        }
    }

    #[test]
    fn 全員が重なる最悪ケースでも出力件数までしか判定しない() {
        // ここは何をしても二次。**出力そのものが二次だから**で、
        // 「出力件数 + 件数」を超えないことだけを固定する。
        for n in [50usize, 100, 200] {
            let list = crowded(n);
            count::reset();
            let out = conflicting_pairs(&list, SAFE_BAND);
            let cmp = count::pairs();
            assert_eq!(out.len(), n * (n - 1) / 2, "重なりの前提が崩れている");
            assert!(
                cmp <= out.len() as u64 + n as u64,
                "n={n}: 判定 {cmp} 回 > 出力 {} 件 + {n}",
                out.len()
            );
        }
    }

    #[test]
    fn 錨の取り直しにかかる行の比較は行数に比例する() {
        let mut prev: Option<(usize, u64)> = None;
        for lines in [4_000usize, 8_000, 16_000, 32_000] {
            let text = make_text(lines);
            let (r, want) = anchored(&text, lines);
            count::reset();
            let got = resolve(&r, &text);
            let visits = count::lines();
            assert_eq!(got, Some(want), "{lines} 行で取り直せなかった");
            assert!(
                visits <= 4 * lines as u64,
                "{lines} 行で {visits} 回の行比較 — 線形を超えていないか疑う"
            );
            if let Some((pl, pv)) = prev {
                assert!(pv > 0);
                let grow = visits as f64 / pv as f64;
                assert!(
                    grow <= 2.5,
                    "{pl} → {lines} 行で行比較が {grow:.2} 倍 (線形を超えた)"
                );
            }
            prev = Some((lines, visits));
        }
    }

    // ───────────────────────────────────────────────────────────────────────
    //  計測本体 (`tools/region-cost.sh` から起こす)
    // ───────────────────────────────────────────────────────────────────────

    fn env_list(key: &str, fallback: &str) -> Vec<usize> {
        let raw = std::env::var(key).unwrap_or_else(|_| fallback.to_string());
        let v: Vec<usize> = raw
            .split(',')
            .filter_map(|t| t.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .collect();
        if v.is_empty() {
            return fallback
                .split(',')
                .filter_map(|t| t.parse::<usize>().ok())
                .collect();
        }
        v
    }

    fn json_rows(rows: &[Row]) -> String {
        let mut s = String::new();
        for (i, r) in rows.iter().enumerate() {
            if i > 0 {
                s.push_str(",\n");
            }
            s.push_str(&format!(
                concat!(
                    "    {{\"case\": \"{}\", \"axis\": \"{}\", \"n\": {}, \"band\": {}, ",
                    "\"total_ns\": {}, \"harness_ns\": {}, \"judge_ns\": {}, \"harness_pct\": {:.1}, ",
                    "\"naive_total_ns\": {}, \"naive_judge_ns\": {}, \"naive_iters\": {}, ",
                    "\"out_pairs\": {}, \"judgements\": {}, \"naive_judgements\": {}}}"
                ),
                r.case,
                r.axis,
                r.n,
                r.band,
                r.total_ns,
                r.harness_ns,
                r.judge_ns(),
                r.harness_pct(),
                r.naive_total_ns,
                r.naive_judge_ns(),
                r.naive_iters,
                r.out_pairs,
                r.judgements,
                r.naive_judgements,
            ));
        }
        s
    }

    /// 同じ `case` の中で、軸を 2 倍にしたときの伸びを出す。
    /// **二次かどうかは回数で決める** — 時間は情報として並べるだけ。
    fn json_growth(rows: &[Row]) -> (String, bool) {
        let mut s = String::new();
        let mut first = true;
        let mut all_linear = true;
        for w in rows.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if a.case != b.case || b.n <= a.n {
                continue;
            }
            let size = b.n as f64 / a.n as f64;
            let cnt = if a.judgements == 0 {
                0.0
            } else {
                b.judgements as f64 / a.judgements as f64
            };
            let tim = if a.judge_ns() == 0 {
                0.0
            } else {
                b.judge_ns() as f64 / a.judge_ns() as f64
            };
            // 出力そのものが二次な土俵 (crowded) は、判定も二次で正しい。
            let output_bound = b.out_pairs > b.n * 2;
            let quadratic = cnt > size * 1.25 && !output_bound;
            if quadratic {
                all_linear = false;
            }
            if !first {
                s.push_str(",\n");
            }
            first = false;
            s.push_str(&format!(
                concat!(
                    "    {{\"case\": \"{}\", \"from\": {}, \"to\": {}, \"size_ratio\": {:.2}, ",
                    "\"judgement_ratio\": {:.2}, \"judge_time_ratio\": {:.2}, ",
                    "\"output_bound\": {}, \"quadratic\": {}}}"
                ),
                a.case, a.n, b.n, size, cnt, tim, output_bound, quadratic
            ));
        }
        (s, all_linear)
    }

    #[test]
    fn 行域判定の費用を測る() {
        if std::env::var("ZAIVERN_REGION_COST").is_err() {
            println!(
                "REGION-COST-SKIP ZAIVERN_REGION_COST が未設定なので計測は飛ばす \
                 (tools/region-cost.sh から起こす)"
            );
            return;
        }
        let sizes = env_list("ZAIVERN_REGION_COST_SIZES", "100,200,400,800");
        let line_sizes = env_list("ZAIVERN_REGION_COST_LINES", "2000,4000,8000,16000");
        let iters = env_list("ZAIVERN_REGION_COST_ITERS", "5")
            .first()
            .copied()
            .unwrap_or(5) as u32;

        let mut rows: Vec<Row> = Vec::new();

        // (a) 件数 N を 2 倍にしたときの伸び
        for &n in &sizes {
            rows.push(measure_pairs(
                "pairs.disjoint",
                n,
                SAFE_BAND,
                iters,
                true,
                &move || disjoint(n, SAFE_BAND),
            ));
        }
        for &n in &sizes {
            rows.push(measure_pairs(
                "pairs.couples",
                n,
                SAFE_BAND,
                iters,
                true,
                &move || couples(n, SAFE_BAND),
            ));
        }
        for &n in &sizes {
            rows.push(measure_pairs(
                "pairs.crowded",
                n,
                SAFE_BAND,
                iters,
                true,
                &move || crowded(n),
            ));
        }

        // (a') 「空かどうか」だけ知りたい経路 (`is_disjoint`)。
        //      `naive_*` 欄は**早降りを入れる前の実装** = 全件を列挙してから
        //      空かを見る。x 欄がそのまま「何倍速くなったか」になる。
        for &n in &sizes {
            rows.push(measure_is_disjoint(
                "check.disjoint",
                n,
                SAFE_BAND,
                iters,
                &move || disjoint(n, SAFE_BAND),
            ));
        }
        for &n in &sizes {
            rows.push(measure_is_disjoint(
                "check.couples",
                n,
                SAFE_BAND,
                iters,
                &move || couples(n, SAFE_BAND),
            ));
        }
        for &n in &sizes {
            rows.push(measure_is_disjoint(
                "check.crowded",
                n,
                SAFE_BAND,
                iters,
                &move || crowded(n),
            ));
        }

        // (c) 帯幅 — 帯を広げると「近すぎる」組が増える
        let band_n = *sizes.last().unwrap_or(&800);
        for band in [0u32, MERGE_ONLY_BAND, SAFE_BAND, 8, 32] {
            // 総当たりはここでは測らない — 入力も件数も上の段と同じで、
            // 5 回分の再計測 (debug で 1 回 17 秒) を払う価値が無い。
            rows.push(measure_pairs(
                "pairs.band",
                band_n,
                band,
                iters,
                false,
                &move || couples(band_n, SAFE_BAND),
            ));
        }

        // (b) 1 ファイルあたりの行数
        for &l in &line_sizes {
            // 錨は**計測の外**で打つ。中で打つと `capture_anchor` の費用が
            // `resolve` の数字に混ざる (実測でちょうど 1 走査ぶん上乗せされた)。
            let pre = make_text(l);
            let (anchored_region, _) = anchored(&pre, l);
            // 錨が空の域は**その場で返る** = 同じ入力に対する「空の判定」。
            let blank = Region {
                path: "src/gen/mid.rs".to_string(),
                span: Some(Span { start: 1, end: 3 }),
                anchor: Anchor::default(),
            };
            rows.push(measure_text(
                "text.resolve",
                l,
                iters,
                &|t| resolve(&anchored_region, t).map_or(0, |s| s.start as u64),
                &|t| resolve(&blank, t).map_or(0, |s| s.start as u64),
            ));
        }
        for &l in &line_sizes {
            rows.push(measure_text(
                "text.anchor",
                l,
                iters,
                &move |t| capture_anchor(t, &Span { start: 2, end: 5 }).len as u64,
                &move |_| capture_anchor("", &Span { start: 2, end: 5 }).len as u64,
            ));
        }
        for &l in &line_sizes {
            rows.push(measure_text(
                "text.touched",
                l,
                iters,
                &move |t| {
                    let edited = t.replacen("let x = ", "let y = ", 1);
                    touched_spans(t, &edited, SAFE_BAND).len() as u64
                },
                &move |_| touched_spans("", "", SAFE_BAND).len() as u64,
            ));
        }

        let (growth, all_linear) = json_growth(&rows);

        // ── 表 (人が読む) ────────────────────────────────────────────────
        println!("REGION-COST-TABLE-BEGIN");
        println!(
            "{:<16} {:>6} {:>4} {:>10} {:>9} {:>10} {:>6} {:>11} {:>6} {:>8} {:>10} {:>11}",
            "case",
            "n",
            "band",
            "total_us",
            "harness",
            "judge_us",
            "har_%",
            "naive_us",
            "x",
            "out",
            "judged",
            "naive_jdg"
        );
        for r in &rows {
            let sp = if r.judge_ns() > 0 && r.naive_judge_ns() > 0 {
                format!("{:.1}", r.naive_judge_ns() as f64 / r.judge_ns() as f64)
            } else {
                "-".to_string()
            };
            let naive_us = if r.naive_total_ns > 0 {
                format!("{:.1}", r.naive_judge_ns() as f64 / 1000.0)
            } else {
                "-".to_string()
            };
            println!(
                "{:<16} {:>6} {:>4} {:>10.1} {:>9.1} {:>10.1} {:>5.1}% {:>11} {:>6} {:>8} {:>10} {:>11}",
                r.case,
                r.n,
                r.band,
                r.total_ns as f64 / 1000.0,
                r.harness_ns as f64 / 1000.0,
                r.judge_ns() as f64 / 1000.0,
                r.harness_pct(),
                naive_us,
                sp,
                r.out_pairs,
                r.judgements,
                r.naive_judgements,
            );
        }
        println!("REGION-COST-TABLE-END");

        // ── JSON (機械が読む) ────────────────────────────────────────────
        println!("REGION-COST-JSON-BEGIN");
        println!("{{");
        println!("  \"safe_band\": {SAFE_BAND},");
        println!("  \"merge_only_band\": {MERGE_ONLY_BAND},");
        println!("  \"iters\": {iters},");
        println!("  \"cases\": [");
        println!("{}", json_rows(&rows));
        println!("  ],");
        println!("  \"growth\": [");
        println!("{growth}");
        println!("  ],");
        println!("  \"linear\": {all_linear}");
        println!("}}");
        println!("REGION-COST-JSON-END");

        assert!(
            all_linear,
            "件数を 2 倍にしたときの判定回数が 2 倍を超えた (出力が二次な土俵を除く)"
        );
    }
}
