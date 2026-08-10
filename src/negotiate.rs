//! 🤝 **行域の交渉** — 「断らない。ずらす。」
//!
//! ## なぜ要るのか (実測が指した唯一の穴)
//!
//! [`crate::region`] の行域オーナーシップで「同じファイルの違う行なら 2 人が
//! 同時に書ける」ところまで来た。衝突ハンクは 0、人手も 0 になった。
//! ところが 64 体が 1 ファイル (2000 行) へぶつかる条件では、こうなる:
//!
//! | 方式 | 完了した担当 | 拒否 | 衝突ハンク | 人手 |
//! |---|---:|---:|---:|---:|
//! | 素の git (保護なし) | 16 | 0 | 48 枝 / 960 行 | 48 回 |
//! | ファイル単位の所有 | 1 | 63 | 0 | 0 |
//! | 行域オーナーシップ | 9 | 55 | 0 | 0 |
//!
//! **衝突 0・人手 0 は達成できているのに、55 件を断っている。** 断られた担当は
//! 1 行も書いていないので、生産量では素の git (16 完了) に負けている。
//! 「衝突しない」だけでは製品にならない — **断らずに通す**必要がある。
//!
//! ここが本モジュールの担当範囲で、やることは 1 つだけ:
//!
//! > ぶつかった要求を、**近くの空いている行域へ振り替える**。
//!
//! [`tests::交渉が拒否を減らす`] が上の crowded 条件を純関数だけで再現し、
//! 効果を数字で固定している (再現した拒否 55 → **48**、完了 9 → **16**)。
//!
//! ## ずらしてよい場合の線引き — ここを間違えると製品が壊れる
//!
//! **行域は「行番号」ではなく「そこにある内容」に紐づいている。**
//! `src/app.rs#L120-180` を要求した担当は、120 行目という数字が欲しいのではなく
//! **そこにある関数**を直したいのである。勝手に `#L400-460` へ動かせば、
//! その担当は**別の関数を編集させられる**。衝突は起きないが、
//! 出来上がるのは誰も頼んでいない差分になる。これは衝突より悪い。
//!
//! だから振り替えてよい条件を、[`Want`] に**自己申告**として持たせる:
//!
//! | 申告 | 意味 | 出せる提案 |
//! |---|---|---|
//! | `movable: false` (既定) | 内容に紐づく要求。**すでに書き始めている域も必ずこちら** | [`Offer::Grant`] / [`Offer::Wait`] |
//! | `movable: true` | まだ 1 バイトも書いていない**新規確保**。行番号は予約票でしかない | ＋ [`Offer::Shift`] |
//! | `size_only: true` | 「n 行ぶんの場所が欲しい」だけ。開始位置に意味が無い | ＋ [`Offer::Split`] |
//!
//! 3 つの帰結を明示しておく:
//!
//! 1. **既に書き始めた域は絶対にずらさない。** 書き始めた時点で申告を
//!    `movable: false` へ落とすのは呼び出し側の責任である。ここでは
//!    「申告が `true` なら未着手」として扱う — 判定材料をこちらは持たない。
//! 2. `movable: false` の要求がぶつかったら **[`Offer::Wait`] しか返さない**。
//!    「近くが空いています」と言うことすらしない (言えば従ってしまうため)。
//! 3. `size_only: true` は「場所に意味が無い」の自己申告なので、
//!    [`Want::max_shift`] の上限を**無効化する** (上限を残すと申告が無意味になる)。
//!
//! ## ずらせる幅に上限を置く理由 (既定値の根拠)
//!
//! `movable: true` は「場所に意味が無い」の申告なので、理屈の上では
//! いくらでもずらしてよい。それでも [`DEFAULT_MAX_SHIFT`] を置くのは
//! **人間のレビュー局所性**のためである。「このあたりを触る」と宣言した
//! 場所から遠く離れた差分は、レビューで「なぜここ?」になる。
//!
//! 既定 200 行の根拠は実測。このリポジトリの `src/*.rs` にある
//! **9,991 個の関数**の長さは p50 = 16 行 / p90 = 50 行 / p95 = 73 行 /
//! p99 = 162 行 (平均 26.1 行) だった。つまり:
//!
//! * 16 行ずらせばもう隣の関数なので、「同じ関数に留まる」は上限の根拠に**ならない**
//! * 200 行 = **p99 の関数 1 つぶん**。「どんなに長い関数でも、たかだか 1 つ跨ぐ」幅
//!
//! 上限は定数ではなく [`Want::max_shift`] という**要求ごとの引数**で、
//! 既定値は設定 `negotiate.max_shift` から差し替えられる。
//!
//! 面白い実測がひとつある: **上限を外すと配れる件数が減ることがある**
//! (crowded 条件の id 順で 13 件 → 11 件)。遠くへ跳んだ 1 件が大きな空き域を
//! 割ってしまい、後続が入れなくなるため。上限は制約であると同時に
//! **断片化の抑制**にもなっている。
//!
//! ## 決定性
//!
//! `HashMap` / `HashSet` を 1 つも使わない。同点は必ず
//! 「行番号が小さい方」→「タスク ID の辞書順」で割る。同じ入力からは
//! どの OS のどのプロセスでも 1 バイト違わない結果が出る。
//!
//! ## `src/mesh.rs` を `use` しない
//!
//! 交渉を運ぶ Erlang 風メッシュは別担当が同時に作っている。ここでは
//! **運ばれる中身**だけを [`Deal`] として定義し、[`encode`] / [`decode`] で
//! 文字列へ往復させる。mesh 側は不透明な 1 行として運ぶだけでよい。
//!
//! ## 統合担当への申し送り
//!
//! CLI (`zai negotiate offer|allocate|deal`) の入口は [`cli_main`] として
//! 公開してある。`src/cli.rs` は共有ファイルなので**こちらでは配線していない**。
//! サブコマンドの分岐へ次の 1 行を足すと繋がる:
//!
//! ```ignore
//! "negotiate" => return Some(crate::features::negotiate::cli_main(&args[1..])),
//! ```
//!
//! **罠**: `zai` は知らない語をワークスペース指定として扱い GUI を起動する。
//! CLI 未登録のまま `zai negotiate ...` を叩くと**窓が生える**ので、
//! 依存する側は `cli::is_cli_subcommand("negotiate")` の門も併せて要る。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::i18n::{tr, trf};
use crate::region::{self, Anchor, Region, Span};

// ═══════════════════════════════════════════════════════════════════════════
//  1. 定数 — どれも「なぜその値か」を実測で言えるものだけ置く
// ═══════════════════════════════════════════════════════════════════════════

/// ずらしてよい幅の既定の上限 (行)。
///
/// 根拠はモジュール冒頭の実測 (このリポジトリの関数長 p99 = 162 行)。
/// **200 行 = 長い関数でもたかだか 1 つ跨ぐ幅**。要求ごとに
/// [`Want::max_shift`] で上書きでき、設定 `negotiate.max_shift` が既定を決める。
pub const DEFAULT_MAX_SHIFT: u32 = 200;

/// 分割したときの 1 断片の下限 (行)。
///
/// これより短い断片は**関数 1 つ入らない**ので、渡してもエージェントが使えない
/// (実測: このリポジトリの関数長 p50 は 16 行)。10 行にしているのは
/// 「宣言 + 短い関数 1 つ」がぎりぎり入る最小値として。
pub const MIN_SPLIT_PART: u32 = 10;

/// 1 つの要求を割ってよい断片の数の上限。
///
/// 5 箇所へ散った担当は、レビューで 5 箇所を追うことになり
/// 「人手 0」の前提が崩れる。**分割は衝突を消すためではなく通すための手**なので、
/// 人が読める範囲で止める。
pub const MAX_SPLIT_PARTS: usize = 4;

/// 設定キー: ずらしてよい幅の既定 (行)。
pub const KEY_MAX_SHIFT: &str = "negotiate.max_shift";

// ═══════════════════════════════════════════════════════════════════════════
//  2. 空き域の計算 — この機能の中核 (純関数)
// ═══════════════════════════════════════════════════════════════════════════

/// 占有域の**両側に `band` 行の安全帯を確保した上で**残る空き域を、
/// 行番号順に返す。
///
/// [`region::spans_too_close`] の裏返しである。あちらが
/// 「2 つの域が近すぎるか」を答えるのに対し、こちらは
/// **「近すぎない場所はどこか」**を全部挙げる。両者が食い違うと
/// 「空きだと言われた場所に置いたら衝突した」になるので、
/// [`tests::空き域に置けば必ず互いに素になる`] が全組合せで突き合わせている。
///
/// 境界の扱い:
///
/// * `file_lines == 0` → 空 (行数が分からないファイルには置けない)
/// * 占有が空 → ファイル全体が 1 つの空き域
/// * `start == 0` / `start > end` の壊れた占有 → 無視する
/// * [`Span::EOF`] の占有 → `file_lines` まで伸ばして扱う
/// * 占有が重なっていても、隣接していても正しく併合する
/// * ファイルの外へはみ出した占有 → 内側だけを塞ぐ
pub fn free_spans(file_lines: u32, occupied: &[Span], band: u32) -> Vec<Span> {
    if file_lines == 0 {
        return Vec::new();
    }
    // 占有域を「安全帯まで含めて塞がっている区間」へ写す。
    let mut blocked: Vec<(u32, u32)> = Vec::new();
    for s in occupied {
        if s.start == 0 {
            continue; // 壊れた入力 (1 始まりなので 0 は無い)
        }
        let end = if s.end == Span::EOF {
            file_lines
        } else {
            s.end
        };
        if s.start > end {
            continue; // 空、またはファイルの外で始まっている
        }
        let lo = s.start.saturating_sub(band).max(1);
        if lo > file_lines {
            continue; // 安全帯ごとファイルの外
        }
        let hi = end.saturating_add(band).min(file_lines);
        blocked.push((lo, hi));
    }
    blocked.sort_unstable();

    // 重なり・隣接を併合する (併合しないと空き域が 0 行で出てくる)。
    let mut merged: Vec<(u32, u32)> = Vec::new();
    for (lo, hi) in blocked {
        match merged.last_mut() {
            Some(last) if lo <= last.1.saturating_add(1) => last.1 = last.1.max(hi),
            _ => merged.push((lo, hi)),
        }
    }

    let mut out = Vec::new();
    let mut cur = 1u32;
    for (lo, hi) in merged {
        if lo > cur {
            out.push(Span {
                start: cur,
                end: lo - 1,
            });
        }
        cur = cur.max(hi.saturating_add(1));
    }
    if cur <= file_lines {
        out.push(Span {
            start: cur,
            end: file_lines,
        });
    }
    out
}

/// glob 記号を含むか。
///
/// `region::is_glob` は非公開なので同じ判定をここに置く。**判定を変えるときは
/// 両方を直すこと** ([`tests::glob判定がregionと一致する`] が番人)。
fn is_glob(p: &str) -> bool {
    p.contains('*') || p.contains('?') || p.contains('[')
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. 要求と提案
// ═══════════════════════════════════════════════════════════════════════════

/// 「この行域が欲しい」という 1 件の要求。
///
/// 申告の意味はモジュール冒頭の表を参照。**既定は最も保守的な
/// `movable: false`** で、明示的に `movable()` を通ったものだけがずらされる。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Want {
    /// タスク ID。同点の割り方に使うので、**空にしない**。
    pub id: String,
    /// 欲しい行域。
    pub region: Region,
    /// ずらしてよいか (= まだ 1 バイトも書いていない新規確保か)。
    #[serde(default)]
    pub movable: bool,
    /// 行数だけ合っていればよいか (= 分割してよいか)。
    #[serde(default)]
    pub size_only: bool,
    /// ずらしてよい幅の上限 (行)。`0` ならずらさない。
    #[serde(default = "default_max_shift")]
    pub max_shift: u32,
}

