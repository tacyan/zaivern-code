//! 🔒 衝突ゼロ証明 — 「**後でマージが一撃で出来る**」を、実測と証明で言い切る。
//!
//! ## なぜ要るのか
//!
//! 並列で N 体のエージェントを走らせて稼いだ時間は、**統合で払い戻される**。
//! このリポジトリには既に 4 つの層があるが、どれも「一撃でマージできる」とは
//! **言い切らない**:
//!
//! | モジュール | 役割 | 言えること |
//! |---|---|---|
//! | [`crate::lease`] | 同じファイルを 2 人に触らせない | 「配った担当は重なっていない」 |
//! | [`crate::conflict`] | 起きた重なりを早く見せる | 「近そうだ」 |
//! | [`crate::split`] | 配る前に担当表を作る | 「この配り方なら重ならない**はず**」 |
//! | 🚃 マージトレイン | 順番を決めて実際に流す | 「この順なら止まる回数が減る」 |
//!
//! どれも **予定**を語っている。エージェントは担当表どおりに書くとは限らないし
//! (`lease` の強制が効くのはフック対応の数ベンダーだけ)、`conflict` の
//! 「近そうだ」は人が読んで判断する材料でしかない。
//!
//! ここが埋めるのは最後の一段 — **実際に書かれた差分だけを見て、
//! 「この N 本は git merge で 1 件も衝突マーカを出さない」と断言する**。
//!
//! ## 証明のかたち
//!
//! 1. 全ブランチの**共通祖先** `C` を 1 つに決める (`git merge-base --octopus`)。
//!    **座標系が 1 つでなければ行域を比べても意味が無い**ので、ここが土台。
//! 2. 各ブランチについて `git diff --unified=0 C..<branch>` を読み、
//!    **C 側の行番号**で「実際に触った行域」を [`crate::region::Region`] として起こす。
//! 3. 全参加者の行域が [`crate::region::MERGE_ONLY_BAND`] 行の安全帯を挟んで
//!    **互いに素**なら [`Proof::disjoint`] を立てる。
//!
//! 互いに素なら、C の各行を書き換えた参加者は高々 1 人。git の 3 方向マージは
//! 「両側が同じ行域を変えた」ときにしか衝突マーカを出さないので、**衝突は
//! 構造的に起こり得ない**。統合先が C より進んでいるときは
//! **統合先自身も参加者に入れる** ([`Proof::base_participates`]) ので、
//! 「統合先の変更とだけ衝突する」抜け道も塞がる。
//!
//! 途中の座標系がずれない理由も安全帯が担保する。先に入った誰かが
//! 間の行を削っても、**未変更行は誰も消さない**ので、残りの参加者の間には
//! 常に `band` 行以上の未変更行が残る (削られるのは相手の行域だけ)。
//!
//! ## なぜ安全帯が 1 行で足りるのか (実測)
//!
//! 帯は長らく [`crate::region::SAFE_BAND`] = 3 を使っていたが、**それは
//! ここでは根拠の無い過剰防衛だった**。実 git で経路ごとに下限を測り直した
//! 結果 (`region.rs` の `実gitで三方向マージの下限が1行であることを測る` /
//! `実gitでパッチ適用の下限を測る`):
//!
//! | 経路 | 相手=置換 | 相手=削除 | 相手=挿入 | 下限 |
//! |---|---:|---:|---:|---:|
//! | `git merge-file -p` | 1 | 1 | 1 | **1** |
//! | `git merge-tree --write-tree` / `git merge` | 1 | 1 | 1 | **1** |
//! | `git apply` (文脈 3 行) | 3 | 3 | 3 | **3** |
//!
//! **三方向マージは 1 行離れていれば足りる。** 「diff の既定文脈が 3 行だから
//! ハンクが畳まれる」は誤りで、3 行は*表示*の話でしかない (`myers` /
//! `minimal` / `patience` / `histogram` の 4 アルゴリズムすべてで同じ結果)。
//! 3 が要るのは**パッチ適用**の経路だけで、[`crate::region::SAFE_BAND`] が
//! 3 なのは `git apply` / `git am` まで含めた最悪経路に合わせているため。
//!
//! **このモジュールの証明が保証しているのは `git merge` が衝突しないこと、
//! ただそれだけである。** 統合の実行部 ([`integrate`]) は
//! `merge-tree` + `commit-tree` + `update-ref` の 3 手しか使わず、
//! `git apply` は 1 度も通らない。よってここは
//! [`crate::region::MERGE_ONLY_BAND`] = 1 を使うのが正しい。
//!
//! > ⚠ **戻す条件**: 将来この機能に `git apply` / `git am` /
//! > `git format-patch` / `git rebase --apply` を通す経路が 1 つでも生えたら、
//! > **帯を [`crate::region::SAFE_BAND`] へ戻さなければならない**。
//! > 帯 1 で証明した組は、パッチ適用では平気で落ちる。
//! > 帯は [`Proof::band`] として画面・JSON の両方に出しているので、
//! > 「どの帯で証明したか」は後からでも必ず分かる (丸めていない)。
//!
//! ## 実測 (`cargo test --bin zai coedit:: -- --nocapture`)
//!
//! 擬似乱数 (種固定。`HashMap` / `HashSet` の反復順は 1 バイトも混ざらない) で
//! **1 ケース = 1 ファイル**を 240 本作り、2〜5 本のブランチが 120 行の
//! ファイルを**置換・削除・挿入**のいずれかで書き換える。行域は production と
//! 同じ `git diff` から取り、**全ペアを実際に `git merge`** して突き合わせた
//! (衝突したファイルは `git ls-files --unmerged` がパス単位で返す)。
//! **誇張しないために、良くない数字も並べて書く**:
//!
//! | | 帯 1 (既定) | 帯 3 (旧既定) |
//! |---|---:|---:|
//! | 回したケース | **240** | 240 (同じケース列) |
//! | 証明が立った | **122** | 72 |
//! | └ 実際に `git merge` が綺麗だった | **122 (全部)** | 72 (全部) |
//! | └ **見逃し (証明が立ったのに衝突した)** | **0** | 0 |
//! | 証明が立たなかった | 118 | 168 |
//! | └ 実際は綺麗に入った (**過剰報告**) | 18 = **15.3%** | 68 = **40.5%** |
//! | └ うち「安全帯だけが理由」(行は重なっていない) | 16 | 68 (全部) |
//!
//! * **見逃しは 1 件も無い。** これが唯一の必須条件で、テストは 1 件でも
//!   出たら `panic!` する。帯を 1 まで下げても 0 のままだった。
//! * **帯 3 → 1 で過剰報告が 40.5% → 15.3% へ落ちた。** 帯 3 では止まっていた
//!   **50 件**が新たに証明でき、そのすべてが実 git で綺麗に入った。
//!   40.5% という数字は「衝突する」と言われた 168 件のうち 68 件が実は
//!   綺麗だったという意味で、**その 68 件は 1 件残らず「行は重なっていないが
//!   安全帯より近い」だけ**だった — つまり旧既定の過剰報告は丸ごと
//!   帯の代金であり、その代金は `git apply` を使わないここでは払う必要が無い。
//! * **残った 15.3% は帯のせいではない。** 18 件の内訳は、
//!   (a) 挿入点は「行と行の間」にあるのに、こちらは直前の 1 行として
//!   持つため左へ 1 行ぶん厚い 16 件と、(b) 2 本が**同じ行を同じように削った**
//!   ため git が同一変更として畳んだ 2 件。どちらも実 git で裏取り済み
//!   (10 行目の直後と 11 行目の直後への挿入 → clean、同じ 3 行の削除どうし
//!   → clean)。安全側の倒し方なので、帯をこれ以上下げても消えない。
//!
//! 過剰報告を許して見逃しを許さないのは、**逆の間違いだけが致命的**だから。
//! 証明が嘘をつくと、ユーザーは「一撃でマージできる」を信じて夜間に無人で
//! 回し、朝に衝突マーカ入りの main を見ることになる。
//!
//! ### 並列度への意味 (純粋な算術)
//!
//! 2,000 行のファイルに 80 行の担当を配ると、1 本が占めるのは
//! 「80 行 + 安全帯」。帯 3 なら 83 行で **24 本**、帯 1 なら 81 行で
//! **24 本** — この粒度では変わらない。差が出るのは**細かく配るとき**で、
//! 20 行の担当なら 2000/23 = **86 本** → 2000/21 = **95 本** (+10%)、
//! 5 行なら 2000/8 = **250 本** → 2000/6 = **333 本** (+33%)。
//! 帯は担当 1 本ごとに定額でかかるので、**担当が細かいほど帯の比率が効く**。
//! 実測の 240 ケースで証明が 72 → 122 本 (**+69%**) 増えたのは、
//! エージェントが実際に書く差分が数行単位だからである。
//!
//! 統合そのものの実測は `実gitで一撃統合が人手ゼロで通る` が出す
//! (**4 本 / 約 0.8 秒 / 人手 0 回**。作業ツリーは一度も触らない)。
//!
//! ## 🚃 マージトレイン (`src/train.rs`) との住み分け
//!
//! **役割が違うので、どちらも要る。**
//!
//! | | 🚃 train | 🔒 coedit |
//! |---|---|---|
//! | 立ち位置 | 衝突は**起きる**前提で、止まる回数を減らす | そもそも**起き得ない**と言い切る |
//! | 出力 | 統合の順序 + 乾式検査 | 証明 (`disjoint` / 原因の組) |
//! | 実行 | `rebase` → fast-forward (履歴 1 本) | plumbing だけでマージコミット鎖を作り、**最後に参照を 1 回動かす** |
//! | 順序 | 重要 (先に入ったものが基準になる) | **無関係** (互いに素なので任意の順で同じ結果) |
//! | 失敗時 | 控えた OID へ全部戻す | **そもそも 1 つも動いていない** |
//!
//! 実行部を再実装せず委譲したかったが、`train` の実体は
//! `src/features/train.rs` の**私有**モジュール (`#[path] mod imp;`) なので、
//! 隣の機能からは呼べない (`main.rs` に `mod train;` は無い)。共有ファイルを
//! 1 バイトも触らない約束を優先し、統合はここで `merge-tree` +
//! `commit-tree` + `update-ref` の 3 手だけで組んだ — 作業ツリーを一度も
//! 触らないので、`rebase` 経路より**戻す作業そのものが存在しない**。
//!
//! ## 担保できないもの (正直に書く)
//!
//! * **意味的衝突は 1 件も見ない。** 行が離れていてもビルドは壊れる
//!   (`semconf.rs` の担当)。ここが言うのは「テキストとして一撃で入る」だけ。
//! * **`rebase` の保証はしない。** 途中のコミットが同じ行を触って戻す形だと
//!   rebase は衝突する。証明は**最終形の 3 方向マージ**についてのもの。
//! * `git merge-tree --write-tree` が無い git (2.38 未満) では**参照を
//!   1 つも動かさない**。証明だけを出して降格する。
//! * 二値ファイル・新規・削除・リネーム・モード変更は**ファイル全体**の域に
//!   落ちる。行で分けられないので、同じファイルを触った時点で衝突扱いになる
//!   (過剰報告側)。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::i18n::{tr, trf};
use crate::region::{self, Region, Span};
use crate::worktree::git_out;

// ═══════════════════════════════════════════════════════════════════════
//  1. 費用の上限 — どれも「黙って切らない」(切ったら必ず外へ出す)
// ═══════════════════════════════════════════════════════════════════════

/// 一度に証明できるブランチ数の上限。1 本あたり git を 2 回起動する。
pub const MAX_BRANCHES: usize = 24;

/// 1 本のブランチから取り込む行域の上限。超えたら**ファイル全体**へ畳む
/// (安全側)。巨大な機械生成差分で証明が O(R²) に膨れるのを止める。
pub const MAX_REGIONS: usize = 4000;

/// 返す [`Clash`] の上限。超えた分は [`Proof::truncated`] に件数で残す。
pub const MAX_CLASHES: usize = 500;

/// plumbing コミットに使う身元。リポジトリの設定を汚さないよう `-c` で
/// その 1 回だけ渡す (`user.email` 未設定の環境でも統合が通る)。
const IDENT: [&str; 4] = [
    "-c",
    "user.name=zaivern-coedit",
    "-c",
    "user.email=coedit@zaivern.invalid",
];

/// パネルの走査間隔の基準。`git::scan_interval` が実測に応じて伸ばす。
const SCAN_BASE: Duration = Duration::from_secs(5);

// ═══════════════════════════════════════════════════════════════════════
//  2. 型
// ═══════════════════════════════════════════════════════════════════════

/// なぜ互いに素でないのか。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Reason {
    /// 行域が重なっている。
    Overlap,
    /// 重なってはいないが、安全帯より近い。
    TooClose,
    /// 片方がファイル全体 (二値・新規・削除・リネーム・モード変更)。
    WholeFile,
}

impl Reason {
    /// 画面に出す短い説明 (**日本語の原文**。表示時に [`tr`] を通す)。
    pub fn label(&self) -> &'static str {
        match self {
            Reason::Overlap => "行域が重なっています",
            Reason::TooClose => "安全帯より近い行です",
            Reason::WholeFile => "ファイル全体を占める変更です",
        }
    }
}

/// 互いに素でない 1 組。**どのブランチの・どのファイルの・どの行が・誰と**。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Clash {
    /// 相手 2 人。**必ず辞書順** (`a <= b`) で入る — 出力を決定的にするため。
    pub a: String,
    pub b: String,
    /// 正規化済みのリポジトリ相対パス。
    pub path: String,
    /// `a` 側の行域 (`None` ならファイル全体)。
    pub a_span: Option<Span>,
    /// `b` 側の行域 (`None` ならファイル全体)。
    pub b_span: Option<Span>,
    /// 間にある未変更行の数。`band` 未満なら近すぎる。
    /// ファイル全体が絡むときは `None`。
    pub gap: Option<u32>,
    pub reason: Reason,
}

