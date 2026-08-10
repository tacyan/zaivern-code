//! 🚦 競合ゼロの**導入・診断・実証・撤去** — `zai czero init|doctor|verify|uninstall`。
//!
//! ## なぜ要るのか (穴の形)
//!
//! 競合ゼロの部品はもう全部ある — 行域の台帳 (`lease`)、git を関所にする
//! フック (`guard`)、追記どうしを自動で解決する merge driver (`union`)、
//! プロセスメッシュ (`mesh`)、交渉 (`negotiate`)、一撃統合 (`coedit`)。
//! **足りないのは「新しいリポジトリでこれを有効にする手順」**だった。
//!
//! 実際に必要だった手作業はこれだけある:
//!
//!   1. `zai guard init` を打つ
//!   2. `git config merge.zaivern-union-auto.driver …` を 4 種ぶん登録する
//!   3. `.gitattributes` に、そのリポジトリに実在する一覧ファイルの
//!      パターンを書く (既存の指定は壊さない)
//!   4. 行域の台帳を有効にする
//!   5. **それらが本当に効いているかを確かめる**
//!
//! 5 を飛ばすのが一番危ない。**入れっぱなしで効いていない**状態は
//! 「守られているつもり」なので、衝突がマージのときまで見えないという
//! 元の問題がそのまま戻ってくる。だからここでは
//! **init が最後に必ず doctor を走らせる**。
//!
//! ## 4 つの入口
//!
//! | 入口 | 何をするか | 対象リポジトリを書き換えるか |
//! |---|---|---|
//! | `init` | 1 コマンドで守りを入れる (冪等) | する (`--dry-run` なら何もしない) |
//! | `doctor` | 段ごとに ✅ / ⚠ / ❌ と理由と直し方を出す | しない (読むだけ) |
//! | `verify` | **実際に競合を起こして止まることを確かめる** | しない (使い捨ての一時領域だけ) |
//! | `uninstall` | 入れたものだけを綺麗に戻す | する |
//!
//! ## `guard` / `union` へ「どう」触っているか (実測で決まった形)
//!
//! `src/guard.rs` と `src/union.rs` は `main.rs` の `mod` 一覧に**居ない**。
//! 実体は `src/features/guard.rs` が `#[path]` で私有の `mod imp` として
//! 取り込んでおり、外へ出ているのは `cli_main` / `FEATURE` / `HELP` **だけ**。
//! つまり `guard::install()` や `union::install()` は、共有ファイルを触らずに
//! 呼ぶ手段が無い (実際に `use crate::{guard, union};` は
//! `E0432: no 'guard' in the root` で落ちた)。そこで:
//!
//! * **書き込みは `zai guard <sub>` へ回す。** これは
//!   [`crate::features::guard::cli_main`] として公開されているので、
//!   ユーザーが手で打つのと**同じ経路**を通る (嘘にならない)。
//!   `zai` 本体として動いているときはサブプロセスへ出して stdout を捕まえ、
//!   そうでないとき (テスト) は同じ関数を直接呼ぶ。
//! * **union は CLI に導入コマンドを持たない** (`zai merge-driver` は
//!   git が起動する側)。ので `.git/config` の登録と `.gitattributes` の
//!   管理ブロックだけはここで書く。**書式は `src/union.rs` と同じ**で
//!   なければ両者が互いのブロックを壊すため、下の「契約」節の定数を
//!   `include_str!` で突き合わせる番人テストを置いてある
//!   (`union_と同じ契約を持っている`)。
//! * **読み取りは全部こちらでやる。** 状態を下位コマンドの出力から
//!   parse すると、テストのときだけ経路が変わって「テストは緑なのに
//!   本番で効いていない」が起こる。git と実ファイルを直接見る。
//!
//! ## 設計
//!
//! * **[`Env`] で外界を全部受け取る。** 台帳の置き場・実行ファイルの場所・
//!   配線の有無を引数にしてあるので、テストは実 `~/.zaivern` に触らない。
//! * **冪等。** どの入口も 2 回打って同じ結果になる (`二回打っても同じ結果になる`)。
//! * **決定的。** 反復は `Vec` / `BTreeMap` だけ。`HashMap` の順序を出力へ漏らさない。
//! * **glob の解釈は git 自身に訊く。** `.gitattributes` が効いているかは
//!   `git check-attr merge -- <実在するパス>` で確かめる。
//! * **git のバージョン番号で機能を推定しない。**
//!   [`crate::conflict::merge_tree_available`] を通す (共通入口が既にある)。
//! * **パスの直書きゼロ。** 一時領域は [`std::env::temp_dir`]、
//!   台帳は [`crate::lease::store_dir`]、実行ファイルは
//!   [`std::env::current_exe`] から導出する。
//!
//! ## 統合担当への申し送り (この 2 行で繋がる)
//!
//! `src/cli.rs` は共有ファイルなので**こちらでは配線していない**。
//! 次の 2 行を足すと `zai czero …` が使えるようになる:
//!
//! ```ignore
//! // (1) is_cli_subcommand() の門へ:
//!             | "czero"
//! // (2) try_run_cli() の match へ:
//!         "czero" => crate::features::czero_init::cli_main(rest),
//! ```
//!
//! **(1) を忘れると窓が生える。** `zai` は知らない語を**ワークスペース指定**
//! として扱って GUI を起動するので、`zai czero doctor` が「czero という名前の
//! フォルダを開く」に化ける (`coedit` で実際に踏まれた罠。`src/cli.rs` の
//! `is_cli_subcommand` のコメントにも記録がある)。
//! 打鍵の要求は無い (`keybinds.rs` も `config.rs` も 1 バイトも要らない)。
//!
//! `guard` の既存パレット項目「ガード: このリポジトリを競合ゼロにする」とは
//! 守備範囲が違う (あちらはフックだけ、こちらは台帳 + フック + driver +
//! `.gitattributes` + 自己検査 + 実証)。**同じ操作への到達経路が 2 つあるのは
//! 望ましくない**ので、統合時にどちらか一方へ寄せてよい (こちらは
//! `zai guard init` をそのまま呼んでいるので、寄せても機能は減らない)。

use crate::i18n::{tr, trf};
use crate::{conflict, lease};
use std::path::{Path, PathBuf};

// ═══════════════════════════════════════════════════════════════════════════
//  1. 終了コードと契約
// ═══════════════════════════════════════════════════════════════════════════

/// すべて正常 (`init` が通った / `doctor` に ❌ が無い / `verify` が全部通った)。
pub const EXIT_OK: i32 = 0;
/// **守れていない。** `doctor` に ❌ が残っている / `verify` で止まらなかった。
/// CI はこれを落第として扱ってよい。
pub const EXIT_UNHEALTHY: i32 = 1;
/// 使い方の誤り (未知のサブコマンド・余分な引数)。
pub const EXIT_USAGE: i32 = 2;
/// 実行時のエラー (git が居ない・リポジトリではない・書き込めない)。
pub const EXIT_RUNTIME: i32 = 3;

// ── `src/guard.rs` と共有している契約 ───────────────────────────────────
// **どれも番人テスト `guard_と同じ契約を持っている` が `include_str!` で
//  突き合わせる。** guard 側が変えたらこちらのテストが落ちる (黙ってズレない)。

/// guard が関所を張るフック。`src/guard.rs` の `HOOKS` と同じ。
const HOOKS: &[&str] = &["pre-commit", "pre-applypatch", "pre-merge-commit"];
/// guard が生成したフックの目印 (版が違っても前方一致で自分のものと判る)。
const GUARD_MARKER: &str = "zaivern-guard:";
/// 元から居たフックの退避先の接尾辞。
const HOOK_PREV_SUFFIX: &str = ".zaivern-prev";
/// フック本文で `zai` の場所を持つ変数。
const HOOK_EXE_VAR: &str = "__zg_exe=";

// ── `src/union.rs` と共有している契約 ───────────────────────────────────
// **番人テストは `union_と同じ契約を持っている`。**

/// 登録するドライバ名と、それに渡す追加フラグ。`src/union.rs` の `DRIVERS` と同じ。
const UNION_DRIVERS: &[(&str, &str)] = &[
    ("zaivern-union", ""),
    ("zaivern-union-auto", "--auto"),
    ("zaivern-union-whole", "--whole"),
    ("zaivern-union-sorted", "--whole --sorted"),
];
/// `.gitattributes` の提案が当てるドライバ。**マーカ無しで効く唯一のもの。**
const UNION_AUTO: &str = "zaivern-union-auto";
/// `git config merge.<名前>.name` に書く説明。
const UNION_DESC: &str = "Zaivern: 追記どうしの衝突だけを自動で解決する";
/// 管理ブロックを探す目印 (説明文が変わっても拾えるよう前方一致で見る)。
const ATTR_BEGIN_KEY: &str = "# zaivern:union-managed-begin";
const ATTR_END_KEY: &str = "# zaivern:union-managed-end";
/// 実際に書く開始行。
const ATTR_BEGIN: &str =
    "# zaivern:union-managed-begin — Zaivern が管理します (行を足すならこのブロックの外へ)";
const ATTR_END: &str = "# zaivern:union-managed-end";
/// 一覧になりやすい拡張子。`src/union.rs` の `DEFAULT_PATTERNS` と同じ並び。
///
/// union の `suggest_attributes` は**中身を読んで**一覧かどうか判定するが、
/// あちらは外から呼べない。こちらは「そのパターンに当たる**追跡ファイルが
/// 実在する**」だけを条件にする — 粗いが、当たっても中身が一覧でなければ
/// ドライバ側が降りるので**安全側に倒れる**。
const LIST_PATTERNS: &[&str] = &["*.md", "*.toml", "*.txt", "*.json", "*.yaml", "*.yml"];

/// 既存の pre-commit フレームワークの目印。**`(名前, リポジトリ頂点からの相対パス)`。**
///
/// 見つけたら「共存できるか / 上書きしてしまうか」を出す。**黙って壊さない**のが
/// この機能で一番大事な性質なので、名前を増やすのはここへ 1 行足すだけにする。
const HOOK_FRAMEWORKS: &[(&str, &str)] = &[
    ("husky", ".husky"),
    ("pre-commit", ".pre-commit-config.yaml"),
    ("pre-commit", ".pre-commit-config.yml"),
    ("lefthook", "lefthook.yml"),
    ("lefthook", "lefthook.yaml"),
    ("lefthook", "lefthook.toml"),
    ("lefthook", ".lefthook.yml"),
    ("lefthook", ".lefthook.yaml"),
    ("overcommit", ".overcommit.yml"),
];

/// `.git/index` がこの大きさを超えたら「巨大」として扱う。
///
/// index の 1 件は概ね 62 バイト + パス長なので、32MiB は**おおよそ 30 万件**。
/// ここを超えると `git ls-files -- <パターン>` を 6 回叩く診断が体感で遅くなる。
/// **判定に使うのは「遅い可能性がある」という注記だけ**で、緑/赤は動かさない。
const HUGE_INDEX_BYTES: u64 = 32 * 1024 * 1024;

// **ネットワーク FS を所要時間から当てるのはやめた (撤回の記録)。**
//
// 「下見が 2 秒を超えたら遅い置き場」という注記を入れて実測したら、
// **ローカルの tmpfs に作った空リポジトリでも毎回 1.7〜2.3 秒**出た。
// 内訳は形態の下見ではなく、`shellenv::resolve_path` が最初の 1 回だけ
// **ログイン + 対話シェルを起こして PATH を解決する**コスト (プロセス
// 全体で 1 回・OnceLock で共有) だった。git 呼び出し自体は 1 回 30〜64ms。
//
// つまりこの数字は「リポジトリの置き場が遅いか」を 1 ミリも測っていない。
// 絶対時間で線を引くと必ず嘘をつく (CLAUDE.md) の実例なので、**判定ごと
// 撤回する**。巨大さは時間ではなく [`HUGE_INDEX_BYTES`] という構造の
// 大きさで見る。

/// `verify` が使い捨ての作業を作るときの名前の接頭辞。
/// 置き場は [`std::env::temp_dir`] 由来で、パスの直書きは 1 文字も無い。
const SCRATCH_PREFIX: &str = "zaivern-czero-verify";

/// `verify` が確保するリースの寿命 (秒)。検証は数秒で終わるが、遅いマシンで
/// 期限切れになると**検証自体が偽陰性になる**ので余裕を取る。
const VERIFY_TTL_SECS: u64 = 600;

/// `verify` が使う一覧の本文。union が「一覧」と認めるのに最低 3 要素が要る。
const VERIFY_BASE_LIST: &str = "alpha\nbravo\ncharlie\ndelta\n";
/// こちら側が足す 1 行。
const VERIFY_OURS_LINE: &str = "echo-ours\n";
/// 相手側が足す 1 行。
const VERIFY_THEIRS_LINE: &str = "foxtrot-theirs\n";
/// git が merge driver へ渡す `%L` (衝突マーカの長さ) の既定。
const VERIFY_MARKER_SIZE: &str = "7";

// ═══════════════════════════════════════════════════════════════════════════
//  2. 段 (Stage) と評価 (Mark)
// ═══════════════════════════════════════════════════════════════════════════

/// 競合ゼロを成り立たせている段。**丸めないための単位**でもある。
///
/// 「有効です」と 1 行だけ出すのは嘘になりやすい — 台帳だけ有効でフックが
/// 入っていない状態でも「有効」と言えてしまう。段ごとに出す。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// **リポジトリの形態**。sparse-checkout / LFS / submodule / bare / 非 git など、
    /// 「どのリポジトリでも動く」の反例になり得るもの。ここが判らないまま
    /// 下の段を緑にしても、**何を保証したのかが判らない**。
    Shape,
    /// この `zai` が CLI サブコマンドを受け付けるか (フックと driver の前提)。
    Wiring,
    /// 行域の台帳 (`~/.zaivern/leases/<キー>.json`)。
    Ledger,
    /// git フック (`pre-commit` ほか)。
    Hooks,
    /// union merge driver の `.git/config` 登録。
    Driver,
    /// `.gitattributes` の指定が**実際に効いている**か。
    Attributes,
    /// `git merge-tree --write-tree` が使えるか (一撃統合の前提)。
    MergeTree,
}

/// 段の一覧。**出力の並びはこれで固定**する (決定的)。
pub const STAGES: &[Stage] = &[
    Stage::Shape,
    Stage::Wiring,
    Stage::Ledger,
    Stage::Hooks,
    Stage::Driver,
    Stage::Attributes,
    Stage::MergeTree,
];

impl Stage {
    /// JSON に載る安定キー。**画面表記が変わっても機械の読み手を壊さない。**
    pub fn key(self) -> &'static str {
        match self {
            Stage::Shape => "shape",
            Stage::Wiring => "wiring",
            Stage::Ledger => "ledger",
            Stage::Hooks => "hooks",
            Stage::Driver => "driver",
            Stage::Attributes => "attributes",
            Stage::MergeTree => "merge_tree",
        }
    }

    /// 画面に出す見出し。**日本語の原文**を置く (表示時に [`tr`] を通す)。
    pub fn label(self) -> &'static str {
        match self {
            Stage::Shape => "リポジトリの形態",
            Stage::Wiring => "CLI の配線",
            Stage::Ledger => "行域の台帳",
            Stage::Hooks => "git フック",
            Stage::Driver => "union merge driver",
            Stage::Attributes => ".gitattributes",
            Stage::MergeTree => "git merge-tree",
        }
    }
}

/// 段の評価。**並びが重さの順** (`max` が最悪を返す)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Mark {
    /// 効いている。
    Ok,
    /// 効いてはいるが穴がある / 縮退している。
    Warn,
    /// 効いていない。
    Bad,
}

impl Mark {
    /// 行頭に出す記号。
    pub fn glyph(self) -> &'static str {
        match self {
            Mark::Ok => "✅",
            Mark::Warn => "⚠",
            Mark::Bad => "❌",
        }
    }

    /// JSON に載る安定キー。
    pub fn key(self) -> &'static str {
        match self {
            Mark::Ok => "ok",
            Mark::Warn => "warn",
            Mark::Bad => "bad",
        }
    }
}

/// 診断 1 行。**理由と直し方を必ず持たせる。**
///
/// 「❌ フック」だけ出しても、ユーザーは何をすればいいか判らないので
/// 機能ごと切る。`reason` は 1 行、`fix` はそのまま貼れるコマンド。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Finding {
    pub stage: Stage,
    pub mark: Mark,
    /// なぜそう判定したか (1 行)。
    pub reason: String,
    /// 直すためのコマンド。**要らないときは空**。
    pub fix: String,
}

impl Finding {
    fn ok(stage: Stage, reason: String) -> Finding {
        Finding {
            stage,
            mark: Mark::Ok,
            reason,
            fix: String::new(),
        }
    }
    fn warn(stage: Stage, reason: String, fix: &str) -> Finding {
        Finding {
            stage,
            mark: Mark::Warn,
            reason,
            fix: fix.to_string(),
        }
    }
    fn bad(stage: Stage, reason: String, fix: &str) -> Finding {
        Finding {
            stage,
            mark: Mark::Bad,
            reason,
            fix: fix.to_string(),
        }
    }
}

/// 段ごとの最悪評価。**段が 1 つも診断されていなければ ❌** (判らない = 守れていない)。
pub fn worst_by_stage(findings: &[Finding]) -> Vec<(Stage, Mark)> {
    STAGES
        .iter()
        .map(|&s| {
            let worst = findings
                .iter()
                .filter(|f| f.stage == s)
                .map(|f| f.mark)
                .max()
                .unwrap_or(Mark::Bad);
            (s, worst)
        })
        .collect()
}

/// この機能が実際に行った / 行う予定の操作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// 実際に書き換えた。
    Did,
    /// 既にそうなっていたので何もしていない (冪等)。
    AlreadyOk,
    /// `--dry-run` なので**書かずに**予定だけ出した。
    Planned,
    /// **わざと触らなかった** (他人の指定を壊さないため)。
    Skipped,
    /// できなかった。
    Failed,
}

impl Action {
    /// JSON に載る安定キー。
    pub fn key(self) -> &'static str {
        match self {
            Action::Did => "did",
            Action::AlreadyOk => "already",
            Action::Planned => "planned",
            Action::Skipped => "skipped",
            Action::Failed => "failed",
        }
    }

    /// 画面に出す短い語。
    pub fn label(self) -> &'static str {
        match self {
            Action::Did => "実施",
            Action::AlreadyOk => "既にそう",
            Action::Planned => "予定",
            Action::Skipped => "見送り",
            Action::Failed => "失敗",
        }
    }
}

/// `init` / `uninstall` が行った 1 段ぶんの操作。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Step {
    pub stage: Stage,
    pub action: Action,
    /// 何をした / 何を飛ばしたかの内訳 (1 行)。
    pub detail: String,
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. 外界 (Env) — テストが実 `~/.zaivern` に触れないための面
// ═══════════════════════════════════════════════════════════════════════════

/// この機能が触る外界を全部ここに集める。
///
/// **ハードコーディング禁止の実装形。** 台帳の置き場も実行ファイルの場所も
/// 引数で受けるので、テストは [`crate::test_util::unique_temp_dir`] 配下だけで
/// 完結する。[`Env::here`] だけが実環境 (`~/.zaivern` / `current_exe`) を見る。
#[derive(Clone, Debug)]
pub struct Env {
    /// 対象リポジトリを探し始める場所。
    pub start: PathBuf,
    /// 台帳の置き場 (既定は `~/.zaivern/leases`)。
    pub ledger_dir: PathBuf,
    /// merge driver へ埋め込む `zai` の場所。
    /// `None` なら [`std::env::current_exe`] から起こす (直書きしない)。
    pub exe: Option<PathBuf>,
    /// この `zai` が `guard` サブコマンドを受け付けるか。
    /// **false のままフックを設置すると、コミットのたびに GUI が起動して
    /// `git commit` が返ってこなくなる** (guard 側も同じ理由で拒否する)。
    pub wired_guard: bool,
    /// `merge-driver` サブコマンドを受け付けるか (union driver の前提)。
    pub wired_driver: bool,
    /// `czero` サブコマンドを受け付けるか (診断が案内するコマンドの前提)。
    pub wired_czero: bool,
    /// 書かずに予定だけ出す。
    pub dry_run: bool,
}

impl Env {
    /// 実環境の既定。**ここだけが `~/.zaivern` と `current_exe` を見る。**
    pub fn here(start: PathBuf) -> Env {
        Env {
            start,
            ledger_dir: lease::store_dir(),
            exe: None,
            wired_guard: crate::cli::is_cli_subcommand("guard"),
            wired_driver: crate::cli::is_cli_subcommand("merge-driver"),
            wired_czero: crate::cli::is_cli_subcommand("czero"),
            dry_run: false,
        }
    }

    /// このプロセスが**本物の `zai`** として動いているか。
    ///
    /// 真なら下位コマンドをサブプロセスへ出せる (stdout を捕まえられるので
    /// `--json` が汚れない)。偽 (= テストバイナリ) なら同じ `cli_main` を
    /// このプロセスで直接呼ぶ — **経路は同じで、捕捉だけ諦める**。
    fn is_real_zai(&self) -> bool {
        self.exe.is_none() && self.wired_guard
    }

    /// merge driver へ埋め込む `zai` の場所。
    fn driver_exe(&self) -> Result<PathBuf, String> {
        match &self.exe {
            Some(p) => Ok(p.clone()),
            None => std::env::current_exe().map_err(|e| {
                trf(
                    "実行ファイルの場所が判りません: {e}",
                    &[("e", e.to_string())],
                )
            }),
        }
    }

    /// 埋め込む先の `zai` が `merge-driver` として働けるか。
    ///
    /// 省略時はこのプロセスの配線で決まる。**明示されているときは実際に
    /// 走らせて確かめる** — ここを「指定されたなら大丈夫」で通すと、
    /// merge-driver を知らない実行ファイル (テストバイナリ等) を登録した
    /// まま `git merge` が**成功してしまい** (git は終了コード 0 を
    /// 「解決した」と読む)、片側の追記が黙って消える。
    fn driver_capable(&self) -> bool {
        match &self.exe {
            Some(p) => probe_driver(p),
            None => self.wired_driver,
        }
    }