fn default_max_shift() -> u32 {
    DEFAULT_MAX_SHIFT
}

impl Want {
    /// **内容に紐づく要求** (既定)。ずらさない。
    pub fn fixed(id: &str, region: Region) -> Want {
        Want {
            id: id.to_string(),
            region,
            movable: false,
            size_only: false,
            max_shift: DEFAULT_MAX_SHIFT,
        }
    }

    /// **まだ書いていない新規確保**。近くの空き域へずらしてよい。
    pub fn movable(id: &str, region: Region) -> Want {
        Want {
            movable: true,
            ..Want::fixed(id, region)
        }
    }

    /// 「n 行ぶんの場所が欲しい」だけの要求へ格上げする (分割を許す)。
    pub fn size_only(mut self) -> Want {
        self.size_only = true;
        self.movable = true;
        self
    }

    /// ずらしてよい幅の上限を差し替える。
    pub fn max_shift(mut self, lines: u32) -> Want {
        self.max_shift = lines;
        self
    }

    /// 要求している行数。**確定しないなら `None`**
    /// (ファイル全体 / 末尾まで / 壊れた域)。
    pub fn lines(&self) -> Option<u32> {
        let s = self.region.span?;
        if s.is_empty() || s.end == Span::EOF {
            return None;
        }
        Some(s.len())
    }

    /// 実際にずらせる要求か。
    pub fn can_shift(&self) -> bool {
        self.movable && (self.size_only || self.max_shift > 0)
    }
}

/// 要求に対して返す提案。
///
/// **`Wait` 以外はすべて「これなら今すぐ通る」**という約束である。
/// 曖昧な返事 (「たぶん空いています」) は 1 つも用意していない —
/// 曖昧な返事を受けたエージェントは結局ぶつかりに行くため。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Offer {
    /// そのまま通る。
    Grant,
    /// 近くの空き域へずらせば通る。`moved` は行番号の差 (負なら上へ)。
    Shift { to: Region, moved: i64 },
    /// 分割すれば通る (行番号順)。
    Split { parts: Vec<Region> },
    /// 待てば通る。`until` は持ち主の期限 (UNIX 秒)、**`0` は不明**。
    Wait { holder: String, until: u64 },
    /// どうやっても通らない。
    Impossible { reason: String },
}

impl Offer {
    /// 画面と CLI に出す短い種別名。
    pub fn kind(&self) -> &'static str {
        match self {
            Offer::Grant => "grant",
            Offer::Shift { .. } => "shift",
            Offer::Split { .. } => "split",
            Offer::Wait { .. } => "wait",
            Offer::Impossible { .. } => "impossible",
        }
    }

    /// 人が読む 1 行。
    pub fn summary(&self) -> String {
        match self {
            Offer::Grant => tr("そのまま通ります"),
            Offer::Shift { to, moved } => trf(
                "{r} へ {n} 行ずらせば通ります",
                &[("r", region::render(to)), ("n", moved.to_string())],
            ),
            Offer::Split { parts } => trf(
                "{n} 箇所へ分ければ通ります: {r}",
                &[
                    ("n", parts.len().to_string()),
                    (
                        "r",
                        parts
                            .iter()
                            .map(region::render)
                            .collect::<Vec<_>>()
                            .join(" / "),
                    ),
                ],
            ),
            Offer::Wait { holder, until } => {
                if *until == 0 {
                    trf("{h} が持っています (期限は不明)", &[("h", holder.clone())])
                } else {
                    trf(
                        "{h} が持っています (期限 {t})",
                        &[("h", holder.clone()), ("t", until.to_string())],
                    )
                }
            }
            Offer::Impossible { reason } => reason.clone(),
        }
    }
}

/// [`Offer::Wait`] の `until` を、台帳から分かった期限で埋める。
///
/// [`offer`] は持ち主の**期限を受け取らない**ので `0` (不明) を返す。
/// 台帳を持っている側 (CLI / パネル) がここで埋める。
/// **すでに入っている値は上書きしない** (より確かな情報を消さないため)。
pub fn fill_deadline(o: &mut Offer, until: u64) {
    if let Offer::Wait { until: slot, .. } = o {
        if *slot == 0 {
            *slot = until;
        }
    }
}

/// 行域だけを差し替えた新しい [`Region`] を作る。
///
/// **錨は引き継がない。** 錨は「そこにあった内容」なので、
/// 場所を変えた域へ持って行くと嘘になる (取り直しで別人の域を掴む)。
fn placed(path: &str, start: u32, lines: u32) -> Region {
    Region {
        path: path.to_string(),
        span: Some(Span {
            start,
            end: start.saturating_add(lines.saturating_sub(1)),
        }),
        anchor: Anchor::default(),
    }
}

/// 1 件の要求に対する提案を出す。**I/O を一切しない。**
///
/// `occupied` は `(持ち主, 域)` の一覧。**他のファイルの占有が混ざっていてよい**
/// (パスの照合は [`region::conflicts`] / [`crate::lease::overlaps`] が行う)。
///
/// `file_lines` は対象ファイルの行数。分からないなら `0` を渡すこと —
/// **0 のときはずらす提案を出さない** (存在しない行へ振り替えないため)。
pub fn offer(want: &Want, occupied: &[(String, Region)], file_lines: u32, band: u32) -> Offer {
    // ── 0. 壊れた要求はここで落とす ──────────────────────────────────
    if let Some(s) = want.region.span {
        if s.is_empty() {
            return Offer::Impossible {
                reason: tr("行域が空です (start > end、または 0 行目)"),
            };
        }
    }

    // ── 1. ぶつかっていなければ、そのまま通る ────────────────────────
    let mut blockers: Vec<&(String, Region)> = occupied
        .iter()
        .filter(|(_, r)| region::conflicts(&want.region, r, band))
        .collect();
    if blockers.is_empty() {
        return Offer::Grant;
    }
    // 代表を決める。**同点は行番号が小さい方 → 持ち主名の辞書順**。
    blockers.sort_by(|a, b| {
        let ka = a.1.span.map_or(0, |s| s.start);
        let kb = b.1.span.map_or(0, |s| s.start);
        ka.cmp(&kb).then_with(|| a.0.cmp(&b.0))
    });
    let holder = blockers[0].0.clone();
    let wait = Offer::Wait {
        holder,
        until: 0, // 期限は台帳を持っている側が fill_deadline で埋める
    };

    // ── 2. ずらしてよいかの線引き (モジュール冒頭の表) ──────────────
    if !want.can_shift() {
        return wait;
    }
    let Some(sp) = want.region.span else {
        return wait; // ファイル全体の要求には「ずらす先」が無い
    };
    if is_glob(&want.region.path) {
        return wait; // どのファイルの何行目か確定しない
    }
    let Some(need) = want.lines() else {
        return wait; // 末尾まで = 行数が決まらない
    };
    if file_lines == 0 {
        return Offer::Impossible {
            reason: tr("ファイルの行数が分からないので、ずらす先を決められません"),
        };
    }
    if need > file_lines {
        return Offer::Impossible {
            reason: trf(
                "要求 {n} 行はファイル ({m} 行) に入りません",
                &[("n", need.to_string()), ("m", file_lines.to_string())],
            ),
        };
    }

    // ── 3. 同じファイルの占有だけを集める ────────────────────────────
    let mut occ: Vec<Span> = Vec::new();
    for (_, r) in occupied {
        if !crate::lease::overlaps(&want.region.path, &r.path) {
            continue;
        }
        if is_glob(&r.path) {
            return wait; // 相手が glob だと空き行が確定しない (安全側へ倒す)
        }
        match r.span {
            Some(s) => occ.push(s),
            None => return wait, // 相手がファイル全体を持っている
        }
    }
    let free = free_spans(file_lines, &occ, band);

    // ── 4. 最も近い空き域へ、要求行数を保ったままずらす ──────────────
    // `size_only` は「場所に意味が無い」の自己申告なので上限を外す。
    let limit: u64 = if want.size_only {
        u64::MAX
    } else {
        u64::from(want.max_shift)
    };
    let mut best: Option<(u64, u32)> = None;
    for f in &free {
        if f.len() < need {
            continue;
        }
        let hi = f.end - need + 1; // f.len() >= need なので下回らない
        let cand = sp.start.clamp(f.start, hi);
        let dist = u64::from(cand.abs_diff(sp.start));
        // 同点は**行番号が小さい方**。ここを揺らすと出力が非決定になる。
        if best.is_none_or(|(bd, bs)| dist < bd || (dist == bd && cand < bs)) {
            best = Some((dist, cand));
        }
    }
    if let Some((dist, start)) = best {
        if dist <= limit {
            return Offer::Shift {
                to: placed(&want.region.path, start, need),
                moved: i64::from(start) - i64::from(sp.start),
            };
        }
    }

    // ── 5. 行数だけ合えばよい要求なら、分割してでも通す ──────────────
    if want.size_only {
        if let Some(parts) = split_into(&want.region.path, need, &free) {
            return Offer::Split { parts };
        }
    }

    // 退いてもらえば入る (need <= file_lines は確認済み) ので「待つ」。
    wait
}

/// 空き域へ要求行数を割り付ける。割り切れなければ `None`。
///
/// **大きい空き域から詰める** — 断片の数を最小にするため。同点は行番号順。
/// 端数が [`MIN_SPLIT_PART`] を切るときは少し多めに取る
/// (3 行の断片を渡しても使えないので、渡さないほうがまだ正直)。
fn split_into(path: &str, need: u32, free: &[Span]) -> Option<Vec<Region>> {
    let mut order: Vec<&Span> = free.iter().filter(|s| s.len() >= MIN_SPLIT_PART).collect();
    order.sort_by(|a, b| b.len().cmp(&a.len()).then(a.start.cmp(&b.start)));

    let mut parts: Vec<Region> = Vec::new();
    let mut rest = need;
    for f in order.into_iter().take(MAX_SPLIT_PARTS) {
        if rest == 0 {
            break;
        }
        let take = rest.max(MIN_SPLIT_PART).min(f.len());
        parts.push(placed(path, f.start, take));
        rest = rest.saturating_sub(take);
    }
    if rest > 0 || parts.is_empty() {
        return None;
    }
    parts.sort_by_key(|r| r.span.map_or(0, |s| s.start));
    Some(parts)
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. 一括配分 — N 体へ同時に配る
// ═══════════════════════════════════════════════════════════════════════════

/// 配れた 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Granted {
    pub id: String,
    /// 実際に割り当てた域 (分割されると 2 つ以上)。
    pub regions: Vec<Region>,
    /// 要求からのずれ (行)。分割のときは先頭断片のずれ。
    pub moved: i64,
    pub how: How,
}

impl Granted {
    /// 割り当てを [`Offer`] の形へ戻す。
    ///
    /// 画面の文言 ([`Offer::summary`]) と交渉の返事 ([`respond`]) が
    /// **同じ 1 実装から出る**ようにするためにある。2 か所で組み立てると、
    /// 片方だけ直したときに「画面には出たのに相手には伝わらない」がおきる。
    pub fn as_offer(&self) -> Offer {
        match self.how {
            How::AsRequested => Offer::Grant,
            How::Shifted => match self.regions.first() {
                Some(r) => Offer::Shift {
                    to: r.clone(),
                    moved: self.moved,
                },
                None => Offer::Impossible {
                    reason: tr("割り当てが空です"),
                },
            },
            How::SplitUp => Offer::Split {
                parts: self.regions.clone(),
            },
        }
    }
}