impl Clash {
    /// `パス:行` 形式の 1 行표示 (画面にもログにも同じ文字列で出す)。
    pub fn render(&self) -> String {
        let span = |s: &Option<Span>| match s {
            None => tr("全体"),
            Some(x) if x.start == x.end => format!("L{}", x.start),
            Some(x) if x.end == Span::EOF => format!("L{}-", x.start),
            Some(x) => format!("L{}-{}", x.start, x.end),
        };
        format!(
            "{}: {} {} ↔ {} {}",
            self.path,
            self.a,
            span(&self.a_span),
            self.b,
            span(&self.b_span)
        )
    }
}

/// 1 本のブランチが**実際に触った**行域。
///
/// 行番号は共通祖先 ([`Proof::base`]) 側の座標。`path` は
/// [`crate::lease::normalize_path`] を通した相対パスなので、
/// 大文字小文字を畳む OS では小文字になっている
/// (畳まないと macOS / Windows で `SRC/A.rs` と `src/a.rs` を**別物**として
/// 数え、衝突を見落とす)。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct BranchRegions {
    pub branch: String,
    pub regions: Vec<Region>,
    /// ファイル全体へ落ちた件数 (二値・新規・削除・リネーム・モード変更)。
    pub whole: usize,
    /// 完全に読めなかった理由。**`Some` なら証明は立たない** (fail-closed)。
    pub note: Option<String>,
}

impl BranchRegions {
    /// 触ったファイル (重複なし・辞書順)。
    pub fn files(&self) -> Vec<String> {
        let set: BTreeSet<&str> = self.regions.iter().map(|r| r.path.as_str()).collect();
        set.into_iter().map(str::to_string).collect()
    }
}

/// 衝突ゼロ証明。
///
/// **[`Proof::disjoint`] が `true` のときだけ「一撃でマージできる」と
/// 言い切ってよい。** 読めなかったもの・切ったものが 1 つでもあれば
/// `false` に倒れる (fail-closed)。
#[derive(Clone, Debug, Default, Serialize)]
pub struct Proof {
    /// 互いに素か。**これが証明そのもの。**
    pub disjoint: bool,
    /// 使った安全帯 (既定は [`crate::region::MERGE_ONLY_BAND`] = 1)。
    ///
    /// **丸めずに必ず出す。** 画面 ([`Proof::verdict`]) と `--json` の両方に
    /// 出るので、「どの帯で立った証明か」を後から取り違えられない。
    /// `--band` で下げた証明と既定の証明を、数字を見ずに混ぜないため。
    pub band: u32,
    /// 互いに素でないなら、原因の組。空でなければ `disjoint` は必ず `false`。
    pub pairs: Vec<Clash>,
    /// [`MAX_CLASHES`] を超えて**返さなかった**組の数。
    pub truncated: usize,
    /// 参加者ごとの行域。
    pub branches: Vec<BranchRegions>,
    /// 証明の座標系になった共通祖先の OID。空なら取れなかった。
    pub base: String,
    /// 統合先の参照名。
    pub base_ref: String,
    /// 統合先自身も参加者に入れたか (統合先が共通祖先より進んでいるとき)。
    pub base_participates: bool,
    /// [`MAX_BRANCHES`] を超えて**見なかった**ブランチ数。
    pub skipped: usize,
    /// 降格・打ち切りの理由。**必ず画面に出す** (無音で切らない)。
    pub note: Option<String>,
    pub took_ms: u128,
}

impl Proof {
    /// 参加者の名前 (辞書順)。
    pub fn names(&self) -> Vec<String> {
        self.branches.iter().map(|b| b.branch.clone()).collect()
    }

    /// 1 行の判定文 (**日本語の原文**を組み立てて返す)。
    pub fn verdict(&self) -> String {
        if self.disjoint {
            return trf(
                "✅ {n} 本は互いに素です — 一撃でマージできます (安全帯 {b} 行)",
                &[
                    ("n", self.branches.len().to_string()),
                    ("b", self.band.to_string()),
                ],
            );
        }
        if let Some(note) = &self.note {
            if self.pairs.is_empty() {
                return trf(
                    "⛔ 証明できません (安全帯 {b} 行): {m}",
                    &[("b", self.band.to_string()), ("m", note.clone())],
                );
            }
        }
        trf(
            "⛔ {n} 組が近すぎます (安全帯 {b} 行) — このままでは人手が要ります",
            &[
                ("n", (self.pairs.len() + self.truncated).to_string()),
                ("b", self.band.to_string()),
            ],
        )
    }
}

/// 証明を立てるための「最小の手直し」1 件。
///
/// `branch` が `region` を**手放せば**、[`Yield::resolves`] 件の衝突が消える。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Yield {
    pub branch: String,
    pub region: Region,
    /// これ 1 件で消える衝突の数。
    pub resolves: usize,
}

impl Yield {
    /// 画面に出す 1 行 (**日本語の原文**)。
    pub fn render(&self) -> String {
        trf(
            "{b} が {r} を手放すと {n} 件消えます",
            &[
                ("b", self.branch.clone()),
                ("r", region::render(&self.region)),
                ("n", self.resolves.to_string()),
            ],
        )
    }
}

/// 統合の指定。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Opts {
    /// 安全帯。既定は [`crate::region::MERGE_ONLY_BAND`] = 1。
    ///
    /// この機能は `git merge` しか通さないので 1 で足りる (モジュール doc の
    /// 実測表)。`git apply` を通す経路が生えたら
    /// [`crate::region::SAFE_BAND`] へ戻すこと。
    pub band: u32,
    /// 乾式検査までで止める (参照を 1 つも動かさない)。
    pub dry_run: bool,
    /// 証明が立たなくても、乾式検査が綺麗なら統合する。
    ///
    /// **既定は `false`。** これを `true` にした統合には「一撃でマージできる」
    /// という保証が付かない (乾式検査は最終形しか見ない)。
    pub force: bool,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            band: region::MERGE_ONLY_BAND,
            dry_run: false,
            force: false,
        }
    }
}

/// 止まった理由。**参照は 1 つも動いていない。**
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Stop {
    /// 止まった段のブランチ (証明で止めたときは空)。
    pub branch: String,
    /// 衝突すると言われたファイル。
    pub files: Vec<String>,
    /// 相手 — 既に入っているブランチのうち、同じファイルを触ったもの。
    pub against: Vec<String>,
    /// **参照を動かす前**に止めたか。ここは常に `true` (設計上、動かしてから
    /// 止まる経路が無い) だが、外から読めるようにしておく。
    pub predicted: bool,
    /// 人が読む理由。
    pub detail: String,
}

/// 統合の結果。
#[derive(Clone, Debug, Default, Serialize)]
pub struct Outcome {
    pub proof: Proof,
    /// 統合できたブランチ (統合順)。
    pub merged: Vec<String>,
    /// 止まったなら理由。`None` なら全部入った。
    pub stop: Option<Stop>,
    /// `git merge-tree --write-tree` が使えたか。
    pub dry_available: bool,
    /// 統合先が指す新しい OID (動かしていなければ空)。
    pub new_head: String,
    /// 参照が開始時のままか。**止まったときは必ず `true`。**
    pub restored: bool,
    /// 統合にかかった実時間。
    pub took_ms: u128,
    /// **人手が要った回数。証明が立った経路では構造的に 0**
    /// (対話も編集も 1 度も行わない)。止まったときは人が解く必要のある
    /// 衝突の組数が入る。
    pub human_touches: u32,
    pub log: Vec<String>,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.stop.is_none()
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  3. 差分 → 行域 (純関数。git を 1 度も起動しない)
// ═══════════════════════════════════════════════════════════════════════

/// [`regions_from_diff`] の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Scan {
    pub regions: Vec<Region>,
    /// ファイル全体へ落ちた件数。
    pub whole: usize,
    /// 読めなかった理由。**`Some` なら証明を立ててはいけない。**
    pub note: Option<String>,
}

