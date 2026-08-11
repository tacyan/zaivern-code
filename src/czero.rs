//! 競合ゼロ点検 — 「**いま自分は本当に守られているのか**」に 1 画面で答える。
//!
//! ## なぜ要るのか
//!
//! この製品の売りは「並列でも競合しない」。ところが実際の防御は
//! 4 つの層にばらけていて、**ユーザーには自分がどの段に居るのかが分からない**:
//!
//! * リースの段は `Enforced / Advisory / Off` の 3 つある ([`crate::lease::Tier`])
//! * 強制が効くベンダーは、カタログ 30 種超のうち
//!   [`crate::agents::HOOK_TARGETS`] に載っている数種だけ
//! * フックは「設置済みだが未承認」だと**1 件も止まらない**
//!   ([`crate::supervisor::hooks::HookStatus::Inactive`])
//! * 衝突レーダーが見ているのは、Zaivern が作った worktree の
//!   いちばん大きな束 1 本だけ
//!
//! CLAUDE.md の設計原則 4 は「エージェントの状態を推測せず、**いま自分が
//! どの段にいるかを UI に出す**」と言っている。このモジュールはそれを
//! **防御そのもの**へ適用する。
//!
//! ## 4 本の鎖
//!
//! 防御は鎖なので、**いちばん弱い輪より強くならない**。だから 4 本を
//! 1 画面に並べ、それぞれに ✅ / ⚠ / ❌ と 1 行の理由と直すためのボタンを付ける:
//!
//! 1. **事前分割** — 稼働中の担当**行域**が安全帯を挟んで互いに素か ([`Chain::Split`])
//! 2. **実行中の強制** — 段 ＋ **1 体ずつ**の実態 ＋ メッシュの生存 ([`Chain::Enforce`])
//! 3. **統合** — いま統合したら**一撃で通るか** ([`Chain::Merge`])
//! 4. **共有面** — 検出できていない穴 ([`Chain::Blind`])
//!
//! ### 鎖 2 を丸めないことが、このモジュールの存在理由
//!
//! 「Enforced」と 1 つだけ出すのが**いまの嘘**である。claude は止まるが
//! cursor-agent は止まらない、という状態でも表示は「強制」になる。
//! [`agent_rows`] は稼働中の 1 体ずつに [`Gate`] を付けて出す。
//!
//! ## 行域を丸めないことが、鎖 1 の存在理由
//!
//! [`crate::region`] が入って、**同じファイルでも違う行なら 2 人が同時に
//! 持てる**ようになった。ところが「ファイル単位で重なっているか」を出す
//! ままだと、`src/app.rs#L1200-1260` と `src/app.rs#L4000-4100` が
//! **重なっていることにされる**。これは鎖 2 の「Enforced と 1 つだけ出す」
//! と同じ丸めであり、同じ理由で禁止する。
//!
//! だから鎖 1 は「守られている / いない」を 1 つ出すのではなく、
//! **どの域が守られていて、どの域が守られていないか**を [`Held`] の 1 件ずつと
//! [`TooClose`] の組で出す。行番号を並べても人は読めないので、ファイルを
//! 1 本の**帯** ([`band_layout`]) に潰して、誰の色がどこにあるかを見せる。
//! 隣り合う 2 つの域が [`crate::region::SAFE_BAND`] 行より近い場所だけが
//! 危険地帯で、そこには**あと何行空ければ安全か** ([`lines_needed`]) を出す。
//!
//! ## メッシュはファイルシステムから読む
//!
//! Erlang 風のプロセスメッシュ (`~/.zaivern/mesh/<スコープ>/`) は別の担当が
//! 同時に作っている。**`use` しない** — 登録ディレクトリを直に読み、
//! 鍵の名前は別名を全部見て、無ければ「未稼働」と 1 行で正直に出す
//! ([`read_mesh`])。相手が出来ていなくてもこの画面は 1 ピクセルも壊れない。
//!
//! ## 疎結合の約束
//!
//! 同時に別のブランチで作られている新規モジュール (mesh / coedit / guard /
//! train / union / split) へは**コンパイル時依存を 1 つも持たない**。
//! 状態はファイルシステムと git から直に検出し、証明器だけは
//! **実行時に**サブプロセスで繋ぐ ([`probe_proof`])。そうしておけば、
//! 相手が出来ていても居なくてもこの画面は動く。
//!
//! 例外は [`crate::region`] だけ。既にコミット済みで、表記
//! ([`crate::region::render`]) と重なり判定 ([`crate::region::conflicts`]) の
//! 契約が固定されている。**2 実装を持つとズレる**ので、ここでは再発明しない。
//!
//! ## 判定は純関数
//!
//! 検出 (I/O) と判定 (純関数) を分けてある。[`judge`] は [`Facts`] だけを見て
//! 4 行を返し、[`tests`] が全組合せをテーブルで固定する。**画面のロジックは
//! ここに全部あり、UI は並べるだけ。**

use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;

use crate::i18n::{tr, trf};

/// 走査の最短間隔。git を起こすので [`crate::conflict`] と同じ 8 秒から始め、
/// 実際の所要時間に応じて [`crate::git::scan_interval`] が自動で後退させる。
const SCAN_BASE: Duration = Duration::from_secs(8);

/// 監査ログから数える「宛先の判らない書き込み」の目印。
/// 書き手は `lease::gate` (`opaque-write <持ち主>`)。
const OPAQUE_MARK: &str = "opaque-write";

/// 監査ログを読む上限。壊れた・膨らんだログで UI を待たせない。
const AUDIT_READ_CAP: u64 = 512 * 1024;

/// 走査を諦める時間。ワーカーがここまで戻らなければ受け口を捨てて次を出す。
///
/// **捨てないと画面が永久に古いまま**になる (`pending` が埋まっている間は
/// 次の走査を出さないため)。捨てたワーカーは自分で終わって送信に失敗するだけ。
const SCAN_GIVEUP: Duration = Duration::from_secs(60);

/// メッシュから読む登録の上限。**64 本の行は情報ではなく壁。**
const MESH_PROC_CAP: usize = 64;

/// 1 つのメールボックスから数える未読の上限。
const MESH_INBOX_CAP: usize = 999;

/// 帯を出すファイルの上限。超えたぶんは件数だけ 1 行で出す。
const BAND_FILE_CAP: usize = 8;

// ═══════════════════════════════════════════════════════════════════════════
//  1. 判定の材料 (検出した生の事実だけ。ここに UI も I/O も無い)
// ═══════════════════════════════════════════════════════════════════════════

/// 1 体ぶんの「強制がどこまで効いているか」。
///
/// **[`Gate::Enforced`] 以外はすべて「止まらない」**。段を 1 つに畳むと
/// この差が消えるので、畳まずに持つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    /// 全イベントが入っていて、実際に発火する。
    Enforced,
    /// 設定ファイルには入っているが、ベンダー側で承認されていない。
    /// **書いてあるのに黙って飛ばされる** = 1 件も止まらない。
    Unapproved,
    /// 一部のイベントしか入っていない (別バージョンの残骸など)。
    Partial,
    /// 仕掛けられるのに、まだ設置していない。
    NotInstalled,
    /// このベンダーはフック機構そのものを持たない。**設置しても止まらない。**
    NoMechanism,
    /// カタログに無いコマンド。何ができるか判らない。
    Unknown,
}

impl Gate {
    /// UI に出す短い名前 (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            Gate::Enforced => "止まります",
            Gate::Unapproved => "止まりません (未承認)",
            Gate::Partial => "止まりません (一部だけ設置)",
            Gate::NotInstalled => "止まりません (未設置)",
            Gate::NoMechanism => "止まりません (フック機構が無い)",
            Gate::Unknown => "判りません (カタログに無い)",
        }
    }

    /// なぜそうなのかの 1 行。
    pub fn detail(self) -> &'static str {
        match self {
            Gate::Enforced => "他人が持つファイルへの書き込みは、このエージェントでは実際に拒否されます",
            Gate::Unapproved => "設定ファイルには入っていますが、ベンダー側で承認されていないので黙って飛ばされます",
            Gate::Partial => "一部のイベントしか入っていません。書き込みの直前を捕まえられない経路が残っています",
            Gate::NotInstalled => "このベンダーはフックを持てますが、まだ設置していません",
            Gate::NoMechanism => "このベンダーはフック機構を持たないので、設置する先がありません。担当パスを分けて守るしかありません",
            Gate::Unknown => "カタログに無いコマンドなので、フックを仕掛けられるかどうかも判りません",
        }
    }

    /// この 1 体の段。**[`Gate::Enforced`] だけが ✅。**
    pub fn grade(self) -> Grade {
        match self {
            Gate::Enforced => Grade::Ok,
            // 「入れたつもり」が最も危ない。未設置より上に置かない。
            Gate::Unapproved | Gate::NoMechanism | Gate::Unknown => Grade::Bad,
            Gate::Partial | Gate::NotInstalled => Grade::Warn,
        }
    }
}

/// 稼働中と分かっている 1 体。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AgentFact {
    /// 画面に出す名前 (台帳の持ち主表記 / タブのタイトル)。
    pub name: String,
    /// カタログ上の実行ファイル名。判らなければ空。
    pub bin: String,
    /// 台帳に持ち主として載っているか。載っていなければタブの記録から起こした
    /// = **いま動いている確証は無い**ので、画面でも区別して出す。
    pub holding: bool,
    /// 強制の実態。
    pub gate: Gate,
    /// 承認されていないときの直し方 (`hooks::ActivationGap::how`)。無ければ空。
    pub how: String,
}

/// 誰がどのファイルの**何行目**を持っているか、1 件ぶん。
///
/// 台帳のパターン 1 つが 1 件になる。同じ持ち主が 3 つの域を持っていれば
/// 3 件で、**まとめない** — まとめた瞬間に「このファイルは A のもの」という
/// ファイル単位の嘘に戻る。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Held {
    /// 持ち主の表示名 (`lease::Holder::display`)。
    pub owner: String,
    /// 担当の実体。[`crate::region::Region::is_whole`] ならファイル全体。
    pub region: crate::region::Region,
}

/// **唯一の危険地帯** — 別々の持ち主の域が、安全帯を挟んでもなお近すぎる組。
///
/// ここが空であることが、そのまま「一撃マージできる」証明になる
/// ([`crate::region::is_disjoint`] と同じ不変条件)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TooClose {
    /// 画面に出すファイル (両者は同じファイルを指している)。
    pub path: String,
    /// [`Facts::held`] の添字 (行の若い方)。
    pub lo: usize,
    /// [`Facts::held`] の添字 (行の深い方)。
    pub hi: usize,
    /// あと何行空ければ安全になるか。**0 は「行をずらしても解けない」**
    /// (ファイル全体・末尾まで・glob のどれか) を意味する。
    pub need: u32,
    /// **交錯**で挙げた組か (`need` は必ず 0)。
    ///
    /// 「近すぎる」と**同じ顔をさせない**ために分けてある。交錯は
    /// *離しても直らない* ので、「あと {k} 行空ければ通ります」という
    /// 案内をそのまま出すと嘘になる。詳細は
    /// [`crate::lease::interleave_ok`] / [`crate::region::anchor_lines`]。
    pub bracketed: bool,
}

/// 台帳のパターンを行域へ起こす (純粋・入力順を保つ)。
///
/// **壊れた指定は捨てずにファイル全体として扱う。** 捨てると「持ち主が
/// 居ない」ように見えてしまい、いちばん危ない側へ倒れる。
pub fn to_held(owned: &[(String, Vec<String>)]) -> Vec<Held> {
    let mut out = Vec::new();
    for (owner, pats) in owned {
        for spec in pats {
            let region =
                crate::region::parse(spec).unwrap_or_else(|_| crate::region::Region::whole(spec));
            out.push(Held {
                owner: owner.clone(),
                region,
            });
        }
    }
    out
}

/// 2 つの域を安全帯まで引き離すのに、**あと何行**必要か (純粋)。
///
/// 深い側を `need` 行だけ下へずらせば [`crate::region::spans_too_close`] が
/// `false` になる。既に離れていれば 0。
///
/// **片方が末尾まで伸びていると、どれだけずらしても解けない**ので 0 を返す。
/// 「0 行で足りる」ではなく「行では解けない」の意味なので、呼び出し側は
/// [`crate::region::conflicts`] と組にして読むこと。
pub fn lines_needed(a: &crate::region::Span, b: &crate::region::Span, band: u32) -> u32 {
    let (lo, hi) = if a.start <= b.start { (a, b) } else { (b, a) };
    if lo.end == crate::region::Span::EOF {
        return 0;
    }
    lo.end
        .saturating_add(band)
        .saturating_add(1)
        .saturating_sub(hi.start)
}

/// 別々の持ち主の間で、安全帯を挟んでもなお近すぎる組を全部出す (純粋・決定的)。
///
/// **同じ持ち主どうしは数えない** — 自分の域が 2 つ隣り合っていても、
/// 書くのは 1 人なので衝突しない (`count_overlaps` 時代からの不変)。
pub fn too_close_pairs(
    held: &[Held],
    band: u32,
    text_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<TooClose> {
    let mut out = Vec::new();
    // 帯で既に挙げた (持ち主, 持ち主, パス) を覚えて、交錯で二重に出さない。
    let mut already: std::collections::BTreeSet<(String, String, String)> =
        std::collections::BTreeSet::new();
    for i in 0..held.len() {
        for j in (i + 1)..held.len() {
            let (a, b) = (&held[i], &held[j]);
            if a.owner == b.owner || !crate::region::conflicts(&a.region, &b.region, band) {
                continue;
            }
            let (need, swap) = match (a.region.span, b.region.span) {
                (Some(x), Some(y)) => (lines_needed(&x, &y, band), y.start < x.start),
                _ => (0, false),
            };
            let (lo, hi) = if swap { (j, i) } else { (i, j) };
            already.insert(owner_pair(&a.owner, &b.owner, &held[lo].region.path));
            out.push(TooClose {
                path: held[lo].region.path.clone(),
                lo,
                hi,
                need,
                bracketed: false,
            });
        }
    }
    out.extend(bracketed_pairs(held, &already, text_of));
    out
}

/// 二重に出さないための鍵 (持ち主 2 人 + パス)。**必ず辞書順**。
fn owner_pair(a: &str, b: &str, path: &str) -> (String, String, String) {
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    (x.to_string(), y.to_string(), path.to_string())
}

/// **交錯**している組を出す (帯を全部通ったあとの 2 段目)。
///
/// 帯 ([`too_close_pairs`] の前半) は**組ごと**の判定で、それは今も正しい。
/// 足りないのは「全部の組が帯を満たす ⇒ まとめてマージしても綺麗に通る」と
/// いう推論のほうで、片方が相手を上下から挟んでいると反復的な本文では帯を
/// 何行取っても `git merge` が衝突する ([`crate::region::anchor_lines`] に実測)。
///
/// 交錯は「A の域が B の 2 つの域に挟まれている」という**集合の性質**なので、
/// 上の二重ループ (1 組ずつ) では定義できない。持ち主ごとに域をまとめてから
/// [`crate::lease::interleave_ok`] へ渡す。
///
/// `text_of` (錨の元になる本文) は**本当に交錯している持ち主の組が
/// あるときだけ**呼ばれる。互いに素な担当表 (この画面が普段映す形) では
/// 1 バイトも読まない。
fn bracketed_pairs(
    held: &[Held],
    already: &std::collections::BTreeSet<(String, String, String)>,
    text_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<TooClose> {
    // (持ち主, パス) ごとに「代表の添字」と「域の一覧」を集める。
    // `BTreeMap` だけを使うので並びは決定的。
    let mut by: std::collections::BTreeMap<(String, String), (usize, Vec<crate::region::Span>)> =
        std::collections::BTreeMap::new();
    for (i, h) in held.iter().enumerate() {
        let path = &h.region.path;
        if path.contains(['*', '?', '[']) {
            continue; // どのファイルを指すか確定しない (帯側が安全に扱う)
        }
        let Some(span) = h.region.span else {
            continue; // ファイル全体 — 帯側が必ず挙げている
        };
        let e = by
            .entry((h.owner.clone(), path.clone()))
            .or_insert_with(|| (i, Vec::new()));
        e.1.push(span);
    }
    let keys: Vec<&(String, String)> = by.keys().collect();
    let mut text: std::collections::BTreeMap<String, Option<String>> =
        std::collections::BTreeMap::new();
    let mut out = Vec::new();
    for x in 0..keys.len() {
        for y in (x + 1)..keys.len() {
            let (ka, kb) = (keys[x], keys[y]);
            if ka.0 == kb.0 || ka.1 != kb.1 {
                continue; // 同じ持ち主 / 別のファイル
            }
            let (ia, sa) = &by[ka];
            let (ib, sb) = &by[kb];
            if !crate::region::interleaved(sa, sb) {
                continue;
            }
            if already.contains(&owner_pair(&ka.0, &kb.0, &ka.1)) {
                continue; // 帯で既に挙げてある
            }
            let t = text
                .entry(ka.1.clone())
                .or_insert_with(|| text_of(&ka.1))
                .clone();
            if crate::lease::interleave_ok(t.as_deref(), sa, sb) {
                continue;
            }
            let (lo, hi) = if held[*ia].region.span.map_or(0, |s| s.start)
                <= held[*ib].region.span.map_or(0, |s| s.start)
            {
                (*ia, *ib)
            } else {
                (*ib, *ia)
            };
            out.push(TooClose {
                path: ka.1.clone(),
                lo,
                hi,
                // **0 は「行をずらしても解けない」の意味。** 交錯はまさに
                // それなので、既存の読み手 (画面 / doctor) が誤解しない。
                need: 0,
                bracketed: true,
            });
        }
    }
    out
}

/// 台帳に出てくる持ち主を、決定的な順に 1 回ずつ (純粋)。
///
/// 色の割り当てに使う。`HashMap` を通すと反復順が画面へ漏れて、
/// **同じ台帳なのに人によって色が違う**という再現しない絵になる。
pub fn owner_list(held: &[Held]) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for h in held {
        if !v.contains(&h.owner) {
            v.push(h.owner.clone());
        }
    }
    v.sort();
    v
}

/// 持ち主 → 色の枠 (純粋)。一覧に無ければ 0 番へ落とす (fail-soft)。
pub fn owner_slot(owners: &[String], name: &str) -> usize {
    owners.iter().position(|o| o == name).unwrap_or(0)
}

// ── メッシュ (Erlang 風のプロセス相互認識) ──────────────────────────

/// 生存の 3 値。**「判らない」を「死んでいる」へ丸めない。**
///
/// 丸めると、PID を書いていない登録が全部「詰まり」に見えて、
/// 唯一の詰まり (死んだのに担当を握ったまま) が埋もれる。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Live {
    /// PID が生きている。
    Yes,
    /// PID が死んでいる。
    No,
    /// 登録に PID が無い / 読めなかった。
    #[default]
    Unknown,
}