/// どう配ったか。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum How {
    /// 要求どおり。
    AsRequested,
    /// ずらした。
    Shifted,
    /// 分割した。
    SplitUp,
}

impl How {
    pub fn label(self) -> &'static str {
        match self {
            How::AsRequested => "as_requested",
            How::Shifted => "shifted",
            How::SplitUp => "split",
        }
    }
}

/// 配れなかった理由の種別。**内訳を数えるためにある** —
/// 「断った」だけでは次に何を直せばよいか分からない。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DenyKind {
    /// 空き行そのものが足りない (ファイルが飽和している)。
    NoRoom,
    /// 空き域はあるが、[`Want::max_shift`] より遠い。
    TooFar,
    /// ずらせない要求が、他人の持つ域とぶつかった (待つしかない)。
    Held,
    /// 要求が壊れている / ファイルに入らない。
    Broken,
}

impl DenyKind {
    pub fn label(self) -> &'static str {
        match self {
            DenyKind::NoRoom => "no_room",
            DenyKind::TooFar => "too_far",
            DenyKind::Held => "held",
            DenyKind::Broken => "broken",
        }
    }
}

/// 配れなかった 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denied {
    pub id: String,
    pub kind: DenyKind,
    pub reason: String,
}

/// 配分の結果。**この構造体だけで自己検査できる** ([`Plan::is_disjoint`])。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Plan {
    pub band: u32,
    pub file_lines: u32,
    /// 入力にあった既存の占有。検査に要るので持ち歩く。
    pub held: Vec<Region>,
    pub granted: Vec<Granted>,
    pub denied: Vec<Denied>,
}

impl Plan {
    /// 既存の占有と、この計画で配る域の全部。
    pub fn all_regions(&self) -> Vec<Region> {
        let mut v = self.held.clone();
        for g in &self.granted {
            v.extend(g.regions.iter().cloned());
        }
        v
    }

    /// **この計画は衝突し得ないか。**
    ///
    /// [`region::is_disjoint`] を通すので、判定はレジストリ側の 1 実装しかない。
    /// [`tests::配分の出力は常に互いに素`] が総当たりで「偽になる入力が
    /// 存在しない」ことを確かめている。
    pub fn is_disjoint(&self) -> bool {
        region::is_disjoint(&self.all_regions(), self.band)
    }

    /// ずらして通した件数。
    pub fn shifted(&self) -> usize {
        self.granted
            .iter()
            .filter(|g| g.how == How::Shifted)
            .count()
    }

    /// 分割して通した件数。
    pub fn split_up(&self) -> usize {
        self.granted
            .iter()
            .filter(|g| g.how == How::SplitUp)
            .count()
    }

    /// 理由別の拒否件数 (`NoRoom`, `TooFar`, `Held`, `Broken` の順)。
    pub fn deny_counts(&self) -> [usize; 4] {
        let mut c = [0usize; 4];
        for d in &self.denied {
            let i = match d.kind {
                DenyKind::NoRoom => 0,
                DenyKind::TooFar => 1,
                DenyKind::Held => 2,
                DenyKind::Broken => 3,
            };
            c[i] += 1;
        }
        c
    }
}

/// 配分の処理順を決める鍵。**ここが決定性と件数の両方を決める。**
///
/// 1. **ずらせない要求が先。** 場所が決まっているので、後回しにすると
///    ずらせる要求に場所を取られて無駄に落ちる。
/// 2. ずらせる要求は**小さいものから**。件数の最大化にはこれが効く
///    (実測: crowded 条件で昇順 14 件 / 降順 6 件 — 2 倍以上違う)。
/// 3. 同点は**タスク ID の辞書順**。
fn sort_key(w: &Want) -> (u8, u32, &str) {
    if w.can_shift() {
        (1, w.lines().unwrap_or(u32::MAX), w.id.as_str())
    } else {
        (0, 0, w.id.as_str())
    }
}

/// N 件の要求を、互いに素な割当へ**最大化**して配る。
///
/// 貪欲だが決定的。同じ入力からは必ず同じ [`Plan`] が出る
/// (`HashMap` / `HashSet` を 1 つも使わない)。
pub fn allocate(wants: &[Want], occupied: &[(String, Region)], file_lines: u32, band: u32) -> Plan {
    let mut order: Vec<&Want> = wants.iter().collect();
    order.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));

    let mut live: Vec<(String, Region)> = occupied.to_vec();
    let mut granted: Vec<Granted> = Vec::new();
    let mut denied: Vec<Denied> = Vec::new();

    for w in order {
        match offer(w, &live, file_lines, band) {
            Offer::Grant => {
                live.push((w.id.clone(), w.region.clone()));
                granted.push(Granted {
                    id: w.id.clone(),
                    regions: vec![w.region.clone()],
                    moved: 0,
                    how: How::AsRequested,
                });
            }
            Offer::Shift { to, moved } => {
                live.push((w.id.clone(), to.clone()));
                granted.push(Granted {
                    id: w.id.clone(),
                    regions: vec![to],
                    moved,
                    how: How::Shifted,
                });
            }
            Offer::Split { parts } => {
                let moved = parts
                    .first()
                    .and_then(|p| p.span)
                    .zip(w.region.span)
                    .map_or(0, |(a, b)| i64::from(a.start) - i64::from(b.start));
                for p in &parts {
                    live.push((w.id.clone(), p.clone()));
                }
                granted.push(Granted {
                    id: w.id.clone(),
                    regions: parts,
                    moved,
                    how: How::SplitUp,
                });
            }
            Offer::Wait { holder, .. } => {
                let kind = deny_kind(w, &live, file_lines, band);
                denied.push(Denied {
                    id: w.id.clone(),
                    kind,
                    reason: match kind {
                        DenyKind::NoRoom => tr("空き行が足りません (ファイルが飽和しています)"),
                        DenyKind::TooFar => trf(
                            "空き域はありますが、ずらせる上限 {n} 行より遠くにあります",
                            &[("n", w.max_shift.to_string())],
                        ),
                        _ => trf("{h} が持っています", &[("h", holder)]),
                    },
                });
            }
            Offer::Impossible { reason } => denied.push(Denied {
                id: w.id.clone(),
                kind: DenyKind::Broken,
                reason,
            }),
        }
    }

    // 出力の並びも決定的にする (処理順は最適化の都合なので、そのまま出さない)。
    granted.sort_by(|a, b| a.id.cmp(&b.id));
    denied.sort_by(|a, b| a.id.cmp(&b.id));

    Plan {
        band,
        file_lines,
        held: occupied.iter().map(|(_, r)| r.clone()).collect(),
        granted,
        denied,
    }
}

