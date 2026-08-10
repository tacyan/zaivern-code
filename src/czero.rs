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
//! 1. **事前分割** — 稼働中の担当パスが互いに素か ([`Chain::Split`])
//! 2. **実行中の強制** — 段 ＋ **1 体ずつ**の実態 ([`Chain::Enforce`])
//! 3. **統合** — いま統合したら衝突するか ([`Chain::Merge`])
//! 4. **共有面** — 検出できていない穴 ([`Chain::Blind`])
//!
//! ### 鎖 2 を丸めないことが、このモジュールの存在理由
//!
//! 「Enforced」と 1 つだけ出すのが**いまの嘘**である。claude は止まるが
//! cursor-agent は止まらない、という状態でも表示は「強制」になる。
//! [`agent_rows`] は稼働中の 1 体ずつに [`Gate`] を付けて出す。
//!
//! ## 疎結合の約束
//!
//! 同時に別のブランチで作られている新規モジュール (guard / train / union /
//! split) へは**コンパイル時依存を 1 つも持たない**。状態はファイルシステムと
//! git から直に検出する — そうしておけば、相手が出来ていても居なくても
//! この画面は動く。
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

/// 検出した生の事実。**[`judge`] はこれだけを見る。**
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Facts {
    // ── 鎖 1: 事前分割 ──────────────────────────────────────────────
    /// このワークスペースで台帳が有効か。
    pub ledger_on: bool,
    /// 担当パスを 1 つ以上持っている持ち主の数。
    pub owners: usize,
    /// **別々の持ち主の**担当パスが重なっている組の数。
    pub overlaps: usize,

    // ── 鎖 2: 実行中の強制 ──────────────────────────────────────────
    /// リースの段。[`crate::lease::Tier::Off`] なら、フックが入っていても
    /// `gate()` は素通りするので**何も止まらない**。
    pub tier: crate::lease::Tier,
    /// 稼働中と分かっている 1 体ずつ。
    pub agents: Vec<AgentFact>,

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

    // ── 鎖 4: 共有面 ────────────────────────────────────────────────
    /// 監査ログに残った「宛先の判らない書き込み」の件数。
    pub opaque_writes: usize,
    /// このエディタ自身の保存が台帳を通っているか ([`crate::lease::armed`])。
    pub editor_guard: bool,
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

/// 鎖 1 — 配る前に担当が分かれているか。
fn split_row(f: &Facts) -> Row {
    let (grade, reason, fix) = if !f.ledger_on {
        (
            Grade::Warn,
            Reason::plain("台帳が無効なので、担当パスが重なっているかどうかを判定できません"),
            Some(Fix::Lease),
        )
    } else if f.overlaps > 0 {
        (
            Grade::Bad,
            Reason::with(
                "{n} 組の担当パスが重なっています — このまま走らせると、衝突はマージのときまで見えません",
                vec![("n", f.overlaps.to_string())],
            ),
            Some(Fix::Lease),
        )
    } else if f.owners >= 2 {
        (
            Grade::Ok,
            Reason::with(
                "{n} 人の担当パスは互いに素です。同じファイルを 2 人が触ることはありません",
                vec![("n", f.owners.to_string())],
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
                "まだ誰も担当パスを確保していません (エージェントが書き込むと自動で登録されます)",
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

/// 鎖 3 — いま統合したら衝突するか。
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
                "{n} 本は、いま統合しても衝突しません",
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
        fix: Some(Fix::Audit),
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

/// 台帳から「持ち主 → 担当パス」を起こし、重なりの組数を数える (純粋)。
///
/// 数えるのは**別々の持ち主の間**だけ。同じ持ち主が自分のパターンを
/// 2 つ重ねて持っていても、衝突は起こらない。
fn count_overlaps(owned: &[(String, Vec<String>)]) -> usize {
    let mut n = 0;
    for i in 0..owned.len() {
        for j in (i + 1)..owned.len() {
            for a in &owned[i].1 {
                for b in &owned[j].1 {
                    if crate::lease::overlaps(a, b) {
                        n += 1;
                    }
                }
            }
        }
    }
    n
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
    f.overlaps = count_overlaps(&owned);

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
    agents.sort_by(|x, y| y.gate.grade().cmp(&x.gate.grade()).then(x.name.cmp(&y.name)));
    f.agents = agents;

    // ── 作業ツリーと統合の見込み ──────────────────────────────────
    let porcelain =
        crate::worktree::git_out(&roots.key, &["worktree", "list", "--porcelain"]).unwrap_or_default();
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
    }
    f.merge_scanned = true;

    // ── 共有面 ────────────────────────────────────────────────────
    f.opaque_writes = read_audit_tail(&audit).map(|s| count_opaque(&s)).unwrap_or(0);
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
enum Act {
    None,
    Refresh,
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
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if st.pending.is_none() {
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = Some(spawn_scan(st.roots.clone()));
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す (閉じたら 1 回も要求しない)。
    ctx.request_repaint_after(Duration::from_millis(250));
}

fn apply(app: &mut crate::app::ZaivernApp, ctx: &egui::Context, st: &mut PanelState, act: Act) {
    match act {
        Act::None => {}
        Act::Refresh => st.last_scan = None,
        Act::Fix(Fix::Lease) => {
            crate::lease::open_panel();
            st.toast = tr("ファイル所有の一覧を開きました");
        }
        Act::Fix(Fix::Radar) => {
            app.toggle_conflict_radar();
            st.toast = tr("衝突レーダーを開きました");
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
            egui::RichText::new(crate::lease::ellipsize(
                &st.roots.key.to_string_lossy(),
                44,
            ))
            .weak(),
        )
        .on_hover_text(st.roots.key.display().to_string());
        if ui.button("⟳").on_hover_text(tr("調べ直す")).clicked() {
            act = Act::Refresh;
        }
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
                    Chain::Enforce => agent_rows(ui, &st.facts, &vis),
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
    settings: &[],
    binds: &[],
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

    /// 「守られている」状態の材料。個々のテストは 1 か所だけ崩す。
    fn guarded() -> Facts {
        Facts {
            ledger_on: true,
            owners: 2,
            overlaps: 0,
            tier: crate::lease::Tier::Enforced,
            agents: vec![agent("claude", Gate::Enforced)],
            trees: 1,
            merge_scanned: true,
            alarm_files: 0,
            merge_note: None,
            dirty_trees: 0,
            opaque_writes: 0,
            editor_guard: true,
        }
    }

    // ── 鎖 1: 事前分割 ──────────────────────────────────────────────

    #[test]
    fn 事前分割の段は台帳と重なりの表で決まる() {
        // (ledger_on, owners, overlaps) → (段, 原文の先頭)
        let table: &[(bool, usize, usize, Grade, &str)] = &[
            (false, 0, 0, Grade::Warn, "台帳が無効なので"),
            (false, 3, 2, Grade::Warn, "台帳が無効なので"),
            (true, 2, 1, Grade::Bad, "{n} 組の担当パスが重なっています"),
            (true, 5, 9, Grade::Bad, "{n} 組の担当パスが重なっています"),
            (true, 2, 0, Grade::Ok, "{n} 人の担当パスは互いに素です"),
            (true, 1, 0, Grade::Ok, "担当は 1 人だけなので"),
            (true, 0, 0, Grade::Warn, "まだ誰も担当パスを確保していません"),
        ];
        for &(ledger_on, owners, overlaps, want, head) in table {
            let f = Facts {
                ledger_on,
                owners,
                overlaps,
                ..guarded()
            };
            let r = &judge(&f)[0];
            assert_eq!(r.chain, Chain::Split);
            assert_eq!(
                r.grade, want,
                "ledger_on={ledger_on} owners={owners} overlaps={overlaps}"
            );
            assert!(
                r.template_starts_with(head),
                "原文が想定と違う: {:?}",
                r.reason.template
            );
        }
    }

    #[test]
    fn 重なりがあるときは件数を差し込む() {
        let f = Facts {
            overlaps: 3,
            ..guarded()
        };
        assert!(judge(&f)[0].reason.text().contains('3'));
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
            (
                crate::lease::Tier::Advisory,
                &[Gate::Enforced],
                Grade::Warn,
            ),
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
            (0, true, 0, None, 0, Grade::Ok, "統合で突き合わせる相手", false),
            (1, true, 0, None, 0, Grade::Ok, "統合で突き合わせる相手", false),
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
            (3, true, 4, None, 0, Grade::Bad, "個のファイルが衝突します", true),
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
            (2, true, 0, None, 0, Grade::Ok, "統合しても衝突しません", true),
            // 衝突が出ているなら、未コミットがあっても ❌ を優先する。
            (2, true, 1, None, 2, Grade::Bad, "個のファイルが衝突します", true),
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
            overlaps: 1,
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
                overlaps: 2,
                tier: crate::lease::Tier::Advisory,
                agents: vec![agent("codex", Gate::Unapproved)],
                trees: 4,
                alarm_files: 7,
                merge_note: Some("x".into()),
                dirty_trees: 1,
                opaque_writes: 3,
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
    fn 重なりは別々の持ち主の間だけ数える() {
        // 同じ持ち主が自分のパターンを重ねて持っていても衝突は起きない。
        let one = vec![("A".to_string(), vec!["src/**".into(), "src/a.rs".into()])];
        assert_eq!(count_overlaps(&one), 0);

        let two = vec![
            ("A".to_string(), vec!["src/auth/**".into()]),
            ("B".to_string(), vec!["src/ui/**".into()]),
        ];
        assert_eq!(count_overlaps(&two), 0);

        let clash = vec![
            ("A".to_string(), vec!["src/**".into()]),
            ("B".to_string(), vec!["src/ui/x.rs".into()]),
        ];
        assert_eq!(count_overlaps(&clash), 1);
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