impl Live {
    /// 行頭に出す記号。
    pub fn glyph(self) -> &'static str {
        match self {
            Live::Yes => "●",
            Live::No => "✕",
            Live::Unknown => "◌",
        }
    }
}

/// メッシュに登録された 1 プロセス。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeshProc {
    /// 画面に出す名前。登録に無ければファイル名から起こす。
    pub name: String,
    /// 種別 (エージェント / エディタ / フック)。判らなければ空。
    pub kind: String,
    /// 生存確認に使う PID。0 = 登録に書かれていない。
    pub pid: u32,
    /// 生きているか。
    pub live: Live,
    /// 握っている担当の数。
    pub holds: usize,
    /// メールボックスに溜まっている未読の数。
    pub unread: usize,
}

/// メッシュ全体。**ディレクトリが無ければ [`Mesh::present`] が `false`。**
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Mesh {
    /// 登録ディレクトリが在るか。無いのは異常ではなく「まだ動いていない」。
    pub present: bool,
    /// 登録の一覧 (名前順・決定的)。
    pub procs: Vec<MeshProc>,
    /// 上限を超えて読まなかった件数。
    pub more: usize,
}

impl Mesh {
    /// **死んだのに担当を握ったままのプロセス** = 唯一の詰まり。
    ///
    /// これが 1 件でもあると、生きているエージェントはその担当が解けるまで
    /// 断られ続ける。「止まる」が「進まない」へ化ける唯一の形なので、
    /// 鎖 2 はここだけで ❌ へ落ちる。
    pub fn stuck(&self) -> usize {
        self.procs
            .iter()
            .filter(|p| p.live == Live::No && p.holds > 0)
            .count()
    }
}

/// 数 / 配列の長さ / 数字の文字列を、どれでも件数として読む (純粋)。
///
/// 相手の登録が「数」で持つか「一覧」で持つかは実装次第なので、両方受ける。
fn count_of(v: &serde_json::Value) -> Option<usize> {
    match v {
        serde_json::Value::Number(x) => x.as_u64().map(|n| n as usize),
        serde_json::Value::Array(a) => Some(a.len()),
        serde_json::Value::String(t) => t.parse().ok(),
        _ => None,
    }
}

/// メッシュの登録 1 件を読む (純粋)。
///
/// **相手のモジュールへコンパイル時依存を持たない**ぶん、鍵の名前は揺れうる。
/// よく使う別名を全部見て、無ければ空のまま通す (fail-soft: 読めない登録を
/// 「死んでいる」ことにしない — それは [`Live::Unknown`] の役目)。
pub fn read_proc(json: &str) -> Option<MeshProc> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    let o = v.as_object()?;
    let text = |keys: &[&str]| -> String {
        keys.iter()
            .find_map(|k| o.get(*k).and_then(|x| x.as_str()))
            .unwrap_or_default()
            .to_string()
    };
    let num = |keys: &[&str]| -> usize {
        keys.iter()
            .find_map(|k| o.get(*k).and_then(count_of))
            .unwrap_or(0)
    };
    Some(MeshProc {
        name: text(&["name", "id", "label", "agent", "holder"]),
        kind: text(&["kind", "role", "type"]),
        pid: num(&["pid", "process_id"]).min(u32::MAX as usize) as u32,
        live: Live::Unknown,
        holds: num(&["holds", "regions", "owns", "patterns", "leases"]),
        unread: num(&["unread", "pending", "queued"]),
    })
}

// ── 一撃マージの証明 ────────────────────────────────────────────────

/// `zai coedit proof --json` の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Proof {
    /// 一撃で通ることが証明できたか。
    pub ok: bool,
    /// 立たなかったときの、近すぎる組の数。
    pub pairs: usize,
    /// 証明器が自分で判定を下げた理由 (空なら下げていない)。
    pub note: String,
}

/// 証明器の出力を読む (純粋)。
///
/// **`ok` に相当する鍵が 1 つも無ければ `None`。** 読めなかったものを
/// 「証明できた」へ丸めるのが、この画面でいちばんやってはいけない嘘。
pub fn read_proof(json: &str) -> Option<Proof> {
    let v: serde_json::Value = serde_json::from_str(json.trim()).ok()?;
    let o = v.as_object()?;
    let ok = ["ok", "proven", "oneshot", "clean", "disjoint"]
        .iter()
        .find_map(|k| o.get(*k).and_then(|x| x.as_bool()))?;
    let pairs = ["pairs", "conflicts", "clashes", "overlaps"]
        .iter()
        .find_map(|k| o.get(*k).and_then(count_of))
        .unwrap_or(0);
    let note = ["note", "reason", "why", "degraded"]
        .iter()
        .find_map(|k| o.get(*k).and_then(|x| x.as_str()))
        .unwrap_or_default()
        .to_string();
    Some(Proof { ok, pairs, note })
}

/// 検出した生の事実。**[`judge`] はこれだけを見る。**
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    // ── 鎖 1: 事前分割 ──────────────────────────────────────────────
    /// このワークスペースで台帳が有効か。
    pub ledger_on: bool,
    /// 担当行域を 1 つ以上持っている持ち主の数。
    pub owners: usize,
    /// 誰がどのファイルの何行目を持っているか。**1 件ずつ持つ** —
    /// 帯の絵も危険地帯の判定も、ここからだけ作る。
    pub held: Vec<Held>,
    /// 安全帯を挟んでもなお近すぎる組 = **唯一の危険地帯**。
    /// 空であることが、そのまま「一撃で通る」の証明になる。
    pub clashes: Vec<TooClose>,

    // ── 鎖 2: 実行中の強制 ──────────────────────────────────────────
    /// リースの段。[`crate::lease::Tier::Off`] なら、フックが入っていても
    /// `gate()` は素通りするので**何も止まらない**。
    pub tier: crate::lease::Tier,
    /// 稼働中と分かっている 1 体ずつ。
    pub agents: Vec<AgentFact>,
    /// Erlang 風メッシュの生存 (登録ディレクトリを直に読んだもの)。
    pub mesh: Mesh,

    // ── 鎖 3: 統合 ──────────────────────────────────────────────────
    /// 同じリポジトリにぶら下がっている作業ツリーの本数 (本体を含む)。
    pub trees: usize,
    /// 統合の走査が 1 度でも終わったか。終わっていなければ「判らない」。
    pub merge_scanned: bool,
    /// いま統合すると衝突するファイル数 ([`crate::conflict::Report::alarm_files`])。
    pub alarm_files: usize,
    /// 走査が判定を下げた理由 ([`crate::conflict::Report::note`])。
    pub merge_note: Option<String>,
    /// 未コミットの変更を抱えたツリーの本数。あると merge-tree の判定は
    /// 権威にならないので、衝突ゼロでも言い切らない。
    pub dirty_trees: usize,
    /// 一撃マージの証明。`None` = **証明器がまだ無い**ので、git の
    /// 突き合わせへ降格していることを表す (「証明が立った」とは言わない)。
    pub proof: Option<Proof>,

    // ── 鎖 4: 共有面 ────────────────────────────────────────────────
    /// 監査ログに残った「宛先の判らない書き込み」の件数。
    pub opaque_writes: usize,
    /// このエディタ自身の保存が台帳を通っているか ([`crate::lease::armed`])。
    pub editor_guard: bool,
    /// 監査ログの大きさ。**0 なら「ログの場所」ボタンを出さない** —
    /// 空のファイルの場所を渡すボタンは、押しても何も起きないのと同じ。
    pub audit_bytes: u64,
}

/// **原理的に検出できない穴**。数と中身を画面に出すためにデータで持つ。
///
/// ここを「無い」ことにするのが、この製品でいちばんやってはいけない嘘。
/// 鎖 4 が ✅ になることは**決してない**のはこのため。
pub struct BlindSpot {
    /// 穴の名前 (日本語原文)。
    pub what: &'static str,
    /// なぜ検出できないか (日本語原文)。
    pub why: &'static str,
}

/// 検出できない穴の一覧。[`judge`] が件数を数え、UI がそのまま列挙する。
pub const BLIND_SPOTS: &[BlindSpot] = &[
    BlindSpot {
        what: "宛先の判らないシェル書き込み",
        why: "eval や変数展開・ヒアドキュメント越しに書かれると、どのファイルが相手なのかフックの時点で判りません。監査ログには残しますが、止めてはいません",
    },
    BlindSpot {
        what: "エディタ外からの書き込み",
        why: "別のエディタや、あなたが手で打った置換コマンドは Zaivern を通らないので、台帳に 1 行も残りません",
    },
    BlindSpot {
        what: "意味的な衝突",
        why: "別々のファイルを触っても、片方が API を変えてもう片方が古い呼び方のままなら壊れます。テキストとしては衝突しないので、この画面には出ません",
    },
];

// ═══════════════════════════════════════════════════════════════════════════
//  2. 判定 (純関数 — この機能の本体)
// ═══════════════════════════════════════════════════════════════════════════

/// 1 本の鎖の段。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Grade {
    /// 守られている。
    Ok,
    /// 判らない / 完全ではない。
    Warn,
    /// 守られていない。
    Bad,
}

impl Grade {
    /// 行頭に出す記号。
    pub fn glyph(self) -> &'static str {
        match self {
            Grade::Ok => "✅",
            Grade::Warn => "⚠",
            Grade::Bad => "❌",
        }
    }

    /// 段に対応する色。
    ///
    /// 「成功」は egui の `Visuals` に相当する色が無い
    /// ([`crate::lease::Tier::color`] と同じ事情) ので、明暗 2 通りをここで持つ。
    pub fn color(self, v: &egui::Visuals) -> egui::Color32 {
        match self {
            Grade::Ok => crate::lease::Tier::Enforced.color(v),
            Grade::Warn => v.warn_fg_color,
            Grade::Bad => v.error_fg_color,
        }
    }
}

/// 4 本の鎖。行の並びはこの順で固定する (弱い輪を後ろへ隠さない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chain {
    /// 配る前に担当を分ける。
    Split,
    /// 走っている間に書き込みを止める。
    Enforce,
    /// 統合するときに衝突しない。
    Merge,
    /// どうしても残る穴。
    Blind,
}

impl Chain {
    /// 行の見出し (日本語原文)。
    pub fn title(self) -> &'static str {
        match self {
            Chain::Split => "① 事前分割",
            Chain::Enforce => "② 実行中の強制",
            Chain::Merge => "③ 統合",
            Chain::Blind => "④ 共有面",
        }
    }
}

/// 1 行の理由。**翻訳前**の原文と差し込み値で持つ。
///
/// [`judge`] が [`trf`] を呼んでしまうと、判定が読み込み済みの辞書に依存して
/// テストが環境で揺れる。原文のまま返し、[`Reason::text`] で表示直前に訳す。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reason {
    /// 日本語の原文 (`{名前}` が差し込み口)。
    pub template: &'static str,
    /// 差し込み値。
    pub args: Vec<(&'static str, String)>,
}

impl Reason {
    fn plain(template: &'static str) -> Self {
        Reason {
            template,
            args: Vec::new(),
        }
    }

    fn with(template: &'static str, args: Vec<(&'static str, String)>) -> Self {
        Reason { template, args }
    }

    /// 表示用の 1 行 (ここで初めて訳す)。
    pub fn text(&self) -> String {
        trf(self.template, &self.args)
    }
}

/// 「直す」ボタンの行き先。**この機能の中で完結する操作だけ**を持つ。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fix {
    /// ファイル所有の一覧を開く。
    Lease,
    /// 稼働中のエージェントへフックを設置する。
    InstallHooks,
    /// 衝突レーダーを開く。
    Radar,
    /// 監査ログの場所をコピーする。
    Audit,
}

impl Fix {
    /// ボタンの文言 (日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            Fix::Lease => "所有を見る",
            Fix::InstallHooks => "フックを設置",
            Fix::Radar => "レーダー",
            Fix::Audit => "ログの場所",
        }
    }

    /// 押すと何が起きるかの 1 行 (日本語原文)。
    ///
    /// **設定ファイルを書き換える操作は、押す前にそう言う。**
    /// 承認をネイティブ UI で取る、という約束の最小形。
    pub fn hint(self) -> &'static str {
        match self {
            Fix::Lease => "誰がどのファイルを持っているかの一覧を開きます",
            Fix::InstallHooks => "稼働中のエージェントの設定ファイルへフックを書き足します (既存の設定は消しません。バックアップを残します)",
            Fix::Radar => "作業ツリー同士を突き合わせた衝突の行列を開きます",
            Fix::Audit => "監査ログの場所をクリップボードへコピーします",
        }
    }

    /// 狭いときのアイコン 1 個 (文字が入らない幅でも押せるように)。
    pub fn icon(self) -> &'static str {
        match self {
            Fix::Lease => "🔐",
            Fix::InstallHooks => "🪝",
            Fix::Radar => "🛰",
            Fix::Audit => "📄",
        }
    }
}

/// 判定結果 1 行。
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub chain: Chain,
    pub grade: Grade,
    pub reason: Reason,
    pub fix: Option<Fix>,
}

/// **4 本の鎖を判定する (純粋)。** この機能のロジックは全部ここにある。
pub fn judge(f: &Facts) -> [Row; 4] {
    [split_row(f), enforce_row(f), merge_row(f), blind_row(f)]
}

/// 鎖全体の段 = **いちばん弱い輪**。鎖は最弱の輪より強くならない。
pub fn weakest(rows: &[Row; 4]) -> Grade {
    rows.iter().map(|r| r.grade).max().unwrap_or(Grade::Warn)
}