/// `git diff --unified=0` の出力から、**ベース側**の行域を起こす。
///
/// ## 決めごと (テストで固定してある)
///
/// | 差分 | 出す域 | 理由 |
/// |---|---|---|
/// | `@@ -a,b +c,d @@` (`b > 0`) | `a .. a+b-1` | ベース側で置き換わる行そのもの |
/// | 削除だけのハンク (`+c,0`) | `a .. a+b-1` | 削除も「その行を触った」— 挿入と区別しない |
/// | 挿入だけのハンク (`-a,0`) | `a .. a` | 挿入点は `a` 行目の**直後**。境界の手前 1 行を安全側に取る (`a = 0` なら 1 行目) |
/// | 新規ファイル | ファイル全体 | 行で分けられない (両側が作ると必ず衝突) |
/// | 削除ファイル | ファイル全体 | 相手の編集と必ず衝突する |
/// | リネーム | **旧新の両パス**が全体 | 追従先が読めないので安全側 |
/// | 二値ファイル | ファイル全体 | 行という概念が無い |
/// | モード変更だけ | ファイル全体 | ハンクが無いので行を特定できない |
/// | 読めない行 | — | [`Scan::note`] を立てて**証明を諦める** |
///
/// **挿入点を `a` 行目に寄せるのは過剰報告側**である。`a` 行目自体は誰も
/// 書き換えていないので、相手がそこを触っていると「衝突」と言ってしまう。
/// 逆側 (見逃し) を絶対に出さないための意図的な倒し方で、実 git の
/// 網羅テストが両方向の頻度を数字で出す。
///
/// パーサはハンク本文を**行数で数えて読み飛ばす**。`--unified=0` の本文は
/// `+` / `-` 行しか無いが、削除された行の中身がたまたま `--- a/x` や
/// `@@ -1 +1 @@` に見えることがあり、素朴な前方一致では**別ファイルの
/// 始まりと誤読する**ためである。
pub fn regions_from_diff(diff_text: &str) -> Scan {
    let mut out = Scan::default();
    let mut cur = FileAcc::default();
    // ハンク本文の残り行数。0 でないあいだはヘッダを 1 つも見ない。
    let mut body = 0usize;
    for raw in diff_text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if body > 0 {
            // `\ No newline at end of file` は行数に数えない。
            if !line.starts_with('\\') {
                body -= 1;
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("diff --git ") {
            flush(&mut cur, &mut out);
            cur = FileAcc {
                active: true,
                ..Default::default()
            };
            match header_paths(rest) {
                Some(p) => {
                    cur.old_path = Some(p.clone());
                    cur.new_path = Some(p);
                }
                // 名前が違う (リネーム) なら `rename from/to` が続く。
                // 引用符付き (`core.quotePath`) やパスに ` b/` を含む例は
                // ここでは決められないので、後段が埋められなければ諦める。
                None => {}
            }
            continue;
        }
        if !cur.active {
            continue;
        }
        if let Some((a, b, _c, d)) = parse_hunk(line) {
            cur.spans.push(base_span(a, b));
            body = b as usize + d as usize;
            continue;
        }
        if let Some(p) = line.strip_prefix("--- ") {
            match p {
                "/dev/null" => cur.created = true,
                _ => cur.old_path = strip_side(p).or(cur.old_path.take()),
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("+++ ") {
            match p {
                "/dev/null" => cur.deleted = true,
                _ => cur.new_path = strip_side(p).or(cur.new_path.take()),
            }
            continue;
        }
        if let Some(p) = line.strip_prefix("rename from ") {
            cur.old_path = Some(p.to_string());
            cur.renamed = true;
            continue;
        }
        if let Some(p) = line.strip_prefix("rename to ") {
            cur.new_path = Some(p.to_string());
            cur.renamed = true;
            continue;
        }
        if line.starts_with("new file mode ") {
            cur.created = true;
        } else if line.starts_with("deleted file mode ") {
            cur.deleted = true;
        } else if line.starts_with("old mode ") || line.starts_with("new mode ") {
            cur.mode_only = true;
        } else if line.starts_with("Binary files ") || line.starts_with("GIT binary patch") {
            cur.binary = true;
        }
    }
    flush(&mut cur, &mut out);
    let key = |r: &Region| (r.path.clone(), r.span.map(|s| (s.start, s.end)));
    out.regions.sort_by(|x, y| key(x).cmp(&key(y)));
    out.regions.dedup();
    if out.regions.len() > MAX_REGIONS {
        // 巨大な機械生成差分。行で分けるのを諦めて**ファイル全体**へ畳む
        // (証明は立ちにくくなるが、見逃しは 1 件も増えない)。
        let files: BTreeSet<String> = out.regions.iter().map(|r| r.path.clone()).collect();
        let n = out.regions.len();
        out.whole = files.len();
        out.regions = files.iter().map(|p| Region::whole(p)).collect();
        out.note = Some(trf(
            "行域が {n} 件を超えたのでファイル単位へ畳みました",
            &[("n", n.to_string())],
        ));
    }
    out
}

/// 1 ファイル分の途中状態。
#[derive(Default)]
struct FileAcc {
    active: bool,
    old_path: Option<String>,
    new_path: Option<String>,
    created: bool,
    deleted: bool,
    renamed: bool,
    binary: bool,
    mode_only: bool,
    spans: Vec<Span>,
}

/// 溜めた 1 ファイル分を [`Scan`] へ落とす。
fn flush(cur: &mut FileAcc, out: &mut Scan) {
    if !cur.active {
        return;
    }
    let old = cur.old_path.clone();
    let new = cur.new_path.clone();
    // パスが 1 つも取れないヘッダは**読めなかった**ものとして扱う。
    // ここで黙って捨てると、そのファイルの衝突を丸ごと見逃す。
    if old.is_none() && new.is_none() {
        out.note = Some(tr("パスを読めない差分がありました (証明は立てません)"));
        return;
    }
    let whole = cur.binary || cur.created || cur.deleted || cur.renamed || cur.spans.is_empty();
    if whole {
        for p in [old, new].into_iter().flatten() {
            out.regions
                .push(Region::whole(&crate::lease::normalize_path(&p)));
            out.whole += 1;
        }
        // ハンクが 1 つも無い普通のファイル = モード変更だけ。安全側に
        // ファイル全体を取ってあるので、note は立てない (読めている)。
        let _ = cur.mode_only;
        return;
    }
    let path = crate::lease::normalize_path(old.or(new).unwrap_or_default().as_str());
    for s in cur.spans.drain(..) {
        out.regions.push(Region {
            path: path.clone(),
            span: Some(s),
            anchor: region::Anchor::default(),
        });
    }
}

/// `@@ -a[,b] +c[,d] @@ …` を読む。
fn parse_hunk(line: &str) -> Option<(u32, u32, u32, u32)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let new = rest.split_once(" @@")?.0;
    let (a, b) = split_count(old)?;
    let (c, d) = split_count(new)?;
    Some((a, b, c, d))
}

/// `12,3` / `12` を `(12, 3)` / `(12, 1)` にする。
fn split_count(s: &str) -> Option<(u32, u32)> {
    match s.split_once(',') {
        Some((a, b)) => Some((a.parse().ok()?, b.parse().ok()?)),
        None => Some((s.parse().ok()?, 1)),
    }
}

/// ハンクヘッダのベース側 `(a, b)` から行域を作る。
fn base_span(a: u32, b: u32) -> Span {
    if b == 0 {
        // 純粋な挿入。挿入点は a 行目の直後。
        let at = a.max(1);
        return Span { start: at, end: at };
    }
    let start = a.max(1);
    Span {
        start,
        end: start.saturating_add(b - 1),
    }
}

/// `a/src/x.rs` → `src/x.rs`。接頭辞が無ければそのまま。
fn strip_side(p: &str) -> Option<String> {
    if p.starts_with('"') {
        // `core.quotePath` の引用。走査側は `-c core.quotePath=false` を
        // 渡しているので通常は来ないが、来たら読めないものとして扱う。
        return None;
    }
    let s = p.split('\t').next().unwrap_or(p);
    Some(
        s.strip_prefix("a/")
            .or_else(|| s.strip_prefix("b/"))
            .unwrap_or(s)
            .to_string(),
    )
}

/// `diff --git a/P b/P` の `P` を取る (**両側が同じパスのときだけ**)。
///
/// パスに空白があっても割れないよう、`a/` + P + ` b/` + P という
/// **長さの等式**から P の長さを決める。両側が違う (リネーム) ときは
/// `None` を返し、後続の `rename from` / `rename to` に任せる。
fn header_paths(rest: &str) -> Option<String> {
    let b = rest.as_bytes();
    if b.len() < 6 || !rest.starts_with("a/") {
        return None;
    }
    // len = 2 + n + 3 + n
    let n = rest.len().checked_sub(5)?;
    if n % 2 != 0 {
        return None;
    }
    let n = n / 2;
    let p1 = rest.get(2..2 + n)?;
    if rest.get(2 + n..5 + n)? != " b/" {
        return None;
    }
    let p2 = rest.get(5 + n..)?;
    (p1 == p2).then(|| p1.to_string())
}

// ═══════════════════════════════════════════════════════════════════════
//  4. 証明 (純関数の中核)
// ═══════════════════════════════════════════════════════════════════════

/// glob 記号を含むか。含むパスは「同じファイルか」が確定しないので、
/// 事前の絞り込みを効かせず [`region::conflicts`] へ丸投げする (安全側)。
fn globby(p: &str) -> bool {
    p.contains('*') || p.contains('?') || p.contains('[')
}

/// 参加者どうしの衝突を全部出す (**純関数**)。
///
/// 返るのは `(組, 打ち切った件数)`。組は `(a, b, path, span)` の辞書順で、
/// `HashMap` / `HashSet` の反復順は 1 バイトも混ざらない。
///
/// 同じブランチの中の行域どうしは**数えない** — 自分の変更が自分と衝突する
/// ことは無い。
pub fn clashes(branches: &[BranchRegions], band: u32) -> (Vec<Clash>, usize) {
    let mut out: Vec<Clash> = Vec::new();
    let mut total = 0usize;
    for i in 0..branches.len() {
        for j in (i + 1)..branches.len() {
            let (x, y) = (&branches[i], &branches[j]);
            // 名前の辞書順で向きを固定する (同点はここで割れる)。
            let flip = x.branch > y.branch;
            for ra in &x.regions {
                for rb in &y.regions {
                    // 具体パスどうしが違えば、`region::conflicts` を呼ぶまでも
                    // なく無関係。N=24 × R=数百 の二重ループを実用速度に保つ。
                    if ra.path != rb.path && !globby(&ra.path) && !globby(&rb.path) {
                        continue;
                    }
                    if !region::conflicts(ra, rb, band) {
                        continue;
                    }
                    total += 1;
                    if out.len() >= MAX_CLASHES {
                        continue;
                    }
                    let (pa, pb) = if flip { (rb, ra) } else { (ra, rb) };
                    let (na, nb) = if flip {
                        (&y.branch, &x.branch)
                    } else {
                        (&x.branch, &y.branch)
                    };
                    out.push(clash_of(na, nb, pa, pb));
                }
            }
        }
    }
    let key = |c: &Clash| {
        (
            c.a.clone(),
            c.b.clone(),
            c.path.clone(),
            c.a_span.map(|s| (s.start, s.end)),
            c.b_span.map(|s| (s.start, s.end)),
        )
    };
    out.sort_by(|p, q| key(p).cmp(&key(q)));
    out.dedup();
    (out, total.saturating_sub(MAX_CLASHES.min(total)))
}

/// 1 組ぶんの [`Clash`] を組み立てる。
fn clash_of(a: &str, b: &str, ra: &Region, rb: &Region) -> Clash {
    let (reason, gap) = match (ra.span, rb.span) {
        (Some(x), Some(y)) if !globby(&ra.path) && !globby(&rb.path) => {
            let (lo, hi) = if x.start <= y.start { (x, y) } else { (y, x) };
            if lo.end == Span::EOF || hi.start <= lo.end {
                (Reason::Overlap, Some(0))
            } else {
                let g = hi.start.saturating_sub(lo.end).saturating_sub(1);
                (Reason::TooClose, Some(g))
            }
        }
        _ => (Reason::WholeFile, None),
    };
    Clash {
        a: a.to_string(),
        b: b.to_string(),
        path: ra.path.clone(),
        a_span: ra.span,
        b_span: rb.span,
        gap,
        reason,
    }
}

/// 集めた行域から証明を立てる (**純関数**。git を 1 度も起動しない)。
///
/// `notes` に 1 つでも中身があると `disjoint` は立たない — 「読めなかったが
/// たぶん大丈夫」を**言わない**ための倒し方 (fail-closed)。
pub fn prove(branches: Vec<BranchRegions>, band: u32) -> Proof {
    let (pairs, truncated) = clashes(&branches, band);
    let unreadable: Vec<String> = branches.iter().filter_map(|b| b.note.clone()).collect();
    let note = (!unreadable.is_empty()).then(|| unreadable.join(" / "));
    Proof {
        disjoint: pairs.is_empty() && truncated == 0 && note.is_none(),
        band,
        pairs,
        truncated,
        branches,
        note,
        ..Default::default()
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  5. 実測 — git から行域を取る (**裏のスレッドから呼ぶこと**)
// ═══════════════════════════════════════════════════════════════════════

/// `<ref>^{commit}` を解決する。
fn rev(repo: &Path, r: &str) -> Result<String, String> {
    let spec = format!("{r}^{{commit}}");
    match git_out(repo, &["rev-parse", "--verify", "--quiet", &spec]) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(trf("{r} が見つかりません", &[("r", r.to_string())])),
    }
}

/// 全参加者の**共通祖先**を 1 つ決める。
///
/// これが取れないと座標系が揃わないので、行域を比べても意味が無い。
/// `--octopus` が使えない / 履歴が無関係なときは `Err`。
pub fn common_base(repo: &Path, refs: &[String]) -> Result<String, String> {
    if refs.is_empty() {
        return Err(tr("参照が 1 つもありません"));
    }
    if refs.len() == 1 {
        return rev(repo, &refs[0]);
    }
    let mut argv: Vec<&str> = vec!["merge-base", "--octopus"];
    argv.extend(refs.iter().map(String::as_str));
    match git_out(repo, &argv) {
        Ok(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => {
            // `--octopus` が無い古い git 向けの畳み込み。
            let mut acc = rev(repo, &refs[0])?;
            for r in &refs[1..] {
                acc = git_out(repo, &["merge-base", &acc, r])
                    .map(|s| s.trim().to_string())
                    .map_err(|e| trf("共通祖先が取れません: {e}", &[("e", e)]))?;
                if acc.is_empty() {
                    return Err(tr("共通祖先がありません (履歴が無関係です)"));
                }
            }
            Ok(acc)
        }
    }
}

/// 1 本ぶんの行域を実測する。**裏のスレッドから呼ぶこと** (git を 1 回起動する)。
pub fn regions_of(repo: &Path, base_oid: &str, branch: &str) -> BranchRegions {
    let out = git_out(
        repo,
        &[
            // 非 ASCII のパスを引用させない (引用されると読めないものとして
            // 証明を諦めることになる)。
            "-c",
            "core.quotePath=false",
            "diff",
            "--unified=0",
            "--no-color",
            "--no-ext-diff",
            "--find-renames",
            base_oid,
            branch,
        ],
    );
    match out {
        Ok(diff) => {
            let scan = regions_from_diff(&diff);
            BranchRegions {
                branch: branch.to_string(),
                regions: scan.regions,
                whole: scan.whole,
                note: scan.note,
            }
        }
        Err(e) => BranchRegions {
            branch: branch.to_string(),
            regions: Vec::new(),
            whole: 0,
            note: Some(trf("差分を読めません: {e}", &[("e", e)])),
        },
    }
}

/// **証明を立てる。裏のスレッドから呼ぶこと** (ブランチ 1 本あたり git を 2 回起動する)。
pub fn proof(repo: &Path, base: &str, branches: &[String], band: u32) -> Proof {
    let t0 = Instant::now();
    let fail = |msg: String| Proof {
        disjoint: false,
        band,
        base_ref: base.to_string(),
        note: Some(msg),
        took_ms: t0.elapsed().as_millis(),
        ..Default::default()
    };
    let top = match crate::worktree::repo_root(repo) {
        Ok(t) => t,
        Err(e) => return fail(e),
    };
    let base_oid = match rev(&top, base) {
        Ok(o) => o,
        Err(e) => return fail(e),
    };
    // 重複を落とし、統合先そのものは参加者から外す (辞書順に固定)。
    let mut names: Vec<String> = BTreeSet::from_iter(
        branches
            .iter()
            .filter(|b| !b.is_empty() && b.as_str() != base)
            .cloned(),
    )
    .into_iter()
    .collect();
    if names.is_empty() {
        return fail(tr("証明するブランチがありません"));
    }
    let skipped = names.len().saturating_sub(MAX_BRANCHES);
    names.truncate(MAX_BRANCHES);

    let mut all = names.clone();
    all.push(base.to_string());
    let common = match common_base(&top, &all) {
        Ok(c) => c,
        Err(e) => return fail(e),
    };

    let mut items: Vec<BranchRegions> =
        names.iter().map(|b| regions_of(&top, &common, b)).collect();
    // **統合先が共通祖先より進んでいるなら、統合先も参加者に入れる。**
    // 入れないと「統合先の変更とだけ衝突する」抜け道が残る。
    let base_participates = common != base_oid;
    if base_participates {
        let mut br = regions_of(&top, &common, base);
        br.branch = base.to_string();
        items.push(br);
    }
    items.sort_by(|a, b| a.branch.cmp(&b.branch));

    let mut p = prove(items, band);
    p.base = common;
    p.base_ref = base.to_string();
    p.base_participates = base_participates;
    p.skipped = skipped;
    if skipped > 0 {
        let extra = trf(
            "{n} 本は上限を超えたので見ていません",
            &[("n", skipped.to_string())],
        );
        p.note = Some(match p.note.take() {
            Some(n) => format!("{n} / {extra}"),
            None => extra,
        });
        p.disjoint = false; // 見ていないものがあるなら言い切らない
    }
    p.took_ms = t0.elapsed().as_millis();
    p
}

/// 統合先より先に進んでいるブランチを集める。**裏のスレッドから呼ぶこと。**
///
/// 別のワークツリーが握っているものは `held` へ回す (作業中のエージェントの
/// 足元で履歴を書き換えないため)。
pub fn candidates(repo: &Path, base: &str) -> (Vec<String>, Vec<(String, PathBuf)>) {
    let porcelain = git_out(repo, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let holders = crate::git::worktree_holders(&porcelain, repo);
    let all = git_out(
        repo,
        &["for-each-ref", "--format=%(refname:short)", "refs/heads"],
    )
    .unwrap_or_default();
    let (mut free, mut held) = (Vec::new(), Vec::new());
    for b in all.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if b == base {
            continue;
        }
        let ahead = git_out(repo, &["rev-list", "--count", &format!("{base}..{b}")])
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .unwrap_or(0);
        if ahead == 0 {
            continue;
        }
        match holders.iter().find(|(n, _)| n == b) {
            Some((_, d)) => held.push((b.to_string(), d.clone())),
            None => free.push(b.to_string()),
        }
    }
    (free, held)
}

/// 統合先を推測する。`origin/HEAD` → 現在のブランチ の順で降りる。
/// **ブランチ名は 1 つもハードコードしない。**
pub fn default_base(repo: &Path) -> String {
    let current = git_out(repo, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let head = git_out(
        repo,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    )
    .unwrap_or_default();
    if let Some((_, b)) = head.trim().split_once('/') {
        if !b.is_empty() && rev(repo, b).is_ok() {
            return b.to_string();
        }
    }
    current
}

// ═══════════════════════════════════════════════════════════════════════
//  6. 最小の手直し — 証明が立たないときに「どこを手放せば立つか」
// ═══════════════════════════════════════════════════════════════════════

/// 証明を立てるために手放す行域を提案する (**純関数**)。
///
/// 貪欲な被覆: 一番多くの衝突に絡んでいる `(ブランチ, 行域)` から順に外す。
/// **最小であることは保証しない** (最小被覆は NP 困難)。実測では
/// 「1 件ずつ潰す」より短いリストが出るが、最適とは限らない。
/// 同点はブランチ名 → パス → 開始行の辞書順で割るので、出力は決定的。
pub fn suggest(p: &Proof) -> Vec<Yield> {
    let mut left: Vec<&Clash> = p.pairs.iter().collect();
    let mut out = Vec::new();
    // 無限ループの保険 (1 件も減らない状況では抜ける)。
    while !left.is_empty() && out.len() < MAX_CLASHES {
        // 候補ごとの被覆数。`BTreeMap` なので反復順が決定的。
        let mut score: BTreeMap<(String, String, u32, u32), usize> = BTreeMap::new();
        for c in &left {
            for (name, span) in [(&c.a, c.a_span), (&c.b, c.b_span)] {
                let (s, e) = span.map(|x| (x.start, x.end)).unwrap_or((0, 0));
                *score
                    .entry((name.clone(), c.path.clone(), s, e))
                    .or_insert(0) += 1;
            }
        }
        // 被覆数が最大 → 同点はキーの辞書順。
        let Some((key, n)) = score
            .into_iter()
            .max_by(|x, y| x.1.cmp(&y.1).then_with(|| y.0.cmp(&x.0)))
        else {
            break;
        };
        let (branch, path, s, e) = key.clone();
        let region = Region {
            path: path.clone(),
            span: (s != 0 || e != 0).then_some(Span { start: s, end: e }),
            anchor: region::Anchor::default(),
        };
        out.push(Yield {
            branch: branch.clone(),
            region,
            resolves: n,
        });
        let before = left.len();
        left.retain(|c| {
            let hit = |name: &String, span: Option<Span>| {
                let (cs, ce) = span.map(|x| (x.start, x.end)).unwrap_or((0, 0));
                *name == branch && c.path == path && cs == s && ce == e
            };
            !(hit(&c.a, c.a_span) || hit(&c.b, c.b_span))
        });
        if left.len() == before {
            break; // 1 件も減らないなら打ち切る (提案が無限に伸びない)
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════
//  7. 一撃統合 — 作業ツリーを一度も触らず、最後に参照を 1 回だけ動かす
// ═══════════════════════════════════════════════════════════════════════

/// `git merge-tree --write-tree` の生の stdout。**衝突のときも終了コードは 1**
/// なので、`git_out` のエラー文から剥がして拾う。
fn merge_tree_raw(repo: &Path, a: &str, b: &str) -> Option<String> {
    let args = crate::conflict::merge_tree_argv(a, b);
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    match git_out(repo, &argv) {
        Ok(s) => Some(s),
        Err(e) => e.strip_prefix("git merge-tree: ").map(str::to_string),
    }
}

/// `merge-tree` の出力の先頭 (= マージ結果のツリー OID)。
fn first_oid(raw: &str) -> Option<String> {
    let head = raw.split('\0').next()?.trim();
    (head.len() >= 40 && head.chars().all(|c| c.is_ascii_hexdigit())).then(|| head.to_string())
}

/// **N 本を人手ゼロで統合する。**
///
/// ## 手順 (この順序が fail-closed の条件)
///
/// 1. 作業ツリーが汚れていたら始めない / 別のワークツリーが握っている参照は動かさない
/// 2. **証明** ([`proof`])。立たなければ 1 本も動かさずに返す (`opts.force` で越えられる)
/// 3. **乾式検査**。`merge-tree --write-tree` → `commit-tree` を鎖にして、
///    最終形のコミットを**参照を 1 つも動かさずに**作る。ここで衝突が出たら中止
/// 4. 着地。統合先がこのワークツリーの HEAD なら `merge --ff-only`、
///    そうでなければ `update-ref <new> <old>` (**古い値を指定した CAS**)
///
/// **3 まで参照は 1 バイトも動かない**ので、途中で失敗しても「戻す」作業が
/// 存在しない。`-X ours` のような強行は 1 か所も無い。
///
/// **裏のスレッドから呼ぶこと。** git を何度も起動する。
pub fn integrate(
    repo: &Path,
    base: &str,
    branches: &[String],
    opts: &Opts,
) -> Result<Outcome, String> {
    let t0 = Instant::now();
    let top = crate::worktree::repo_root(repo)?;
    let mut log: Vec<String> = Vec::new();

    // ① 参照の実在と、動かしてよいかを先に確かめる。
    let base_oid = rev(&top, base)?;
    let mut names: Vec<String> = Vec::new();
    for b in branches {
        if b == base || b.is_empty() || names.contains(b) {
            continue;
        }
        rev(&top, b)?;
        names.push(b.clone());
    }
    names.sort();
    if names.is_empty() {
        return Err(tr("統合するブランチがありません。"));
    }
    let porcelain = git_out(&top, &["worktree", "list", "--porcelain"]).unwrap_or_default();
    let held: Vec<String> = crate::git::worktree_holders(&porcelain, &top)
        .into_iter()
        .filter(|(n, _)| n == base || names.contains(n))
        .map(|(n, d)| format!("{n} ({})", d.display()))
        .collect();
    if !held.is_empty() {
        return Err(trf(
            "別のワークツリーが使用中の参照は動かせません: {list}",
            &[("list", held.join(", "))],
        ));
    }

    // ② 証明。**これが立たない限り「一撃」ではない。**
    let p = proof(&top, base, &names, opts.band);
    let mut out = Outcome {
        proof: p,
        restored: true,
        ..Default::default()
    };
    if !out.proof.disjoint && !opts.force {
        let files: BTreeSet<String> = out.proof.pairs.iter().map(|c| c.path.clone()).collect();
        let against: BTreeSet<String> = out
            .proof
            .pairs
            .iter()
            .flat_map(|c| [c.a.clone(), c.b.clone()])
            .collect();
        out.human_touches = (out.proof.pairs.len() + out.proof.truncated) as u32;
        out.stop = Some(Stop {
            branch: String::new(),
            files: files.into_iter().collect(),
            against: against.into_iter().collect(),
            predicted: true,
            detail: out.proof.verdict(),
        });
        out.log = vec![tr("証明が立たないので参照を 1 つも動かしていません")];
        out.took_ms = t0.elapsed().as_millis();
        return Ok(out);
    }

    // ③ 乾式検査。git が古いなら**証明だけへ綺麗に降格**する。
    //
    // 能力はバージョン番号から推定しない — ディストリがバックポートした版
    // (番号は古いのに機能はある) と、機能を削って再パッケージした版を
    // 必ず取り違える。`conflict::merge_tree_available` が実際に 1 回叩いて
    // 決めるので、判定はクレート内で 1 実装しかない。版番号は**使えなかった
    // ときの説明にだけ**使う。
    out.dry_available = crate::conflict::merge_tree_available(&top);
    if !out.dry_available {
        let version = git_out(&top, &["--version"]).unwrap_or_default();
        out.stop = Some(Stop {
            branch: String::new(),
            files: Vec::new(),
            against: Vec::new(),
            predicted: true,
            detail: trf(
                "{v} には merge-tree --write-tree がありません。証明だけを出しています。",
                &[("v", version.trim().to_string())],
            ),
        });
        out.log = vec![tr("git が古いので参照を 1 つも動かしていません")];
        out.took_ms = t0.elapsed().as_millis();
        return Ok(out);
    }

    let mut head = base_oid.clone();
    let mut merged: Vec<String> = Vec::new();
    for b in &names {
        let Some(raw) = merge_tree_raw(&top, &head, b) else {
            out.stop = Some(stop_of(
                b,
                Vec::new(),
                &merged,
                tr("乾式検査ができませんでした"),
            ));
            break;
        };
        let Some(tree) = first_oid(&raw) else {
            out.stop = Some(stop_of(
                b,
                Vec::new(),
                &merged,
                raw.lines().next().unwrap_or_default().to_string(),
            ));
            break;
        };
        let files = crate::conflict::parse_merge_tree(&raw).unwrap_or_default();
        if !files.is_empty() {
            out.stop = Some(stop_of(b, files, &merged, tr("乾式検査で衝突しました")));
            break;
        }
        let msg = format!("Merge branch '{b}' (zaivern coedit: proven disjoint)");
        let mut argv: Vec<&str> = IDENT.to_vec();
        argv.extend_from_slice(&["commit-tree", &tree, "-p", &head, "-p", b, "-m", &msg]);
        match git_out(&top, &argv) {
            Ok(c) if !c.trim().is_empty() => head = c.trim().to_string(),
            _ => {
                out.stop = Some(stop_of(
                    b,
                    Vec::new(),
                    &merged,
                    tr("統合コミットを作れませんでした"),
                ));
                break;
            }
        }
        merged.push(b.clone());
        log.push(trf(
            "{b} を混ぜました (参照はまだ動いていません)",
            &[("b", b.clone())],
        ));
    }
    if out.stop.is_some() {
        // **1 バイトも動いていない。** 戻す作業そのものが存在しない。
        out.merged.clear();
        out.human_touches = 1;
        out.restored = rev(&top, base).map(|o| o == base_oid).unwrap_or(false);
        log.push(tr("⛔ 止めました。参照は 1 つも動いていません。"));
        out.log = log;
        out.took_ms = t0.elapsed().as_millis();
        return Ok(out);
    }
    if opts.dry_run {
        log.push(tr("乾式検査だけを行いました (参照は 1 つも動いていません)"));
        out.merged = merged;
        out.log = log;
        out.took_ms = t0.elapsed().as_millis();
        return Ok(out);
    }

    // ④ 着地。ここで初めて参照が 1 回だけ動く。
    let current = git_out(&top, &["symbolic-ref", "--quiet", "--short", "HEAD"])
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let landed = if current == base {
        // 作業ツリーごと進める。**fast-forward しかしない**ので、
        // 途中で誰かが動かしていたら失敗して何も変わらない。
        let dirty = git_out(&top, &["status", "--porcelain", "--untracked-files=no"])?;
        if !dirty.trim().is_empty() {
            return Err(tr(
                "作業ツリーに未コミットの変更があります。コミットしてから始めてください。",
            ));
        }
        git_out(&top, &["merge", "--ff-only", "--quiet", &head])
    } else {
        // 古い値を指定した CAS。誰かが先に動かしていたら失敗する。
        let target = format!("refs/heads/{base}");
        git_out(&top, &["update-ref", &target, &head, &base_oid])
    };
    if let Err(e) = landed {
        out.merged.clear();
        out.human_touches = 1;
        out.restored = rev(&top, base).map(|o| o == base_oid).unwrap_or(false);
        out.stop = Some(stop_of(base, Vec::new(), &[], e));
        log.push(tr("⛔ 着地に失敗しました。参照は動いていません。"));
        out.log = log;
        out.took_ms = t0.elapsed().as_millis();
        return Ok(out);
    }
    out.new_head = head;
    out.merged = merged;
    out.human_touches = 0;
    out.restored = false; // 進んだので「開始時のまま」ではない
    log.push(trf(
        "✅ {n} 本すべてを {o} へ入れました (人手 0 回)",
        &[("n", out.merged.len().to_string()), ("o", base.to_string())],
    ));
    out.log = log;
    out.took_ms = t0.elapsed().as_millis();
    Ok(out)
}

/// 止まった理由を組み立てる。相手は**既に混ざったブランチ**から探す。
fn stop_of(branch: &str, files: Vec<String>, merged: &[String], detail: String) -> Stop {
    Stop {
        branch: branch.to_string(),
        files,
        against: merged.to_vec(),
        predicted: true,
        detail,
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  8. CLI — `zai coedit …`
// ═══════════════════════════════════════════════════════════════════════

/// `zai coedit …` の実装一式。
///
/// **`src/cli.rs` へ 1 行入るまで、ここは 1 つも呼ばれない。** cli.rs は
/// 並列ブランチが取り合う共有ファイルなので、機能ブランチ側では配線しない
/// 約束になっている (`src/features/coedit.rs` の申し送りを参照)。配線が
/// `src/cli.rs` の dispatch から `zai coedit …` として呼ばれる
/// (統合時に直列で配線済み。`allow(dead_code)` はその時点で外した)。
pub mod cliface {
    use super::*;

    /// 終了コードの意味。**この表が仕様**で、`usage()` と同じ文言を出す。
    ///
    /// | コード | 意味 |
    /// |---:|---|
    /// | 0 | 証明が立った / 統合が全部入った |
    /// | 1 | 証明が立たなかった / 統合が止まった (**参照は動いていない**) |
    /// | 2 | 引数エラー・リポジトリが読めない |
    /// | 3 | git が古くて乾式検査ができない (証明だけへ降格。参照は動いていない) |
    pub const EXIT_OK: i32 = 0;
    /// 証明が立たなかった / 統合が止まった。
    pub const EXIT_NOT_PROVEN: i32 = 1;
    /// 引数エラー。
    pub const EXIT_USAGE: i32 = 2;
    /// git が古くて乾式検査ができない。
    pub const EXIT_NO_MERGE_TREE: i32 = 3;

    fn usage() -> String {
        tr("\
    zai coedit — 衝突ゼロ証明 (後でマージが一撃で出来ることを実測で言い切る)

      zai coedit proof   [--repo <dir>] [--base <ref>] [--band <n>] [--json] [<branch>...]
      zai coedit merge   [--repo <dir>] [--base <ref>] [--band <n>] [--json] [--dry-run] [--force] [<branch>...]
      zai coedit regions [--repo <dir>] [--base <ref>] [--json] <branch>

      ブランチを省略すると、統合先より先に進んでいるものを全部使います
      (別のワークツリーが握っているものは外します)。

      --band <n>   安全帯の行数 (既定 1)。既定の 1 は三方向マージの実測下限で、
                   これ以上下げると一撃マージの保証が壊れます。パッチ適用
                   (git apply / git am) まで通す運用なら 3 を指定してください。
      --force      証明が立たなくても、乾式検査が綺麗なら統合します
                   (この統合に「一撃」の保証は付きません)。

    終了コード:
      0  証明が立った / 統合が全部入った
      1  証明が立たなかった / 統合が止まった (参照は 1 つも動いていません)
      2  引数エラー・リポジトリが読めない
      3  git が古くて乾式検査ができない (証明だけへ降格。参照は動いていません)
    ")
    }

    #[derive(Debug, Default)]
    struct Flags {
        repo: PathBuf,
        base: Option<String>,
        band: u32,
        json: bool,
        dry_run: bool,
        force: bool,
        rest: Vec<String>,
    }

    fn parse_flags(args: &[String]) -> Result<Flags, String> {
        let mut f = Flags {
            repo: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            band: region::MERGE_ONLY_BAND,
            ..Default::default()
        };
        let mut i = 0;
        while i < args.len() {
            let a = args[i].as_str();
            let mut need = |what: &str| -> Result<String, String> {
                i += 1;
                args.get(i)
                    .cloned()
                    .ok_or_else(|| trf("{f} には値が要ります", &[("f", what.to_string())]))
            };
            match a {
                "--repo" => f.repo = PathBuf::from(need("--repo")?),
                "--base" | "--onto" => f.base = Some(need("--base")?),
                "--band" => {
                    let v = need("--band")?;
                    f.band = v.parse().map_err(|_| {
                        trf("--band は数字で指定してください: {v}", &[("v", v.clone())])
                    })?;
                }
                "--json" => f.json = true,
                "--dry-run" => f.dry_run = true,
                "--force" => f.force = true,
                other if other.starts_with('-') => {
                    return Err(trf("知らないオプション: {o}", &[("o", other.to_string())]))
                }
                other => f.rest.push(other.to_string()),
            }
            i += 1;
        }
        Ok(f)
    }

    /// `zai coedit <sub>` の実体。`src/cli.rs` の dispatch から呼ばれる。
    pub fn cli_main(argv: &[String]) -> i32 {
        let Some(sub) = argv.first().map(String::as_str) else {
            print!("{}", usage());
            return EXIT_USAGE;
        };
        match sub {
            "help" | "--help" | "-h" => {
                print!("{}", usage());
                EXIT_OK
            }
            "proof" => cli_proof(&argv[1..]),
            "merge" => cli_merge(&argv[1..]),
            "regions" => cli_regions(&argv[1..]),
            other => {
                eprintln!(
                    "{}",
                    trf(
                        "zai coedit: 知らないサブコマンド {s}",
                        &[("s", other.to_string())]
                    )
                );
                print!("{}", usage());
                EXIT_USAGE
            }
        }
    }

    /// 引数で指定が無ければ候補を集める。`(統合先, 対象, 握られているもの)`。
    fn resolve_targets(f: &Flags) -> Result<(PathBuf, String, Vec<String>, Vec<String>), String> {
        let top = crate::worktree::repo_root(&f.repo)?;
        let base = match &f.base {
            Some(b) => b.clone(),
            None => default_base(&top),
        };
        if base.is_empty() {
            return Err(tr(
                "統合先のブランチが分かりません (--base で指定してください)",
            ));
        }
        let (free, held) = candidates(&top, &base);
        let held_names: Vec<String> = held
            .iter()
            .map(|(n, d)| format!("{n} ({})", d.display()))
            .collect();
        let targets = if f.rest.is_empty() {
            free
        } else {
            f.rest.clone()
        };
        Ok((top, base, targets, held_names))
    }

    fn cli_proof(args: &[String]) -> i32 {
        let f = match parse_flags(args) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                print!("{}", usage());
                return EXIT_USAGE;
            }
        };
        let (top, base, targets, held) = match resolve_targets(&f) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return EXIT_USAGE;
            }
        };
        let p = proof(&top, &base, &targets, f.band);
        if f.json {
            #[derive(Serialize)]
            struct Out<'a> {
                proof: &'a Proof,
                suggest: Vec<Yield>,
                held: Vec<String>,
            }
            let body = Out {
                suggest: suggest(&p),
                held,
                proof: &p,
            };
            println!(
                "{}",
                serde_json::to_string_pretty(&body)
                    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
            );
        } else {
            print_proof(&p, &held);
        }
        if p.disjoint {
            EXIT_OK
        } else {
            EXIT_NOT_PROVEN
        }
    }

    /// 帯の数字に**根拠を 1 語だけ添える**。数字だけ出すと、既定なのか
    /// `--band` で下げたのかが読み手に分からない。
    fn band_note(band: u32) -> String {
        if band == region::MERGE_ONLY_BAND {
            tr("既定 = 三方向マージの実測下限")
        } else if band == region::SAFE_BAND {
            tr("パッチ適用まで含めた最悪経路")
        } else {
            tr("--band で指定")
        }
    }

    fn print_proof(p: &Proof, held: &[String]) {
        println!("{}", p.verdict());
        println!(
            "{}",
            trf(
                "  基準 {c} / 統合先 {b}{extra} / 安全帯 {band} 行 ({why}) / {ms} ms",
                &[
                    ("c", p.base.chars().take(12).collect::<String>()),
                    ("b", p.base_ref.clone()),
                    ("band", p.band.to_string()),
                    ("why", band_note(p.band)),
                    (
                        "extra",
                        if p.base_participates {
                            tr(" (統合先も参加者)")
                        } else {
                            String::new()
                        }
                    ),
                    ("ms", p.took_ms.to_string()),
                ]
            )
        );
        for b in &p.branches {
            println!(
                "  {:<28} {}",
                b.branch,
                trf(
                    "{r} 域 / {f} ファイル{w}{n}",
                    &[
                        ("r", b.regions.len().to_string()),
                        ("f", b.files().len().to_string()),
                        (
                            "w",
                            if b.whole > 0 {
                                trf(" (全体 {n})", &[("n", b.whole.to_string())])
                            } else {
                                String::new()
                            }
                        ),
                        (
                            "n",
                            match &b.note {
                                Some(n) => format!(" ⚠ {n}"),
                                None => String::new(),
                            }
                        ),
                    ]
                )
            );
        }
        for c in &p.pairs {
            println!("  ⛔ {}", c.render());
        }
        if p.truncated > 0 {
            println!(
                "  {}",
                trf("ほか {n} 組", &[("n", p.truncated.to_string())])
            );
        }
        let ys = suggest(p);
        if !ys.is_empty() {
            println!(
                "{}",
                tr("最小の手直し (貪欲。最小であることは保証しません):")
            );
            for y in ys {
                println!("  ↩ {}", y.render());
            }
        }
        if !held.is_empty() {
            println!(
                "{}",
                trf(
                    "作業中で外したブランチ: {list}",
                    &[("list", held.join(", "))]
                )
            );
        }
    }

    fn cli_merge(args: &[String]) -> i32 {
        let f = match parse_flags(args) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                print!("{}", usage());
                return EXIT_USAGE;
            }
        };
        let (top, base, targets, held) = match resolve_targets(&f) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("{e}");
                return EXIT_USAGE;
            }
        };
        let opts = Opts {
            band: f.band,
            dry_run: f.dry_run,
            force: f.force,
        };
        let out = match integrate(&top, &base, &targets, &opts) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("{e}");
                return EXIT_USAGE;
            }
        };
        if f.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&out)
                    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
            );
        } else {
            print_proof(&out.proof, &held);
            for l in &out.log {
                println!("  {l}");
            }
            if let Some(s) = &out.stop {
                println!("  ⛔ {}", s.detail);
                for file in &s.files {
                    println!("     {file}");
                }
            }
            println!(
                "{}",
                trf(
                    "{n} 本 / {ms} ms / 人手 {h} 回",
                    &[
                        ("n", out.merged.len().to_string()),
                        ("ms", out.took_ms.to_string()),
                        ("h", out.human_touches.to_string()),
                    ]
                )
            );
        }
        if out.ok() {
            EXIT_OK
        } else if !out.dry_available {
            EXIT_NO_MERGE_TREE
        } else {
            EXIT_NOT_PROVEN
        }
    }

    fn cli_regions(args: &[String]) -> i32 {
        let f = match parse_flags(args) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                print!("{}", usage());
                return EXIT_USAGE;
            }
        };
        let Some(branch) = f.rest.first().cloned() else {
            eprintln!("{}", tr("ブランチを 1 つ指定してください"));
            print!("{}", usage());
            return EXIT_USAGE;
        };
        let top = match crate::worktree::repo_root(&f.repo) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("{e}");
                return EXIT_USAGE;
            }
        };
        let base = f.base.clone().unwrap_or_else(|| default_base(&top));
        let common = match common_base(&top, &[base.clone(), branch.clone()]) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("{e}");
                return EXIT_USAGE;
            }
        };
        let br = regions_of(&top, &common, &branch);
        if f.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&br)
                    .unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
            );
        } else {
            for r in &br.regions {
                println!("{}", region::render(r));
            }
            if let Some(n) = &br.note {
                eprintln!("⚠ {n}");
            }
        }
        if br.note.is_some() {
            EXIT_NOT_PROVEN
        } else {
            EXIT_OK
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════
//  9. パネル — `app.rs` を 1 バイトも触らずにウィンドウを出す
// ═══════════════════════════════════════════════════════════════════════

/// 走査 1 回ぶん。**ウィンドウより長生きさせる** (設計原則 1)。
#[derive(Clone, Debug, Default)]
struct Snapshot {
    repo: PathBuf,
    base: String,
    proof: Proof,
    held: Vec<String>,
    cost: Duration,
}

#[derive(Default)]
struct PanelState {
    open: bool,
    root: PathBuf,
    snap: Snapshot,
    pending: Option<Receiver<Snapshot>>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
    run_rx: Option<Receiver<Result<Outcome, String>>>,
    outcome: Option<Outcome>,
    error: Option<String>,
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
    let opened = state().lock().map(|s| s.open).unwrap_or(false);
    let Ok(mut st) = state().lock() else { return };
    if opened {
        st.open = false;
        return;
    }
    st.open = true;
    st.root = gui_workspace_root();
    st.last_scan = None; // 開いた回は必ず取り直す
}

/// 1 回ぶんの走査 (**裏のスレッドで動く**)。
fn scan(root: PathBuf, band: u32) -> Snapshot {
    let t0 = Instant::now();
    let Ok(top) = crate::worktree::repo_root(&root) else {
        return Snapshot {
            proof: Proof {
                note: Some(tr("git リポジトリではありません")),
                ..Default::default()
            },
            cost: t0.elapsed(),
            ..Default::default()
        };
    };
    let base = default_base(&top);
    let (free, held) = candidates(&top, &base);
    let p = proof(&top, &base, &free, band);
    Snapshot {
        repo: top,
        base,
        proof: p,
        held: held.into_iter().map(|(n, _)| n).collect(),
        cost: t0.elapsed(),
    }
}

fn spawn_scan(root: PathBuf, band: u32) -> Option<Receiver<Snapshot>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("zv-coedit-scan".into())
        .spawn(move || {
            let _ = tx.send(scan(root, band));
        })
        .ok()
        .map(|_| rx)
}