    /// 台帳の置き場が実環境と同じか。
    ///
    /// **`verify` が実 `git commit` まで試せるかの条件。** フックが読むのは
    /// 常に実環境の台帳なので、こちらが scratch の台帳を使っていると
    /// 実コミットは止まらない (= 偽陰性になる)。
    fn ledger_is_real(&self) -> bool {
        self.ledger_dir == lease::store_dir()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 下位コマンドと git への薄い入口
// ═══════════════════════════════════════════════════════════════════════════

fn git(repo: &Path, args: &[&str]) -> Result<String, String> {
    crate::worktree::git_out(repo, args)
}

/// `zai guard <args>` を走らせて終了コードを返す。
///
/// **ユーザーが手で打つのと同じ経路**を通る ([`crate::features::guard::cli_main`])。
/// フックの設置は「既存のフックを退避して連鎖する」という安全側の処理を
/// 持っており、それをここで書き直すと必ずズレるので**呼ぶだけにする**。
fn guard_run(env: &Env, args: &[&str]) -> Result<i32, String> {
    if env.is_real_zai() {
        let exe = std::env::current_exe().map_err(|e| {
            trf(
                "実行ファイルの場所が判りません: {e}",
                &[("e", e.to_string())],
            )
        })?;
        let out = crate::procx::hidden_command(&exe)
            .arg("guard")
            .args(args)
            .output()
            .map_err(|e| trf("zai guard を起動できません: {e}", &[("e", e.to_string())]))?;
        return Ok(out.status.code().unwrap_or(EXIT_RUNTIME));
    }
    let argv: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
    Ok(crate::features::guard::cli_main(&argv))
}

/// このリポジトリの**実際の**フック置き場。
///
/// `core.hooksPath` と linked worktree の両方を git 自身に解かせる。
/// 自前で `.git/hooks` を組み立てると、husky を入れている環境
/// (`core.hooksPath=.husky`) で丸ごと外す。
fn hooks_dir(repo: &Path) -> Result<PathBuf, String> {
    let raw = git(repo, &["rev-parse", "--git-path", "hooks"])?;
    let p = PathBuf::from(raw.trim());
    Ok(if p.is_absolute() { p } else { repo.join(p) })
}

/// フック 1 本の状態。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HookState {
    /// zaivern のフックが入っている。
    Ours,
    /// 他人のフックが居る。
    Foreign,
    /// 何も無い。
    Missing,
}

/// フックの現状。**下位コマンドの出力を parse しない** — 実ファイルを読む。
struct Hooks {
    dir: PathBuf,
    /// `(名前, 状態, 退避が居るか, 埋め込まれた zai)`。並びは [`HOOKS`] のまま = 決定的。
    rows: Vec<(String, HookState, bool, Option<String>)>,
}

impl Hooks {
    fn names(&self, want: HookState) -> Vec<String> {
        self.rows
            .iter()
            .filter(|(_, s, _, _)| *s == want)
            .map(|(n, _, _, _)| n.clone())
            .collect()
    }
    fn chained(&self) -> Vec<String> {
        self.rows
            .iter()
            .filter(|(_, _, prev, _)| *prev)
            .map(|(n, _, _, _)| n.clone())
            .collect()
    }
}

fn read_hooks(repo: &Path) -> Result<Hooks, String> {
    let dir = hooks_dir(repo)?;
    let mut rows = Vec::new();
    for name in HOOKS {
        let path = dir.join(name);
        // 非 UTF-8 のフック (コンパイル済みバイナリ) もあり得るので lossy で読む。
        let text = std::fs::read(&path)
            .map(|b| String::from_utf8_lossy(&b).to_string())
            .ok();
        let (state, exe) = match &text {
            None => (HookState::Missing, None),
            Some(t) if t.contains(GUARD_MARKER) => (HookState::Ours, hook_exe_of(t)),
            Some(_) => (HookState::Foreign, None),
        };
        let prev = dir.join(format!("{name}{HOOK_PREV_SUFFIX}")).exists();
        rows.push(((*name).to_string(), state, prev, exe));
    }
    Ok(Hooks { dir, rows })
}

/// `git check-attr <name> -- <path>` の値。**glob の解釈を git 自身に訊く。**
///
/// 出力は `<path>: <name>: <value>` なので、最後の `": "` の後ろを取る
/// (パスに `: ` が入っていても壊れない)。
fn attr_value(repo: &Path, name: &str, probe: &str) -> String {
    let out = git(repo, &["check-attr", name, "--", probe]).unwrap_or_default();
    out.rsplit_once(": ")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default()
}

/// その値は「まだ誰も merge 指定を置いていない」か。
///
/// **自分が置いた指定は上書き可**、他人の指定は触らない。
pub fn attr_free(value: &str) -> bool {
    let v = value.trim();
    v.is_empty() || v == "unspecified" || v == "unset" || v.starts_with("zaivern-union")
}

/// パターンに実際に当たる追跡ファイルを 1 つ。**無ければ `None`。**
///
/// `git check-attr` はパスを要求するので、`*.md` のようなパターンを
/// そのまま渡すと「そのパターンという名前のファイル」を訊くことになる。
fn sample_path(root: &Path, pattern: &str) -> Option<String> {
    let out = git(root, &["ls-files", "-z", "--", pattern]).ok()?;
    out.split('\0').find(|s| !s.is_empty()).map(str::to_string)
}

/// その実行ファイルが本当に `zai merge-driver` として働けるか、**実際に走らせて**確かめる。
///
/// 「登録されたのだから大丈夫」で通すと、merge-driver を知らない実行ファイルを
/// git が起動し、**何もせず終了コード 0 を返す**。git はそれを「解決した」と
/// 読むので、片側の追記が黙って消える (実証がいちばん見逃してはいけない形)。
/// 使い方の本文に出る 2 つの語で判定する — どちらも他のコマンドの help には出ない。
fn probe_driver(exe: &Path) -> bool {
    let Ok(out) = crate::procx::hidden_command(exe)
        .arg("merge-driver")
        .arg("--help")
        .output()
    else {
        return false;
    };
    if !out.status.success() {
        return false;
    }
    let so = String::from_utf8_lossy(&out.stdout);
    so.contains("merge-driver") && so.contains("<base>")
}

/// POSIX sh の単一引用符クオート。`'` は `'\''` で閉じ直す。
///
/// **パスは環境ごとに全く違う** (日本語ユーザー名・空白・`$`・
/// Windows の `C:/Users/...`) ので、必ずここを通してから埋め込む。
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// パスを POSIX sh から見た形へ寄せる。
///
/// **Windows だけ** `\` を `/` へ替える。sh の引用の中で `\` は場合により
/// escape として食われるため。unix ではファイル名に `\` が入り得るので
/// **絶対に置換しない**。
fn sh_path(p: &Path) -> String {
    let raw = p.to_string_lossy().to_string();
    if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw
    }
}

/// POSIX sh の単一引用符を戻す ([`sh_quote`] の逆)。
///
/// フックと `.git/config` に埋め込まれた `zai` の場所を取り出して、
/// **その実行ファイルがまだそこに居るか**を確かめるために要る。
/// 居なくなっていると、フックは fail-open で黙って素通りするので、
/// ユーザーからは「入れたのに止まらない」に見える (診断で拾う穴)。
pub fn sh_unquote(s: &str) -> Option<String> {
    let mut chars = s.trim_start().chars();
    if chars.next() != Some('\'') {
        return None;
    }
    let mut out = String::new();
    loop {
        match chars.next()? {
            '\'' => {
                // `'\''` は「閉じて・リテラルの `'`・開き直す」の 4 文字。
                if chars.as_str().starts_with("\\''") {
                    out.push('\'');
                    chars.next();
                    chars.next();
                    chars.next();
                } else {
                    return Some(out);
                }
            }
            c => out.push(c),
        }
    }
}

/// フック本文から、埋め込まれた `zai` の場所を取り出す。
///
/// **改行を正規化してから探す。** Windows のチェックアウトは CRLF なので、
/// `lines()` へ渡す前に畳まないと末尾の `\r` が値に混ざる。
pub fn hook_exe_of(text: &str) -> Option<String> {
    text.replace("\r\n", "\n")
        .lines()
        .find_map(|l| l.trim().strip_prefix(HOOK_EXE_VAR).and_then(sh_unquote))
}

/// ドライバに登録するコマンド行。
fn driver_command(exe: &Path, flags: &str) -> String {
    let q = sh_quote(&sh_path(exe));
    if flags.is_empty() {
        format!("{q} merge-driver %O %A %B %L %P")
    } else {
        format!("{q} merge-driver {flags} %O %A %B %L %P")
    }
}

/// このリポジトリに union のドライバが登録済みか。
fn driver_installed(repo: &Path) -> bool {
    !registered_driver_command(repo).trim().is_empty()
}

fn registered_driver_command(repo: &Path) -> String {
    git(
        repo,
        &[
            "config",
            "--local",
            "--get",
            &format!("merge.{UNION_AUTO}.driver"),
        ],
    )
    .unwrap_or_default()
}

/// 4 種のドライバを `.git/config` へ登録する。**冪等** (同じ値を書くだけ)。
fn driver_install(repo: &Path, exe: &Path) -> Result<usize, String> {
    for (name, flags) in UNION_DRIVERS {
        git(
            repo,
            &[
                "config",
                "--local",
                &format!("merge.{name}.name"),
                UNION_DESC,
            ],
        )?;
        git(
            repo,
            &[
                "config",
                "--local",
                &format!("merge.{name}.driver"),
                &driver_command(exe, flags),
            ],
        )?;
    }
    Ok(UNION_DRIVERS.len())
}

/// 登録を解除する。**未登録でも失敗にしない** (解除は何度打っても同じ結果)。
fn driver_uninstall(repo: &Path) -> usize {
    for (name, _) in UNION_DRIVERS {
        let _ = git(
            repo,
            &[
                "config",
                "--local",
                "--remove-section",
                &format!("merge.{name}"),
            ],
        );
    }
    UNION_DRIVERS.len()
}

// ═══════════════════════════════════════════════════════════════════════════
//  4.5. リポジトリの形態 — 「どのリポジトリでも動く」の反例を先に数える
// ═══════════════════════════════════════════════════════════════════════════

/// このリポジトリが**どういう形をしているか**。
///
/// ## なぜ要るのか
///
/// 台帳・フック・driver・`.gitattributes` が全部緑でも、それは
/// **「普通の作業ツリーなら守られる」**しか言っていない。sparse-checkout で
/// 作業ツリーに無いパス、LFS のポインタ、submodule の中、bare リポジトリ —
/// どれも上の 4 段の外側にあり、**緑のまま素通りする**。
///
/// 「未検証」を「安全」と書かないために、形態を先に数えて
/// **何が保証され、何が保証されないか**を段として出す。
///
/// ## 集める側の約束
///
/// * git は**下見のあいだ最大 5 回**しか叩かない (遅い置き場で往復を増やさない)。
/// * どの項目も**失敗したら「判らない」に倒す** — 判らないものを緑にしない。
/// * パスの直書きは 1 文字も無い (すべて git と [`std::path`] 由来)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Shape {
    /// 診断を始めた場所 (`--repo` かカレント)。
    pub start: PathBuf,
    /// 作業ツリーの頂点。**bare と非 git では `None`。**
    pub root: Option<PathBuf>,
    /// このツリーの git ディレクトリ (linked worktree なら `.git/worktrees/<名>`)。
    pub git_dir: Option<PathBuf>,
    /// 共有の git ディレクトリ (`.git/config` と `hooks/` が居る側)。
    pub common_dir: Option<PathBuf>,
    /// bare リポジトリ (作業ツリーが無い)。
    pub bare: bool,
    /// linked worktree (`git worktree add` で切ったツリー)。
    pub linked_worktree: bool,
    /// submodule の中に居る。
    pub inside_submodule: bool,
    /// このリポジトリが抱えている submodule のパス (`.gitmodules` 由来・決定的)。
    pub submodules: Vec<String>,
    /// sparse-checkout が有効。
    pub sparse: bool,
    /// shallow clone (`.git/shallow` が居る)。
    pub shallow: bool,
    /// git-lfs のフィルタが設定されている。
    pub lfs: bool,
    /// **`filter=lfs` なのに `merge` が空いている**パターン (union を当てると壊れる)。
    pub lfs_unsafe: Vec<String>,
    /// `core.hooksPath` が設定されているときの値 (既定なら `None`)。
    pub hooks_path: Option<String>,
    /// 見つかった既存の pre-commit フレームワーク (重複なし・決定的)。
    pub frameworks: Vec<&'static str>,
    /// 頂点がシンボリックリンク越しだったときの実体。
    pub symlinked: Option<PathBuf>,
    /// 書けなかった場所の説明 (空なら書けた)。
    pub unwritable: Vec<String>,
    /// `.git/index` の大きさ (バイト)。0 は「読めなかった」。
    pub index_bytes: u64,
}

impl Shape {
    /// git リポジトリとして認識できたか。
    pub fn is_git(&self) -> bool {
        self.git_dir.is_some()
    }

    /// `git -C` の相手にする場所。**bare でも git は答えられる。**
    pub fn probe_dir(&self) -> &Path {
        self.root
            .as_deref()
            .or(self.git_dir.as_deref())
            .unwrap_or(&self.start)
    }

    /// 作業ツリーが無いときの理由 (`init` / `verify` がそのまま返す 1 行)。
    pub fn no_worktree_reason(&self) -> String {
        if self.bare {
            trf(
                "bare リポジトリなので作業ツリーがありません: {p} (czero は作業ツリーへ .gitattributes を書き、pre-commit で止めるので、bare には入りません。clone した作業ツリー側で実行してください)",
                &[("p", self.probe_dir().display().to_string())],
            )
        } else {
            trf(
                "git リポジトリではありません: {p} (czero init は git rev-parse --show-toplevel を要求します)",
                &[("p", self.start.display().to_string())],
            )
        }
    }

    /// 実証 ([`verify`]) が**扱えていない**形態の注記。空なら注記なし。
    ///
    /// 実証は使い捨てリポジトリで行うので、対象リポジトリ固有の形態は
    /// 1 つも通っていない。**「守られています」と出すときに、この行を
    /// 一緒に出さないと静かな嘘になる。**
    pub fn untested_notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.sparse {
            out.push(tr(
                "sparse-checkout — 実証は全ファイルが揃った使い捨てツリーで行っています",
            ));
        }
        if !self.lfs_unsafe.is_empty() {
            out.push(trf(
                "git-lfs — union が当たっているのに merge=lfs が無いパターンがあります: {list}",
                &[("list", self.lfs_unsafe.join(" "))],
            ));
        } else if self.lfs {
            out.push(tr("git-lfs — 実証は LFS を使っていません"));
        }
        if !self.submodules.is_empty() {
            out.push(trf(
                "submodule {n} 件 — 実証は submodule を作っていません (submodule は別リポジトリなので個別に導入が要ります)",
                &[("n", self.submodules.len().to_string())],
            ));
        }
        if self.linked_worktree {
            out.push(tr(
                "linked worktree — 実証は本体ツリーだけを作っています",
            ));
        }
        if !self.frameworks.is_empty() {
            out.push(trf(
                "既存のフックフレームワーク ({list}) — 実証は素のフックだけを試しています",
                &[("list", self.frameworks.join(" "))],
            ));
        }
        out
    }
}

/// `git rev-parse` の答えを 1 行ずつ。**失敗したら空** (判らないものは埋めない)。
fn rev_parse(start: &Path, args: &[&str]) -> Vec<String> {
    let mut v = vec!["rev-parse"];
    v.extend_from_slice(args);
    git(start, &v)
        .map(|s| s.lines().map(|l| l.trim().to_string()).collect())
        .unwrap_or_default()
}

/// 存在すれば実体へ寄せる。**比較する前に必ず通す。**
fn canon(p: PathBuf) -> PathBuf {
    p.canonicalize().unwrap_or(p)
}

/// git が返したパスを絶対に寄せる (相対なら `base` から解く)。
fn abs_from(base: &Path, raw: &str) -> Option<PathBuf> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    let p = PathBuf::from(t);
    Some(if p.is_absolute() { p } else { base.join(p) })
}

/// その場所へ**書けるか**を、消える一時ファイル 1 つで確かめる。
///
/// `metadata().permissions().readonly()` は unix では「誰も書けない」しか
/// 見ないので、他人の所有物 (0755) を「書ける」と答えてしまう。
/// **実際に作って消す**のが唯一正しい答えだが、作業ツリーへ置くと
/// `git add -A` に巻き込まれ得るので、**呼ぶ側は git ディレクトリだけ**にする。
fn probe_writable(dir: &Path) -> bool {
    let name = format!(
        ".zaivern-czero-probe-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    );
    let p = dir.join(name);
    match std::fs::write(&p, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&p);
            true
        }
        Err(_) => false,
    }
}

/// 作業ツリーの頂点が読み取り専用に見えるか。**作業ツリーには 1 バイトも書かない。**
///
/// 判るのは「誰も書けない」形 (`chmod 555` / 読み取り専用マウント / Windows の
/// 読み取り専用属性) まで。他人の所有物は判らないので、**偽陰性がある**ことを
/// `docs/czero-repo-shapes.md` に書いてある。
fn looks_readonly(dir: &Path) -> bool {
    std::fs::metadata(dir)
        .map(|m| m.permissions().readonly())
        .unwrap_or(false)
}

/// `.gitmodules` の `path = <値>` を並び順そのままで。**決定的で重複しない。**
fn submodule_paths(root: &Path) -> Vec<String> {
    let raw = std::fs::read_to_string(root.join(".gitmodules")).unwrap_or_default();
    let mut out: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").lines() {
        let t = line.trim();
        let Some(rest) = t.strip_prefix("path") else {
            continue;
        };
        let Some((_, v)) = rest.split_once('=') else {
            continue;
        };
        let v = v.trim().to_string();
        if !v.is_empty() && !out.contains(&v) {
            out.push(v);
        }
    }
    out
}

/// union を当てているのに `merge=lfs` が無いパターン。**当てると壊れる組。**
///
/// git-lfs が置く行は `filter=lfs diff=lfs merge=lfs -text` なので普段は
/// `merge` が埋まっていて [`attr_free`] が弾く。ところが `filter=lfs` だけを
/// 手で書いた `.gitattributes` があると `merge` は空きに見え、union が当たる。
/// **union はポインタ行を連結する**ので、その時点で LFS オブジェクトが壊れる。
/// `git config --show-scope --get-regexp` の出力から拾う値。
#[derive(Debug, Default, PartialEq, Eq)]
struct ScopedConfig {
    sparse: bool,
    hooks_path: Option<String>,
    /// **このリポジトリに書かれた** `filter.lfs.*` があるか。
    lfs_local: bool,
}

/// 上の出力を読む純関数。**git を起こさないのでテーブルで固定できる。**
///
/// 形は `<scope>\t<key> <value>`。`--show-scope` を付けない古い git や、
/// 取り違えた呼び出しでタブが無いときは**スコープ不明**として扱い、
/// LFS は数えない (fail-closed 側 = 「このリポジトリのものだ」と決めつけない)。
fn parse_scoped_config(out: &str) -> ScopedConfig {
    let mut c = ScopedConfig::default();
    // Windows のチェックアウト / パイプで CRLF が混ざりうる。
    for line in out.replace("\r\n", "\n").lines() {
        let (scope, rest) = match line.split_once('\t') {
            Some((s, r)) => (s.trim(), r),
            None => ("", line),
        };
        let (k, v) = rest.split_once(' ').unwrap_or((rest, ""));
        let key = k.to_ascii_lowercase();
        // **`_ =>` で残り全部を LFS に倒さない。** `git sparse-checkout init --cone`
        // が置く `core.sparseCheckoutCone` まで LFS と読んで、sparse なだけの
        // リポジトリに「git-lfs が居ます」と出した (実際に出た)。
        if key == "core.sparsecheckout" {
            c.sparse = v.trim().eq_ignore_ascii_case("true");
        } else if key == "core.hookspath" {
            c.hooks_path = Some(v.trim().to_string());
        } else if key.starts_with("filter.lfs.") {
            // `worktree` はリポジトリ固有の設定なので local と同じ扱い。
            c.lfs_local |= scope == "local" || scope == "worktree";
        }
    }
    c
}

/// `.gitattributes` に `filter=lfs` が書かれているか (= このリポジトリが LFS を使う)。
///
/// **設定 (`filter.lfs.*`) では判定できない。** git-lfs を 1 度でも入れた
/// マシンでは global に置かれるので、LFS を使っていないリポジトリまで真になる。
fn gitattributes_declares_lfs(root: &Path) -> bool {
    for rel in [".gitattributes", ".git/info/attributes"] {
        let Ok(raw) = std::fs::read_to_string(root.join(rel)) else {
            continue;
        };
        // Windows のチェックアウトは CRLF なので正規化してから探す。
        if raw
            .replace("\r\n", "\n")
            .lines()
            .any(|l| !l.trim_start().starts_with('#') && l.contains("filter=lfs"))
        {
            return true;
        }
    }
    false
}

fn lfs_unsafe_patterns(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for pat in managed_patterns(root)
        .into_iter()
        .chain(LIST_PATTERNS.iter().map(|p| (*p).to_string()))
    {
        if seen.contains(&pat) {
            continue;
        }
        seen.push(pat.clone());
        let Some(sample) = sample_path(root, &pat) else {
            continue;
        };
        if attr_value(root, "filter", &sample) != "lfs" {
            continue;
        }
        let merge = attr_value(root, "merge", &sample);
        if attr_free(&merge) {
            out.push(format!("{pat} ({sample})"));
        }
    }
    out
}