/// 鎖 1 — 配る前に担当**行域**が分かれているか。
///
/// ファイル単位ではなく行域で見る。`src/app.rs#L1200-1260` と
/// `src/app.rs#L4000-4100` は同じファイルだが**同時に持ってよい** —
/// ここを丸めると、行域オーナーシップの価値がそのまま画面から消える。
fn split_row(f: &Facts) -> Row {
    let band = crate::region::SAFE_BAND.to_string();
    let (grade, reason, fix) = if !f.ledger_on {
        (
            Grade::Warn,
            Reason::plain("台帳が無効なので、担当行域が近すぎるかどうかを判定できません"),
            Some(Fix::Lease),
        )
    } else if !f.clashes.is_empty() {
        let need = f.clashes.iter().map(|c| c.need).max().unwrap_or(0);
        let n = f.clashes.len().to_string();
        if need > 0 {
            (
                Grade::Bad,
                Reason::with(
                    "{n} 組の行域が安全帯 {b} 行より近すぎます — 最大であと {k} 行空ければ、すべて一撃で通ります",
                    vec![("n", n), ("b", band), ("k", need.to_string())],
                ),
                Some(Fix::Lease),
            )
        } else if f.clashes.iter().all(|c| c.bracketed) {
            // **交錯を「丸ごと重なっている」と言わない。** 重なってはいない —
            // 片方が相手を挟んでいるだけで、離しても直らない別の形である。
            (
                Grade::Bad,
                Reason::with(
                    "{n} 組の担当が交錯しています — 片方が相手の行域を上下から挟んでいて、間に手がかりの行がありません。離しても直らないので、連続した 1 本の行域にするか担当を分けてください",
                    vec![("n", n)],
                ),
                Some(Fix::Lease),
            )
        } else {
            (
                Grade::Bad,
                Reason::with(
                    "{n} 組の担当が丸ごと重なっています — 行をずらしても解けないので、担当そのものを分ける必要があります",
                    vec![("n", n)],
                ),
                Some(Fix::Lease),
            )
        }
    } else if f.owners >= 2 {
        (
            Grade::Ok,
            Reason::with(
                "{n} 人が持つ {r} 個の行域は、安全帯 {b} 行を挟んで互いに素です。同じファイルでも行が違えば同時に書けます",
                vec![
                    ("n", f.owners.to_string()),
                    ("r", f.held.len().to_string()),
                    ("b", band),
                ],
            ),
            None,
        )
    } else if f.owners == 1 {
        (
            Grade::Ok,
            Reason::plain("担当は 1 人だけなので、重なりようがありません"),
            None,
        )
    } else {
        (
            Grade::Warn,
            Reason::plain(
                "まだ誰も担当行域を確保していません (エージェントが書き込むと自動で登録されます)",
            ),
            Some(Fix::Lease),
        )
    };
    Row {
        chain: Chain::Split,
        grade,
        reason,
        fix,
    }
}

/// 鎖 2 — 走っている間に本当に止まるか。**1 体ずつの実態から起こす。**
fn enforce_row(f: &Facts) -> Row {
    let total = f.agents.len();
    let bad = f
        .agents
        .iter()
        .filter(|a| a.gate.grade() == Grade::Bad)
        .count();
    let warn = f
        .agents
        .iter()
        .filter(|a| a.gate.grade() == Grade::Warn)
        .count();
    // **設置できる相手が 1 体も居なければ「フックを設置」は出さない。**
    // フック機構を持たないベンダーや未承認のフックには設置しても効かないので、
    // 残る手は担当パスを分けること = 台帳を見せる方へ倒す。
    let hook_fix = if f
        .agents
        .iter()
        .any(|a| matches!(a.gate, Gate::NotInstalled | Gate::Partial))
    {
        Fix::InstallHooks
    } else {
        Fix::Lease
    };

    // **台帳が無効なら、フックが全部入っていても 1 件も止まらない** —
    // `lease::gate` は台帳が無効なワークスペースを素通りさせるため。
    // ここを先に見ないと「強制」と出しながら何も止まらない状態になる。
    let (grade, reason, fix) = if f.tier == crate::lease::Tier::Off {
        (
            Grade::Bad,
            Reason::plain(
                "リースが無効なので、フックを設置してあっても書き込みは 1 件も止まりません",
            ),
            Some(Fix::Lease),
        )
    } else if f.mesh.stuck() > 0 {
        // **死んだのに担当を握ったままのプロセスは、唯一の詰まり。**
        // フックが全部入っていても、生きているエージェントはこの担当が
        // 解けるまで断られ続ける。「止まる」が「進まない」へ化ける形なので、
        // 1 体ずつの内訳より先に見る。
        (
            Grade::Bad,
            Reason::with(
                "メッシュに、死んだのに担当を握ったままのプロセスが {n} 件あります — 生きているエージェントは、この担当が解けるまで断られ続けます",
                vec![("n", f.mesh.stuck().to_string())],
            ),
            Some(Fix::Lease),
        )
    } else if total == 0 {
        // **設置する相手が居ないので「フックを設置」は出さない** (押しても
        // 何も起きないボタンは、機能が有ることの嘘になる)。台帳を見せる。
        (
            Grade::Warn,
            Reason::plain("稼働中のエージェントが見つからないので、1 体ずつの判定ができません"),
            Some(Fix::Lease),
        )
    } else if bad > 0 {
        (
            Grade::Bad,
            Reason::with(
                "{total} 体のうち {bad} 体は書き込みが止まりません",
                vec![("total", total.to_string()), ("bad", bad.to_string())],
            ),
            Some(hook_fix),
        )
    } else if warn > 0 {
        (
            Grade::Warn,
            Reason::with(
                "{total} 体のうち {warn} 体はフックが完全ではありません",
                vec![("total", total.to_string()), ("warn", warn.to_string())],
            ),
            Some(hook_fix),
        )
    } else if f.tier == crate::lease::Tier::Advisory {
        (
            Grade::Warn,
            Reason::plain("所有は記録していますが、ブロックはしていません"),
            Some(hook_fix),
        )
    } else {
        (
            Grade::Ok,
            Reason::with(
                "稼働中の {total} 体すべてで、他人のファイルへの書き込みは拒否されます",
                vec![("total", total.to_string())],
            ),
            None,
        )
    };
    Row {
        chain: Chain::Enforce,
        grade,
        reason,
        fix,
    }
}

/// 鎖 3 — いま統合したら**一撃で通るか**。
///
/// 権威は証明器 ([`Proof`])。居なければ git の突き合わせへ**降格する**が、
/// そのときは「証明が立った」とは決して言わない (言えるのは
/// 「いま突き合わせた限り衝突しない」まで)。
fn merge_row(f: &Facts) -> Row {
    let n = f.trees.to_string();
    let (grade, reason) = if f.trees < 2 {
        (
            Grade::Ok,
            Reason::with(
                "作業ツリーは {n} 本だけ。統合で突き合わせる相手が居ません",
                vec![("n", n)],
            ),
        )
    } else if let Some(p) = &f.proof {
        if !p.note.is_empty() {
            (
                Grade::Warn,
                Reason::with(
                    "証明器は判定を下げました: {note}",
                    vec![("note", p.note.clone())],
                ),
            )
        } else if p.ok {
            (
                Grade::Ok,
                Reason::with(
                    "{n} 本は、いま統合すれば一撃で通ります (行域が安全帯を挟んで互いに素であることを証明できました)",
                    vec![("n", n)],
                ),
            )
        } else {
            (
                Grade::Bad,
                Reason::with(
                    "{k} 組の行域が近すぎるので、いま統合しても一撃では通りません",
                    vec![("k", p.pairs.to_string())],
                ),
            )
        }
    } else if !f.merge_scanned {
        (
            Grade::Warn,
            Reason::with(
                "{n} 本の作業ツリーを調べています (結果が出るまでは判りません)",
                vec![("n", n)],
            ),
        )
    } else if let Some(note) = &f.merge_note {
        (
            Grade::Warn,
            Reason::with(
                "{n} 本を調べましたが、判定を下げました: {note}",
                vec![("n", n), ("note", note.clone())],
            ),
        )
    } else if f.alarm_files > 0 {
        (
            Grade::Bad,
            Reason::with(
                "{n} 本を突き合わせると、{k} 個のファイルが衝突します",
                vec![("n", n), ("k", f.alarm_files.to_string())],
            ),
        )
    } else if f.dirty_trees > 0 {
        (
            Grade::Warn,
            Reason::with(
                "いまは衝突しませんが、{d} 本に未コミットの変更があるので判定は確定ではありません",
                vec![("d", f.dirty_trees.to_string())],
            ),
        )
    } else {
        (
            Grade::Ok,
            Reason::with(
                "{n} 本は、git で突き合わせた限り衝突しません (証明器がまだ無いので代用しています)",
                vec![("n", n)],
            ),
        )
    };
    Row {
        chain: Chain::Merge,
        grade,
        reason,
        // 中身を見るのはレーダー。**1 本しか無いときは開いても空**なので出さない。
        fix: (f.trees >= 2).then_some(Fix::Radar),
    }
}

/// 鎖 4 — 検出できていない穴。**✅ にはならない。**
fn blind_row(f: &Facts) -> Row {
    let holes = BLIND_SPOTS.len().to_string();
    let (grade, reason) = if f.opaque_writes > 0 {
        (
            Grade::Bad,
            Reason::with(
                "宛先の判らない書き込みが {c} 件ありました。ほかに {n} 個、原理的に検出できない穴があります",
                vec![("c", f.opaque_writes.to_string()), ("n", holes)],
            ),
        )
    } else if !f.editor_guard {
        (
            Grade::Warn,
            Reason::with(
                "このエディタ自身の保存が台帳を通っていません。ほかに {n} 個、原理的に検出できない穴があります",
                vec![("n", holes)],
            ),
        )
    } else {
        (
            Grade::Warn,
            Reason::with(
                "{n} 個の穴は原理的に検出できません。下に全部挙げます",
                vec![("n", holes)],
            ),
        )
    };
    Row {
        chain: Chain::Blind,
        grade,
        reason,
        // **ログが空ならボタンごと出さない。** 何も書かれていないファイルの
        // 場所を渡すボタンは、押しても何も起きないのと同じ。
        fix: (f.audit_bytes > 0).then_some(Fix::Audit),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. レイアウト (純関数 — 極端な寸法でテーブルテストする)
// ═══════════════════════════════════════════════════════════════════════════

/// この幅を下回ったらボタンをアイコンだけへ縮退させる。
const COMPACT_WIDTH: f32 = 460.0;

/// 列の隙間。
const GAP: f32 = 8.0;

/// 記号の列幅。
const GLYPH_W: f32 = 22.0;

/// 見出し列の下限。
const TITLE_MIN: f32 = 84.0;

/// 1 行ぶんの矩形。**どの幅でも見切れないこと**を関数で保証する。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub glyph: egui::Rect,
    pub title: egui::Rect,
    pub reason: egui::Rect,
    pub fix: egui::Rect,
}

impl RowLayout {
    /// 左から右への並び (テストで走査するため)。
    pub fn columns(&self) -> [egui::Rect; 4] {
        [self.glyph, self.title, self.reason, self.fix]
    }
}

/// 幅が狭いときはボタンをアイコンだけへ縮退させる。
pub fn is_compact(width: f32) -> bool {
    width < COMPACT_WIDTH
}

/// 行のレイアウト。
///
/// 決め方:
/// * 記号は固定
/// * ボタンは固定 (狭いときはアイコン 1 個ぶん)
/// * 見出しは「最長の見出し幅」と「可用幅の 28%」の小さい方、下限あり
/// * 理由が残り全部を取る (**必ず 0 以上**に切り詰める)
///
/// 隙間は**極端に狭いときだけ 0 へ落とす** — 落とさないと、隙間の合計が
/// 可用幅を超えて右端が領域からはみ出す。
pub fn row_layout(avail: egui::Rect, longest_title: f32) -> RowLayout {
    let w = avail.width().max(0.0);
    let gap = if w < GAP * 3.0 + GLYPH_W + TITLE_MIN {
        0.0
    } else {
        GAP
    };
    let gaps = gap * 3.0;
    let glyph = GLYPH_W.min(w);
    let rest = (w - glyph - gaps).max(0.0);
    let fix = if is_compact(w) { 30.0f32 } else { 96.0f32 }.min(rest);
    let rest = rest - fix;
    let title = longest_title
        .clamp(TITLE_MIN, (w * 0.28).max(TITLE_MIN))
        .min(rest);
    let reason = rest - title;

    let y = avail.y_range();
    let mut x = avail.left();
    let mut col = |width: f32| {
        let r = egui::Rect::from_x_y_ranges(x..=(x + width), y);
        x += width + gap;
        r
    };
    RowLayout {
        glyph: col(glyph),
        title: col(title),
        reason: col(reason),
        fix: col(fix),
    }
}

/// 空状態のカード。**利用可能領域の中央**に 1 枚 (下や上に取り残さない)。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let w = (avail.width() * 0.72).clamp(0.0, 420.0).min(avail.width());
    let h = 120.0f32.min(avail.height());
    egui::Rect::from_center_size(avail.center(), egui::vec2(w, h))
}

// ── 帯 (ミニマップ) — 誰がどの行を持っているかを 1 本に潰す ─────────

/// 1 行だけの域でも、これだけは見えるようにする幅。
///
/// **見えない所有は無いのと同じ。** 1 行の域が 0 ピクセルになると、
/// 「誰も持っていない」という絵になってしまう。
const BAND_MIN_W: f32 = 3.0;

/// 帯の高さ。
const BAND_H: f32 = 10.0;

/// 帯へ並べる域 1 件ぶんの入力。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandItem {
    /// [`Facts::held`] の添字。
    pub held: usize,
    /// 行域。`None` はファイル全体 (帯を丸ごと埋める)。
    pub span: Option<crate::region::Span>,
    /// 近すぎる相手が居る域か。
    pub danger: bool,
}

/// 帯の中の 1 区画。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandSeg {
    pub rect: egui::Rect,
    /// [`Facts::held`] の添字。
    pub held: usize,
    /// 危険地帯の当事者か (枠を付けて出す)。
    pub danger: bool,
}

/// 帯の目盛 = このファイルで見えている最大行 (純粋)。
///
/// ファイルの実際の行数は読まない (走査で全ファイルを開くと UI が待つ)。
/// **見えている域の最大行を 1 とする**ので、帯は「持っている範囲の相対図」
/// になる。行の絶対位置ではなく**隣との距離**が読めればよいので、これで足りる。
pub fn band_scale(items: &[BandItem]) -> u32 {
    let mut max = 1u32;
    for it in items {
        let e = match it.span {
            None => 1,
            Some(sp) if sp.end == crate::region::Span::EOF => sp.start,
            Some(sp) => sp.end,
        };
        max = max.max(e);
    }
    max
}

/// 同じファイルの行域を 1 本の帯へ並べる (純粋)。
///
/// x が行番号に写る。保証するのは 2 つだけで、どちらもテーブルテストで固定する:
///
/// * **`avail` から 1 ピクセルもはみ出さない** (どの幅でも見切れない)
/// * **区画どうしが 1 ピクセルも重ならない** — 重ねると「2 人が同じ行を
///   持っている」という嘘の絵になる
///
/// 幅が尽きたら**描かない**。無理に押し込むと、上の 2 つが同時には守れない。
pub fn band_layout(avail: egui::Rect, items: &[BandItem], scale: u32) -> Vec<BandSeg> {
    let scale = scale.max(1);
    let w = avail.width().max(0.0);
    let x_of = |line: u32| -> f32 {
        let t = (line.saturating_sub(1) as f32 / scale as f32).clamp(0.0, 1.0);
        avail.left() + w * t
    };
    // 先頭行の昇順 (同着は添字順) — 出力順を決定的にする。
    let mut order: Vec<&BandItem> = items.iter().collect();
    order.sort_by_key(|it| (it.span.map(|sp| sp.start).unwrap_or(1), it.held));

    let mut out = Vec::with_capacity(order.len());
    let mut cursor = avail.left();
    for it in order {
        let (from, to) = match it.span {
            None => (1, scale),
            Some(sp) if sp.end == crate::region::Span::EOF => (sp.start, scale),
            Some(sp) => (sp.start, sp.end),
        };
        let left = x_of(from).max(cursor);
        if left >= avail.right() {
            continue;
        }
        let right = x_of(to.saturating_add(1))
            .max(left + BAND_MIN_W)
            .min(avail.right());
        out.push(BandSeg {
            rect: egui::Rect::from_x_y_ranges(left..=right, avail.y_range()),
            held: it.held,
            danger: it.danger,
        });
        cursor = right;
    }
    out
}

/// 1 ファイルぶんの帯。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileBand {
    pub path: String,
    /// [`Facts::held`] の添字 (先頭行の昇順・同着は添字順)。
    pub items: Vec<usize>,
    /// この帯に含まれる危険地帯の数。
    pub danger: usize,
}

/// 帯を出すファイルを選ぶ (純粋・決定的)。
///
/// 並びは **危険地帯がある順 → 持ち主が多い順 → パス順**。
/// 上限を超えたぶんは画面に出さず件数だけ返す — **50 本の帯は情報ではなく壁**で、
/// 危険地帯が壁の中に埋もれたら、この画面は目的を果たしていない。
pub fn band_files(held: &[Held], clashes: &[TooClose], cap: usize) -> (Vec<FileBand>, usize) {
    // パスごとにまとめる (`HashMap` は使わない — 反復順が画面へ漏れる)。
    let mut groups: Vec<FileBand> = Vec::new();
    for (i, h) in held.iter().enumerate() {
        match groups.iter_mut().find(|g| g.path == h.region.path) {
            Some(g) => g.items.push(i),
            None => groups.push(FileBand {
                path: h.region.path.clone(),
                items: vec![i],
                danger: 0,
            }),
        }
    }
    for g in groups.iter_mut() {
        g.items
            .sort_by_key(|&i| (held[i].region.span.map(|s| s.start).unwrap_or(1), i));
        g.danger = clashes
            .iter()
            .filter(|c| g.items.contains(&c.lo) || g.items.contains(&c.hi))
            .count();
    }
    groups.sort_by(|a, b| {
        b.danger
            .cmp(&a.danger)
            .then(b.items.len().cmp(&a.items.len()))
            .then(a.path.cmp(&b.path))
    });
    let more = groups.len().saturating_sub(cap);
    groups.truncate(cap);
    (groups, more)
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 検出 (I/O — 必ず裏スレッド)
// ═══════════════════════════════════════════════════════════════════════════

/// `git worktree list --porcelain` の 1 本ぶん。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRef {
    pub dir: PathBuf,
    /// ブランチ名 (detached なら空)。**表示のためだけ**に持つ。
    pub branch: String,
}