/// 統合を裏で始める。**UI スレッドは 1 ミリ秒も待たない。**
fn start_run(st: &mut PanelState) {
    if st.run_rx.is_some() {
        return;
    }
    let repo = st.snap.repo.clone();
    let base = st.snap.base.clone();
    let names = st.snap.proof.names();
    let band = st.snap.proof.band;
    let (tx, rx) = std::sync::mpsc::channel();
    let spawned = std::thread::Builder::new()
        .name("zv-coedit-run".into())
        .spawn(move || {
            let opts = Opts {
                band,
                ..Default::default()
            };
            let _ = tx.send(integrate(&repo, &base, &names, &opts));
        });
    if spawned.is_ok() {
        st.run_rx = Some(rx);
        st.error = None;
        st.outcome = None;
    }
}

/// 非同期の結果を拾い、必要なら次の走査を出す。**待たない。**
fn poll(st: &mut PanelState, ctx: &egui::Context) {
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(s) => {
                st.last_cost = Some(s.cost);
                st.snap = s;
                st.last_scan = Some(Instant::now());
                st.pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if let Some(rx) = &st.run_rx {
        match rx.try_recv() {
            Ok(Ok(o)) => {
                st.outcome = Some(o);
                st.run_rx = None;
                st.last_scan = None; // 参照が動いたので取り直す
            }
            Ok(Err(e)) => {
                st.error = Some(e);
                st.run_rx = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.run_rx = None,
        }
    }
    if st.pending.is_none() && st.run_rx.is_none() {
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = spawn_scan(st.root.clone(), region::MERGE_ONLY_BAND);
            if st.pending.is_none() {
                st.last_scan = Some(Instant::now());
            }
        }
    }
    ctx.request_repaint_after(Duration::from_millis(500));
}

/// パネルから返る操作。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Act {
    #[default]
    None,
    /// 走査をやり直す。
    Rescan,
    /// 統合を始める。
    Run,
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
///
/// **ここから git を撃たない。** 表示するのは常に「いま手元にある値」で、
/// 1 テンポ古くてよい (番人テスト `描画から同期gitを撃たない` がある)。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut act = Act::None;
    egui::Window::new(tr("🔒 衝突ゼロ証明 — 一撃でマージできるか"))
        .collapsible(false)
        .resizable(true)
        .default_width(640.0)
        .default_height(380.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            act = body(ui, &st);
        });
    if !open {
        st.open = false;
    }
    match act {
        Act::Rescan => st.last_scan = None,
        Act::Run => start_run(&mut st),
        Act::None => {}
    }
}