/// 断った理由を分ける。
///
/// [`offer`] は [`Offer::Wait`] しか返さないので、**「空きが無い」と
/// 「空きはあるが遠い」を分けるのはここ**。内訳が取れないと
/// 「上限を緩めれば通るのか、そもそも入らないのか」が分からない。
fn deny_kind(want: &Want, live: &[(String, Region)], file_lines: u32, band: u32) -> DenyKind {
    if !want.can_shift() {
        return DenyKind::Held;
    }
    let Some(need) = want.lines() else {
        return DenyKind::Held;
    };
    if file_lines == 0 || need > file_lines {
        return DenyKind::NoRoom;
    }
    let mut occ: Vec<Span> = Vec::new();
    for (_, r) in live {
        if !crate::lease::overlaps(&want.region.path, &r.path) {
            continue;
        }
        match r.span {
            Some(s) => occ.push(s),
            None => return DenyKind::Held,
        }
    }
    if free_spans(file_lines, &occ, band)
        .iter()
        .any(|f| f.len() >= need)
    {
        DenyKind::TooFar
    } else {
        DenyKind::NoRoom
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. 交渉プロトコル — Erlang 風メッシュが運ぶ「中身」だけ
// ═══════════════════════════════════════════════════════════════════════════

/// メッシュが運ぶ 1 通。
///
/// **`src/mesh.rs` を `use` しない。** メッシュ (Pid / mailbox / link / monitor)
/// は別担当が同時に作っているので、こちらは**運ばれる中身**だけを決めて
/// [`encode`] / [`decode`] で文字列へ往復させる。mesh 側は
/// **不透明な 1 行**として運ぶだけでよい (改行を含まないことは [`encode`] が保証する)。
///
/// 意味の約束:
///
/// * `Accept` — その域で**いま確保できた**。
/// * `Counter` — **この条件なら通る**という具体案。`Shift` / `Split` / `Wait` が載る。
/// * `Reject` — どうやっても通らない (理由つき)。
///
/// `Counter` は**予約付きの提案**である。[`respond`] は対案として出した域を
/// 他の要求へ配らない (二重に配ると衝突が戻ってくる)。**予約の期限切れは
/// 上位層 (mesh の monitor / DOWN) の仕事**で、ここでは扱わない。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "deal", rename_all = "snake_case")]
pub enum Deal {
    Propose {
        from: String,
        want: Want,
    },
    Accept {
        from: String,
        id: String,
        region: Region,
    },
    Reject {
        from: String,
        id: String,
        reason: String,
    },
    Counter {
        from: String,
        id: String,
        offer: Offer,
    },
}

/// [`Deal`] を 1 行の文字列へ。**改行を含まない** (メッシュが行単位で運ぶため)。
pub fn encode(d: &Deal) -> String {
    // serde_json::to_string は改行を出さない。失敗し得ないが、失敗しても
    // panic させず「読めない 1 行」を返す (decode 側が Err にする)。
    serde_json::to_string(d).unwrap_or_else(|e| format!("{{\"deal\":\"broken\",\"e\":{e:?}}}"))
}

/// [`encode`] の逆。
pub fn decode(s: &str) -> Result<Deal, String> {
    serde_json::from_str(s).map_err(|e| e.to_string())
}

/// 受け取った提案の束へ、まとめて返事を作る。
///
/// **1 通ずつ返事をしない。** 同じ束に入っている 2 つの提案が互いに重なる
/// ことがあり、片方ずつ答えると両方に「通る」と答えてしまう。
/// [`allocate`] を通すことで、返事そのものが互いに素であることを保証する。
///
/// 読めなかった行には `Reject`(id 空) を返す — メッシュで黙って落とすと
/// 送り手が永遠に待つため。
///
/// 呼び手は `zai negotiate deal` (標準入力から 1 行ずつ受け取る形)。
/// メッシュの上で回す経路は `crate::negomesh` が担当する — あちらは
/// 素の [`crate::features::mesh::Msg::Claim`] と交渉形の要求が**混ざって**
/// 届くので、両方を 1 回の [`allocate`] へまとめて渡す必要がある。
pub fn respond(
    inbox: &[String],
    occupied: &[(String, Region)],
    file_lines: u32,
    band: u32,
    me: &str,
) -> Vec<String> {
    let mut wants: Vec<Want> = Vec::new();
    let mut out: Vec<String> = Vec::new();
    for line in inbox {
        match decode(line) {
            Ok(Deal::Propose { want, .. }) => wants.push(want),
            // 自分宛ての返事は、送り手側の状態機械が扱う。ここでは黙って流す。
            Ok(_) => {}
            Err(e) => out.push(encode(&Deal::Reject {
                from: me.to_string(),
                id: String::new(),
                reason: e,
            })),
        }
    }
    let plan = allocate(&wants, occupied, file_lines, band);
    for g in &plan.granted {
        let deal = match g.as_offer() {
            // 要求どおりに取れたときだけ Accept。ずらした / 分けたものは
            // 相手の同意が要るので Counter で返す。
            Offer::Grant => Deal::Accept {
                from: me.to_string(),
                id: g.id.clone(),
                region: g.regions[0].clone(),
            },
            offer => Deal::Counter {
                from: me.to_string(),
                id: g.id.clone(),
                offer,
            },
        };
        out.push(encode(&deal));
    }
    for d in &plan.denied {
        let deal = match d.kind {
            // 「待てば通る」は拒否ではないので Counter で返す。
            DenyKind::Held | DenyKind::TooFar | DenyKind::NoRoom => Deal::Counter {
                from: me.to_string(),
                id: d.id.clone(),
                offer: Offer::Wait {
                    holder: me.to_string(),
                    until: 0,
                },
            },
            DenyKind::Broken => Deal::Reject {
                from: me.to_string(),
                id: d.id.clone(),
                reason: d.reason.clone(),
            },
        };
        out.push(encode(&deal));
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. CLI — `zai negotiate offer|allocate|deal`
// ═══════════════════════════════════════════════════════════════════════════

/// 入力の 1 占有。域は `src/a.rs#L10-40` の**仕様文字列**で書く
/// (`Region` を丸ごと JSON にすると錨まで書かせることになり、手で書けない)。
#[derive(Deserialize)]
struct HeldIn {
    #[serde(default)]
    holder: String,
    region: String,
    #[serde(default)]
    until: u64,
}

/// 入力の 1 要求。
#[derive(Deserialize)]
struct WantIn {
    #[serde(default)]
    id: String,
    region: String,
    #[serde(default)]
    movable: bool,
    #[serde(default)]
    size_only: bool,
    #[serde(default)]
    max_shift: Option<u32>,
}

#[derive(Deserialize)]
struct OfferIn {
    #[serde(default)]
    file_lines: u32,
    #[serde(default)]
    band: Option<u32>,
    want: WantIn,
    #[serde(default)]
    occupied: Vec<HeldIn>,
}

#[derive(Deserialize)]
struct AllocIn {
    #[serde(default)]
    file_lines: u32,
    #[serde(default)]
    band: Option<u32>,
    #[serde(default)]
    wants: Vec<WantIn>,
    #[serde(default)]
    occupied: Vec<HeldIn>,
}

#[derive(Deserialize)]
struct DealIn {
    #[serde(default)]
    file_lines: u32,
    #[serde(default)]
    band: Option<u32>,
    #[serde(default)]
    occupied: Vec<HeldIn>,
    #[serde(default)]
    inbox: Vec<String>,
    #[serde(default)]
    me: String,
}

#[derive(Serialize)]
struct OfferOut {
    kind: &'static str,
    summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    to: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    moved: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    parts: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    until: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Serialize)]
struct GrantOut {
    id: String,
    how: &'static str,
    regions: Vec<String>,
    moved: i64,
}

#[derive(Serialize)]
struct DenyOut {
    id: String,
    why: &'static str,
    reason: String,
}

#[derive(Serialize)]
struct PlanOut {
    band: u32,
    file_lines: u32,
    granted: Vec<GrantOut>,
    denied: Vec<DenyOut>,
    shifted: usize,
    split: usize,
    /// 断った理由の内訳 `[空きが無い, 遠すぎる, 持たれている, 壊れている]`。
    /// **「何件断った」だけでは次に何を直せばよいか分からない** —
    /// 上限を緩めれば通るのか、ファイルがもう飽和しているのかがここで分かる。
    deny_counts: [usize; 4],
    /// **出力自身の検査結果。** 偽なら呼び出し側は計画を捨ててよい。
    disjoint: bool,
}

/// [`Offer`] を JSON の形へ。
fn offer_out(o: &Offer) -> OfferOut {
    let mut out = OfferOut {
        kind: o.kind(),
        summary: o.summary(),
        to: None,
        moved: None,
        parts: Vec::new(),
        holder: None,
        until: None,
        reason: None,
    };
    match o {
        Offer::Grant => {}
        Offer::Shift { to, moved } => {
            out.to = Some(region::render(to));
            out.moved = Some(*moved);
        }
        Offer::Split { parts } => out.parts = parts.iter().map(region::render).collect(),
        Offer::Wait { holder, until } => {
            out.holder = Some(holder.clone());
            out.until = Some(*until);
        }
        Offer::Impossible { reason } => out.reason = Some(reason.clone()),
    }
    out
}

fn to_want(w: &WantIn) -> Result<Want, String> {
    let r = region::parse(&w.region)?;
    let id = if w.id.is_empty() {
        region::render(&r)
    } else {
        w.id.clone()
    };
    Ok(Want {
        id,
        region: r,
        movable: w.movable || w.size_only,
        size_only: w.size_only,
        max_shift: w.max_shift.unwrap_or(DEFAULT_MAX_SHIFT),
    })
}

fn to_held(list: &[HeldIn]) -> Result<Vec<(String, Region)>, String> {
    let mut out = Vec::new();
    for h in list {
        let r = region::parse(&h.region)?;
        let name = if h.holder.is_empty() {
            tr("(名前なし)")
        } else {
            h.holder.clone()
        };
        out.push((name, r));
    }
    Ok(out)
}

/// 台帳から分かる期限を引く (`holder` が一致する最初のもの)。
fn deadline_of(list: &[HeldIn], holder: &str) -> u64 {
    list.iter()
        .find(|h| h.holder == holder)
        .map_or(0, |h| h.until)
}

fn usage() -> String {
    tr("\
zai negotiate — 行域がぶつかったとき、断らずに「ずらす」

  zai negotiate offer      < in.json   1 件の要求への提案を出す
  zai negotiate allocate   < in.json   N 件をまとめて互いに素に配る
  zai negotiate deal       < in.json   交渉メッセージの束へ返事を作る
  zai negotiate serve                  メッシュの上で交渉役として実際に回る
                                       [--rounds N] [--lines N] [--band N]
  zai negotiate ask --spec <域>        交渉役へ行域を要求して、返事を待つ
                                       [--movable] [--size-only] [--to <pid>]
                                       [--as <pid>] [--rounds N] [--max-shift N]
  zai negotiate help                   この使い方

入力は標準入力の JSON。域は \"src/a.rs#L10-40\" の仕様文字列で書く。

  offer:    {\"file_lines\":2000,\"band\":3,
             \"want\":{\"id\":\"t1\",\"region\":\"src/a.rs#L10-40\",\"movable\":true},
             \"occupied\":[{\"holder\":\"bob\",\"region\":\"src/a.rs#L1-50\",\"until\":0}]}
  allocate: 同じ形で \"want\" の代わりに \"wants\":[...]
  deal:     同じ形で \"inbox\":[\"<1 行の Deal>\", ...] と \"me\":\"自分の名前\"

`--movable` を付けたときだけ「ずらしてよい」を明示する。付けなければ
**絶対にずらさない** — 行域は行番号ではなく*そこにある内容*に紐づくので、
勝手にずらすと「別の関数を編集しろ」と言ったことになる。

終了コード:
  0  そのまま通る / 全件配れた / 取れた (ask)
  1  どうやっても通らない / 1 件も配れなかった / 断られた (ask)
  2  使い方の誤り (入力が読めない・サブコマンドが違う)
  3  提案がある (ずらす・分ける・待つ) / 一部だけ配れた
     serve/ask では「交渉役がちょうど 1 体」の破れ
     (serve=既に居る / ask=居ない)
  4  上限まで待ったが返事が来ない (ask のみ。断られたのとは別物)
  5  メッシュが無効 (先に `zai mesh join`)
")
}

/// 標準入力を全部読む。
fn read_stdin() -> Result<String, String> {
    std::io::read_to_string(std::io::stdin()).map_err(|e| e.to_string())
}

/// `zai negotiate <sub>` の実体。argv は `"negotiate"` の**次**から渡される。
///
/// 終了コードの意味は [`usage`] を参照 (0=通る / 1=通らない / 2=使い方 / 3=提案あり)。
/// メッシュを使う `serve` / `ask` は 3〜5 を別の意味で使う —
/// 一覧は [`crate::negomesh`] のモジュールドキュメントにある。
///
/// `src/cli.rs` の dispatch から `zai negotiate …` として呼ばれる
/// (統合時に直列で配線済み。`allow(dead_code)` はその時点で外した)。
pub fn cli_main(argv: &[String]) -> i32 {
    let Some(sub) = argv.first().map(String::as_str) else {
        print!("{}", usage());
        return 2;
    };
    match sub {
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            0
        }
        "offer" => cli_offer(),
        "allocate" => cli_allocate(),
        "deal" => cli_deal(),
        // メッシュの上で実際に交渉を回す。実体は `crate::negomesh`
        // (mesh と negotiate は互いを知らない設計なので、繋ぐ層は別に置く)。
        "serve" => crate::negomesh::serve_cli(argv),
        // 要求する側。交渉役へ送って、**上限つきで**返事を待つ。
        "ask" => crate::negomesh::ask_cli(argv),
        other => {
            eprintln!(
                "{}",
                trf(
                    "zai negotiate: 知らないサブコマンド {s}",
                    &[("s", other.to_string())]
                )
            );
            print!("{}", usage());
            2
        }
    }
}

fn fail(msg: String) -> i32 {
    eprintln!("{msg}");
    2
}

fn cli_offer() -> i32 {
    let text = match read_stdin() {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let input: OfferIn = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let want = match to_want(&input.want) {
        Ok(w) => w,
        Err(e) => return fail(e),
    };
    let held = match to_held(&input.occupied) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    let band = input.band.unwrap_or(region::SAFE_BAND);
    let mut o = offer(&want, &held, input.file_lines, band);
    if let Offer::Wait { holder, .. } = &o {
        let until = deadline_of(&input.occupied, holder);
        fill_deadline(&mut o, until);
    }
    println!(
        "{}",
        serde_json::to_string_pretty(&offer_out(&o)).unwrap_or_default()
    );
    match o {
        Offer::Grant => 0,
        Offer::Impossible { .. } => 1,
        _ => 3,
    }
}

fn plan_out(plan: &Plan) -> PlanOut {
    PlanOut {
        band: plan.band,
        file_lines: plan.file_lines,
        granted: plan
            .granted
            .iter()
            .map(|g| GrantOut {
                id: g.id.clone(),
                how: g.how.label(),
                regions: g.regions.iter().map(region::render).collect(),
                moved: g.moved,
            })
            .collect(),
        denied: plan
            .denied
            .iter()
            .map(|d| DenyOut {
                id: d.id.clone(),
                why: d.kind.label(),
                reason: d.reason.clone(),
            })
            .collect(),
        shifted: plan.shifted(),
        split: plan.split_up(),
        deny_counts: plan.deny_counts(),
        disjoint: plan.is_disjoint(),
    }
}

fn cli_allocate() -> i32 {
    let text = match read_stdin() {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let input: AllocIn = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let mut wants = Vec::new();
    for w in &input.wants {
        match to_want(w) {
            Ok(v) => wants.push(v),
            Err(e) => return fail(e),
        }
    }
    let held = match to_held(&input.occupied) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    let band = input.band.unwrap_or(region::SAFE_BAND);
    let plan = allocate(&wants, &held, input.file_lines, band);
    println!(
        "{}",
        serde_json::to_string_pretty(&plan_out(&plan)).unwrap_or_default()
    );
    if !plan.is_disjoint() {
        eprintln!("{}", tr("計画が互いに素になっていません (実装のバグです)"));
        return 1;
    }
    if plan.denied.is_empty() {
        0
    } else if plan.granted.is_empty() {
        1
    } else {
        3
    }
}

#[derive(Serialize)]
struct DealOut {
    outbox: Vec<String>,
}

fn cli_deal() -> i32 {
    let text = match read_stdin() {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let input: DealIn = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => return fail(e.to_string()),
    };
    let held = match to_held(&input.occupied) {
        Ok(h) => h,
        Err(e) => return fail(e),
    };
    let band = input.band.unwrap_or(region::SAFE_BAND);
    let me = if input.me.is_empty() {
        "negotiator".to_string()
    } else {
        input.me.clone()
    };
    let outbox = respond(&input.inbox, &held, input.file_lines, band, &me);
    println!(
        "{}",
        serde_json::to_string_pretty(&DealOut {
            outbox: outbox.clone()
        })
        .unwrap_or_default()
    );
    let accepts = outbox.iter().filter(|s| s.contains("\"accept\"")).count();
    let rejects = outbox.iter().filter(|s| s.contains("\"reject\"")).count();
    if accepts == outbox.len() && !outbox.is_empty() {
        0
    } else if rejects == outbox.len() && !outbox.is_empty() {
        1
    } else {
        3
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. パネル — `app.rs` を 1 バイトも触らずに交渉卓を出す
// ═══════════════════════════════════════════════════════════════════════════

/// 走査 1 回ぶんの結果。**ウィンドウより長生きさせる** (設計原則 1)。
#[derive(Clone, Debug, Default)]
struct Snapshot {
    /// 台帳から拾った、対象ファイルに関わる占有 (持ち主 / 域 / 期限)。
    held: Vec<(String, Region, u64)>,
    /// 対象ファイルの行数。読めなければ 0 (= ずらす提案を出さない)。
    file_lines: u32,
    /// 設定 `negotiate.max_shift` の値。
    cfg_max_shift: u32,
    /// 走査した時刻 (UNIX 秒)。期限の残りを出すのに使う。
    /// **描画のたびに時計を読まない**ため、走査時の値を持ち回る。
    now: u64,
    /// 台帳が読めない等の説明。
    note: Option<String>,
    cost: Duration,
}

#[derive(Default)]
struct PanelState {
    open: bool,
    root: PathBuf,
    /// 要求の仕様文字列 (`src/app.rs#L120-180`)。
    spec: String,
    /// いま走査に使った仕様 (これが変わったら取り直す)。
    scanned: String,
    /// 打鍵が止まってから走査するための起点。
    typed_at: Option<Instant>,
    movable: bool,
    size_only: bool,
    max_shift: u32,
    /// ユーザーが幅をいじったか (いじった後に設定で上書きしない)。
    shift_touched: bool,
    snap: Snapshot,
    pending: Option<Receiver<Snapshot>>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
    toast: String,
}

fn state() -> &'static Mutex<PanelState> {
    static S: OnceLock<Mutex<PanelState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(PanelState::default()))
}

/// GUI が開いているワークスペース。取れなければカレントディレクトリ。
/// **パスは 1 つもハードコードしない。**
fn gui_workspace_root() -> PathBuf {
    let me = std::process::id();
    crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// パレットからの入口。開閉を切り替える。
pub fn toggle_panel() {
    let Ok(mut st) = state().lock() else { return };
    if st.open {
        st.open = false;
        return;
    }
    st.open = true;
    st.root = gui_workspace_root();
    st.last_scan = None; // 開いた回は必ず取り直す
    st.toast.clear();
    if st.max_shift == 0 {
        st.max_shift = DEFAULT_MAX_SHIFT;
    }
}

/// ファイルの行数を数える。**巨大なファイルは読まない** (走査が固まるため)。
fn count_lines(p: &Path) -> Option<u32> {
    const CAP: u64 = 8 * 1024 * 1024;
    let md = std::fs::metadata(p).ok()?;
    if !md.is_file() || md.len() > CAP {
        return None;
    }
    let text = std::fs::read_to_string(p).ok()?;
    Some(text.lines().count() as u32)
}

/// 1 回ぶんの走査 (**裏のスレッドで動く**)。
///
/// 台帳の読み取りとファイルの行数え。**UI スレッドからは呼ばない。**
fn scan(root: PathBuf, spec: String) -> Snapshot {
    let t0 = Instant::now();
    let cfg = crate::config::load(std::slice::from_ref(&root), false);
    let cfg_max_shift = cfg.feature_i64(KEY_MAX_SHIFT).clamp(0, i64::from(u32::MAX)) as u32;
    let want = match region::parse(&spec) {
        Ok(r) => r,
        Err(e) => {
            return Snapshot {
                cfg_max_shift,
                note: Some(e),
                cost: t0.elapsed(),
                ..Default::default()
            }
        }
    };
    let roots = crate::lease::roots_of(&root);
    let store = crate::lease::store_path_in(&crate::lease::store_dir(), &roots.key);
    let now = crate::lease::now_secs();
    let mut held: Vec<(String, Region, u64)> = Vec::new();
    let mut note = None;
    match crate::lease::read_store(&store) {
        Ok(s) => {
            for l in &s.leases {
                // 期限だけで足切りする。PID の生存確認は台帳側の仕事で、
                // ここで真似ると 2 実装になってズレる。
                if l.expires_at != 0 && l.expires_at < now {
                    continue;
                }
                for p in &l.patterns {
                    let Ok(r) = region::parse(p) else { continue };
                    if !crate::lease::overlaps(&want.path, &r.path) {
                        continue;
                    }
                    held.push((l.holder.display(), r, l.expires_at));
                }
            }
        }
        Err(e) => note = Some(e),
    }
    // 決定的に並べる (行番号 → 持ち主)。
    held.sort_by(|a, b| {
        a.1.span
            .map_or(0, |s| s.start)
            .cmp(&b.1.span.map_or(0, |s| s.start))
            .then_with(|| a.0.cmp(&b.0))
    });
    let file_lines = if is_glob(&want.path) {
        0
    } else {
        count_lines(&roots.tree.join(&want.path)).unwrap_or(0)
    };
    Snapshot {
        held,
        file_lines,
        cfg_max_shift,
        now,
        note,
        cost: t0.elapsed(),
    }
}

fn spawn_scan(root: PathBuf, spec: String) -> Option<Receiver<Snapshot>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("zv-negotiate-scan".into())
        .spawn(move || {
            let _ = tx.send(scan(root, spec));
        })
        .ok()
        .map(|_| rx)
}

/// 打鍵が止まってから走査するまでの間。短すぎると 1 文字ごとに I/O が走る。
const TYPE_SETTLE: Duration = Duration::from_millis(350);
/// 走査の下限間隔。実所要の 4 倍まで自動で空く ([`crate::git::scan_interval`])。
const SCAN_BASE: Duration = Duration::from_millis(800);

fn poll(st: &mut PanelState, ctx: &egui::Context) {
    if let Some(rx) = &st.pending {
        if let Ok(snap) = rx.try_recv() {
            st.last_cost = Some(snap.cost);
            if !st.shift_touched && snap.cfg_max_shift > 0 {
                st.max_shift = snap.cfg_max_shift;
            }
            st.snap = snap;
            st.pending = None;
            st.last_scan = Some(Instant::now());
        }
    }
    // 打鍵が落ち着いたら取り直す。
    if let Some(t) = st.typed_at {
        if t.elapsed() >= TYPE_SETTLE {
            st.typed_at = None;
            st.last_scan = None;
        }
    }
    if st.pending.is_none() && st.typed_at.is_none() && st.scanned != st.spec {
        st.last_scan = None;
    }
    if st.pending.is_none() && st.typed_at.is_none() {
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.scanned = st.spec.clone();
            st.pending = spawn_scan(st.root.clone(), st.spec.clone());
            if st.pending.is_none() {
                st.last_scan = Some(Instant::now());
            }
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す (閉じたら 0 コスト)。
    ctx.request_repaint_after(Duration::from_millis(400));
}

/// いま手元にある値から計画を作る。**I/O をしないので描画中に呼んでよい。**
///
/// 1 件しか無くても [`allocate`] を通す。[`offer`] だけだと
/// 「[`Offer::Wait`]」しか返らず、**「空きが無い」と「空きはあるが遠い」を
/// 区別できない**ため — その区別こそが、上限を緩めれば通るのかどうかを
/// 決める唯一の情報である。
fn current_plan(st: &PanelState) -> Option<(Want, Plan)> {
    let r = region::parse(&st.spec).ok()?;
    let id = region::render(&r);
    let mut want = if st.movable || st.size_only {
        Want::movable(&id, r)
    } else {
        Want::fixed(&id, r)
    };
    if st.size_only {
        want = want.size_only();
    }
    want = want.max_shift(st.max_shift);
    let occ: Vec<(String, Region)> = st
        .snap
        .held
        .iter()
        .map(|(h, r, _)| (h.clone(), r.clone()))
        .collect();
    let plan = allocate(
        std::slice::from_ref(&want),
        &occ,
        st.snap.file_lines,
        region::SAFE_BAND,
    );
    Some((want, plan))
}

/// 空状態のカード。**利用可能領域の中央**に 1 枚 (下や上に取り残さない)。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let w = (avail.width() * 0.72).clamp(0.0, 420.0).min(avail.width());
    let h = 120.0f32.min(avail.height());
    egui::Rect::from_center_size(avail.center(), egui::vec2(w, h))
}

/// パネルから返る操作。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Act {
    #[default]
    None,
    /// 提案の域を書式どおりクリップボードへ。
    Copy(String),
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
///
/// **ここから I/O を撃たない。** 表示するのは常に「いま手元にある値」で、
/// 1 テンポ古くてよい。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut act = Act::None;
    egui::Window::new(tr("🤝 行域の交渉 — 断らずにずらす"))
        .collapsible(false)
        .resizable(true)
        .default_width(680.0)
        .default_height(420.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            act = body(ui, &mut st);
        });
    if !open {
        st.open = false;
    }
    if let Act::Copy(text) = act {
        ctx.output_mut(|o| o.copied_text = text.clone());
        st.toast = trf("{r} をコピーしました", &[("r", text)]);
    }
}

/// 本体。押された操作を返す。
fn body(ui: &mut egui::Ui, st: &mut PanelState) -> Act {
    let mut act = Act::None;
    let vis = ui.visuals().clone();
    let dim = vis.weak_text_color();

    // ── 入力 ────────────────────────────────────────────────────────
    ui.horizontal_wrapped(|ui| {
        ui.label(tr("欲しい行域"));
        let w = (ui.available_width() - 120.0).clamp(120.0, 320.0);
        let edit = ui.add_sized(
            [w, ui.spacing().interact_size.y],
            egui::TextEdit::singleline(&mut st.spec).hint_text("src/app.rs#L120-180"),
        );
        if edit.changed() {
            st.typed_at = Some(Instant::now());
            st.toast.clear();
        }
        if st.pending.is_some() {
            ui.spinner();
        }
        if let Some(c) = st.last_cost {
            ui.label(
                egui::RichText::new(format!("{} ms", c.as_millis()))
                    .color(dim)
                    .small(),
            )
            .on_hover_text(tr("台帳の読み取りは裏のスレッドなので、UI は止まりません"));
        }
    });
    ui.horizontal_wrapped(|ui| {
        ui.checkbox(&mut st.movable, tr("ずらしてよい"))
            .on_hover_text(tr(
                "まだ 1 バイトも書いていない新規確保のときだけ入れてください。\
                 書き始めた域をずらすと、別の場所を編集させることになります。",
            ));
        ui.checkbox(&mut st.size_only, tr("行数だけ合えばよい"))
            .on_hover_text(tr(
                "開始位置に意味が無い要求です。分割してでも通し、ずらす幅の上限も外します。",
            ));
        ui.label(tr("上限"));
        let drag = ui.add(
            egui::DragValue::new(&mut st.max_shift)
                .range(0..=10_000)
                .suffix(tr(" 行")),
        );
        if drag.changed() {
            st.shift_touched = true;
        }
        drag.on_hover_text(tr(
            "ここまでならずらしてよい幅。既定 200 行は、このリポジトリの関数長 p99 (162 行) から。",
        ));
    });
    ui.separator();

    // ── 中身が無いときは、中央に 1 枚だけ ────────────────────────────
    let avail = ui.available_rect_before_wrap();
    if st.spec.trim().is_empty() || st.snap.note.is_some() {
        let msg = st.snap.note.clone().unwrap_or_else(|| {
            tr("欲しい行域を入れると、いま通るか・どこへずらせば通るかを出します")
        });
        let card = empty_card(avail);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new(msg).color(dim));
                });
            });
        });
        return act;
    }

    // ── 提案 ────────────────────────────────────────────────────────
    if let Some((_want, plan)) = current_plan(st) {
        // 通ったなら「どう通るか」、断るなら「なぜ断るか」。どちらも 1 行。
        let (msg, color, copy) = match (plan.granted.first(), plan.denied.first()) {
            (Some(g), _) => {
                let color = if g.how == How::AsRequested {
                    vis.hyperlink_color
                } else {
                    vis.warn_fg_color
                };
                let text = g
                    .regions
                    .iter()
                    .map(region::render)
                    .collect::<Vec<_>>()
                    .join(" ");
                (g.as_offer().summary(), color, Some(text))
            }
            (None, Some(d)) => (d.reason.clone(), vis.error_fg_color, None),
            (None, None) => (String::new(), vis.text_color(), None),
        };
        if !msg.is_empty() {
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(msg).color(color).strong());
            });
        }
        if let Some(text) = copy {
            ui.horizontal_wrapped(|ui| {
                if ui
                    .button(tr("この域をコピー"))
                    .on_hover_text(tr("そのまま lease の確保に渡せる表記です"))
                    .clicked()
                {
                    act = Act::Copy(text);
                }
                if !st.toast.is_empty() {
                    ui.label(egui::RichText::new(st.toast.clone()).color(dim).small());
                }
            });
        }
    }

    // ── 占有と空き (どちらも空なら見出しごと出さない) ────────────────
    egui::ScrollArea::vertical().show(ui, |ui| {
        if !st.snap.held.is_empty() {
            ui.add_space(4.0);
            ui.label(
                egui::RichText::new(trf(
                    "いま持たれている域 {n}",
                    &[("n", st.snap.held.len().to_string())],
                ))
                .small()
                .color(dim),
            );
            for (holder, r, until) in st.snap.held.iter().take(30) {
                let line = format!("{}  —  {}", region::render(r), holder);
                let life = if *until > st.snap.now {
                    trf(
                        "あと {n} 分",
                        &[("n", ((until - st.snap.now) / 60).to_string())],
                    )
                } else {
                    tr("期限は不明")
                };
                ui.label(crate::lease::ellipsize(&line, 96))
                    .on_hover_text(format!("{line}\n{life}"));
            }
        }
        if st.snap.file_lines > 0 {
            let occ: Vec<Span> = st.snap.held.iter().filter_map(|(_, r, _)| r.span).collect();
            let free = free_spans(st.snap.file_lines, &occ, region::SAFE_BAND);
            if !free.is_empty() {
                ui.add_space(4.0);
                ui.label(
                    egui::RichText::new(trf(
                        "空いている域 {n} / 全 {m} 行",
                        &[
                            ("n", free.len().to_string()),
                            ("m", st.snap.file_lines.to_string()),
                        ],
                    ))
                    .small()
                    .color(dim),
                );
                let text = free
                    .iter()
                    .take(20)
                    .map(|s| format!("L{}-{}", s.start, s.end))
                    .collect::<Vec<_>>()
                    .join("  ");
                ui.label(egui::RichText::new(text).monospace().small());
            }
        }
    });
    act
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. 登録
// ═══════════════════════════════════════════════════════════════════════════