/// `git worktree list --porcelain` を読む (純粋)。
///
/// **`prunable` が付いた行は捨てる** — 既に消えたフォルダを本数に数えると、
/// 「2 本あるのに 1 本しか無い」という嘘になる。
/// 改行は正規化してから見る (Windows のチェックアウトは CRLF)。
pub fn parse_worktrees(porcelain: &str) -> Vec<TreeRef> {
    let text = porcelain.replace("\r\n", "\n");
    let mut out: Vec<TreeRef> = Vec::new();
    let mut cur: Option<TreeRef> = None;
    let mut prunable = false;
    let flush = |cur: &mut Option<TreeRef>, prunable: &mut bool, out: &mut Vec<TreeRef>| {
        if let Some(t) = cur.take() {
            if !*prunable {
                out.push(t);
            }
        }
        *prunable = false;
    };
    for line in text.lines() {
        if let Some(p) = line.strip_prefix("worktree ") {
            flush(&mut cur, &mut prunable, &mut out);
            cur = Some(TreeRef {
                dir: PathBuf::from(p.trim()),
                branch: String::new(),
            });
        } else if let Some(b) = line.strip_prefix("branch ") {
            if let Some(t) = cur.as_mut() {
                // `refs/heads/foo` → `foo`。接頭辞が無い形もそのまま通す。
                t.branch = b
                    .trim()
                    .rsplit_once("refs/heads/")
                    .map(|(_, s)| s.to_string())
                    .unwrap_or_else(|| b.trim().to_string());
            }
        } else if line.trim() == "prunable" || line.starts_with("prunable ") {
            prunable = true;
        }
    }
    flush(&mut cur, &mut prunable, &mut out);
    out
}

/// 監査ログから「宛先の判らない書き込み」を数える (純粋)。
pub fn count_opaque(log: &str) -> usize {
    log.replace("\r\n", "\n")
        .lines()
        .filter(|l| l.contains(OPAQUE_MARK))
        .count()
}

/// 1 体ぶんの [`Gate`] を実際に調べる (I/O)。
fn gate_of(bin: &str, tree: &Path, exe: &Path) -> (Gate, String) {
    let Some(plan) = crate::supervisor::hooks::plan_for(bin, tree, exe) else {
        // カタログに居るのにフック設定を持たない = 機構が無い。
        // カタログにも居なければ、そもそも何のコマンドか判らない。
        let g = if crate::agents::spec_for_bin(bin).is_some() {
            Gate::NoMechanism
        } else {
            Gate::Unknown
        };
        return (g, String::new());
    };
    match crate::supervisor::hooks::status(&plan) {
        crate::supervisor::hooks::HookStatus::Installed => (Gate::Enforced, String::new()),
        crate::supervisor::hooks::HookStatus::Inactive => {
            let how = crate::supervisor::hooks::activation_gaps(&plan)
                .first()
                .map(|g| g.how.clone())
                .unwrap_or_default();
            (Gate::Unapproved, how)
        }
        crate::supervisor::hooks::HookStatus::Partial => (Gate::Partial, String::new()),
        crate::supervisor::hooks::HookStatus::Missing => (Gate::NotInstalled, String::new()),
    }
}

/// 走査 1 回ぶんの結果。
struct Scan {
    facts: Facts,
    /// 監査ログの場所 (「ログの場所」ボタンが返す文字列)。
    audit: PathBuf,
    cost: Duration,
}

/// GUI が開いているワークスペースのルート。
///
/// `app.rs` へ触らずに済ませるため、**自分自身のインスタンス登録**
/// (`~/.zaivern/instances/<pid>.json`) から引く。登録が無い / 壊れている
/// ときはカレントディレクトリへ落ちる (fail-soft)。
fn workspace_root() -> PathBuf {
    let me = std::process::id();
    crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// メッシュの登録ディレクトリ (`<zaivern>/mesh/<スコープ>/`)。
///
/// **`crate::mesh` へは `use` しない** — 別のブランチで同時に作られている
/// ので、コンパイル時に繋ぐと相手が居ないだけでこの画面ごとビルドが落ちる。
/// スコープの鍵は台帳と同じ [`crate::history::workspace_key`] から出すので、
/// 書き手と読み手は**必ず同じフォルダへ行き着く** (名前を 2 か所に持たない)。
fn mesh_dir(scope: &Path) -> PathBuf {
    crate::config::zaivern_dir()
        .join("mesh")
        .join(crate::history::workspace_key(scope))
}

/// メールボックスに溜まっている未読の数 (I/O・上限つき)。
///
/// 名前は相手の実装次第なので、よくある 3 つを順に見る。
fn inbox_count(proc_dir: &Path) -> usize {
    for name in ["inbox", "mbox", "mailbox"] {
        let Ok(rd) = std::fs::read_dir(proc_dir.join(name)) else {
            continue;
        };
        return rd
            .flatten()
            .filter(|e| e.path().is_file())
            .take(MESH_INBOX_CAP)
            .count();
    }
    0
}

/// PID から生存の 3 値を決める (純粋)。
///
/// **0 を「死んでいる」へ丸めない** — 登録に PID を書いていないだけの
/// プロセスまで詰まり扱いすると、唯一の詰まりがその中に埋もれる。
///
/// 生存確認を引数で受けるのは、テストが**実在しない PID を作らない**ため。
/// [`crate::instances::pid_alive`] は `pid as libc::pid_t` で i32 へ落とすので、
/// 大きな値を渡すと負のプロセスグループへ signal が飛ぶ (CLAUDE.md の
/// 「`kill` に負の PID」と同じ罠)。
pub fn liveness(pid: u32, alive: &dyn Fn(u32) -> bool) -> Live {
    if pid == 0 {
        Live::Unknown
    } else if alive(pid) {
        Live::Yes
    } else {
        Live::No
    }
}

/// メッシュをファイルシステムから直に読む (I/O)。
///
/// 受け付ける形は 2 つ。どちらで来ても読めるようにしておくのは、
/// **相手の形をこちらが決められない**から:
///
/// * `<スコープ>/<名前>.json` — 登録 1 件が 1 ファイル
/// * `<スコープ>/<名前>/proc.json` ＋ `<名前>/inbox/*` — メールボックス付き
///
/// ディレクトリが無ければ [`Mesh::present`] が `false` のまま返る。
/// **これはエラーではない** (まだ動いていないだけ)。
fn read_mesh(dir: &Path, alive: &dyn Fn(u32) -> bool) -> Mesh {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Mesh::default();
    };
    // 列挙順は OS 依存。**必ず並べ直す** (同じ状態から違う画面を出さない)。
    let mut names: Vec<(std::ffi::OsString, bool)> = rd
        .flatten()
        .map(|e| (e.file_name(), e.path().is_dir()))
        .collect();
    names.sort();

    let mut m = Mesh {
        present: true,
        ..Mesh::default()
    };
    for (name, is_dir) in names {
        if m.procs.len() >= MESH_PROC_CAP {
            m.more += 1;
            continue;
        }
        let path = dir.join(&name);
        let raw = name.to_string_lossy().into_owned();
        let stem = raw.strip_suffix(".json").unwrap_or(&raw).to_string();
        let mut proc = if is_dir {
            let mut p = ["proc.json", "self.json", "reg.json"]
                .iter()
                .find_map(|f| std::fs::read_to_string(path.join(f)).ok())
                .and_then(|t| read_proc(&t))
                .unwrap_or_default();
            p.unread += inbox_count(&path);
            p
        } else if raw.ends_with(".json") {
            match std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| read_proc(&t))
            {
                Some(p) => p,
                None => continue,
            }
        } else {
            continue;
        };
        if proc.name.is_empty() {
            proc.name = stem;
        }
        proc.live = liveness(proc.pid, alive);
        m.procs.push(proc);
    }
    m.procs.sort_by(|a, b| a.name.cmp(&b.name));
    m
}

/// 一撃マージの証明器を叩く (I/O・ワーカースレッドからだけ)。
///
/// **`crate::coedit` へは `use` しない。** 同時に別のブランチで作られている
/// ので、繋ぐなら実行時に繋ぐ — 自分自身の CLI をサブプロセスで起こして
/// JSON だけを受け取る。まだ無ければ `None` で、鎖 3 は git の突き合わせへ
/// 綺麗に降格する。
///
/// ## 罠: 先に [`crate::cli::is_cli_subcommand`] を必ず見ること
///
/// `zai` は**知らない語をワークスペース指定として扱い、GUI を起動する**。
/// 登録前に `zai coedit …` を起こすと、走査のたびに新しいエディタの窓が
/// 生える。登録済みかどうかは同じバイナリの中にある
/// [`crate::cli::is_cli_subcommand`] が唯一の真実源なので、そこで門を閉じる。
fn probe_proof(tree: &Path, exe: &Path) -> Option<Proof> {
    if !crate::cli::is_cli_subcommand("coedit") {
        return None;
    }
    let out = crate::procx::hidden_command(exe)
        .arg("coedit")
        .arg("proof")
        .arg("--json")
        .current_dir(tree)
        .stdin(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    read_proof(&crate::textenc::decode_output(&out.stdout))
}

/// 走査を裏のスレッドへ出す。**UI は 1 ミリ秒も待たない。**
///
/// git の教訓 (同期 `git branch --show-current` が 6023ms / 最悪フレーム
/// 4376ms) と同じ規律で、`git worktree list` も `conflict::scan` もここ。
fn spawn_scan(roots: crate::lease::Roots) -> Receiver<Scan> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let scan = collect(&roots);
        let _ = tx.send(Scan {
            facts: scan.0,
            audit: scan.1,
            cost: t0.elapsed(),
        });
    });
    rx
}

/// 事実を集める (I/O 本体)。**ワーカースレッドからだけ呼ぶこと。**
fn collect(roots: &crate::lease::Roots) -> (Facts, PathBuf) {
    let dir = crate::lease::store_dir();
    let store_path = crate::lease::store_path_in(&dir, &roots.key);
    let audit = crate::lease::audit_log_path(&dir);
    let mut f = Facts {
        ledger_on: crate::lease::enabled(&store_path),
        tier: crate::lease::current_tier(roots),
        ..Facts::default()
    };

    // ── 台帳の持ち主と担当パス ────────────────────────────────────
    let now = crate::lease::now_secs();
    let alive: &dyn Fn(u32) -> bool = &crate::instances::pid_alive;
    let mut owned: Vec<(String, Vec<String>)> = Vec::new();
    let mut agents: Vec<AgentFact> = Vec::new();
    if let Ok(store) = crate::lease::read_store(&store_path) {
        for l in store.leases.iter().filter(|l| l.active(now, alive)) {
            let name = l.holder.display();
            match owned.iter_mut().find(|(n, _)| *n == name) {
                Some((_, ps)) => ps.extend(l.patterns.iter().cloned()),
                None => owned.push((name.clone(), l.patterns.clone())),
            }
            if !agents.iter().any(|a: &AgentFact| a.name == name) {
                agents.push(AgentFact {
                    name,
                    bin: l.holder.agent.clone(),
                    holding: true,
                    gate: Gate::Unknown,
                    how: String::new(),
                });
            }
        }
    }
    f.owners = owned.iter().filter(|(_, p)| !p.is_empty()).count();
    // **ファイル単位ではなく行域で見る。** 同じファイルでも安全帯を挟んで
    // 離れていれば、2 人が同時に持ってよい。
    f.held = to_held(&owned);
    // 錨の元になる本文。**交錯している持ち主の組があるときだけ**呼ばれるので、
    // 互いに素な担当表 (普段の形) では 1 バイトも読まない。
    let read = |rel: &str| crate::lease::read_capped(&roots.tree.join(rel), &roots.tree);
    f.clashes = too_close_pairs(&f.held, crate::region::SAFE_BAND, &read);

    // ── タブの記録から起こす分 (台帳に居ない = まだ書いていないエージェント) ──
    // 「1 体ずつ出す」が目的なので、書き込む前から名前を出せるようにする。
    if let Some(data) = crate::session::load(std::slice::from_ref(&roots.tree)) {
        for rec in &data.agents {
            let bin = crate::agents::spec_for_command(&rec.command)
                .map(|s| s.bin.to_string())
                .unwrap_or_default();
            let name = if rec.title.is_empty() {
                bin.clone()
            } else {
                rec.title.clone()
            };
            if name.is_empty() || agents.iter().any(|a| a.name == name || a.bin == bin) {
                continue;
            }
            agents.push(AgentFact {
                name,
                bin,
                holding: false,
                gate: Gate::Unknown,
                how: String::new(),
            });
        }
    }

    // ── 1 体ずつのフック状態 (同じ bin は 1 回だけ調べる) ─────────
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zai"));
    let mut cache: Vec<(String, (Gate, String))> = Vec::new();
    for a in agents.iter_mut() {
        let hit = match cache.iter().find(|(b, _)| *b == a.bin) {
            Some((_, v)) => v.clone(),
            None => {
                let v = gate_of(&a.bin, &roots.tree, &exe);
                cache.push((a.bin.clone(), v.clone()));
                v
            }
        };
        a.gate = hit.0;
        a.how = hit.1;
    }
    agents.sort_by(|x, y| {
        y.gate
            .grade()
            .cmp(&x.gate.grade())
            .then(x.name.cmp(&y.name))
    });
    f.agents = agents;

    // ── 作業ツリーと統合の見込み ──────────────────────────────────
    let porcelain = crate::worktree::git_out(&roots.key, &["worktree", "list", "--porcelain"])
        .unwrap_or_default();
    let trees = parse_worktrees(&porcelain);
    f.trees = trees.len();
    if trees.len() >= 2 {
        let specs: Vec<crate::conflict::TreeSpec> = trees
            .iter()
            .enumerate()
            .map(|(i, t)| crate::conflict::TreeSpec {
                id: i as u64,
                label: t
                    .dir
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| t.branch.clone()),
                branch: t.branch.clone(),
                dir: t.dir.clone(),
            })
            .collect();
        let report = crate::conflict::scan(&specs, false);
        f.alarm_files = report.alarm_files();
        f.merge_note = report.note.clone();
        f.dirty_trees = report.trees.iter().filter(|t| t.dirty).count();
        // 証明器が居れば、こちらが権威になる (居なければ上の git 判定のまま)。
        f.proof = probe_proof(&roots.tree, &exe);
    }
    f.merge_scanned = true;

    // ── 共有面 ────────────────────────────────────────────────────
    f.audit_bytes = std::fs::metadata(&audit).map(|m| m.len()).unwrap_or(0);
    f.opaque_writes = read_audit_tail(&audit)
        .map(|s| count_opaque(&s))
        .unwrap_or(0);

    // ── メッシュ (登録ディレクトリを直に読む) ─────────────────────
    f.mesh = read_mesh(&mesh_dir(&roots.key), alive);
    (f, audit)
}

/// 監査ログの末尾を読む。**上限付き** (膨らんだログで走査を止めない)。
fn read_audit_tail(path: &Path) -> Option<String> {
    let len = std::fs::metadata(path).ok()?.len();
    let raw = std::fs::read(path).ok()?;
    let from = if len > AUDIT_READ_CAP {
        raw.len().saturating_sub(AUDIT_READ_CAP as usize)
    } else {
        0
    };
    Some(String::from_utf8_lossy(&raw[from..]).into_owned())
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. UI — パレットから開くパネル
// ═══════════════════════════════════════════════════════════════════════════

/// パネルの状態。**ウィンドウより長生きさせる** (設計原則 1) ため、
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
/// こうすると `app.rs` を 1 バイトも触らずに機能が繋がる。
#[derive(Default)]
struct PanelState {
    open: bool,
    roots: crate::lease::Roots,
    facts: Facts,
    /// 1 度でも結果が返ったか。返るまでは中央のカードだけを出す。
    ready: bool,
    audit: PathBuf,
    toast: String,
    /// 走っている走査。UI スレッドは**絶対に待たない**。
    pending: Option<Receiver<Scan>>,
    /// その走査を出した時刻。[`SCAN_GIVEUP`] を超えたら受け口ごと捨てる。
    pending_since: Option<Instant>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
}

fn state() -> &'static Mutex<PanelState> {
    static S: OnceLock<Mutex<PanelState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(PanelState::default()))
}