/// リポジトリの形態を下見する。**書き換えは git ディレクトリの一時ファイル 1 つだけ**
/// (作って即消す)。
pub fn read_shape(env: &Env) -> Shape {
    let start = env.start.clone();
    let mut sh = Shape {
        start: start.clone(),
        ..Shape::default()
    };

    // 1 回で 3 つ訊く (bare でも答えられる 3 つ)。
    let head = rev_parse(
        &start,
        &["--is-bare-repository", "--absolute-git-dir", "--git-common-dir"],
    );
    if head.is_empty() {
        return sh; // 非 git。ここから先は 1 回も git を叩かない。
    }
    sh.bare = head.first().map(|s| s == "true").unwrap_or(false);
    // **必ず正規形へ寄せてから比べる。** git は `--absolute-git-dir` を
    // 実体で、`--git-common-dir` を相対 (`.git`) で返すので、生のまま
    // 比べると `/var/…//x/.git` と `/private/var/…/x/.git` が別物になり、
    // **どこも linked worktree に見える** (実際にそう出た)。
    sh.git_dir = head.get(1).and_then(|s| abs_from(&start, s)).map(canon);
    sh.common_dir = head
        .get(2)
        .and_then(|s| abs_from(&start, s))
        .map(canon)
        .or_else(|| sh.git_dir.clone());
    sh.linked_worktree = match (&sh.git_dir, &sh.common_dir) {
        (Some(g), Some(c)) => g != c,
        _ => false,
    };
    if !sh.bare {
        sh.root = rev_parse(&start, &["--show-toplevel"])
            .first()
            .and_then(|s| abs_from(&start, s));
    }
    let probe = sh.probe_dir().to_path_buf();

    // submodule の中に居るか (git 自身に訊く — `.git` がファイルなのは
    // linked worktree でも同じなので、ポインタの中身で判別しない)。
    sh.inside_submodule = !rev_parse(&probe, &["--show-superproject-working-tree"])
        .first()
        .map(|s| s.trim().is_empty())
        .unwrap_or(true);

    // 設定は 1 回の `--get-regexp` でまとめて拾う (往復を増やさない)。
    //
    // **`--show-scope` を付けてスコープまで見る。** 素の `git config` は
    // system / global / local を全部混ぜて返すので、**git-lfs が入っている
    // マシンでは、LFS を 1 バイトも使っていないリポジトリまで「LFS が居る」
    // になる** (CI の ubuntu / macOS / windows ランナーは git-lfs 同梱なので
    // 3 つとも赤くなり、手元の macOS だけ緑という形で出た)。
    // `core.hooksPath` / `core.sparseCheckout` は global に置いても
    // **このリポジトリの挙動を変える**ので全スコープのまま見る。
    // `filter.lfs.*` が global にあるのは「git-lfs が入っている」だけの意味で、
    // **このリポジトリが LFS を使っているか**は `.gitattributes` が決める。
    let conf = git(
        &probe,
        &[
            "config",
            "--show-scope",
            "--get-regexp",
            "^(core\\.sparsecheckout|core\\.hookspath|filter\\.lfs\\.)",
        ],
    )
    .unwrap_or_default();
    let cfg = parse_scoped_config(&conf);
    sh.sparse = cfg.sparse;
    sh.hooks_path = cfg.hooks_path;
    sh.lfs |= cfg.lfs_local;
    if let Some(c) = &sh.common_dir {
        sh.shallow = c.join("shallow").exists();
        // **`info/sparse-checkout` の存在では判定しない。** git が見るのは
        // `core.sparseCheckout` だけで、一覧が残っていても無効なら全件出る。
        if !probe_writable(c) {
            sh.unwritable.push(trf(
                "git ディレクトリ {p}",
                &[("p", c.display().to_string())],
            ));
        }
    }
    sh.index_bytes = rev_parse(&probe, &["--git-path", "index"])
        .first()
        .and_then(|s| abs_from(&probe, s))
        .and_then(|p| std::fs::metadata(p).ok())
        .map(|m| m.len())
        .unwrap_or(0);

    if let Some(root) = sh.root.clone() {
        if looks_readonly(&root) {
            sh.unwritable.push(trf(
                "作業ツリー {p}",
                &[("p", root.display().to_string())],
            ));
        }
        if let Ok(real) = root.canonicalize() {
            if real != root {
                sh.symlinked = Some(real);
            }
        }
        sh.submodules = submodule_paths(&root);
        for (name, rel) in HOOK_FRAMEWORKS {
            if root.join(rel).exists() && !sh.frameworks.contains(name) {
                sh.frameworks.push(name);
            }
        }
        if sh.lfs || !managed_patterns(&root).is_empty() {
            sh.lfs_unsafe = lfs_unsafe_patterns(&root);
        // **「このリポジトリが LFS を使っている」の本当の証拠は `.gitattributes`。**
        // 設定は local スコープしか数えないので (global の `filter.lfs.*` は
        // 「git-lfs が入っている」だけの意味)、属性側でも拾っておく。
        // `merge=lfs` が正しく入っている普通の LFS リポジトリは
        // `lfs_unsafe` に載らないため、そこだけでは検出できない。
        sh.lfs |= gitattributes_declares_lfs(&root);
        }
    }
    sh
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. `.gitattributes` の管理ブロック
// ═══════════════════════════════════════════════════════════════════════════

/// 管理ブロックだけを抜いた本文と、使われている改行。
///
/// **既存の行は 1 つも壊さない。** 「開始行から末尾まで」を捨てると、
/// ブロックの後ろに人が足した行を巻き込んで消す
/// (`.gitattributes` を触るツールが一番嫌われる壊れ方)。
/// 終了行が手で消されていても、**こちらが書いた形の行しか捨てない**。
fn strip_block(old: &str) -> (String, &'static str) {
    let eol = if old.contains("\r\n") { "\r\n" } else { "\n" };
    let mut out = String::new();
    let mut skipping = false;
    for line in old.split_inclusive('\n') {
        let t = line.trim_end_matches(['\r', '\n']).trim();
        if !skipping {
            if t.starts_with(ATTR_BEGIN_KEY) {
                skipping = true;
                continue;
            }
            out.push_str(line);
            continue;
        }
        if t.starts_with(ATTR_END_KEY) {
            skipping = false;
            continue;
        }
        // 終了行が無いまま人の行に当たったら、そこで抜けるのをやめる。
        let ours = t.is_empty()
            || t.starts_with('#')
            || t.split_whitespace().any(|w| {
                w.strip_prefix("merge=")
                    .is_some_and(|d| d.starts_with("zaivern-union"))
            });
        if !ours {
            skipping = false;
            out.push_str(line);
        }
    }
    (out, eol)
}

/// 管理ブロックを書き直す。`lines` が空ならブロックごと消す。
fn write_attributes(root: &Path, patterns: &[String]) -> Result<(), String> {
    let path = root.join(".gitattributes");
    let old = std::fs::read_to_string(&path).unwrap_or_default();
    let (mut kept, eol) = strip_block(&old);
    if !patterns.is_empty() {
        if !kept.is_empty() && !kept.ends_with('\n') {
            kept.push_str(eol);
        }
        kept.push_str(ATTR_BEGIN);
        kept.push_str(eol);
        for p in patterns {
            kept.push_str(&format!("{p} merge={UNION_AUTO}"));
            kept.push_str(eol);
        }
        kept.push_str(ATTR_END);
        kept.push_str(eol);
    }
    if kept.trim().is_empty() {
        // 自分の行しか無かったなら、ファイルごと消す (痕跡を残さない)。
        if path.exists() {
            std::fs::remove_file(&path).map_err(|e| e.to_string())?;
        }
        return Ok(());
    }
    if kept == old {
        return Ok(()); // 中身が同じなら書かない (mtime を無駄に動かさない)
    }
    std::fs::write(&path, kept)
        .map_err(|e| trf(".gitattributes を書けません: {e}", &[("e", e.to_string())]))
}

/// `.gitattributes` へ足すべきパターンと、既存の指定があるので見送るパターン。
///
/// * **存在するファイルだけ**を対象にする (使われていないパターンを並べない)。
/// * 既に別の merge ドライバが当たっているパターンは**足さない**
///   (判定は `git check-attr` にやらせるので、glob の解釈がずれない)。
/// * **`filter=lfs` が当たっているパターンも足さない。** git-lfs は普段
///   `merge=lfs` も一緒に置くので上の条件で弾けるが、`filter=lfs` だけを手で
///   書いた `.gitattributes` があると `merge` は空きに見える。そこへ union を
///   当てると**ポインタ行を連結して LFS オブジェクトを壊す**ので、
///   フィルタ側も見る。
/// * 並びは [`LIST_PATTERNS`] のまま = 決定的。
fn plan_patterns(root: &Path) -> (Vec<String>, Vec<String>) {
    let (mut want, mut skip) = (Vec::new(), Vec::new());
    for pat in LIST_PATTERNS {
        let Some(sample) = sample_path(root, pat) else {
            continue; // 当たる追跡ファイルが無い = 書いても効果がない
        };
        let free = attr_free(&attr_value(root, "merge", &sample))
            && attr_value(root, "filter", &sample) != "lfs";
        if free {
            want.push((*pat).to_string());
        } else {
            skip.push((*pat).to_string());
        }
    }
    (want, skip)
}

/// `.gitattributes` の管理ブロックに載っているパターン。**決定的で重複しない。**
fn managed_patterns(root: &Path) -> Vec<String> {
    let raw = std::fs::read_to_string(root.join(".gitattributes")).unwrap_or_default();
    // Windows のチェックアウトは CRLF なので、必ず正規化してから行に割る。
    let mut out: Vec<String> = Vec::new();
    for line in raw.replace("\r\n", "\n").lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut it = t.split_whitespace();
        let Some(pat) = it.next() else { continue };
        let ours = it.any(|w| {
            w.strip_prefix("merge=")
                .is_some_and(|d| d.starts_with("zaivern-union"))
        });
        if ours && !out.iter().any(|p| p == pat) {
            out.push(pat.to_string());
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. init — 1 コマンドで守りを入れる
// ═══════════════════════════════════════════════════════════════════════════

/// `init` の結果。**何をしたか / 何を飛ばしたか / いま効いているか**の 3 つ。
#[derive(Clone, Debug)]
pub struct InitReport {
    /// 実際に触った作業ツリーの頂点。
    pub repo: PathBuf,
    /// 台帳のキー = **元のリポジトリのルート** (linked worktree でも 1 つに寄る)。
    pub ledger_key: PathBuf,
    /// 台帳ファイル。
    pub ledger: PathBuf,
    pub dry_run: bool,
    pub steps: Vec<Step>,
    /// 最後に必ず走らせる自己検査。**入れっぱなしで効いていないのが一番悪い。**
    pub findings: Vec<Finding>,
}

impl InitReport {
    /// ❌ が 1 つも無いか。
    pub fn healthy(&self) -> bool {
        self.findings.iter().all(|f| f.mark != Mark::Bad)
    }
}

/// 対象リポジトリの頂点。**linked worktree でもそのツリーの頂点**を返す。
fn repo_root(start: &Path) -> Result<PathBuf, String> {
    crate::worktree::repo_root(start)
}

/// 1 コマンドで守りを入れる。**冪等** — 2 回打っても同じ結果になる。
///
/// 順番に意味がある: 台帳 → フック → driver → `.gitattributes` → 自己検査。
/// フックは台帳を読むので台帳が先、`.gitattributes` は driver 名を書くので
/// driver が先。最後の自己検査は**省略できない**。
pub fn init(env: &Env) -> Result<InitReport, String> {
    let shape = read_shape(env);
    // **作業ツリーが無い形は、素の git のエラーで落とさない。** `git rev-parse` の
    // 「this operation must be run in a work tree」だけを見せられても、
    // ユーザーには何をすればいいか判らない (bare と非 git で直し方が違う)。
    let Some(repo) = shape.root.clone() else {
        return Err(shape.no_worktree_reason());
    };
    let roots = lease::roots_of(&repo);
    let store = lease::store_path_in(&env.ledger_dir, &roots.key);
    let mut steps: Vec<Step> = Vec::new();

    steps.push(step_ledger(env, &store));
    steps.push(step_hooks(env, &repo));
    let (driver, attrs) = steps_union(env, &repo);
    steps.push(driver);
    steps.push(attrs);
    steps.push(step_merge_tree(&repo));

    Ok(InitReport {
        repo: repo.clone(),
        ledger_key: roots.key,
        ledger: store,
        dry_run: env.dry_run,
        steps,
        // **入れた直後の形態も一緒に出す。** 入った段が緑でも、submodule や
        // sparse-checkout が居れば「どこまで守れたか」は緑の数と一致しない。
        findings: findings(env, &read_shape(env)),
    })
}

/// 2. 行域の台帳を有効にする。**有効化はファイルの存在**で表す
/// (使っていない人が払うコストが `stat` 1 回で済む)。
fn step_ledger(env: &Env, store: &Path) -> Step {
    if lease::enabled(store) {
        return Step {
            stage: Stage::Ledger,
            action: Action::AlreadyOk,
            detail: trf("既に有効です: {p}", &[("p", store.display().to_string())]),
        };
    }
    if env.dry_run {
        return Step {
            stage: Stage::Ledger,
            action: Action::Planned,
            detail: trf(
                "空の台帳を置きます: {p}",
                &[("p", store.display().to_string())],
            ),
        };
    }
    match lease::enable(store) {
        Ok(()) => Step {
            stage: Stage::Ledger,
            action: Action::Did,
            detail: trf("有効にしました: {p}", &[("p", store.display().to_string())]),
        },
        Err(e) => Step {
            stage: Stage::Ledger,
            action: Action::Failed,
            detail: e,
        },
    }
}

/// 3. git フックを設置する。**`zai guard init` をそのまま呼ぶ。**
///
/// 既存フックの退避と連鎖 (husky / lefthook / pre-commit framework との共存) は
/// guard 側の責務で、ここで書き直すと必ずズレる。
fn step_hooks(env: &Env, repo: &Path) -> Step {
    let before = read_hooks(repo);
    if !env.wired_guard {
        return Step {
            stage: Stage::Hooks,
            action: Action::Failed,
            detail: tr(
                "この zai は `guard` サブコマンドを受け付けません。設置するとコミットのたびに GUI が起動して git commit が返らなくなるので、設置しませんでした (src/cli.rs への配線が要ります)",
            ),
        };
    }
    if env.dry_run {
        let detail = match &before {
            Ok(h) => trf(
                "`zai guard init` を実行します — 新規 {miss} / 貼り直し {ok} / 退避して連鎖 {foreign}",
                &[
                    ("miss", join_or_none(&h.names(HookState::Missing))),
                    ("ok", join_or_none(&h.names(HookState::Ours))),
                    ("foreign", join_or_none(&h.names(HookState::Foreign))),
                ],
            ),
            Err(e) => e.clone(),
        };
        return Step {
            stage: Stage::Hooks,
            action: Action::Planned,
            detail,
        };
    }
    let repo_arg = repo.to_string_lossy().to_string();
    match guard_run(env, &["init", "--repo", &repo_arg]) {
        Ok(0) => {}
        Ok(code) => {
            return Step {
                stage: Stage::Hooks,
                action: Action::Failed,
                detail: trf(
                    "`zai guard init` が終了コード {c} で失敗しました",
                    &[("c", code.to_string())],
                ),
            }
        }
        Err(e) => {
            return Step {
                stage: Stage::Hooks,
                action: Action::Failed,
                detail: e,
            }
        }
    }
    match read_hooks(repo) {
        Ok(after) => {
            let was_ours = before
                .as_ref()
                .map(|h| h.names(HookState::Ours))
                .unwrap_or_default();
            let now_ours = after.names(HookState::Ours);
            let fresh: Vec<String> = now_ours
                .iter()
                .filter(|n| !was_ours.contains(n))
                .cloned()
                .collect();
            let blocked = after.names(HookState::Foreign);
            let action = if !blocked.is_empty() {
                Action::Skipped
            } else if fresh.is_empty() {
                Action::AlreadyOk
            } else {
                Action::Did
            };
            Step {
                stage: Stage::Hooks,
                action,
                detail: trf(
                    "設置済み {ok} / 新たに入った {fresh} / 元のフックを退避して連鎖 {chain} / 触らなかった {blocked} (置き場: {dir})",
                    &[
                        ("ok", join_or_none(&now_ours)),
                        ("fresh", join_or_none(&fresh)),
                        ("chain", join_or_none(&after.chained())),
                        ("blocked", join_or_none(&blocked)),
                        ("dir", after.dir.display().to_string()),
                    ],
                ),
            }
        }
        Err(e) => Step {
            stage: Stage::Hooks,
            action: Action::Failed,
            detail: e,
        },
    }
}

/// 4 と 5. merge driver の登録と `.gitattributes` への追記。
fn steps_union(env: &Env, repo: &Path) -> (Step, Step) {
    let root = match repo_root(repo) {
        Ok(r) => r,
        Err(e) => return (fail(Stage::Driver, &e), fail(Stage::Attributes, &e)),
    };
    let already = driver_installed(&root);
    let (want, skip) = plan_patterns(&root);
    if env.dry_run {
        let driver = Step {
            stage: Stage::Driver,
            action: if already {
                Action::AlreadyOk
            } else {
                Action::Planned
            },
            detail: if already {
                tr("既に .git/config へ登録済みです")
            } else {
                trf(
                    "{n} 種のドライバを .git/config へ登録します",
                    &[("n", UNION_DRIVERS.len().to_string())],
                )
            },
        };
        return (driver, attrs_step(Action::Planned, &want, &skip));
    }
    let exe = match env.driver_exe() {
        Ok(e) => e,
        Err(e) => return (fail(Stage::Driver, &e), fail(Stage::Attributes, &e)),
    };
    let driver = match driver_install(&root, &exe) {
        Ok(n) => Step {
            stage: Stage::Driver,
            action: if already {
                Action::AlreadyOk
            } else {
                Action::Did
            },
            detail: trf(
                "{n} 種のドライバを .git/config へ登録しました ({exe})",
                &[("n", n.to_string()), ("exe", sh_path(&exe))],
            ),
        },
        Err(e) => fail(Stage::Driver, &e),
    };
    // **既に管理ブロックに載っているパターンは残す。** 追跡ファイルが一時的に
    // 消えただけで指定を落とすと、次のコミットで衝突が戻る。
    let before = managed_patterns(&root);
    let mut all = before.clone();
    for p in &want {
        if !all.contains(p) {
            all.push(p.clone());
        }
    }
    all.sort();
    let attrs = match write_attributes(&root, &all) {
        Ok(()) => {
            // **2 回目に「実施」と出さない。** 冪等な操作が毎回「書き換えた」と
            // 言うと、報告を見ても本当に変わったのかが判らなくなる。
            let newly: Vec<String> = all
                .iter()
                .filter(|p| !before.contains(p))
                .cloned()
                .collect();
            let action = if !newly.is_empty() {
                Action::Did
            } else if !skip.is_empty() {
                Action::Skipped
            } else {
                Action::AlreadyOk
            };
            attrs_step(action, &all, &skip)
        }
        Err(e) => fail(Stage::Attributes, &e),
    };
    (driver, attrs)
}

fn attrs_step(action: Action, want: &[String], skip: &[String]) -> Step {
    let verb = if action == Action::Planned {
        "書く予定"
    } else {
        "書きました"
    };
    Step {
        stage: Stage::Attributes,
        action,
        detail: trf(
            "{n} 件を{verb} [{added}] / 既存の merge 指定があるので見送り {s} 件 [{skipped}]",
            &[
                ("n", want.len().to_string()),
                ("verb", tr(verb)),
                ("added", join_or_none(want)),
                ("s", skip.len().to_string()),
                ("skipped", join_or_none(skip)),
            ],
        ),
    }
}

fn fail(stage: Stage, detail: &str) -> Step {
    Step {
        stage,
        action: Action::Failed,
        detail: detail.to_string(),
    }
}

/// 6. `git merge-tree --write-tree` は**入れるものではない**ので、見るだけ。
fn step_merge_tree(repo: &Path) -> Step {
    if conflict::merge_tree_available(repo) {
        Step {
            stage: Stage::MergeTree,
            action: Action::AlreadyOk,
            detail: tr("この git は merge-tree --write-tree を持っています (一撃統合が使えます)"),
        }
    } else {
        Step {
            stage: Stage::MergeTree,
            action: Action::Skipped,
            detail: tr(
                "この git は merge-tree --write-tree を持っていません (一撃統合は縮退します)",
            ),
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. doctor — 効いているかを診断する
// ═══════════════════════════════════════════════════════════════════════════

/// `doctor` の結果。
#[derive(Clone, Debug)]
pub struct Doctor {
    pub repo: PathBuf,
    pub ledger: PathBuf,
    /// 診断したリポジトリの形態。**判定の前提そのもの**なので一緒に持つ。
    pub shape: Shape,
    /// 何か 1 つでも入っているか。**全部無いなら「未導入です」と正直に出す**
    /// (エラーにしない — 使っていない人を叱らない)。
    pub installed: bool,
    pub findings: Vec<Finding>,
}

impl Doctor {
    /// ❌ が 1 つも無いか。
    pub fn healthy(&self) -> bool {
        self.findings.iter().all(|f| f.mark != Mark::Bad)
    }
}

/// 段ごとに ✅ / ⚠ / ❌ と理由と直し方を出す。**書き換えは一切しない。**
pub fn doctor(env: &Env) -> Result<Doctor, String> {
    let shape = read_shape(env);
    // **非 git / bare でもエラーにしない。** ここで `git rev-parse` の失敗を
    // そのまま返すと、いちばん説明が要る場面 (`zai lease claim` は動くのに
    // `czero init` だけ失敗する) で 1 行のエラーしか出なくなる。
    let target = shape.root.clone().unwrap_or_else(|| shape.probe_dir().to_path_buf());
    let roots = lease::roots_of(&target);
    let store = lease::store_path_in(&env.ledger_dir, &roots.key);
    let hooks_in = read_hooks(&target)
        .map(|h| !h.names(HookState::Ours).is_empty())
        .unwrap_or(false);
    Ok(Doctor {
        installed: lease::enabled(&store) || hooks_in || driver_installed(&target),
        repo: target,
        ledger: store,
        findings: findings(env, &shape),
        shape,
    })
}

/// 段ごとの診断。**丸めない** — 段ぶん必ず 1 行以上出す。
///
/// 形態 ([`Shape`]) を先に見て、**作業ツリーが無い形 (非 git / bare) では
/// 下の段を「判らない」ではなく「この形では入らない」と理由付きで出す。**
/// 判らないものを緑にしないのと同じくらい、**判っている赤を無言にしない**
/// ことが大事なので、6 段ぶんの行は必ず埋める。
fn findings(env: &Env, sh: &Shape) -> Vec<Finding> {
    let mut out = check_shape(sh);
    out.push(check_wiring(env));
    let Some(repo) = sh.root.clone() else {
        out.extend(check_ledger(env, sh.probe_dir()));
        let why = sh.no_worktree_reason();
        for stage in [Stage::Hooks, Stage::Driver, Stage::Attributes] {
            out.push(Finding::bad(
                stage,
                why.clone(),
                "作業ツリーのある場所で: zai czero init",
            ));
        }
        out.push(if sh.is_git() {
            check_merge_tree(sh.probe_dir())
        } else {
            Finding::warn(
                Stage::MergeTree,
                tr("git リポジトリではないので merge-tree は試していません"),
                "",
            )
        });
        return out;
    };
    out.extend(check_ledger(env, &repo));
    out.extend(check_hooks(&repo));
    out.extend(check_driver(&repo));
    out.extend(check_attributes(&repo));
    out.push(check_merge_tree(&repo));
    out
}

/// リポジトリの形態。**「未検証」を「安全」と書かない**のがこの関数の全部。
///
/// 各形態について「何が保証され、何が保証されないか」を 1 行ずつ出す。
/// 詳細は `docs/czero-repo-shapes.md`。
fn check_shape(sh: &Shape) -> Vec<Finding> {
    let mut out = Vec::new();
    if !sh.is_git() {
        out.push(Finding::bad(
            Stage::Shape,
            trf(
                "git リポジトリではありません: {p} — zai lease claim はここでも成功し、カレント基準の台帳を作ります。つまり台帳に参加したエージェントだけは互いの行域を避けますが、フックが入らないので他のプロセス (人間の git commit・台帳を知らないエージェント) は 1 つも止まりません",
                &[("p", sh.start.display().to_string())],
            ),
            "git init するか、git リポジトリを --repo で指すか、強制が要らないなら zai lease list で台帳だけを見てください",
        ));
        return out;
    }
    if sh.bare {
        out.push(Finding::bad(
            Stage::Shape,
            trf(
                "bare リポジトリです: {p} — 作業ツリーが無いので .gitattributes も pre-commit も置けません。保証されるのは「clone した作業ツリー側で czero init を打てば守られる」ことだけです",
                &[("p", sh.probe_dir().display().to_string())],
            ),
            "作業ツリー側 (git clone した先) で: zai czero init",
        ));
        return out;
    }

    // ── 素の作業ツリー ─────────────────────────────────────────────
    let plain = !sh.linked_worktree
        && !sh.inside_submodule
        && !sh.sparse
        && !sh.shallow
        && !sh.lfs
        && sh.submodules.is_empty()
        && sh.lfs_unsafe.is_empty()
        && sh.frameworks.is_empty()
        && sh.hooks_path.is_none()
        && sh.symlinked.is_none()
        && sh.unwritable.is_empty();
    if plain {
        out.push(Finding::ok(
            Stage::Shape,
            trf(
                "素の作業ツリーです ({p}) — 下の段が緑ならこのツリーのコミットは全部フックを通ります",
                &[("p", sh.probe_dir().display().to_string())],
            ),
        ));
    }

    if sh.linked_worktree {
        out.push(Finding::ok(
            Stage::Shape,
            trf(
                "linked worktree です (共有 git ディレクトリ {c}) — merge driver とフックは共有側に入るので**本体と他の worktree にも同時に効きます**。台帳のキーも本体へ寄るので、worktree をまたいだ行域の取り合いを検出できます",
                &[(
                    "c",
                    sh.common_dir
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_default(),
                )],
            ),
        ));
    }
    if sh.inside_submodule {
        out.push(Finding::warn(
            Stage::Shape,
            tr("submodule の中で実行しています — ここへ入れた守りはこの submodule だけに効きます。親リポジトリは別のフックと別の .git/config を持つので、親側でもう一度 czero init が要ります"),
            "親リポジトリでも: zai czero init",
        ));
    }
    if !sh.submodules.is_empty() {
        out.push(Finding::warn(
            Stage::Shape,
            trf(
                "submodule を {n} 件抱えています ({list}) — submodule は別リポジトリなので、**この導入は 1 つも届きません**。submodule 内のコミットは素通りします",
                &[
                    ("n", sh.submodules.len().to_string()),
                    ("list", sh.submodules.join(" ")),
                ],
            ),
            "各 submodule で: zai czero init --repo <submodule のパス>",
        ));
    }
    if sh.sparse {
        out.push(Finding::warn(
            Stage::Shape,
            tr("sparse-checkout が有効です — 台帳とフックは stage されたパスを見るので効きます。効かないのは .gitattributes の側で、cone の外にある .gitattributes は作業ツリーに無く、czero init が書くのも頂点の 1 枚だけです (cone の外を後から checkout すると、そこだけ union が当たらないことがあります)"),
            "cone を広げてから: zai czero doctor",
        ));
    }
    if sh.shallow {
        out.push(Finding::warn(
            Stage::Shape,
            tr("shallow clone です — フックと台帳は効きますが、切り落とされた履歴に共通祖先がある組では git merge-tree が失敗し、coedit の一撃統合が縮退します"),
            "git fetch --unshallow",
        ));
    }
    if !sh.lfs_unsafe.is_empty() {
        out.push(Finding::bad(
            Stage::Shape,
            trf(
                "git-lfs のフィルタが当たっているのに merge 指定が空いているパターンがあります: {list} — union driver はポインタ行を連結するので、この組で衝突が起きると LFS オブジェクトが壊れます",
                &[("list", sh.lfs_unsafe.join(" / "))],
            ),
            "そのパターンへ merge=lfs を書いてから (git lfs track が既定で書きます): zai czero doctor",
        ));
    } else if sh.lfs {
        out.push(Finding::ok(
            Stage::Shape,
            tr("git-lfs が居ますが、union を当てているパターンとは重なっていません (LFS のパターンには merge=lfs が入っているので czero は避けます)"),
        ));
    }
    if let Some(hp) = &sh.hooks_path {
        out.push(Finding::warn(
            Stage::Shape,
            trf(
                "core.hooksPath が {hp} に設定されています — czero はこの置き場へ入れ、既に居るフックは .zaivern-prev へ退避して**連鎖**します (上書きしません)。ただしこの置き場を管理しているツールが自分でフックを書き直すと、こちらの関所が消えます",
                &[("hp", hp.clone())],
            ),
            "書き直したあとは必ず: zai czero doctor",
        ));
    }
    if !sh.frameworks.is_empty() {
        out.push(Finding::warn(
            Stage::Shape,
            trf(
                "既存の pre-commit フレームワークが居ます: {list} — 導入時は既存のフックを退避して連鎖するので**共存できます**。保証できないのは向こうが再インストールしたとき ( pre-commit install / husky install はフック本体を書き直すので、こちらの関所が黙って消えます)",
                &[("list", sh.frameworks.join(" "))],
            ),
            "向こうを入れ直したあとは必ず: zai czero doctor",
        ));
    }
    if let Some(real) = &sh.symlinked {
        out.push(Finding::ok(
            Stage::Shape,
            trf(
                "シンボリックリンク越しの頂点です (実体 {real}) — 台帳のキーは実体側へ寄せるので、リンク経由と実体経由で台帳が 2 つに割れることはありません。保証できないのは**リポジトリの中から外を指すシンボリックリンク**で、その先の書き込みは行域の管轄外です",
                &[("real", real.display().to_string())],
            ),
        ));
    }
    if !sh.unwritable.is_empty() {
        out.push(Finding::bad(
            Stage::Shape,
            trf(
                "書けない場所があります: {list} — 読み取り専用のチェックアウトでは czero init が失敗します (診断だけは動きます)",
                &[("list", sh.unwritable.join(" / "))],
            ),
            "書ける場所へ clone し直すか、権限を直してから: zai czero init",
        ));
    }
    if sh.index_bytes >= HUGE_INDEX_BYTES {
        out.push(Finding::warn(
            Stage::Shape,
            trf(
                "index が {mb} MiB あります (巨大リポジトリ) — 守りの強さは変わりませんが、診断が叩く git ls-files がパターンごとに全件を舐めるので待たされます",
                &[("mb", (sh.index_bytes / (1024 * 1024)).to_string())],
            ),
            "",
        ));
    }
    out
}


/// CLI の配線。**フックも merge driver も `zai <sub>` を叩くので、ここが
/// 通っていないと上の段が全部「入っているのに効かない」になる。**
fn check_wiring(env: &Env) -> Finding {
    let mut missing: Vec<&str> = Vec::new();
    if !env.wired_guard {
        missing.push("guard");
    }
    if !env.wired_driver {
        missing.push("merge-driver");
    }
    if !env.wired_czero {
        missing.push("czero");
    }
    if missing.is_empty() {
        return Finding::ok(
            Stage::Wiring,
            tr("guard / merge-driver / czero が CLI サブコマンドとして配線されています"),
        );
    }
    Finding::bad(
        Stage::Wiring,
        trf(
            "CLI に配線されていないサブコマンドがあります: {list} (zai は知らない語をワークスペース指定と解釈して GUI を起動します)",
            &[("list", missing.join(" "))],
        ),
        "統合担当へ: src/cli.rs の is_cli_subcommand と try_run_cli へ足してください",
    )
}

/// 台帳。**何件の担当が載っているか / 期限切れが残っていないか**まで出す。
fn check_ledger(env: &Env, repo: &Path) -> Vec<Finding> {
    let roots = lease::roots_of(repo);
    let store = lease::store_path_in(&env.ledger_dir, &roots.key);
    if !lease::enabled(&store) {
        return vec![Finding::bad(
            Stage::Ledger,
            trf(
                "台帳が無効です (このリポジトリでは行域の担当が 1 件も記録されません): {p}",
                &[("p", store.display().to_string())],
            ),
            "zai czero init",
        )];
    }
    let st = match lease::read_store(&store) {
        Ok(s) => s,
        Err(e) => {
            return vec![Finding::bad(
                Stage::Ledger,
                trf("台帳を読めません: {e}", &[("e", e)]),
                "壊れた台帳を消してから: zai czero init",
            )]
        }
    };
    let now = lease::now_secs();
    let alive = |p: u32| crate::instances::pid_alive(p);
    let stale = st.leases.iter().filter(|l| !l.active(now, &alive)).count();
    let mut out = vec![Finding::ok(
        Stage::Ledger,
        trf(
            "台帳は有効です (担当 {n} 件 / {p})",
            &[
                ("n", st.leases.len().to_string()),
                ("p", store.display().to_string()),
            ],
        ),
    )];
    if stale > 0 {
        out.push(Finding::warn(
            Stage::Ledger,
            trf(
                "期限切れの担当が {n} 件残っています (次の書き込みで落ちますが、それまで一覧に出ます)",
                &[("n", stale.to_string())],
            ),
            "zai lease list",
        ));
    }
    out
}

/// git フック。**設置済みでも中身が別物になっていないか**まで見る。
fn check_hooks(repo: &Path) -> Vec<Finding> {
    let h = match read_hooks(repo) {
        Ok(h) => h,
        Err(e) => {
            return vec![Finding::bad(
                Stage::Hooks,
                trf("フックの置き場が判りません: {e}", &[("e", e)]),
                "zai czero init",
            )]
        }
    };
    let mut out = Vec::new();
    let missing = h.names(HookState::Missing);
    if !missing.is_empty() {
        out.push(Finding::bad(
            Stage::Hooks,
            trf(
                "未設置のフックがあります: {list} (このフックを通る書き込みは素通りします)",
                &[("list", missing.join(" "))],
            ),
            "zai czero init",
        ));
    }
    let foreign = h.names(HookState::Foreign);
    if !foreign.is_empty() {
        out.push(Finding::warn(
            Stage::Hooks,
            trf(
                "他のツールのフックが居ます: {list} (設置時に退避して連鎖します — 上書きはしません)",
                &[("list", foreign.join(" "))],
            ),
            "zai czero init",
        ));
    }
    let ours = h.names(HookState::Ours);
    if !ours.is_empty() {
        out.push(Finding::ok(
            Stage::Hooks,
            trf(
                "設置済み: {list} (連鎖中 {chain} / 置き場 {dir})",
                &[
                    ("list", ours.join(" ")),
                    ("chain", join_or_none(&h.chained())),
                    ("dir", h.dir.display().to_string()),
                ],
            ),
        ));
    }
    // **設置済みでも中身が別物になっていないか。** 目印だけ見ても、埋め込まれた
    // `zai` が引っ越し / アンインストールで居なくなった場合を拾えない。
    // フックは fail-open なので**黙って素通り**する = ユーザーからは
    // 「入れたのに止まらない」に見える、一番悪い壊れ方。
    let mut dangling: Vec<String> = Vec::new();
    for (name, state, _, exe) in &h.rows {
        if *state != HookState::Ours {
            continue;
        }
        match exe {
            Some(p) if Path::new(p).exists() => {}
            Some(p) => dangling.push(format!("{name} → {p}")),
            None => dangling.push(format!("{name} → {}", tr("(zai の場所が読めません)"))),
        }
    }
    if !dangling.is_empty() {
        out.push(Finding::bad(
            Stage::Hooks,
            trf(
                "フックが指す zai が見つかりません: {list} (フックは fail-open なので黙って素通りします)",
                &[("list", dangling.join(" / "))],
            ),
            "zai czero init",
        ));
    }
    out
}

/// merge driver の `.git/config` 登録。**登録先の実行ファイルの実在まで見る。**
fn check_driver(repo: &Path) -> Vec<Finding> {
    let cmd = registered_driver_command(repo);
    if cmd.trim().is_empty() {
        return vec![Finding::bad(
            Stage::Driver,
            tr("union merge driver が .git/config に登録されていません (一覧への追記は毎回衝突します)"),
            "zai czero init",
        )];
    }
    match sh_unquote(&cmd) {
        Some(p) if Path::new(&p).exists() => vec![Finding::ok(
            Stage::Driver,
            trf(
                "{n} 種のドライバが登録済みです ({p})",
                &[("n", UNION_DRIVERS.len().to_string()), ("p", p)],
            ),
        )],
        Some(p) => vec![Finding::bad(
            Stage::Driver,
            trf(
                "登録されている zai が見つかりません: {p} (マージのたびに git がドライバの起動に失敗します)",
                &[("p", p)],
            ),
            "zai czero init",
        )],
        None => vec![Finding::warn(
            Stage::Driver,
            trf(
                "登録の書式を読めませんでした: {cmd}",
                &[("cmd", cmd.trim().to_string())],
            ),
            "zai czero init",
        )],
    }
}

/// `.gitattributes`。**効いているかは `git check-attr` に訊く** (glob の解釈は
/// git 自身にしか正しく判らない)。
fn check_attributes(repo: &Path) -> Vec<Finding> {
    let root = match repo_root(repo) {
        Ok(r) => r,
        Err(e) => {
            return vec![Finding::bad(
                Stage::Attributes,
                trf("リポジトリの頂点が判りません: {e}", &[("e", e)]),
                "zai czero init",
            )]
        }
    };
    let patterns = managed_patterns(&root);
    if patterns.is_empty() {
        return vec![Finding::bad(
            Stage::Attributes,
            tr(".gitattributes に union の指定がありません (driver を登録しても当たるファイルがゼロです)"),
            "zai czero init",
        )];
    }
    let mut live: Vec<String> = Vec::new();
    let mut dead: Vec<String> = Vec::new();
    let mut empty: Vec<String> = Vec::new();
    for pat in &patterns {
        match sample_path(&root, pat) {
            None => empty.push(pat.clone()),
            Some(sample) => {
                let v = attr_value(&root, "merge", &sample);
                if v.starts_with("zaivern-union") {
                    live.push(format!("{pat}→{v}"));
                } else {
                    dead.push(format!("{pat} ({sample}→{v})"));
                }
            }
        }
    }
    let mut out = Vec::new();
    if !live.is_empty() {
        out.push(Finding::ok(
            Stage::Attributes,
            trf(
                "{n} 件のパターンが実際に効いています (git check-attr で確認): {list}",
                &[("n", live.len().to_string()), ("list", live.join(" / "))],
            ),
        ));
    }
    if !dead.is_empty() {
        out.push(Finding::bad(
            Stage::Attributes,
            trf(
                "指定はあるのに効いていません: {list} (後ろの行や .git/info/attributes に上書きされています)",
                &[("list", dead.join(" / "))],
            ),
            "zai czero init",
        ));
    }
    if !empty.is_empty() {
        out.push(Finding::warn(
            Stage::Attributes,
            trf(
                "当たる追跡ファイルが 1 つも無いパターン: {list} (無害ですが効果もありません)",
                &[("list", empty.join(" "))],
            ),
            "zai czero init",
        ));
    }
    out
}

/// `git merge-tree --write-tree`。**バージョン番号で推定しない。**
fn check_merge_tree(repo: &Path) -> Finding {
    if conflict::merge_tree_available(repo) {
        Finding::ok(
            Stage::MergeTree,
            tr("merge-tree --write-tree が使えます (作業ツリーを汚さずに衝突ゼロを証明できます)"),
        )
    } else {
        Finding::warn(
            Stage::MergeTree,
            tr("この git は merge-tree --write-tree を持っていません (coedit の一撃統合が縮退します)"),
            "git を新しくしてください (この機能は git 2.38 で入りました)",
        )
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. verify — 本当に止まるかを実際に試す
// ═══════════════════════════════════════════════════════════════════════════

/// 1 つの実証の結果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// 期待どおりに守られた。
    Passed,
    /// **守られなかった。** ここが 1 つでもあれば守りは成立していない。
    Failed,
    /// この環境では試せなかった (理由を `detail` に出す)。**成功と数えない。**
    Skipped,
}

impl Outcome {
    /// 行頭に出す記号。
    pub fn glyph(self) -> &'static str {
        match self {
            Outcome::Passed => "✅",
            Outcome::Failed => "❌",
            Outcome::Skipped => "⚠",
        }
    }
    /// JSON に載る安定キー。
    pub fn key(self) -> &'static str {
        match self {
            Outcome::Passed => "passed",
            Outcome::Failed => "failed",
            Outcome::Skipped => "skipped",
        }
    }
}

/// 実証全体の**段**。
///
/// ## なぜ 2 値ではいけないか
///
/// 以前は「❌ が 1 つも無い」= 守られています、だった。ところが
/// [`trial_live_commit`] は本物の `zai` と実環境の台帳が揃っていないと
/// **黙って飛ぶ**ので、いちばん重い実地試行を 1 度も走らせないまま
/// 「守られています」と出せてしまう。**試していないものを「守られている」と
/// 言わない**ために、飛んだ試行がある状態には別の名前を与える。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 全部の試行を実際に起こして、全部通った。
    Verified,
    /// 落ちた試行は無いが、**試せなかった試行がある**。検証済みとは言わない。
    Partial,
    /// 守られていない試行がある。
    Broken,
}

impl Verdict {
    /// JSON に載る安定キー。
    pub fn key(self) -> &'static str {
        match self {
            Verdict::Verified => "verified",
            Verdict::Partial => "partial",
            Verdict::Broken => "broken",
        }
    }

    /// 画面に出す見出し。**「守られています」と言えるのは 1 つだけ。**
    pub fn headline(self) -> &'static str {
        match self {
            Verdict::Verified => {
                "🔬 実証 — 守られています (全ての試行を実際に起こして確かめました)"
            }
            Verdict::Partial => {
                "🔬 実証 — 部分的にしか確かめられていません (試せなかった試行か、実証が触れていない形態があるので「検証済み」とは言えません)"
            }
            Verdict::Broken => "🔬 実証 — ここが効いていません",
        }
    }
}

/// 実証 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Trial {
    /// 何を試したか (日本語の原文)。
    pub name: &'static str,
    pub outcome: Outcome,
    /// 実際に観測したこと。
    pub detail: String,
}

/// `verify` の結果。
#[derive(Clone, Debug)]
pub struct VerifyReport {
    /// 使い捨ての作業場所 (片付いていれば既に存在しない)。
    pub scratch: PathBuf,
    /// 片付いたか。**残すのは `--keep` を明示したときだけ。**
    pub cleaned: bool,
    pub trials: Vec<Trial>,
    /// **実証が扱えていない対象リポジトリの形態。**
    ///
    /// 試行は全部使い捨てリポジトリで起こすので、submodule や sparse-checkout
    /// のような対象固有の形は 1 つも通っていない。ここを黙ると
    /// 「守られています」が対象リポジトリ全体の保証に読める (静かな嘘)。
    pub shape_notes: Vec<String>,
}

impl VerifyReport {
    /// 守られていない試行が無いか。**「検証済み」ではない** — [`Self::verdict`] を見ること。
    pub fn protected(&self) -> bool {
        self.trials.iter().all(|t| t.outcome != Outcome::Failed)
    }

    /// 試せなかった試行。
    pub fn skipped(&self) -> Vec<&Trial> {
        self.trials
            .iter()
            .filter(|t| t.outcome == Outcome::Skipped)
            .collect()
    }

    /// 段。**飛んだ試行か、実証が触れていない形態があれば `Partial` 止まり。**
    ///
    /// 形態も数えるのは、試行が全部通っても**それは使い捨てリポジトリの話**
    /// だから。submodule を抱えたリポジトリで「守られています」と出すのは、
    /// 飛んだ試行を黙るのと同じ嘘になる (submodule 内は 1 つも守られていない)。
    pub fn verdict(&self) -> Verdict {
        if !self.protected() {
            Verdict::Broken
        } else if !self.skipped().is_empty() || !self.shape_notes.is_empty() {
            Verdict::Partial
        } else {
            Verdict::Verified
        }
    }
}

/// **実際に競合を起こして、止まることを確かめる。**
///
/// 設定を読むだけの診断 ([`doctor`]) と違い、こちらは実際に
/// 「他人の行域を取ろうとする」「他人の保有ファイルをコミットする」
/// 「一覧へ両側から追記する」を起こす。これが他社の導入ウィザードに無い
/// 部分で、**「入れたのに効いていない」を唯一つぶせる**手段でもある。
///
/// ## 対象リポジトリを 1 バイトも汚さない
///
/// すべて [`std::env::temp_dir`] 配下の使い捨てに作る。終わったら消す
/// (`keep` が真のときだけ残す — 落ちた原因を見るため)。
/// 実コミットまで試す段では実環境の台帳へ**一時的に** 1 件だけ書くが、
/// これも必ず消す ([`cleanup_live_ledger`])。
pub fn verify(env: &Env, keep: bool) -> VerifyReport {
    let scratch = std::env::temp_dir().join(format!(
        "{SCRATCH_PREFIX}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or_default()
    ));
    let ledger = scratch.join("ledger");
    // **順番がそのまま画面の順番**。`vec![]` で並べておくと、増やすときに
    // 途中へ差し込むのが 1 行で済む (push の列だと順序が見えにくくなる)。
    let mut trials = vec![
        run_trial(
            "同じファイルでも、離れた行なら 2 人が同時に持てる",
            trial_regions(),
        ),
        run_trial(
            "他人が保有するファイルへの書き込みを台帳が断る",
            trial_ledger_denies(&scratch, &ledger),
        ),
        run_trial(
            "一覧への両側追記を merge driver が解決する",
            trial_driver_resolves(&scratch),
        ),
        trial_live_commit(env, &scratch),
    ];
    trials.push(match env.driver_exe() {
        Ok(exe) if env.driver_capable() => run_trial(
            "一覧への両側追記が、実際の git merge で自動解決する",
            trial_union_live(&scratch, &exe),
        ),
        Ok(exe) => Trial {
            name: "一覧への両側追記が、実際の git merge で自動解決する",
            outcome: Outcome::Skipped,
            detail: trf(
                "この実行ファイルは merge-driver サブコマンドを受け付けないので、実マージは試せませんでした ({p} — src/cli.rs への配線が要ります)",
                &[("p", sh_path(&exe))],
            ),
        },
        Err(e) => Trial {
            name: "一覧への両側追記が、実際の git merge で自動解決する",
            outcome: Outcome::Skipped,
            detail: trf(
                "merge driver に埋める実行ファイルの場所が判らないので試せませんでした: {e}",
                &[("e", e)],
            ),
        },
    });

    // **必ず片付ける。** 作った台帳も一時ディレクトリも残さない。
    let cleaned = if keep {
        false
    } else {
        let _ = std::fs::remove_dir_all(&scratch);
        !scratch.exists()
    };
    VerifyReport {
        scratch,
        cleaned,
        trials,
        // **対象リポジトリの形は 1 つも試していない**ので、そう書く。
        shape_notes: read_shape(env).untested_notes(),
    }
}

fn run_trial(name: &'static str, r: Result<String, String>) -> Trial {
    match r {
        Ok(detail) => Trial {
            name,
            outcome: Outcome::Passed,
            detail,
        },
        Err(detail) => Trial {
            name,
            outcome: Outcome::Failed,
            detail,
        },
    }
}

/// 実証 1: 行域の相互排除。**I/O を持たない**ので、どの環境でも同じ答えが出る。
///
/// 「同じファイルは 1 人だけ」ではなく「**同じファイルでも離れた行なら 2 人**」
/// が、この製品が競合他社より 1 段細かいところ。両方を確かめる。
fn trial_regions() -> Result<String, String> {
    let mut store = lease::Store::default();
    let now = lease::now_secs();
    let alive = |_: u32| true;
    let a = holder("検証: 担当A", "czero-verify-a", "verify-a");
    let b = holder("検証: 担当B", "czero-verify-b", "verify-b");

    if let lease::Claim::Refused { owner, .. } = lease::try_claim(
        &mut store,
        &a,
        &["src/a.rs#L10-40".to_string()],
        now,
        VERIFY_TTL_SECS,
        &alive,
    ) {
        return Err(trf(
            "空の台帳で最初の確保が断られました (持ち主: {o})",
            &[("o", owner)],
        ));
    }
    // 重なる行域 → 断られること。
    if let lease::Claim::Granted(_) = lease::try_claim(
        &mut store,
        &b,
        &["src/a.rs#L20-30".to_string()],
        now,
        VERIFY_TTL_SECS,
        &alive,
    ) {
        return Err(tr(
            "重なる行域 (src/a.rs#L20-30) が確保できてしまいました — 行域の相互排除が効いていません",
        ));
    }
    // 離れた行域 → 通ること。**ここが通らないと並列度が落ちる。**
    match lease::try_claim(
        &mut store,
        &b,
        &["src/a.rs#L100-140".to_string()],
        now,
        VERIFY_TTL_SECS,
        &alive,
    ) {
        lease::Claim::Granted(_) => Ok(tr(
            "src/a.rs#L10-40 を持つ担当が居る状態で、#L20-30 は断られ、#L100-140 は通りました",
        )),
        lease::Claim::Refused { owner, pattern, .. } => Err(trf(
            "離れた行域 (src/a.rs#L100-140) が断られました (持ち主 {o} / {p}) — 並列度が落ちています",
            &[("o", owner), ("p", pattern)],
        )),
    }
}

fn holder(agent: &str, session: &str, cwd_leaf: &str) -> lease::Holder {
    lease::Holder {
        agent: agent.to_string(),
        session: session.to_string(),
        cwd: lease::normalize_path(&std::env::temp_dir().join(cwd_leaf).to_string_lossy()),
        pid: 0,
    }
}

/// 実証 2: **台帳が他人の書き込みを断る。**
///
/// フックが最後に呼ぶ判断 ([`lease::decide`]) を、実際に台帳ファイルへ
/// 書いた状態で走らせる。止まることだけでなく、**解放したら通ること**まで
/// 見る (通らないなら、止めていたのはリースではない = 検証になっていない)。
fn trial_ledger_denies(scratch: &Path, ledger: &Path) -> Result<String, String> {
    let repo = scratch.join("ledger-repo");
    make_repo(&repo, scratch)?;
    let roots = lease::roots_of(&repo);
    let store = lease::store_path_in(ledger, &roots.key);
    lease::enable(&store)?;
    let owner = holder("検証: 別の担当", "czero-verify-owner", "verify-owner");
    let me = holder("検証: 自分", "czero-verify-me", "verify-me");
    let claimed = lease::with_store(&store, |st| {
        lease::try_claim(
            st,
            &owner,
            &["shared.txt".to_string()],
            lease::now_secs(),
            VERIFY_TTL_SECS,
            &|_| true,
        )
    })?;
    if let lease::Claim::Refused { owner, .. } = claimed {
        return Err(trf(
            "検証用の確保が断られました (持ち主: {o})",
            &[("o", owner)],
        ));
    }
    let st = lease::read_store(&store)?;
    let now = lease::now_secs();
    let denied =
        match lease::decide(&st, &me, "shared.txt", now, &|_| true) {
            lease::Verdict::Deny(r) => r.lines().next().unwrap_or_default().to_string(),
            lease::Verdict::Allow => return Err(tr(
                "他人が保有する shared.txt への書き込みが通ってしまいました — 台帳が効いていません",
            )),
        };
    // 解放したら通ること。**ここが通らないなら、止めていたのはリースではない。**
    lease::with_store(&store, |s| lease::release(s, &owner))?;
    let st = lease::read_store(&store)?;
    match lease::decide(&st, &me, "shared.txt", now, &|_| true) {
        lease::Verdict::Allow => Ok(trf(
            "保有中は断られ ({r})、解放したら通りました",
            &[("r", denied)],
        )),
        lease::Verdict::Deny(_) => Err(tr(
            "担当を解放したのに、まだ断られます — 止めているのはリースではありません",
        )),
    }
}

/// 実証 3: **merge driver そのものを走らせる。**
///
/// git が起動するのと同じ入口 ([`crate::features::union::cli_main`]) へ、
/// git と同じ 5 引数 (`%O %A %B %L %P`) を渡す。結果は `%A` のパスへ
/// 上書きされ、完全解決なら終了コード 0。
fn trial_driver_resolves(scratch: &Path) -> Result<String, String> {
    let dir = scratch.join("driver");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let base = dir.join("base");
    let ours = dir.join("ours");
    let theirs = dir.join("theirs");
    write(&base, VERIFY_BASE_LIST)?;
    write(&ours, &format!("{VERIFY_BASE_LIST}{VERIFY_OURS_LINE}"))?;
    write(&theirs, &format!("{VERIFY_BASE_LIST}{VERIFY_THEIRS_LINE}"))?;
    let argv: Vec<String> = vec![
        "--auto".to_string(),
        base.to_string_lossy().to_string(),
        ours.to_string_lossy().to_string(),
        theirs.to_string_lossy().to_string(),
        VERIFY_MARKER_SIZE.to_string(),
        "list.txt".to_string(),
    ];
    let code = crate::features::union::cli_main(&argv);
    let text = std::fs::read_to_string(&ours).map_err(|e| e.to_string())?;
    if code != 0 {
        return Err(trf(
            "driver が終了コード {c} を返しました (衝突が残っています): {t}",
            &[("c", code.to_string()), ("t", text.replace('\n', "⏎"))],
        ));
    }
    if !text.contains(VERIFY_OURS_LINE.trim()) || !text.contains(VERIFY_THEIRS_LINE.trim()) {
        return Err(trf(
            "解決はしましたが、片側の追記が消えました: {t}",
            &[("t", text.replace('\n', "⏎"))],
        ));
    }
    Ok(trf(
        "両側の 1 行追記が両方残りました: {t}",
        &[("t", text.replace('\n', " / ").trim_end().to_string())],
    ))
}

/// 実証 4: **実際の `git commit` が止まる。**
///
/// フックを本当に設置し、他人の保有ファイルを stage して `git commit` する。
/// ここまでやらないと「フックは入っているが、中で何も起きていない」を
/// 見抜けない。
///
/// フックが読むのは**実環境の台帳**なので、台帳が使い捨てへ寄っているとき
/// (= テスト) は試せない。**その場合は「試せなかった」と正直に出す**
/// (通ったことにしない)。
fn trial_live_commit(env: &Env, scratch: &Path) -> Trial {
    const NAME: &str = "他人が保有するファイルの git commit が実際に止まる";
    // **なぜ飛ばすのかを 1 件ずつ挙げる。** 「揃っていません」とだけ出すと、
    // 読んだ人は自分の環境の何を直せばいいか判らないまま「まあ緑だし」で
    // 通す (この試行こそが「本当に守られるか」の唯一の実地試験なのに)。
    let mut why: Vec<String> = Vec::new();
    if !env.is_real_zai() {
        why.push(tr(
            "この実行ファイルが本物の zai ではありません (テストバイナリか、guard が CLI へ配線されていないビルド)",
        ));
    }
    if !env.ledger_is_real() {
        why.push(trf(
            "台帳が使い捨てへ寄っています ({here}) — フックが読むのは実環境の {real} なので、ここでコミットしても止まりません",
            &[
                ("here", env.ledger_dir.display().to_string()),
                ("real", lease::store_dir().display().to_string()),
            ],
        ));
    }
    if !why.is_empty() {
        return Trial {
            name: NAME,
            outcome: Outcome::Skipped,
            detail: trf(
                "実コミットを試せませんでした: {why}",
                &[("why", why.join(" / "))],
            ),
        };
    }
    let repo = scratch.join("commit");
    let store_holder = std::cell::RefCell::new(None::<(PathBuf, lease::Holder)>);
    let r = (|| -> Result<String, String> {
        make_repo(&repo, scratch)?;
        write(&repo.join("shared.txt"), VERIFY_BASE_LIST)?;
        git(&repo, &["add", "-A"])?;
        git(&repo, &["commit", "-m", "base", "--no-verify"])?;
        // フックを本当に置く (置き場は make_repo が逃がしているので戻す)。
        let _ = git(&repo, &["config", "--local", "--unset", "core.hooksPath"]);
        let repo_arg = repo.to_string_lossy().to_string();
        let code = guard_run(env, &["init", "--repo", &repo_arg])?;
        if code != 0 {
            return Err(trf(
                "`zai guard init` が終了コード {c} で失敗しました",
                &[("c", code.to_string())],
            ));
        }
        let roots = lease::roots_of(&repo);
        let store = lease::store_path_in(&env.ledger_dir, &roots.key);
        lease::enable(&store)?;
        let owner = holder("検証: 別の担当", "czero-verify-live", "verify-live");
        *store_holder.borrow_mut() = Some((store.clone(), owner.clone()));
        lease::with_store(&store, |st| {
            lease::try_claim(
                st,
                &owner,
                &["shared.txt".to_string()],
                lease::now_secs(),
                VERIFY_TTL_SECS,
                &|_| true,
            )
        })?;
        write(
            &repo.join("shared.txt"),
            &format!("{VERIFY_BASE_LIST}{VERIFY_OURS_LINE}"),
        )?;
        git(&repo, &["add", "-A"])?;
        match git(&repo, &["commit", "-m", "should be blocked"]) {
            Ok(_) => Err(tr(
                "他人が保有する shared.txt をコミットできてしまいました — フックが効いていません",
            )),
            Err(e) => {
                let first = e.lines().find(|l| !l.trim().is_empty()).unwrap_or_default();
                // 解放したら通ること (止めていたのがリースだと確かめる)。
                lease::with_store(&store, |s| lease::release(s, &owner))?;
                git(&repo, &["commit", "-m", "now allowed"])
                    .map_err(|e2| trf("担当を解放したのにコミットできません: {e}", &[("e", e2)]))?;
                Ok(trf(
                    "保有中は git commit が止まり ({r})、解放したら通りました",
                    &[("r", first.trim().to_string())],
                ))
            }
        }
    })();
    // **必ず片付ける。** 実環境の台帳へ書いた 1 件を残さない。
    if let Some((store, owner)) = store_holder.borrow().as_ref() {
        cleanup_live_ledger(store, owner);
    }
    run_trial(NAME, r)
}

/// 実証で実環境の台帳へ書いた分を消す。
///
/// **使い捨てリポジトリ 1 つぶんの台帳ファイルごと消す。** 他のリポジトリの
/// 台帳とはファイルが分かれている (キーが `workspace_key`) ので巻き込まない。
fn cleanup_live_ledger(store: &Path, owner: &lease::Holder) {
    let _ = lease::with_store(store, |s| lease::release(s, owner));
    let _ = std::fs::remove_file(store);
}

/// 実証 5: **実際の `git merge`。** git がドライバを起動するところまで通す。
///
/// 判定ロジック ([`trial_driver_resolves`]) が正しくても、`.git/config` の
/// 書式や `%O %A %B %L %P` の受け渡しが壊れていれば実マージは失敗する。
/// **そこは実際に git を走らせないと判らない。**
fn trial_union_live(scratch: &Path, exe: &Path) -> Result<String, String> {
    let repo = scratch.join("union");
    make_repo(&repo, scratch)?;
    let list = repo.join("list.txt");
    write(&list, VERIFY_BASE_LIST)?;
    driver_install(&repo, exe)?;
    write_attributes(&repo, &["*.txt".to_string()])?;
    git(&repo, &["add", "-A"])?;
    git(&repo, &["commit", "-m", "base", "--no-verify"])?;
    // 既定ブランチ名は環境設定で変わる (main / master / 任意)。**決め打ちしない。**
    let base = git(&repo, &["symbolic-ref", "--short", "HEAD"])?;

    git(&repo, &["checkout", "-b", "zaivern-verify-theirs"])?;
    write(&list, &format!("{VERIFY_BASE_LIST}{VERIFY_THEIRS_LINE}"))?;
    git(&repo, &["commit", "-am", "theirs", "--no-verify"])?;

    git(&repo, &["checkout", &base])?;
    write(&list, &format!("{VERIFY_BASE_LIST}{VERIFY_OURS_LINE}"))?;
    git(&repo, &["commit", "-am", "ours", "--no-verify"])?;

    git(&repo, &["merge", "--no-edit", "zaivern-verify-theirs"]).map_err(|e| {
        trf(
            "git merge が衝突しました (driver が起動していません): {e}",
            &[("e", e)],
        )
    })?;
    let text = std::fs::read_to_string(&list).map_err(|e| e.to_string())?;
    if text.contains("<<<<<<<") {
        return Err(tr("マージ後の本文に衝突マーカが残っています"));
    }
    if !text.contains(VERIFY_OURS_LINE.trim()) || !text.contains(VERIFY_THEIRS_LINE.trim()) {
        return Err(trf(
            "マージは通りましたが、片側の追記が消えました: {t}",
            &[("t", text.replace('\n', "⏎"))],
        ));
    }
    Ok(trf(
        "git merge が衝突なしで完了し、両側の追記が残りました: {t}",
        &[("t", text.replace('\n', " / ").trim_end().to_string())],
    ))
}

/// 使い捨ての git リポジトリを作る。
///
/// **ユーザーの global 設定に左右されないこと**が肝。署名の強制・
/// `core.hooksPath` の上書き・`gc` の割り込みは、検証を偽陰性にする。
/// フックの置き場は scratch 配下の空ディレクトリへ寄せる (パスの直書き無し)。
fn make_repo(repo: &Path, scratch: &Path) -> Result<(), String> {
    std::fs::create_dir_all(repo).map_err(|e| e.to_string())?;
    let nohooks = scratch.join("nohooks");
    std::fs::create_dir_all(&nohooks).map_err(|e| e.to_string())?;
    let hooks_path = sh_path(&nohooks);
    git(repo, &["init"])?;
    for (k, v) in [
        ("user.email", "czero-verify@example.invalid"),
        ("user.name", "Zaivern czero verify"),
        ("commit.gpgsign", "false"),
        ("tag.gpgsign", "false"),
        ("gc.auto", "0"),
        ("core.hooksPath", hooks_path.as_str()),
    ] {
        git(repo, &["config", "--local", k, v])?;
    }
    Ok(())
}

fn write(path: &Path, text: &str) -> Result<(), String> {
    std::fs::write(path, text).map_err(|e| {
        trf(
            "{p} を書けません: {e}",
            &[("p", path.display().to_string()), ("e", e.to_string())],
        )
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. uninstall — 綺麗に戻す
// ═══════════════════════════════════════════════════════════════════════════

/// `uninstall` の結果。
#[derive(Clone, Debug)]
pub struct UninstallReport {
    pub repo: PathBuf,
    pub steps: Vec<Step>,
}

/// **入れたものだけを戻す。**
///
/// * 退避してあった元のフックを復元する (`zai guard uninstall` がやる)。
/// * `.gitattributes` から**自分が書いた管理ブロックだけ**を抜く。
///   人が書いた行は 1 つも触らない。
/// * 台帳は、**まだ生きている担当が居るなら消さない**。他のエージェントが
///   その台帳を頼りに走っているので、消すと衝突検出が黙って死ぬ。
///   `purge` を明示したときだけ消す。
///
/// **入れたものを綺麗に戻せないツールは信用されない。**
pub fn uninstall(env: &Env, purge: bool) -> Result<UninstallReport, String> {
    let repo = repo_root(&env.start)?;
    let roots = lease::roots_of(&repo);
    let store = lease::store_path_in(&env.ledger_dir, &roots.key);
    let mut steps = Vec::new();

    let had_hooks = read_hooks(&repo)
        .map(|h| h.names(HookState::Ours))
        .unwrap_or_default();
    let repo_arg = repo.to_string_lossy().to_string();
    steps.push(match guard_run(env, &["uninstall", "--repo", &repo_arg]) {
        Ok(0) => {
            let after = read_hooks(&repo).ok();
            Step {
                stage: Stage::Hooks,
                action: if had_hooks.is_empty() {
                    Action::AlreadyOk
                } else {
                    Action::Did
                },
                detail: trf(
                    "消した {rm} / 残っている自分のフック {left} / 元のフックを戻した退避 {prev}",
                    &[
                        ("rm", join_or_none(&had_hooks)),
                        (
                            "left",
                            join_or_none(
                                &after
                                    .as_ref()
                                    .map(|h| h.names(HookState::Ours))
                                    .unwrap_or_default(),
                            ),
                        ),
                        (
                            "prev",
                            join_or_none(&after.as_ref().map(|h| h.chained()).unwrap_or_default()),
                        ),
                    ],
                ),
            }
        }
        Ok(code) => fail(
            Stage::Hooks,
            &trf(
                "`zai guard uninstall` が終了コード {c} で失敗しました",
                &[("c", code.to_string())],
            ),
        ),
        Err(e) => fail(Stage::Hooks, &e),
    });

    let had_driver = driver_installed(&repo);
    let n = driver_uninstall(&repo);
    steps.push(Step {
        stage: Stage::Driver,
        action: if had_driver {
            Action::Did
        } else {
            Action::AlreadyOk
        },
        detail: trf(
            "{n} 種のドライバの登録を解除しました",
            &[("n", n.to_string())],
        ),
    });

    let root = repo_root(&repo).unwrap_or_else(|_| repo.clone());
    let had_attrs = managed_patterns(&root);
    steps.push(match write_attributes(&root, &[]) {
        Ok(()) => Step {
            stage: Stage::Attributes,
            action: if had_attrs.is_empty() {
                Action::AlreadyOk
            } else {
                Action::Did
            },
            detail: trf(
                "管理ブロックを取り除きました ({n} 件 / 人が書いた行は触っていません)",
                &[("n", had_attrs.len().to_string())],
            ),
        },
        Err(e) => fail(Stage::Attributes, &e),
    });

    steps.push(step_drop_ledger(&store, purge));
    Ok(UninstallReport { repo, steps })
}

/// 台帳を落とす段。**生きている担当が居るなら消さない。**
fn step_drop_ledger(store: &Path, purge: bool) -> Step {
    if !lease::enabled(store) {
        return Step {
            stage: Stage::Ledger,
            action: Action::AlreadyOk,
            detail: tr("台帳は元から無効です"),
        };
    }
    let now = lease::now_secs();
    let alive = |p: u32| crate::instances::pid_alive(p);
    let live = lease::read_store(store)
        .map(|s| s.leases.iter().filter(|l| l.active(now, &alive)).count())
        .unwrap_or(0);
    if live > 0 && !purge {
        return Step {
            stage: Stage::Ledger,
            action: Action::Skipped,
            detail: trf(
                "まだ生きている担当が {n} 件あるので台帳は消しませんでした (消すには --purge)",
                &[("n", live.to_string())],
            ),
        };
    }
    match std::fs::remove_file(store) {
        Ok(()) => Step {
            stage: Stage::Ledger,
            action: Action::Did,
            detail: trf(
                "台帳を消しました: {p}",
                &[("p", store.display().to_string())],
            ),
        },
        Err(e) => Step {
            stage: Stage::Ledger,
            action: Action::Failed,
            detail: e.to_string(),
        },
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  10. 表示 (人が読む形 / JSON)
// ═══════════════════════════════════════════════════════════════════════════

/// 空のリストを「(なし)」にする。**空文字を出すと行が壊れて読めない。**
fn join_or_none(v: &[String]) -> String {
    if v.is_empty() {
        tr("(なし)")
    } else {
        v.join(" ")
    }
}

fn render_steps(steps: &[Step]) -> String {
    let mut out = String::new();
    for s in steps {
        out.push_str(&format!(
            "  [{}] {} — {}\n",
            tr(s.action.label()),
            tr(s.stage.label()),
            s.detail
        ));
    }
    out
}

fn render_findings(findings: &[Finding]) -> String {
    let mut out = String::new();
    for f in findings {
        out.push_str(&format!(
            "  {} {} — {}\n",
            f.mark.glyph(),
            tr(f.stage.label()),
            f.reason
        ));
        if !f.fix.is_empty() {
            out.push_str(&format!("      {} {}\n", tr("直し方:"), f.fix));
        }
    }
    out
}

/// 「6 段中 N 段が緑」。**これだけを出すのは禁止** (丸めると嘘になる) —
/// 必ず段ごとの内訳と一緒に出す。
fn green_count(findings: &[Finding]) -> String {
    let by = worst_by_stage(findings);
    let green = by.iter().filter(|(_, m)| *m == Mark::Ok).count();
    trf(
        "{n}/{all} 段が緑",
        &[("n", green.to_string()), ("all", by.len().to_string())],
    )
}

/// `init` の結果を人が読む形へ。
pub fn render_init(r: &InitReport) -> String {
    let head = if r.dry_run {
        tr("🚦 競合ゼロの導入 — 下見 (--dry-run: 1 バイトも書いていません)")
    } else {
        tr("🚦 競合ゼロの導入")
    };
    format!(
        "{head}\n{}\n\n{}\n{}\n{}\n{}",
        trf(
            "  リポジトリ: {repo}\n  台帳のキー: {key}",
            &[
                ("repo", r.repo.display().to_string()),
                ("key", r.ledger_key.display().to_string()),
            ],
        ),
        tr("やったこと:"),
        render_steps(&r.steps),
        trf(
            "いま効いているか (自己検査 — {g}):",
            &[("g", green_count(&r.findings))],
        ),
        render_findings(&r.findings),
    )
}

/// `doctor` の結果を人が読む形へ。
pub fn render_doctor(d: &Doctor) -> String {
    // **作業ツリーが無い形は「未導入」ではない。** 「入れれば入る」と読める
    // 見出しを出すと、入らない理由 (非 git / bare) が本文まで届かない。
    let head = if d.shape.root.is_none() {
        trf(
            "🩺 競合ゼロの診断 — {repo}\n  {why}",
            &[
                ("repo", d.repo.display().to_string()),
                ("why", d.shape.no_worktree_reason()),
            ],
        )
    } else if d.installed {
        trf(
            "🩺 競合ゼロの診断 — {repo} ({g})",
            &[
                ("repo", d.repo.display().to_string()),
                ("g", green_count(&d.findings)),
            ],
        )
    } else {
        trf(
            "🩺 競合ゼロの診断 — {repo}\n  このリポジトリはまだ未導入です (`zai czero init` で入ります)",
            &[("repo", d.repo.display().to_string())],
        )
    };
    format!("{head}\n{}", render_findings(&d.findings))
}

/// `verify` の結果を人が読む形へ。
///
/// **飛んだ試行と、実証が触れていない形態は、必ず見出しの直後に出す。**
/// 一覧の中に ⚠ として紛れさせると、下まで読まない人には
/// 「守られています」しか残らない。
pub fn render_verify(v: &VerifyReport) -> String {
    let verdict = v.verdict();
    let head = trf(
        "{h} ({p} 件が通り / {f} 件が落ち / {s} 件は試せず)",
        &[
            ("h", tr(verdict.headline())),
            (
                "p",
                v.trials
                    .iter()
                    .filter(|t| t.outcome == Outcome::Passed)
                    .count()
                    .to_string(),
            ),
            (
                "f",
                v.trials
                    .iter()
                    .filter(|t| t.outcome == Outcome::Failed)
                    .count()
                    .to_string(),
            ),
            ("s", v.skipped().len().to_string()),
        ],
    );
    let mut body = String::new();
    for t in v.skipped() {
        body.push_str(&format!(
            "  {} {}\n      {} {}\n",
            Outcome::Skipped.glyph(),
            tr("試せなかったので、この点は検証済みではありません:"),
            tr(t.name),
            t.detail
        ));
    }
    for n in &v.shape_notes {
        body.push_str(&format!(
            "  {} {} {}\n",
            Mark::Warn.glyph(),
            tr("実証は使い捨てリポジトリで行うので、この形態は試していません:"),
            n
        ));
    }
    for t in &v.trials {
        body.push_str(&format!("  {} {}\n", t.outcome.glyph(), tr(t.name)));
        body.push_str(&format!("      {}\n", t.detail));
    }
    let tail = if v.cleaned {
        tr("  一時領域は片付けました。")
    } else {
        trf(
            "  一時領域を残しました: {p}",
            &[("p", v.scratch.display().to_string())],
        )
    };
    format!("{head}\n{body}{tail}")
}

/// `uninstall` の結果を人が読む形へ。
pub fn render_uninstall(u: &UninstallReport) -> String {
    format!(
        "{}\n{}",
        trf(
            "🧹 競合ゼロの撤去 — {repo}",
            &[("repo", u.repo.display().to_string())],
        ),
        render_steps(&u.steps)
    )
}

fn steps_json(steps: &[Step]) -> serde_json::Value {
    serde_json::Value::Array(
        steps
            .iter()
            .map(|s| {
                serde_json::json!({
                    "stage": s.stage.key(),
                    "action": s.action.key(),
                    "detail": s.detail,
                })
            })
            .collect(),
    )
}

fn findings_json(findings: &[Finding]) -> serde_json::Value {
    serde_json::Value::Array(
        findings
            .iter()
            .map(|f| {
                serde_json::json!({
                    "stage": f.stage.key(),
                    "mark": f.mark.key(),
                    "reason": f.reason,
                    "fix": f.fix,
                })
            })
            .collect(),
    )
}

/// `init --json`。
pub fn init_json(r: &InitReport) -> serde_json::Value {
    serde_json::json!({
        "repo": r.repo.display().to_string(),
        "ledger_key": r.ledger_key.display().to_string(),
        "ledger": r.ledger.display().to_string(),
        "dry_run": r.dry_run,
        "steps": steps_json(&r.steps),
        "findings": findings_json(&r.findings),
        "healthy": r.healthy(),
    })
}

/// `doctor --json`。
pub fn doctor_json(d: &Doctor) -> serde_json::Value {
    serde_json::json!({
        "repo": d.repo.display().to_string(),
        "ledger": d.ledger.display().to_string(),
        "installed": d.installed,
        "findings": findings_json(&d.findings),
        "healthy": d.healthy(),
        "shape": shape_json(&d.shape),
    })
}

/// 形態を機械の読み手へ。**真偽値をそのまま出す** — 「普通です」に丸めない。
pub fn shape_json(sh: &Shape) -> serde_json::Value {
    serde_json::json!({
        "is_git": sh.is_git(),
        "root": sh.root.as_ref().map(|p| p.display().to_string()),
        "git_dir": sh.git_dir.as_ref().map(|p| p.display().to_string()),
        "common_dir": sh.common_dir.as_ref().map(|p| p.display().to_string()),
        "bare": sh.bare,
        "linked_worktree": sh.linked_worktree,
        "inside_submodule": sh.inside_submodule,
        "submodules": sh.submodules,
        "sparse_checkout": sh.sparse,
        "shallow": sh.shallow,
        "lfs": sh.lfs,
        "lfs_unsafe_patterns": sh.lfs_unsafe,
        "hooks_path": sh.hooks_path,
        "hook_frameworks": sh.frameworks,
        "symlinked_root_real": sh.symlinked.as_ref().map(|p| p.display().to_string()),
        "unwritable": sh.unwritable,
        "index_bytes": sh.index_bytes,
    })
}

/// `verify --json`。
pub fn verify_json(v: &VerifyReport) -> serde_json::Value {
    serde_json::json!({
        "scratch": v.scratch.display().to_string(),
        "cleaned": v.cleaned,
        // **`protected` だけを読む機械の読み手を騙さない。** 飛んだ試行があると
        // `protected` は真のままなので、`verdict` を必ず一緒に出す。
        "protected": v.protected(),
        "verdict": v.verdict().key(),
        "counts": {
            "passed": v.trials.iter().filter(|t| t.outcome == Outcome::Passed).count(),
            "failed": v.trials.iter().filter(|t| t.outcome == Outcome::Failed).count(),
            "skipped": v.skipped().len(),
        },
        "skipped": serde_json::Value::Array(
            v.skipped()
                .iter()
                .map(|t| serde_json::json!({ "name": t.name, "reason": t.detail }))
                .collect(),
        ),
        "untested_shapes": serde_json::Value::Array(
            v.shape_notes
                .iter()
                .map(|n| serde_json::Value::String(n.clone()))
                .collect(),
        ),
        "trials": serde_json::Value::Array(
            v.trials
                .iter()
                .map(|t| serde_json::json!({
                    "name": t.name,
                    "outcome": t.outcome.key(),
                    "detail": t.detail,
                }))
                .collect(),
        ),
    })
}

/// `uninstall --json`。
pub fn uninstall_json(u: &UninstallReport) -> serde_json::Value {
    serde_json::json!({
        "repo": u.repo.display().to_string(),
        "steps": steps_json(&u.steps),
    })
}

// ═══════════════════════════════════════════════════════════════════════════
//  11. CLI
// ═══════════════════════════════════════════════════════════════════════════

/// `zai czero --help` の本文。
pub const HELP: &str = "\
czero (どのリポジトリでも競合が起きないようにする — 導入 / 診断 / 実証 / 撤去):
  zai czero init [--repo <パス>] [--dry-run] [--json]
        1 コマンドで守りを入れる (冪等)。台帳 → git フック → merge driver →
        .gitattributes の順に入れ、**最後に必ず自己検査する**。
        --dry-run は 1 バイトも書かずに予定だけ出す。
  zai czero doctor [--repo <パス>] [--json]
        段ごとに ✅ / ⚠ / ❌ と理由と直し方を出す (書き換えない)。
        未導入なら「未導入です」と 1 行で出す (エラーにしない)。
        **非 git / bare / sparse-checkout / LFS / submodule / linked worktree /
        既存 core.hooksPath / husky・pre-commit・lefthook も検出して、
        何が保証され何が保証されないかを出す** (非 git と bare は
        エラーではなく診断として出す — そこだけ他とエラーの扱いが違う)。
  zai czero verify [--repo <パス>] [--keep] [--strict] [--json]
        **実際に競合を起こして止まることを確かめる。**
        使い捨ての一時領域だけを使い、対象リポジトリは 1 バイトも汚さない。
        **試せなかった試行があるときは「検証済み」と言わない** —
        判定は verified / partial / broken の 3 段 (--json の verdict 欄)。
        --strict を付けると partial も落第 (終了コード 1) として扱う。
        --keep は落ちた原因を見るために一時領域を残す。
  zai czero uninstall [--repo <パス>] [--purge] [--json]
        入れたものだけを戻す。退避した元のフックを復元し、
        .gitattributes は自分が書いた管理ブロックだけを抜く。
        --purge を付けると台帳ファイルも消す (既定は、生きている担当が
        居るなら残す — 他のエージェントがそれを頼りに走っているため)。

終了コード:
  0  正常 (init が通った / doctor に ❌ が無い / verify が全部通った)
  1  守れていない (doctor に ❌ が残っている / verify で止まらなかった /
     --strict 付きの verify が partial だった)
  2  使い方の誤り
  3  実行時のエラー (git が居ない / 書き込めない / init と verify を
     作業ツリーの無い場所で打った)。**doctor だけは非 git / bare でも
     エラーにせず、何が動いて何が動かないかを出す。**

--repo を省くとカレントディレクトリから git rev-parse --show-toplevel で解決します。
init は内部で `zai guard init` を呼びます (ユーザーが手で打つのと同じ経路)。
";

/// `zai czero <sub>` の実体。argv は `"czero"` の**次**から渡される。
///
/// **`src/cli.rs` への配線はこちらでは行わない** (共有ファイルなので、
/// 統合担当が直列で 2 行入れる — モジュール冒頭の申し送りを参照)。
/// 配線されるまでこの関数は呼ばれないが、GUI のパレット項目
/// (`czero_init.run`) が同じ処理へ到達するので、機能そのものは死んでいない。
///
/// **`#[allow(dead_code)]` は付けない。** `src/features/czero_init.rs` の
/// `const _: fn(&[String]) -> i32 = cli_main;` が実際の参照になるので、
/// 警告は出ない。抑止属性を撒くと「作ったのに繋いでいない」の検出器
/// (CLAUDE.md) がその分だけ鈍る。
pub fn cli_main(argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", HELP.trim_end());
        return EXIT_OK;
    }
    let sub = argv.first().map(String::as_str).unwrap_or("");
    let rest: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };
    let (repo_opt, rest) = take_opt(rest, "--repo");
    let (json, rest) = take_flag(&rest, "--json");
    let (dry, rest) = take_flag(&rest, "--dry-run");
    let (keep, rest) = take_flag(&rest, "--keep");
    let (strict, rest) = take_flag(&rest, "--strict");
    let (purge, rest) = take_flag(&rest, "--purge");
    if let Some(x) = rest.first() {
        return usage(&trf("余分な引数です: {x}", &[("x", x.clone())]));
    }
    let start = match repo_opt {
        Some(d) => PathBuf::from(d),
        None => match std::env::current_dir() {
            Ok(d) => d,
            Err(e) => {
                return usage(&trf(
                    "カレントディレクトリが判りません: {e}",
                    &[("e", e.to_string())],
                ))
            }
        },
    };
    let mut env = Env::here(start);
    env.dry_run = dry;

    match sub {
        "init" => match init(&env) {
            Ok(r) => {
                emit(json, || init_json(&r), || render_init(&r));
                if r.healthy() {
                    EXIT_OK
                } else {
                    EXIT_UNHEALTHY
                }
            }
            Err(e) => runtime(&e),
        },
        "doctor" => match doctor(&env) {
            Ok(d) => {
                emit(json, || doctor_json(&d), || render_doctor(&d));
                if d.healthy() {
                    EXIT_OK
                } else {
                    EXIT_UNHEALTHY
                }
            }
            Err(e) => runtime(&e),
        },
        "verify" => {
            // **リポジトリの存在だけは先に確かめる** (`--repo` の打ち間違いを
            // 「守られています」と答えてしまわないため)。理由は形態から起こす
            // ので、bare と非 git で違う直し方が出る。
            let shape = read_shape(&env);
            if shape.root.is_none() {
                return runtime(&shape.no_worktree_reason());
            }
            let v = verify(&env, keep);
            emit(json, || verify_json(&v), || render_verify(&v));
            verify_exit(v.verdict(), strict)
        }
        "uninstall" => match uninstall(&env, purge) {
            Ok(u) => {
                emit(json, || uninstall_json(&u), || render_uninstall(&u));
                if u.steps.iter().any(|s| s.action == Action::Failed) {
                    EXIT_UNHEALTHY
                } else {
                    EXIT_OK
                }
            }
            Err(e) => runtime(&e),
        },
        "" => usage(&tr(
            "サブコマンドを指定してください (init / doctor / verify / uninstall)",
        )),
        other => usage(&trf(
            "知らないサブコマンドです: {x}",
            &[("x", other.to_string())],
        )),
    }
}

/// `verify` の終了コード。
///
/// **既定では partial を落第にしない** (飛んだ試行は失敗ではないので、
/// 既存の CI を突然赤くしない)。ただし出力は必ず「検証済みではない」と言うし、
/// `--strict` を付ければ落第にできる。
///
/// 写像を関数に切り出してあるのは、**テストが実 `~/.zaivern` を触らずに
/// 終了コードを固定できる**ようにするため (`cli_main` を通すと
/// [`Env::here`] が実環境の台帳を掴む)。
fn verify_exit(verdict: Verdict, strict: bool) -> i32 {
    match verdict {
        Verdict::Verified => EXIT_OK,
        Verdict::Partial if !strict => EXIT_OK,
        Verdict::Partial | Verdict::Broken => EXIT_UNHEALTHY,
    }
}

/// JSON と人が読む形の出し分け。**片方しか作らない** (診断は git を叩くので、
/// 使わないほうまで組み立てると無駄に遅くなる)。
fn emit(json: bool, as_json: impl FnOnce() -> serde_json::Value, as_text: impl FnOnce() -> String) {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&as_json()).unwrap_or_else(|_| "{}".into())
        );
    } else {
        println!("{}", as_text().trim_end());
    }
}

fn usage(msg: &str) -> i32 {
    eprintln!("{msg}\n\n{}", HELP.trim_end());
    EXIT_USAGE
}

fn runtime(msg: &str) -> i32 {
    eprintln!("{msg}");
    EXIT_RUNTIME
}

/// `--key <値>` を抜き、残りを返す。**値が無い `--key` は食わない**
/// (次の引数を巻き込んで消すと、誤りが「余分な引数」として出なくなる)。
fn take_opt(args: &[String], key: &str) -> (Option<String>, Vec<String>) {
    let mut value = None;
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        if args[i] == key && i + 1 < args.len() {
            value = Some(args[i + 1].clone());
            i += 2;
            continue;
        }
        rest.push(args[i].clone());
        i += 1;
    }
    (value, rest)
}

