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
//! ## なぜ「行域」で本当に衝突が消えるのか (git の仕組みに基づく理由)
//!
//! git の三方向マージ (xdiff) が衝突マーカを出すのは、**両側の変更ハンクが
//! 重なるか、間に十分な未変更行が無いとき**だけである。逆に言えば、
//! 2 人の変更が [`SAFE_BAND`] 行以上離れていれば、git は**人の手を借りずに
//! 両方を取り込む**。これは経験則ではなく xdiff の合流条件そのもので、
//! [`tests::実gitで安全帯の下限を測る`] が実際に `git merge-file` を起こして
//! 「BAND-1 では衝突し、BAND では衝突しない」ことを毎回確かめている。
//!
//! よって**行域オーナーシップが守るべき不変条件は 1 つだけ**:
//!
//! > 稼働中の 2 つの行域は、同じファイル内では [`SAFE_BAND`] 行以上離れている。
//!
//! この不変条件が保たれている限り、**マージは常に一撃で通る**。
//! 「後で衝突するかもしれない」ではなく「衝突し得ない」が構造的に言える。
//!
//! ## 行番号は動く — だからアンカーを持つ
//!
//! 他人が自分より上の行を書き換えると、自分の行域は下へずれる。行番号だけを
//! 持っていると、次の書き込みで**別人の領域を自分のものだと思い込む**。
//! [`Anchor`] は域の先頭行・末尾行の内容と行数を持ち、[`resolve`] が現在の
//! テキストから域を取り直す。追従は [`crate::marks::map_lines`] を再利用する
//! (2 実装を持つとズレるため、行対応の計算はここで再発明しない)。
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
/// git の diff は既定で 3 行の文脈を付ける。両側の変更が 3 行未満しか
/// 離れていないと、xdiff はハンクを 1 つに畳んで衝突にする。
/// **この値を下げてはいけない** — 下げた瞬間に「一撃マージ」の保証が壊れる。
/// 上げるぶんには安全側だが、確保できる行域が減るので並列度が落ちる。
///
/// [`tests::実gitで安全帯の下限を測る`] が実際の git で下限を検証する。
pub const SAFE_BAND: u32 = 3;

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

/// 仕様文字列を行域へ分解する。
///
/// 受け付ける書き方:
///
/// | 書き方 | 意味 |
/// |---|---|
/// | `src/a.rs` | ファイル全体 |
/// | `src/a.rs#L10-40` | 10〜40 行目 (両端含む) |
/// | `src/a.rs#L10+30` | 10 行目から 30 行 |
/// | `src/a.rs#L10-` | 10 行目から末尾まで |
/// | `src/a.rs#L10` | 10 行目だけ |
///
/// **パス側に `#` を含む行域は表現できない** (そのままパスとして扱う)。
/// 実在するパスで `#` を使うことは稀なので、曖昧さより単純さを採る。
pub fn parse(spec: &str) -> Result<Region, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("空の指定です".into());
    }
    let Some((path, frag)) = spec.rsplit_once('#') else {
        return Ok(Region::whole(spec));
    };
    let Some(body) = frag.strip_prefix('L').or_else(|| frag.strip_prefix('l')) else {
        // `#` があっても `L` で始まらないならパスの一部とみなす
        return Ok(Region::whole(spec));
    };
    if path.is_empty() {
        return Err(format!("パスがありません: {spec}"));
    }
    let span = parse_span_body(body).ok_or_else(|| format!("行域を読めません: {frag}"))?;
    if span.is_empty() {
        return Err(format!("行域が空です: {frag}"));
    }
    Ok(Region {
        path: path.to_string(),
        span: Some(span),
        anchor: Anchor::default(),
    })
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