/// パレットの項目から呼ぶ入口。
pub fn open_panel() {
    let roots = crate::lease::roots_of(&workspace_root());
    if let Ok(mut st) = state().lock() {
        st.open = true;
        st.roots = roots;
        st.last_scan = None; // 開いた回だけ必ず取り直す
        st.toast.clear();
    }
}

/// パネルが要求した副作用 (描画の中では I/O をしない)。
///
/// **「調べ直す」は持たない。** 同じ操作へ到達する経路が 3 つ
/// (⟳ ボタン / パレットから開き直す / 走査間隔での自動更新) あったので、
/// ボタンを消して 2 つにした。状態を書き換える操作
/// ([`install_hooks`]) は、自分で `last_scan` を落として即座に取り直す。
enum Act {
    None,
    Fix(Fix),
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut act = Act::None;
    egui::Window::new(tr("🛟 競合ゼロ点検"))
        .collapsible(false)
        .resizable(true)
        .default_width(720.0)
        .default_height(500.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            act = body(ui, &st);
        });
    if !open {
        st.open = false;
    }
    apply(app, ctx, &mut st, act);
}

/// 非同期の結果を拾い、必要なら次の走査を出す。**待たない**。
fn poll(st: &mut PanelState, ctx: &egui::Context) {
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(scan) => {
                st.facts = scan.facts;
                // ガードは同じプロセスのメモリ上の状態なので、
                // ワーカーではなくここで見る (I/O ゼロ・常に最新)。
                st.facts.editor_guard = crate::lease::armed();
                st.audit = scan.audit;
                st.last_cost = Some(scan.cost);
                st.last_scan = Some(Instant::now());
                st.ready = true;
                st.pending = None;
                st.pending_since = None;
            }
            Err(TryRecvError::Empty) => {
                // **戻らないワーカーを待ち続けない。** `pending` が埋まって
                // いる間は次の走査を出さないので、諦めないと画面が永久に
                // 古いままになる。捨てたワーカーは送信に失敗して終わるだけ。
                if st.pending_since.is_some_and(|t| t.elapsed() >= SCAN_GIVEUP) {
                    st.pending = None;
                }
            }
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if st.pending.is_none() {
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = Some(spawn_scan(st.roots.clone()));
            st.pending_since = Some(Instant::now());
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す (閉じたら 1 回も要求しない)。
    ctx.request_repaint_after(Duration::from_millis(250));
}

fn apply(app: &mut crate::app::ZaivernApp, ctx: &egui::Context, st: &mut PanelState, act: Act) {
    match act {
        Act::None => {}
        // **「開きました」というトーストは出さない。** パネルが開いたことは
        // 開いたパネル自身が示している。見えている事実をもう一度書くと、
        // 本当に見えない結果 (コピー / 設定ファイルの書き換え) が埋もれる。
        Act::Fix(Fix::Lease) => {
            crate::lease::open_panel();
            st.toast.clear();
        }
        Act::Fix(Fix::Radar) => {
            app.toggle_conflict_radar();
            st.toast.clear();
        }
        Act::Fix(Fix::Audit) => {
            let p = st.audit.display().to_string();
            ctx.output_mut(|o| o.copied_text = p.clone());
            st.toast = trf("監査ログの場所をコピーしました: {p}", &[("p", p)]);
        }
        Act::Fix(Fix::InstallHooks) => {
            st.toast = install_hooks(st);
            st.last_scan = None;
        }
    }
}

/// 稼働中のエージェントのうち、**フックを持てるのにまだ入っていない**ものへ設置する。
///
/// 押されたときにしか書き換えない (同意はこのボタンで取る)。使っていない
/// ベンダーの設定ファイルには 1 バイトも書かない。
fn install_hooks(st: &PanelState) -> String {
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zai"));
    let mut done = 0usize;
    let mut errs: Vec<String> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for a in st.facts.agents.iter() {
        if !matches!(a.gate, Gate::NotInstalled | Gate::Partial) || seen.contains(&a.bin) {
            continue;
        }
        seen.push(a.bin.clone());
        let Some(plan) = crate::supervisor::hooks::plan_for(&a.bin, &st.roots.tree, &exe) else {
            continue;
        };
        match crate::supervisor::hooks::install(&plan) {
            Ok(()) => done += 1,
            Err(e) => errs.push(e),
        }
    }
    if !errs.is_empty() {
        return errs.join(" / ");
    }
    if done == 0 {
        return tr("設置できる相手が居ません (フック機構を持たないベンダーには仕掛けられません)");
    }
    trf(
        "{n} 種類のエージェントにフックを設置しました",
        &[("n", done.to_string())],
    )
}

fn body(ui: &mut egui::Ui, st: &PanelState) -> Act {
    let mut act = Act::None;
    let vis = ui.visuals().clone();

    ui.horizontal_wrapped(|ui| {
        if st.ready {
            let rows = judge(&st.facts);
            let worst = weakest(&rows);
            ui.label(
                egui::RichText::new(format!("{} {}", worst.glyph(), tr(headline(worst))))
                    .color(worst.color(&vis))
                    .strong(),
            )
            .on_hover_text(tr(
                "鎖は最も弱い輪より強くなりません。4 本のうち最悪の段を出しています",
            ));
        }
        ui.label(
            egui::RichText::new(crate::lease::ellipsize(&st.roots.key.to_string_lossy(), 44))
                .weak(),
        )
        .on_hover_text(st.roots.key.display().to_string());
    });
    ui.separator();

    if !st.ready {
        // 空状態は**中央に 1 枚のカード** (CLAUDE.md「空白は作らない」)。
        let avail = ui.available_rect_before_wrap();
        let card = empty_card(avail);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(tr("4 本の鎖を調べています…")).strong());
                ui.label(
                    egui::RichText::new(tr(
                        "台帳・フック・作業ツリー・監査ログを見ています。git を起こすので数秒かかることがあります",
                    ))
                    .weak(),
                );
            });
        });
        return act;
    }

    let rows = judge(&st.facts);
    egui::ScrollArea::vertical()
        .id_salt("zv-czero-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for r in &rows {
                if let Some(a) = chain_row(ui, r, &vis) {
                    act = a;
                }
                match r.chain {
                    Chain::Split => band_rows(ui, &st.facts, &vis),
                    Chain::Enforce => {
                        agent_rows(ui, &st.facts, &vis);
                        mesh_rows(ui, &st.facts.mesh, &vis);
                    }
                    Chain::Blind => blind_rows(ui),
                    _ => {}
                }
                ui.add_space(6.0);
            }
        });

    if !st.toast.is_empty() {
        ui.separator();
        ui.label(egui::RichText::new(st.toast.clone()).weak());
    }
    act
}

/// 全体の見出し (日本語原文)。
fn headline(g: Grade) -> &'static str {
    match g {
        Grade::Ok => "守られています",
        Grade::Warn => "一部しか守られていません",
        Grade::Bad => "守られていません",
    }
}

/// 鎖 1 行。**どの幅でも見切れない** ([`row_layout`] が保証する)。
fn chain_row(ui: &mut egui::Ui, r: &Row, vis: &egui::Visuals) -> Option<Act> {
    let mut act = None;
    let w = ui.available_width();
    let rect = egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(w, 20.0));
    // **列幅は純関数が決めた通りに使う** (ここで足し引きすると、
    // テーブルテストが保証した「収まる・重ならない」が崩れる)。
    let [c_glyph, c_title, c_reason, _c_fix] = row_layout(rect, TITLE_MIN).columns();
    let compact = is_compact(w);
    let reason = r.reason.text();
    ui.horizontal(|ui| {
        ui.allocate_ui(egui::vec2(c_glyph.width(), 20.0), |ui| {
            ui.label(egui::RichText::new(r.grade.glyph()).color(r.grade.color(vis)));
        });
        ui.allocate_ui(egui::vec2(c_title.width(), 20.0), |ui| {
            ui.label(egui::RichText::new(tr(r.chain.title())).strong());
        });
        ui.allocate_ui(egui::vec2(c_reason.width(), 20.0), |ui| {
            // 長い理由は省略し、全文はホバーで出す。
            let max = (c_reason.width() / 7.0).max(8.0) as usize;
            ui.label(crate::lease::ellipsize(&reason, max))
                .on_hover_text(reason.clone());
        });
        if let Some(fix) = r.fix {
            let label = if compact {
                fix.icon().to_string()
            } else {
                tr(fix.label())
            };
            let hover = format!("{}\n{}", tr(fix.label()), tr(fix.hint()));
            if ui.button(label).on_hover_text(hover).clicked() {
                act = Some(Act::Fix(fix));
            }
        }
    });
    act
}

/// 鎖 2 の内訳 — **稼働中の 1 体ずつ**。ここを丸めないのがこの機能の要。
///
/// **空なら見出しごと出さない** (CLAUDE.md「空白は作らない」)。
fn agent_rows(ui: &mut egui::Ui, f: &Facts, vis: &egui::Visuals) {
    if f.agents.is_empty() {
        return;
    }
    ui.indent("zv-czero-agents", |ui| {
        for a in &f.agents {
            let g = a.gate.grade();
            let w = ui.available_width();
            let name = crate::lease::ellipsize(&a.name, (w / 14.0).max(6.0) as usize);
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(g.glyph()).color(g.color(vis)));
                ui.label(egui::RichText::new(name).monospace())
                    .on_hover_text(&a.name);
                ui.label(egui::RichText::new(tr(a.gate.label())).color(g.color(vis)))
                    .on_hover_text(tr(a.gate.detail()));
                if !a.holding {
                    ui.label(egui::RichText::new(tr("(タブの記録から)")).weak())
                        .on_hover_text(tr(
                            "台帳にはまだ載っていません。前回のタブの記録から名前だけを出しています",
                        ));
                }
            });
            if !a.how.is_empty() {
                ui.label(
                    egui::RichText::new(crate::lease::ellipsize(&a.how, 96))
                        .weak()
                        .italics(),
                )
                .on_hover_text(&a.how);
            }
        }
    });
}

/// 持ち主の色。黄金比で色相を回すので、何人居ても隣の色と混ざらない。
///
/// `Visuals` に「N 人ぶんの色」は無いので、明暗で彩度と明度だけ変える。
/// 色は [`owner_slot`] (名前順) から出すので、**同じ台帳なら誰の画面でも同じ色**。
fn owner_color(slot: usize, vis: &egui::Visuals) -> egui::Color32 {
    let hue = (slot as f32 * 0.618_034) % 1.0;
    let (sat, val) = if vis.dark_mode {
        (0.55, 0.95)
    } else {
        (0.72, 0.72)
    };
    egui::Color32::from(egui::ecolor::Hsva::new(hue, sat, val, 1.0))
}

/// 鎖 1 の内訳 — **誰がどのファイルの何行目を持っているか**を帯で出す。
///
/// **空なら見出しごと出さない** (CLAUDE.md「空白は作らない」)。
fn band_rows(ui: &mut egui::Ui, f: &Facts, vis: &egui::Visuals) {
    if f.held.is_empty() {
        return;
    }
    let (files, more) = band_files(&f.held, &f.clashes, BAND_FILE_CAP);
    if files.is_empty() {
        return;
    }
    let owners = owner_list(&f.held);
    ui.indent("zv-czero-bands", |ui| {
        for fb in &files {
            // 可変長リストの中なので、要素の ID を混ぜる (egui 0.29 の ID 規則)。
            ui.push_id(fb.path.clone(), |ui| file_band(ui, f, fb, &owners, vis));
        }
        if more > 0 {
            ui.label(
                egui::RichText::new(trf(
                    "他 {n} ファイル (危険な順に上から出しています)",
                    &[("n", more.to_string())],
                ))
                .weak(),
            );
        }
    });
}

/// 1 ファイルぶんの帯。
///
/// 帯を**横**に寝かせているのは、縦棒にするとパネルの高さを人数ぶん食って
/// 4 本の鎖が画面外へ出るため (CLAUDE.md「画面が突然変わらない」)。
/// 読みたいのは行の絶対位置ではなく**隣との距離**なので、横で足りる。
fn file_band(ui: &mut egui::Ui, f: &Facts, fb: &FileBand, owners: &[String], vis: &egui::Visuals) {
    let w = ui.available_width();

    // 1 行目: ファイル名 ＋ 誰が居るかの色見本 (狭ければ折り返す)。
    ui.horizontal_wrapped(|ui| {
        let max = (w / 9.0).max(10.0) as usize;
        ui.label(
            egui::RichText::new(crate::lease::ellipsize(&fb.path, max))
                .monospace()
                .weak(),
        )
        .on_hover_text(&fb.path);
        for &i in &fb.items {
            let h = &f.held[i];
            ui.label(
                egui::RichText::new("■").color(owner_color(owner_slot(owners, &h.owner), vis)),
            )
            .on_hover_text(held_label(h));
        }
    });

    // 2 行目: 帯そのもの。
    let items: Vec<BandItem> = fb
        .items
        .iter()
        .map(|&i| BandItem {
            held: i,
            span: f.held[i].region.span,
            danger: f.clashes.iter().any(|c| c.lo == i || c.hi == i),
        })
        .collect();
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, BAND_H), egui::Sense::hover());
    let paint = ui.painter().clone();
    // 地 = まだ誰も持っていない行。ここが見えている限り、まだ入れる。
    paint.rect_filled(rect, 2.0, vis.extreme_bg_color);
    for seg in band_layout(rect, &items, band_scale(&items)) {
        let h = &f.held[seg.held];
        paint.rect_filled(
            seg.rect,
            1.0,
            owner_color(owner_slot(owners, &h.owner), vis),
        );
        if seg.danger {
            paint.rect_stroke(
                seg.rect,
                1.0,
                egui::Stroke::new(1.5_f32, Grade::Bad.color(vis)),
            );
        }
    }
    let full: Vec<String> = fb.items.iter().map(|&i| held_label(&f.held[i])).collect();
    resp.on_hover_text(full.join("\n"));

    // 3 行目以降: **危険地帯にだけ**「あと何行」を出す。
    // 守られている域には 1 行も割かない (安全は帯の絵で足りている)。
    for c in f
        .clashes
        .iter()
        .filter(|c| fb.items.contains(&c.lo) || fb.items.contains(&c.hi))
    {
        let args = vec![
            ("a", held_label(&f.held[c.lo])),
            ("b", held_label(&f.held[c.hi])),
            ("k", c.need.to_string()),
        ];
        // **交錯を「近すぎる」とも「丸ごと重なっている」とも言わない。**
        // 離しても直らないし、重なってもいない。第 3 の形である。
        let msg = if c.bracketed {
            trf(
                "{a} と {b} は交錯しています — 片方が相手を上下から挟んでいて、間に「このファイルで 1 回しか出てこない行」がありません。離しても直らないので、連続した 1 本の行域にするか担当を分けてください",
                &args,
            )
        } else if c.need > 0 {
            trf(
                "{a} と {b} は近すぎます — あと {k} 行空ければ一撃で通ります",
                &args,
            )
        } else {
            trf(
                "{a} と {b} は丸ごと重なっています — 行では解けないので、担当そのものを分けてください",
                &args,
            )
        };
        ui.horizontal_wrapped(|ui| {
            ui.label(egui::RichText::new(Grade::Bad.glyph()).color(Grade::Bad.color(vis)));
            let max = (w / 8.0).max(12.0) as usize;
            ui.label(egui::RichText::new(crate::lease::ellipsize(&msg, max)).weak())
                .on_hover_text(&msg);
        });
    }
    ui.add_space(2.0);
}

/// 1 件の担当を「持ち主 行域」の 1 行にする (表記は [`crate::region::render`])。
fn held_label(h: &Held) -> String {
    format!("{} {}", h.owner, crate::region::render(&h.region))
}