/// `--flag` を抜き、残りを返す。
fn take_flag(args: &[String], key: &str) -> (bool, Vec<String>) {
    let found = args.iter().any(|a| a == key);
    (found, args.iter().filter(|a| *a != key).cloned().collect())
}

// ═══════════════════════════════════════════════════════════════════════════
//  12. 機能レジストリ (GUI からの到達経路)
// ═══════════════════════════════════════════════════════════════════════════

/// パレットからの到達経路。
///
/// **`Feature` の欄はすべて明示する** (このブランチには `Feature::DEFAULT` が
/// まだ無い)。打鍵は割り当てない — 導入は一度きりの操作なので、
/// パレット 1 経路で足りる (`keybinds.rs` を触らずに済むという副産物もある)。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "czero_init",
    entries: &[crate::feature::Entry {
        icon: "🚦",
        label: "競合ゼロ: このリポジトリに導入して自己診断する",
        id: "czero_init.run",
    }],
    dispatch: |_app, ctx, id| match id {
        "czero_init.run" => {
            open_panel(ctx.clone(), Job::Init);
            true
        }
        _ => false,
    },
    // 窓は中央ビューに属さないオーバーレイなので毎フレームここから描く。
    // **閉じているフレームは 1 命令も走らない** (設計原則 3)。
    draw: Some(draw),
    ..crate::feature::Feature::DEFAULT
};