/// [`parse`] の逆。台帳へ書くときはこれを通す (表記を 1 つに保つ)。
pub fn render(r: &Region) -> String {
    match r.span {
        None => r.path.clone(),
        Some(s) if s.end == Span::EOF => format!("{}#L{}-", r.path, s.start),
        Some(s) if s.start == s.end => format!("{}#L{}", r.path, s.start),
        Some(s) => format!("{}#L{}-{}", r.path, s.start, s.end),
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

/// テキストと行域から錨を作る。
pub fn capture_anchor(text: &str, span: &Span) -> Anchor {
    let lines: Vec<&str> = text.lines().collect();
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
pub fn resolve(r: &Region, text: &str) -> Option<Span> {
    let span = r.span?;
    if r.anchor.is_blank() {
        return Some(span);
    }
    let lines: Vec<&str> = text.lines().collect();
    let n = lines.len() as u32;
    if n == 0 {
        return None;
    }
    let head = find_near(&lines, &r.anchor.head, span.start)?;
    let want_tail = if r.anchor.tail.is_empty() {
        head
    } else {
        find_near(&lines, &r.anchor.tail, head + r.anchor.len.saturating_sub(1))?
    };
    if want_tail < head {
        return None;
    }
    Some(Span {
        start: head,
        end: want_tail,
    })
}

/// `want` と一致する行を、`preferred` (1 始まり) の近くから外向きに探す。
///
/// [`crate::marks::scan_outward`] と同じ考え方だが、あちらは
/// `&[&str]` と 0 始まり添字で組まれているのでそのまま使う。
fn find_near(lines: &[&str], want: &str, preferred: u32) -> Option<u32> {
    if want.is_empty() {
        return None;
    }
    let pref0 = preferred.saturating_sub(1) as usize;
    crate::marks::scan_outward(lines, pref0, want).map(|i| (i + 1) as u32)
}

/// 他人の編集で行がずれた後、自分の行域を追従させる。
///
/// 行の対応付けは [`crate::marks::map_lines`] を再利用する。
/// 対応が取れない (自分の域が丸ごと消された) 場合は `false` を返し、
/// 呼び出し側は**確保し直す**。
pub fn follow(r: &mut Region, old_text: &str, new_text: &str) -> bool {
    let Some(span) = r.span else {
        return true; // ファイル全体は動かない
    };
    let old: Vec<&str> = old_text.lines().collect();
    let new: Vec<&str> = new_text.lines().collect();
    let map = crate::marks::map_lines(&old, &new);
    let at = |n: u32| -> Option<u32> {
        let i = n.checked_sub(1)? as usize;
        map.get(i).copied().flatten().map(|j| (j + 1) as u32)
    };
    let end_in = if span.end == Span::EOF {
        old.len() as u32
    } else {
        span.end
    };
    let (Some(s), Some(e)) = (at(span.start), at(end_in)) else {
        return false;
    };
    if e < s {
        return false;
    }
    r.span = Some(Span {
        start: s,
        end: if span.end == Span::EOF { Span::EOF } else { e },
    });
    r.anchor = capture_anchor(new_text, &Span { start: s, end: e });
    true
}

/// 書き込みの前後から「実際に触れた行域」を出す。
///
/// リースの関門 ([`crate::lease::gate`]) はこれを使って
/// **「持っている域の中だけを書いたか」**を判定する。持っていない域へ
/// 1 行でもはみ出したら止める。
///
/// 隣り合う変更は `band` 行以内なら 1 つの域に畳む (git のハンクと同じ挙動に
/// 揃えるため — 畳まないと「別々の小さな域」に見えて判定が甘くなる)。
pub fn touched_spans(old_text: &str, new_text: &str, band: u32) -> Vec<Span> {
    let old: Vec<&str> = old_text.lines().collect();
    let new: Vec<&str> = new_text.lines().collect();
    let map = crate::marks::map_lines(&old, &new);

    // new 側で「old のどこにも対応しない行」= 追加、および
    // old 側で「new のどこにも対応しない行」= 削除。どちらも new 側の
    // 行番号へ寄せて域にする (呼び出し側が見るのは書いた後のファイル)。
    let mut hit = vec![false; new.len().max(1)];
    let mut mapped = vec![false; new.len().max(1)];
    for (_, to) in map.iter().enumerate() {
        if let Some(j) = to {
            if *j < mapped.len() {
                mapped[*j] = true;
            }
        }
    }
    for (j, m) in mapped.iter().enumerate() {
        if !*m && j < hit.len() {
            hit[j] = true;
        }
    }
    // 削除だけが起きた位置も印を付ける (直後の行を触ったとみなす)
    let mut prev_to: i64 = -1;
    for to in map.iter() {
        match to {
            Some(j) => prev_to = *j as i64,
            None => {
                let j = (prev_to + 1).clamp(0, new.len().saturating_sub(1) as i64) as usize;
                if j < hit.len() {
                    hit[j] = true;
                }
            }
        }
    }
    if new.is_empty() && !old.is_empty() {
        return vec![Span { start: 1, end: 1 }];
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
//  テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

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
        let a = Span { start: 10, end: Span::EOF };
        assert!(spans_too_close(&a, &Span { start: 9999, end: 9999 }, SAFE_BAND));
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
        assert!(conflicts(&a, &b, SAFE_BAND), "glob は行域で切り分けられない");
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

    fn numbered(n: u32) -> String {
        (1..=n).map(|i| format!("line {i}
")).collect()
    }

    #[test]
    fn 錨で行域を取り直せる() {
        let text = numbered(50);
        let span = Span { start: 10, end: 20 };
        let mut r = Region { path: "a".into(), span: Some(span), anchor: capture_anchor(&text, &span) };
        // 上に 5 行足す → 15..25 へずれる
        let shifted = format!("x
x
x
x
x
{text}");
        let got = resolve(&r, &shifted).expect("取り直せるはず");
        assert_eq!(got, Span { start: 15, end: 25 });
        // follow でも同じ結論になる
        assert!(follow(&mut r, &text, &shifted));
        assert_eq!(r.span, Some(Span { start: 15, end: 25 }));
    }

    #[test]
    fn 触れた行域を書き込みの前後から出せる() {
        let old = numbered(30);
        let mut lines: Vec<String> = old.lines().map(|s| s.to_string()).collect();
        lines[14] = "CHANGED".into();
        let new = lines.join("
") + "
";
        let spans = touched_spans(&old, &new, SAFE_BAND);
        assert_eq!(spans, vec![Span { start: 15, end: 15 }], "15 行目だけのはず");
        assert!(within(&[Span { start: 10, end: 20 }], &spans));
        assert!(!within(&[Span { start: 1, end: 10 }], &spans));
    }
}