/// 鎖 2 の内訳 — **メッシュの生存**。登録ディレクトリを直に読んだ結果。
///
/// **未稼働はエラーではない。** 1 行で正直に出して終わる
/// (空のセクションで高さを取らない)。
fn mesh_rows(ui: &mut egui::Ui, m: &Mesh, vis: &egui::Visuals) {
    ui.indent("zv-czero-mesh", |ui| {
        if !m.present {
            ui.label(
                egui::RichText::new(tr(
                    "メッシュ未稼働 — プロセス同士の相互認識はまだ動いていません",
                ))
                .weak(),
            )
            .on_hover_text(tr(
                "登録ディレクトリが現れると、生きているプロセスと未読がここに出ます",
            ));
            return;
        }
        if m.procs.is_empty() {
            ui.label(
                egui::RichText::new(tr(
                    "メッシュは動いていますが、登録されたプロセスが 1 つもありません",
                ))
                .weak(),
            );
            return;
        }
        for p in &m.procs {
            let w = ui.available_width();
            let col = match p.live {
                Live::Yes => Grade::Ok.color(vis),
                Live::No => Grade::Bad.color(vis),
                Live::Unknown => vis.weak_text_color(),
            };
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(p.live.glyph()).color(col))
                    .on_hover_text(tr(match p.live {
                        Live::Yes => "プロセスは生きています",
                        Live::No => "プロセスは終了しています",
                        Live::Unknown => "登録に PID が無いので、生死を確かめられません",
                    }));
                ui.label(
                    egui::RichText::new(crate::lease::ellipsize(
                        &p.name,
                        (w / 14.0).max(6.0) as usize,
                    ))
                    .monospace(),
                )
                .on_hover_text(&p.name);
                if !p.kind.is_empty() {
                    ui.label(egui::RichText::new(&p.kind).weak());
                }
                if p.unread > 0 {
                    ui.label(
                        egui::RichText::new(trf("未読 {n}", &[("n", p.unread.to_string())])).weak(),
                    )
                    .on_hover_text(tr(
                        "メールボックスに溜まったままのメッセージです。相手が読んでいません",
                    ));
                }
                if p.live == Live::No && p.holds > 0 {
                    ui.label(
                        egui::RichText::new(trf(
                            "死んでいるのに担当を {n} 件握ったままです",
                            &[("n", p.holds.to_string())],
                        ))
                        .color(Grade::Bad.color(vis)),
                    )
                    .on_hover_text(tr(
                        "生きているエージェントは、この担当が解けるまで断られ続けます",
                    ));
                }
            });
        }
        if m.more > 0 {
            ui.label(
                egui::RichText::new(trf("他 {n} プロセス", &[("n", m.more.to_string())])).weak(),
            );
        }
    });
}

/// 鎖 4 の内訳 — **検出できない穴を全部並べる**。
fn blind_rows(ui: &mut egui::Ui) {
    ui.indent("zv-czero-blind", |ui| {
        for b in BLIND_SPOTS {
            let w = ui.available_width();
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new("•").weak());
                ui.label(egui::RichText::new(tr(b.what)).strong());
                let max = (w / 8.0).max(12.0) as usize;
                ui.label(egui::RichText::new(crate::lease::ellipsize(&tr(b.why), max)).weak())
                    .on_hover_text(tr(b.why));
            });
        }
    });
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. 登録 (`src/features/czero.rs` が再エクスポートする)
// ═══════════════════════════════════════════════════════════════════════════

/// パレットへの登録。**共有ファイルを 1 バイトも触らずに機能が繋がる**入口。
///
/// 打鍵は割り当てていない — `keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ最も硬い共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "czero",
    entries: &[crate::feature::Entry {
        icon: "🛟",
        label: "競合ゼロ点検 — いま自分がどこまで守られているかを見る",
        id: "czero.open",
    }],
    dispatch: |_app, _ctx, id| match id {
        "czero.open" => {
            open_panel();
            true
        }
        _ => false,
    },
    // 窓として自分で描く (`app.rs` のビュー列挙に触らない)。
    draw: Some(draw),
    ..crate::feature::Feature::DEFAULT
};