/// 裏で走らせる作業。**UI スレッドでは 1 つも走らせない。**
#[derive(Clone, Copy, PartialEq, Eq)]
enum Job {
    Init,
    Doctor,
    Verify,
    Uninstall,
}

impl Job {
    fn running_label(self) -> &'static str {
        match self {
            Job::Init => "導入しています…",
            Job::Doctor => "診断しています…",
            Job::Verify => "実際に競合を起こして試しています…",
            Job::Uninstall => "撤去しています…",
        }
    }
}

#[derive(Default)]
struct Panel {
    open: bool,
    /// 走っている作業。**UI スレッドは絶対に待たない。**
    pending: Option<std::sync::mpsc::Receiver<String>>,
    title: String,
    body: String,
}

fn panel() -> &'static std::sync::Mutex<Panel> {
    static P: std::sync::OnceLock<std::sync::Mutex<Panel>> = std::sync::OnceLock::new();
    P.get_or_init(|| std::sync::Mutex::new(Panel::default()))
}

/// パレットの項目から呼ぶ入口。
///
/// **git はここで待たない。** このリポジトリでは `git branch --show-current`
/// が 6023ms 返らず、最悪フレームが 4376ms になった実測がある。
/// 導入は git を 20 回以上叩くので、UI スレッドで待てば必ず固まる。
fn open_panel(ctx: egui::Context, job: Job) {
    let Ok(mut p) = panel().lock() else { return };
    p.open = true;
    p.title = tr(job.running_label());
    p.body.clear();
    p.pending = Some(spawn(ctx, job));
}