/// 本体。押された操作を返す。**幅に収める**ため、長い行は省略してホバーで全文。
fn body(ui: &mut egui::Ui, st: &PanelState) -> Act {
    let mut act = Act::None;
    let vis = ui.visuals().clone();
    let dim = vis.weak_text_color();
    let p = &st.snap.proof;
    ui.horizontal_wrapped(|ui| {
        let color = if p.disjoint {
            vis.hyperlink_color
        } else {
            vis.warn_fg_color
        };
        ui.label(egui::RichText::new(p.verdict()).strong().color(color))
            .on_hover_text(trf(
                "安全帯 {b} 行で証明しています。\n\
                 三方向マージ (git merge) の実測下限は 1 行、\n\
                 パッチ適用 (git apply) まで通すなら 3 行が要ります。",
                &[("b", p.band.to_string())],
            ));
        if let Some(c) = st.last_cost {
            ui.label(
                egui::RichText::new(format!("{} ms", c.as_millis()))
                    .color(dim)
                    .small(),
            )
            .on_hover_text(tr("走査は裏のスレッドで行うので、UI は止まりません"));
        }
        if st.pending.is_some() || st.run_rx.is_some() {
            ui.spinner();
        }
    });
    if let Some(n) = &p.note {
        ui.label(egui::RichText::new(n).color(vis.warn_fg_color).small());
    }
    if let Some(e) = &st.error {
        ui.label(egui::RichText::new(e).color(vis.error_fg_color).small());
    }
    ui.separator();
    // 中身が空のセクションは高さを確保しない (見出しごと消す)。
    if p.branches.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(tr("統合先より先に進んでいるブランチがありません"));
        });
    } else {
        egui::ScrollArea::vertical()
            .max_height(220.0)
            .show(ui, |ui| {
                for b in &p.branches {
                    let line = format!(
                        "{}  —  {} 域 / {} ファイル",
                        b.branch,
                        b.regions.len(),
                        b.files().len()
                    );
                    ui.label(elide(&line, ui.available_width(), ui))
                        .on_hover_text(&line);
                }
                for c in &p.pairs {
                    let line = format!("⛔ {}", c.render());
                    ui.label(
                        egui::RichText::new(elide(&line, ui.available_width(), ui))
                            .color(vis.warn_fg_color),
                    )
                    .on_hover_text(format!(
                        "{}\n{}",
                        line,
                        tr(c.reason.label())
                    ));
                }
                for y in suggest(p) {
                    let line = format!("↩ {}", y.render());
                    ui.label(
                        egui::RichText::new(elide(&line, ui.available_width(), ui)).color(dim),
                    )
                    .on_hover_text(&line);
                }
            });
    }
    if let Some(o) = &st.outcome {
        ui.separator();
        for l in &o.log {
            ui.label(egui::RichText::new(l).small());
        }
    }
    ui.separator();
    ui.horizontal_wrapped(|ui| {
        if ui.button(tr("再走査")).clicked() {
            act = Act::Rescan;
        }
        let ready = p.disjoint && !p.branches.is_empty() && st.run_rx.is_none();
        let hint = if ready {
            tr("証明が立っています。人手ゼロで統合します (失敗しても参照は 1 つも動きません)。")
        } else {
            tr("証明が立つまで統合しません。上の「手放すと消えます」を参考に直してください。")
        };
        if ui
            .add_enabled(ready, egui::Button::new(tr("一撃で統合")))
            .on_hover_text(&hint)
            .on_disabled_hover_text(&hint)
            .clicked()
        {
            act = Act::Run;
        }
        if !st.snap.held.is_empty() {
            ui.label(
                egui::RichText::new(trf(
                    "作業中 {n} 本は外しています",
                    &[("n", st.snap.held.len().to_string())],
                ))
                .color(dim)
                .small(),
            )
            .on_hover_text(st.snap.held.join("\n"));
        }
    });
    act
}