// ═══════════════════════════════════════════════════════════════════════════
//  7. テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(bin: &str, gate: Gate) -> AgentFact {
        AgentFact {
            name: bin.to_string(),
            bin: bin.to_string(),
            holding: true,
            gate,
            how: String::new(),
        }
    }

    /// 台帳のパターン 1 つぶんの担当。
    fn held(owner: &str, spec: &str) -> Held {
        Held {
            owner: owner.to_string(),
            region: crate::region::parse(spec).expect("行域の指定"),
        }
    }

    /// 危険地帯 1 組ぶん (段の表を作るためだけの素材)。
    fn clash(i: usize, need: u32) -> TooClose {
        TooClose {
            path: "src/a.rs".into(),
            lo: i * 2,
            hi: i * 2 + 1,
            need,
            bracketed: false,
        }
    }

    /// メッシュの登録 1 件。
    fn proc(name: &str, live: Live, holds: usize, unread: usize) -> MeshProc {
        MeshProc {
            name: name.to_string(),
            kind: "agent".into(),
            pid: if live == Live::Unknown { 0 } else { 4242 },
            live,
            holds,
            unread,
        }
    }

    /// 「守られている」状態の材料。個々のテストは 1 か所だけ崩す。
    fn guarded() -> Facts {
        Facts {
            ledger_on: true,
            owners: 2,
            // **同じファイルの違う行**を 2 人が持っている = 新しい既定形。
            held: vec![
                held("A", "src/app.rs#L1200-1260"),
                held("B", "src/app.rs#L4000-4100"),
            ],
            clashes: Vec::new(),
            tier: crate::lease::Tier::Enforced,
            agents: vec![agent("claude", Gate::Enforced)],
            mesh: Mesh::default(),
            trees: 1,
            merge_scanned: true,
            alarm_files: 0,
            merge_note: None,
            dirty_trees: 0,
            proof: None,
            opaque_writes: 0,
            editor_guard: true,
            audit_bytes: 1,
        }
    }

    // ── 鎖 1: 事前分割 ──────────────────────────────────────────────

    #[test]
    fn 事前分割の段は台帳と行域の表で決まる() {
        // (ledger_on, owners, 危険地帯の数) → (段, 原文の先頭)
        let table: &[(bool, usize, usize, Grade, &str)] = &[
            (false, 0, 0, Grade::Warn, "台帳が無効なので"),
            (false, 3, 2, Grade::Warn, "台帳が無効なので"),
            (true, 2, 1, Grade::Bad, "{n} 組の行域が安全帯"),
            (true, 5, 9, Grade::Bad, "{n} 組の行域が安全帯"),
            (true, 2, 0, Grade::Ok, "{n} 人が持つ {r} 個の行域は"),
            (true, 1, 0, Grade::Ok, "担当は 1 人だけなので"),
            (
                true,
                0,
                0,
                Grade::Warn,
                "まだ誰も担当行域を確保していません",
            ),
        ];
        for &(ledger_on, owners, clashes, want, head) in table {
            let f = Facts {
                ledger_on,
                owners,
                clashes: (0..clashes).map(|i| clash(i, 2)).collect(),
                ..guarded()
            };
            let r = &judge(&f)[0];
            assert_eq!(r.chain, Chain::Split);
            assert_eq!(
                r.grade, want,
                "ledger_on={ledger_on} owners={owners} clashes={clashes}"
            );
            assert!(
                r.template_starts_with(head),
                "原文が想定と違う: {:?}",
                r.reason.template
            );
        }
    }

    #[test]
    fn 危険地帯にはあと何行空ければよいかを差し込む() {
        let f = Facts {
            clashes: vec![clash(0, 2), clash(1, 7)],
            ..guarded()
        };
        let t = judge(&f)[0].reason.text();
        assert!(t.contains('2'), "組の数が出ていない: {t}");
        assert!(t.contains('7'), "最大であと何行かが出ていない: {t}");
    }

    #[test]
    fn 行では解けない重なりは別の言い方をする() {
        // ファイル全体を持たれていると、行をずらしても解けない。
        // 「あと 0 行」と出すのは嘘なので、言い方ごと変える。
        let f = Facts {
            clashes: vec![clash(0, 0)],
            ..guarded()
        };
        let r = &judge(&f)[0];
        assert_eq!(r.grade, Grade::Bad);
        assert!(
            r.template_starts_with("{n} 組の担当が丸ごと重なっています"),
            "原文が想定と違う: {:?}",
            r.reason.template
        );
    }

    // ── 行域そのもの ────────────────────────────────────────────────

    #[test]
    fn あと何行空ければ安全かを出す() {
        use crate::region::Span;
        // 帯の幅は引数なので、定数と切り離して 3 で表を作る。
        let band = 3u32;
        let table: &[(Span, Span, u32)] = &[
            // 間に 3 行あるので、もう動かさなくてよい
            (Span { start: 1, end: 10 }, Span { start: 14, end: 20 }, 0),
            // 間が 1 行だけ → あと 2 行
            (Span { start: 1, end: 10 }, Span { start: 12, end: 20 }, 2),
            // 隣り合っている → あと 3 行
            (Span { start: 1, end: 10 }, Span { start: 11, end: 20 }, 3),
            // 丸ごと食い込んでいる
            (Span { start: 1, end: 100 }, Span { start: 50, end: 60 }, 54),
            // 引数の順は関係ない
            (Span { start: 14, end: 20 }, Span { start: 1, end: 10 }, 0),
        ];
        for &(a, b, want) in table {
            assert_eq!(lines_needed(&a, &b, band), want, "{a:?} / {b:?}");
            // **0 と「もう安全」が一致していること。** ここがずれると
            // 「あと 0 行」と出しながら赤いまま、という画面になる。
            assert_eq!(
                crate::region::spans_too_close(&a, &b, band),
                lines_needed(&a, &b, band) > 0,
                "{a:?} / {b:?}"
            );
        }
        // 末尾までの域は、どれだけずらしても解けない (0 = 行では解けない)。
        let eof = Span {
            start: 5,
            end: Span::EOF,
        };
        let far = Span {
            start: 900,
            end: 910,
        };
        assert_eq!(lines_needed(&eof, &far, band), 0);
        assert!(crate::region::spans_too_close(&eof, &far, band));
    }

    #[test]
    fn 同じファイルでも安全帯を挟めば同時に持てる() {
        let band = crate::region::SAFE_BAND;
        // **これが方針転換の芯。** ファイル単位で丸めると両方 ❌ になる。
        let far = vec![
            held("A", "src/app.rs#L1200-1260"),
            held("B", "src/app.rs#L4000-4100"),
        ];
        assert!(
            too_close_pairs(&far, band, &|_| None).is_empty(),
            "行域なのにファイル単位で丸めている"
        );

        // 近すぎれば 1 組だけ出る。
        let near = vec![
            held("A", "src/app.rs#L1200-1260"),
            held("B", "src/app.rs#L1262-1300"),
        ];
        let got = too_close_pairs(&near, band, &|_| None);
        assert_eq!(got.len(), 1);
        assert_eq!((got[0].lo, got[0].hi), (0, 1));
        assert!(got[0].need > 0, "あと何行かが出ていない");

        // 行の若い方が lo (帯の絵と説明の順を一致させる)。
        let rev = vec![
            held("A", "src/app.rs#L1262-1300"),
            held("B", "src/app.rs#L1200-1260"),
        ];
        assert_eq!(too_close_pairs(&rev, band, &|_| None)[0].lo, 1);

        // 同じ持ち主どうしは数えない (書くのは 1 人)。
        let mine = vec![
            held("A", "src/app.rs#L1-10"),
            held("A", "src/app.rs#L11-20"),
        ];
        assert!(too_close_pairs(&mine, band, &|_| None).is_empty());

        // 別ファイルなら行が重なっていても関係ない。
        let other = vec![held("A", "src/a.rs#L1-10"), held("B", "src/b.rs#L1-10")];
        assert!(too_close_pairs(&other, band, &|_| None).is_empty());

        // 片方がファイル全体 = 行では解けない (need を 0 で出す)。
        let whole = vec![held("A", "src/app.rs"), held("B", "src/app.rs#L900-910")];
        let got = too_close_pairs(&whole, band, &|_| None);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].need, 0, "行をずらせば解けると嘘をついている");
    }

    /// **交錯を「近すぎる」とも「丸ごと重なる」とも数えない。**
    ///
    /// 帯 ([`crate::region::SAFE_BAND`]) は全部満たしているのに `git merge` が
    /// 衝突する唯一の形。ここが赤くなったら、`region` が直した判定が
    /// 競合ゼロの画面から外れている。
    #[test]
    fn 交錯した担当表を危険地帯として出す() {
        let band = crate::region::SAFE_BAND;
        // A が B の 2 つの域に挟まれている。どの組も帯を満たす。
        let held = vec![
            held("B", "src/a.rs#L13-13"),
            held("A", "src/a.rs#L17-17"),
            held("B", "src/a.rs#L25-25"),
        ];
        // 反復本文 = 錨 (ファイル内で唯一の行) が 1 本も無い
        let 反復: String = (0..60).map(|i| format!("line {}\n", i % 3)).collect();
        let got = too_close_pairs(&held, band, &|_| Some(反復.clone()));
        assert_eq!(got.len(), 1, "交錯を挙げていない: {got:?}");
        assert!(got[0].bracketed, "交錯として印を付けていない");
        assert_eq!(got[0].need, 0, "行をずらせば解けると嘘をついている");
        assert_eq!((got[0].lo, got[0].hi), (0, 1), "行の若い方が lo になっていない");

        // 錨が立つ本文なら通す (常に危険と言うへ倒れていないこと)
        let 一意: String = (0..60).map(|i| format!("行 {i} は他と違う\n")).collect();
        assert!(
            too_close_pairs(&held, band, &|_| Some(一意.clone())).is_empty(),
            "一意な本文まで危険地帯にした"
        );

        // 本文が読めなければ **fail-closed** (帯だけへ落とさない)
        let blind = too_close_pairs(&held, band, &|_| None);
        assert_eq!(blind.len(), 1, "読めないのに黙って通した: {blind:?}");
        assert!(blind[0].bracketed);
    }

    /// 交錯していない担当表では、**本文を 1 バイトも読まない** (費用の番人)。
    #[test]
    fn 交錯していなければ本文を読まない() {
        let band = crate::region::SAFE_BAND;
        // **A の外接域が B を跨がないこと**が「交錯していない」の意味。
        // (A に 200 行目を持たせると、間の B を挟むので交錯になる)
        let far = vec![
            held("A", "src/a.rs#L10-20"),
            held("A", "src/a.rs#L30-40"),
            held("B", "src/a.rs#L100-110"),
        ];
        let reads = std::cell::Cell::new(0u32);
        let got = too_close_pairs(&far, band, &|_| {
            reads.set(reads.get() + 1);
            None
        });
        assert!(got.is_empty(), "互いに素なのに挙げた: {got:?}");
        assert_eq!(reads.get(), 0, "交錯していないのに本文を読んだ");
    }

    #[test]
    fn 壊れた指定はファイル全体として扱う() {
        // 捨てると「持ち主が居ない」ように見えて、いちばん危ない側へ倒れる。
        let owned = vec![(
            "A".to_string(),
            vec!["src/a.rs#L".to_string(), "src/b.rs#L1-9".to_string()],
        )];
        let got = to_held(&owned);
        assert_eq!(got.len(), 2);
        assert!(got[0].region.is_whole(), "壊れた指定を捨てている");
        assert_eq!(got[0].region.path, "src/a.rs#L");
        assert_eq!(got[1].region.span.map(|s| (s.start, s.end)), Some((1, 9)));
    }

    #[test]
    fn 持ち主の色は名前順から決まる() {
        // `HashMap` の反復順が漏れると、同じ台帳でも人によって色が変わる。
        let h = vec![held("Z", "a.rs"), held("A", "b.rs"), held("Z", "c.rs")];
        assert_eq!(owner_list(&h), vec!["A".to_string(), "Z".to_string()]);
        let owners = owner_list(&h);
        assert_eq!(owner_slot(&owners, "A"), 0);
        assert_eq!(owner_slot(&owners, "Z"), 1);
        assert_eq!(owner_slot(&owners, "居ない"), 0, "知らない名前でも落ちない");
    }

    // ── 鎖 2: 実行中の強制 ──────────────────────────────────────────

    #[test]
    fn 台帳が無効なら段は必ず守られていない() {
        // フックが全部入っていても、`lease::gate` は無効なワークスペースを
        // 素通りさせる。ここを Ok にすると「止まらないのに止まると表示」になる。
        let f = Facts {
            tier: crate::lease::Tier::Off,
            agents: vec![agent("claude", Gate::Enforced)],
            ..guarded()
        };
        let r = &judge(&f)[1];
        assert_eq!(r.grade, Grade::Bad);
        assert_eq!(r.fix, Some(Fix::Lease));
    }

    #[test]
    fn 強制の段は一体ずつの最悪から決まる() {
        // (段, 稼働中の内訳) → 鎖の段
        let table: &[(crate::lease::Tier, &[Gate], Grade)] = &[
            (crate::lease::Tier::Enforced, &[], Grade::Warn),
            (
                crate::lease::Tier::Enforced,
                &[Gate::Enforced, Gate::Enforced],
                Grade::Ok,
            ),
            // **これが今の嘘**: claude だけ見て「強制」と出していた形。
            (
                crate::lease::Tier::Enforced,
                &[Gate::Enforced, Gate::NoMechanism],
                Grade::Bad,
            ),
            (
                crate::lease::Tier::Enforced,
                &[Gate::Enforced, Gate::Unapproved],
                Grade::Bad,
            ),
            (
                crate::lease::Tier::Enforced,
                &[Gate::Enforced, Gate::Unknown],
                Grade::Bad,
            ),
            (
                crate::lease::Tier::Enforced,
                &[Gate::Enforced, Gate::NotInstalled],
                Grade::Warn,
            ),
            (
                crate::lease::Tier::Enforced,
                &[Gate::Enforced, Gate::Partial],
                Grade::Warn,
            ),
            // 台帳はあるがフックが 1 つも無い = 勧告どまり。
            (crate::lease::Tier::Advisory, &[Gate::Enforced], Grade::Warn),
        ];
        for (tier, gates, want) in table {
            let f = Facts {
                tier: *tier,
                agents: gates.iter().map(|g| agent("x", *g)).collect(),
                ..guarded()
            };
            let r = &judge(&f)[1];
            assert_eq!(r.chain, Chain::Enforce);
            assert_eq!(r.grade, *want, "tier={tier:?} gates={gates:?}");
        }
    }

    #[test]
    fn 死んだまま担当を握っていると強制の段は守られていない() {
        // フックが全部入っていても、この 1 件があると生きているエージェントは
        // 断られ続ける。「止まる」が「進まない」へ化ける唯一の形。
        let stuck = Facts {
            mesh: Mesh {
                present: true,
                procs: vec![proc("a", Live::No, 3, 0)],
                more: 0,
            },
            ..guarded()
        };
        let r = &judge(&stuck)[1];
        assert_eq!(r.grade, Grade::Bad);
        assert!(r.reason.text().contains('1'), "件数が出ていない");
        assert_eq!(r.fix, Some(Fix::Lease));

        // 死んでいても担当を握っていなければ詰まりではない。
        let gone = Facts {
            mesh: Mesh {
                present: true,
                procs: vec![proc("a", Live::No, 0, 0)],
                more: 0,
            },
            ..guarded()
        };
        assert_eq!(judge(&gone)[1].grade, Grade::Ok);

        // PID を書いていないだけの登録を詰まり扱いにしない。
        let unknown = Facts {
            mesh: Mesh {
                present: true,
                procs: vec![proc("a", Live::Unknown, 5, 0)],
                more: 0,
            },
            ..guarded()
        };
        assert_eq!(judge(&unknown)[1].grade, Grade::Ok);

        // **未稼働はエラーではない。** ✅ を妨げない。
        assert_eq!(judge(&guarded())[1].grade, Grade::Ok);
    }

    #[test]
    fn 詰まりは死んでいて担当を握っている一件だけ数える() {
        // 生存も未読も 1 体ずつ画面に出す (数え上げは持たない — 常に 0 の
        // バッジを増やさないため)。判定に効くのは詰まりだけ。
        let m = Mesh {
            present: true,
            procs: vec![
                proc("a", Live::Yes, 2, 4),
                proc("b", Live::No, 1, 0),
                proc("c", Live::Unknown, 9, 1),
                proc("d", Live::No, 0, 0),
            ],
            more: 0,
        };
        assert_eq!(m.stuck(), 1, "死んでいても担当が無ければ詰まりではない");
        assert_eq!(Mesh::default().stuck(), 0);
    }

    #[test]
    fn 止まるのは強制の一体だけ() {
        // Gate の段は 1 か所で決める。ここが緩むと「止まらないのに ✅」になる。
        assert_eq!(Gate::Enforced.grade(), Grade::Ok);
        for g in [Gate::Unapproved, Gate::NoMechanism, Gate::Unknown] {
            assert_eq!(g.grade(), Grade::Bad, "{g:?}");
        }
        for g in [Gate::Partial, Gate::NotInstalled] {
            assert_eq!(g.grade(), Grade::Warn, "{g:?}");
        }
    }

    #[test]
    fn 一体ずつの説明は全種類そろっている() {
        for g in [
            Gate::Enforced,
            Gate::Unapproved,
            Gate::Partial,
            Gate::NotInstalled,
            Gate::NoMechanism,
            Gate::Unknown,
        ] {
            assert!(!g.label().is_empty(), "{g:?}");
            assert!(!g.detail().is_empty(), "{g:?}");
        }
    }

    // ── 鎖 3: 統合 ──────────────────────────────────────────────────

    #[test]
    fn 統合の段はツリー数と走査結果の表で決まる() {
        // (trees, scanned, alarm, note, dirty) → (段, 原文の一部, ボタン)
        let table: &[(usize, bool, usize, Option<&str>, usize, Grade, &str, bool)] = &[
            (
                0,
                true,
                0,
                None,
                0,
                Grade::Ok,
                "統合で突き合わせる相手",
                false,
            ),
            (
                1,
                true,
                0,
                None,
                0,
                Grade::Ok,
                "統合で突き合わせる相手",
                false,
            ),
            (3, false, 0, None, 0, Grade::Warn, "調べています", true),
            (
                3,
                true,
                0,
                Some("merge-tree が使えません"),
                0,
                Grade::Warn,
                "判定を下げました",
                true,
            ),
            (
                3,
                true,
                4,
                None,
                0,
                Grade::Bad,
                "個のファイルが衝突します",
                true,
            ),
            (
                3,
                true,
                0,
                None,
                2,
                Grade::Warn,
                "未コミットの変更があるので",
                true,
            ),
            (
                2,
                true,
                0,
                None,
                0,
                Grade::Ok,
                "git で突き合わせた限り衝突しません",
                true,
            ),
            // 衝突が出ているなら、未コミットがあっても ❌ を優先する。
            (
                2,
                true,
                1,
                None,
                2,
                Grade::Bad,
                "個のファイルが衝突します",
                true,
            ),
        ];
        for &(trees, merge_scanned, alarm_files, note, dirty_trees, want, part, has_fix) in table {
            let f = Facts {
                trees,
                merge_scanned,
                alarm_files,
                merge_note: note.map(str::to_string),
                dirty_trees,
                ..guarded()
            };
            let r = &judge(&f)[2];
            assert_eq!(r.chain, Chain::Merge);
            assert_eq!(r.grade, want, "trees={trees} alarm={alarm_files}");
            assert!(
                r.reason.template.contains(part),
                "原文が想定と違う: {:?}",
                r.reason.template
            );
            assert_eq!(r.fix.is_some(), has_fix, "trees={trees}");
        }
    }

    #[test]
    fn 証明が立てば一撃_立たなければ守られていない() {
        let base = Facts {
            trees: 3,
            ..guarded()
        };
        // 証明が立った
        let f = Facts {
            proof: Some(Proof {
                ok: true,
                pairs: 0,
                note: String::new(),
            }),
            ..base.clone()
        };
        let r = &judge(&f)[2];
        assert_eq!(r.grade, Grade::Ok);
        assert!(r.reason.template.contains("一撃で通ります"));

        // 立たなかった → 近すぎる組の数を証拠として出す
        let f = Facts {
            proof: Some(Proof {
                ok: false,
                pairs: 4,
                note: String::new(),
            }),
            ..base.clone()
        };
        let r = &judge(&f)[2];
        assert_eq!(r.grade, Grade::Bad);
        assert!(r.reason.text().contains('4'));

        // 証明器が自分で判定を下げたら、こちらも言い切らない
        let f = Facts {
            proof: Some(Proof {
                ok: true,
                pairs: 0,
                note: "錨を取り直せません".into(),
            }),
            ..base.clone()
        };
        assert_eq!(judge(&f)[2].grade, Grade::Warn);

        // **証明器がまだ無いときは git へ降格し、「証明」とは決して言わない。**
        let r = &judge(&base)[2];
        assert_eq!(r.grade, Grade::Ok);
        assert!(
            r.reason.template.contains("代用"),
            "降格していない: {:?}",
            r.reason.template
        );
        assert!(!r.reason.template.contains("証明できました"));
    }

    #[test]
    fn 証明器の出力を読む() {
        // 鍵の名前は相手の実装次第なので、別名を全部受ける。
        assert_eq!(
            read_proof(r#"{"ok":true}"#),
            Some(Proof {
                ok: true,
                pairs: 0,
                note: String::new()
            })
        );
        assert_eq!(
            read_proof(r#"{"proven":false,"conflicts":[[0,1],[2,3]]}"#),
            Some(Proof {
                ok: false,
                pairs: 2,
                note: String::new()
            })
        );
        assert_eq!(
            read_proof(r#" {"oneshot":true,"pairs":0,"note":"抜き"} "#).map(|p| p.note),
            Some("抜き".to_string())
        );
        // **読めなかったものを「証明できた」へ丸めない。**
        assert_eq!(read_proof(""), None);
        assert_eq!(read_proof("not json"), None);
        assert_eq!(read_proof("[]"), None);
        assert_eq!(read_proof(r#"{"pairs":0}"#), None, "ok が無いのに証明扱い");
    }

    #[test]
    fn 証明器はサブコマンドに登録されるまで起こさない() {
        // `zai` は**知らない語をワークスペース指定として扱い、GUI を起動する**。
        // 門を外すと、走査のたびに新しいエディタの窓が生える。
        let src = include_str!("czero.rs").replace("\r\n", "\n");
        let body = src
            .split_once("fn probe_proof(")
            .expect("probe_proof が見つからない")
            .1;
        let head: String = body.lines().take(8).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("is_cli_subcommand") && head.contains("return None"),
            "サブコマンド登録の門が無い:\n{head}"
        );
        if !crate::cli::is_cli_subcommand("coedit") {
            let dir = crate::test_util::unique_temp_dir("zv-czero", "proof");
            assert_eq!(
                probe_proof(&dir, &PathBuf::from("zai")),
                None,
                "登録前なのに起こした"
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }

    // ── 鎖 4: 共有面 ────────────────────────────────────────────────

    #[test]
    fn 共有面は決して守られている扱いにならない() {
        // 原理的に検出できない穴があるので、✅ は嘘になる。
        for opaque_writes in [0usize, 1, 40] {
            for editor_guard in [false, true] {
                let f = Facts {
                    opaque_writes,
                    editor_guard,
                    ..guarded()
                };
                let r = &judge(&f)[3];
                assert_eq!(r.chain, Chain::Blind);
                assert_ne!(r.grade, Grade::Ok, "opaque={opaque_writes}");
                assert!(r.fix.is_some());
            }
        }
    }

    #[test]
    fn 宛先の判らない書き込みがあれば証拠つきで守られていない() {
        let f = Facts {
            opaque_writes: 5,
            ..guarded()
        };
        let r = &judge(&f)[3];
        assert_eq!(r.grade, Grade::Bad);
        assert!(r.reason.text().contains('5'));
    }

    #[test]
    fn ログが空ならログの場所ボタンを出さない() {
        // 何も書かれていないファイルの場所を渡すボタンは、
        // 押しても何も起きないのと同じ (常に 0 のバッジと同類)。
        let f = Facts {
            audit_bytes: 0,
            ..guarded()
        };
        assert_eq!(judge(&f)[3].fix, None);
        let f = Facts {
            audit_bytes: 12,
            ..guarded()
        };
        assert_eq!(judge(&f)[3].fix, Some(Fix::Audit));
    }

    #[test]
    fn 穴の一覧は空でも重複でもない() {
        assert!(!BLIND_SPOTS.is_empty());
        let mut seen: Vec<&str> = Vec::new();
        for b in BLIND_SPOTS {
            assert!(!b.what.is_empty());
            assert!(!b.why.is_empty());
            assert!(!seen.contains(&b.what), "穴の名前が重複: {}", b.what);
            seen.push(b.what);
        }
    }

    // ── 鎖全体 ──────────────────────────────────────────────────────

    #[test]
    fn 鎖は最も弱い輪より強くならない() {
        // 4 本すべてが最良でも、鎖 4 が ⚠ なので全体は ⚠ 止まり。
        let rows = judge(&guarded());
        assert_eq!(rows[0].grade, Grade::Ok);
        assert_eq!(rows[1].grade, Grade::Ok);
        assert_eq!(rows[2].grade, Grade::Ok);
        assert_eq!(weakest(&rows), Grade::Warn);

        // 1 本でも ❌ があれば全体は ❌。
        let f = Facts {
            clashes: vec![clash(0, 2)],
            ..guarded()
        };
        assert_eq!(weakest(&judge(&f)), Grade::Bad);
    }

    #[test]
    fn 四本の鎖はこの順で必ず出る() {
        let rows = judge(&Facts::default());
        let got: Vec<Chain> = rows.iter().map(|r| r.chain).collect();
        assert_eq!(
            got,
            vec![Chain::Split, Chain::Enforce, Chain::Merge, Chain::Blind]
        );
        for r in &rows {
            assert!(!r.chain.title().is_empty());
            assert!(!r.reason.template.is_empty());
        }
    }

    #[test]
    fn 何も判っていないときは守られている扱いにしない() {
        // 既定値 = 台帳オフ・エージェント不明・走査前。ここが Ok に倒れると
        // 「起動直後は必ず安全」という最悪の嘘になる。
        assert_eq!(weakest(&judge(&Facts::default())), Grade::Bad);
    }

    #[test]
    fn ボタンの文言とアイコンは全種類そろっている() {
        for f in [Fix::Lease, Fix::InstallHooks, Fix::Radar, Fix::Audit] {
            assert!(!f.label().is_empty(), "{f:?}");
            assert!(!f.icon().is_empty(), "{f:?}");
            assert!(!f.hint().is_empty(), "{f:?}");
        }
    }

    #[test]
    fn 押しても何も起きないボタンは出さない() {
        // 「フックを設置」は**設置できる相手が居るときだけ**。居ないのに
        // 出すと、押して何も起きないボタンが「機能が有る」という嘘になる。
        for gates in [
            vec![],
            vec![Gate::Enforced],
            vec![Gate::NoMechanism],
            vec![Gate::Unknown],
            vec![Gate::NotInstalled],
            vec![Gate::Partial],
            vec![Gate::Enforced, Gate::NoMechanism],
        ] {
            let f = Facts {
                agents: gates.iter().map(|g| agent("x", *g)).collect(),
                ..guarded()
            };
            let installable = gates
                .iter()
                .any(|g| matches!(g, Gate::NotInstalled | Gate::Partial));
            if judge(&f)[1].fix == Some(Fix::InstallHooks) {
                assert!(installable, "設置先が無いのに設置ボタンを出した: {gates:?}");
            }
        }
    }

    #[test]
    fn 差し込み口は必ず埋まる() {
        // 原文に `{x}` が残ったまま画面へ出ると、そこだけ生の記法が見える。
        let cases = [
            Facts::default(),
            guarded(),
            Facts {
                clashes: vec![clash(0, 5), clash(1, 2)],
                tier: crate::lease::Tier::Advisory,
                agents: vec![agent("codex", Gate::Unapproved)],
                trees: 4,
                alarm_files: 7,
                merge_note: Some("x".into()),
                dirty_trees: 1,
                opaque_writes: 3,
                ..guarded()
            },
            // 行では解けない重なり ＋ メッシュの詰まり ＋ 証明が立たない
            Facts {
                clashes: vec![clash(0, 0)],
                mesh: Mesh {
                    present: true,
                    procs: vec![proc("a", Live::No, 2, 3)],
                    more: 1,
                },
                trees: 3,
                proof: Some(Proof {
                    ok: false,
                    pairs: 2,
                    note: String::new(),
                }),
                ..guarded()
            },
            // 証明器が自分で判定を下げた形
            Facts {
                trees: 3,
                proof: Some(Proof {
                    ok: true,
                    pairs: 0,
                    note: "錨".into(),
                }),
                ..guarded()
            },
        ];
        for f in cases {
            for r in judge(&f) {
                let t = r.reason.text();
                assert!(!t.contains('{'), "差し込みが残っている: {t}");
                assert!(!t.contains('}'), "差し込みが残っている: {t}");
            }
        }
    }

    // ── 検出の純粋部分 ──────────────────────────────────────────────

    #[test]
    fn worktree一覧を読む() {
        let raw = "worktree /repo\nHEAD aaa\nbranch refs/heads/main\n\n\
                   worktree /repo/.claude/worktrees/x\nHEAD bbb\nbranch refs/heads/feat/x\n\n\
                   worktree /gone\nHEAD ccc\ndetached\nprunable gitdir file points to non-existent location\n";
        let got = parse_worktrees(raw);
        assert_eq!(got.len(), 2, "prunable は数えない");
        assert_eq!(got[0].branch, "main");
        assert_eq!(got[1].branch, "feat/x");
        assert_eq!(got[1].dir, PathBuf::from("/repo/.claude/worktrees/x"));
    }

    #[test]
    fn worktree一覧は復帰改行が混ざっても読める() {
        // Windows のチェックアウト / パイプ越しでは改行が CRLF になる。
        let raw = "worktree C:/repo\r\nHEAD aaa\r\nbranch refs/heads/main\r\n\r\n\
                   worktree C:/wt\r\nHEAD bbb\r\ndetached\r\n";
        let got = parse_worktrees(raw);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].branch, "main");
        assert_eq!(got[1].branch, "", "detached はブランチ名を持たない");
        assert_eq!(got[1].dir, PathBuf::from("C:/wt"));
    }

    #[test]
    fn 空の一覧でも落ちない() {
        assert!(parse_worktrees("").is_empty());
        assert!(parse_worktrees("\n\n").is_empty());
    }

    #[test]
    fn 宛先の判らない書き込みを数える() {
        let log = "1 deny claude\n2 opaque-write codex #ab\n3 deny gemini\n4 opaque-write claude\n";
        assert_eq!(count_opaque(log), 2);
        assert_eq!(count_opaque(""), 0);
        assert_eq!(count_opaque("1 opaque-write x\r\n2 deny y\r\n"), 1);
    }

    #[test]
    fn 監査ログは上限つきで読む() {
        // 実 `~/.zaivern` に触れない。
        let dir = crate::test_util::unique_temp_dir("zv-czero", "audit");
        let path = dir.join("gate.log");
        let mut big = String::new();
        for i in 0..40_000 {
            big.push_str(&format!("{i} opaque-write agent\n"));
        }
        std::fs::write(&path, &big).expect("write audit log");
        let tail = read_audit_tail(&path).expect("read tail");
        assert!(
            tail.len() as u64 <= AUDIT_READ_CAP,
            "上限を超えて読んでいる: {}",
            tail.len()
        );
        assert!(count_opaque(&tail) > 0);
        assert!(read_audit_tail(&dir.join("no-such.log")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 監査ログの場所は台帳と同じ真実源から来る() {
        // 名前を 2 か所に持つと、書き手と読み手が静かにずれる。
        let dir = crate::test_util::unique_temp_dir("zv-czero", "path");
        assert_eq!(
            crate::lease::audit_log_path(&dir).parent(),
            Some(dir.as_path())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── レイアウト ──────────────────────────────────────────────────

    fn rect(w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(12.0, 34.0), egui::vec2(w, h))
    }

    #[test]
    fn どの幅でも列は領域に収まり重ならない() {
        // 極端な寸法を含める (900×700 / 1200×300 は CLAUDE.md の指定)。
        let sizes = [
            (900.0, 700.0),
            (1200.0, 300.0),
            (400.0, 900.0),
            (640.0, 480.0),
            (459.0, 200.0),
            (300.0, 120.0),
            (160.0, 60.0),
            (80.0, 40.0),
            (24.0, 20.0),
            (0.0, 0.0),
        ];
        for (w, h) in sizes {
            for longest in [0.0f32, 84.0, 400.0, 5_000.0] {
                let avail = rect(w, h);
                let lay = row_layout(avail, longest);
                let cols = lay.columns();
                for (i, c) in cols.iter().enumerate() {
                    assert!(c.width() >= 0.0, "負の幅 w={w} i={i}");
                    assert!(
                        c.left() >= avail.left() - 0.01 && c.right() <= avail.right() + 0.01,
                        "領域からはみ出した w={w} longest={longest} i={i}: {c:?} ⊄ {avail:?}"
                    );
                }
                for i in 1..cols.len() {
                    assert!(
                        cols[i].left() >= cols[i - 1].right() - 0.01,
                        "列が重なった w={w} longest={longest} i={i}: {:?} / {:?}",
                        cols[i - 1],
                        cols[i]
                    );
                }
            }
        }
    }

    #[test]
    fn 広いときは理由に一番幅を割く() {
        let lay = row_layout(rect(900.0, 700.0), 84.0);
        assert!(
            lay.reason.width() > lay.title.width(),
            "理由 {} ≤ 見出し {}",
            lay.reason.width(),
            lay.title.width()
        );
        assert!(lay.reason.width() > lay.fix.width());
    }

    #[test]
    fn 狭いときはボタンがアイコンだけへ縮む() {
        assert!(is_compact(459.0));
        assert!(!is_compact(460.0));
        let wide = row_layout(rect(900.0, 700.0), 84.0);
        let narrow = row_layout(rect(400.0, 200.0), 84.0);
        assert!(narrow.fix.width() < wide.fix.width());
    }

    #[test]
    fn 空状態のカードは中央にあり領域からはみ出さない() {
        for (w, h) in [(900.0, 700.0), (1200.0, 300.0), (200.0, 90.0), (10.0, 8.0)] {
            let avail = rect(w, h);
            let card = empty_card(avail);
            assert!(
                (card.center().x - avail.center().x).abs() < 0.01
                    && (card.center().y - avail.center().y).abs() < 0.01,
                "中央にない w={w}"
            );
            assert!(
                card.width() <= avail.width() + 0.01 && card.height() <= avail.height() + 0.01,
                "はみ出した w={w}: {card:?} ⊄ {avail:?}"
            );
        }
    }

    #[test]
    fn 帯はどの幅でも領域に収まり重ならない() {
        use crate::region::Span;
        let sets: &[&[BandItem]] = &[
            &[],
            // ファイル全体を 1 人が持つ
            &[BandItem {
                held: 0,
                span: None,
                danger: false,
            }],
            // 離れた 2 つ (同じファイルを 2 人で持てている形)
            &[
                BandItem {
                    held: 0,
                    span: Some(Span {
                        start: 1200,
                        end: 1260,
                    }),
                    danger: false,
                },
                BandItem {
                    held: 1,
                    span: Some(Span {
                        start: 4000,
                        end: 4100,
                    }),
                    danger: false,
                },
            ],
            // 1 行だけの域が並ぶ (最小幅の確保と「重ならない」が両立するか)
            &[
                BandItem {
                    held: 0,
                    span: Some(Span::line(1)),
                    danger: true,
                },
                BandItem {
                    held: 1,
                    span: Some(Span::line(2)),
                    danger: true,
                },
                BandItem {
                    held: 2,
                    span: Some(Span::line(3)),
                    danger: false,
                },
                BandItem {
                    held: 3,
                    span: Some(Span::line(4)),
                    danger: false,
                },
                BandItem {
                    held: 4,
                    span: Some(Span::line(5)),
                    danger: false,
                },
            ],
            // 末尾まで ＋ その中に入れ子
            &[
                BandItem {
                    held: 0,
                    span: Some(Span {
                        start: 10,
                        end: Span::EOF,
                    }),
                    danger: true,
                },
                BandItem {
                    held: 1,
                    span: Some(Span { start: 12, end: 14 }),
                    danger: true,
                },
            ],
        ];
        // 極端な寸法 (900×700 / 1200×300 / 400×900 は CLAUDE.md の指定)。
        for (w, h) in [
            (900.0, 700.0),
            (1200.0, 300.0),
            (400.0, 900.0),
            (160.0, 60.0),
            (24.0, 20.0),
            (2.0, 10.0),
            (0.0, 0.0),
        ] {
            for items in sets {
                let avail =
                    egui::Rect::from_min_size(egui::pos2(12.0, 34.0), egui::vec2(w, BAND_H.min(h)));
                let segs = band_layout(avail, items, band_scale(items));
                for (i, seg) in segs.iter().enumerate() {
                    assert!(seg.rect.width() >= 0.0, "負の幅 w={w} i={i}");
                    assert!(
                        seg.rect.left() >= avail.left() - 0.01
                            && seg.rect.right() <= avail.right() + 0.01,
                        "領域からはみ出した w={w} i={i}: {:?} ⊄ {avail:?}",
                        seg.rect
                    );
                }
                for i in 1..segs.len() {
                    assert!(
                        segs[i].rect.left() >= segs[i - 1].rect.right() - 0.01,
                        "区画が重なった (同じ行を 2 人が持っている絵) w={w} i={i}: {:?} / {:?}",
                        segs[i - 1].rect,
                        segs[i].rect
                    );
                }
            }
        }
    }

    #[test]
    fn 一行だけの域も見える幅を持つ() {
        // **見えない所有は無いのと同じ。** 1 行の域が 0 ピクセルになると
        // 「誰も持っていない」という絵になる。
        use crate::region::Span;
        let items = [
            BandItem {
                held: 0,
                span: Some(Span::line(1)),
                danger: false,
            },
            BandItem {
                held: 1,
                span: Some(Span {
                    start: 500,
                    end: 600,
                }),
                danger: false,
            },
        ];
        let avail = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(600.0, BAND_H));
        let segs = band_layout(avail, &items, band_scale(&items));
        assert_eq!(segs.len(), 2);
        assert!(
            segs[0].rect.width() >= BAND_MIN_W - 0.01,
            "1 行の域が消えた: {:?}",
            segs[0].rect
        );
    }

    #[test]
    fn 帯は危険な順に上限まで出す() {
        // 危険地帯が壁の下に埋もれたら、この画面は目的を果たしていない。
        let mut h = Vec::new();
        for i in 0..10 {
            h.push(held("A", &format!("src/f{i:02}.rs#L1-10")));
        }
        h.push(held("A", "src/zz.rs#L1-10"));
        h.push(held("B", "src/zz.rs#L11-20"));
        let clashes = too_close_pairs(&h, crate::region::SAFE_BAND, &|_| None);
        assert_eq!(clashes.len(), 1);

        let (files, more) = band_files(&h, &clashes, 3);
        assert_eq!(files.len(), 3);
        assert_eq!(more, 8, "上限を超えたぶんは件数だけ出す");
        assert_eq!(files[0].path, "src/zz.rs", "危険地帯が先頭に来ていない");
        assert_eq!(files[0].danger, 1);
        assert_eq!(files[0].items, vec![10, 11], "先頭行の昇順で並べる");
        // 残りはパス順 (同じ入力からは必ず同じ並び)。
        assert_eq!(files[1].path, "src/f00.rs");
        assert_eq!(files[2].path, "src/f01.rs");
        assert_eq!(band_files(&h, &clashes, 3).0, files);
        // 上限に届かなければ余りは 0。
        assert_eq!(band_files(&h, &clashes, 99).1, 0);
        assert!(band_files(&[], &[], 8).0.is_empty());
    }

    // ── メッシュの読み取り ──────────────────────────────────────────

    #[test]
    fn 判らない生存を死んだ扱いにしない() {
        assert_eq!(liveness(0, &|_: u32| true), Live::Unknown);
        assert_eq!(liveness(9, &|_: u32| true), Live::Yes);
        assert_eq!(liveness(9, &|_: u32| false), Live::No);
        assert_eq!(Live::default(), Live::Unknown);
        for l in [Live::Yes, Live::No, Live::Unknown] {
            assert!(!l.glyph().is_empty(), "{l:?}");
        }
    }

    #[test]
    fn メッシュの登録は別名でも読める() {
        // 相手の鍵名をこちらが決められないので、よくある別名を全部見る。
        let p = read_proc(
            r#"{"id":"claude-1","role":"agent","process_id":77,"owns":["a","b"],"pending":3}"#,
        )
        .expect("読めない");
        assert_eq!(p.name, "claude-1");
        assert_eq!(p.kind, "agent");
        assert_eq!(p.pid, 77);
        assert_eq!(p.holds, 2);
        assert_eq!(p.unread, 3);
        // 件数は数でも一覧でも受ける。
        assert_eq!(
            read_proc(r#"{"name":"x","holds":5}"#).map(|p| p.holds),
            Some(5)
        );
        // 壊れていても画面ごと壊さない。
        assert_eq!(read_proc("[]"), None);
        assert_eq!(read_proc("こわれ"), None);
        assert_eq!(read_proc("{}").map(|p| p.live), Some(Live::Unknown));
    }

    #[test]
    fn メッシュは登録ディレクトリから直に読む() {
        // 実 `~/.zaivern` には触れない。
        let dir = crate::test_util::unique_temp_dir("zv-czero", "mesh");
        // **未稼働はエラーではない** (ディレクトリが無いだけ)。
        assert_eq!(
            read_mesh(&dir.join("no-such"), &|_: u32| true),
            Mesh::default()
        );
        assert!(!Mesh::default().present);

        std::fs::create_dir_all(&dir).expect("mkdir");
        // 形 1: 1 ファイル 1 登録 (PID 無し)
        std::fs::write(dir.join("editor.json"), r#"{"kind":"editor"}"#).expect("write");
        // 形 2: フォルダ ＋ メールボックス (名前は登録に無い)
        let a = dir.join("agent-a");
        std::fs::create_dir_all(a.join("inbox")).expect("mkdir");
        std::fs::write(
            a.join("proc.json"),
            r#"{"kind":"agent","pid":4242,"regions":["src/a.rs#L1-9"]}"#,
        )
        .expect("write");
        for i in 0..2 {
            std::fs::write(a.join("inbox").join(format!("{i}.json")), "{}").expect("write");
        }
        // 読めないもの / 関係ないものは黙って飛ばす。
        std::fs::write(dir.join("broken.json"), "not json").expect("write");
        std::fs::write(dir.join("notes.txt"), "x").expect("write");

        let m = read_mesh(&dir, &|_: u32| false);
        assert!(m.present);
        assert_eq!(m.procs.len(), 2, "読めない登録で画面を壊している");
        assert_eq!(
            m.procs[0].name, "agent-a",
            "名前が無ければフォルダ名から起こす"
        );
        assert_eq!(m.procs[0].unread, 2, "メールボックスを数えていない");
        assert_eq!(m.procs[0].holds, 1);
        assert_eq!(m.procs[0].live, Live::No);
        assert_eq!(m.procs[1].name, "editor");
        assert_eq!(
            m.procs[1].live,
            Live::Unknown,
            "PID が無いのを死んだ扱いにしている"
        );
        assert_eq!(m.stuck(), 1);
        // 生きていれば詰まりではない。
        assert_eq!(read_mesh(&dir, &|_: u32| true).stuck(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn メッシュの場所は台帳と同じ鍵から出す() {
        // 書き手と読み手が別の鍵を持つと、静かに空の画面になる。
        let dir = crate::test_util::unique_temp_dir("zv-czero", "scope");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let got = mesh_dir(&dir);
        assert!(
            got.ends_with(crate::history::workspace_key(&dir)),
            "鍵が台帳と違う: {got:?}"
        );
        assert_eq!(
            got.parent().and_then(|p| p.file_name()),
            Some(std::ffi::OsStr::new("mesh"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 登録 ────────────────────────────────────────────────────────

    #[test]
    fn 登録はモジュール接頭辞つきで描画も持つ() {
        assert_eq!(FEATURE.module, "czero");
        assert!(!FEATURE.entries.is_empty());
        for e in FEATURE.entries {
            assert!(e.id.starts_with("czero."), "ID の接頭辞が違う: {}", e.id);
            assert!(!e.icon.is_empty());
            assert!(!e.label.is_empty());
        }
        // パネルは窓として自分で描くので、描画が無いと**到達できない**。
        assert!(FEATURE.draw.is_some(), "draw が無いと開いても何も出ない");
    }

    #[test]
    fn 打鍵表記をベタ書きしていない() {
        // 画面に出す打鍵は config で再割り当てされるし、OS で表記も違う。
        // ベタ書きした瞬間に嘘になるので、記号ごと持たない。
        const GLYPHS: [char; 4] = ['⌘', '⌥', '⌃', '⇧'];
        let src = include_str!("czero.rs").replace("\r\n", "\n");
        for (i, line) in src.lines().enumerate() {
            if line.contains("GLYPHS") || line.contains("assert") {
                continue;
            }
            assert!(
                !(line.contains('"') && line.chars().any(|c| GLYPHS.contains(&c))),
                "{}行目に打鍵表記のベタ書き: {line}",
                i + 1
            );
        }
    }

    #[test]
    fn 新しい内訳は必ず描画から呼ばれている() {
        // 「作ったのに繋いでいない」を構造で防ぐ (`never used` の代わり —
        // 描画から呼ばれていない内訳は、UI から到達できないので未完成)。
        let src = include_str!("czero.rs").replace("\r\n", "\n");
        let after = src
            .split_once("fn body(ui: &mut egui::Ui")
            .expect("body が見つからない")
            .1;
        // 自分より後ろの関数やテスト本文を拾わないよう、次の関数で切る。
        let inside = after
            .split_once("fn headline(")
            .map(|(a, _)| a)
            .unwrap_or(after);
        for f in ["band_rows(", "mesh_rows(", "agent_rows(", "blind_rows("] {
            assert!(inside.contains(f), "描画から呼ばれていない: {f}");
        }
        // 消したものが黙って戻っていないこと (`concat!` は自己一致よけ)。
        assert!(
            !src.contains(concat!("Act::", "Refresh")),
            "⟳ ボタンが戻っている (到達経路が 3 つに戻る)"
        );
        assert!(
            !src.contains(concat!("fn count", "_overlaps")),
            "ファイル単位の重なり計算が戻っている (行域と 2 実装になる)"
        );
    }

    #[test]
    fn 閉じている間は描画で何もしない() {
        // 設計原則 3 (アイドル時のコストはゼロ)。`draw` の先頭で即 return
        // していることを構造で固定する。
        let src = include_str!("czero.rs").replace("\r\n", "\n");
        let body = src
            .split_once("pub fn draw(app: &mut crate::app::ZaivernApp")
            .expect("draw が見つからない")
            .1;
        let head: String = body.lines().take(6).collect::<Vec<_>>().join("\n");
        assert!(
            head.contains("if !st.open") && head.contains("return"),
            "draw の先頭で閉じているときに return していない:\n{head}"
        );
    }

    impl Row {
        fn template_starts_with(&self, head: &str) -> bool {
            self.reason.template.starts_with(head)
        }
    }
}