/// パレット / 設定 / 描画の登録。
///
/// 打鍵は割り当てていない — `keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ共有ファイルなので、機能ブランチから触ると必ず衝突する。
/// **欲しい打鍵は報告に書き、統合担当が直列で入れる**。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "negotiate",
    entries: &[crate::feature::Entry {
        icon: "🤝",
        label: "行域の交渉 — 断らずにずらす",
        id: "negotiate.panel",
    }],
    dispatch: |_app, _ctx, id| match id {
        "negotiate.panel" => {
            toggle_panel();
            true
        }
        _ => false,
    },
    draw: Some(draw),
    settings: &[crate::feature::Setting {
        key: KEY_MAX_SHIFT,
        label: "ずらしてよい幅の上限 (行)",
        help: "まだ書いていない新規確保だけをここまでずらします。既定 200 行は、\
               このリポジトリの関数長 p99 (162 行) = 「長い関数でもたかだか 1 つ跨ぐ」幅。",
        default: crate::feature::SettingValue::Int(DEFAULT_MAX_SHIFT as i64),
    }],
    binds: &[],
};

// ═══════════════════════════════════════════════════════════════════════════
//  10. テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn sp(a: u32, b: u32) -> Span {
        Span { start: a, end: b }
    }

    fn reg(spec: &str) -> Region {
        region::parse(spec).expect("読める仕様")
    }

    fn held(list: &[(&str, &str)]) -> Vec<(String, Region)> {
        list.iter()
            .map(|(h, r)| ((*h).to_string(), reg(r)))
            .collect()
    }

    // ── 空き域 ───────────────────────────────────────────────────────

    #[test]
    fn 空き域の境界を全部固定する() {
        // 占有なし → ファイル全体が 1 つの空き
        assert_eq!(free_spans(100, &[], 3), vec![sp(1, 100)]);
        // 行数 0 → どこにも置けない
        assert!(free_spans(0, &[], 3).is_empty());
        // 真ん中 1 件。両側に band が食い込む
        assert_eq!(
            free_spans(100, &[sp(20, 30)], 3),
            vec![sp(1, 16), sp(34, 100)]
        );
        // 先頭に張り付いた占有 → 前の空きは消える (0 行の空きを作らない)
        assert_eq!(free_spans(100, &[sp(1, 10)], 3), vec![sp(14, 100)]);
        // 先頭近くで band が 1 行目を割る
        assert_eq!(free_spans(100, &[sp(2, 10)], 3), vec![sp(14, 100)]);
        // 末尾に張り付いた占有 → 後ろの空きは消える
        assert_eq!(free_spans(100, &[sp(95, 100)], 3), vec![sp(1, 91)]);
        // ファイル全部が占有 → 空きなし
        assert!(free_spans(10, &[sp(1, 10)], 3).is_empty());
        // ファイルが短すぎて band に埋まる
        assert!(free_spans(5, &[sp(3, 3)], 3).is_empty());
        // 重なった占有は併合される
        assert_eq!(
            free_spans(100, &[sp(20, 40), sp(30, 50)], 3),
            vec![sp(1, 16), sp(54, 100)]
        );
        // 隣接した占有も併合される (間に 0 行の空きを作らない)
        assert_eq!(
            free_spans(100, &[sp(20, 30), sp(31, 40)], 3),
            vec![sp(1, 16), sp(44, 100)]
        );
        // 並びが逆でも同じ結果 (決定的)
        assert_eq!(
            free_spans(100, &[sp(60, 70), sp(20, 30)], 3),
            free_spans(100, &[sp(20, 30), sp(60, 70)], 3)
        );
        // EOF を含む占有は file_lines まで伸ばして扱う
        assert_eq!(
            free_spans(
                100,
                &[Span {
                    start: 50,
                    end: Span::EOF
                }],
                3
            ),
            vec![sp(1, 46)]
        );
        // 壊れた占有 (start=0 / start>end) は無視する
        assert_eq!(
            free_spans(100, &[sp(0, 0), sp(40, 20)], 3),
            vec![sp(1, 100)]
        );
        // ファイルの外の占有は、内側だけを塞ぐ
        assert_eq!(free_spans(100, &[sp(95, 200)], 3), vec![sp(1, 91)]);
        assert_eq!(free_spans(100, &[sp(200, 300)], 3), vec![sp(1, 100)]);
        // band = 0 なら隙間 0 でも隣り合える
        assert_eq!(
            free_spans(100, &[sp(20, 30)], 0),
            vec![sp(1, 19), sp(31, 100)]
        );
    }

    /// **空き域の定義と衝突判定が食い違っていないこと。**
    ///
    /// 「空きだと言われた場所に置いたら衝突した」が起きたら、この機能は
    /// 存在ごと嘘になる。小さな盤面を総当たりして、
    /// [`free_spans`] が返した域のどこに置いても
    /// [`region::spans_too_close`] が偽であることを確かめる。
    #[test]
    fn 空き域に置けば必ず互いに素になる() {
        for lines in 1u32..=14 {
            for band in 0u32..=3 {
                for a0 in 1..=lines {
                    for a1 in a0..=lines {
                        for b0 in 1..=lines {
                            for b1 in b0..=lines {
                                let occ = [sp(a0, a1), sp(b0, b1)];
                                for f in free_spans(lines, &occ, band) {
                                    // 空き域の中の 1 行を切り出しても衝突しない
                                    for n in f.start..=f.end {
                                        for o in &occ {
                                            assert!(
                                                !region::spans_too_close(&sp(n, n), o, band),
                                                "空きのはずが衝突: lines={lines} band={band} \
                                                 occ={occ:?} free={f:?} n={n}"
                                            );
                                        }
                                    }
                                    // 空き域を丸ごと取っても衝突しない
                                    for o in &occ {
                                        assert!(!region::spans_too_close(&f, o, band));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn glob判定がregionと一致する() {
        // region::is_glob は非公開なので、glob だと安全側へ倒す挙動で照合する。
        for p in [
            "src/a.rs",
            "src/*.rs",
            "src/**/x.rs",
            "src/a?.rs",
            "src/[ab].rs",
            "a/b/c.rs",
        ] {
            let a = reg(&format!("{p}#L1-10"));
            let b = reg(&format!("{p}#L100-110"));
            assert_eq!(
                is_glob(p),
                region::conflicts(&a, &b, 3),
                "glob 判定がずれている: {p}"
            );
        }
    }

    // ── 提案 ─────────────────────────────────────────────────────────

    #[test]
    fn ぶつからなければそのまま通る() {
        let w = Want::fixed("t", reg("src/a.rs#L100-120"));
        let occ = held(&[("bob", "src/a.rs#L1-50"), ("eve", "src/b.rs#L100-120")]);
        assert_eq!(offer(&w, &occ, 500, 3), Offer::Grant);
    }

    #[test]
    fn 書き始めた域はずらさない() {
        // movable: false = 内容に紐づく要求。近くが空いていても Shift を返さない。
        let w = Want::fixed("t", reg("src/a.rs#L10-40"));
        let occ = held(&[("bob", "src/a.rs#L1-50")]);
        match offer(&w, &occ, 500, 3) {
            Offer::Wait { holder, until } => {
                assert_eq!(holder, "bob");
                assert_eq!(until, 0, "期限は台帳を持つ側が埋める");
            }
            other => panic!("ずらしてはいけない要求に {other:?} を返した"),
        }
    }

    #[test]
    fn 新規確保は最も近い空き域へずれる() {
        let w = Want::movable("t", reg("src/a.rs#L10-40")); // 31 行
        let occ = held(&[("bob", "src/a.rs#L1-50")]);
        match offer(&w, &occ, 500, 3) {
            Offer::Shift { to, moved } => {
                // 占有 1-50 + 安全帯 3 → 54 行目から空く
                assert_eq!(region::render(&to), "src/a.rs#L54-84");
                assert_eq!(moved, 44);
                assert!(!region::conflicts(&to, &occ[0].1, 3), "ずらした先が衝突");
            }
            other => panic!("Shift のはずが {other:?}"),
        }
    }

    #[test]
    fn 上を向いた空きが近ければ上へずれる() {
        // 200 行の要求が 300-500 を持たれている。上 (1-296) のほうが近い。
        let w = Want::movable("t", reg("src/a.rs#L310-509"));
        let occ = held(&[("bob", "src/a.rs#L300-500")]);
        match offer(&w, &occ, 1000, 3) {
            Offer::Shift { to, moved } => {
                assert_eq!(region::render(&to), "src/a.rs#L504-703");
                assert_eq!(moved, 194);
            }
            other => panic!("Shift のはずが {other:?}"),
        }
        // 上に十分な空きがあるなら上を選ぶ
        let w2 = Want::movable("t", reg("src/a.rs#L290-309"));
        match offer(&w2, &occ, 1000, 3) {
            Offer::Shift { to, moved } => {
                assert_eq!(region::render(&to), "src/a.rs#L277-296");
                assert_eq!(moved, -13);
            }
            other => panic!("Shift のはずが {other:?}"),
        }
    }

    #[test]
    fn 上限より遠いところへは飛ばさない() {
        let w = Want::movable("t", reg("src/a.rs#L10-40")).max_shift(10);
        let occ = held(&[("bob", "src/a.rs#L1-50")]);
        assert!(
            matches!(offer(&w, &occ, 500, 3), Offer::Wait { .. }),
            "上限 10 行なのに 44 行ずらした"
        );
    }

    #[test]
    fn 行数だけの要求は分割してでも通す() {
        // 60 行欲しい。空きは 30 行 × 3 箇所しかない。
        let w = Want::movable("t", reg("src/a.rs#L1-60")).size_only();
        let occ = held(&[
            ("a", "src/a.rs#L34-100"),
            ("b", "src/a.rs#L131-200"),
            ("c", "src/a.rs#L231-300"),
        ]);
        match offer(&w, &occ, 330, 3) {
            Offer::Split { parts } => {
                assert!(parts.len() >= 2, "分割されていない: {parts:?}");
                let total: u32 = parts.iter().filter_map(|p| p.span).map(|s| s.len()).sum();
                assert!(total >= 60, "行数が足りない: {total}");
                for p in &parts {
                    for (_, o) in &occ {
                        assert!(!region::conflicts(p, o, 3), "分割先が衝突: {p:?}");
                    }
                }
                assert!(region::is_disjoint(&parts, 3), "分割どうしが衝突");
            }
            other => panic!("Split のはずが {other:?}"),
        }
        // 同じ要求でも size_only でなければ分割しない
        let w2 = Want::movable("t", reg("src/a.rs#L1-60"));
        assert!(matches!(offer(&w2, &occ, 330, 3), Offer::Wait { .. }));
    }

    #[test]
    fn 入らない要求は通せないと返す() {
        let w = Want::movable("t", reg("src/a.rs#L1-600"));
        assert!(matches!(
            offer(&w, &held(&[("bob", "src/a.rs#L1-50")]), 100, 3),
            Offer::Impossible { .. }
        ));
        // 行数が分からないファイルへはずらす提案を出さない
        let w2 = Want::movable("t", reg("src/a.rs#L10-40"));
        assert!(matches!(
            offer(&w2, &held(&[("bob", "src/a.rs#L1-50")]), 0, 3),
            Offer::Impossible { .. }
        ));
        // 壊れた域
        let broken = Want::movable(
            "t",
            Region {
                path: "src/a.rs".into(),
                span: Some(sp(40, 20)),
                anchor: Anchor::default(),
            },
        );
        assert!(matches!(
            offer(&broken, &[], 100, 3),
            Offer::Impossible { .. }
        ));
    }

    #[test]
    fn ファイル全体を持たれていたら待つしかない() {
        let w = Want::movable("t", reg("src/a.rs#L10-40"));
        let occ = held(&[("bob", "src/a.rs")]);
        assert!(matches!(offer(&w, &occ, 500, 3), Offer::Wait { .. }));
        // 自分がファイル全体を欲しがっている側でも同じ
        let w2 = Want::movable("t", reg("src/a.rs"));
        let occ2 = held(&[("bob", "src/a.rs#L10-20")]);
        assert!(matches!(offer(&w2, &occ2, 500, 3), Offer::Wait { .. }));
    }

    #[test]
    fn glob相手にはずらす提案を出さない() {
        let w = Want::movable("t", reg("src/a.rs#L10-40"));
        assert!(matches!(
            offer(&w, &held(&[("bob", "src/*.rs#L1-50")]), 500, 3),
            Offer::Wait { .. }
        ));
        let w2 = Want::movable("t", reg("src/*.rs#L10-40"));
        assert!(matches!(
            offer(&w2, &held(&[("bob", "src/a.rs#L1-50")]), 500, 3),
            Offer::Wait { .. }
        ));
    }

    #[test]
    fn 期限は台帳を持つ側が埋める() {
        let mut o = Offer::Wait {
            holder: "bob".into(),
            until: 0,
        };
        fill_deadline(&mut o, 1234);
        assert_eq!(
            o,
            Offer::Wait {
                holder: "bob".into(),
                until: 1234
            }
        );
        // 既に入っている値は上書きしない
        fill_deadline(&mut o, 999);
        assert!(matches!(o, Offer::Wait { until: 1234, .. }));
        // Wait 以外は触らない
        let mut g = Offer::Grant;
        fill_deadline(&mut g, 42);
        assert_eq!(g, Offer::Grant);
    }

    #[test]
    fn 同じ入力からは同じ提案が出る() {
        let w = Want::movable("t", reg("src/a.rs#L10-40"));
        let occ = held(&[
            ("z", "src/a.rs#L200-260"),
            ("a", "src/a.rs#L1-50"),
            ("m", "src/a.rs#L100-150"),
        ]);
        let first = offer(&w, &occ, 500, 3);
        for _ in 0..20 {
            assert_eq!(offer(&w, &occ, 500, 3), first);
        }
        // 入力の並びが違っても同じ答え
        let mut rev = occ.clone();
        rev.reverse();
        assert_eq!(offer(&w, &rev, 500, 3), first);
    }

    // ── 一括配分 ─────────────────────────────────────────────────────

    #[test]
    fn 配分は互いに素で決定的() {
        let wants: Vec<Want> = (0..8)
            .map(|i| {
                Want::movable(
                    &format!("t{i:02}"),
                    reg(&format!("src/a.rs#L{}-{}", 1 + i * 5, 40 + i * 5)),
                )
            })
            .collect();
        let occ = held(&[("bob", "src/a.rs#L1-30")]);
        let plan = allocate(&wants, &occ, 600, 3);
        assert!(plan.is_disjoint(), "配分が互いに素でない: {plan:?}");
        assert!(!plan.granted.is_empty());
        assert_eq!(plan.granted.len() + plan.denied.len(), wants.len());
        for _ in 0..5 {
            assert_eq!(allocate(&wants, &occ, 600, 3), plan);
        }
        // 入力の並びを変えても同じ計画
        let mut shuffled = wants.clone();
        shuffled.reverse();
        assert_eq!(allocate(&shuffled, &occ, 600, 3), plan);
    }

    /// **出力自身が検査できること。** 小さな全組合せを回して
    /// 「[`Plan::is_disjoint`] が偽になる入力が存在しない」ことを固定する。
    #[test]
    fn 配分の出力は常に互いに素() {
        let lines = 40u32;
        for band in [0u32, 1, 3] {
            for a in 1..=6u32 {
                for b in 1..=6u32 {
                    for c in 1..=6u32 {
                        let mk = |i: u32, start: u32, len: u32, movable: bool| {
                            let r = reg(&format!("src/a.rs#L{}-{}", start, start + len - 1));
                            if movable {
                                Want::movable(&format!("t{i}"), r).max_shift(lines)
                            } else {
                                Want::fixed(&format!("t{i}"), r)
                            }
                        };
                        let wants = vec![
                            mk(0, a * 3, 5, true),
                            mk(1, b * 5, 7, false),
                            mk(2, c * 4, 6, true),
                            mk(3, 1, 9, true),
                        ];
                        let occ = held(&[("bob", "src/a.rs#L15-18")]);
                        let plan = allocate(&wants, &occ, lines, band);
                        assert!(
                            plan.is_disjoint(),
                            "band={band} a={a} b={b} c={c} で互いに素でない: {plan:?}"
                        );
                        assert_eq!(plan.granted.len() + plan.denied.len(), wants.len());
                    }
                }
            }
        }
    }

    #[test]
    fn 断った理由の内訳が取れる() {
        // 空きが足りない (飽和)
        let wants = vec![Want::movable("t0", reg("src/a.rs#L1-60"))];
        let occ = held(&[("bob", "src/a.rs#L1-100")]);
        let plan = allocate(&wants, &occ, 100, 3);
        assert_eq!(plan.deny_counts(), [1, 0, 0, 0], "NoRoom のはず: {plan:?}");
        // 空きはあるが遠い
        let wants = vec![Want::movable("t0", reg("src/a.rs#L1-30")).max_shift(5)];
        let plan = allocate(&wants, &occ, 400, 3);
        assert_eq!(plan.deny_counts(), [0, 1, 0, 0], "TooFar のはず: {plan:?}");
        // ずらせない要求
        let wants = vec![Want::fixed("t0", reg("src/a.rs#L1-30"))];
        let plan = allocate(&wants, &occ, 400, 3);
        assert_eq!(plan.deny_counts(), [0, 0, 1, 0], "Held のはず: {plan:?}");
        // 壊れている
        let wants = vec![Want::movable("t0", reg("src/a.rs#L1-600"))];
        let plan = allocate(&wants, &occ, 100, 3);
        assert_eq!(plan.deny_counts(), [0, 0, 0, 1], "Broken のはず: {plan:?}");
    }

    // ── 交渉プロトコル ───────────────────────────────────────────────

    #[test]
    fn dealは文字列と往復する() {
        let deals = vec![
            Deal::Propose {
                from: "a".into(),
                want: Want::movable("t1", reg("src/a.rs#L10-40")).max_shift(120),
            },
            Deal::Propose {
                from: "a".into(),
                want: Want::fixed("t2", reg("src/a.rs")),
            },
            Deal::Accept {
                from: "b".into(),
                id: "t1".into(),
                region: reg("src/a.rs#L10-40"),
            },
            Deal::Reject {
                from: "b".into(),
                id: "t1".into(),
                reason: "入りません".into(),
            },
            Deal::Counter {
                from: "b".into(),
                id: "t1".into(),
                offer: Offer::Shift {
                    to: reg("src/a.rs#L54-84"),
                    moved: 44,
                },
            },
            Deal::Counter {
                from: "b".into(),
                id: "t1".into(),
                offer: Offer::Split {
                    parts: vec![reg("src/a.rs#L1-20"), reg("src/a.rs#L30-40")],
                },
            },
            Deal::Counter {
                from: "b".into(),
                id: "t1".into(),
                offer: Offer::Wait {
                    holder: "c".into(),
                    until: 7,
                },
            },
            Deal::Counter {
                from: "b".into(),
                id: "t1".into(),
                offer: Offer::Grant,
            },
            Deal::Counter {
                from: "b".into(),
                id: "t1".into(),
                offer: Offer::Impossible {
                    reason: "だめ".into(),
                },
            },
        ];
        for d in &deals {
            let line = encode(d);
            assert!(!line.contains('\n'), "メッシュは行単位で運ぶ: {line}");
            assert_eq!(&decode(&line).expect("読める"), d, "往復で壊れた: {line}");
        }
        assert!(decode("これは JSON ではない").is_err());
    }

    #[test]
    fn 束で受けた提案どうしがぶつからない() {
        // 同じ域を 2 人が同時に欲しがる。片方ずつ答えると両方に「通る」と
        // 答えてしまう — まとめて配ることでそれを防ぐ。
        let inbox: Vec<String> = ["p1", "p2"]
            .iter()
            .map(|id| {
                encode(&Deal::Propose {
                    from: (*id).to_string(),
                    want: Want::movable(id, reg("src/a.rs#L10-40")),
                })
            })
            .collect();
        let out = respond(&inbox, &[], 400, 3, "me");
        assert_eq!(out.len(), 2);
        let mut placed: Vec<Region> = Vec::new();
        for line in &out {
            match decode(line).expect("読める") {
                Deal::Accept { region, .. } => placed.push(region),
                Deal::Counter {
                    offer: Offer::Shift { to, .. },
                    ..
                } => placed.push(to),
                other => panic!("想定外の返事: {other:?}"),
            }
        }
        assert_eq!(placed.len(), 2);
        assert!(
            region::is_disjoint(&placed, 3),
            "返事どうしが衝突: {placed:?}"
        );
        // 読めない行には Reject が返る (黙って落とすと送り手が永遠に待つ)
        let out = respond(&["こわれている".to_string()], &[], 400, 3, "me");
        assert!(matches!(
            decode(&out[0]).expect("読める"),
            Deal::Reject { .. }
        ));
    }

    // ── CLI ──────────────────────────────────────────────────────────

    #[test]
    fn cliの入口と終了コード() {
        assert_eq!(cli_main(&[]), 2, "サブコマンド無しは使い方の誤り");
        assert_eq!(cli_main(&["help".to_string()]), 0);
        assert_eq!(cli_main(&["しらない".to_string()]), 2);
        // 使い方に 6 つの終了コードが全部書いてある
        // (3〜5 は serve / ask がメッシュ上で使う。番号だけ足して説明を
        //  書き忘れると、呼び出し側が「断られた」と「返事が来ない」を
        //  取り違える)。
        let u = usage();
        for code in ["0 ", "1 ", "2 ", "3 ", "4 ", "5 "] {
            assert!(u.contains(code), "終了コード {code} の説明が無い");
        }
        // サブコマンドが使い方に載っている (載せ忘れると到達できない)
        for sub in ["offer", "allocate", "deal", "serve", "ask"] {
            assert!(u.contains(sub), "サブコマンド {sub} が使い方に無い");
        }
    }

    #[test]
    fn 仕様文字列から要求と占有を作れる() {
        let w = to_want(&WantIn {
            id: String::new(),
            region: "src/a.rs#L10-40".into(),
            movable: false,
            size_only: true,
            max_shift: None,
        })
        .expect("読める");
        assert_eq!(w.id, "src/a.rs#L10-40", "id 未指定なら域そのものを使う");
        assert!(w.movable, "size_only は movable を含む");
        assert_eq!(w.max_shift, DEFAULT_MAX_SHIFT);
        assert!(to_want(&WantIn {
            id: "x".into(),
            region: "src/a.rs#Lなんとか".into(),
            movable: false,
            size_only: false,
            max_shift: None,
        })
        .is_err());

        let list = vec![HeldIn {
            holder: String::new(),
            region: "src/a.rs#L1-9".into(),
            until: 55,
        }];
        let h = to_held(&list).expect("読める");
        assert_eq!(h.len(), 1);
        assert!(!h[0].0.is_empty(), "名前なしでも空文字にはしない");
        assert_eq!(deadline_of(&list, ""), 55);
        assert_eq!(deadline_of(&list, "いない"), 0);
    }

    #[test]
    fn 提案と計画のjson出力に要る欄が出る() {
        let j = serde_json::to_string(&offer_out(&Offer::Shift {
            to: reg("src/a.rs#L54-84"),
            moved: 44,
        }))
        .expect("書ける");
        assert!(j.contains("\"kind\":\"shift\""), "{j}");
        assert!(j.contains("src/a.rs#L54-84"), "{j}");
        assert!(!j.contains("\"parts\""), "空の欄まで出している: {j}");

        let plan = allocate(
            &[Want::movable("t0", reg("src/a.rs#L1-30"))],
            &held(&[("bob", "src/a.rs#L1-50")]),
            400,
            3,
        );
        let j = serde_json::to_string(&plan_out(&plan)).expect("書ける");
        assert!(j.contains("\"disjoint\":true"), "自己検査が出ていない: {j}");
        assert!(j.contains("\"how\":\"shifted\""), "{j}");
    }

    // ── レイアウト ───────────────────────────────────────────────────

    #[test]
    fn 空状態のカードは中央に収まる() {
        for (w, h) in [
            (900.0f32, 700.0f32),
            (1200.0, 300.0),
            (240.0, 120.0),
            (60.0, 40.0),
        ] {
            let avail = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(w, h));
            let card = empty_card(avail);
            assert!(avail.contains_rect(card), "はみ出した: {w}x{h} → {card:?}");
            assert!(
                (card.center() - avail.center()).length() < 0.01,
                "中央でない: {w}x{h}"
            );
        }
    }

    // ── 登録 ─────────────────────────────────────────────────────────

    #[test]
    fn 登録の約束を守っている() {
        assert_eq!(FEATURE.module, "negotiate");
        assert!(!FEATURE.entries.is_empty(), "パレットから到達できない");
        for e in FEATURE.entries {
            assert!(
                e.id.starts_with("negotiate."),
                "ID にモジュール接頭辞が無い: {}",
                e.id
            );
            assert!(!e.icon.is_empty() && !e.label.is_empty());
        }
        assert!(FEATURE.draw.is_some(), "描画が繋がっていない");
        for s in FEATURE.settings {
            assert!(
                s.key.starts_with("negotiate."),
                "設定キーの接頭辞: {}",
                s.key
            );
        }
        assert_eq!(
            FEATURE.settings.len(),
            1,
            "設定を増やすなら config.rs を触らずに済むか確かめる"
        );
    }

    // ── 実測の再現 (この機能の存在理由) ───────────────────────────────

    /// 決定的な擬似乱数 (splitmix64)。**seed が同じなら OS を問わず同じ列**。
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }
        fn below(&mut self, n: u32) -> u32 {
            (self.next() % u64::from(n.max(1))) as u32
        }
    }

    /// crowded 条件の要求を作る。
    ///
    /// `docs/conflict-zero.md` の合成リポジトリと同じ seed (20260810) を使う。
    /// **域の長さの分布 (80〜320 行) だけは表から復元した値**で、
    /// 「交渉なしで 9 件しか通らない」という公開済みの結果を再現する
    /// ([`交渉が拒否を減らす`] の最初の assert がその再現そのもの)。
    fn crowded(seed: u64, agents: u32, lines: u32, lo: u32, hi: u32) -> Vec<(String, Span)> {
        let mut r = Rng(seed);
        let mut out = Vec::new();
        for i in 0..agents {
            let len = lo + r.below(hi - lo + 1);
            let start = 1 + r.below(lines.saturating_sub(len).saturating_add(1));
            out.push((format!("a{i:02}"), sp(start, start + len - 1)));
        }
        out
    }

    fn wants_from(base: &[(String, Span)], movable: bool, size_only: bool) -> Vec<Want> {
        base.iter()
            .map(|(id, s)| {
                let r = reg(&format!("src/big.rs#L{}-{}", s.start, s.end));
                let w = if movable {
                    Want::movable(id, r)
                } else {
                    Want::fixed(id, r)
                };
                if size_only {
                    w.size_only()
                } else {
                    w
                }
            })
            .collect()
    }

    /// 位置を無視して小さい順に詰めたときに入る件数 = **この条件の上限**。
    fn packing_ceiling(base: &[(String, Span)], lines: u32, band: u32) -> usize {
        let mut lens: Vec<u32> = base.iter().map(|(_, s)| s.len()).collect();
        lens.sort_unstable();
        let mut used = 0u32;
        let mut k = 0usize;
        for l in lens {
            let need = used + l + if k > 0 { band } else { 0 };
            if need <= lines {
                used = need;
                k += 1;
            } else {
                break;
            }
        }
        k
    }

    /// **この機能の存在理由を数字で固定する。**
    ///
    /// 64 体が 1 ファイル (2000 行) へぶつかる crowded 条件で、
    /// 「断るだけ」と「ずらす」「分ける」を比べる。
    #[test]
    fn 交渉が拒否を減らす() {
        const AGENTS: u32 = 64;
        const LINES: u32 = 2000;
        const BAND: u32 = 3;
        let base = crowded(20_260_810, AGENTS, LINES, 80, 320);

        // (1) 交渉なし = ずらせない要求として配る (実測表の再現)
        let plain = allocate(&wants_from(&base, false, false), &[], LINES, BAND);
        assert!(plain.is_disjoint());
        assert_eq!(
            (plain.granted.len(), plain.denied.len()),
            (9, 55),
            "実測表 (完了 9 / 拒否 55) を再現できていない"
        );

        // (2) 交渉あり (ずらすだけ)
        let shift = allocate(&wants_from(&base, true, false), &[], LINES, BAND);
        assert!(shift.is_disjoint(), "ずらした結果が互いに素でない");

        // (3) 交渉あり (行数だけの要求 = 分割も許す)
        let split = allocate(&wants_from(&base, true, true), &[], LINES, BAND);
        assert!(split.is_disjoint(), "分割した結果が互いに素でない");

        let ceiling = packing_ceiling(&base, LINES, BAND);
        eprintln!(
            "crowded 64体/2000行: 交渉なし {}/{} · ずらす {}/{} (shift {}) · +分割 {}/{} (shift {} split {}) · 詰め込み上限 {} · 内訳 {:?} / {:?}",
            plain.granted.len(),
            plain.denied.len(),
            shift.granted.len(),
            shift.denied.len(),
            shift.shifted(),
            split.granted.len(),
            split.denied.len(),
            split.shifted(),
            split.split_up(),
            ceiling,
            shift.deny_counts(),
            split.deny_counts(),
        );

        assert!(
            shift.granted.len() > plain.granted.len(),
            "ずらしても増えていない"
        );
        assert_eq!(
            split.granted.len(),
            ceiling,
            "分割まで許せば詰め込み上限に届くはず"
        );
    }

    /// 1 つの seed の当たりではないことを確かめる。
    #[test]
    fn 交渉の効果は種を変えても出る() {
        for seed in [1u64, 2, 3, 4, 5] {
            let base = crowded(seed, 64, 2000, 80, 320);
            let plain = allocate(&wants_from(&base, false, false), &[], 2000, 3);
            let split = allocate(&wants_from(&base, true, true), &[], 2000, 3);
            assert!(split.is_disjoint());
            assert!(
                split.granted.len() > plain.granted.len(),
                "seed={seed} で増えていない ({} → {})",
                plain.granted.len(),
                split.granted.len()
            );
        }
    }
}