/// 可用幅に収まるよう末尾を省略する (**どの幅でも見切れない**)。
fn elide(text: &str, avail: f32, ui: &egui::Ui) -> String {
    let per = ui.text_style_height(&egui::TextStyle::Body) * 0.55;
    let max = ((avail / per.max(1.0)) as usize).max(8);
    elide_to(text, max)
}

/// 文字数上限で省略する (**純関数**。テストはこちらを通る)。
fn elide_to(text: &str, max: usize) -> String {
    let n = text.chars().count();
    if n <= max {
        return text.to_string();
    }
    let keep = max.saturating_sub(1);
    text.chars().take(keep).collect::<String>() + "…"
}

// ═══════════════════════════════════════════════════════════════════════
//  10. 登録
// ═══════════════════════════════════════════════════════════════════════

/// パレットへの登録。
///
/// 打鍵は割り当てていない — `keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ最も硬い共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する。
/// **欲しい打鍵は統合担当へ報告して直列に入れてもらう。**
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "coedit",
    entries: &[crate::feature::Entry {
        icon: "🔒",
        label: "衝突ゼロ証明 (一撃マージ)",
        id: "coedit.proof",
    }],
    dispatch: |_app, _ctx, id| match id {
        "coedit.proof" => {
            toggle_panel();
            true
        }
        _ => false,
    },
    draw: Some(draw),
    binds: &[],
    ..crate::feature::Feature::DEFAULT
};