fn spawn(ctx: egui::Context, job: Job) -> std::sync::mpsc::Receiver<String> {
    let (tx, rx) = std::sync::mpsc::channel();
    // 名前を付ける (パニックログとプロファイラで出所が判る)。
    // **起こせなかったときも rx をそのまま返す** — `tx` がクロージャごと落ちて
    // 受信側が Disconnected を見るので、窓が「実行中」のまま固まらない。
    let _ = std::thread::Builder::new()
        .name("zaivern-czero-init".into())
        .spawn(move || {
            let _ = tx.send(run_job(job));
            ctx.request_repaint();
        });
    rx
}

fn run_job(job: Job) -> String {
    let env = Env::here(lease::gui_workspace_root());
    match job {
        Job::Init => match init(&env) {
            Ok(r) => render_init(&r),
            Err(e) => e,
        },
        Job::Doctor => match doctor(&env) {
            Ok(d) => render_doctor(&d),
            Err(e) => e,
        },
        Job::Verify => render_verify(&verify(&env, false)),
        Job::Uninstall => match uninstall(&env, false) {
            Ok(u) => render_uninstall(&u),
            Err(e) => e,
        },
    }
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない。**
fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut p) = panel().lock() else { return };
    if !p.open {
        return;
    }
    if let Some(rx) = &p.pending {
        match rx.try_recv() {
            Ok(text) => {
                p.title = tr("🚦 競合ゼロ (導入・診断・実証)");
                p.body = text;
                p.pending = None;
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                // 結果を拾うためだけに軽く回す (アイドルへは戻る)。
                ctx.request_repaint_after(std::time::Duration::from_millis(120));
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                p.pending = None;
                if p.body.is_empty() {
                    p.body = tr("処理を起動できませんでした");
                }
            }
        }
    }
    let mut open = true;
    let mut job: Option<Job> = None;
    let busy = p.pending.is_some();
    let title = if p.title.is_empty() {
        tr("🚦 競合ゼロ (導入・診断・実証)")
    } else {
        p.title.clone()
    };
    egui::Window::new(title)
        // **題名から ID を切り離す。** egui の `Window` は既定で題名を ID に
        // 使うので、進捗表示で題名が変わるたびに位置と大きさを失う。
        .id(egui::Id::new("czero_init.panel"))
        .collapsible(false)
        .resizable(true)
        .default_width(680.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_max_width(ui.available_width());
            ui.label(tr(
                "台帳・git フック・merge driver・.gitattributes を 1 度に入れて、\
                 最後に必ず自己検査します。入れっぱなしで効いていないのが一番危ないので、\
                 「実際に試す」で本当に止まるところまで確かめられます。",
            ));
            ui.separator();
            if !p.body.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt("czero_init.body")
                    .max_height(360.0)
                    .show(ui, |ui| {
                        ui.label(&p.body);
                    });
                ui.separator();
            }
            // 狭い幅でも見切れないよう折り返す。
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!busy, |ui| {
                    if ui.button(tr("導入する")).clicked() {
                        job = Some(Job::Init);
                    }
                    if ui.button(tr("診断だけ")).clicked() {
                        job = Some(Job::Doctor);
                    }
                    if ui.button(tr("実際に試す")).clicked() {
                        job = Some(Job::Verify);
                    }
                    if ui.button(tr("元に戻す")).clicked() {
                        job = Some(Job::Uninstall);
                    }
                });
            });
        });
    if !open {
        p.open = false;
    }
    if let Some(j) = job {
        p.title = tr(j.running_label());
        p.body.clear();
        p.pending = Some(spawn(ctx.clone(), j));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  13. テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    /// テスト用の使い捨てリポジトリと [`Env`]。**実 `~/.zaivern` に触れない。**
    struct Fixture {
        dir: PathBuf,
        repo: PathBuf,
        env: Env,
    }

    impl Fixture {
        fn new(tag: &str) -> Fixture {
            let dir = unique_temp_dir("zaivern-czero-init-test", tag);
            let repo = dir.join("repo");
            make_repo(&repo, &dir).expect("使い捨てリポジトリを作れない");
            // フックの置き場を既定へ戻す (make_repo は検証用に空へ逃がしている)。
            let _ = git(&repo, &["config", "--local", "--unset", "core.hooksPath"]);
            std::fs::write(repo.join("notes.md"), VERIFY_BASE_LIST).unwrap();
            std::fs::write(repo.join("list.txt"), VERIFY_BASE_LIST).unwrap();
            git(&repo, &["add", "-A"]).unwrap();
            git(&repo, &["commit", "-m", "base", "--no-verify"]).unwrap();
            let env = Env {
                start: repo.clone(),
                ledger_dir: dir.join("ledger"),
                // driver に埋め込む「zai」は**実在するファイル**でなければ
                // 診断が「見つかりません」を出す。テストではこのテスト
                // バイナリ自身を指す (場所は current_exe 由来 = 直書き無し)。
                exe: Some(std::env::current_exe().expect("current_exe")),
                wired_guard: true,
                wired_driver: true,
                wired_czero: true,
                dry_run: false,
            };
            Fixture { dir, repo, env }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn stage_rows(findings: &[Finding], s: Stage) -> Vec<&Finding> {
        findings.iter().filter(|f| f.stage == s).collect()
    }

    fn step(steps: &[Step], s: Stage) -> &Step {
        steps.iter().find(|x| x.stage == s).expect("段が無い")
    }

    fn worst(findings: &[Finding], s: Stage) -> Mark {
        worst_by_stage(findings)
            .into_iter()
            .find(|(x, _)| *x == s)
            .map(|(_, m)| m)
            .unwrap_or(Mark::Bad)
    }

    // ─────────────── 契約 (guard / union とのズレを検出する番人) ───────────────

    /// `src/guard.rs` と共有している文字列がズレていないこと。
    ///
    /// guard の中身は外から呼べない (`features::guard` は `cli_main` /
    /// `FEATURE` / `HELP` しか出していない) ので、状態の読み取りだけは
    /// こちらで持つしかない。**持つなら、ズレたら落ちるようにする。**
    #[test]
    fn guard_と同じ契約を持っている() {
        let src = include_str!("guard.rs").replace("\r\n", "\n");
        for name in HOOKS {
            assert!(
                src.contains(&format!("\"{name}\"")),
                "guard.rs にフック名 {name} が無い (HOOKS が変わった?)"
            );
        }
        assert!(
            src.contains(&format!("MARKER_PREFIX: &str = \"{GUARD_MARKER}\"")),
            "guard.rs の MARKER_PREFIX が {GUARD_MARKER:?} でなくなった"
        );
        assert!(
            src.contains(&format!("PREV_SUFFIX: &str = \"{HOOK_PREV_SUFFIX}\"")),
            "guard.rs の PREV_SUFFIX が {HOOK_PREV_SUFFIX:?} でなくなった"
        );
        assert!(
            src.contains(HOOK_EXE_VAR),
            "guard.rs のフック本文に {HOOK_EXE_VAR} が無い (場所の読み出しが死ぬ)"
        );
    }

    /// `src/union.rs` と共有している文字列がズレていないこと。
    ///
    /// **ここがズレると両者が互いの `.gitattributes` ブロックを壊す。**
    #[test]
    fn union_と同じ契約を持っている() {
        let src = include_str!("union.rs").replace("\r\n", "\n");
        for (name, flags) in UNION_DRIVERS {
            assert!(
                src.contains(&format!("(\"{name}\", \"{flags}\")"))
                    || src.contains(&format!("(AUTO_DRIVER, \"{flags}\")")),
                "union.rs の DRIVERS に ({name}, {flags}) が無い"
            );
        }
        assert!(
            src.contains(&format!("AUTO_DRIVER: &str = \"{UNION_AUTO}\"")),
            "union.rs の AUTO_DRIVER が {UNION_AUTO:?} でなくなった"
        );
        assert!(
            src.contains(&format!("ATTR_BEGIN_KEY: &str = \"{ATTR_BEGIN_KEY}\"")),
            "union.rs の管理ブロック開始目印が変わった"
        );
        assert!(
            src.contains(&format!("ATTR_END_KEY: &str = \"{ATTR_END_KEY}\"")),
            "union.rs の管理ブロック終了目印が変わった"
        );
        assert!(
            src.contains(&format!("DRIVER_DESC: &str = \"{UNION_DESC}\"")),
            "union.rs の DRIVER_DESC が変わった"
        );
        assert!(
            src.contains(&format!(
                "DEFAULT_PATTERNS: &str = \"{}\"",
                LIST_PATTERNS.join(" ")
            )),
            "union.rs の DEFAULT_PATTERNS と並びがズレた"
        );
        assert!(ATTR_BEGIN.starts_with(ATTR_BEGIN_KEY));
        assert!(ATTR_END.starts_with(ATTR_END_KEY));
    }

    // ─────────────── 段と評価 (純粋な部分) ───────────────

    #[test]
    fn 段のキーとラベルは重複しない() {
        let n = STAGES.len();
        let mut keys: Vec<&str> = STAGES.iter().map(|s| s.key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), n, "Stage::key が重複している");
        let mut labels: Vec<&str> = STAGES.iter().map(|s| s.label()).collect();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), n, "Stage::label が重複している");
    }

    #[test]
    fn 評価は重い順に並んでいる() {
        assert!(Mark::Ok < Mark::Warn && Mark::Warn < Mark::Bad);
        assert_eq!(
            [Mark::Ok, Mark::Bad, Mark::Warn].into_iter().max(),
            Some(Mark::Bad)
        );
        assert_eq!(Mark::Ok.glyph(), "✅");
        assert_eq!(Mark::Bad.glyph(), "❌");
        // 診断が 1 行も無い段は「判らない」= ❌ (緑に倒さない)
        assert_eq!(worst_by_stage(&[]).len(), STAGES.len());
        assert!(worst_by_stage(&[]).iter().all(|(_, m)| *m == Mark::Bad));
    }

    // ─────────────── sh の引用を戻す ───────────────

    #[test]
    fn sh引用を往復できる() {
        let base = std::env::temp_dir();
        let cases = [
            base.join("zai"),
            base.join("日本語のフォルダ").join("zai"),
            base.join("dir with space").join("zai"),
            base.join("it's here").join("zai"),
        ];
        for p in &cases {
            let raw = sh_path(p);
            let q = sh_quote(&raw);
            assert_eq!(
                sh_unquote(&q).as_deref(),
                Some(raw.as_str()),
                "往復できない"
            );
        }
        // 引用で始まらないものは None (誤って先頭を食わない)
        assert_eq!(sh_unquote("zai merge-driver"), None);
        assert_eq!(sh_unquote(""), None);
        // 閉じていないものも None (無限ループにならないこと)
        assert_eq!(sh_unquote("'abc"), None);
    }

    #[test]
    fn 引用の後ろに引数が続いても実行ファイルだけを取り出す() {
        let exe = std::env::temp_dir().join("zai bin").join("zai");
        let cmd = driver_command(&exe, "--auto");
        assert_eq!(sh_unquote(&cmd).as_deref(), Some(sh_path(&exe).as_str()));
        assert!(cmd.ends_with("merge-driver --auto %O %A %B %L %P"));
        assert!(driver_command(&exe, "").ends_with("merge-driver %O %A %B %L %P"));
    }

    #[test]
    fn フック本文から実行ファイルの場所を読める() {
        let text = format!(
            "#!/bin/sh\n{HOOK_EXE_VAR}{}\nexit 0\n",
            sh_quote("/opt/zai/zai")
        );
        assert_eq!(hook_exe_of(&text).as_deref(), Some("/opt/zai/zai"));
        // Windows のチェックアウト (CRLF) でも読めること。
        assert_eq!(
            hook_exe_of(&text.replace('\n', "\r\n")).as_deref(),
            Some("/opt/zai/zai")
        );
        assert_eq!(hook_exe_of("#!/bin/sh\nexit 0\n"), None);
    }

    #[test]
    fn 既存のmerge指定は空きと見なさない() {
        assert!(attr_free(""));
        assert!(attr_free("unspecified"));
        assert!(attr_free("unset"));
        assert!(attr_free(UNION_AUTO), "自分の指定は上書きしてよい");
        assert!(!attr_free("ours"));
        assert!(!attr_free("binary"));
        assert!(!attr_free("someone-else"));
    }

    // ─────────────── .gitattributes の管理ブロック ───────────────

    #[test]
    fn 管理ブロックは人の行を巻き込まない() {
        let old = format!(
            "*.png binary\n{ATTR_BEGIN}\n*.md merge={UNION_AUTO}\n{ATTR_END}\n# 人が後から足した\n*.log text\n"
        );
        let (kept, eol) = strip_block(&old);
        assert_eq!(eol, "\n");
        assert!(kept.contains("*.png binary"));
        assert!(kept.contains("# 人が後から足した"));
        assert!(kept.contains("*.log text"));
        assert!(!kept.contains(UNION_AUTO));
    }

    #[test]
    fn 終了行が消えていても人の行で止まる() {
        // 終了行だけ手で消された壊れ方。**後ろの人の行を巻き込まない。**
        let old = format!("{ATTR_BEGIN}\n*.md merge={UNION_AUTO}\n*.log text\n");
        let (kept, _) = strip_block(&old);
        assert!(kept.contains("*.log text"), "人の行を巻き込んだ: {kept:?}");
        assert!(!kept.contains(UNION_AUTO));
    }

    #[test]
    fn crlfのgitattributesでも壊れない() {
        let old =
            format!("*.png binary\r\n{ATTR_BEGIN}\r\n*.md merge={UNION_AUTO}\r\n{ATTR_END}\r\n");
        let (kept, eol) = strip_block(&old);
        assert_eq!(eol, "\r\n");
        assert!(kept.contains("*.png binary"));
        assert!(!kept.contains(UNION_AUTO));
    }

    // ─────────────── init ───────────────

    #[test]
    fn initは五段すべてを報告する() {
        let f = Fixture::new("init-stages");
        let r = init(&f.env).expect("init");
        let stages: Vec<Stage> = r.steps.iter().map(|s| s.stage).collect();
        assert_eq!(
            stages,
            vec![
                Stage::Ledger,
                Stage::Hooks,
                Stage::Driver,
                Stage::Attributes,
                Stage::MergeTree,
            ],
            "init の段が欠けている / 並びが変わった"
        );
        assert!(
            !r.findings.is_empty(),
            "init は最後に必ず自己検査すること (入れっぱなしで効いていないのが一番悪い)"
        );
    }

    #[test]
    fn initは台帳とフックとドライバを実際に入れる() {
        let f = Fixture::new("init-effect");
        let r = init(&f.env).expect("init");
        assert!(lease::enabled(&r.ledger), "台帳が有効になっていない");
        let h = read_hooks(&f.repo).expect("hooks");
        assert_eq!(
            h.names(HookState::Missing),
            Vec::<String>::new(),
            "未設置のフックが残っている"
        );
        assert!(driver_installed(&f.repo), "merge driver が登録されていない");
        let attrs = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap_or_default();
        assert!(
            attrs.contains(UNION_AUTO),
            ".gitattributes に union の指定が書かれていない: {attrs:?}"
        );
        // 実在するファイルのパターンだけ (yaml は無いので書かれない)
        assert!(attrs.contains("*.md"), "notes.md があるのに *.md が無い");
        assert!(attrs.contains("*.txt"), "list.txt があるのに *.txt が無い");
        assert!(
            !attrs.contains("*.yaml"),
            "実在しない *.yaml を書いた: {attrs:?}"
        );
    }

    #[test]
    fn 二回打っても同じ結果になる() {
        let f = Fixture::new("idempotent");
        let first = init(&f.env).expect("1 回目");
        let attrs1 = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap_or_default();
        let second = init(&f.env).expect("2 回目");
        let attrs2 = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap_or_default();
        assert_eq!(
            attrs1, attrs2,
            ".gitattributes が 2 回目で変わった (冪等でない)"
        );
        assert_eq!(
            first.findings.iter().map(|x| x.mark).collect::<Vec<_>>(),
            second.findings.iter().map(|x| x.mark).collect::<Vec<_>>(),
            "2 回目で診断の結果が変わった"
        );
        // 2 回目は「実施」が消え「既にそう」になる (= 何も書き換えていない)。
        assert_eq!(step(&second.steps, Stage::Ledger).action, Action::AlreadyOk);
        assert_eq!(step(&second.steps, Stage::Driver).action, Action::AlreadyOk);
        assert_eq!(step(&second.steps, Stage::Hooks).action, Action::AlreadyOk);
    }

    #[test]
    fn dry_runは一バイトも書かない() {
        let f = Fixture::new("dry-run");
        let mut env = f.env.clone();
        env.dry_run = true;
        let r = init(&env).expect("init --dry-run");
        assert!(r.dry_run);
        assert!(!lease::enabled(&r.ledger), "--dry-run なのに台帳を作った");
        assert!(
            !driver_installed(&f.repo),
            "--dry-run なのに driver を登録した"
        );
        assert!(
            !f.repo.join(".gitattributes").exists(),
            "--dry-run なのに .gitattributes を書いた"
        );
        let h = read_hooks(&f.repo).expect("hooks");
        assert_eq!(
            h.names(HookState::Ours),
            Vec::<String>::new(),
            "--dry-run なのにフックを設置した"
        );
        assert_eq!(step(&r.steps, Stage::Ledger).action, Action::Planned);
        assert_eq!(step(&r.steps, Stage::Hooks).action, Action::Planned);
        assert_eq!(step(&r.steps, Stage::Attributes).action, Action::Planned);
    }

    #[test]
    fn 既存のmerge指定があるパターンは飛ばして報告する() {
        let f = Fixture::new("respect-existing");
        // 人が先に `*.txt` へ別のドライバを当てている状態を作る。
        std::fs::write(f.repo.join(".gitattributes"), "*.txt merge=ours\n").unwrap();
        git(&f.repo, &["add", "-A"]).unwrap();
        git(&f.repo, &["commit", "-m", "existing", "--no-verify"]).unwrap();
        let r = init(&f.env).expect("init");
        let attrs = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap();
        assert!(
            attrs.contains("*.txt merge=ours"),
            "人が書いた行を消してしまった: {attrs:?}"
        );
        assert!(
            !attrs.contains(&format!("*.txt merge={UNION_AUTO}")),
            "既存の指定があるのに上書きした: {attrs:?}"
        );
        assert!(
            attrs.contains(&format!("*.md merge={UNION_AUTO}")),
            "空いている *.md まで諦めた"
        );
        // 飛ばしたことが報告に出る (黙って飛ばさない)。
        let s = step(&r.steps, Stage::Attributes);
        assert!(
            s.detail.contains("*.txt"),
            "飛ばしたパターンを報告していない: {}",
            s.detail
        );
    }

    #[test]
    fn 配線されていないzaiではフックを設置しない() {
        let f = Fixture::new("not-wired");
        let mut env = f.env.clone();
        env.wired_guard = false;
        let r = init(&env).expect("init");
        assert_eq!(step(&r.steps, Stage::Hooks).action, Action::Failed);
        let h = read_hooks(&f.repo).expect("hooks");
        assert_eq!(
            h.names(HookState::Ours),
            Vec::<String>::new(),
            "配線されていないのにフックを設置した (コミットのたびに GUI が起動する)"
        );
    }

    #[test]
    fn 既存のフックを壊さない() {
        let f = Fixture::new("chain-hook");
        let dir = hooks_dir(&f.repo).expect("hooks_dir");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("pre-commit");
        std::fs::write(&mine, "#!/bin/sh\n# 人が書いたフック\nexit 0\n").unwrap();
        init(&f.env).expect("init");
        let prev = dir.join(format!("pre-commit{HOOK_PREV_SUFFIX}"));
        assert!(prev.exists(), "既存のフックを退避していない");
        assert!(
            std::fs::read_to_string(&prev)
                .unwrap()
                .contains("人が書いたフック"),
            "退避の中身が違う"
        );
    }

    // ─────────────── doctor ───────────────

    #[test]
    fn 未導入なら未導入と正直に出しエラーにしない() {
        let f = Fixture::new("doctor-fresh");
        let d = doctor(&f.env).expect("doctor は未導入でも Err にしない");
        assert!(!d.installed, "何も入れていないのに導入済みと判定した");
        assert!(!d.healthy(), "未導入なら ❌ が立つべき");
        let text = render_doctor(&d);
        assert!(text.contains("未導入"), "未導入だと 1 行で判らない: {text}");
    }

    #[test]
    fn doctorは段ごとに理由と直し方を出す() {
        let f = Fixture::new("doctor-detail");
        let d = doctor(&f.env).expect("doctor");
        for s in STAGES {
            let rows = stage_rows(&d.findings, *s);
            assert!(!rows.is_empty(), "{s:?} の診断が出ていない (丸めている)");
            for r in rows {
                assert!(!r.reason.trim().is_empty(), "{s:?} の理由が空");
                if r.mark == Mark::Bad {
                    assert!(!r.fix.trim().is_empty(), "{s:?} の ❌ に直し方が無い");
                }
            }
        }
    }

    #[test]
    fn 導入後は台帳とフックとドライバが緑になる() {
        let f = Fixture::new("doctor-green");
        init(&f.env).expect("init");
        let d = doctor(&f.env).expect("doctor");
        for s in [
            Stage::Ledger,
            Stage::Hooks,
            Stage::Driver,
            Stage::Attributes,
        ] {
            assert_ne!(
                worst(&d.findings, s),
                Mark::Bad,
                "導入したのに {s:?} が ❌ のまま: {:#?}",
                stage_rows(&d.findings, s)
            );
        }
        assert!(d.installed);
    }

    #[test]
    fn フックが指すzaiが消えたら赤にする() {
        let f = Fixture::new("dangling-exe");
        init(&f.env).expect("init");
        // フックが指す zai が引っ越した / 消えた状態を作る。
        let dir = hooks_dir(&f.repo).expect("hooks_dir");
        let path = dir.join("pre-commit");
        let text = std::fs::read_to_string(&path).expect("フック");
        let ghost = f.dir.join("gone").join("zai");
        let old = hook_exe_of(&text).expect("元の場所");
        let text = text.replace(&sh_quote(&old), &sh_quote(&sh_path(&ghost)));
        std::fs::write(&path, &text).unwrap();

        let d = doctor(&f.env).expect("doctor");
        assert_eq!(
            worst(&d.findings, Stage::Hooks),
            Mark::Bad,
            "居なくなった zai を指すフックを緑と判定した (黙って素通りする一番悪い壊れ方): {:#?}",
            stage_rows(&d.findings, Stage::Hooks)
        );
    }

    #[test]
    fn 配線されていないと配線の段が赤になる() {
        let f = Fixture::new("wiring-red");
        let mut env = f.env.clone();
        env.wired_czero = false;
        let d = doctor(&env).expect("doctor");
        let w = stage_rows(&d.findings, Stage::Wiring);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].mark, Mark::Bad);
        assert!(w[0].reason.contains("czero"), "何が欠けているか出ていない");
    }

    #[test]
    fn 効いているかはgitのcheck_attrで確かめている() {
        let f = Fixture::new("attr-live");
        init(&f.env).expect("init");
        let d = doctor(&f.env).expect("doctor");
        let ok: Vec<&Finding> = stage_rows(&d.findings, Stage::Attributes)
            .into_iter()
            .filter(|x| x.mark == Mark::Ok)
            .collect();
        assert!(
            !ok.is_empty(),
            "効いている指定が 1 つも無い: {:#?}",
            d.findings
        );
        assert!(
            ok[0].reason.contains("check-attr"),
            "glob の解釈を git に訊いたことを言っていない: {}",
            ok[0].reason
        );
    }

    #[test]
    fn 指定はあるのに効いていないなら赤にする() {
        let f = Fixture::new("attr-dead");
        init(&f.env).expect("init");
        // 後ろから別のドライバで上書きする (`.gitattributes` は後勝ち)。
        let p = f.repo.join(".gitattributes");
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("*.md merge=ours\n");
        std::fs::write(&p, text).unwrap();
        let d = doctor(&f.env).expect("doctor");
        assert_eq!(
            worst(&d.findings, Stage::Attributes),
            Mark::Bad,
            "後ろで潰された指定を緑と判定した: {:#?}",
            stage_rows(&d.findings, Stage::Attributes)
        );
    }

    // ─────────────── リポジトリ形態 (穴 2 と穴 3) ───────────────

    /// 形態ごとの使い捨てリポジトリを置く場所。**実 `~/.zaivern` に触れない。**
    struct ShapeDir(PathBuf);

    impl ShapeDir {
        fn new(tag: &str) -> ShapeDir {
            ShapeDir(unique_temp_dir("zaivern-czero-shape-test", tag))
        }
        /// `start` を見に行く [`Env`]。台帳も実行ファイルもこのディレクトリ由来。
        fn env(&self, start: &Path) -> Env {
            Env {
                start: start.to_path_buf(),
                ledger_dir: self.0.join("ledger"),
                exe: Some(std::env::current_exe().expect("current_exe")),
                wired_guard: true,
                wired_driver: true,
                wired_czero: true,
                dry_run: false,
            }
        }
        /// コミットが 1 つある普通の作業ツリーを作る。
        fn repo(&self, name: &str) -> PathBuf {
            let repo = self.0.join(name);
            make_repo(&repo, &self.0).expect("使い捨てリポジトリ");
            let _ = git(&repo, &["config", "--local", "--unset", "core.hooksPath"]);
            std::fs::write(repo.join("notes.md"), VERIFY_BASE_LIST).unwrap();
            git(&repo, &["add", "-A"]).unwrap();
            git(&repo, &["commit", "-m", "base", "--no-verify"]).unwrap();
            repo
        }
    }

    impl Drop for ShapeDir {
        fn drop(&mut self) {
            // Windows は最後のハンドルが閉じるまで削除が保留になるので、失敗を許す。
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 段に、この語を含む理由の行が居ること。**部分一致で状態を決めない**ための
    /// 補助 (探すのは理由の本文であって、判定そのものではない)。
    fn reason_has(findings: &[Finding], s: Stage, needle: &str) -> bool {
        stage_rows(findings, s)
            .iter()
            .any(|f| f.reason.contains(needle))
    }

    /// **どの形態でも 6 段すべてに 1 行以上出ること。**
    ///
    /// 行が無い段は [`worst_by_stage`] が ❌ に倒すので、黙って赤くなる。
    /// 「判らない」を理由も無く赤にすると、直しようが無い診断になる。
    fn 全段が埋まっている(findings: &[Finding]) {
        for s in STAGES {
            assert!(
                !stage_rows(findings, *s).is_empty(),
                "{:?} の段に 1 行も出ていない",
                s
            );
        }
    }

    #[test]
    fn 非gitではleaseだけが動く非対称を説明する() {
        let d = ShapeDir::new("not-git");
        let plain = d.0.join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let env = d.env(&plain);

        let sh = read_shape(&env);
        assert!(!sh.is_git(), "git ではないのに git と判定した");
        assert!(sh.root.is_none() && !sh.bare);

        // doctor は**エラーにしない** — ここが穴 3 の本体。
        let doc = doctor(&env).expect("非 git でも診断は出ること");
        全段が埋まっている(&doc.findings);
        assert_eq!(worst(&doc.findings, Stage::Shape), Mark::Bad);
        assert!(
            reason_has(&doc.findings, Stage::Shape, "lease claim")
                && reason_has(&doc.findings, Stage::Shape, "フック"),
            "lease は動くのにフックが入らない、という非対称を説明していない: {:?}",
            stage_rows(&doc.findings, Stage::Shape)
        );
        assert!(!doc.healthy());

        // init と verify は今までどおり実行時エラー。**理由が bare と違うこと。**
        let e = init(&env).expect_err("非 git に init は入らない");
        assert!(e.contains("git"), "理由が具体的でない: {e}");
        let args: Vec<String> = ["doctor", "--repo", &plain.to_string_lossy()]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(cli_main(&args), EXIT_UNHEALTHY, "doctor は診断として出す");
    }

    #[test]
    fn bareリポジトリは作業ツリーが無いと説明する() {
        let d = ShapeDir::new("bare");
        let bare = d.0.join("bare.git");
        std::fs::create_dir_all(&bare).unwrap();
        git(&bare, &["init", "--bare"]).expect("bare を作れない");
        let env = d.env(&bare);

        let sh = read_shape(&env);
        assert!(sh.is_git() && sh.bare, "bare を検出していない: {sh:?}");
        assert!(sh.root.is_none());

        let doc = doctor(&env).expect("bare でも診断は出ること");
        全段が埋まっている(&doc.findings);
        assert_eq!(worst(&doc.findings, Stage::Shape), Mark::Bad);
        assert!(reason_has(&doc.findings, Stage::Shape, "bare"));
        let e = init(&env).expect_err("bare に init は入らない");
        assert!(e.contains("bare"), "bare だと言っていない: {e}");
    }

    /// **回帰の番人。** git は `--absolute-git-dir` を実体で、`--git-common-dir`
    /// を相対 (`.git`) で返す。正規形へ寄せずに比べていたころ、
    /// `/var/…//x/.git` と `/private/var/…/x/.git` が別物になって
    /// **素の作業ツリーまで linked worktree に見えていた** (実バイナリで発覚)。
    #[test]
    fn 素の作業ツリーをlinked_worktreeと言わない() {
        let d = ShapeDir::new("plain");
        let repo = d.repo("plain");
        // シンボリックリンク越し (macOS の temp_dir は /var → /private/var) と
        // 末尾スラッシュ、どちらの渡し方でも同じ答えになること。
        for start in [repo.clone(), repo.join("")] {
            let sh = read_shape(&d.env(&start));
            assert!(
                !sh.linked_worktree,
                "素の作業ツリーを linked worktree と判定した ({}): {sh:?}",
                start.display()
            );
            assert_eq!(sh.git_dir, sh.common_dir);
        }
        let doc = doctor(&d.env(&repo)).expect("診断");
        全段が埋まっている(&doc.findings);
        assert!(
            reason_has(&doc.findings, Stage::Shape, "素の作業ツリー"),
            "素の形をそう言っていない: {:?}",
            stage_rows(&doc.findings, Stage::Shape)
        );
    }

    #[test]
    fn linked_worktreeは共有側に効くと出す() {
        let d = ShapeDir::new("linked");
        let main = d.repo("main");
        let wt = d.0.join("wt");
        git(
            &main,
            &["worktree", "add", &wt.to_string_lossy(), "-b", "czero-wt"],
        )
        .expect("worktree を切れない");

        let env = d.env(&wt);
        let sh = read_shape(&env);
        assert!(sh.linked_worktree, "linked worktree を検出していない: {sh:?}");
        assert!(sh.root.is_some());
        assert_ne!(sh.git_dir, sh.common_dir);

        // 本体で入れた守りが、worktree 側の診断でも緑になること
        // (フックと driver は共有 git ディレクトリに入るため)。
        init(&d.env(&main)).expect("本体へ init");
        let doc = doctor(&env).expect("worktree の診断");
        全段が埋まっている(&doc.findings);
        assert_eq!(worst(&doc.findings, Stage::Shape), Mark::Ok);
        assert_eq!(
            worst(&doc.findings, Stage::Hooks),
            Mark::Ok,
            "共有側に入れたフックが worktree から見えていない: {:?}",
            stage_rows(&doc.findings, Stage::Hooks)
        );
    }

    #[test]
    fn submoduleは届かないと出す() {
        let d = ShapeDir::new("submodule");
        let child = d.repo("child");
        let parent = d.repo("parent");
        // **本物の `git submodule add` を試す。** git 2.38+ は既定で
        // file プロトコルを止めるので、その 1 回だけ許可する。
        // 使えない環境 (古い git / 制限された CI) では `.gitmodules` を
        // 直に置いて、検出そのものは必ず試す。
        let added = git(
            &parent,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--",
                &child.to_string_lossy(),
                "child",
            ],
        )
        .is_ok();
        if !added {
            std::fs::write(
                parent.join(".gitmodules"),
                "[submodule \"child\"]\n\tpath = child\n\turl = ../child\n",
            )
            .unwrap();
            git(&parent, &["add", "--", ".gitmodules"]).unwrap();
        }

        let env = d.env(&parent);
        let sh = read_shape(&env);
        assert_eq!(sh.submodules, vec!["child".to_string()]);
        let doc = doctor(&env).expect("診断");
        全段が埋まっている(&doc.findings);
        assert_eq!(worst(&doc.findings, Stage::Shape), Mark::Warn);
        assert!(
            reason_has(&doc.findings, Stage::Shape, "submodule"),
            "submodule へ届かないことを言っていない"
        );

        if added {
            // submodule の**中**から見たら、親にも要ると言うこと。
            let inside = d.env(&parent.join("child"));
            let sh_in = read_shape(&inside);
            assert!(
                sh_in.inside_submodule,
                "submodule の中に居ることを検出していない: {sh_in:?}"
            );
        }
    }

    #[test]
    fn sparse_checkoutを検出してgitattributesの穴を出す() {
        let d = ShapeDir::new("sparse");
        let repo = d.repo("sparse");
        // 本物の sparse-checkout を試し、使えない git では設定だけを置く
        // (`read_shape` が見るのは `core.sparseCheckout` なので、どちらでも
        //  同じ形になる)。
        if git(&repo, &["sparse-checkout", "init", "--cone"]).is_err() {
            git(&repo, &["config", "--local", "core.sparseCheckout", "true"]).unwrap();
        }

        let env = d.env(&repo);
        let sh = read_shape(&env);
        assert!(sh.sparse, "sparse-checkout を検出していない: {sh:?}");
        // **回帰の番人。** 設定の振り分けを `_ =>` で締めていたころ、
        // `git sparse-checkout init --cone` が置く `core.sparseCheckoutCone` を
        // LFS と読んで「git-lfs が居ます」と出した。
        assert!(!sh.lfs, "sparse なだけのリポジトリを LFS と判定した");
        let doc = doctor(&env).expect("診断");
        全段が埋まっている(&doc.findings);
        assert_eq!(worst(&doc.findings, Stage::Shape), Mark::Warn);
        assert!(reason_has(&doc.findings, Stage::Shape, ".gitattributes"));
    }

    #[test]
    fn 既存のフック置き場へ入れて上書きしない() {
        let d = ShapeDir::new("hookspath");
        let repo = d.repo("hp");
        // husky を模した置き場。**相対パスで持たせる** (git がそう書くため)。
        std::fs::create_dir_all(repo.join(".husky")).unwrap();
        git(&repo, &["config", "--local", "core.hooksPath", ".husky"]).unwrap();
        std::fs::write(repo.join(".husky").join("pre-commit"), "#!/bin/sh\necho hi\n").unwrap();

        let env = d.env(&repo);
        let sh = read_shape(&env);
        assert_eq!(sh.hooks_path.as_deref(), Some(".husky"));
        assert_eq!(sh.frameworks, vec!["husky"]);

        // **入れても既存のフックは消えない。**
        init(&env).expect("init");
        let h = read_hooks(&repo).expect("hooks");
        assert!(
            h.dir.ends_with(".husky"),
            "core.hooksPath を無視して .git/hooks へ入れた: {}",
            h.dir.display()
        );
        assert!(
            repo.join(".husky")
                .join(format!("pre-commit{HOOK_PREV_SUFFIX}"))
                .exists(),
            "既存の pre-commit を退避していない (黙って壊した)"
        );
        let doc = doctor(&env).expect("診断");
        assert_eq!(worst(&doc.findings, Stage::Shape), Mark::Warn);
        assert!(reason_has(&doc.findings, Stage::Shape, "husky"));
    }

    #[test]
    fn 既存のフックフレームワークを全部名指しする() {
        let d = ShapeDir::new("frameworks");
        let repo = d.repo("fw");
        std::fs::create_dir_all(repo.join(".husky")).unwrap();
        std::fs::write(repo.join(".pre-commit-config.yaml"), "repos: []\n").unwrap();
        std::fs::write(repo.join("lefthook.yml"), "pre-commit:\n").unwrap();

        let sh = read_shape(&d.env(&repo));
        assert_eq!(
            sh.frameworks,
            vec!["husky", "pre-commit", "lefthook"],
            "並びか中身が変わった (決定的でなくなると差分が読めない)"
        );
    }

    /// **CI が 3 プラットフォームとも赤くなった回帰の番人。**
    ///
    /// `git config --get-regexp` は system / global / local を混ぜて返すので、
    /// git-lfs が入っているマシン (GitHub の ubuntu / macOS / windows
    /// ランナーは同梱) では、LFS を 1 バイトも使っていないリポジトリまで
    /// 「LFS が居る」になっていた。手元の macOS には global の `filter.lfs`
    /// が 1 件も無かったので**ローカルだけ緑**という形で出た。
    ///
    /// git を起こさない純関数で固定する (環境変数を差し替える形にすると
    /// 並列に走る他のテストへ漏れる)。
    #[test]
    fn 設定のスコープを見てglobalのgit_lfsをこのリポジトリのものと数えない() {
        let cases: &[(&str, &str, ScopedConfig)] = &[
            (
                "global の git-lfs だけ (CI ランナーの形)",
                "global\tfilter.lfs.clean git-lfs clean -- %f\n\
                 global\tfilter.lfs.smudge git-lfs smudge -- %f\n\
                 global\tfilter.lfs.required true\n",
                ScopedConfig {
                    sparse: false,
                    hooks_path: None,
                    lfs_local: false,
                },
            ),
            (
                "system に居ても数えない",
                "system\tfilter.lfs.process git-lfs filter-process\n",
                ScopedConfig::default(),
            ),
            (
                "local なら数える",
                "local\tfilter.lfs.clean git-lfs clean -- %f\n",
                ScopedConfig {
                    sparse: false,
                    hooks_path: None,
                    lfs_local: true,
                },
            ),
            (
                "worktree もリポジトリ固有なので数える",
                "worktree\tfilter.lfs.clean git-lfs clean -- %f\n",
                ScopedConfig {
                    sparse: false,
                    hooks_path: None,
                    lfs_local: true,
                },
            ),
            (
                "sparse は global に置いてもこのリポジトリの挙動を変えるので見る",
                "global\tcore.sparsecheckout true\n",
                ScopedConfig {
                    sparse: true,
                    hooks_path: None,
                    lfs_local: false,
                },
            ),
            (
                "hooksPath も同じ (global でも効く)",
                "global\tcore.hookspath .husky\n",
                ScopedConfig {
                    sparse: false,
                    hooks_path: Some(".husky".to_string()),
                    lfs_local: false,
                },
            ),
            (
                "cone を LFS と読まない (前の回帰)",
                "local\tcore.sparsecheckout true\nlocal\tcore.sparsecheckoutcone true\n",
                ScopedConfig {
                    sparse: true,
                    hooks_path: None,
                    lfs_local: false,
                },
            ),
            (
                "スコープ不明 (タブ無し) なら数えない — 決めつけない側へ倒す",
                "filter.lfs.clean git-lfs clean -- %f\n",
                ScopedConfig::default(),
            ),
            (
                "CRLF が混ざっても壊れない",
                "local\tcore.sparsecheckout true\r\nglobal\tfilter.lfs.clean x\r\n",
                ScopedConfig {
                    sparse: true,
                    hooks_path: None,
                    lfs_local: false,
                },
            ),
            ("空の出力", "", ScopedConfig::default()),
        ];
        for (name, out, want) in cases {
            assert_eq!(&parse_scoped_config(out), want, "{name}");
        }
    }

    #[test]
    fn lfsのパターンにunionを当てない() {
        let d = ShapeDir::new("lfs");
        let repo = d.repo("lfs");
        // **`filter=lfs` だけ**を手で書いた形 (merge が空きに見える)。
        std::fs::write(repo.join(".gitattributes"), "*.md filter=lfs\n").unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-m", "lfs attrs", "--no-verify"]).unwrap();

        let root = repo.clone();
        let (want, skip) = plan_patterns(&root);
        assert!(
            !want.contains(&"*.md".to_string()),
            "LFS が当たっているパターンへ union を足そうとした (ポインタが壊れる)"
        );
        assert!(skip.contains(&"*.md".to_string()), "見送りとして報告していない");

        // 人が手で union を当ててしまっていたら ❌ にする。
        std::fs::write(
            repo.join(".gitattributes"),
            format!("*.md filter=lfs\n*.md merge={UNION_AUTO}\n"),
        )
        .unwrap();
        let sh = read_shape(&d.env(&repo));
        assert!(
            !sh.lfs_unsafe.is_empty(),
            "filter=lfs と union が重なっているのに黙っている"
        );
        let doc = doctor(&d.env(&repo)).expect("診断");
        assert_eq!(worst(&doc.findings, Stage::Shape), Mark::Bad);
    }

    #[test]
    fn shallowを検出する() {
        let d = ShapeDir::new("shallow");
        let repo = d.repo("shallow");
        let sh0 = read_shape(&d.env(&repo));
        assert!(!sh0.shallow);
        // shallow clone の印は共有 git ディレクトリの `shallow` ファイル。
        // 実際に浅い clone を作ると `file://` URL の書式が OS で割れるので、
        // **見ている印そのもの**を置いて判定を固定する。
        let common = sh0.common_dir.clone().expect("common dir");
        std::fs::write(common.join("shallow"), "").unwrap();
        let sh = read_shape(&d.env(&repo));
        assert!(sh.shallow, "shallow を検出していない");
        let doc = doctor(&d.env(&repo)).expect("診断");
        assert!(reason_has(&doc.findings, Stage::Shape, "merge-tree"));
    }

    #[test]
    fn シンボリックリンク越しでも台帳のキーが割れない() {
        let d = ShapeDir::new("symlink");
        let repo = d.repo("real");
        let link = d.0.join("link");
        #[cfg(unix)]
        let made = std::os::unix::fs::symlink(&repo, &link).is_ok();
        // Windows のディレクトリシンボリックリンクは開発者モードか管理者が要る。
        // 作れない環境では**この検査を飛ばす** (作れないことを失敗にしない)。
        #[cfg(windows)]
        let made = std::os::windows::fs::symlink_dir(&repo, &link).is_ok();
        if !made {
            eprintln!("[skip] シンボリックリンクを作れない環境");
            return;
        }
        let a = doctor(&d.env(&repo)).expect("実体側");
        let b = doctor(&d.env(&link)).expect("リンク側");
        assert_eq!(
            a.ledger, b.ledger,
            "同じリポジトリなのに台帳が 2 つに割れている"
        );
    }

    #[test]
    fn 書けない場所は書けないと出す() {
        let d = ShapeDir::new("readonly");
        assert!(
            !probe_writable(&d.0.join("no-such-dir")),
            "存在しない場所を「書ける」と答えた"
        );
        assert!(probe_writable(&d.0), "書ける場所を「書けない」と答えた");
        assert!(
            !looks_readonly(&d.0),
            "普通のディレクトリを読み取り専用と答えた"
        );
    }

    /// 形態 → 判定のテーブル。**[`check_shape`] は [`Shape`] の純粋関数**なので、
    /// 実ファイルを作れない組み合わせ (巨大 index / 遅い置き場 / 読み取り専用) も
    /// ここで固定できる。
    #[test]
    fn 形態ごとの判定を表で固定する() {
        let git_dir = std::env::temp_dir().join("g");
        let base = Shape {
            start: std::env::temp_dir(),
            root: Some(std::env::temp_dir().join("r")),
            git_dir: Some(git_dir.clone()),
            common_dir: Some(git_dir),
            ..Shape::default()
        };
        let case = |f: &dyn Fn(&mut Shape)| {
            let mut s = base.clone();
            f(&mut s);
            s
        };
        let table: Vec<(&str, Shape, Mark, &str)> = vec![
            ("素の作業ツリー", base.clone(), Mark::Ok, "素の作業ツリー"),
            (
                "非 git",
                Shape {
                    start: std::env::temp_dir(),
                    ..Shape::default()
                },
                Mark::Bad,
                "lease claim",
            ),
            (
                "bare",
                Shape {
                    bare: true,
                    root: None,
                    ..base.clone()
                },
                Mark::Bad,
                "bare",
            ),
            (
                "linked worktree",
                case(&|s| s.linked_worktree = true),
                Mark::Ok,
                "linked worktree",
            ),
            (
                "submodule の中",
                case(&|s| s.inside_submodule = true),
                Mark::Warn,
                "親リポジトリ",
            ),
            (
                "submodule を抱える",
                case(&|s| s.submodules = vec!["child".into()]),
                Mark::Warn,
                "submodule",
            ),
            (
                "sparse-checkout",
                case(&|s| s.sparse = true),
                Mark::Warn,
                ".gitattributes",
            ),
            (
                "shallow",
                case(&|s| s.shallow = true),
                Mark::Warn,
                "merge-tree",
            ),
            ("LFS だけ", case(&|s| s.lfs = true), Mark::Ok, "git-lfs"),
            (
                "LFS と union が重なる",
                case(&|s| {
                    s.lfs = true;
                    s.lfs_unsafe = vec!["*.md".into()];
                }),
                Mark::Bad,
                "ポインタ",
            ),
            (
                "core.hooksPath",
                case(&|s| s.hooks_path = Some(".husky".into())),
                Mark::Warn,
                "退避",
            ),
            (
                "フレームワーク",
                case(&|s| s.frameworks = vec!["pre-commit"]),
                Mark::Warn,
                "共存",
            ),
            (
                "シンボリックリンク",
                case(&|s| s.symlinked = Some(std::env::temp_dir().join("real"))),
                Mark::Ok,
                "台帳のキー",
            ),
            (
                "読み取り専用",
                case(&|s| s.unwritable = vec!["作業ツリー".into()]),
                Mark::Bad,
                "読み取り専用",
            ),
            (
                "巨大 index",
                case(&|s| s.index_bytes = HUGE_INDEX_BYTES),
                Mark::Warn,
                "巨大リポジトリ",
            ),
        ];
        for (name, sh, mark, needle) in table {
            let f = check_shape(&sh);
            assert!(!f.is_empty(), "{name}: 1 行も出ていない");
            assert_eq!(
                f.iter().map(|x| x.mark).max().unwrap(),
                mark,
                "{name}: 評価が違う ({:?})",
                f
            );
            assert!(
                f.iter().any(|x| x.reason.contains(needle)),
                "{name}: 「{needle}」に触れていない ({:?})",
                f.iter().map(|x| x.reason.as_str()).collect::<Vec<_>>()
            );
            assert!(
                f.iter().all(|x| x.stage == Stage::Shape),
                "{name}: 形態の段以外へ出している"
            );
            // ⚠ / ❌ には必ず直し方を付ける (注記だけの 2 件を除く)。
            for x in &f {
                if x.mark == Mark::Bad {
                    assert!(!x.fix.trim().is_empty(), "{name}: ❌ に直し方が無い");
                }
            }
        }
    }

    // ─────────────── verify ───────────────

    #[test]
    fn verifyは実際に止まることを確かめる() {
        let f = Fixture::new("verify");
        let v = verify(&f.env, false);
        assert_eq!(v.trials.len(), 5, "実証の本数が変わった");
        for t in &v.trials {
            assert_ne!(
                t.outcome,
                Outcome::Failed,
                "実証 {:?} が落ちた: {}",
                t.name,
                t.detail
            );
            assert!(!t.detail.trim().is_empty(), "{:?} の観測結果が空", t.name);
        }
        // 行域・台帳の拒否・driver の 3 本は、どの環境でも通ること。
        for t in v.trials.iter().take(3) {
            assert_eq!(
                t.outcome,
                Outcome::Passed,
                "{:?} が通らない: {}",
                t.name,
                t.detail
            );
        }
    }

    #[test]
    fn verifyは対象リポジトリを一バイトも汚さない() {
        let f = Fixture::new("verify-clean");
        init(&f.env).expect("init");
        let before = git(&f.repo, &["status", "--porcelain"]).unwrap();
        let head_before = git(&f.repo, &["rev-parse", "HEAD"]).unwrap();
        let v = verify(&f.env, false);
        assert!(v.protected(), "{}", render_verify(&v));
        assert_eq!(
            before,
            git(&f.repo, &["status", "--porcelain"]).unwrap(),
            "verify が対象リポジトリの作業ツリーを触った"
        );
        assert_eq!(
            head_before,
            git(&f.repo, &["rev-parse", "HEAD"]).unwrap(),
            "verify が対象リポジトリへコミットした"
        );
    }

    #[test]
    fn verifyは後片付けをする() {
        let f = Fixture::new("verify-cleanup");
        let v = verify(&f.env, false);
        assert!(v.cleaned, "一時領域を片付けていない");
        assert!(
            !v.scratch.exists(),
            "一時領域が残っている: {}",
            v.scratch.display()
        );
    }

    #[test]
    fn keepを付けたときだけ一時領域を残す() {
        let f = Fixture::new("verify-keep");
        let v = verify(&f.env, true);
        assert!(!v.cleaned);
        assert!(v.scratch.exists(), "--keep なのに消えた");
        assert!(
            v.scratch.starts_with(std::env::temp_dir()),
            "一時領域が temp_dir 由来でない: {}",
            v.scratch.display()
        );
        let _ = std::fs::remove_dir_all(&v.scratch);
    }

    #[test]
    fn 試せなかった段は成功と数えない() {
        let f = Fixture::new("verify-skip");
        let mut env = f.env.clone();
        env.exe = None;
        env.wired_driver = false;
        env.wired_guard = false;
        let v = verify(&env, false);
        let skipped: Vec<&Trial> = v
            .trials
            .iter()
            .filter(|t| t.outcome == Outcome::Skipped)
            .collect();
        assert_eq!(skipped.len(), 2, "試せない 2 本が Skipped になっていない");
        for t in skipped {
            assert!(!t.detail.trim().is_empty(), "なぜ試せないかを出していない");
        }
        assert!(v.protected(), "Skipped は失敗ではない");
        // **ここが穴 1。** 飛んだ試行があるのに「検証済み」と言わせない。
        assert_eq!(v.verdict(), Verdict::Partial);
        let text = render_verify(&v);
        assert!(
            !text.contains("守られています"),
            "試せていないのに守られていると出した:\n{text}"
        );
        assert!(
            text.contains(&tr("試せなかったので、この点は検証済みではありません:")),
            "飛んだ試行を見出しの直後に出していない:\n{text}"
        );
        // 飛んだ理由は**具体的**であること (何を直せばいいかが判る語)。
        let live = v
            .trials
            .iter()
            .find(|t| t.name.contains("git commit"))
            .expect("実コミットの試行");
        assert_eq!(live.outcome, Outcome::Skipped);
        assert!(
            live.detail.contains("本物の zai") || live.detail.contains("台帳"),
            "なぜ実コミットを試せないかが具体的でない: {}",
            live.detail
        );
        // JSON にも段と理由が出ること。
        let j = verify_json(&v);
        assert_eq!(j["verdict"], "partial");
        assert_eq!(j["counts"]["skipped"], 2);
        let sk = j["skipped"].as_array().expect("skipped 配列");
        assert_eq!(sk.len(), 2);
        for x in sk {
            assert!(!x["reason"].as_str().unwrap_or_default().trim().is_empty());
        }
    }

    /// **`cli_main` を通さない。** `Env::here` は実環境の台帳を掴むので、
    /// ここで `verify` を走らせるとテストが実 `~/.zaivern` に触れる。
    /// 見たいのは「段 → 終了コード」の写像だけなので、そこだけを固定する。
    #[test]
    fn strictを付けたときだけ部分検証を落第にする() {
        let table = [
            (Verdict::Verified, false, EXIT_OK),
            (Verdict::Verified, true, EXIT_OK),
            (Verdict::Partial, false, EXIT_OK),
            (Verdict::Partial, true, EXIT_UNHEALTHY),
            (Verdict::Broken, false, EXIT_UNHEALTHY),
            (Verdict::Broken, true, EXIT_UNHEALTHY),
        ];
        for (v, strict, want) in table {
            assert_eq!(
                verify_exit(v, strict),
                want,
                "{v:?} / --strict={strict} の終了コードが違う"
            );
        }
        assert!(HELP.contains("--strict"), "--strict が HELP に無い");
        let (found, rest) = take_flag(&["--strict".to_string()], "--strict");
        assert!(found && rest.is_empty(), "--strict を引数から抜けていない");
    }

    #[test]
    fn 実証が触れていない形態を黙らない() {
        let d = ShapeDir::new("untested-shape");
        let repo = d.repo("shapes");
        std::fs::write(
            repo.join(".gitmodules"),
            "[submodule \"child\"]\n\tpath = child\n\turl = ../child\n",
        )
        .unwrap();
        git(&repo, &["add", "-A"]).unwrap();
        git(&repo, &["commit", "-m", "sub", "--no-verify"]).unwrap();
        let v = verify(&d.env(&repo), false);
        assert!(
            v.shape_notes.iter().any(|n| n.contains("submodule")),
            "実証が submodule を試していないことを黙っている: {:?}",
            v.shape_notes
        );
        assert_eq!(
            v.verdict(),
            Verdict::Partial,
            "試していない形態があるのに「検証済み」と言った"
        );
        let text = render_verify(&v);
        assert!(text.contains("使い捨てリポジトリ"), "本文に注記が出ていない:\n{text}");
        assert!(
            !text.contains("守られています"),
            "触れていない形態があるのに守られていると出した:\n{text}"
        );
        assert!(
            !verify_json(&v)["untested_shapes"]
                .as_array()
                .expect("untested_shapes")
                .is_empty()
        );
    }

    /// **実バイナリがあるときは、実際の `git merge` まで通す。**
    ///
    /// `cargo test` のテストバイナリは `merge-driver` を受け付けないので
    /// 通常は Skipped になる。ビルド済みの `zai` が隣に居るときだけ
    /// (= 証拠ゲートで `cargo build --bin zai` した後) 本物で試す。
    /// 場所は `current_exe()` から導出するので**直書きは 1 文字も無い**。
    #[test]
    fn 実バイナリがあれば実マージまで通す() {
        let Some(zai) = built_zai() else {
            return; // 未ビルド。`試せなかった段は成功と数えない` が縮退側を見ている
        };
        let f = Fixture::new("live-merge");
        let mut env = f.env.clone();
        env.exe = Some(zai);
        let v = verify(&env, false);
        let live = v.trials.last().expect("最後は実マージ");
        assert_eq!(
            live.outcome,
            Outcome::Passed,
            "実バイナリでの git merge が通らない: {}",
            live.detail
        );
    }

    /// テストバイナリの隣にあるビルド済み `zai`。無ければ `None`。
    // 関所は `test_util` に 1 つだけ (`guard` 側と同じ理由)。版が同じまま
    // 中身が古い残骸を `--version` の照合は素通りさせる。
    fn built_zai() -> Option<PathBuf> {
        crate::test_util::real_zai("実バイナリでのマージ試験")
    }

    // ─────────────── uninstall ───────────────

    #[test]
    fn uninstallは入れたものだけを戻す() {
        let f = Fixture::new("uninstall");
        // 人が書いた行を先に置いておく (これを消したら失格)。
        std::fs::write(f.repo.join(".gitattributes"), "*.png binary\n").unwrap();
        init(&f.env).expect("init");
        let mid = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap();
        assert!(mid.contains(UNION_AUTO), "そもそも入っていない");

        let u = uninstall(&f.env, true).expect("uninstall");
        assert!(
            u.steps.iter().all(|s| s.action != Action::Failed),
            "撤去に失敗した段がある: {:#?}",
            u.steps
        );
        let attrs = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap_or_default();
        assert!(
            attrs.contains("*.png binary"),
            "人が書いた行を消した: {attrs:?}"
        );
        assert!(
            !attrs.contains(UNION_AUTO),
            "自分が書いた行が残っている: {attrs:?}"
        );
        assert!(!driver_installed(&f.repo), "driver の登録が残っている");
        let h = read_hooks(&f.repo).expect("hooks");
        assert_eq!(
            h.names(HookState::Ours),
            Vec::<String>::new(),
            "フックが残っている"
        );
        let store = lease::store_path_in(&f.env.ledger_dir, &lease::roots_of(&f.repo).key);
        assert!(!lease::enabled(&store), "--purge なのに台帳が残っている");
    }

    #[test]
    fn 退避した既存フックを復元する() {
        let f = Fixture::new("restore-hook");
        let dir = hooks_dir(&f.repo).expect("hooks_dir");
        std::fs::create_dir_all(&dir).unwrap();
        let mine = dir.join("pre-commit");
        std::fs::write(&mine, "#!/bin/sh\n# 人が書いたフック\nexit 0\n").unwrap();
        init(&f.env).expect("init");
        assert!(
            std::fs::read_to_string(&mine)
                .unwrap()
                .contains(GUARD_MARKER),
            "設置後は zaivern のフックになっているはず"
        );
        uninstall(&f.env, true).expect("uninstall");
        let back = std::fs::read_to_string(&mine).unwrap_or_default();
        assert!(
            back.contains("人が書いたフック"),
            "退避した既存フックを復元していない: {back:?}"
        );
    }

    #[test]
    fn 自分の行しか無いgitattributesはファイルごと消す() {
        let f = Fixture::new("attrs-vanish");
        init(&f.env).expect("init");
        assert!(f.repo.join(".gitattributes").exists());
        uninstall(&f.env, true).expect("uninstall");
        assert!(
            !f.repo.join(".gitattributes").exists(),
            "自分の行しか無いのに空ファイルを残した"
        );
    }

    #[test]
    fn 生きている担当が居るなら台帳を消さない() {
        let f = Fixture::new("keep-ledger");
        init(&f.env).expect("init");
        let store = lease::store_path_in(&f.env.ledger_dir, &lease::roots_of(&f.repo).key);
        lease::with_store(&store, |st| {
            lease::try_claim(
                st,
                &holder("他のエージェント", "other-session", "other-cwd"),
                &["notes.md".to_string()],
                lease::now_secs(),
                VERIFY_TTL_SECS,
                &|_| true,
            )
        })
        .expect("確保");
        let u = uninstall(&f.env, false).expect("uninstall");
        assert_eq!(step(&u.steps, Stage::Ledger).action, Action::Skipped);
        assert!(
            lease::enabled(&store),
            "他のエージェントが頼っている台帳を黙って消した"
        );
    }

    #[test]
    fn 撤去も二回打って同じ結果になる() {
        let f = Fixture::new("uninstall-twice");
        init(&f.env).expect("init");
        uninstall(&f.env, true).expect("1 回目");
        let attrs1 = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap_or_default();
        let u = uninstall(&f.env, true).expect("2 回目");
        let attrs2 = std::fs::read_to_string(f.repo.join(".gitattributes")).unwrap_or_default();
        assert_eq!(attrs1, attrs2);
        assert!(
            u.steps.iter().all(|s| s.action != Action::Failed),
            "2 回目の撤去が失敗した: {:#?}",
            u.steps
        );
    }

    #[test]
    fn 導入と撤去を往復しても元に戻る() {
        let f = Fixture::new("round-trip");
        let before = git(&f.repo, &["status", "--porcelain"]).unwrap();
        init(&f.env).expect("init");
        uninstall(&f.env, true).expect("uninstall");
        assert_eq!(
            before,
            git(&f.repo, &["status", "--porcelain"]).unwrap(),
            "往復したのに作業ツリーが元に戻っていない"
        );
    }

    // ─────────────── CLI ───────────────

    #[test]
    fn ヘルプは終了コードの意味を書いている() {
        for code in ["0", "1", "2", "3"] {
            assert!(HELP.contains(code), "終了コード {code} の説明が無い");
        }
        for sub in ["init", "doctor", "verify", "uninstall"] {
            assert!(HELP.contains(sub), "{sub} が HELP に無い");
        }
    }

    #[test]
    fn 未知のサブコマンドは使い方の誤り() {
        assert_eq!(cli_main(&["おかしな語".to_string()]), EXIT_USAGE);
        assert_eq!(cli_main(&[]), EXIT_USAGE);
        assert_eq!(cli_main(&["--help".to_string()]), EXIT_OK);
    }

    #[test]
    fn 引数の抜き出しは順序に依存しない() {
        let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let (repo, rest) = take_opt(&v(&["--json", "--repo", "/x", "--dry-run"]), "--repo");
        assert_eq!(repo.as_deref(), Some("/x"));
        let (json, rest) = take_flag(&rest, "--json");
        let (dry, rest) = take_flag(&rest, "--dry-run");
        assert!(json && dry);
        assert!(rest.is_empty(), "残りが空にならない: {rest:?}");
        // 値の無い --repo は「値」として食わない (次の引数を壊さない)
        let (repo, rest) = take_opt(&v(&["--repo"]), "--repo");
        assert_eq!(repo, None);
        assert_eq!(rest, v(&["--repo"]));
    }

    /// **`doctor` だけは非 git でもエラーにしない。**
    ///
    /// 守りを入れられない場所こそ説明が要る (`zai lease claim` は動くのに
    /// `czero init` だけ失敗する、という非対称の説明)。書き換える側の
    /// `init` / `verify` は今までどおり実行時エラーのまま。
    #[test]
    fn 書き換える側だけがリポジトリでない場所を実行時エラーにする() {
        let dir = unique_temp_dir("zaivern-czero-init-test", "not-a-repo");
        let args = |sub: &str| -> Vec<String> {
            [sub, "--repo", &dir.to_string_lossy()]
                .iter()
                .map(|s| s.to_string())
                .collect()
        };
        assert_eq!(cli_main(&args("init")), EXIT_RUNTIME);
        assert_eq!(cli_main(&args("verify")), EXIT_RUNTIME);
        assert_eq!(
            cli_main(&args("doctor")),
            EXIT_UNHEALTHY,
            "doctor は非 git を説明するので、使い方の誤りでも実行時エラーでもない"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ─────────────── JSON / 表示 ───────────────

    #[test]
    fn jsonは安定キーで出す() {
        let f = Fixture::new("json");
        let r = init(&f.env).expect("init");
        let v = init_json(&r);
        assert!(v.get("healthy").is_some());
        let steps = v.get("steps").and_then(|s| s.as_array()).expect("steps");
        assert_eq!(steps.len(), r.steps.len());
        for s in steps {
            let key = s.get("stage").and_then(|x| x.as_str()).unwrap_or_default();
            assert!(
                STAGES.iter().any(|st| st.key() == key),
                "知らない stage キー: {key}"
            );
        }
        assert!(serde_json::to_string_pretty(&v).is_ok());
        assert!(serde_json::to_string_pretty(&doctor_json(&doctor(&f.env).unwrap())).is_ok());
        assert!(serde_json::to_string_pretty(&verify_json(&verify(&f.env, false))).is_ok());
        assert!(
            serde_json::to_string_pretty(&uninstall_json(&uninstall(&f.env, true).unwrap()))
                .is_ok()
        );
    }

    #[test]
    fn 表示は空行だけにならない() {
        let f = Fixture::new("render");
        let r = init(&f.env).expect("init");
        for text in [
            render_init(&r),
            render_doctor(&doctor(&f.env).expect("doctor")),
            render_verify(&verify(&f.env, false)),
            render_uninstall(&uninstall(&f.env, true).expect("uninstall")),
        ] {
            assert!(text.trim().lines().count() >= 3, "出力が薄すぎる: {text}");
            assert!(!text.contains("\n\n\n"), "空行が続いている: {text:?}");
        }
    }

    #[test]
    fn 空の一覧は括弧なしと出す() {
        assert_eq!(join_or_none(&[]), tr("(なし)"));
        assert_eq!(join_or_none(&["a".to_string(), "b".to_string()]), "a b");
    }

    // ─────────────── 決定性 ───────────────

    #[test]
    fn 二回診断しても並びが変わらない() {
        let f = Fixture::new("deterministic");
        init(&f.env).expect("init");
        let a = doctor(&f.env).expect("1 回目");
        let b = doctor(&f.env).expect("2 回目");
        assert_eq!(
            a.findings, b.findings,
            "診断の中身か並びが揺れている (HashMap の順序が漏れている?)"
        );
    }

    #[test]
    fn 管理ブロックの抽出は決定的で重複しない() {
        let f = Fixture::new("patterns");
        std::fs::write(
            f.repo.join(".gitattributes"),
            format!(
                "# コメント\n*.png binary\n*.md merge={UNION_AUTO}\r\n*.txt merge={UNION_AUTO}\n*.md merge={UNION_AUTO}\n\n"
            ),
        )
        .unwrap();
        let p = managed_patterns(&f.repo);
        assert_eq!(p, vec!["*.md".to_string(), "*.txt".to_string()]);
        assert_eq!(p, managed_patterns(&f.repo), "2 回目で並びが変わった");
    }

    // ─────────────── 到達経路 ───────────────

    #[test]
    fn パレットの登録が正しい() {
        assert_eq!(FEATURE.module, "czero_init");
        assert_eq!(FEATURE.entries.len(), 1, "到達経路は 1 つに絞る");
        let e = &FEATURE.entries[0];
        assert!(
            e.id.starts_with("czero_init."),
            "ID の接頭辞が違う: {}",
            e.id
        );
        assert!(!e.icon.trim().is_empty());
        assert!(!e.label.trim().is_empty());
        assert!(FEATURE.draw.is_some(), "窓を描かないと結果が見えない");
        assert!(FEATURE.settings.is_empty(), "共有の config.rs を要求しない");
        assert!(FEATURE.binds.is_empty(), "共有の keybinds.rs を要求しない");
    }

    /// 製品コードだけ (テストモジュールより前) を返す。
    ///
    /// **禁止語を探す番人は、自分の禁止語リストに引っかかる。** テストの中に
    /// 書いた `"/tmp/` という**文字列リテラルそのもの**を検出して落ちた
    /// (実際に 2 件落ちた)。番人が見たいのは製品コードなので、そこで切る。
    fn 製品コード() -> String {
        let src = include_str!("czero_init.rs").replace("\r\n", "\n");
        let cut = src
            .find("\n#[cfg(test)]\n")
            .expect("テストモジュールの開始が見つからない (番人が全文を見てしまう)");
        src[..cut].to_string()
    }

    /// **共有ファイルを 1 バイトも要求していないこと**の構造検査。
    #[test]
    fn 共有ファイルへの追記を要求していない() {
        let src = 製品コード();
        assert!(
            !src.contains("Cmd::"),
            "palette.rs の Cmd に variant を足そうとしている"
        );
        assert!(
            !src.contains("BindAction"),
            "keybinds.rs の BindAction を増やそうとしている"
        );
        // 申し送りが残っていること (統合担当がこの 2 行を入れる)。
        assert!(
            src.contains("| \"czero\"") && src.contains("czero_init::cli_main"),
            "統合担当への申し送り (cli.rs の 2 行) が消えている"
        );
    }

    #[test]
    fn パスの直書きが無い() {
        let src = 製品コード();
        for bad in ["\"/tmp/", "\"/Users/", "\"/home/", "\"C:\\\\", "\"C:/"] {
            assert!(
                !src.contains(bad),
                "パスを直書きしている: {bad} (どの環境でも動かなくなる)"
            );
        }
        // 番人が本当に中身を見ていること (切りすぎて 0 文字を検査しない)。
        assert!(src.contains("SCRATCH_PREFIX"), "製品コードを切りすぎている");
    }
}