// ═══════════════════════════════════════════════════════════════════════
//  11. テスト
// ═══════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::cliface::*;
    use super::*;

    fn r(path: &str, from: u32, to: u32) -> Region {
        Region {
            path: path.into(),
            span: Some(Span {
                start: from,
                end: to,
            }),
            anchor: region::Anchor::default(),
        }
    }

    fn br(name: &str, regions: Vec<Region>) -> BranchRegions {
        BranchRegions {
            branch: name.into(),
            regions,
            whole: 0,
            note: None,
        }
    }

    // ── 差分 → 行域 (純関数) ──────────────────────────────────────

    #[test]
    fn 差分から行域を起こす表() {
        // (名前, 差分, 期待する行域の表記, 期待する全体件数)
        let cases: Vec<(&str, &str, Vec<&str>, usize)> = vec![
            (
                "普通の置換",
                "diff --git a/src/a.rs b/src/a.rs\n\
                 index 1..2 100644\n\
                 --- a/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -10,3 +10,3 @@\n\
                 -old1\n-old2\n-old3\n+new1\n+new2\n+new3\n",
                vec!["src/a.rs#L10-12"],
                0,
            ),
            (
                "削除だけのハンク — ベース側の行をそのまま取る",
                "diff --git a/src/a.rs b/src/a.rs\n\
                 --- a/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -20,4 +19,0 @@\n\
                 -a\n-b\n-c\n-d\n",
                vec!["src/a.rs#L20-23"],
                0,
            ),
            (
                "挿入だけのハンク — 挿入点の手前 1 行を安全側に取る",
                "diff --git a/src/a.rs b/src/a.rs\n\
                 --- a/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -30,0 +31,2 @@\n\
                 +x\n+y\n",
                vec!["src/a.rs#L30"],
                0,
            ),
            (
                "先頭への挿入 — 0 行目は無いので 1 行目へ寄せる",
                "diff --git a/src/a.rs b/src/a.rs\n\
                 --- a/src/a.rs\n\
                 +++ b/src/a.rs\n\
                 @@ -0,0 +1,2 @@\n\
                 +x\n+y\n",
                vec!["src/a.rs#L1"],
                0,
            ),
            (
                "新規ファイルはファイル全体",
                "diff --git a/new.rs b/new.rs\n\
                 new file mode 100644\n\
                 --- /dev/null\n\
                 +++ b/new.rs\n\
                 @@ -0,0 +1,1 @@\n\
                 +hello\n",
                vec!["new.rs", "new.rs"],
                2,
            ),
            (
                "削除ファイルはファイル全体",
                "diff --git a/gone.rs b/gone.rs\n\
                 deleted file mode 100644\n\
                 --- a/gone.rs\n\
                 +++ /dev/null\n\
                 @@ -1,1 +0,0 @@\n\
                 -bye\n",
                vec!["gone.rs", "gone.rs"],
                2,
            ),
            (
                "リネームは旧新の両方が全体",
                "diff --git a/old.rs b/new2.rs\n\
                 similarity index 90%\n\
                 rename from old.rs\n\
                 rename to new2.rs\n",
                vec!["new2.rs", "old.rs"],
                2,
            ),
            (
                "二値ファイルはファイル全体 (行という概念が無い)",
                "diff --git a/img.png b/img.png\n\
                 index 1..2 100644\n\
                 Binary files a/img.png and b/img.png differ\n",
                vec!["img.png", "img.png"],
                2,
            ),
            (
                "モード変更だけでもファイル全体 (行を特定できない)",
                "diff --git a/run.sh b/run.sh\n\
                 old mode 100644\n\
                 new mode 100755\n",
                vec!["run.sh", "run.sh"],
                2,
            ),
            (
                "空白を含むパス",
                "diff --git a/my dir/a b.rs b/my dir/a b.rs\n\
                 --- a/my dir/a b.rs\n\
                 +++ b/my dir/a b.rs\n\
                 @@ -5,1 +5,1 @@\n\
                 -x\n+y\n",
                vec!["my dir/a b.rs#L5"],
                0,
            ),
            (
                "削除行の中身がヘッダに見えても誤読しない",
                "diff --git a/doc.md b/doc.md\n\
                 --- a/doc.md\n\
                 +++ b/doc.md\n\
                 @@ -7,2 +7,1 @@\n\
                 ---- a/fake.rs\n\
                 -@@ -1,1 +1,1 @@\n\
                 +one line\n",
                vec!["doc.md#L7-8"],
                0,
            ),
            ("空の差分", "", vec![], 0),
        ];
        for (name, diff, want, whole) in cases {
            let got = regions_from_diff(diff);
            let rendered: Vec<String> = got.regions.iter().map(region::render).collect();
            let want_norm: Vec<String> = want
                .iter()
                .map(|w| {
                    // 期待値も同じ正規化 (大文字小文字を畳む OS がある)
                    let re = region::parse(w).expect("期待値の書式");
                    region::render(&Region {
                        path: crate::lease::normalize_path(&re.path),
                        ..re
                    })
                })
                .collect();
            let mut want_sorted = want_norm.clone();
            want_sorted.sort();
            want_sorted.dedup();
            let mut got_sorted = rendered.clone();
            got_sorted.sort();
            got_sorted.dedup();
            assert_eq!(got_sorted, want_sorted, "{name}: {rendered:?}");
            assert_eq!(got.whole, whole, "{name}: 全体件数");
            assert!(got.note.is_none(), "{name}: {:?}", got.note);
        }
    }

    #[test]
    fn 読めない差分は証明を諦める() {
        // パスの取れないヘッダ (`core.quotePath` の引用など)。
        let diff = "diff --git \"a/\\346\\227\\245.rs\" \"b/\\346\\227\\245.rs\"\n\
                    --- \"a/\\346\\227\\245.rs\"\n\
                    +++ \"b/\\346\\227\\245.rs\"\n\
                    @@ -1,1 +1,1 @@\n-a\n+b\n";
        let got = regions_from_diff(diff);
        assert!(got.note.is_some(), "読めないと言う");
        let p = prove(
            vec![BranchRegions {
                branch: "x".into(),
                regions: got.regions,
                whole: got.whole,
                note: got.note,
            }],
            region::MERGE_ONLY_BAND,
        );
        assert!(!p.disjoint, "読めなかったら証明しない (fail-closed)");
    }

    // ── 証明 (純関数) ────────────────────────────────────────────

    #[test]
    fn 証明の表() {
        let band = region::MERGE_ONLY_BAND;
        // 間に未変更行が 1 行あれば素 (三方向マージの実測下限ちょうど)
        let p = prove(
            vec![
                br("a", vec![r("f.rs", 1, 10)]),
                br("b", vec![r("f.rs", 12, 20)]),
            ],
            band,
        );
        assert!(p.disjoint, "間に 1 行あるので素: {:?}", p.pairs);

        // **帯 3 なら止めていた組**が、帯 1 では素になる (過剰報告が減る箇所)
        let p = prove(
            vec![
                br("a", vec![r("f.rs", 1, 10)]),
                br("b", vec![r("f.rs", 13, 20)]),
            ],
            band,
        );
        assert!(p.disjoint, "間に 2 行: 帯 1 なら素 {:?}", p.pairs);
        let p3 = prove(
            vec![
                br("a", vec![r("f.rs", 1, 10)]),
                br("b", vec![r("f.rs", 13, 20)]),
            ],
            region::SAFE_BAND,
        );
        assert!(!p3.disjoint, "帯 3 では同じ組が止まる (差はここ)");

        // 隣接 (間に 1 行も無い) → 近すぎる。git の三方向マージも実際に衝突する
        let p = prove(
            vec![
                br("a", vec![r("f.rs", 1, 10)]),
                br("b", vec![r("f.rs", 11, 20)]),
            ],
            band,
        );
        assert!(!p.disjoint);
        assert_eq!(p.pairs.len(), 1);
        assert_eq!(p.pairs[0].gap, Some(0));
        assert_eq!(p.pairs[0].a, "a", "向きは辞書順で固定");
        assert_eq!(p.band, band, "どの帯で判定したかを丸めずに持つ");
        assert!(
            p.verdict().contains(&band.to_string()),
            "画面の 1 行に帯が出る: {}",
            p.verdict()
        );

        // 別ファイルなら何行でも素
        let p = prove(
            vec![
                br("a", vec![r("f.rs", 1, 100)]),
                br("b", vec![r("g.rs", 1, 100)]),
            ],
            band,
        );
        assert!(p.disjoint);

        // ファイル全体が絡むと必ず衝突
        let p = prove(
            vec![
                br("a", vec![Region::whole("f.rs")]),
                br("b", vec![r("f.rs", 500, 501)]),
            ],
            band,
        );
        assert!(!p.disjoint);
        assert_eq!(p.pairs[0].reason, Reason::WholeFile);

        // 同じブランチの中の近い域は数えない
        let p = prove(
            vec![br("a", vec![r("f.rs", 1, 10), r("f.rs", 11, 12)])],
            band,
        );
        assert!(p.disjoint, "自分と自分は衝突しない");
    }

    #[test]
    fn 名前の順を入れ替えても同じ結果が出る() {
        let band = region::MERGE_ONLY_BAND;
        let one = prove(
            vec![
                br("zeta", vec![r("f.rs", 1, 10)]),
                br("alpha", vec![r("f.rs", 11, 20)]),
            ],
            band,
        );
        let two = prove(
            vec![
                br("alpha", vec![r("f.rs", 11, 20)]),
                br("zeta", vec![r("f.rs", 1, 10)]),
            ],
            band,
        );
        assert_eq!(one.pairs, two.pairs, "入力順は出力へ漏れない");
        assert_eq!(one.pairs[0].a, "alpha");
        assert_eq!(one.pairs[0].b, "zeta");
    }

    #[test]
    fn 最小の手直しを提案する() {
        let band = region::MERGE_ONLY_BAND;
        // b の 1 つの域が a と c の両方とぶつかる → b が手放すのが最短。
        let p = prove(
            vec![
                br("a", vec![r("f.rs", 10, 12)]),
                br("b", vec![r("f.rs", 13, 15)]),
                br("c", vec![r("f.rs", 16, 18)]),
            ],
            band,
        );
        assert!(!p.disjoint);
        let ys = suggest(&p);
        assert_eq!(ys[0].branch, "b", "一番多くぶつかっている側を先に出す");
        assert!(ys[0].resolves >= 2, "{:?}", ys);
        // 提案どおり手放すと本当に証明が立つ。
        let after = prove(
            vec![
                br("a", vec![r("f.rs", 10, 12)]),
                br("b", vec![]),
                br("c", vec![r("f.rs", 16, 18)]),
            ],
            band,
        );
        assert!(after.disjoint, "手放したら立つ: {:?}", after.pairs);
    }

    #[test]
    fn 省略はどの幅でも見切れない() {
        assert_eq!(elide_to("abc", 10), "abc");
        assert_eq!(elide_to("abcdefghij", 5), "abcd…");
        // 日本語 (幅の広い文字) でも文字数で切れる
        assert_eq!(elide_to("あいうえお", 3), "あい…");
        assert_eq!(elide_to("", 0), "");
    }

    #[test]
    fn 描画から同期gitを撃たない() {
        // 描画のたびに git の完了を待つと、そのままフレームが止まる
        // (このリポジトリの実測で最悪 4376ms)。
        let src = include_str!("coedit.rs").replace("\r\n", "\n");
        for sig in [
            "pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {",
            "fn body(ui: &mut egui::Ui, st: &PanelState) -> Act {",
        ] {
            let body = src
                .split(sig)
                .nth(1)
                .unwrap_or_else(|| panic!("{sig} が見つからない"));
            let body = body.split("\n}\n").next().expect("本体の終端");
            for bad in ["git_out(", "proof(", "integrate(", "candidates("] {
                assert!(
                    !body.contains(bad),
                    "{sig} が {bad} を同期で呼んでいる (UI スレッドが止まる)"
                );
            }
        }
    }

    #[test]
    fn 引数エラーは2で返る() {
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| (*x).to_string()).collect() };
        assert_eq!(cli_main(&[]), EXIT_USAGE);
        assert_eq!(cli_main(&s(&["nope"])), EXIT_USAGE);
        assert_eq!(cli_main(&s(&["proof", "--nope"])), EXIT_USAGE);
        assert_eq!(cli_main(&s(&["proof", "--band"])), EXIT_USAGE);
        assert_eq!(cli_main(&s(&["proof", "--band", "x"])), EXIT_USAGE);
        assert_eq!(cli_main(&s(&["help"])), EXIT_OK);
    }

    // ── 実 git ───────────────────────────────────────────────────

    struct Repo(PathBuf);

    impl Repo {
        fn git(&self, args: &[&str]) -> String {
            git_out(&self.0, args).unwrap_or_default()
        }
        fn try_git(&self, args: &[&str]) -> Result<String, String> {
            git_out(&self.0, args)
        }
        fn write(&self, rel: &str, text: &str) {
            let p = self.0.join(rel);
            if let Some(d) = p.parent() {
                std::fs::create_dir_all(d).expect("親ディレクトリ");
            }
            std::fs::write(p, text).expect("書き込み");
        }
        fn commit(&self, rel: &str, text: &str, msg: &str) {
            self.write(rel, text);
            self.git(&["add", "--all"]);
            self.git(&["commit", "--quiet", "-m", msg]);
        }
        fn oid(&self, r: &str) -> String {
            self.git(&["rev-parse", r])
        }
    }

    /// 実 git リポジトリを作る。**実 `~/.zaivern` には触れない。**
    /// git が無い / 古い環境では `None` を返してテストを綺麗に飛ばす。
    fn make_repo(tag: &str) -> Option<Repo> {
        if git_out(Path::new("."), &["--version"]).is_err() {
            return None;
        }
        let dir = crate::test_util::unique_temp_dir("zv-coedit", tag);
        // 既定ブランチ名は git の版で変わるので**必ず自分で決める**。
        if git_out(&dir, &["init", "--quiet", "--initial-branch=base"]).is_err() {
            git_out(&dir, &["init", "--quiet"]).ok()?;
            git_out(&dir, &["checkout", "--quiet", "-b", "base"]).ok()?;
        }
        let r = Repo(dir);
        r.git(&["config", "user.name", "zaivern test"]);
        r.git(&["config", "user.email", "test@zaivern.invalid"]);
        r.git(&["config", "commit.gpgsign", "false"]);
        Some(r)
    }

    fn lines(n: usize) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    /// 決定的な擬似乱数 (xorshift64*)。**`Math.random` 的な非決定は禁止**なので、
    /// 種を固定して同じケース列を毎回作る。
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Rng {
            Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
        }
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_F491_4F6C_DD1D)
        }
        /// `[lo, hi]` の一様乱数。
        fn range(&mut self, lo: usize, hi: usize) -> usize {
            lo + (self.next() % ((hi - lo + 1) as u64)) as usize
        }
    }

    /// 合成ファイルの `at` 行目から `len` 行を書き換えた本文。
    fn edited(total: usize, at: usize, len: usize, tag: &str) -> String {
        (1..=total)
            .map(|i| {
                if i >= at && i < at + len {
                    format!("{tag} {i}\n")
                } else {
                    format!("line {i}\n")
                }
            })
            .collect()
    }

    /// 書き換えの種別。**帯の表は種別ごとに測ってある**ので、生成側も
    /// 置換だけでなく削除・挿入を混ぜる (置換しか作らないと、いちばん
    /// 危ない「挿入点は行と行の間にある」経路を 1 度も踏まない)。
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Kind {
        Replace,
        Delete,
        Insert,
    }

    /// 1 か所の書き換え。`at` は**ベースの行番号** (1 始まり)。
    #[derive(Clone, Copy, Debug)]
    struct Edit {
        at: usize,
        len: usize,
        kind: Kind,
    }

    impl Edit {
        /// この書き換えが `git diff` のベース側で占める行域。
        /// 挿入は「行と行の間」なので、直前の行 1 行として現れる。
        fn span(&self) -> (usize, usize) {
            match self.kind {
                Kind::Insert => (self.at, self.at),
                _ => (self.at, self.at + self.len - 1),
            }
        }
    }

    /// ベース本文へ書き換えを適用する。**後ろから当てる**ので、
    /// 前の書き換えで行番号がずれない。
    fn apply(base: &[String], edits: &[Edit], tag: &str) -> String {
        let mut v: Vec<String> = base.to_vec();
        let mut es = edits.to_vec();
        es.sort_by(|a, b| b.at.cmp(&a.at));
        for e in &es {
            match e.kind {
                Kind::Replace => {
                    for i in e.at..(e.at + e.len).min(v.len() + 1) {
                        v[i - 1] = format!("{tag} {i}");
                    }
                }
                Kind::Delete => {
                    let hi = (e.at + e.len - 1).min(v.len());
                    v.drain((e.at - 1)..hi);
                }
                Kind::Insert => {
                    let add: Vec<String> = (0..e.len)
                        .map(|k| format!("{tag} ins {} {k}", e.at))
                        .collect();
                    let at = e.at.min(v.len());
                    v.splice(at..at, add);
                }
            }
        }
        let mut out = v.join("\n");
        out.push('\n');
        out
    }

    /// 次に書き換える場所を決める。
    ///
    /// **一様乱数だけでは境界を踏まない。** 120 行に数個の域を散らすと、
    /// 間隔がちょうど 1〜2 行になる確率はごく低く、「帯 1 で本当に大丈夫か」を
    /// 1 度も試さないまま緑になってしまう。半分は**既に置いた域から
    /// 0〜4 行だけ離した位置**を採り、帯 1 の境界を正面から踏む。
    fn spot(rng: &mut Rng, placed: &[(usize, usize)], len: usize, total: usize) -> usize {
        let far = |rng: &mut Rng| rng.range(2, total - len - 2);
        if placed.is_empty() || rng.range(0, 1) == 0 {
            return far(rng);
        }
        let (lo, hi) = placed[rng.range(0, placed.len() - 1)];
        let gap = rng.range(0, 4);
        let cand = if rng.range(0, 1) == 0 {
            hi + 1 + gap
        } else {
            match lo.checked_sub(gap + len) {
                Some(v) if v >= 2 => v,
                _ => return far(rng),
            }
        };
        if cand < 2 || cand + len + 1 >= total {
            far(rng)
        } else {
            cand
        }
    }

    /// **この機能の主張そのもの。** 証明が立ったケースは実 git で必ず綺麗に入る。
    ///
    /// # 数え方 — 1 ケース = 1 ファイル
    ///
    /// 以前は「1 ケースごとに枝を切って順に `git merge` する」形で、
    /// 48 ケースに **git を約 1,400 回**起こしていた (macOS 実測 130 秒)。
    /// CI の nextest は `slow-timeout 60s / terminate-after 1` なので、
    /// **ケースを増やした瞬間に殺される**。数を増やすために形を変えた:
    ///
    /// * 1 ケース = **1 ファイル**。ベースに `f000..` を並べ、枝は
    ///   自分の担当ファイルだけを書き換える
    /// * 枝は 5 本だけ作り、**全ペアを 1 回ずつ実際に `git merge`** する。
    ///   衝突したファイルは `git ls-files --unmerged` が**パス単位**で返すので、
    ///   1 回のマージが数百ケースぶんの答えを一度に出す
    /// * 行域は `proof()` = 実際の `git diff` から取る (production の経路)。
    ///   ケースごとの判定は、その行域をファイルで絞って `prove()` に通すだけ
    ///
    /// これで **git 起動は約 55 回**に落ちた (240 ケース)。
    ///
    /// # ペアで測ることが N 本の主張と同じである理由
    ///
    /// 証明の単位 ([`Clash`]) がそもそもペアである。そして先に入った枝は
    /// **自分の行域しか変えない**ので、`base + b1 + b2` へ `b3` を混ぜる
    /// 三方向マージは「`b3` 対 `b1`」「`b3` 対 `b2`」の判定に分解される
    /// (未変更行は誰も消さないので、間の未変更行は最後まで残る)。
    /// 全ペアが綺麗なら順に混ぜても綺麗、が成り立つ。N 本を実際に流す実測は
    /// [`tests::実gitで一撃統合が人手ゼロで通る`] が別に持っている。
    #[test]
    fn 実gitで証明の見逃しが1件も無いことを網羅的に確かめる() {
        let Some(r) = make_repo("prove") else { return };
        const FILES: usize = 240;
        const LINES: usize = 120;
        const BRANCHES: usize = 5;
        let path_of = |f: usize| format!("f{f:03}.txt");
        let base_lines = |f: usize| -> Vec<String> {
            (1..=LINES).map(|i| format!("f{f:03} line {i}")).collect()
        };

        // ── 生成 (決定的。git は 1 度も起こさない) ──────────────────
        let mut rng = Rng::new(20_260_810);
        // edits[file][branch] — **ファイルが外側**。1 ケース = 1 ファイルなので
        // 生成も書き出しもこの順で回る (枝で外側を回すと index が交差する)。
        let mut edits: Vec<Vec<Vec<Edit>>> = vec![vec![Vec::new(); BRANCHES]; FILES];
        for per_branch in edits.iter_mut() {
            // この案件に参加する枝を 2〜5 本選ぶ (重複なし)。
            let n = rng.range(2, BRANCHES);
            let mut pool: Vec<usize> = (0..BRANCHES).collect();
            let mut parts: Vec<usize> = Vec::new();
            for _ in 0..n {
                parts.push(pool.remove(rng.range(0, pool.len() - 1)));
            }
            parts.sort_unstable();
            // この案件で既に置いた行域。**わざと際どい間隔で置く**。
            let mut placed: Vec<(usize, usize)> = Vec::new();
            for &b in &parts {
                for _ in 0..rng.range(1, 2) {
                    let kind = match rng.range(0, 2) {
                        0 => Kind::Replace,
                        1 => Kind::Delete,
                        _ => Kind::Insert,
                    };
                    let len = rng.range(1, 3);
                    let e = Edit {
                        at: spot(&mut rng, &placed, len, LINES),
                        len,
                        kind,
                    };
                    let (lo, hi) = e.span();
                    // 同じ枝の中で重なると本文が壊れるので、そこだけ捨てる。
                    if per_branch[b].iter().any(|o| {
                        let (olo, ohi) = o.span();
                        lo <= ohi + o.len && olo <= hi + len
                    }) {
                        continue;
                    }
                    per_branch[b].push(e);
                    placed.push((lo, hi));
                }
            }
        }

        // ── 実 git ─────────────────────────────────────────────
        for f in 0..FILES {
            r.write(&path_of(f), &(base_lines(f).join("\n") + "\n"));
        }
        r.git(&["add", "--all"]);
        r.git(&["commit", "--quiet", "-m", "base"]);
        let base_oid = r.oid("base");

        let names: Vec<String> = (0..BRANCHES).map(|b| format!("b{b}")).collect();
        for (b, name) in names.iter().enumerate() {
            r.git(&["checkout", "--quiet", "-b", name, "base"]);
            for (f, per_branch) in edits.iter().enumerate() {
                if per_branch[b].is_empty() {
                    continue;
                }
                r.write(&path_of(f), &apply(&base_lines(f), &per_branch[b], name));
            }
            // 追跡済みファイルだけなので 1 起動で済む。
            r.git(&["commit", "--quiet", "--all", "-m", name]);
        }
        r.git(&["checkout", "--quiet", "base"]);

        // 行域は production の経路 (`git diff`) から取る。
        let p = proof(&r.0, "base", &names, region::MERGE_ONLY_BAND);
        assert_eq!(p.base, base_oid, "座標系は共通祖先で 1 つ");
        assert_eq!(p.branches.len(), BRANCHES, "全枝を読めた");
        assert!(
            p.branches.iter().all(|b| b.note.is_none() && b.whole == 0),
            "読めなかった枝がある: {:?}",
            p.branches.iter().map(|b| &b.note).collect::<Vec<_>>()
        );

        // 全ペアを実際に `git merge` して、衝突したパスを集める。
        let mut conflicted: BTreeSet<String> = BTreeSet::new();
        let mut pairs_run = 0usize;
        for i in 0..BRANCHES {
            for j in (i + 1)..BRANCHES {
                r.git(&["checkout", "--quiet", "--force", "-B", "mrg", &names[i]]);
                let msg = format!("merge {}", names[j]);
                let ok = r
                    .try_git(&["merge", "--no-edit", "-m", &msg, &names[j]])
                    .is_ok();
                let unmerged = r.git(&["ls-files", "--unmerged"]);
                for l in unmerged.lines() {
                    if let Some((_, path)) = l.split_once('\t') {
                        conflicted.insert(path.trim().to_string());
                    }
                }
                if !ok {
                    let _ = r.try_git(&["merge", "--abort"]);
                    assert!(
                        !unmerged.trim().is_empty(),
                        "マージは落ちたのに未解決パスが取れない ({} × {})",
                        names[i],
                        names[j]
                    );
                }
                pairs_run += 1;
            }
        }
        r.git(&["checkout", "--quiet", "--force", "base"]);
        assert_eq!(pairs_run, BRANCHES * (BRANCHES - 1) / 2, "全ペアを回した");

        // ── 突き合わせ (ここから先は git を 1 度も起こさない) ─────────
        let per_file = |path: &str| -> Vec<BranchRegions> {
            p.branches
                .iter()
                .filter_map(|b| {
                    let regions: Vec<Region> = b
                        .regions
                        .iter()
                        .filter(|x| x.path == path)
                        .cloned()
                        .collect();
                    (!regions.is_empty()).then(|| BranchRegions {
                        branch: b.branch.clone(),
                        regions,
                        whole: 0,
                        note: None,
                    })
                })
                .collect()
        };

        let (mut proven, mut proven_clean, mut miss) = (0usize, 0usize, 0usize);
        let (mut unproven, mut over, mut over_adjacent) = (0usize, 0usize, 0usize);
        // 帯 3 (SAFE_BAND) との比較。**同じケース列**を git を 1 回も足さずに測る。
        let (mut proven3, mut unproven3, mut over3, mut tight) = (0, 0, 0, 0usize);
        let mut ran = 0usize;

        for f in 0..FILES {
            let path = path_of(f);
            let parts = per_file(&path);
            if parts.len() < 2 {
                continue;
            }
            ran += 1;
            let pf = prove(parts.clone(), region::MERGE_ONLY_BAND);
            let pf3 = prove(parts, region::SAFE_BAND);
            let clean = !conflicted.contains(&path);

            if pf.disjoint {
                proven += 1;
                if clean {
                    proven_clean += 1;
                } else {
                    miss += 1;
                    panic!(
                        "見逃し {miss} 件目: 証明は立ったのに実 git が衝突した \
                         (file={path}, band={})\n{:#?}",
                        pf.band, pf.branches
                    );
                }
                if !pf3.disjoint {
                    tight += 1;
                }
            } else {
                unproven += 1;
                if clean {
                    over += 1;
                    // **内訳を取る。** 行が重なっていないのに安全帯だけで
                    // 止めた組は、原理的に「安全側へ倒したぶん」である。
                    if pf.pairs.iter().all(|c| c.reason == Reason::TooClose) {
                        over_adjacent += 1;
                    }
                }
            }
            if pf3.disjoint {
                proven3 += 1;
            } else {
                unproven3 += 1;
                if clean {
                    over3 += 1;
                }
            }
        }

        assert!(ran >= 150, "十分な数のケースを回した: {ran}");
        assert!(
            proven > 0,
            "証明が立つケースが 1 つも無いのは網羅になっていない"
        );
        assert!(
            unproven > 0,
            "止まるケースが 1 つも無いなら境界を踏んでいない"
        );
        assert_eq!(miss, 0, "**見逃しは 1 件も許さない**");
        assert!(
            tight > 0,
            "帯 3 では止まっていた組が 1 つも無いなら、帯を下げた意味が測れていない"
        );
        let rate = |o: usize, u: usize| {
            if u == 0 {
                0.0
            } else {
                o as f64 * 100.0 / u as f64
            }
        };
        let (over_rate, over_rate3) = (rate(over, unproven), rate(over3, unproven3));
        // 数字は `--nocapture` で読む (誇張しないために、悪い側も出す)。
        eprintln!(
            "[coedit] 実 git 網羅 (帯 {}): {ran} ケース / 証明が立った {proven} \
             (全部綺麗 {proven_clean} · 見逃し {miss}) / 立たなかった {unproven} / \
             そのうち実際は綺麗だった {over} = 過剰報告 {over_rate:.1}% \
             (うち {over_adjacent} 件は安全帯だけが理由)",
            region::MERGE_ONLY_BAND
        );
        eprintln!(
            "[coedit] 同じ {ran} ケースを帯 {} で測ると: 証明が立った {proven3} / \
             立たなかった {unproven3} / 過剰報告 {over_rate3:.1}% — \
             帯を 3→{} にして新たに立った証明は {tight} 件 (どれも実 git で綺麗)",
            region::SAFE_BAND,
            region::MERGE_ONLY_BAND
        );
    }

    #[test]
    fn 実gitで一撃統合が人手ゼロで通る() {
        let Some(r) = make_repo("integrate") else {
            return;
        };
        r.commit("shared.txt", &lines(200), "base");
        let before_base = r.oid("base");
        let mut names = Vec::new();
        for (i, at) in [10usize, 60, 120, 180].into_iter().enumerate() {
            let b = format!("w{i}");
            r.git(&["checkout", "--quiet", "-b", &b, "base"]);
            r.commit("shared.txt", &edited(200, at, 2, &b), &b);
            names.push(b);
        }
        r.git(&["checkout", "--quiet", "base"]);
        let before: Vec<String> = names.iter().map(|b| r.oid(b)).collect();

        let out = integrate(&r.0, "base", &names, &Opts::default()).expect("実行できる");
        if !out.dry_available {
            return; // git 2.38 未満: 証明だけへ降格する経路 (別テストで確認)
        }
        assert!(out.ok(), "止まった: {:?}", out.stop);
        assert!(out.proof.disjoint, "証明が立っている");
        assert_eq!(out.human_touches, 0, "**人手ゼロ**");
        assert_eq!(out.merged, names, "4 本すべてが入った");
        // 4 本ぶんの変更が全部載っている。
        let head = r.git(&["show", "base:shared.txt"]);
        for b in &names {
            assert!(head.contains(b.as_str()), "{b} の変更が入っている");
        }
        // ブランチ側の参照は 1 つも動いていない (rebase していない)。
        let after: Vec<String> = names.iter().map(|b| r.oid(b)).collect();
        assert_eq!(after, before, "ブランチは動かさない");
        assert_ne!(r.oid("base"), before_base, "統合先だけが進む");
        assert!(r.git(&["status", "--porcelain"]).trim().is_empty());
        eprintln!(
            "[coedit] 一撃統合: {} 本 / {} ms / 人手 {} 回",
            out.merged.len(),
            out.took_ms,
            out.human_touches
        );
    }

    #[test]
    fn 証明が立たなければ参照を1つも動かさない() {
        let Some(r) = make_repo("refuse") else { return };
        r.commit("shared.txt", &lines(100), "base");
        for (i, at) in [40usize, 41].into_iter().enumerate() {
            let b = format!("x{i}");
            r.git(&["checkout", "--quiet", "-b", &b, "base"]);
            r.commit("shared.txt", &edited(100, at, 1, &b), &b);
        }
        r.git(&["checkout", "--quiet", "base"]);
        let names = vec!["x0".to_string(), "x1".to_string()];
        let before = (r.oid("base"), r.oid("x0"), r.oid("x1"));

        let out = integrate(&r.0, "base", &names, &Opts::default()).expect("実行できる");
        assert!(!out.proof.disjoint, "同じ行なので立たない");
        assert!(out.merged.is_empty(), "1 本も入っていない");
        let stop = out.stop.as_ref().expect("止まる");
        assert!(stop.predicted, "参照を動かす前に止めた");
        assert!(out.restored, "開始時のまま");
        assert!(out.human_touches > 0, "人手が要ると言う");
        assert_eq!((r.oid("base"), r.oid("x0"), r.oid("x1")), before);
        assert!(r.git(&["status", "--porcelain"]).trim().is_empty());

        // 提案どおりに片方を手放せば立つ、という形になっている。
        assert!(!suggest(&out.proof).is_empty(), "手直しを提案する");
    }

    #[test]
    fn 統合先が進んでいたら統合先も参加者に入れる() {
        let Some(r) = make_repo("basemoved") else {
            return;
        };
        r.commit("shared.txt", &lines(100), "base");
        // 先に枝を切ってから、統合先を進める。
        r.git(&["checkout", "--quiet", "-b", "y0", "base"]);
        r.commit("shared.txt", &edited(100, 50, 2, "y0"), "y0");
        r.git(&["checkout", "--quiet", "base"]);
        r.commit("shared.txt", &edited(100, 50, 2, "moved"), "base moves");

        let p = proof(&r.0, "base", &["y0".to_string()], region::MERGE_ONLY_BAND);
        assert!(p.base_participates, "統合先も参加者に入る");
        assert!(!p.disjoint, "統合先とだけ衝突する抜け道を塞ぐ");
        assert!(p.pairs.iter().any(|c| c.a == "base" || c.b == "base"));
    }

    #[test]
    fn 上限を超えたら言い切らない() {
        let Some(r) = make_repo("cap") else { return };
        r.commit("shared.txt", &lines(60), "base");
        // 上限より 2 本多く作る (どれも別ファイルなので本来は互いに素)。
        let n = MAX_BRANCHES + 2;
        let mut names = Vec::new();
        for i in 0..n {
            let b = format!("z{i:02}");
            r.git(&["checkout", "--quiet", "-b", &b, "base"]);
            r.commit(&format!("solo{i}.txt"), "x\n", &b);
            r.git(&["checkout", "--quiet", "base"]);
            names.push(b);
        }
        let p = proof(&r.0, "base", &names, region::MERGE_ONLY_BAND);
        assert_eq!(p.skipped, 2, "切った本数を必ず返す");
        assert!(!p.disjoint, "見ていないものがあるなら言い切らない");
        assert!(p.note.is_some(), "無音で切らない");
    }
}
