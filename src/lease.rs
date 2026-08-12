//! **ファイル所有リース** — 並列エージェントの衝突を「検出」ではなく「発生させない」。
//!
//! ## なぜ要るのか (計測された空白)
//!
//! 9 種の OSS エージェント・オーケストレータを調べた調査は「**どれ 1 つとして
//! マージ衝突の処理を自動化していない**」と結論している。各ツールの答えは
//! 揃って「git worktree がファイルシステムを分離する」で、これは
//! **同じファイルへの同時書き込みを防ぐだけ**であり、
//! **2 つの worktree で同じファイルを編集する 2 人**には 1 ミリも効かない。
//!
//! - AgenticFlict (arXiv 2604.03551) は 142,000 件超のエージェント PR を測って
//!   **衝突率 27.67%**。
//! - Cursor の swarm 実験は **2 時間で 7 万件超のマージ衝突**を出して中断。
//!
//! CLAUDE.md 設計原則 5 の「セッションの所有権はアトミックに主張し、競合したら
//! fail-closed にする」を、**ファイル単位**へ下ろしたのがこのモジュール。
//!
//! ## 3 つの部品
//!
//! 1. **リース台帳** — `~/.zaivern/leases/<スコープキー>.json`。
//!    プロセスをまたいで見えることが要件 (判定するのは GUI ではなく、
//!    ベンダー CLI が起こす短命の `zai hook` プロセス)。
//!    確保は**アトミック**で、競合したら片方だけが勝つ (後勝ちにしない)。
//! 2. **強制** — `zai hook` が `PreToolUse` で書き込み系ツールを見たとき、
//!    他人が持っているパスなら **deny を返してツール呼び出しを止める**。
//! 3. **事前の重複検出** — N 人へ配る前に担当集合の重なりを出し、分割を促す。
//!
//! ## スコープは worktree ではなく「元のリポジトリ」
//!
//! ここが競合との差。`main_repo_root` は linked worktree の `.git` ファイル
//! (`gitdir: …/.git/worktrees/<名前>`) を辿って**元のリポジトリのルート**へ
//! 寄せる。そうしないと worktree ごとに台帳が分かれ、まさに調査が指摘した
//! 「worktree は意味的な衝突を 1 つも防がない」状態に戻ってしまう。
//!
//! ## 段 (CLAUDE.md 設計原則 4 の作法をそのまま適用)
//!
//! | 段 | 条件 | 効果 |
//! |---|---|---|
//! | 強制 | フックが設置済み | 書き込みが**実際に止まる** |
//! | 勧告 | 台帳はあるがフックが無い | UI が警告するだけ |
//! | 無効 | 台帳が無い | 何もしない (フックの追加コストは `stat` 1 回) |
//!
//! **「効いていると思わせて実は勧告」は無いより悪い。** so 段は画面に出す。
//!
//! ## 所有の単位は「ファイル」から「行域」へ
//!
//! ファイル単位の所有は衝突 0 を**買えた**が、その代金は拒否だった。
//! `docs/conflict-zero.md` の実測は 64 体・1536 書込でマージ衝突 0 件 —
//! ただしその 0 件は **971 回の書き込みを断って**買ったもので、
//! **並列度がファイル数で頭打ちになる**。
//!
//! いまは `src/a.rs#L10-40` の書き方で**ファイル内の行域**を持てる:
//!
//! * 台帳のスキーマは 1 バイトも変えていない (`patterns: Vec<String>` のまま)。
//!   `#L` を含まない古い台帳はファイル全体の域として読める
//! * 判定は [`overlaps`] 1 本に集約してある。確保・分割・重なり検出が
//!   同じ規則で動くので、**2 実装がズレる余地が無い**
//! * 「同じファイルでも安全帯 ([`crate::region::SAFE_BAND`] = 3 行) ぶん
//!   離れていれば 2 人が同時に持てる」。3 行は git の diff が既定で付ける
//!   文脈の幅で、**これより近い 2 つの変更は xdiff が 1 ハンクに畳んで衝突にする**
//! * 関門 ([`gate`]) は「書き込み前の中身」と「ペイロードから作った
//!   書き込み後の中身」から**実際に触れる行域**を出して判定する。
//!   出せないもの (シェル経由・パッチ・巨大ファイル) は
//!   **ファイル全体を触るもの**として扱う = 従来と同じ挙動へ落ちる
//!
//! ## 行域は「行番号」ではなく「そこにある内容」に紐づく (錨)
//!
//! 行域リースには**行番号が動く**という穴があった。A が 100 行目付近を編集して
//! 10 行増やすと、B が持っている「200〜260 行目」は実際には「210〜270 行目」へ
//! ずれる。台帳が行番号しか覚えていないと、次の判定で B は**他人の領域を
//! 自分のものだと思い込む** — 拒否も警告も出ないまま「衝突ゼロ」の保証が
//! 静かに破れる、いちばん危ない壊れ方である。
//!
//! * **確保の瞬間**に [`crate::region::capture_anchor`] で先頭行・末尾行の
//!   中身を覚え、[`Lease::anchors`] へ `patterns` と並べて持つ
//!   (`#[serde(default)]` なので**錨を知らない古い台帳もそのまま読める**)
//! * **判定の瞬間**に [`crate::region::resolve`] で取り直す (遅延解決)。
//!   書き込みのたびに全担当の台帳を書き直す eager な追従は**採らない** —
//!   帳簿付けの費用が要るうえ、フックが 1 回でも飛ぶと台帳が現実とずれて
//!   **ずれたことを誰も検出できない**
//! * 取り直せない (域が消えた / 同じ内容の行が同距離に複数) ときは
//!   **台帳に書いてある行番号へ落ちる** = 錨が入る前とまったく同じ判定。
//!   理由は [`Lease::live_span_of`] に書いた
//! * 記号で指す域 (`src/a.rs#fn:draw_toolbar`) は [`hydrate_in`] が確保の
//!   瞬間に実ファイルから行域へ落とす。**Rust だけ**が対象
//!
//! ## 失敗の向き
//!
//! - **内部エラーは fail-open** (許可)。台帳が読めない・ロックが取れないのは
//!   こちらの都合で、それでユーザーのエージェントを止めるのは衝突より悪い。
//! - **本物の競合は fail-closed** (拒否)。
//!
//! ## 統合担当へ — このブランチが触れなかった 3 か所
//!
//! いずれも**行域リースが正しく効く**うえでの制限で、直せば良くなる:
//!
//! 1. **`src/cli.rs`**: `zai lease claim src/a.rs#L10-40` も
//!    `zai lease claim 'src/a.rs#fn:draw_toolbar'` も**そのまま動く**
//!    ([`try_claim`] が [`normalize_spec`] と [`hydrate_in`] を通すため)。
//!    残っているのは 2 つ:
//!    (a) `HELP_LEASE` の `<パターン...>` の説明へ
//!    「`src/a.rs#L10-40` / `src/a.rs#fn:名前` のように域も指定できます」を足す。
//!    (b) `lease claim` は基準フォルダを渡さないので [`default_tree`] =
//!    **プロセスの作業フォルダ**へ落ちる。`--dir <別フォルダ>` と記号指定を
//!    同時に使うと別のファイルを読む。`cli.rs` が [`try_claim_in`] へ
//!    `roots.tree` を渡せば消える (1 行)。
//!    `lease claim` は [`with_store`] を直に呼んでいるので、
//!    [`with_store_retry`] へ替えると混雑時の空振りも消える
//!    (`gate` / [`claim_for`] / [`release_one`] は既に替えてある)。
//! 2. **`src/guard.rs`** のコミット前点検は `Lease::covers_path` を使う =
//!    行番号を見ない。行域リースは**ファイル粒度で効く**ので、離れた行域を
//!    持つ 2 人が同じファイルへコミットすると**両方止まる** (安全側だが過剰)。
//!    [`Lease::owned_spans`] / [`decide_spans`] を渡せば緩められる。
//! 3. **`src/agents.rs`**: [`applied_text`] が使う編集ペイロードのキーは
//!    本来 `HookTarget` が持つデータ ([`EDIT_KEYS`] のコメントに手順がある)。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, TryRecvError};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use eframe::egui;
use serde::{Deserialize, Serialize};

use crate::i18n::{tr, trf};

// ═══════════════════════════════════════════════════════════════════════════
//  定数
// ═══════════════════════════════════════════════════════════════════════════

/// リースの既定寿命 (秒)。死んだエージェントにリポジトリを人質へ取らせない。
/// フック経由の自動確保は書き込みのたびに延長されるので、**30 分黙った
/// エージェントは所有権を失う**。
pub const DEFAULT_TTL_SECS: u64 = 30 * 60;

/// 期限切れ後、所有プロセスが生きている間だけ与える猶予 (秒)。
///
/// **上限が要る理由**: 生存確認は PID で行うが、PID は再利用される。
/// 猶予を無制限にすると、再利用された無関係な PID のせいでリースが
/// 永久に生き残り得る。CLAUDE.md の「終了済みセッションへ kill を撃たない」
/// と同じ懸念で、こちらは kill しない代わりに**寿命の上限で封じる**。
const RECLAIM_GRACE_SECS: u64 = 5 * 60;

/// 残りがこの割合を切ったら延長する (書き込みのたびに書き戻さない)。
const REFRESH_BELOW: f64 = 0.5;

/// 1 スコープに置ける最大リース数。壊れた書き手に台帳を膨らませない。
const MAX_LEASES: usize = 512;

/// 1 リースに載せられる最大パターン数。
///
/// 行域リース ([`normalize_spec`]) は「同じファイルの違う行」を別々に持つので、
/// 素朴に足していくと台帳が書き込みのたびに 1 行ずつ伸びる。
/// [`absorb`] が安全帯 ([`crate::region::SAFE_BAND`]) 以内の域を畳むので
/// 実際にはファイルあたり数件で頭打ちになるが、**壊れた書き手への防壁**として
/// 上限も置く。当たったら [`MAX_LEASES`] と同じく**止めて人に知らせる**
/// (「確保できていないのに取れたと言う」のがいちばん危ない)。
const MAX_PATTERNS: usize = 1_024;

/// ロック待ちの上限 (ミリ秒)。**エージェントの書き込みの臨界路**なので短い。
/// 取れなければ fail-open で許可する。
const LOCK_WAIT_MS: u64 = 200;

/// ロックを待つとき、**寝る前に譲るだけ**で済ませる回数。
///
/// ## これが `busy-deny` の正体だった
/// 以前は取れないたびに **5ms 固定で寝ていた**。臨界区間 (小さな JSON の
/// 読み書き) は 0.1〜0.5ms しかかからないのに、待ち手は 5ms 刻みでしか
/// 起きないので、**ロックの受け渡しが毎秒 200 回で頭打ち**になる。
/// 64 体が同時に確保しに来ると 64 × 5ms = 320ms 必要で、
/// 待ち予算 ([`LOCK_WAIT_MS`] = 200ms) を構造的に超える —
/// `docs/conflict-zero.md` が「32 体以上で busy-deny が増える」と書いていた
/// のはこれで、混み方の問題ではなく**待ち方の問題**だった。
const LOCK_SPIN_ROUNDS: u32 = 96;

/// 譲るのをやめた後の待ち時間の初期値 (マイクロ秒)。
const LOCK_BACKOFF_US: u64 = 120;

/// 指数バックオフの上限 (マイクロ秒)。これ以上空けても取り合いは減らない。
const LOCK_BACKOFF_CAP_US: u64 = 4_000;

/// `busy` を返す前に [`with_store_retry`] が使う再試行の総予算 (ミリ秒)。
///
/// **1 回の `acquire_lock` を長くするのではなく、外側で作り直す。**
/// 置き去りロックの奪取 ([`LOCK_STALE_MS`]) と TTL の判定を毎回やり直せるので、
/// 「先客がクラッシュしていた」場合にも自力で抜けられる。
const LOCK_RETRY_MS: u64 = 1_000;

/// 進捗が観測できている間でも、これを超えたら諦める (ミリ秒)。
///
/// [`with_store_retry`] は「台帳が書き換わり続けている＝系は生きている」
/// 限り待ち続ける。台数が増えれば待ち時間も伸びるのが正しいが、**短命な
/// `zai hook` プロセスが何十秒も居座るのは別の壊れ方**なので上限を置く。
/// 30 秒は「64 体が 1 ファイルへ殺到しても実測 1.1 秒」に対して 27 倍の余裕。
const LOCK_RETRY_CAP_MS: u64 = 30_000;

/// ロック待ちが尽きたことを示す接頭辞 (表示はされない制御文字)。
/// [`is_lock_busy`] だけが読む。
const LOCK_BUSY: &str = "\u{0}busy:";

/// 置き去りロックを奪ってよくなるまでの時間 (ミリ秒)。
/// フックは短命なので、これを超えて握っているのはクラッシュの跡。
const LOCK_STALE_MS: u64 = 5_000;

/// 台帳のポーリング間隔の基準。実所要の 4 倍まで自動で空く
/// ([`crate::git::scan_interval`])。
const SCAN_BASE: Duration = Duration::from_millis(1_500);

/// 診断ログの上限 (バイト)。超えたら作り直す (無限に伸ばさない)。
const LOG_CAP: u64 = 64 * 1024;

/// 行域を出すために読むファイルの上限 (バイト)。
///
/// [`gate`] は**書き込みのたびに走る短命プロセス**なので、
/// 「書き込み前の中身」を読む I/O に上限が要る。超えたファイルは
/// **行域を決められないもの = ファイル全体を触るもの**として扱う
/// (= 従来のファイル単位リースと同じ挙動へ落ちる)。
/// 1MiB は src/ の最大ファイル (`app.rs` 約 0.5MB) の 2 倍。
const GATE_READ_CAP: u64 = 1024 * 1024;

/// 画面が狭いときにボタンをアイコンだけへ縮退させる境界 (pt)。
const COMPACT_WIDTH: f32 = 560.0;

// ═══════════════════════════════════════════════════════════════════════════
//  1. パスの正規化と glob (純粋関数 — ここが取り違えると全部が狂う)
// ═══════════════════════════════════════════════════════════════════════════

/// パス / パターンを台帳の正規形へ。
///
/// * 区切りは `/` へ寄せる (Windows の `\` をそのまま保存すると、
///   同じファイルが 2 つのキーで台帳に載る)
/// * 連続する区切りと `./` を潰す
/// * Windows は大文字小文字を区別しないファイルシステムが既定なので畳む。
///   **両方の側を実装する** — unix はそのまま (`Foo.rs` と `foo.rs` は別物)
pub fn normalize_path(raw: &str) -> String {
    // 規則は 3 つの OS ぶんあるが、動いている OS のぶんしか実行されない。
    // **引数へ出しておかないと、macOS で開発している限り Windows / Linux の
    // 規則は一度も検査されない** (`keybinds::canonical_mods_on` と同じ流儀)。
    normalize_path_on(raw, true, cfg!(any(windows, target_os = "macos")))
}

/// 規則を明示する [`normalize_path`]。
///
/// * `win_sep` — `\` も区切りとして畳むか (Windows 由来のパスを受けるため)
/// * `fold_case` — 大文字小文字を畳むか
///
/// 固定すべき表は Windows=(true, true) / macOS=(true, true) / Linux=(true, false)。
/// **どのホストからでも 3 通り全部をテストできる**ようにするのがこの関数の目的。
pub fn normalize_path_on(raw: &str, win_sep: bool, fold_case: bool) -> String {
    let slashed = if win_sep {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    };
    // **末尾の `/` は「その配下ぜんぶ」の意味**。ここで落とすと
    // `zai lease claim src/` が `src` という 1 ファイルの確保になり、
    // **配下を 1 つも守らないのに「確保しました」と返る** (実測で踏んだ。
    // 人が担当表を書くときの最も自然な書き方が no-op になっていた)。
    let subtree = slashed.ends_with('/');
    let mut segs: Vec<&str> = Vec::new();
    for seg in slashed.split('/') {
        match seg {
            "" | "." => continue,
            // `..` を畳む。畳まないと `src/sub/../mod.rs` が
            // `src/sub/../mod.rs` のまま台帳に載り、実際の
            // `src/mod.rs` への書き込みと一致しない (確保側だけずれる)。
            ".." => {
                // 先頭を越える `..` は落とす (スコープ相対なので外は関知しない)。
                segs.pop();
            }
            _ => segs.push(seg),
        }
    }
    let mut out = segs.join("/");
    if subtree && !out.is_empty() {
        out.push_str("/**");
    }
    // **大小非区別は「OS」ではなく「ボリューム」の性質**だが、macOS の既定
    // (APFS) と Windows はどちらも非区別なので、この 2 つは畳む。
    // ここを `cfg!(windows)` だけにしていたため、**開発機である macOS で
    // `src/Foo.rs` と `src/foo.rs` が別リースになり、同じ物理ファイルへ
    // 2 人が同時に書けていた** (実バイナリで再現済み)。
    // 畳みすぎる側は「別ファイルを同じ扱いにする」= 過剰に止める方向なので
    // fail-closed。取りこぼす側と違って衝突は生まない。
    if fold_case {
        out.to_lowercase()
    } else {
        out
    }
}

/// パターンを区切りごとの並びへ。
///
/// 末尾が `/` のものは**サブツリー指定**とみなして `**` を足す
/// (「auth モジュールを直して」と頼まれたエージェントは配下を丸ごと持つ)。
fn segments(pattern: &str) -> Vec<String> {
    let trailing_dir = pattern.ends_with('/') || pattern.ends_with('\\');
    let norm = normalize_path(pattern);
    let mut segs: Vec<String> = norm.split('/').map(str::to_string).collect();
    segs.retain(|s| !s.is_empty());
    if trailing_dir {
        segs.push("**".to_string());
    }
    segs
}

// ── 行域つきの仕様 (`src/a.rs#L10-40`) ────────────────────────────────────
//
// **台帳のスキーマは 1 バイトも変えない。** `Lease::patterns` は今までどおり
// `Vec<String>` で、そこに `src/a.rs#L10-40` という書き方が載るだけ。
// `#L` を含まない古い台帳は「ファイル全体の域」として読めるので、
// 版を上げずに読み書きできる ([`tests::古い台帳は全体の域として読める`])。

/// 行域が付いている**可能性**があるか。
///
/// **フックの臨界路なので、付いていない圧倒的多数は 1 バイトも解析しない。**
/// `#` を含むパスは実在し得る (稀) が、その場合も [`crate::region::parse`] が
/// 「`L` で始まらない断片はパスの一部」と判断するので取り違えない。
fn has_frag(spec: &str) -> bool {
    spec.as_bytes().contains(&b'#')
}

/// 仕様文字列からパス部分だけを取り出す。行域が無ければそのまま返す。
///
/// 行域を**見ない**のが肝で、[`covers`] のようにファイル粒度でしか
/// 判断できない呼び出し元は、ここを通して**安全側 (= ファイル全体)** へ倒す。
pub fn spec_path(spec: &str) -> &str {
    if !has_frag(spec) {
        return spec;
    }
    let t = spec.trim();
    match crate::region::parse(t) {
        // `parse` は trim した文字列を `#` で分けるので、`path` は必ず前半。
        Ok(r) if !r.is_whole() => &t[..r.path.len()],
        _ => spec,
    }
}

/// 仕様文字列の行域。`None` = **ファイル全体** (従来のリースと等価)。
pub fn spec_span(spec: &str) -> Option<crate::region::Span> {
    if !has_frag(spec) {
        return None;
    }
    crate::region::parse(spec.trim()).ok().and_then(|r| r.span)
}

/// 仕様文字列を [`crate::region::Region`] へ。
/// **壊れていればファイル全体として扱う** (安全側 — 読めない指定で
/// 「どの行も持っていない」と判断すると、誰でも書けてしまう)。
pub fn spec_region(spec: &str) -> crate::region::Region {
    if !has_frag(spec) {
        return crate::region::Region::whole(spec);
    }
    match crate::region::parse(spec.trim()) {
        Ok(r) => r,
        Err(_) => crate::region::Region::whole(spec),
    }
}

/// 台帳の正規形へ。**行域を保ったまま**パス部分だけ [`normalize_path`] を通す。
///
/// ## なぜ [`normalize_path`] をそのまま使えないか
/// Windows / macOS は大小を畳むので、`src/A.rs#L10-40` が
/// `src/a.rs#l10-40` になる。[`crate::region::parse`] は小文字の `l` も
/// 受けるので**壊れはしない**が、同じ域が `#L` と `#l` の 2 通りで
/// 台帳に載ると `patterns.contains` が外れて二重登録になる。
/// [`crate::region::render`] を通して**表記を 1 つに保つ**。
pub fn normalize_spec(raw: &str) -> String {
    normalize_spec_on(raw, true, cfg!(any(windows, target_os = "macos")))
}

/// 規則を明示する [`normalize_spec`] ([`normalize_path_on`] と同じ流儀)。
///
/// **どのホストからでも 3 つの OS の規則を検査できる**ようにするのが目的。
/// 実際にここを `cfg!` のままにしていたため、大小を畳む OS
/// (Windows / macOS) だけで `a.rs#L1-10` が `a.rs#l1-10` になり、
/// **同じファイルの別パスとして台帳に並んで重なり判定をすり抜けた**
/// (`tests::正規化は行域の手前だけに掛かる` が番人)。
pub fn normalize_spec_on(raw: &str, win_sep: bool, fold_case: bool) -> String {
    if !has_frag(raw) {
        return normalize_path_on(raw, win_sep, fold_case);
    }
    match crate::region::parse(raw.trim()) {
        Ok(r) if !r.is_whole() => {
            // **規則が掛かるのはパス部分だけ。** 行域はそのまま
            // `crate::region::render` が `#L…` の 1 表記へ揃える。
            let path = normalize_path_on(&r.path, win_sep, fold_case);
            if path.is_empty() {
                return String::new();
            }
            crate::region::render(&crate::region::Region {
                path,
                span: r.span,
                anchor: r.anchor,
            })
        }
        // 行域が無い / 読めない = 従来どおりのパス正規化
        _ => normalize_path_on(raw, win_sep, fold_case),
    }
}

/// 利用者が打った 1 件の指定を、**台帳が使えるスコープ相対の仕様**へ直す。
///
/// ## なぜ要るのか
/// [`normalize_path_on`] は台帳の鍵を作る関数なので、**先頭の `/` を落とす**
/// (区切りで分けて空の断片を捨てるため)。そこへ絶対パスを渡すと
/// `/repo/src/a.rs` が `repo/src/a.rs` という**実在しない鍵**として載り、
/// 同じファイルの相対指定 `src/a.rs` と永久に一致しない。
/// CLI は「1 件を確保しました」と返すのに**何ひとつ守らない**
/// (実バイナリで再現済み: 2 人が同じ物理ファイルを同時に持てた)。
///
/// ここは「人が打った文字列」と「台帳の鍵」の境目で、**絶対パスを
/// ツリー相対へ畳むか、畳めないなら明示的に失敗する**のが仕事。
/// 成功と偽らない (fail-closed) — 守れない指定を黙って受けるのが最悪。
///
/// * `tree` — スコープ相対パスの起点 (= [`Roots::tree`])
/// * `raw` — `src/a.rs` / `/abs/src/a.rs#L10-20` / `C:\repo\src\a.rs` / `src/`
///
/// 相対指定は**1 バイトも変えずに**返す (従来の経路と完全に同じ)。
pub fn resolve_spec_arg(tree: &Path, raw: &str) -> Result<String, String> {
    let t = raw.trim();
    if t.is_empty() {
        return Err(tr("空の指定です"));
    }
    // パス部分と `#…` を分ける。**行域は 1 バイトも触らない** —
    // ここで触ると `#L10-20` が壊れ、行域オーナーシップが丸ごと消える。
    let path_part = spec_path(t);
    let frag = &t[path_part.len()..];
    if !is_absolute_any(path_part) {
        return Ok(t.to_string()); // 相対 = 従来どおり
    }
    // 末尾の `/` は「その配下ぜんぶ」の意味。相対化で消えるので先に覚える
    // (`normalize_path_on` が `**` を足す判断に使う)。
    let subtree = path_part.ends_with('/') || path_part.ends_with('\\');
    let abs = PathBuf::from(path_part.replace('\\', "/"));
    // ツリー自身を指したら「配下ぜんぶ」。`rel_within` は空を `None` に
    // するので、ここで拾わないと「ルートを指したのに失敗」になる。
    if canonical_best_effort(&abs) == canonical_best_effort(tree) {
        return Ok(format!("**{frag}"));
    }
    let Some(rel) = rel_within(tree, &abs) else {
        // **ツリーの外**。台帳はスコープ相対でしか物を言えないので、
        // ここを通すと「載ったのに誰も守らない」鍵が生まれる。断る。
        return Err(trf(
            "「{path}」はこのスコープの外です (スコープ: {tree})。スコープ内の相対パスで指定してください",
            &[("path", path_part.to_string()), ("tree", tree.display().to_string())],
        ));
    };
    // `rel_within` は [`normalize_path`] を通済み (macOS / Windows は小文字化)。
    let rel = if subtree { format!("{rel}/") } else { rel };
    Ok(format!("{rel}{frag}"))
}

/// [`resolve_spec_arg`] を並びへ。**1 件でも直せなければ全部やめる**
/// (全か無か — 一部だけ確保して「守っている」と誤解させない)。
pub fn resolve_spec_args(tree: &Path, raw: &[String]) -> Result<Vec<String>, String> {
    raw.iter().map(|r| resolve_spec_arg(tree, r)).collect()
}

/// パターンが具体的なパスを覆うか。**フックの臨界路**なのでここは単純に保つ。
///
/// `path` 側は実在のパスなので `*` / `?` はワイルドカードとして扱わない
/// (ファイル名に `*` が入る環境では過剰一致し得る — Windows では不正文字、
/// unix でも実運用ではまず無い。既知の限界として受け入れる)。
///
/// **行域は見ない。** `src/a.rs#L10-40` は `src/a.rs` を「覆う」と答える。
/// ここを行域まで見る作りにすると、行番号を知らない呼び出し元
/// ([`crate::guard`] のコミット前点検など) が「誰も持っていない」と
/// 誤答して**素通しになる** — 失敗の向きが逆で、いちばん危ない。
/// 行番号まで見たいときは [`covers_span`] を使う。
pub fn covers(pattern: &str, path: &str) -> bool {
    seg_covers(&segments(spec_path(pattern)), &segments(spec_path(path)))
}

/// **行番号まで見る [`covers`]。** パターンが `touched` の行に関わるか。
///
/// | パターン | 答え |
/// |---|---|
/// | パスが当たらない | `false` |
/// | 行域なし (ファイル全体) | `true` — 何行目だろうと関わる |
/// | 行域あり・`touched` が空 | `true` — 触れた行が判らない = 安全側 |
/// | 行域あり | `touched` のどれかと安全帯 ([`crate::region::SAFE_BAND`]) 以内なら `true` |
///
/// 安全帯を挟むのは、git の diff が既定で 3 行の文脈を付けるため。
/// 3 行未満しか離れていない 2 つの変更は xdiff が 1 ハンクに畳んで
/// **衝突にする**ので、「重なっていない」と答えてはいけない。
pub fn covers_span(pattern: &str, path: &str, touched: &[crate::region::Span]) -> bool {
    covers(pattern, path) && hits(spec_span(pattern), touched)
}

/// 「この域は `touched` に関わるか」の**唯一の規則**。
///
/// `own` が `None` (= ファイル全体) と `touched` が空 (= 触れた行が判らない) は
/// どちらも安全側 = **関わる**へ倒す。錨で取り直した域も、台帳の行番号のままの
/// 域も必ずここを通す — 判定を 2 実装持つと必ずズレる。
fn hits(own: Option<crate::region::Span>, touched: &[crate::region::Span]) -> bool {
    let Some(own) = own else {
        return true;
    };
    touched.is_empty()
        || touched
            .iter()
            .any(|t| crate::region::spans_too_close(&own, t, crate::region::SAFE_BAND))
}

fn seg_covers(pat: &[String], path: &[String]) -> bool {
    // **`**` が複数あると素の再帰は組合せ爆発する。** 実測で `**` 8 個の
    // パターン 1 件の判定に 35 秒かかった = 書き込みの臨界路が丸ごと止まる。
    // (状態は「パターンの何番目 × パスの何番目」しか無いので、一度調べた
    //  組を覚えるだけで O(|pat|×|path|) に落ちる。)
    let mut seen = vec![false; (pat.len() + 1) * (path.len() + 1)];
    seg_covers_memo(pat, path, 0, 0, path.len() + 1, &mut seen)
}

fn seg_covers_memo(
    pat: &[String],
    path: &[String],
    pi: usize,
    si: usize,
    stride: usize,
    seen: &mut Vec<bool>,
) -> bool {
    let key = pi * stride + si;
    if seen[key] {
        // この (pi, si) は別の経路で調べ済み。偽だったから戻ってきている。
        return false;
    }
    seen[key] = true;
    let Some(head) = pat.get(pi) else {
        return si == path.len();
    };
    if head == "**" {
        return (si..=path.len()).any(|k| seg_covers_memo(pat, path, pi + 1, k, stride, seen));
    }
    let Some(seg) = path.get(si) else {
        return false;
    };
    seg_one(head, seg) && seg_covers_memo(pat, path, pi + 1, si + 1, stride, seen)
}

/// 1 セグメント内の `*` / `?` 照合。
fn seg_one(pat: &str, s: &str) -> bool {
    let p: Vec<char> = pat.chars().collect();
    let t: Vec<char> = s.chars().collect();
    // 素直な DP。セグメントは短い (せいぜい数十文字)。
    let mut reach = vec![false; t.len() + 1];
    reach[0] = true;
    for pc in p {
        let mut next = vec![false; t.len() + 1];
        for (j, &ok) in reach.iter().enumerate() {
            if !ok {
                continue;
            }
            match pc {
                '*' => {
                    // 0 文字以上: ここから後ろ全部へ届く
                    for n in next.iter_mut().skip(j) {
                        *n = true;
                    }
                }
                '?' => {
                    if j < t.len() {
                        next[j + 1] = true;
                    }
                }
                c => {
                    if j < t.len() && t[j] == c {
                        next[j + 1] = true;
                    }
                }
            }
        }
        reach = next;
    }
    reach[t.len()]
}

/// **2 つのパターンが同じパスに当たり得るか。** 事前の重複検出と、
/// 確保時の競合判定はどちらもこれ 1 本で決まる。
///
/// 難しいのは境界で、テーブルテストで固定してある:
/// `src/**` と `src/a.rs` は重なる / ファイルとその親ディレクトリは重なる /
/// `src/*.rs` と `src/sub/a.rs` は重ならない (`*` は `/` を越えない)。
///
/// ## 行域 (`src/a.rs#L10-40`)
/// **どちらかに行域が付いていれば [`crate::region::conflicts`] へ渡す。**
/// これで「同じファイルでも安全帯ぶん離れていれば 2 人が同時に持てる」
/// が確保・分割・重なり検出の**全部**へ一度に効く (判定はここ 1 本しかない)。
/// 片方でも glob なら「同じファイルを指しているか」が確定しないので、
/// `conflicts` が安全側 (= 衝突扱い) へ倒す。
///
/// 再帰しないことの確認: `conflicts` はここを**パス部分だけ**で呼び直すが、
/// パス部分に `#` は残らないので必ず [`seg_overlap`] 側へ落ちる。
pub fn overlaps(a: &str, b: &str) -> bool {
    if has_frag(a) || has_frag(b) {
        let (ra, rb) = (spec_region(a), spec_region(b));
        if !ra.is_whole() || !rb.is_whole() {
            return crate::region::conflicts(&ra, &rb, crate::region::SAFE_BAND);
        }
    }
    seg_overlap(&segments(a), &segments(b))
}

/// 本文の走査 ([`Lease::live_span_of`]) を何回通ったかを数えるカウンタ。
///
/// 1 回が**ファイル全長の走査**なので、担当が N 人居るところで不用意に
/// 呼ぶと確保 1 回の費用が N 倍になる。時間ではなく**回数**で固定するのは、
/// 絶対時間の線が Docker の仮想 FS でも同時実行でも必ず嘘をつくため。
///
/// **プロセス共通の `static` にしない。** 同時に走っている他のテストの
/// 呼び出しまで混ざる (実績あり)。
#[cfg(test)]
mod scan_count {
    use std::cell::Cell;

    thread_local! {
        // `live_span_of` を通った回数
        static HITS: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn hit() {
        HITS.with(|c| c.set(c.get().saturating_add(1)));
    }
    pub(super) fn reset() {
        HITS.with(|c| c.set(0));
    }
    pub(super) fn get() -> u64 {
        HITS.with(Cell::get)
    }
}

/// **交錯の関所。** 帯だけでは足りない唯一の形をここで止める。
///
/// [`crate::region::conflicts`] は**組ごと**の判定で、それは今も正しい。
/// 足りないのは
///
/// > 「全部の組が帯を満たす ⇒ まとめてマージしても綺麗に通る」
///
/// という推論のほうで、片方が相手を上下から挟んでいる (交錯) と、反復的な
/// 本文では帯を何行取っても `git merge` が衝突する (ort は diff アルゴリズムを
/// histogram に固定していて、同じ側の複数の変更を 1 つの巨大なハンクへ畳む)。
/// 実測は [`crate::region::anchor_lines`] の doc にある。
///
/// 返り値 `true` = 通してよい。`a` / `b` は**同じ 1 つのファイル**に対する
/// 2 人ぶんの行域一覧 (持ち主ごとにまとめて渡すこと — 1 組ずつ渡すと交錯は
/// 定義できない)。
///
/// # 元テキストが読めないときは **fail-closed** (断る)
///
/// 錨 ([`crate::region::anchor_lines`]) は元の本文からしか数えられない。
/// 読めない (存在しない / [`KEY_GATE_READ_CAP`] 超え / バイナリ) ときに
/// 「帯だけ」へ落とすと、**いちばん判定が効いてほしい場面 — 生成物・
/// データファイル・行数の多い反復的なファイル — でだけ静かに緩む**。
/// この製品が主張しているのは「一撃でマージできる」ことなので、証明できない
/// ものを通す側へ倒すと主張そのものが嘘になる。空の錨を必ず `false` にする
/// [`crate::region::interleave_safe`] と同じ向きである。
///
/// 断られた側の逃げ道は安い (連続した 1 本の域にする / 別ファイルにする /
/// `zai lease claim --shift`) ので、fail-closed の代償は小さい。
/// **黙って断らないこと** — 文面は [`interleave_reason`] が出す。
///
/// # 費用
///
/// 0.16.0 までは [`crate::region::anchor_lines`] (ファイル全体の走査) を
/// **交錯している組が実際にあるときだけ**呼んでいた。その門は見逃すことが
/// 実測で分かったので ([`crate::region::needs_wall`])、いまは**同じファイルを
/// 他人が持っている組すべて**で払う。関所は書き込みのたびに走る短命プロセス
/// なので、呼び出し側は**ファイルにつき 1 回**へまとめること
/// (`crate::guard::bracket_hit` の `text` / `crate::czero` の `text` 表が手本)。
pub fn interleave_ok(
    text: Option<&str>,
    a: &[crate::region::Span],
    b: &[crate::region::Span],
) -> bool {
    if !crate::region::needs_wall(a, b) {
        return true; // 片方が空 = 境目が無い
    }
    interleave_ok_anchors(text.map(crate::region::anchor_lines).as_deref(), a, b)
}

/// [`interleave_ok`] の、**錨を数え終えている**版。
///
/// # なぜ分けるのか — O(N²) の全長走査になっていた
///
/// 門が [`crate::region::needs_wall`] になって「同じファイルを持つ組すべて」を
/// 見るようになったので、持ち主ごとに [`crate::region::anchor_lines`]
/// (ファイル全長の走査 + `BTreeMap` の構築) を払うと **持ち主 N 人 × 確保 N 回**
/// になる。実測: 1 ファイル 2000 行へ 16/32/64/128 体が確保する試験が
/// **86.8 秒**まで伸びて nextest の 60 秒に届いた。
/// 錨は**ファイルにつき 1 回**数えれば足りる (`bracket_conflict` が呼び出し前に
/// 1 回だけ数える)。直した後は 2.5 秒。
///
/// `anchors` が `None` (= 本文を読めなかった) なら必ず `false` = fail-closed。
pub fn interleave_ok_anchors(
    anchors: Option<&[bool]>,
    a: &[crate::region::Span],
    b: &[crate::region::Span],
) -> bool {
    if !crate::region::needs_wall(a, b) {
        return true;
    }
    match anchors {
        Some(x) => crate::region::interleave_safe(x, a, b),
        None => false,
    }
}

/// 交錯で断るときの文面。
///
/// **「近すぎます」と同じ顔をさせない。** 交錯は*離しても直らない*
/// (帯を広げるとむしろ悪化する組が実測で出ている) ので、利用者が
/// 「もう少しずらせば通る」と読める文面は嘘になる。`coedit::Reason::Bracketed`
/// と同じことを言っている。
///
/// `known_text` が `false` なら、断った理由が「錨が無い」ではなく
/// 「**数えられなかった**」であることまで出す — 劣化したことを黙らせない。
pub fn interleave_reason(known_text: bool) -> String {
    if known_text {
        tr("交錯しています: 相手の行域との境目に「このファイルで 1 回しか出てこない行」がありません。離しても直りません (帯を広げると悪化する組が実測であります) — 連続した 1 本の行域にするか、別のファイルにしてください")
    } else {
        trf(
            "交錯しています: 相手の行域と同じファイルですが、元の内容を読めなかったので境目の手がかりの行を数えられませんでした (安全側で断ります。読み取り上限は設定 {key} で上げられます)。連続した 1 本の行域にするか、別のファイルにしてください",
            &[("key", KEY_GATE_READ_CAP.to_string())],
        )
    }
}

fn seg_overlap(a: &[String], b: &[String]) -> bool {
    match (a.first(), b.first()) {
        (None, None) => true,
        // 片方が尽きたら、残りが全部 `**` (= 0 個に当たれる) のときだけ重なる。
        (None, Some(_)) => b.iter().all(|s| s == "**"),
        (Some(_), None) => a.iter().all(|s| s == "**"),
        (Some(x), Some(y)) => {
            if x == "**" {
                return seg_overlap(&a[1..], b) || seg_overlap(a, &b[1..]);
            }
            if y == "**" {
                return seg_overlap(a, &b[1..]) || seg_overlap(&a[1..], b);
            }
            seg_intersects(x, y) && seg_overlap(&a[1..], &b[1..])
        }
    }
}

/// 1 セグメントぶんのパターン同士が、共通の文字列に当たり得るか (DP)。
///
/// 素朴な再帰は `*` が並ぶと指数になるので、到達可能な `(i, j)` を
/// 幅優先で 1 回だけ塗る。
fn seg_intersects(x: &str, y: &str) -> bool {
    let a: Vec<char> = x.chars().collect();
    let b: Vec<char> = y.chars().collect();
    let (n, m) = (a.len(), b.len());
    let idx = |i: usize, j: usize| i * (m + 1) + j;
    let mut seen = vec![false; (n + 1) * (m + 1)];
    let mut stack = vec![(0usize, 0usize)];
    seen[0] = true;
    while let Some((i, j)) = stack.pop() {
        if i == n && j == m {
            return true;
        }
        let mut push = |i: usize, j: usize, stack: &mut Vec<(usize, usize)>| {
            if !seen[idx(i, j)] {
                seen[idx(i, j)] = true;
                stack.push((i, j));
            }
        };
        match (a.get(i), b.get(j)) {
            (Some('*'), _) => {
                // `*` は 0 文字で終わる / 相手の 1 文字を飲む
                push(i + 1, j, &mut stack);
                if j < m {
                    push(i, j + 1, &mut stack);
                }
            }
            (_, Some('*')) => {
                push(i, j + 1, &mut stack);
                if i < n {
                    push(i + 1, j, &mut stack);
                }
            }
            (Some(&ca), Some(&cb)) => {
                if ca == '?' || cb == '?' || ca == cb {
                    push(i + 1, j + 1, &mut stack);
                }
            }
            // 片方だけ尽きた: 残りが全部 `*` なら空文字に当たれる
            (None, Some(_)) => {
                if b[j..].iter().all(|c| *c == '*') {
                    return true;
                }
            }
            (Some(_), None) => {
                if a[i..].iter().all(|c| *c == '*') {
                    return true;
                }
            }
            (None, None) => return true,
        }
    }
    false
}

// ═══════════════════════════════════════════════════════════════════════════
//  2. スコープ — 「どのリポジトリの話か」
// ═══════════════════════════════════════════════════════════════════════════

/// linked worktree の `.git` ファイルの中身から、**元のリポジトリのルート**を出す。
///
/// 中身は `gitdir: <元のリポジトリ>/.git/worktrees/<名前>` の 1 行。
/// `worktrees/<名前>` を 2 つ落とすと `.git` に戻り、その親が元のルート。
/// 形が違えば `None` (推測しない)。
/// `Path::is_absolute` は**動いている OS の規則**でしか判定しない。
/// Windows で作られた `.git` を unix 側から読むと `C:/…` が「相対」に見え、
/// 基準ディレクトリを頭に足してしまう。両方の綴りを絶対として扱う。
fn is_absolute_any(p: &str) -> bool {
    let b = p.as_bytes();
    p.starts_with('/')
        || p.starts_with('\\')
        || (b.len() >= 2 && b[1] == b':' && b[0].is_ascii_alphabetic())
}

/// git ディレクトリが **submodule のもの**か (`…/.git/modules/<名前>`)。
///
/// 名前は多段になり得る (`vendor/sub` / `deep/nest/sub`) ので**段数を
/// 数えない**。`.git` の直後が `modules` かどうかだけを見る。
/// 入れ子の submodule (`…/.git/modules/foo/modules/bar`) も同じ判定で
/// 拾えて、いちばん内側の git ディレクトリがそのまま鍵になる。
///
/// **区切りは OS 依存にしない** — Windows で作られたポインタを unix から
/// 読むことがあるので、[`Path::components`] ではなく正規化した文字列で見る。
fn is_submodule_gitdir(gitdir: &Path) -> bool {
    let norm = gitdir.to_string_lossy().replace('\\', "/");
    let segs: Vec<&str> = norm.split('/').filter(|s| !s.is_empty()).collect();
    segs.windows(2)
        .any(|w| w[0].ends_with(".git") && w[1] == "modules")
}

pub fn main_repo_root_from_pointer(text: &str, dot_dir: &Path) -> Option<PathBuf> {
    let line = text
        .lines()
        .find_map(|l| l.trim().strip_prefix("gitdir:"))?;
    let raw = PathBuf::from(line.trim());
    // **`gitdir:` は相対で書かれることがある** (submodule は git が常に相対で
    // 書く)。基準は「`.git` ファイルが置かれているディレクトリ」であって
    // プロセスの作業フォルダではない。ここを取り違えると、`cwd` が `/` の
    // ときにキーが `/` という**全世界共通のバケツ**になる (実測で踏んだ)。
    let gitdir = if is_absolute_any(line.trim()) {
        raw
    } else {
        dot_dir.join(raw)
    };
    // git は linked worktree の gitdir に必ず `commondir` を置く。中身は
    // 共有 git ディレクトリへの (多くは相対) パス。
    // **`.git` という名前を決め打ちしない**のが肝で、bare リポジトリから
    // 生やした worktree では共有側が `.git` ではない。決め打ちしていたため
    // **並列エージェント運用で最も勧められる「bare + worktree 群」で保証が
    // 丸ごと消えていた** (しかも無言で)。
    let common = std::fs::read_to_string(gitdir.join("commondir"))
        .ok()
        .map(|t| {
            let t = t.trim().to_string();
            if Path::new(&t).is_absolute() {
                PathBuf::from(t)
            } else {
                gitdir.join(t)
            }
        });
    // **submodule は親と台帳を分ける。** git は submodule の実体を
    // `<親>/.git/modules/<名前>` へ置く。ここを親のルートへ畳むと:
    //
    // * パスは `Roots::tree` (= submodule のフォルダ) 基準で相対化されるので、
    //   submodule の `f.txt` が親の台帳へ**ただの `f.txt`** として載る。
    //   親自身の `f.txt` と、さらに**別の submodule の `f.txt` とも**
    //   同じ鍵になり、衝突しようのない組を止める (偽陽性)
    // * 逆に本当に守りたい組 (親から見た `vendor/sub/f.txt`) とは鍵が違うので、
    //   畳んでも真陽性は 1 件も増えない
    //
    // submodule は独立したリポジトリで、コミットもマージも別に進む。
    // **マージ衝突が起き得る単位＝リポジトリ**なので、台帳もそこで割る。
    //
    // 鍵に git ディレクトリ (`…/.git/modules/<名前>`) を使うのは、
    // submodule から生やした linked worktree が `commondir` 経由で
    // **同じ値**に着くため。これで「submodule 本体 + その worktree 群」が
    // 1 つの台帳を共有する (同じリポジトリなので衝突し得る) という、
    // 通常のリポジトリとまったく同じ規則になる。
    //
    // 直していたのは**非対称**である: `gitdir: ../.git/modules/flat` は
    // 親へ畳まれるのに `../../.git/modules/vendor/sub` は畳まれない、
    // という取り違えが実バイナリで再現していた (名前の段数で挙動が変わる)。
    //
    // `commondir` があるとき (submodule から生やした worktree) はそれが
    // submodule の git ディレクトリそのものなので、**両方の入口が同じ値**に
    // 着く。無ければ `gitdir` 自身が submodule の git ディレクトリ。
    let base = common.clone().unwrap_or_else(|| gitdir.clone());
    if is_submodule_gitdir(&base) {
        return Some(canonical_best_effort(&base));
    }
    let Some(common) = common else {
        // `commondir` が読めない = git が置いた worktree ではない (あるいは
        // 非常に古い git)。**ここで形を推測しない** — 従来どおり
        // `…/.git/worktrees/<名前>` の形にだけ合わせ、違えば `None` を返す。
        // 緩めると、worktree でもない `.git` ファイルから見当違いの
        // ルートを作ってしまう。
        let git = gitdir.parent()?.parent()?;
        if git.file_name().and_then(|s| s.to_str()) != Some(".git") {
            return None;
        }
        return git.parent().map(Path::to_path_buf);
    };
    let common = canonical_best_effort(&common);
    // 共有 git ディレクトリが `.git` なら**その親**が作業リポジトリのルート。
    // そうでなければ (bare) 共有ディレクトリ自身をキーにする。
    if common.file_name().and_then(|s| s.to_str()) == Some(".git") {
        common.parent().map(Path::to_path_buf)
    } else {
        Some(common)
    }
}

/// 台帳のキーになるルートと、パスを相対化する作業ツリーのルート。
///
/// **この 2 つは linked worktree では別物**で、そこを取り違えると機能が
/// 丸ごと無言で効かなくなる (実際に e2e で踏んだ: worktree のファイルは
/// 元のリポジトリの配下に**無い**ので、元リポジトリ基準の相対化が必ず失敗し、
/// 全部「スコープ外」として素通りしていた)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Roots {
    /// 台帳のキー = **元のリポジトリのルート**。worktree 群が 1 つの台帳を共有する。
    pub key: PathBuf,
    /// パスの相対化に使う = **いまいる作業ツリーのルート**。
    pub tree: PathBuf,
    /// ルートを**確定できた**か (`.git` があった / 既に台帳がある)。
    ///
    /// `false` = git 管理下でもなく既存の台帳も無い ＝ **推測で選んだ**。
    /// 台帳を新しく作る操作 (`claim` / `enable`) はここで断って、
    /// `--dir` の明示か `git init` を促す。読むだけの操作 (`status` /
    /// `list`) は従来どおり動く (断ると現状が見えなくなるため)。
    ///
    /// **既存の台帳は「利用者が以前ここをルートに選んだ」証拠**なので
    /// 確定扱いにする。そうしないと、`--dir` で 1 度決めたあと配下から
    /// 打つたびに断られ続ける (実バイナリで踏んだ)。
    pub rooted: bool,
}

/// 与えられた場所から [`Roots`] を出す。
///
/// 1. 上へ辿って最初の `.git` を探す → そこが `tree`
/// 2. `.git` がファイルなら linked worktree → `key` は元のリポジトリへ寄せる
///    (**ここを寄せないと worktree ごとに台帳が割れて、この機能の意味が消える**)
/// 3. `.git` が見つからなければ、その場所自身 (git 管理でないフォルダでも動く)
///
/// 返り値は必ず同じ正規形にする。片方だけ canonicalize すると、macOS の
/// `/var` → `/private/var` のようなシンボリックリンクで同じリポジトリが
/// 2 つのキーへ割れる (これもテストで踏んだ)。
pub fn roots_of(start: &Path) -> Roots {
    let ((key, tree), rooted) = roots_raw_full(start);
    Roots {
        key: key.canonicalize().unwrap_or(key),
        tree: tree.canonicalize().unwrap_or(tree),
        rooted,
    }
}

/// ルートの生の探索。返り値の `bool` は「`.git` で決まったのか」。
///
/// `false` = `.git` がひとつも見つからず、**フォルダを推測で選んだ**。
/// 呼び出し元 (CLI) はここを見て、黙って別の台帳を生やす代わりに
/// 明示的に断れる ([`Roots::git`])。
fn roots_raw_full(start: &Path) -> ((PathBuf, PathBuf), bool) {
    roots_raw_with(start, &|p| store_path_in(&store_dir(), p).exists())
}

/// 台帳の在処を差し替えられる [`roots_raw_full`]。
///
/// **テストが実 `~/.zaivern` を触らないため**に要る (既定の探索は
/// ホームの台帳フォルダを stat するので、素のままでは検査できない)。
fn roots_raw_with(start: &Path, has_store: &dyn Fn(&Path) -> bool) -> ((PathBuf, PathBuf), bool) {
    let base = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in base.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return ((dir.to_path_buf(), dir.to_path_buf()), true);
        }
        if dot.is_file() {
            let main = std::fs::read_to_string(&dot)
                .ok()
                .and_then(|t| main_repo_root_from_pointer(&t, dir))
                .unwrap_or_else(|| dir.to_path_buf());
            return ((main, dir.to_path_buf()), true);
        }
    }
    // ── git 管理でない ────────────────────────────────────────────────
    // **サブフォルダごとに別の台帳を生やさない。** 以前はここで
    // `(cwd, cwd)` を返していたので、`/work` と `/work/a` と `/work/a/b` が
    // **3 つの別々の台帳**になり、同じファイルを見ている 2 人が互いに
    // 見えなかった (実バイナリで再現済み: 鍵が 3 つに割れた)。
    //
    // 既に台帳がある祖先まで上がって、**そこへ寄せる**。既存の台帳は
    // 「利用者がここをルートとして選んだ」という唯一の手掛かりで、
    // 推測ではない。見つからなければ「git でも既存の台帳でもない」と
    // 判る形 (`false`) で返し、断るかどうかは呼び出し元が決める。
    for anc in base.ancestors() {
        if has_store(anc) {
            // 既に台帳がある = 利用者が以前ここをルートに選んだ。**確定**。
            return ((anc.to_path_buf(), anc.to_path_buf()), true);
        }
    }
    ((base.clone(), base), false)
}

/// **まだ存在しないパスでも**実在する祖先まで解決する canonicalize。
///
/// 素の [`Path::canonicalize`] は存在しないパスで失敗する。そこで諦めると
/// **`Write` による新規ファイル作成が丸ごと素通りする** — 台帳側は
/// canonicalize 済み (macOS なら `/private/var/…`) なのに、対象だけ
/// 生のパス (`/var/…`) のままになり、前方一致が必ず外れるため。
fn canonical_best_effort(p: &Path) -> PathBuf {
    if let Ok(c) = p.canonicalize() {
        return c;
    }
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    while let Some(name) = cur.file_name().map(|s| s.to_os_string()) {
        let Some(parent) = cur.parent().map(Path::to_path_buf) else {
            break;
        };
        rest.push(name);
        if let Ok(c) = parent.canonicalize() {
            let mut out = c;
            for r in rest.iter().rev() {
                out.push(r);
            }
            return out;
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        cur = parent;
    }
    p.to_path_buf()
}

/// ルートからの相対パス (正規形)。ルートの外なら `None` = **関知しない**。
pub fn rel_within(root: &Path, target: &Path) -> Option<String> {
    let t = canonical_best_effort(target);
    let s = canonical_best_effort(root);
    let rel = t.strip_prefix(&s).ok()?;
    let norm = normalize_path(&rel.to_string_lossy());
    if norm.is_empty() {
        None
    } else {
        Some(norm)
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. 台帳の型
// ═══════════════════════════════════════════════════════════════════════════

/// 持ち主。**ベンダーのセッション ID が第一の身元**で、無ければ作業フォルダ。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Holder {
    /// 画面と拒否理由に出す名前 (エージェント名 / セッション名)。
    #[serde(default)]
    pub agent: String,
    /// ベンダーが振ったセッション ID。空なら `cwd` で照合する。
    #[serde(default)]
    pub session: String,
    /// 作業フォルダ (正規形)。
    #[serde(default)]
    pub cwd: String,
    /// 生存確認に使う PID。0 = 確認手段なし (TTL だけで回収)。
    #[serde(default)]
    pub pid: u32,
}

impl Holder {
    /// 画面に出す 1 行の名前。
    pub fn display(&self) -> String {
        if self.agent.is_empty() {
            tr("(名前なし)")
        } else if self.session.is_empty() {
            self.agent.clone()
        } else {
            let short: String = self.session.chars().take(8).collect();
            format!("{} #{short}", self.agent)
        }
    }

    /// 同じ持ち主か。**セッション ID が両方にあるならそれだけで決める** —
    /// 同じフォルダで 2 セッション走っていても取り違えない。
    pub fn same(&self, other: &Holder) -> bool {
        if !self.session.is_empty() && !other.session.is_empty() {
            return self.session == other.session;
        }
        !self.cwd.is_empty() && self.cwd == other.cwd && self.agent == other.agent
    }
}

/// リース 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Lease {
    pub holder: Holder,
    /// 所有するパターン (スコープからの相対、`/` 区切り、glob 可)。
    #[serde(default)]
    pub patterns: Vec<String>,
    /// `patterns[i]` に対応する錨 (**行番号ではなく中身**で域を指すため)。
    ///
    /// ## なぜ並行する欄なのか
    /// `patterns` の中身は `zai lease list` と画面にそのまま出る文字列で、
    /// ここへ錨を混ぜると人が読めなくなる。**別の欄に並べて持つ**と
    /// 表記は 1 バイトも変わらず、`#[serde(default)]` のおかげで
    /// **錨を知らない古い台帳もそのまま読める**
    /// ([`span_tests::錨を知らない古い台帳をそのまま読み書きできる`])。
    ///
    /// 長さは `patterns` に合わせるのが不変条件だが、**ずれていても落ちない** —
    /// 手で編集された台帳や別バージョンが書いた台帳が来るので、
    /// [`Lease::anchor_at`] が足りない分を「錨なし」として読む。
    /// [`read_store`] が読み込みの一点で [`Lease::align_anchors`] を通す。
    #[serde(default)]
    pub anchors: Vec<crate::region::Anchor>,
    /// 確保した時刻 (UNIX 秒)。
    #[serde(default)]
    pub acquired_at: u64,
    /// 期限 (UNIX 秒)。
    #[serde(default)]
    pub expires_at: u64,
    /// 何のための確保か (拒否理由に出す)。
    #[serde(default)]
    pub note: String,
}

impl Lease {
    /// このリースが具体的なパスを覆うか。**行域は見ない** ([`covers`] と同じ
    /// 理由で、行番号を知らない呼び出し元には安全側の答えを返す)。
    pub fn covers_path(&self, rel: &str) -> bool {
        self.patterns.iter().any(|p| covers(p, rel))
    }

    /// このリースが `rel` の `touched` 行に**関わる**か ([`covers_span`])。
    ///
    /// `text` は `rel` の**いまの中身**。渡すと、台帳の行域を錨で取り直してから
    /// 見る (遅延解決)。`None` なら台帳の行番号をそのまま使う。
    pub fn touches(&self, rel: &str, touched: &[crate::region::Span], text: Option<&str>) -> bool {
        self.patterns.iter().enumerate().any(|(i, p)| {
            if text.is_none() || self.anchors.get(i).is_none_or(|a| a.is_blank()) {
                // 錨が無い = 取り直しようが無い。錨が入る前とまったく同じ経路。
                return covers_span(p, rel, touched);
            }
            covers(p, rel) && hits(self.live_span_of(i, text), touched)
        })
    }

    /// `patterns[i]` に対応する錨。**長さがずれた台帳でも落ちない**
    /// (足りない分は「錨なし」= 台帳の行番号をそのまま使う)。
    pub fn anchor_at(&self, i: usize) -> crate::region::Anchor {
        self.anchors.get(i).cloned().unwrap_or_default()
    }

    /// `patterns[i]` を錨つきの [`crate::region::Region`] として取り出す。
    pub fn region_at(&self, i: usize) -> crate::region::Region {
        let mut r = spec_region(self.patterns.get(i).map(String::as_str).unwrap_or(""));
        r.anchor = self.anchor_at(i);
        r
    }

    /// 錨の並びを `patterns` の長さへ揃える (足りなければ空で埋め、余りは落とす)。
    pub fn align_anchors(&mut self) {
        self.anchors.resize(self.patterns.len(), Default::default());
    }

    /// `patterns[i]` が**いま**指している行域。`None` = ファイル全体。
    ///
    /// # 取り直せなかったらどちらへ倒すか
    ///
    /// [`crate::region::resolve`] が `None` (域が丸ごと消えた / 同じ内容の行が
    /// 同距離に複数あって決められない) を返したら、**台帳に書いてある行番号を
    /// そのまま使う**。落とす先は 3 つ考えられて、選ばなかった 2 つには
    /// はっきりした害がある:
    ///
    /// * 「誰のものでもない」— **2 人が同じ場所を書けるようになる**。
    ///   このモジュールが売っている保証そのものが消えるので論外。
    /// * 「ファイル全体」— 持ち主が**自分の域の先頭行を直しただけ**で錨は外れる
    ///   (いちばん普通の編集)。そのたびに同じファイルの他の担当を全員締め出すので、
    ///   並列度がファイル数で頭打ちになり、行域を入れた意味が消える。
    /// * **記録された行番号** — 錨が入る前とまったく同じ判定。既に出荷している
    ///   保証と同じ強さで、過剰でも過少でもない。ここへ倒す。
    fn live_span_of(&self, i: usize, text: Option<&str>) -> Option<crate::region::Span> {
        #[cfg(test)]
        scan_count::hit();
        let r = self.region_at(i);
        if r.is_whole() {
            return None;
        }
        let recorded = r.span?;
        let Some(t) = text else {
            return Some(recorded);
        };
        Some(crate::region::resolve(&r, t).unwrap_or(recorded))
    }

    /// `rel` について、このリースが持っている行域。
    ///
    /// `None` = **ファイル全体**を持っている (行域で切り分ける必要が無い)。
    /// `Some(vec![])` = このパスは 1 行も持っていない。
    /// 並びはパターンの登録順のまま = 決定的。
    /// `text` を渡すと、錨で取り直した**いまの**行域を返す。
    pub fn owned_spans(&self, rel: &str, text: Option<&str>) -> Option<Vec<crate::region::Span>> {
        let mut out = Vec::new();
        for (i, p) in self.patterns.iter().enumerate() {
            if !covers(p, rel) {
                continue;
            }
            match self.live_span_of(i, text) {
                None => return None,
                Some(s) => out.push(s),
            }
        }
        Some(out)
    }

    /// まだ効いているか。
    ///
    /// 期限内なら当然有効。期限切れでも**持ち主のプロセスが生きている間は
    /// [`RECLAIM_GRACE_SECS`] だけ猶予する** (エージェントが戻ってきたときに
    /// 所有を奪い返されないため)。猶予に上限があるのが肝で、PID 再利用で
    /// 「生きている」と誤判定しても永久には残らない。
    pub fn active(&self, now: u64, alive: &dyn Fn(u32) -> bool) -> bool {
        if now < self.expires_at {
            return true;
        }
        if self.holder.pid == 0 {
            return false;
        }
        now <= self.expires_at.saturating_add(RECLAIM_GRACE_SECS) && alive(self.holder.pid)
    }
}

/// 1 スコープぶんの台帳。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Store {
    #[serde(default)]
    pub leases: Vec<Lease>,
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 純粋な判断 (I/O を一切しない — テーブルテストで固定する部分)
// ═══════════════════════════════════════════════════════════════════════════

/// 確保の結果。**競合したら fail-closed** で、後勝ちにはしない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Claim {
    /// 取れた (件数は新たに足したパターン数)。
    Granted(usize),
    /// 他人が持っている。
    Refused {
        owner: String,
        pattern: String,
        until: u64,
    },
}

/// 失効したリースを落とす。
pub fn prune(store: &mut Store, now: u64, alive: &dyn Fn(u32) -> bool) {
    store.leases.retain(|l| l.active(now, alive));
}

// ── 確保したい 1 件 (仕様 + 錨 + いまの中身) ──────────────────────────────

/// 確保したい 1 件。**錨を打つのは確保のこの瞬間だけ。**
///
/// ## なぜ文字列だけでは足りないのか
/// 台帳に載るのは `src/a.rs#L200-260` という**行番号**だが、他人が 100 行目へ
/// 10 行足した瞬間にその中身は 210〜270 行目へ動く。行番号しか持っていないと、
/// 次の判定で持ち主は**他人の領域を自分のものだと思い込む**。
/// 確保の瞬間に先頭行・末尾行の中身を [`crate::region::capture_anchor`] で
/// 覚えておき、判定のたびに [`crate::region::resolve`] で取り直す。
///
/// `text` は「いまのファイルの中身」。判定側が**台帳に載っている他人の域**を
/// 取り直すのにも使うので、読んだ 1 回ぶんを持ち回る
/// ([`gate`] は行域を出すのに既に読んでいる — 2 度読まない)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Want {
    /// 確保したい仕様 (`src/a.rs` / `src/a.rs#L10-40`)。
    pub spec: String,
    /// 確保の瞬間の錨。空 = 行番号だけで持つ (ファイル全体 / ファイルが読めない)。
    pub anchor: crate::region::Anchor,
    /// 判定に使う「いまの中身」。`None` = 読めない / 要らない。
    pub text: Option<std::sync::Arc<str>>,
}

impl Want {
    /// 実ファイルを見ない 1 件 (ファイル全体の担当 / 行域を追わない確保)。
    pub fn plain(spec: &str) -> Want {
        Want {
            spec: spec.to_string(),
            anchor: crate::region::Anchor::default(),
            text: None,
        }
    }
}

/// 仕様を**実ファイルへ突き合わせて**確定させる。
///
/// | 書き方 | すること | ファイルが読めないとき |
/// |---|---|---|
/// | `src/a.rs` | 何もしない (I/O ゼロ) | — |
/// | `src/a.rs#L10-40` | [`crate::region::capture_anchor`] で錨を打つ | 錨なしで確保する |
/// | `src/a.rs#fn:draw_toolbar` | [`crate::region::resolve_spec`] → [`crate::region::symbol_span`] で行域へ落とす | **`Err`** |
///
/// ## 記号指定だけ `Err` にする理由
/// 行域指定は「錨が無い = 昔と同じ判定」へ落ちるだけなので確保して構わない。
/// 記号指定は**行域に落ちないと意味が決まらない**。黙ってファイル全体として
/// 扱うと、「関数 1 つ取った」つもりの本人が他の 63 体を締め出す。黙って
/// 捨てると、取れたつもりで誰にも守られない。**その場で断るのが唯一正しい。**
///
/// 記号は **Rust だけ**を見る ([`crate::region::SYMBOL_KINDS`])。他言語まで
/// 広げると誤検出が増えてユーザーが機能ごと切る、というのがこのリポジトリの流儀。
pub fn hydrate_in(tree: &Path, spec: &str) -> Result<Want, String> {
    if !has_frag(spec) {
        return Ok(Want::plain(spec)); // 圧倒的多数。1 バイトも読まない
    }
    let parsed = match crate::region::parse_spec(spec.trim()) {
        Ok(p) => p,
        // 壊れた指定は [`spec_region`] と同じくファイル全体として扱う
        // (ここで断ると、`#` を含む実在のパスが確保できなくなる)。
        Err(_) => return Ok(Want::plain(spec)),
    };
    if matches!(parsed.sel, crate::region::Sel::Whole) {
        return Ok(Want::plain(spec)); // 行番号に依存しない = 錨も要らない
    }
    let body = match read_capped_ex(&tree.join(&parsed.path), tree) {
        FileRead::Text(b) => b,
        // **行域 (`#L10-20`) は中身を読まなくても確保できる。**
        // 錨が無いだけで、域そのものは台帳へそのまま載る
        // (= 錨が入る前の版とまったく同じ判定)。記号指定 (`#fn:`) だけが
        // 解析を要するので、そちらだけを断る。
        other => {
            let crate::region::Sel::Symbol { kind, name } = parsed.sel else {
                return Ok(Want::plain(spec));
            };
            // **理由を取り違えない。** 「上限超え」を「読めませんでした」と
            // 出していたため、健在な 1.8MB のファイルに対して
            // 直しようのない拒否が出ていた。
            return Err(match other {
                FileRead::TooLarge(size, cap) => trf(
                    "{kind} {name} を探せません: 「{path}」は {size} バイトで上限 {cap} バイトを超えます (行番号での指定 (例: {path}#L10-20) なら確保できます。上限は設定 {key} で変えられます)",
                    &[
                        ("kind", kind),
                        ("name", name),
                        ("path", parsed.path.clone()),
                        ("size", size.to_string()),
                        ("cap", cap.to_string()),
                        ("key", KEY_GATE_READ_CAP.to_string()),
                    ],
                ),
                _ => trf(
                    "{kind} {name} を探せません: 「{path}」を読めませんでした",
                    &[
                        ("kind", kind),
                        ("name", name),
                        ("path", parsed.path.clone()),
                    ],
                ),
            });
        }
    };
    let region = crate::region::resolve_spec(&parsed, &body)?;
    Ok(Want {
        spec: crate::region::render(&region),
        anchor: region.anchor,
        text: Some(std::sync::Arc::from(body)),
    })
}

/// 行域の指定が**実際には行単位で守られない**なら、その理由を返す。
///
/// ## なぜ要るのか
/// 確保は通る (`src/big.rs#L10-20` は台帳へそのまま載る) のに、
/// **フック側 ([`gate`]) は上限を超えたファイルの行域を出せない**ので、
/// 判定が [`decide_spans`] から [`decide`] へ落ちて**ファイル全体**になる。
/// 実バイナリで再現済み: 1.8MB のファイルで `#L10-20` を持っている相手が
/// いると、**900 行目への書き込みまで拒否**される。
/// しかも拒否文には「行単位で見られなかった」と 1 文字も出ないので、
/// 利用者は「なぜ離れた行なのに止まるのか」を知る手段が無い。
///
/// 確保のときに**先に**言う。黙って劣化させない。
pub fn degradation_note(tree: &Path, spec: &str) -> Option<String> {
    let path = spec_path(spec);
    if spec_span(spec).is_none() {
        return None; // ファイル全体の確保 = 劣化のしようが無い
    }
    if path.contains(['*', '?', '[']) {
        return None; // どのファイルを指すか確定しない
    }
    match read_capped_ex(&tree.join(path), tree) {
        FileRead::TooLarge(size, cap) => Some(trf(
            "「{path}」は {size} バイトで上限 {cap} バイトを超えます — このファイルは**行単位ではなくファイル全体**として守られます (同じファイルの離れた行を他の人が取れません)。設定 {key} を上げると行単位に戻ります",
            &[
                ("path", path.to_string()),
                ("size", size.to_string()),
                ("cap", cap.to_string()),
                ("key", KEY_GATE_READ_CAP.to_string()),
            ],
        )),
        _ => None,
    }
}

/// 仕様を実ファイルへ突き合わせるときの基準フォルダ。
///
/// **[`Holder::cwd`] は使えない** — [`normalize_path`] が先頭の `/` を落として
/// 大小も畳むので、あれはもはやファイルシステムのパスではない (台帳の鍵専用)。
/// 基準を明示できる呼び出し元 ([`gate`] / [`arm_in`]) は [`try_claim_in`] を
/// 使い、明示できない `zai lease claim` だけがここへ落ちる。
/// **`zai lease claim --dir <別のフォルダ>` と記号指定の組み合わせは、
/// ここでプロセスの作業フォルダを見るので当たらない** (既知の限界)。
fn default_tree() -> PathBuf {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    roots_of(&cwd).tree
}

/// 2 つの行域を包む最小の域。片方でも末尾までなら末尾まで。
fn hull(a: crate::region::Span, b: crate::region::Span) -> crate::region::Span {
    crate::region::Span {
        start: a.start.min(b.start),
        end: if a.end == crate::region::Span::EOF || b.end == crate::region::Span::EOF {
            crate::region::Span::EOF
        } else {
            a.end.max(b.end)
        },
    }
}

/// 持ち物へ 1 件足す。**同じファイルの近接した行域は畳む。** 変化したら `true`。
///
/// ## なぜ畳むのか
/// フックは書き込みのたびに「今回触れた域」を確保する。畳まないと
/// 台帳が 1 書き込みにつき 1 行ずつ伸び、`try_claim` の重なり走査
/// (O(リース数 × パターン数)) がじわじわ重くなる。
/// **安全帯 ([`crate::region::SAFE_BAND`]) 以内の 2 つの域は、他人から見れば
/// どのみち衝突扱い**なので、包んでも誰の自由も奪わない。
///
/// 1 回の吸収で新たに届く域が出る (A↔B は遠いが A∪C↔B は近い) ため、
/// 変化が止まるまで繰り返す。並びは **(start, end) の昇順**で決定的。
/// 2 つの錨を包む。**片方でも錨が無いなら結果も錨なし** —
/// 半端な錨 (先頭だけ / 末尾だけ) は [`crate::region::resolve`] が必ず断るので、
/// 「錨がある」と嘘をつくより「無い」と言うほうが判定がぶれない。
fn hull_anchor(
    a: (crate::region::Span, &crate::region::Anchor),
    b: (crate::region::Span, &crate::region::Anchor),
) -> crate::region::Anchor {
    if a.1.is_blank() || b.1.is_blank() {
        return crate::region::Anchor::default();
    }
    let h = hull(a.0, b.0);
    let head = if a.0.start <= b.0.start { a.1 } else { b.1 };
    // 末尾までの域が混ざったらそちらが末尾を決める。
    let tail_is_a = a.0.end == crate::region::Span::EOF
        || (b.0.end != crate::region::Span::EOF && a.0.end >= b.0.end);
    let tail = if tail_is_a { a.1 } else { b.1 };
    crate::region::Anchor {
        head: head.head.clone(),
        tail: tail.tail.clone(),
        len: if h.end == crate::region::Span::EOF {
            a.1.len.max(b.1.len)
        } else {
            h.len()
        },
    }
}

fn absorb(pats: &mut Vec<String>, ancs: &mut Vec<crate::region::Anchor>, want: &Want) -> bool {
    let w = spec_region(&want.spec);
    let text = want.text.as_deref();
    let mut same: Vec<(crate::region::Span, crate::region::Anchor)> = Vec::new();
    for (i, p) in pats.iter().enumerate() {
        let mut r = spec_region(p);
        if r.path != w.path {
            continue;
        }
        if r.is_whole() {
            // 既にファイル全体を持っている = 行域を足す意味が無い
            return false;
        }
        r.anchor = ancs.get(i).cloned().unwrap_or_default();
        let recorded = r.span.unwrap_or(crate::region::Span { start: 1, end: 1 });
        // **持ち主自身の域は、確保のたびに「いまの座標」へ書き直す。**
        // 他人の帳簿には 1 行も触らないので、これは `region::follow` を
        // 全担当へ撒く eager な追従とは別物 (落ちても誰の台帳もずれない)。
        //
        // ## ただし「動いた」と読めても、**元の場所を手放してはいけない**
        //
        // 錨は確保した瞬間の中身で、**持ち主が自分でその行を書き換えると
        // 自分の錨が合わなくなる**。すると [`crate::region::resolve`] は
        // 似た行 (README の空行・```・--- など実ファイルには山ほどある) に
        // 当たり、域が**別の場所へ移ったこと**にしてしまう。
        //
        // 実測 (`tools/anyrepo-prove.sh --repo hyperframes --writers 8`,
        // 種 20260818): w1 が `README.md#L21` を確保 → 21 行目を書く →
        // **同じファイルの別の域を確保し直した瞬間に L21 が L15 へ移動** し、
        // 空いた 21 行目を w2 が正当に確保して**2 人が同じ行を書いた**。
        // 台帳の最終形に重なりは残らない (移動しただけ) ので、
        // 台帳を見ても気付けない — いちばん静かな壊れ方だった。
        //
        // そこで**動いたと読めたら、その読みを採らない**。
        //
        // 包んで (`hull`) 両方持つことも試したが、伸びた域が**他人が既に
        // 持っている域を飲み込んで**しまい、台帳の不変条件 (どの 2 人の
        // 担当も重ならない) が壊れた (実測: 種 20260818 で `overlaps=1`)。
        // 元の場所に留めるのが、伸びも移動もしない唯一の安全な選択。
        //
        // **追従が消えるわけではない。** 他人から見た判定は
        // [`overlaps_live`] が判断のたびに錨で取り直すので、行が本当に
        // ずれていても保護は効く。ここで書き換えるのは*自分の帳簿の座標*
        // だけで、それを動かさないだけである。
        let live = match text {
            Some(t) => match crate::region::resolve(&r, t) {
                Some(moved) if moved == recorded => moved,
                // 「動いた」と読めた = 錨が別の行に当たった可能性がある。
                // 自分が書いた行を手放すより、留まるほうが必ず安全。
                _ => recorded,
            },
            None => recorded,
        };
        same.push((live, r.anchor));
    }
    let merged: Vec<(crate::region::Span, crate::region::Anchor)> = if w.is_whole() {
        // 足すのが「ファイル全体」なら、同じパスの行域は全部畳まれる
        Vec::new()
    } else {
        let mut cur = (
            w.span.unwrap_or(crate::region::Span { start: 1, end: 1 }),
            want.anchor.clone(),
        );
        let mut rest = same;
        loop {
            let mut hit = false;
            let mut next = Vec::new();
            for a in rest.drain(..) {
                if crate::region::spans_too_close(&a.0, &cur.0, crate::region::SAFE_BAND) {
                    let anchor = hull_anchor((a.0, &a.1), (cur.0, &cur.1));
                    cur = (hull(a.0, cur.0), anchor);
                    hit = true;
                } else {
                    next.push(a);
                }
            }
            rest = next;
            if !hit {
                break;
            }
        }
        rest.push(cur);
        rest.sort_by(|x, y| x.0.cmp(&y.0));
        rest.dedup_by(|x, y| x.0 == y.0);
        // 中身が手元にあるなら、畳んだ結果の錨は**その場で打ち直す**。
        // 畳んで出来た域の先頭行・末尾行は元のどちらとも違い得るので、
        // 継ぎ接ぎの錨より実測のほうが必ず正しい。
        if let Some(t) = text {
            for (s, a) in rest.iter_mut() {
                *a = crate::region::capture_anchor(t, s);
            }
        }
        rest
    };
    let render = |s: Option<crate::region::Span>| {
        crate::region::render(&crate::region::Region {
            path: w.path.clone(),
            span: s,
            anchor: crate::region::Anchor::default(),
        })
    };
    let place = |op: &mut Vec<String>, oa: &mut Vec<crate::region::Anchor>| {
        if merged.is_empty() {
            op.push(render(None));
            oa.push(crate::region::Anchor::default()); // ファイル全体に錨は要らない
        } else {
            for (s, a) in merged.iter() {
                op.push(render(Some(*s)));
                oa.push(a.clone());
            }
        }
    };
    // 同じパスの最初の位置へ畳んだ結果を置き、残りは落とす
    // (並べ直さないので、既に整っていれば台帳は 1 バイトも変わらない)。
    let mut out: Vec<String> = Vec::with_capacity(pats.len() + 1);
    let mut out_a: Vec<crate::region::Anchor> = Vec::with_capacity(pats.len() + 1);
    let mut placed = false;
    for (i, p) in pats.iter().enumerate() {
        if spec_region(p).path != w.path {
            out.push(p.clone());
            out_a.push(ancs.get(i).cloned().unwrap_or_default());
            continue;
        }
        if !placed {
            placed = true;
            place(&mut out, &mut out_a);
        }
    }
    if !placed {
        place(&mut out, &mut out_a);
    }
    // **「増えた件数」は昔どおりパターンだけで数える。** 錨を打ち直しただけの
    // 再確保を「1 件増えた」と報告すると、`zai lease claim` の出す数が嘘になる。
    let changed = out != *pats;
    *pats = out;
    *ancs = out_a;
    changed
}

/// パターン群を確保する。**1 つでも他人と重なれば 1 つも取らない** (全か無か)。
///
/// 全か無かにするのは、部分的に取れた状態がいちばん危ないため —
/// エージェントは「取れた」と思って作業を始め、取れなかったパスで衝突する。
///
/// `patterns` には `src/a.rs#L10-40` の書き方をそのまま渡せる
/// ([`normalize_spec`] が正規形へ寄せ、[`overlaps`] が行域で切り分ける)。
pub fn try_claim(
    store: &mut Store,
    holder: &Holder,
    patterns: &[String],
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Claim {
    // **`#` を含まない圧倒的多数は 1 バイトも読まない。** ここで基準フォルダを
    // 求めてしまうと、ファイル単位の確保 (GUI / 従来の使い方) にまで
    // `roots_of` の I/O を払わせることになる。
    if !patterns.iter().any(|p| has_frag(p)) {
        let wants: Vec<Want> = patterns.iter().map(|p| Want::plain(p)).collect();
        return try_claim_wants(store, holder, &wants, now, ttl, alive);
    }
    try_claim_in(&default_tree(), store, holder, patterns, now, ttl, alive)
}

/// 基準フォルダを明示する [`try_claim`]。
///
/// `tree` は**スコープ相対パスの起点** (= [`Roots::tree`])。`src/a.rs#fn:name`
/// のような記号指定はここからの相対で実ファイルを読んで行域へ落とす
/// ([`hydrate_in`])。記号が見つからない / ファイルが読めないときは
/// **1 件も確保せずに断る** (全か無か)。
pub fn try_claim_in(
    tree: &Path,
    store: &mut Store,
    holder: &Holder,
    patterns: &[String],
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Claim {
    let mut wants: Vec<Want> = Vec::with_capacity(patterns.len());
    for p in patterns {
        match hydrate_in(tree, p) {
            Ok(w) => wants.push(w),
            Err(reason) => {
                return Claim::Refused {
                    owner: reason,
                    pattern: p.clone(),
                    until: now,
                }
            }
        }
    }
    try_claim_wants(store, holder, &wants, now, ttl, alive)
}

/// 錨を打ち終えた [`Want`] で確保する。**実ファイルを読まない** =
/// 台帳ロックの内側で I/O が起きない ([`gate`] はここを直に呼ぶ)。
pub fn try_claim_wants(
    store: &mut Store,
    holder: &Holder,
    wants: &[Want],
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Claim {
    prune(store, now, alive);
    let wanted: Vec<Want> = wants
        .iter()
        .map(|w| Want {
            spec: normalize_spec(&w.spec),
            anchor: w.anchor.clone(),
            text: w.text.clone(),
        })
        .filter(|w| !w.spec.is_empty())
        .collect();
    for l in store.leases.iter().filter(|l| !l.holder.same(holder)) {
        for w in &wanted {
            if let Some((_, hit)) = l
                .patterns
                .iter()
                .enumerate()
                .find(|(i, p)| overlaps_live(p, &l.anchor_at(*i), &w.spec, w.text.as_deref()))
            {
                return Claim::Refused {
                    owner: l.holder.display(),
                    pattern: hit.clone(),
                    until: l.expires_at,
                };
            }
        }
    }
    // ── 交錯 ────────────────────────────────────────────────────────────
    // ここまでは**組ごと**の帯で、それは今も正しい。足りないのは
    // 「全部の組が帯を満たす ⇒ まとめてマージしても綺麗に通る」のほうで、
    // 片方が相手を上下から挟んでいると反復的な本文では帯を何行取っても
    // 衝突する。判定は [`interleave_ok`] 1 本 (関所ごとに書かない)。
    if let Some((owner, reason, until)) = interleave_clash(store, holder, &wanted) {
        return Claim::Refused {
            owner: reason,
            pattern: owner,
            until,
        };
    }
    let expires = now.saturating_add(ttl);
    if let Some(mine) = store.leases.iter_mut().find(|l| l.holder.same(holder)) {
        // **上限の検査は書き換える前に済ませる** (全か無かを壊さないため)。
        if mine.patterns.len().saturating_add(wanted.len()) > MAX_PATTERNS {
            return Claim::Refused {
                owner: tr("1 つのリースが持てるパターン数の上限に達しています"),
                pattern: tr(
                    "(不要な確保を解放してください: zai lease list で確認し、zai lease release)",
                ),
                until: now,
            };
        }
        // 錨の並びを patterns へ揃えてから触る (ずれた台帳でも落ちない)。
        mine.align_anchors();
        let mut added = 0;
        for w in &wanted {
            if absorb(&mut mine.patterns, &mut mine.anchors, w) {
                added += 1;
            }
        }
        mine.expires_at = mine.expires_at.max(expires);
        // 持ち主の表示名と PID は最後に見たものへ更新する (名前が付いた等)。
        if !holder.agent.is_empty() {
            mine.holder.agent = holder.agent.clone();
        }
        if holder.pid != 0 {
            mine.holder.pid = holder.pid;
        }
        return Claim::Granted(added);
    }
    if store.leases.len() >= MAX_LEASES {
        // 台帳が上限に達した。**ここを「許可」に倒してはいけない** —
        // 以前は `Granted(0)` を返していたが、それは「確保できていないのに
        // 取れたと言う」ことで、以後**全員が同じファイルへ通ってしまう**
        // (敵対的検証で実際に破られた)。上限は壊れた書き手への防壁なので、
        // 防壁に当たったら止めて人に知らせるのが正しい。
        return Claim::Refused {
            owner: tr("台帳が上限に達しています"),
            pattern: tr(
                "(台帳の掃除が必要です: zai lease list で確認し、不要な確保を解放してください)",
            ),
            until: now,
        };
    }
    // 新規のリースでも同じ畳み方を通す (同じ確保に近接した域が並ぶことがある)。
    let mut patterns: Vec<String> = Vec::with_capacity(wanted.len());
    let mut anchors: Vec<crate::region::Anchor> = Vec::with_capacity(wanted.len());
    let mut n = 0;
    for w in &wanted {
        if absorb(&mut patterns, &mut anchors, w) {
            n += 1;
        }
    }
    store.leases.push(Lease {
        holder: holder.clone(),
        patterns,
        anchors,
        acquired_at: now,
        expires_at: expires,
        note: String::new(),
    });
    Claim::Granted(n)
}

/// 台帳側の仕様を**いまのテキストで取り直してから**重なりを見る。
///
/// `text` は `mine` のパスの中身。**パスが一致するときだけ取り直す** —
/// 別ファイルのテキストで錨を探すのは意味が無いどころか、偶然同じ行が
/// あれば他人の域を勝手に動かす。glob が絡むときも取り直さない
/// (どのファイルを指すか確定しないので、[`overlaps`] の安全側判定に任せる)。
fn overlaps_live(
    theirs: &str,
    anchor: &crate::region::Anchor,
    mine: &str,
    text: Option<&str>,
) -> bool {
    let Some(t) = text else {
        return overlaps(theirs, mine);
    };
    if anchor.is_blank() || !has_frag(theirs) {
        return overlaps(theirs, mine);
    }
    let mut their_region = spec_region(theirs);
    if their_region.is_whole() || their_region.path != spec_path(mine) {
        return overlaps(theirs, mine);
    }
    let recorded = match their_region.span {
        Some(s) => s,
        None => return overlaps(theirs, mine),
    };
    their_region.anchor = anchor.clone();
    // 取り直せなければ記録された行番号へ落ちる (`Lease::live_span_of` と同じ規則)。
    let live = crate::region::resolve(&their_region, t).unwrap_or(recorded);
    crate::region::conflicts(
        &crate::region::Region {
            path: their_region.path,
            span: Some(live),
            anchor: crate::region::Anchor::default(),
        },
        &spec_region(mine),
        crate::region::SAFE_BAND,
    )
}

/// [`try_claim_wants`] の**交錯**検査。断るなら `(仕様, 理由, 期限)`。
///
/// 帯の検査 ([`overlaps_live`]) を全部通った**あとにだけ**呼ぶこと。
/// 帯で既に断っている組をここで数え直しても答えは変わらない。
///
/// ## なぜ持ち主ごとにまとめるのか
///
/// 交錯は「A の域が B の 2 つの域に挟まれている」という**集合の性質**で、
/// 1 組ずつ ([`overlaps`]) では定義できない。実際に穴だったのはここで、
/// `A={17}` `B={13,25}` はどの組も帯 (3 行) を満たすのに `git merge` は
/// 衝突する。
///
/// ## 費用
///
/// * glob / ファイル全体の要求は 1 バイトも見ない (帯側が安全に断っている)
/// * 具体パスが一致する他人のリースが無ければ [`interleave_ok`] を呼ばない
/// * [`crate::region::anchor_lines`] は [`interleave_ok`] の中で、
///   **本当に交錯している組があるときだけ**走る
fn interleave_clash(
    store: &Store,
    holder: &Holder,
    wanted: &[Want],
) -> Option<(String, String, u64)> {
    // 1. 要求を「具体パス」ごとにまとめる (行域を持つものだけ)。
    //    値は (自分の域, 判定に使う本文, 代表の仕様)。
    let mut mine: std::collections::BTreeMap<
        String,
        (Vec<crate::region::Span>, Option<&str>, &str),
    > = std::collections::BTreeMap::new();
    for w in wanted {
        if !has_frag(&w.spec) {
            continue; // ファイル全体 — 帯側が必ず断っている
        }
        let r = spec_region(&w.spec);
        let (Some(span), false) = (r.span, is_globby(&r.path)) else {
            continue;
        };
        let e = mine
            .entry(r.path.clone())
            .or_insert_with(|| (Vec::new(), None, w.spec.as_str()));
        e.0.push(span);
        if e.1.is_none() {
            e.1 = w.text.as_deref();
        }
    }
    if mine.is_empty() {
        return None;
    }
    // 2. 自分が既に持っている域も足す。**足さないと交錯を作れてしまう** —
    //    1 本ずつ取れば毎回「相手 1 人・自分 1 本」に見えるので、
    //    2 回目の確保で相手を挟んだことに誰も気付かない。
    //
    //    **他人が誰もそのパスに居なければ 1 バイトも読まない。**
    //    `live_span_of` は錨を本文から探し直す (= ファイル全長の走査) ので、
    //    「同じファイルに他人が居る」と分かってからでないと払えない。
    let contested: std::collections::BTreeSet<String> = mine
        .keys()
        .filter(|path| {
            store
                .leases
                .iter()
                .filter(|l| !l.holder.same(holder))
                .any(|l| l.patterns.iter().any(|p| covers(p, path)))
        })
        .cloned()
        .collect();
    if contested.is_empty() {
        return None;
    }
    if let Some(l) = store.leases.iter().find(|l| l.holder.same(holder)) {
        for (path, e) in mine.iter_mut() {
            if !contested.contains(path) {
                continue;
            }
            for (i, p) in l.patterns.iter().enumerate() {
                if !covers(p, path) {
                    continue;
                }
                if let Some(s) = l.live_span_of(i, e.1) {
                    e.0.push(s);
                }
            }
        }
    }
    // 錨は**ファイルにつき 1 回**数える。持ち主ごとに数えると
    // `anchor_lines` (ファイル全長の走査 + BTreeMap の構築) が
    // **持ち主 N 人 × 確保 N 回**になり、1 ファイル 2000 行へ 128 体が
    // 集まる試験が 86.8 秒まで伸びた (`interleave_ok_anchors` に実測)。
    let walls: std::collections::BTreeMap<String, Option<Vec<bool>>> = contested
        .iter()
        .map(|path| {
            let text = mine.get(path).and_then(|e| e.1);
            (path.clone(), text.map(crate::region::anchor_lines))
        })
        .collect();
    // 3. 他人ごとに、同じパスの域をまとめて突き合わせる。
    for l in store.leases.iter().filter(|l| !l.holder.same(holder)) {
        for (path, (my_spans, text, spec)) in &mine {
            // **1 本ずつでも壁は要る。** 0.16.0 まではここで
            // 「どちらも 1 本なら交錯は起こり得ない」と降りていた。交錯は
            // 起こり得なくても**壁は要る** — 削除・挿入が混ざると上下に
            // 分かれた組でも `git merge` は衝突する
            // (`crate::region::needs_wall` に実測)。降りてよいのは
            // 「相手がこのパスを 1 本も持っていない」ときだけ。
            let theirs_n = l.patterns.iter().filter(|p| covers(p, path)).count();
            if theirs_n == 0 {
                continue;
            }
            let mut theirs: Vec<crate::region::Span> = Vec::new();
            for (i, p) in l.patterns.iter().enumerate() {
                if !covers(p, path) {
                    continue;
                }
                match l.live_span_of(i, *text) {
                    // 丸ごと持たれている = 帯側が既に断っている。
                    None => return None,
                    Some(s) => theirs.push(s),
                }
            }
            if theirs.is_empty() {
                continue;
            }
            let wall = walls.get(path).and_then(|w| w.as_deref());
            if !interleave_ok_anchors(wall, my_spans, &theirs) {
                return Some((
                    (*spec).to_string(),
                    interleave_reason(text.is_some()),
                    l.expires_at,
                ));
            }
        }
    }
    None
}

/// glob 記号を含むか (`crate::region` 側の同名判定と同じ規則)。
fn is_globby(p: &str) -> bool {
    p.contains('*') || p.contains('?') || p.contains('[')
}

/// `path` を覆っている担当の**いまの**行域を集める。
///
/// * `None` — 誰かがそのファイルを**丸ごと**持っている (= ずらす先が無い)
/// * `Some(v)` — 埋まっている域の一覧。並びは台帳の順のまま = 決定的
///
/// `keep` が `false` を返したリースは数えない (自分の担当は自分を止めない)。
/// `text` は `path` の**いまの中身**。台帳の行番号は他人が上へ行を足した
/// 瞬間に古くなるので、錨で取り直してから空きを探す
/// ([`Lease::live_span_of`] と同じ規則で、取り直せなければ記録値へ落ちる)。
fn busy_spans(
    store: &Store,
    path: &str,
    text: Option<&str>,
    keep: impl Fn(&Lease) -> bool,
) -> Option<Vec<crate::region::Span>> {
    let mut busy: Vec<crate::region::Span> = Vec::new();
    for l in store.leases.iter().filter(|l| keep(l)) {
        for (i, p) in l.patterns.iter().enumerate() {
            if !covers(p, path) {
                continue;
            }
            match l.live_span_of(i, text) {
                None => return None, // 丸ごと持たれている
                Some(s) => busy.push(s),
            }
        }
    }
    Some(busy)
}

/// **他人の担当を 1 回だけ取り直して、「ぶつかるか」と「埋まっている域」を
/// 同時に出す。**
///
/// ## なぜ 1 回にこだわるのか (台帳ロックの長さがそのまま拒否になる)
/// 素直に書くと [`overlaps_live`] (ぶつかるか) と [`busy_spans`] (どこが
/// 埋まっているか) で**同じ錨を 2 度取り直す**。[`crate::region::resolve`] は
/// 1 回ごとにテキスト全体を行へ割り直すので、2000 行 × 63 担当では
/// 1 件あたり **50ms**、64 体を直列に通すと台帳ロックを **3 秒**握る。
/// そこまで長いと `with_store_retry` の予算を食い潰し、**衝突でも拒否でもない
/// busy** が出る (実測: 全テスト同時実行下で 64 件中 **33 件が busy**)。
/// 拒否を潰すために作った機能が別の形の拒否を作っては意味が無い。
///
/// 返り値は `(ぶつかるか, 埋まっている域)`。域が `None` =
/// **行で切り分けられない** (誰かが丸ごと持っている / glob で持っている)
/// = ずらす先が無い。
///
/// `path` / `want` は**具体パスの、長さが決まっている行域**であること
/// (ファイル全体 / glob / 末尾までの要求はそもそもずらせないので、
///  呼び出し側が [`overlaps_live`] の素の走査へ倒す)。
/// 判定は [`try_claim_wants`] が使う [`overlaps_live`] と一致する —
/// 具体パスに対しては `covers` と `overlaps` のパス判定が同じ答えを出し、
/// 丸ごと / glob は安全側 (= ぶつかる) へ倒すため。
fn live_view(
    store: &Store,
    holder: &Holder,
    path: &str,
    want: crate::region::Span,
    text: Option<&str>,
) -> (bool, Option<Vec<crate::region::Span>>) {
    let mut taken = false;
    let mut busy: Vec<crate::region::Span> = Vec::new();
    for l in store.leases.iter().filter(|l| !l.holder.same(holder)) {
        for (i, p) in l.patterns.iter().enumerate() {
            if !covers(p, path) {
                continue;
            }
            // glob で持たれている域は「どのファイルの何行目か」が確定しないので
            // 行では切り分けられない ([`crate::region::conflicts`] と同じ安全側)。
            if p.contains(['*', '?', '[']) {
                return (true, None);
            }
            match l.live_span_of(i, text) {
                None => return (true, None), // 丸ごと持たれている
                Some(s) => {
                    if crate::region::spans_too_close(&s, &want, crate::region::SAFE_BAND) {
                        taken = true;
                    }
                    busy.push(s);
                }
            }
        }
    }
    (taken, Some(busy))
}

/// テキストの行数。**空 / 読めない / `u32` に収まらないときは `None`**。
///
/// 「行数が分からないならずらさない」を 1 か所で決めるための関門。
/// 知らない場所を勧めると、台帳の上では取れているのに**書く先が無い**
/// という、いちばん気付きにくい壊れ方になる。
fn line_count(text: Option<&str>) -> Option<u32> {
    let n = text?.lines().count();
    if n == 0 {
        return None;
    }
    u32::try_from(n).ok()
}

/// **ファイル全体の空きを総なめして、要求と同じ幅が入るいちばん近い場所を返す。**
///
/// ## なぜ「直後 / 直前 / 先頭」の 3 候補では足りなかったのか
/// 以前の候補は占有域の直後・直前・先頭だけだった。詰まった配置ではその 3 つが
/// すべて埋まっていて、**空きが 1868 行あるのに 53 件が断られた**
/// (実測: `tools/coedit-bench.sh --layout crowded`。2000 行のファイルへ
/// 64 体が幅 6 行を stride 2 で要求する条件。要求が集中しているのは
/// 934〜1065 行の 132 行だけで、64 体を互いに素に置くのに要るのは 573 行)。
/// ここでは占有域を畳んだ**隙間の一覧**を作り、隙間ごとに「要求開始行に
/// いちばん近い開始位置」を 1 つ取る。O(n log n) で、空きを 1 つも見落とさない。
///
/// ## 引数
/// * `busy` — いま埋まっている域 (錨で取り直したあとの座標)
/// * `want` — 欲しい域。**挿入点 (幅 0) は幅 1 の点として置き、挿入点で返す**
///   ([`crate::region::spans_too_close`] が挿入点を点として扱うのと同じ寄せ方)
/// * `total` — ファイルの行数。**ここを超えた場所は返さない。**
///   行数を知らない呼び出し (拘束力の無い提案) は [`u32::MAX`] を渡す
/// * `band` — 安全帯 ([`crate::region::SAFE_BAND`])
///
/// ## 決定性
/// 近いほうが勝ち、同点なら**行番号が小さいほう**。集合は `Vec` だけで持ち、
/// `HashMap` / `HashSet` を一切通さないので、どの OS のどのプロセスでも
/// 1 バイト違わない答えが出る (64 体が同じ台帳を見て別の答えを出すと、
/// 「ずらしたのに重なる」が起きる)。
fn fit_span(
    busy: &[crate::region::Span],
    want: crate::region::Span,
    total: u32,
    band: u32,
) -> Option<crate::region::Span> {
    use crate::region::Span;
    if want.end == Span::EOF || want.is_empty() {
        return None; // 長さが決まらない
    }
    let insert = want.is_insert();
    let len = if insert { 1 } else { want.len() };
    if len == 0 || len > total {
        return None; // ファイルより広い域は入らない
    }
    // 占有域を閉区間へ落とす。EOF まで伸びている域は末尾までとして扱う。
    let mut iv: Vec<(u32, u32)> = busy
        .iter()
        .map(|b| {
            let (s, e) = if b.is_insert() {
                (b.start, b.start)
            } else {
                (b.start, b.end)
            };
            (s, if e == Span::EOF { total } else { e.min(total) })
        })
        .filter(|(s, _)| *s <= total)
        .collect();
    iv.sort_unstable();
    let mut occupied: Vec<(u32, u32)> = Vec::with_capacity(iv.len());
    for (s, e) in iv {
        match occupied.last_mut() {
            // 重なっている / 隣接しているものだけ畳む (畳まなくても答えは
            // 同じだが、隙間の数が減るぶん走査が短くなる)。
            Some(p) if s <= p.1.saturating_add(1) => p.1 = p.1.max(e),
            _ => occupied.push((s, e)),
        }
    }
    // 開始位置として許される閉区間を左から並べる。
    // 手前へ置く条件: `s + len - 1 <= b0 - band - 1` → `s <= b0 - band - len`
    // 後ろへ置く条件: `s >= b1 + band + 1`
    let last = total.saturating_sub(len).saturating_add(1); // 開始位置の上限
    let mut gaps: Vec<(u32, u32)> = Vec::with_capacity(occupied.len() + 1);
    let mut lo = 1u32;
    for (s, e) in &occupied {
        let hi = s.saturating_sub(band).saturating_sub(len).min(last);
        if lo <= hi {
            gaps.push((lo, hi));
        }
        // 入れ子・重なりがあっても後退させない (並びは start 昇順なので
        // `max` を取れば「ここまでは埋まっている」を正しく持ち越せる)。
        lo = lo.max(e.saturating_add(band).saturating_add(1));
    }
    if lo <= last {
        gaps.push((lo, last));
    }
    let mut best: Option<(u32, u32)> = None; // (要求からの距離, 開始行)
    for (a, b) in gaps {
        let s = want.start.clamp(a, b);
        let d = s.abs_diff(want.start);
        if best.is_none_or(|(bd, bs)| d < bd || (d == bd && s < bs)) {
            best = Some((d, s));
        }
    }
    let (_, start) = best?;
    Some(if insert {
        Span::insert_before(start)
    } else {
        Span {
            start,
            end: start.saturating_add(len - 1),
        }
    })
}

/// **断る代わりに「ずらす」提案を出す純関数。**
///
/// 拒否は正しいが、拒否しか返せないと並列度は上がらない。実測 (crowded 条件)
/// では行域リースが 53 件を断っており、そのうち多くは
/// **空いている行へずらせば通る**。ここはその候補を 1 つ返す。
///
/// * `None` — ずらす必要が無い (`want` がそのまま取れる) か、
///   ずらしようが無い (ファイル全体 / 末尾までの域 / glob /
///   **誰かがそのファイルを丸ごと持っている** / どこにも入らない)
/// * `Some(r)` — `r` なら誰とも重ならない。**長さは `want` と同じ**
///
/// 探し方は [`fit_span`] — ファイル全体の空きを見て、`want.start` に
/// いちばん近い場所、同点なら**行番号が小さいほう**。
///
/// **`store` は呼び出し側が [`prune`] 済みであること** — 引数に `now` が
/// 無いのは、交渉層 (メッシュ) が判定済みの台帳を渡す前提だから。
/// `text` は `want.path` の**いまの中身**。台帳の行域を錨で取り直してから
/// 空きを探す — 古い行番号のまま提案すると、提案どおり確保しても弾かれる。
/// `text` があれば**その行数を超えた場所は勧めない**。無ければ上限なしで
/// 探す (提案には拘束力が無いので、黙るより出すほうが情報が多い)。
pub fn suggest_alternative(
    store: &Store,
    want: &crate::region::Region,
    text: Option<&str>,
) -> Option<crate::region::Region> {
    use crate::region::{Span, SAFE_BAND};
    let span = want.span?; // ファイル全体はずらせない
    if span.end == Span::EOF || span.is_empty() {
        return None; // 長さが決まらない
    }
    if want.path.contains(['*', '?', '[']) {
        return None; // glob はどのファイルを指すか確定しない
    }
    let busy = busy_spans(store, &want.path, text, |_| true)?;
    if busy
        .iter()
        .all(|b| !crate::region::spans_too_close(b, &span, SAFE_BAND))
    {
        return None; // そのまま取れる
    }
    let alt = fit_span(&busy, span, line_count(text).unwrap_or(u32::MAX), SAFE_BAND)?;
    Some(crate::region::Region {
        path: want.path.clone(),
        span: Some(alt),
        anchor: crate::region::Anchor::default(),
    })
}

// ── 断らない確保 (`zai lease claim --shift`) ────────────────────────────

/// 1 件ぶんの確保結果。**要求と実際が違い得る**のがこの型の存在理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Grant {
    /// 要求 (錨を打ったあとの正規形)。
    pub asked: String,
    /// 実際に確保した仕様。ずらしていなければ [`Grant::asked`] と同じ。
    pub spec: String,
}

impl Grant {
    /// ずらしたか。
    pub fn moved(&self) -> bool {
        self.asked != self.spec
    }
}

/// [`try_claim_wants_shift`] の結果。
///
/// [`Claim`] を増やさずに別の型にしたのは、**`--shift` を付けない経路の
/// 挙動を 1 バイトも変えない**ため ([`Claim`] は GUI・フック・既存テストが
/// 網羅的に `match` している)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ShiftClaim {
    /// 全部取れた。**要求と同じ並び**で、実際に確保した仕様が入る。
    Granted(Vec<Grant>),
    /// どこへずらしても入らない (= [`Claim::Refused`] とまったく同じ文面)。
    Refused {
        owner: String,
        pattern: String,
        until: u64,
    },
}

/// **断らずにずらして確保する** ([`try_claim_in`] の `--shift` 版)。
///
/// `tree` はスコープ相対パスの起点 (= [`Roots::tree`])。仕様は
/// [`hydrate_in`] で実ファイルへ突き合わせてから確保するので、
/// 記号指定 (`src/a.rs#fn:draw`) もずらせる (行域へ落ちたあとで動かす)。
// 引数が 8 本ある。束ねると 17 箇所の呼び出しを書き換えることになり、
// **ずらし上限を足すという 1 点の変更**に対して差分が大きくなりすぎる。
// このリポジトリの既存の流儀 (deck / terminal / whichkey) に合わせて許可する。
#[allow(clippy::too_many_arguments)]
pub fn try_claim_shift_in(
    tree: &Path,
    store: &mut Store,
    holder: &Holder,
    patterns: &[String],
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
    max_shift: Option<u32>,
) -> ShiftClaim {
    let mut wants: Vec<Want> = Vec::with_capacity(patterns.len());
    for p in patterns {
        match hydrate_in(tree, p) {
            Ok(w) => wants.push(w),
            Err(reason) => {
                return ShiftClaim::Refused {
                    owner: reason,
                    pattern: p.clone(),
                    until: now,
                }
            }
        }
    }
    try_claim_wants_shift(store, holder, &wants, now, ttl, alive, max_shift)
}

/// 設定キー: ずらしてよい幅の既定 (行)。
///
/// **交渉層の `negotiate::KEY_MAX_SHIFT` と同じ文字列**を指す。定数を
/// import せず綴りを持つのは、実体の `src/negotiate.rs` が
/// `src/features/negotiate.rs` の**私有 `mod imp`** として `#[path]` で
/// 取り込まれていて、クレート外からは辿れないから (担当外のファイルなので
/// re-export も足さない)。綴りがずれたら黙って既定値へ落ちて
/// 「設定したのに効かない」になるので、
/// [`tests::ずらし幅の設定キーは交渉層と同じ綴り`] が突き合わせる。
const KEY_MAX_SHIFT: &str = "negotiate.max_shift";

/// ずらしてよい幅の既定の上限 (行)。設定 [`KEY_MAX_SHIFT`] から取る。
///
/// **交渉層と同じ値を使う。** あちらは `zai negotiate ask --max-shift` から
/// 届くのに、`zai lease claim --shift` だけが**無制限**だった
/// (出荷経路がいちばん緩い、という最悪の形)。1 万行ずらされたら、
/// 利用者の意図とまったく無関係な場所を確保してしまう。
pub fn default_max_shift_in(root: &Path) -> u32 {
    crate::config::load(std::slice::from_ref(&root.to_path_buf()), false)
        .feature_i64(KEY_MAX_SHIFT)
        .clamp(0, i64::from(u32::MAX)) as u32
}

/// **ずらしてよい幅の実効上限。** 設定値に「いま実際に場所を奪っているぶん」を足す。
///
/// ## 固定値が必ず破綻する理由 (実測)
///
/// 設定 [`KEY_MAX_SHIFT`] は「**空いている**ファイルでどこまで飛んでよいか」、
/// つまり人間のレビュー局所性の表明である。ところが実際にずらす距離を
/// 決めているのは利用者ではなく**同時に来た人数**で、`n` 体が幅 `w` を
/// 安全帯 `b` で並べれば、端の 1 体は `n(w+b)/2` 行ずれる。固定値 `m` は
/// そこから「`n ≤ 2m/(w+b)` までしか通さない」という**誰も表明していない
/// 上限**を勝手に作る。
///
/// 実測 (`tools/coedit-bench.sh --agents 64 --lines 2000 --layout crowded`、
/// 6 回): 既定 200 行のままだと段 C+ は **51〜54 完了 / 10〜13 拒否**で、
/// stderr には「上限は 200 行です」だけが出ていた。`--max-shift 600` を
/// 渡すと 64/64。**空きは 1868 行あり、断る理由はどこにも無かった。**
///
/// ## 何を足すか
///
/// **他人 1 人が押しのけてよいのは、その人の域の幅 (＋安全帯 2 本) まで。**
/// ただし 1 人あたりの寄与は設定値で頭打ちにする — さもないと
/// 「1 人が 9000 行持っている」だけで上限が実質無効になり、
/// 「頼んだ場所と何の関係も無い所を確保しない」という元の保護が消える
/// ([`crate::features::negotiate`] の
/// `tests::断った理由の内訳が取れる` が番人)。
///
/// ## 帰結 (これが「N がいくつまで大丈夫か」への答え)
///
/// * `busy` が空 (空いているファイル) → **設定値そのまま**。従来と 1 行も変わらない
/// * `n` 体が並んでいる → 上限は `m + n(min(w,m) + 2b)`。**n に比例して伸びる**。
///   必要距離は `n(w+b)/2` なので、`w ≤ m` である限り伸びは必要量の
///   **2 倍以上**で、`n` がいくつでも上限が先に尽きることはない
///   ([`tests::書き手を倍にしても成立率が落ちない`] が 32/64/128 体で固定)
/// * `w > m` (1 人の域が設定値より広い) のときは `n = 2` の時点で当たる。
///   **「しばらく通ってから急に落ちる」という静かな崖にはならない**ので、
///   利用者は最初の 1 回で気付ける (文面に必要な数を出す)
/// * ファイル行数で頭打ち。本当に入らないときは「遠すぎる」ではなく
///   「空きが無い」として断る (理由の内訳が嘘にならない)
/// * `configured == 0` は「**ずらすな**」の意思表示なので 0 のまま
pub fn shift_ceiling(
    configured: u32,
    busy: &[crate::region::Span],
    band: u32,
    file_lines: u32,
) -> u32 {
    if configured == 0 {
        return 0; // 「ずらすな」を混雑で覆さない
    }
    let mut reach = u64::from(configured);
    for s in busy {
        let w = if s.is_insert() { 1 } else { s.len() };
        reach = reach
            .saturating_add(u64::from(w.min(configured)))
            .saturating_add(2 * u64::from(band));
    }
    // ファイルの外へはずらせないので、行数より上を許しても意味が無い。
    // ただし設定値そのものは下回らせない (行数の分からない呼び出しで
    // 「設定したのに効かない」を作らないため)。
    let cap = u64::from(file_lines).max(u64::from(configured));
    reach.min(cap) as u32
}

/// **埋まっていたら空いている場所へずらして確保する。** 実ファイルを読まない
/// ([`try_claim_wants`] と同じく、台帳ロックの内側で I/O が起きない)。
///
/// ## なぜ「拒否」を潰す必要があったのか
/// 行域オーナーシップは離れた域なら 64 体が 1 ファイルへ同時に書ける。
/// 残っていた唯一の弱点が**拒否**で、crowded 条件 (2000 行へ 64 体が
/// 幅 6 行を stride 2 で要求) では **完了 11 / 拒否 53**。ところが
/// 要求が集中しているのは 132 行ぶんだけで、**空きは 1868 行**あり、
/// 64 体を互いに素に置くのに要るのは 573 行しかない。
/// **断られていたのは空きが無いからではなく、誰もずらしていなかったから。**
/// [`suggest_alternative`] は提案までしていたのに、受け取って確保し直す側が
/// 居なかった。ここがその受け手である。
///
/// ## 守っている不変条件
/// * **全か無か** — 1 件でもずらす先が無ければ 1 件も取らない。
///   台帳の書き換えは最後の [`try_claim_wants`] 1 回だけなので、
///   途中で諦めても台帳は 1 バイトも変わらない
/// * **互いに素** — ずらした先は他人の域からも、*この確保の中で先に置いた域*
///   からも安全帯ぶん離す。台帳は常に
///   [`crate::region::is_disjoint`] を満たす
/// * **決定的** — 位置決めは [`fit_span`] (`Vec` だけ、`HashMap` 無し)。
///   同じ台帳・同じ要求からは、どの OS のどのプロセスでも同じ答えが出る
/// * **ずらせないものはずらさない** — ファイル全体 (そのファイルは 1 つしか
///   無い) / 末尾までの域 / glob / **行数が分からないファイル**
///
/// ## 錨は打ち直す
/// ずらした先の錨は必ず [`crate::region::capture_anchor`] で取り直す。
/// 元の錨は元の行の中身なので、そのまま持たせると次の取り直しで
/// **他人の域へ吸い寄せられる** (= 静かに保証が破れる)。
pub fn try_claim_wants_shift(
    store: &mut Store,
    holder: &Holder,
    wants: &[Want],
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
    max_shift: Option<u32>,
) -> ShiftClaim {
    use crate::region::SAFE_BAND;
    prune(store, now, alive);
    // この 1 回の確保で先に置いた域 (パスごと)。**`BTreeMap`** — `HashMap` は
    // 走査順が実行ごとに変わるので、同じ要求から違う配置が出てしまう。
    let mut placed: std::collections::BTreeMap<String, Vec<crate::region::Span>> =
        std::collections::BTreeMap::new();
    let mut planned: Vec<Want> = Vec::with_capacity(wants.len());
    let mut grants: Vec<Grant> = Vec::with_capacity(wants.len());
    for w in wants {
        let asked = normalize_spec(&w.spec);
        if asked.is_empty() {
            continue; // `try_claim_wants` と同じ扱い (空の指定は無かったことに)
        }
        let text = w.text.as_deref();
        let region = spec_region(&asked);
        let here: &[crate::region::Span] = placed
            .get(&region.path)
            .map(Vec::as_slice)
            .unwrap_or_default();
        // **ずらせる形か**を先に決める。ここに「ずらさない条件」を集める:
        //
        // | 条件 | なぜずらさないのか |
        // |---|---|
        // | ファイル全体 (`span == None`) | そのファイルは 1 つしか無い |
        // | 末尾までの域 (`#L10-`) | 長さが決まらない |
        // | glob (`src/*.rs#L1-5`) | どのファイルを指すか確定しない |
        let movable = match region.span {
            Some(s)
                if s.end != crate::region::Span::EOF
                    && !s.is_empty()
                    && !region.path.contains(['*', '?', '[']) =>
            {
                Some(s)
            }
            _ => None,
        };
        let (taken, busy) = match movable {
            // ずらせる形 = 錨の取り直しを 1 回で済ませる速い経路
            Some(s) => live_view(store, holder, &region.path, s, text),
            // ずらせない形 = 判定だけを従来どおりの走査で出す
            None => (
                store
                    .leases
                    .iter()
                    .filter(|l| !l.holder.same(holder))
                    .any(|l| {
                        l.patterns
                            .iter()
                            .enumerate()
                            .any(|(i, p)| overlaps_live(p, &l.anchor_at(i), &asked, text))
                    }),
                None,
            ),
        };
        // 同じ確保の中で先に置いた域とも重ねない。
        let taken = taken
            || match region.span {
                None => false, // ファイル全体は自分の担当と畳まれるだけ
                Some(s) => here
                    .iter()
                    .any(|p| crate::region::spans_too_close(p, &s, SAFE_BAND)),
            };
        if !taken {
            if let Some(s) = region.span {
                placed.entry(region.path.clone()).or_default().push(s);
            }
            planned.push(Want {
                spec: asked.clone(),
                anchor: w.anchor.clone(),
                text: w.text.clone(),
            });
            grants.push(Grant {
                asked: asked.clone(),
                spec: asked,
            });
            continue;
        }
        // ── 埋まっている。空いている場所を探す ──────────────────────
        // **行数が分からないファイルへはずらさない** ([`line_count`]) —
        // 知らない場所を勧めると、台帳の上では取れているのに書く先が無い。
        // ずらす先と、**その距離を決めている混雑ぶん**を同時に出す。
        // 上限の判定 ([`shift_ceiling`]) には「誰がどれだけ塞いでいるか」が要る。
        let mut crowd: Vec<crate::region::Span> = Vec::new();
        let mut total_lines = 0u32;
        let alt = match (movable, busy) {
            (Some(s), Some(mut b)) => line_count(text).and_then(|total| {
                b.extend_from_slice(here);
                total_lines = total;
                let got = fit_span(&b, s, total, SAFE_BAND);
                crowd = b;
                got
            }),
            _ => None,
        };
        // **ずらし幅に上限を効かせる。** 上限が無いと、詰まった台帳では
        // 1 万行離れた場所が「いちばん近い空き」になり得る。そこは利用者が
        // 頼んだ場所と何の関係も無いので、確保しても意味が無いどころか
        // **無関係な他人の作業域を先取りする**。
        //
        // ただし上限は**固定値ではない** — [`shift_ceiling`] が「他人が
        // いま実際に押しのけているぶん」を足す。固定値のままだと、
        // 「同時に何体まで通すか」を誰も表明していないのに決めてしまう
        // (実測: 既定 200 行で 64 体中 10〜13 体が断られていた)。
        //
        // 断るときは**具体的な数**を出す (「入りません」だけでは、
        // 上限を上げれば通るのか、そもそも空きが無いのかが判らない)。
        if let (Some(a), Some(cfg), Some(s)) = (alt, max_shift, movable) {
            let dist = a.start.abs_diff(s.start);
            let limit = shift_ceiling(cfg, &crowd, SAFE_BAND, total_lines);
            if dist > limit {
                return ShiftClaim::Refused {
                    owner: trf(
                        // **先頭に `:` を含む見出しを置く。** `cli::refusal` は
                        // 「持ち主の名前」と「断る理由」を `:` の有無で見分けて
                        // 文型を変えるので、`:` が無いと
                        // 「…通ります **が持っています**」という意味の通らない
                        // 文になる (実バイナリで実際にそう出た)。
                        "ずらせる上限に当たりました: {dist} 行ずらす必要がありますが、上限は {limit} 行です (設定 {key} の {cfg} 行 ＋ 他人の域 {n} 件ぶんの混雑 {crowd} 行)。`--max-shift {dist}` を渡すか、設定 {key} を {dist} 以上にすると通ります",
                        &[
                            ("dist", dist.to_string()),
                            ("limit", limit.to_string()),
                            ("key", KEY_MAX_SHIFT.to_string()),
                            ("cfg", cfg.to_string()),
                            ("n", crowd.len().to_string()),
                            ("crowd", limit.saturating_sub(cfg).to_string()),
                        ],
                    ),
                    pattern: asked,
                    until: now,
                };
            }
        }
        let Some(alt) = alt else {
            // ずらす先が無い。**文面は `--shift` 無しとまったく同じ**にする
            // (拒否の理由が 2 通りあると、読む側が原因を切り分けられない)。
            return match try_claim_wants(store, holder, wants, now, ttl, alive) {
                Claim::Refused {
                    owner,
                    pattern,
                    until,
                } => ShiftClaim::Refused {
                    owner,
                    pattern,
                    until,
                },
                // ここへは来ないはずだが、取れたなら取れたと言う
                // (起こり得ない前提で嘘の拒否を作らない)。
                Claim::Granted(_) => ShiftClaim::Granted(
                    wants
                        .iter()
                        .map(|w| normalize_spec(&w.spec))
                        .filter(|s| !s.is_empty())
                        .map(|s| Grant {
                            asked: s.clone(),
                            spec: s,
                        })
                        .collect(),
                ),
            };
        };
        let spec = normalize_spec(&crate::region::render(&crate::region::Region {
            path: region.path.clone(),
            span: Some(alt),
            anchor: crate::region::Anchor::default(),
        }));
        let anchor = match text {
            Some(t) => crate::region::capture_anchor(t, &alt),
            None => crate::region::Anchor::default(),
        };
        placed.entry(region.path.clone()).or_default().push(alt);
        planned.push(Want {
            spec: spec.clone(),
            anchor,
            text: w.text.clone(),
        });
        grants.push(Grant { asked, spec });
    }
    match try_claim_wants(store, holder, &planned, now, ttl, alive) {
        Claim::Granted(_) => ShiftClaim::Granted(grants),
        // 全か無か: ここで断られても台帳は書き換わっていない。
        Claim::Refused {
            owner,
            pattern,
            until,
        } => ShiftClaim::Refused {
            owner,
            pattern,
            until,
        },
    }
}

/// 持ち主のリースを手放す。返り値は消した件数。
pub fn release(store: &mut Store, holder: &Holder) -> usize {
    let before = store.leases.len();
    store.leases.retain(|l| !l.holder.same(holder));
    before - store.leases.len()
}

/// フックの答え。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 通す (自分のもの / 誰も持っていない)。
    Allow,
    /// 止める。文面は**そのままエージェントとユーザーに見せる**。
    Deny(String),
}

/// **1 パスに対する判定 (行番号を知らない呼び出し元用)。** I/O を持たない。
///
/// ## 行域リースが入ってからの向き
/// 行域が入ると、**同じパスを複数のリースが覆える**ようになった
/// (A が `#L1-20`、B が `#L100-200`)。以前は「最初に見つけた 1 件」で
/// 即答していたので、台帳の並び順で答えが変わる = 非決定的になる。
///
/// ここでは**他人が勝つ**。自分の分を先に見つけて `Allow` を返すと、
/// 「1〜20 行だけ持っている自分」が**ファイル全体を保存できてしまい**、
/// 100〜200 行を持つ B を黙って上書きする。行番号を知らないのだから、
/// 知らないなりに安全側 (= 止める) へ倒すのが正しい。
/// 行番号が判る呼び出し元 ([`gate`]) は [`decide_spans`] を使う。
pub fn decide(
    store: &Store,
    holder: &Holder,
    rel: &str,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Verdict {
    for l in &store.leases {
        if !l.active(now, alive) || !l.covers_path(rel) {
            continue;
        }
        if l.holder.same(holder) {
            continue; // 自分のものは止める理由にならない (が、即答もしない)
        }
        return Verdict::Deny(deny_reason(rel, l, now));
    }
    Verdict::Allow
}

/// **行域まで見る判定。** 実際に触れた行 `touched` が他人の域に掛かるときだけ止める。
///
/// `touched` が空 = 「触れた行を決められなかった」= ファイル全体を触るものとして
/// 扱う ([`covers_span`] がそこを安全側へ倒す)。
///
/// **自分の域に収まっているか**は見ない。ここは「他人と衝突しないか」だけを
/// 答え、収まっているかどうかは [`owns_touched`] が別に判断する
/// (誰も持っていない域は [`gate`] がその場で自分のものにするため、
///  「持っていないから止める」にすると自動確保が成立しない)。
///
/// ## 台帳の行域は**そのまま信じない** (遅延解決)
/// `text` は `rel` の**いまの中身**。台帳に載っている行番号は、他人が上へ
/// 行を足した瞬間に古くなる。そのまま信じると、持ち主がもう居ない行を
/// 「他人のもの」として止め続け、いま持っている行を素通しする —
/// **保証が静かに破れる**いちばん危ない壊れ方なので、ここで取り直す。
///
/// ## なぜ遅延解決なのか (eager な追従を採らない理由)
/// 書き込みのたびに全担当へ [`crate::region::follow`] を掛けて台帳を書き直す
/// 手もあるが、そうすると **(1)** 書き込みごとに台帳のロックと書き込みが要り、
/// **(2)** フックが 1 回でも落ちた / スキップされた瞬間に台帳が現実とずれて、
/// **ずれたことを誰も検出できない**。判定の瞬間にその場のテキストから
/// 取り直せば、帳簿付けは 0 で、フックが飛んでも次の判定が正しい答えを出す。
#[allow(clippy::too_many_arguments)]
pub fn decide_spans(
    store: &Store,
    holder: &Holder,
    rel: &str,
    touched: &[crate::region::Span],
    text: Option<&str>,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Verdict {
    for l in &store.leases {
        if !l.active(now, alive) || l.holder.same(holder) {
            continue;
        }
        if l.touches(rel, touched, text) {
            let mut reason = deny_reason(&touched_label(rel, touched), l, now);
            // **断るだけで終わらせない。** 同じ長さが入る近くの空きを一緒に出す。
            // 拒否しか返せないと、エージェントは待つか諦めるかしかできない。
            if let Some(alt) =
                wanted_region(rel, touched).and_then(|w| suggest_alternative(store, &w, text))
            {
                reason.push_str(&trf(
                    "\n(4) ずらす — {alt} なら誰とも重なりません",
                    &[("alt", crate::region::render(&alt))],
                ));
            }
            return Verdict::Deny(reason);
        }
    }
    Verdict::Allow
}

/// 触れた行域を「1 つの欲しい域」へ畳む ([`suggest_alternative`] へ渡す形)。
fn wanted_region(rel: &str, touched: &[crate::region::Span]) -> Option<crate::region::Region> {
    let (first, last) = (touched.first()?, touched.last()?);
    Some(crate::region::Region {
        path: rel.to_string(),
        span: Some(hull(*first, *last)),
        anchor: crate::region::Anchor::default(),
    })
}

/// 拒否の文面に出す「どこを触ろうとしたか」。行域が判るなら行番号まで出す。
fn touched_label(rel: &str, touched: &[crate::region::Span]) -> String {
    let (Some(first), Some(last)) = (touched.first(), touched.last()) else {
        return rel.to_string();
    };
    crate::region::render(&crate::region::Region {
        path: rel.to_string(),
        span: Some(hull(*first, *last)),
        anchor: crate::region::Anchor::default(),
    })
}

/// 触れた行を**自分が既に持っている**か。
///
/// `touched` が `None` (= 行域を決められなかった) ときは、ファイル全体を
/// 持っているときだけ `true`。持っている域からはみ出したら `false` で、
/// 呼び出し側は改めて確保しに行く ([`crate::region::within`])。
///
/// `text` を渡すと**自分の域も錨で取り直してから**見る。取り直せなければ
/// 台帳の行番号へ落ちる。ここが「持っていない」に倒れても害は無い —
/// 呼び出し側が確保し直すだけで、その確保が他人と衝突すれば
/// [`try_claim_wants`] が断るので、保証は [`decide_spans`] 側で閉じている。
#[allow(clippy::too_many_arguments)]
pub fn owns_touched(
    store: &Store,
    holder: &Holder,
    rel: &str,
    touched: Option<&[crate::region::Span]>,
    text: Option<&str>,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> bool {
    let mut mine: Vec<crate::region::Span> = Vec::new();
    let mut any = false;
    for l in &store.leases {
        if !l.active(now, alive) || !l.holder.same(holder) {
            continue;
        }
        match l.owned_spans(rel, text) {
            None => return true, // ファイル全体を持っている
            Some(s) => {
                any |= !s.is_empty();
                mine.extend(s);
            }
        }
    }
    let Some(t) = touched else {
        return false; // 全体が要るのに、持っているのは行域だけ
    };
    if t.is_empty() {
        return true; // 1 行も触らない書き込み = 確保するものが無い
    }
    any && crate::region::within(&mine, t)
}

/// 拒否の文面。**「拒否されました」だけでは、ユーザーは機能を切るだけ。**
/// 誰が・いつから持っていて・どうすればよいかを必ず出す。
fn deny_reason(rel: &str, l: &Lease, now: u64) -> String {
    let since = crate::instances::humanize_uptime(now.saturating_sub(l.acquired_at));
    let left = crate::instances::humanize_uptime(l.expires_at.saturating_sub(now));
    let note = if l.note.is_empty() {
        String::new()
    } else {
        trf("\n目的: {note}", &[("note", l.note.clone())])
    };
    trf(
        "「{path}」は {owner} が確保しています ({since}前から / 期限まであと {left})。{note}\n\
         同じファイルを 2 人が同時に編集すると、衝突はマージのときまで見えません。\n\
         対処: (1) {owner} の完了を待つ (2) 担当を分ける — 別のファイル / 別のディレクトリを受け持つ \
         (3) 引き継ぐなら Zaivern Code のコマンドパレットで「ファイル所有の一覧」を開き、該当のリースを解放する",
        &[
            ("path", rel.to_string()),
            ("owner", l.holder.display()),
            ("since", since),
            ("left", left),
            ("note", note),
        ],
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. 台帳の入出力 (アトミック / fail-open)
// ═══════════════════════════════════════════════════════════════════════════

/// 台帳の置き場所 (`~/.zaivern/leases/`)。
pub fn store_dir() -> PathBuf {
    crate::config::zaivern_dir().join("leases")
}

/// スコープに対応する台帳ファイル。キーは `history::workspace_key` と共通なので、
/// GUI と `zai hook` が**必ず同じファイルへ行き着く**。
pub fn store_path_in(dir: &Path, scope: &Path) -> PathBuf {
    dir.join(format!("{}.json", crate::history::workspace_key(scope)))
}

/// このスコープで機能が有効か。
///
/// **有効化はファイルの存在**で表す。無効なら `zai hook` は `stat` 1 回で
/// 抜けるので、使っていないユーザーが払うコストが実質ゼロになる
/// (設計原則 3: アイドル時のコストはゼロ)。
pub fn enabled(store: &Path) -> bool {
    store.exists()
}

/// このスコープで有効にする (空の台帳を置く)。既にあれば何もしない。
pub fn enable(store: &Path) -> Result<(), String> {
    if store.exists() {
        return Ok(());
    }
    write_store(store, &Store::default())
}

/// エラーが「混んでいて登録できなかった」か (= 再試行で直る)。
pub fn is_lock_busy(e: &str) -> bool {
    e.starts_with(LOCK_BUSY)
}

/// 台帳を読む。無ければ空、**壊れていれば `Err`** (握り潰さない)。
pub fn read_store(store: &Path) -> Result<Store, String> {
    let raw = match std::fs::read_to_string(store) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Store::default()),
        Err(e) => return Err(format!("台帳を読めません: {e}")),
    };
    if raw.trim().is_empty() {
        return Ok(Store::default());
    }
    let mut st: Store =
        serde_json::from_str(&raw).map_err(|e| format!("台帳が壊れています: {e}"))?;
    // **錨の並びを patterns へ揃えるのは読み込みのこの一点だけ。**
    // 手で編集された台帳・別バージョンが書いた台帳は長さがずれ得るので、
    // 以後のコードが添字で対応を取れるようにここで正規化する。
    for l in st.leases.iter_mut() {
        l.align_anchors();
    }
    Ok(st)
}

/// 台帳を書く。**tmp → rename** なので、読み手が書きかけを見ることはない。
fn write_store(store: &Path, s: &Store) -> Result<(), String> {
    if let Some(dir) = store.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("台帳フォルダを作れません: {e}"))?;
    }
    let json = serde_json::to_string_pretty(s).map_err(|e| format!("JSON 化に失敗: {e}"))?;
    let tmp = store.with_extension(format!("tmp{}", std::process::id()));
    std::fs::write(&tmp, json).map_err(|e| format!("台帳を書けません: {e}"))?;
    // rename は同一ディレクトリ内なら unix / Windows とも置換が保証される。
    std::fs::rename(&tmp, store).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("台帳を差し替えられません: {e}")
    })
}

/// 排他ロック。`create_new` は OS の `O_EXCL` / `CREATE_NEW` に落ちるので、
/// **同時に来た 2 プロセスのうち 1 つだけが成功する** (後勝ちにならない)。
/// 握っているロック。**自分が張ったものだけを外す。**
///
/// ## なぜ中身 (token) を照合するのか
/// 以前は `remove_file` を無条件に撃っていた。置き去りロックの奪取
/// ([`acquire_lock_in`]) が `remove_file` + `create_new` の**2 手**だったため、
/// 同じ置き去りを見た 2 人が順に「消して張る」と、**後の人が先の人の
/// 張りたてのロックを消して**しまい、2 人が同時に臨界区間へ入っていた。
/// そのまま両方が read → modify → write すると、**後の書き戻しが先の
/// 予約を消す** (lost update)。
///
/// これは観測された 2 つの症状を**同時に**説明する:
/// * 古い台帳を読んで書き戻す → 他人の予約が消える = **二重配布**
/// * 古い台帳を読んで「空いていない」と判断する = **取りこぼし**
///
/// token を照合すれば、奪われた側の `Drop` が他人のロックを消すことはない。
struct LockGuard {
    path: PathBuf,
    token: String,
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        // **自分の token が入っているときだけ外す。** 読めない / 違う =
        // 既に誰かへ渡っているので、触らないのが正しい。
        if std::fs::read_to_string(&self.path).ok().as_deref() == Some(self.token.as_str()) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// このロック取得を他と区別する印。**プロセスとスレッドと時刻**で作る。
///
/// 同じプロセスの別スレッドも別の握りなので、PID だけでは足りない。
fn lock_token() -> String {
    format!(
        "{}-{:?}-{}",
        std::process::id(),
        std::thread::current().id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    )
}

/// 置き去りロックを**原子的に**奪う。
///
/// ## なぜ `remove_file` ではいけなかったのか
/// `remove_file` は**誰が呼んでも成功する**。同じ置き去りを見た 2 人が
///
/// 1. P2 が消す → P2 が張る (P2 が握った)
/// 2. P3 が消す ← **P2 の張りたてのロックが消える** → P3 が張る
///
/// と進むと、**P2 と P3 が同時に臨界区間へ入る**。[`with_store`] は
/// 読み → 変更 → 書き戻しなので、後から書いたほうが先の予約を丸ごと
/// 消してしまう (lost update)。これが「予約が途中で消える (二重配布)」と
/// 「空いているのに取れない (取りこぼし)」の**単一の原因**だった。
///
/// ## なぜ `rename` なら正しいのか
/// `rename` は**元が在るときしか成功しない**。同時に奪おうとした複数の
/// 待ち手のうち、成功するのは 1 人だけで、残りは `ENOENT` で落ちる。
/// つまり「奪う」という操作そのものが直列化される。
///
/// 奪った後に**中身を照合する**のは、観測してから `rename` するまでの
/// あいだに正当な持ち主が入れ替わっている可能性があるため。別物だったら
/// 元へ戻す (戻せなければ諦める — どのみち次の周回で判定し直す)。
fn steal_stale_lock(path: &Path, observed: &str) {
    let tmp = path.with_extension(format!("steal-{}", lock_token()));
    if std::fs::rename(path, &tmp).is_err() {
        return; // 競走に負けた = 誰かが先に奪った / 持ち主が外した
    }
    let got = std::fs::read_to_string(&tmp).unwrap_or_default();
    if got == observed {
        let _ = std::fs::remove_file(&tmp); // 正当な奪取
                                            // **奪取は異常事象なので必ず記録する。** 「なぜ予約が消えたのか」を
                                            // 後から追える唯一の手掛かりで、頻発するなら持ち主がロックを
                                            // 長く握りすぎている ([`LOCK_STALE_MS`]) という別の問題を指す。
        if let Some(dir) = path.parent() {
            log_line(dir, &format!("stole-stale-lock {}", path.display()));
        }
        return;
    }
    // 見ていたものとは別のロックだった = 生きている持ち主のものかもしれない。
    // **消さずに戻す。ただし `rename` では戻さない** — 戻すあいだに別の人が
    // 張っていたら、その張りたてを上書きして 2 人が握ってしまう。
    // `create_new` なら**空いているときしか書けない**ので、決して奪わない。
    let restored = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map(|mut f| {
            use std::io::Write;
            let _ = f.write_all(got.as_bytes());
        })
        .is_ok();
    let _ = restored; // 戻せなければ新しい持ち主が居る = そのままでよい
    let _ = std::fs::remove_file(&tmp);
}

/// ロック待ちの間の**譲り方**。[`LOCK_SPIN_ROUNDS`] に理由を書いてある。
///
/// 揺らぎ (jitter) を入れるのは、同じ瞬間に寝た待ち手が同じ瞬間に起きて
/// また取り合う (thundering herd) のを崩すため。種はスレッド ID なので
/// **出力へは 1 バイトも漏れない** (順序も結果も変えない)。
fn lock_backoff(attempt: &mut u32) {
    let n = *attempt;
    *attempt = attempt.saturating_add(1);
    if n < LOCK_SPIN_ROUNDS {
        // 臨界区間は小さな JSON の読み書き = 0.1〜0.5ms。寝るより譲るほうが速い。
        std::thread::yield_now();
        return;
    }
    let step = 1u64 << (n - LOCK_SPIN_ROUNDS).min(5);
    let base = LOCK_BACKOFF_US
        .saturating_mul(step)
        .min(LOCK_BACKOFF_CAP_US);
    std::thread::sleep(Duration::from_micros(base / 2 + jitter_us(base)));
}

/// `0..base` の疑似乱数 (スレッドごとに独立)。
fn jitter_us(base: u64) -> u64 {
    use std::cell::Cell;
    use std::hash::{Hash, Hasher};
    thread_local! {
        static SEED: Cell<u64> = const { Cell::new(0) };
    }
    SEED.with(|s| {
        let mut x = s.get();
        if x == 0 {
            let mut h = std::collections::hash_map::DefaultHasher::new();
            std::thread::current().id().hash(&mut h);
            std::process::id().hash(&mut h);
            x = h.finish() | 1;
        }
        // xorshift64
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        x % base.max(1)
    })
}

/// この失敗は「取り合っているだけ」か (= 待てば取れるか)。
///
/// POSIX なら `AlreadyExists` だけ。**Windows はもう 1 つある**:
/// ファイルを消すと、最後のハンドルが閉じるまで *delete pending* という
/// 中間状態になり、そのあいだに `create_new` すると `AlreadyExists` ではなく
/// **`ACCESS_DENIED` (os error 5)** が返る。64 体がロックを奪い合うと、
/// 誰かが `LockGuard` を落とした瞬間に別の誰かが必ずこの窓を踏む。
///
/// これを「壊れている」と扱うと、いちばん混んでいるとき = いちばん衝突しやすい
/// ときにだけ台帳が使えなくなる。**最悪の壊れ方**なので、Windows では
/// 取り合いとして扱って待つ (CI の windows-latest が実際にここで落ちた:
/// `台帳が壊れた: ロックを作れません: Access is denied. (os error 5)`)。
///
/// unix で `PermissionDenied` を待ちに回さないのは、あちらでは本物の権限問題
/// だからである — 待っても直らないので、その場で正直に失敗する。
fn lock_contended(e: &std::io::Error) -> bool {
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        return true;
    }
    cfg!(windows) && e.kind() == std::io::ErrorKind::PermissionDenied
}

fn acquire_lock(store: &Path) -> Result<LockGuard, String> {
    acquire_lock_in(store, LOCK_STALE_MS)
}

/// 置き去り判定の閾値を明示する [`acquire_lock`]。
///
/// **閾値を引数へ出さないと、奪取の競走をテストできない** (既定の 5 秒を
/// 待つテストは書けないし、絶対時間で線を引くテストは必ず嘘をつく)。
fn acquire_lock_in(store: &Path, stale_ms: u64) -> Result<LockGuard, String> {
    let path = store.with_extension("lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("台帳フォルダを作れません: {e}"))?;
    }
    let deadline = Instant::now() + Duration::from_millis(LOCK_WAIT_MS);
    let mut attempt = 0u32;
    loop {
        let token = lock_token();
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut f) => {
                // **中身を書いてから握ったことにする。** 空のまま返すと、
                // `Drop` の照合が自分のロックを外せなくなる。
                use std::io::Write;
                let _ = f.write_all(token.as_bytes());
                let _ = f.flush();
                return Ok(LockGuard { path, token });
            }
            Err(e) if !lock_contended(&e) => return Err(format!("ロックを作れません: {e}")),
            Err(_) => {}
        }
        // クラッシュで置き去りになったロックは奪う (でないと永久に詰まる)。
        // **観測した中身も一緒に覚える** — 奪う瞬間に別物へ入れ替わって
        // いないことを、これで確かめる。
        let observed = std::fs::read_to_string(&path).unwrap_or_default();
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_millis() as u64 > stale_ms);
        if stale {
            steal_stale_lock(&path, &observed);
            continue;
        }
        if Instant::now() >= deadline {
            // **文面ではなく接頭辞で識別する** (文面は翻訳で変わる)。
            // 呼び出し側は「混んでいて登録できなかった」と
            // 「台帳が壊れている」を区別しなければならない — 前者は
            // 再試行すれば直るので止めてよく、後者は止めると詰む。
            return Err(format!(
                "{LOCK_BUSY}{}",
                tr("ロックを取れませんでした (先客が握っています)")
            ));
        }
        lock_backoff(&mut attempt);
    }
}

/// ロックを取って読み → 変更 → 書き戻す。**確保の唯一の入口**。
///
/// **変化が無ければ書かない。** ここが並列度の効く場所で、以前は
/// 「許可されるだけ」の呼び出しでも tmp 書き + rename を払っていた。
/// ロックの保持時間がそのぶん伸び、**16 体を並べるとロック待ち
/// ([`LOCK_WAIT_MS`]) を超えて fail-open し、本物の衝突が漏れた**
/// (計測で実際に踏んだ: 128 回中 1 回・B 群に 1 ハンク)。
/// 判定だけの呼び出しはロックを読みっぱなしで抜けるので、保持時間は
/// 小さな JSON の読み取りだけになる。
pub fn with_store<T>(store: &Path, f: impl FnOnce(&mut Store) -> T) -> Result<T, String> {
    let _lock = acquire_lock(store)?;
    let mut s = read_store(store)?;
    let before = s.clone();
    let out = f(&mut s);
    if s != before {
        write_store(store, &s)?;
    }
    Ok(out)
}

/// [`with_store`] を、**混雑 (`busy`) のときだけ**上限付きの指数バックオフで
/// 作り直す。壊れている ([`read_store`] の `Err`) は再試行しない — 直らない。
///
/// ## なぜ台帳を分割 (シャーディング) しなかったか
/// `docs/conflict-zero.md` が測った `busy-deny` を消す手は 2 つある:
/// 「台帳をパスのハッシュで割る」と「待ち方を直す」。**前者を採らなかった。**
///
/// 台帳の不変条件は「**どの 2 人の担当も重ならない**」で、この判定
/// ([`overlaps`]) は **glob を含む**。`src/**` は全シャードに当たり得るので、
/// シャードを 1 つ引いただけでは重なりを判定できない。すると:
///
/// * glob 用の共有シャードを置く → 具体パスの確保も毎回そこを掴む
///   = **結局グローバルロック**。分けた意味が無い
/// * 複数シャードを順序付きで全部ロックする → デッドロックは避けられるが、
///   確保 1 回で N 個のロックを取ることになり、待ち時間はむしろ伸びる
/// * シャードごとに独立して確保する → **「全か無か」が壊れる**。
///   部分的に取れた状態は「取れたと思って書き始めて途中で衝突する」
///   いちばん危ない形で、この機能が存在する理由そのものを失う
///
/// 一方、実測すると `busy` の原因は混雑ではなく**待ち方**だった
/// ([`LOCK_SPIN_ROUNDS`] に数字がある: 5ms 固定の sleep がロックの
/// 受け渡しを毎秒 200 回に縛っていた)。譲る + 揺らぎ付き指数バックオフと、
/// **進捗で延びる待ち予算**に変えると 64 体同時でも `busy` が 0 になる
/// ([`span_tests::六十四体が同時に確保してもbusyは出ず勝者は一つ`])。
/// **台帳は 1 スコープ 1 ファイルのまま = 全か無かは自明に保たれる。**
///
/// **この保証は `with_store_retry` を通ったときのもの**で、素の
/// [`with_store`] には無い。あちらは 1 回ぶんの primitive で、`busy` は
/// その定義された失敗形である (機械が飽和した状態で素の版を 64 体で叩くと
/// 36 件 busy が出た)。製品の経路 (`zai lease claim` / [`gate`]) は
/// すべてこの retry 版を通ること。
pub fn with_store_retry<T>(store: &Path, mut f: impl FnMut(&mut Store) -> T) -> Result<T, String> {
    // **固定の待ち予算は N が増えれば必ず破綻する。**
    //
    // 臨界区間が 1 回 t かかるなら、N 体が順番に通るには N·t 要る。予算を
    // 定数にすると「N がいくつまでなら大丈夫か」を暗黙に決めることになり、
    // その上を踏んだ瞬間に **誤りではない busy** が出る (実測: 64 体には
    // 320ms 要るのに予算 200ms だった)。台数を引数で渡す設計にもできるが、
    // 呼び出し側は自分以外に何体居るかを知らない。
    //
    // そこで**進捗で延ばす**: 台帳ファイルが書き換わっていれば「誰かが通った」
    // ので、こちらは待ち続けてよい。誰も通らないまま [`LOCK_RETRY_MS`] 経ったら
    // 本当に詰まっているので諦める。これで N に依存しなくなり、かつ
    // デッドロック時に永久待ちもしない。上限 [`LOCK_RETRY_CAP_MS`] は
    // 「壊れた環境で短命フックが居座らない」ための安全弁。
    let start = Instant::now();
    let cap = Duration::from_millis(LOCK_RETRY_CAP_MS);
    let idle_budget = Duration::from_millis(LOCK_RETRY_MS);
    let mut last_progress = Instant::now();
    let mut seen = progress_token(store);
    let mut attempt = 0u32;
    loop {
        match with_store(store, &mut f) {
            Ok(v) => return Ok(v),
            Err(e) if is_lock_busy(&e) => {
                let now = Instant::now();
                let rev = progress_token(store);
                if rev != seen {
                    // 誰かが通った = 系は生きている。待ちの起点を巻き直す。
                    seen = rev;
                    last_progress = now;
                }
                if now.duration_since(last_progress) >= idle_budget
                    || now.duration_since(start) >= cap
                {
                    return Err(e);
                }
                lock_backoff(&mut attempt);
            }
            Err(e) => return Err(e),
        }
    }
}

/// 「系が動いているか」を、ロックを取らずに安く見るための印。
///
/// 2 つを見る:
///
/// 1. **台帳** `(更新時刻, 長さ)` — 誰かが確保・解放に成功した
/// 2. **ロックファイル** の更新時刻 — 誰かが臨界区間を*通り抜けた*
///
/// 1 だけでは足りない。[`with_store`] は**中身が変わったときしか書かない**
/// ので、63 体が「他人が持っている」と断られるだけの局面では台帳が 1 度も
/// 動かず、待ち手からは「詰まっている」と区別が付かない。ロックは
/// `create_new` で作られ `LockGuard` の破棄で消えるため、通り抜けるたびに
/// 作り直されて更新時刻が変わる — これが**断られた側にも見える進捗**になる。
///
/// 取りこぼしても、こちらの待ちが早く尽きるだけで**誤った成功にはならない**。
fn progress_token(store: &Path) -> (u64, u64, u64) {
    let mtime = |p: &Path| -> u64 {
        std::fs::metadata(p)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    };
    let len = std::fs::metadata(store).map(|m| m.len()).unwrap_or(0);
    (mtime(store), len, mtime(&store.with_extension("lock")))
}

/// 診断ログの場所。**書き手 ([`log_line`]) と読み手 (競合ゼロ点検) が
/// ファイル名を 2 か所に持たないための唯一の真実源。**
pub fn audit_log_path(dir: &Path) -> PathBuf {
    dir.join("gate.log")
}

/// 診断ログ (`~/.zaivern/leases/gate.log`)。**拒否と内部エラーだけ**書く。
/// 許可のたびに書くとエージェントの臨界路で I/O が増える。
fn log_line(dir: &Path, line: &str) {
    use std::io::Write;
    let path = audit_log_path(dir);
    if std::fs::metadata(&path).is_ok_and(|m| m.len() > LOG_CAP) {
        let _ = std::fs::remove_file(&path);
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let _ = writeln!(f, "{} {line}", now_secs());
    }
}

/// 現在時刻 (UNIX 秒)。時計が epoch 以前でも落とさない。
pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// PID の生存確認 (既定の実装)。テストは偽の関数を渡す。
fn pid_alive(pid: u32) -> bool {
    crate::instances::pid_alive(pid)
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. フック経路 — 強制はここでしか起きない
// ═══════════════════════════════════════════════════════════════════════════

/// ベンダーへ返す答え。`stdout` が空なら「判断しない」= 通常の許可フローへ。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HookAnswer {
    pub stdout: String,
    pub stderr: String,
    pub exit: i32,
}

/// 拒否を Claude Code の `PreToolUse` 出力スキーマへ (実ドキュメントで確認済み)。
///
/// ```text
/// {"hookSpecificOutput":{"hookEventName":"PreToolUse",
///  "permissionDecision":"deny","permissionDecisionReason":"…"}}
/// ```
/// 終了コードは **0**。ドキュメント曰く「JSON output is only processed on exit 0」で、
/// exit 2 にすると stdout は無視され stderr がエラーとして流れる。
/// ここでは正規の permission decision を使う (エラーではなく判断なので)。
///
/// **許可のときは何も出さない。** `"allow"` を返すとユーザー自身の許可設定を
/// 飛び越えてしまう — こちらが与えたいのは「止める権限」だけで、
/// 「他人の確認を省く権限」ではない。
/// 拒否文へ「行単位で見られなかった」ことを添える。
///
/// **判定が落ちたことを黙っていると、利用者には直しようが無い。**
/// 「離れた行を触っているのに止まる」の原因はここにしか無いので、
/// 拒否そのものと同じ場所に出す (監査ログだけでは誰も見ない)。
fn with_cap_note(reason: &str, degraded: &Option<(String, u64, u64)>) -> String {
    let Some((path, size, cap)) = degraded else {
        return reason.to_string();
    };
    format!(
        "{reason}\n{}",
        trf(
            "補足: 「{path}」は {size} バイトで上限 {cap} バイトを超えるため、**どの行を触るかを判定できず、ファイル全体として扱いました**。設定 {key} を上げると行単位に戻ります",
            &[
                ("path", path.clone()),
                ("size", size.to_string()),
                ("cap", cap.to_string()),
                ("key", KEY_GATE_READ_CAP.to_string()),
            ],
        )
    )
}

pub fn deny_answer(agent: &str, reason: &str) -> HookAnswer {
    // **拒否の形はベンダーごとに違う。** カタログから引き、無ければ
    // Claude の形へ落とす (未知のエージェントで無反応にならないように)。
    // 文字列連結ではなく serde で組むのがカタログ側の約束 — 理由に `"` や
    // 改行が入ると壊れた JSON になり、**拒否が黙って無視される**ため。
    if let Some((stdout, exit)) = crate::agents::deny_payload(agent, reason) {
        return HookAnswer {
            stdout,
            stderr: reason.to_string(),
            exit,
        };
    }
    let json = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    });
    HookAnswer {
        stdout: json.to_string(),
        // stderr にも出す: 終了コードだけを見るベンダーや、ログを追う人向け。
        stderr: reason.to_string(),
        exit: 0,
    }
}

/// 判断しない (通常の許可フローへ戻す)。
pub fn pass_answer() -> HookAnswer {
    HookAnswer {
        stdout: String::new(),
        stderr: String::new(),
        exit: 0,
    }
}

/// ペイロードから書き込み先のパスを取り出す **純関数**。
///
/// キーはエージェント固有なので [`crate::agents::HOOK_TARGETS`] の
/// `write_path_keys` から渡す (ここにリテラルを置かない)。
pub fn target_path(payload: &serde_json::Value, keys: &[&str]) -> String {
    let input = payload.get("tool_input").unwrap_or(payload);
    for k in keys {
        if let Some(s) = input.get(*k).and_then(|v| v.as_str()) {
            if !s.is_empty() {
                return s.to_string();
            }
        }
    }
    // MultiEdit 系: edits[] の中にパスが入る形にも対応する。
    if let Some(arr) = input.get("edits").and_then(|v| v.as_array()) {
        for e in arr {
            for k in keys {
                if let Some(s) = e.get(*k).and_then(|v| v.as_str()) {
                    if !s.is_empty() {
                        return s.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// 編集ペイロードのキー。
///
/// **統合担当へ**: これは本来 `agents.rs` の `HookTarget` が持つべきデータで、
/// CLAUDE.md の「エージェント固有値は `agents.rs` のカタログにデータとして持つ」
/// に反している。このブランチは `agents.rs` を触れない (同時に別の担当が
/// 編集中) ので暫定的にここへ置いた。繋ぎ方は:
///
/// 1. `HookTarget` に `edit_keys: EditKeys` を足す
/// 2. `hook_edit_keys(bin) -> Option<&'static EditKeys>` を生やす
/// 3. ここの `EDIT_KEYS` を消し、[`applied_text`] の引数で受け取る
///
/// 現状の値は claude (`Edit` / `MultiEdit` / `Write`) と
/// gemini (`replace` / `write_file`) で**同じ**なので、表に分けても
/// 今のところ行は 1 つになる (だから暫定でも実害が出ていない)。
/// codex の `apply_patch` はパッチ本文なので**この形に当たらず**、
/// 行域を決められないもの = ファイル全体として扱われる。
struct EditKeys {
    /// 全文置換の中身 (`Write` / `write_file`)。
    content: &'static str,
    /// 置換前 / 置換後 (`Edit` / `replace`)。
    old: &'static str,
    new: &'static str,
    /// すべて置換するか。
    all: &'static str,
    /// 連続適用の配列 (`MultiEdit`)。
    edits: &'static str,
}

const EDIT_KEYS: EditKeys = EditKeys {
    content: "content",
    old: "old_string",
    new: "new_string",
    all: "replace_all",
    edits: "edits",
};

/// 1 回の置換。**当たらなければ `None`** (そのツール呼び出しは失敗するので、
/// 「書き込み後の中身」を推測してはいけない)。
fn replace_one(text: &str, old: &str, new: &str, all: bool) -> Option<String> {
    if old.is_empty() || !text.contains(old) {
        return None;
    }
    Some(if all {
        text.replace(old, new)
    } else {
        text.replacen(old, new, 1)
    })
}

/// ペイロードから**書き込み後の中身**を作る純関数。
///
/// | 形 | 中身 |
/// |---|---|
/// | `content` を持つ | それが全文 (`Write`) |
/// | `edits[]` を持つ | 先頭から順に置換 (`MultiEdit`) |
/// | `old_string` を持つ | 1 回置換 (`Edit`) |
/// | それ以外 | `None` = **判らない** |
///
/// `None` を返したら呼び出し側は**ファイル全体を触るもの**として扱う。
/// 「たぶんこう変わる」で行域を狭く見積もると、他人の域へはみ出す書き込みを
/// 通してしまう — 判らないときは広く見るのが安全側。
pub fn applied_text(old: &str, input: &serde_json::Value) -> Option<String> {
    let s = |k: &str| input.get(k).and_then(|v| v.as_str());
    if let Some(c) = s(EDIT_KEYS.content) {
        return Some(c.to_string());
    }
    if let Some(arr) = input.get(EDIT_KEYS.edits).and_then(|v| v.as_array()) {
        if arr.is_empty() {
            return None;
        }
        let mut cur = old.to_string();
        for e in arr {
            let o = e.get(EDIT_KEYS.old).and_then(|v| v.as_str())?;
            let n = e.get(EDIT_KEYS.new).and_then(|v| v.as_str()).unwrap_or("");
            let all = e
                .get(EDIT_KEYS.all)
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            cur = replace_one(&cur, o, n, all)?;
        }
        return Some(cur);
    }
    let o = s(EDIT_KEYS.old)?;
    let n = s(EDIT_KEYS.new).unwrap_or("");
    let all = input
        .get(EDIT_KEYS.all)
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    replace_one(old, o, n, all)
}

/// [`read_capped_ex`] の結果。**「読めなかった」を 1 つにまとめない。**
///
/// 以前は [`Option`] だったので「上限超え」と「存在しない / 壊れている」が
/// 同じ `None` になり、利用者には
/// **「読めませんでした」としか出なかった** (実バイナリで再現済み:
/// 1.8MB のファイルに対して「読めませんでした」と出るが、ファイルは
/// 健在で ただ上限を超えているだけだった)。原因が判らない拒否は直せない。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FileRead {
    /// 読めた。
    Text(String),
    /// 実在するが上限超え。`0` = 実サイズ、`1` = そのとき効いていた上限。
    TooLarge(u64, u64),
    /// 無い / ディレクトリ / 読めない。
    Unavailable,
}

/// 設定キー: 行域を出すために読むファイルの上限 (バイト)。
pub const KEY_GATE_READ_CAP: &str = "lease.gate_read_cap_bytes";

/// このスコープで効いている読み取り上限 (バイト)。
///
/// 既定は [`GATE_READ_CAP`]。設定 [`KEY_GATE_READ_CAP`] で上げられる
/// — **生成コード・lock・データファイルが 1MiB を超えるリポジトリでは、
/// 上げるのが唯一の「行単位に戻す」手段**だから (フックは書き込みのたびに
/// 走る短命プロセスなので、既定を上げると全員が I/O を払う)。
/// `0` / 未設定 = 既定。
///
/// **既定より小さい値は既定へ引き上げる。** 呼び出し側は「既定を超えたとき
/// だけ設定を引く」ので、下げた値は**ファイルの大きさによって効いたり
/// 効かなかったりする**。効かない設定を受け付けるほうが、受け付けないより
/// 質が悪い (利用者は下げたつもりでいる)。上げる方向だけが意味を持つ。
fn configured_cap(root: &Path) -> u64 {
    let cfg = crate::config::load(std::slice::from_ref(&root.to_path_buf()), false);
    let v = cfg.feature_i64(KEY_GATE_READ_CAP);
    if v <= 0 {
        GATE_READ_CAP
    } else {
        (v as u64).max(GATE_READ_CAP)
    }
}

/// 判定のためにファイルを読む。**上限付き**。読めなかった**理由**まで返す版。
///
/// `root` は設定 ([`KEY_GATE_READ_CAP`]) を引くためのスコープ。
/// **既定の上限を超えたときにだけ設定を読む** — フックは書き込みのたびに
/// 走る短命プロセスなので、圧倒的多数を占める小さいファイルに
/// TOML の読み込みを払わせない (設計原則 3)。
/// 判定のためにファイルを読む (上限付き)。読めたときだけ中身を返す版。
///
/// 交錯の判定 ([`interleave_ok`]) を持つ関所 — `guard` / `czero` /
/// `negotiate` — が錨を数えるために使う。**読めなければ `None`** で、
/// 呼び出し側はそれを fail-closed (断る) として扱うこと。
/// `root` は読み取り上限の設定 ([`KEY_GATE_READ_CAP`]) を引くスコープ。
pub fn read_capped(abs: &Path, root: &Path) -> Option<String> {
    match read_capped_ex(abs, root) {
        FileRead::Text(s) => Some(s),
        _ => None,
    }
}

fn read_capped_ex(abs: &Path, root: &Path) -> FileRead {
    let Ok(m) = std::fs::metadata(abs) else {
        return FileRead::Unavailable;
    };
    if !m.is_file() {
        return FileRead::Unavailable;
    }
    if m.len() > GATE_READ_CAP {
        let cap = configured_cap(root);
        if m.len() > cap {
            return FileRead::TooLarge(m.len(), cap);
        }
    }
    match std::fs::read_to_string(abs) {
        Ok(s) => FileRead::Text(s),
        Err(_) => FileRead::Unavailable,
    }
}

/// 行域の並びを整える (昇順 + 安全帯以内は 1 本に畳む)。
fn coalesce(mut v: Vec<crate::region::Span>, band: u32) -> Vec<crate::region::Span> {
    v.sort();
    let mut out: Vec<crate::region::Span> = Vec::with_capacity(v.len());
    for s in v {
        match out.last_mut() {
            Some(p) if crate::region::spans_too_close(p, &s, band) => *p = hull(*p, s),
            _ => out.push(s),
        }
    }
    out
}

/// **この書き込みが実際に触れる行域**を、書き込み前の中身とペイロードから出す。
///
/// `None` = 決められなかった (ファイルが無い / 大きすぎる / ペイロードの形が
/// 判らない) = **ファイル全体を触るもの**として扱う。
///
/// ## 前後の**両方向**を見る理由 (実測で踏んだ穴)
/// [`crate::region::touched_spans`] が返すのは**書いた後の行番号**だけ。
/// ところがリースは**書く前の行番号**で登録されているので、これだけを使うと
/// **削除がまるごと見えなくなる**。200 行のファイルを 2 行で全文置換すると
/// 「触れたのは 1〜2 行目」と出て、95〜105 行目を持っている相手を
/// **黙って消せてしまった** (このテストを書いて初めて分かった)。
///
/// なので `old → new` (書いた後の座標) と `new → old` (書く前の座標) の
/// 両方を出して**合併**する。小さな編集では 2 つはほぼ一致するので域は
/// 広がらず、全文置換のときだけ正しく広くなる。
/// **中身は呼び出し側がもう読んである** ([`gate`] は行域を出すのにも、台帳の
/// 行域を錨で取り直すのにも同じ 1 回ぶんを使う — 2 度読まない)。
fn touched_in(old: &str, input: &serde_json::Value) -> Option<Vec<crate::region::Span>> {
    let new = applied_text(old, input)?;
    let band = crate::region::SAFE_BAND;
    let mut all = crate::region::touched_spans(old, &new, band);
    all.extend(crate::region::touched_spans(&new, old, band));
    Some(coalesce(all, band))
}

/// `zai hook` から呼ぶ**強制の本体**。GUI が動いていなくても効く。
///
/// 3 つの制約 (どれも load-bearing):
/// * **速いこと** — 書き込みのたびに通る。無効なら `stat` 1 回で戻る。
///   リポジトリを走査しない。
/// * **内部エラーは fail-open** — 台帳が読めない / ロックが取れないで
///   ユーザーのエージェントを止めない。
/// * **本物の競合は fail-closed**。
///
/// ## 行域での判定 (ファイル単位からの置き換え)
/// 1. 書き込み先の**現在の中身**を読む ([`read_capped_ex`] — 上限 [`GATE_READ_CAP`])
/// 2. ペイロードから**書き込み後の中身**を作る ([`applied_text`])
/// 3. [`crate::region::touched_spans`] で**実際に触れる行域**を出す
/// 4. 触れた域が自分の域に収まっていれば通し ([`owns_touched`])、
///    他人の域へ掛かっていれば止める ([`decide_spans`])。
///    誰も持っていなければ**その域だけ**を自分のものにする
///
/// ## 行域で判定できないものの線引き (**明示的に広く倒す**)
///
/// | 形 | 扱い | 理由 |
/// |---|---|---|
/// | `Write` (全文置換) | 触れた域は広くなる。持っていなければ止まる | 正しい挙動 |
/// | `Edit` / `MultiEdit` | 置換後の中身から行域が出る | |
/// | `sed -i` / `> file` などシェル経由 | **ファイル全体** | 書き込み後の中身を事前に知れない |
/// | codex の `apply_patch` | **ファイル全体** | パッチ本文は [`applied_text`] の形に当たらない |
/// | 1 コマンドが複数ファイルを書く | **ファイル全体** | どのペイロードがどのファイルの中身か対応が取れない |
/// | 対象不明 (`opaque`: `eval` / ヒアドキュメント) | **ファイル全体**、かつ通す | 監査ログに残す。ここで止めると `ls` まで落ちる形になり、ユーザーは機能ごと切る |
/// | ファイルが無い / [`GATE_READ_CAP`] 超 | **ファイル全体** | 前の中身が無い / 読む I/O に上限が要る |
///
/// 「ファイル全体として扱う」= **そのファイルの域を誰かが持っていたら止まる**
/// (従来のファイル単位リースとまったく同じ挙動)。
///
/// ## fail-open の範囲 (ここだけ。それ以外は fail-closed)
/// * 台帳が**無い** (この機能が無効なワークスペース)
/// * 台帳が**壊れている / 読めない**
/// * ペイロードが JSON として読めない / エージェントがカタログに無い
/// * ツールの形が判らない (`opaque`)
///
/// 混雑 (`busy`) は fail-open**しない** — [`with_store_retry`] が
/// 上限付きで作り直し、それでも駄目なら止める (再試行すれば通る)。
pub fn gate(agent: &str, event: &str, payload: &str) -> HookAnswer {
    let v: serde_json::Value = match serde_json::from_str(payload) {
        Ok(v) => v,
        Err(_) => return pass_answer(), // 読めない = こちらの都合。通す
    };
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let event = if event.is_empty() {
        s("hook_event_name")
    } else {
        event.to_string()
    };
    // イベント名もベンダーごとに違う (gemini は `BeforeTool`)。
    // 未知のエージェントは `None` が返るのでここで抜ける = 従来と同じ。
    if Some(event.as_str()) != crate::agents::hook_gate_event(agent) {
        return pass_answer();
    }
    if crate::agents::hook_target(agent).is_none() {
        return pass_answer(); // カタログに無いエージェント = 形が判らない
    }
    // 「書き込み系ツールか」もカタログから引く (ここにツール名を書かない)。
    //
    // **パス型 (`Edit`/`Write`) だけでなくコマンド型 (`Bash`) も通す。**
    // 以前はパス型しか見ておらず、`printf X > shared.rs` のような
    // シェル経由の書き込みが**丸ごと素通り**していた (敵対的検証で実際に
    // 上書きされた)。エージェントは `sed -i` / リダイレクトで日常的に書く。
    let tool = s("tool_name");
    let editing = crate::agents::hook_tool_state(agent, &tool)
        == Some(crate::supervisor::protocol::ProtoState::Editing);
    if !editing && crate::agents::hook_command_key(agent, &tool).is_none() {
        // `ls` や `cargo test` はここで抜ける (stat すら踏まない)。
        return pass_answer();
    }
    let cwd = s("cwd");
    let cwd = if cwd.is_empty() {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    } else {
        PathBuf::from(cwd)
    };
    let roots = roots_of(&cwd);
    let dir = store_dir();
    let store = store_path_in(&dir, &roots.key);
    // ここが「使っていない人が払う全コスト」= stat 1 回。
    if !enabled(&store) {
        return pass_answer();
    }
    let holder = Holder {
        agent: agent.to_string(),
        session: s("session_id"),
        cwd: normalize_path(&cwd.to_string_lossy()),
        pid: 0, // フックは短命プロセス。生存確認には使えないので TTL に委ねる
    };
    // 書き込み先の抽出は**有効なワークスペースでだけ**払う (コマンド行の
    // 解析は stat より高いので、使っていない人に持たせない)。
    let write = crate::agents::hook_write_targets(agent, &tool, &v);
    if write.opaque {
        // 書き込みらしいのに宛先が判らない (`eval` / 変数展開 / ヒアドキュメント)。
        // **止めない** — `ls` まで落ちる作りにするとユーザーは機能ごと切り、
        // 切られた機能の保証はゼロになる。監査に残して明示リースで守る。
        log_line(&dir, &format!("opaque-write {}", holder.display()));
    }
    if write.paths.is_empty() {
        return pass_answer();
    }
    // 相対パスへ。**相対化は作業ツリー基準**で行う (worktree のファイルは
    // 元のリポジトリの配下に無いので、key 基準にすると必ず外れる)。
    // ツリーの外 (別リポジトリ・システムのファイル) は関知しない。
    // **絶対パスも一緒に持ち回る** — 行域を出すには実ファイルを読む必要があり、
    // `rel` は正規化 (macOS / Windows では小文字化) を通っているので、
    // ツリーへ繋ぎ直すと大小非区別に頼ることになる。
    let targets: Vec<(PathBuf, String)> = write
        .paths
        .iter()
        .map(|raw| {
            if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                cwd.join(raw)
            }
        })
        .filter_map(|abs| rel_within(&roots.tree, &abs).map(|rel| (abs, rel)))
        .collect();
    if targets.is_empty() {
        return pass_answer();
    }
    let rels: Vec<String> = targets.iter().map(|(_, r)| r.clone()).collect();

    // **実際に触れる行域**。`None` = 決められない = ファイル全体を触る扱い。
    // 出せるのは「パス欄を持つ編集ツールが 1 ファイルだけを書く」形のときだけ
    // (複数ファイルを書くコマンドは、どのペイロードがどのファイルの中身かの
    //  対応が取れない)。この線引きは [`gate`] のドキュメント表にある。
    //
    // **中身は 1 回だけ読む。** 行域を出すのにも、台帳に載っている他人の行域を
    // 錨で取り直すのにも同じテキストが要る (`GATE_READ_CAP` = 1MiB の上限つき)。
    // 2 度読むとフックの往復が倍になり、短命プロセスがそのぶん遅くなる。
    // 上限超えで行域を出せなかったファイル (拒否文へ添えるため)。
    let mut cap_degraded: Option<(String, u64, u64)> = None;
    let (spans, text): (
        Vec<Option<Vec<crate::region::Span>>>,
        Option<std::sync::Arc<str>>,
    ) = if editing && targets.len() == 1 && !write.opaque {
        match read_capped_ex(&targets[0].0, &roots.tree) {
            FileRead::Text(old) => {
                let input = v.get("tool_input").unwrap_or(&v);
                let t = touched_in(&old, input);
                (vec![t], Some(std::sync::Arc::from(old)))
            }
            // **上限超えは黙って落とさない。** 行域が出せない = 判定が
            // ファイル全体へ落ちるということなので、監査に必ず残す
            // (拒否されたときに「なぜ離れた行なのに」を追える唯一の手掛かり)。
            FileRead::TooLarge(size, cap) => {
                log_line(
                    &dir,
                    &format!(
                        "cap-degraded {} {} size={size} cap={cap}",
                        holder.display(),
                        rels[0]
                    ),
                );
                cap_degraded = Some((rels[0].clone(), size, cap));
                (vec![None], None)
            }
            FileRead::Unavailable => (vec![None], None),
        }
    } else {
        (vec![None; targets.len()], None)
    };
    // `text` が `Some` なのは対象が 1 件のときだけなので、添字とずれない。
    let text_at = |i: usize| -> Option<&str> {
        if i == 0 {
            text.as_deref()
        } else {
            None
        }
    };
    let now = now_secs();
    let alive: &dyn Fn(u32) -> bool = &pid_alive;
    // 1 パスぶんの判定 (ロックの内でも外でも同じ規則を使う — 2 実装は必ずズレる)。
    let judge = |st: &Store, i: usize| -> Verdict {
        match &spans[i] {
            Some(t) => decide_spans(st, &holder, &rels[i], t, text_at(i), now, alive),
            None => decide(st, &holder, &rels[i], now, alive),
        }
    };

    // **拒否はロックを取らずに決める。** 台帳の置き換えは tmp → rename
    // なので、ロック無しで読んでも書きかけは見えない。ここでロックを待つと、
    // 並列度が上がったときに「待ちきれず fail-open して衝突が漏れる」—
    // つまり**いちばん混んでいるとき (= いちばん衝突しやすいとき) にだけ
    // 効かなくなる**という最悪の壊れ方をする。実測で踏んだ穴なので塞ぐ。
    if let Ok(st) = read_store(&store) {
        // **1 件でも他人が持っていたら止める。** 1 コマンドが複数のファイルを
        // 書く (`mv a b` / `sed -i f1 f2`) ので、部分的に通すと「取れたと
        // 思って書き始めて、途中で衝突する」いちばん危ない形になる。
        for (i, rel) in rels.iter().enumerate() {
            if let Verdict::Deny(reason) = judge(&st, i) {
                log_line(&dir, &format!("deny {} {rel}", holder.display()));
                return deny_answer(agent, &with_cap_note(&reason, &cap_degraded));
            }
        }
        // **自分の域に全部収まっていて、期限にも余裕があるならロックを取らない。**
        // 行域リースでいちばん多いのが「さっき確保した域を続けて書く」形なので、
        // ここで抜けられると台帳の取り合いそのものが起きない。
        let fresh = st.leases.iter().any(|l| {
            l.holder.same(&holder)
                && l.expires_at.saturating_sub(now) as f64
                    >= DEFAULT_TTL_SECS as f64 * REFRESH_BELOW
        });
        if fresh
            && (0..targets.len()).all(|i| {
                owns_touched(
                    &st,
                    &holder,
                    &rels[i],
                    spans[i].as_deref(),
                    text_at(i),
                    now,
                    alive,
                )
            })
        {
            return pass_answer();
        }
    }

    // 通す側だけロックを取り、「判定 → 自動確保 / 延長」を済ませる。
    let outcome = with_store_retry(&store, |st| {
        prune(st, now, alive);
        for i in 0..rels.len() {
            if let Verdict::Deny(reason) = judge(st, i) {
                return Verdict::Deny(reason);
            }
        }
        // **誰も持っていないなら、書いた本人のものにする。**
        // これがあるから、ユーザーが 1 件も設定しなくても
        // 2 人目が同じ**行域**へ来た瞬間に止まる。
        let refresh = st
            .leases
            .iter()
            .find(|l| l.holder.same(&holder))
            .is_none_or(|l| {
                let left = l.expires_at.saturating_sub(now) as f64;
                left < DEFAULT_TTL_SECS as f64 * REFRESH_BELOW
            });
        let need = !(0..targets.len()).all(|i| {
            owns_touched(
                st,
                &holder,
                &rels[i],
                spans[i].as_deref(),
                text_at(i),
                now,
                alive,
            )
        });
        if refresh || need {
            // **触れた域だけ**を確保する (`try_claim` は全か無か)。
            // 行域を決められなかったパスは、パスそのもの = ファイル全体。
            //
            // **錨は「いま手元にあるテキスト」から打つ。** フックは書き込みの
            // **前**に走るので、これは書き込み前の中身。書き込みで動いたぶんは
            // 次の書き込みが打ち直す。取り直せなくなっても
            // [`Lease::live_span_of`] が記録された行番号へ落ちるので、
            // **最悪でも錨が入る前と同じ判定**にしかならない。
            let want: Vec<Want> = (0..targets.len())
                .flat_map(|i| match &spans[i] {
                    None => vec![Want::plain(&rels[i])],
                    Some(t) => t
                        .iter()
                        .map(|s| Want {
                            spec: crate::region::render(&crate::region::Region {
                                path: rels[i].clone(),
                                span: Some(*s),
                                anchor: crate::region::Anchor::default(),
                            }),
                            anchor: text_at(i)
                                .map(|x| crate::region::capture_anchor(x, s))
                                .unwrap_or_default(),
                            text: if i == 0 { text.clone() } else { None },
                        })
                        .collect(),
                })
                .collect();
            // **実ファイルを読まない入口を使う** — ここは台帳ロックの内側で、
            // I/O を足すと混雑時にロックの保持時間が伸びて全員が待つ。
            let _ = try_claim_wants(st, &holder, &want, now, DEFAULT_TTL_SECS, alive);
        }
        Verdict::Allow
    });
    match outcome {
        Ok(Verdict::Deny(reason)) => {
            log_line(
                &dir,
                &format!("deny {} {}", holder.display(), rels.join(" ")),
            );
            deny_answer(agent, &with_cap_note(&reason, &cap_degraded))
        }
        Ok(Verdict::Allow) => pass_answer(),
        Err(e) if is_lock_busy(&e) => {
            // **混んでいるだけなら止める (fail-closed)。**
            // ここを通していたため、書き込みが多いときに「誰も持っていない」
            // と判定したまま登録に失敗し、**同じファイルへ複数のエージェントが
            // 入れた** (実測: 1500 書込 × 16 体で 42 ファイルが重複、うち 3 件は
            // 本物のマージ衝突になった)。いちばん混んでいる時 = いちばん
            // 衝突しやすい時にだけ効かなくなる、最悪の壊れ方だった。
            // 再試行すれば直るので、止めても作業は進む。
            //
            // **ここまで来ることは実質無くなった。** `with_store_retry` が
            // [`LOCK_RETRY_MS`] のあいだ作り直し、その内側の `acquire_lock` は
            // 5ms 固定の sleep をやめた ([`LOCK_SPIN_ROUNDS`])。
            // 64 体同時でも busy は 0 件
            // ([`tests::六十四体が同時に確保してもbusyは出ず勝者は一つ`])。
            // それでも腕を残すのは、ディスクが刺さった等の本物の異常のとき
            // **通してはいけない**から。
            log_line(&dir, &format!("busy-deny {}", rels.join(" ")));
            deny_answer(agent, &tr(
                "ファイル所有の台帳が混み合っていて、担当を登録できませんでした。\n                 そのまま書くと他の担当と同じファイルを触る恐れがあるため、いったん止めます。\n                 対処: 数秒おいて同じ操作をやり直してください (再試行すれば通ります)",
            ))
        }
        Err(e) => {
            // 台帳が壊れている / 読めない = こちらの都合。**ここは通す。**
            // エージェント全体の書き込みを台帳の破損で止めると、
            // ユーザーは機能ごと切る (切られた機能の保証はゼロ)。
            // エディタ自身の保存経路 (`check_write`) は fail-closed なので、
            // 手元の編集は守られる。
            log_line(&dir, &format!("fail-open {}: {e}", rels.join(" ")));
            pass_answer()
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. 事前の重複検出 — いちばん安い勝ち
// ═══════════════════════════════════════════════════════════════════════════

/// 「この担当にこのファイル群」の 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Assignment {
    pub agent: String,
    pub patterns: Vec<String>,
}

/// 重なり 1 件 (どの 2 人が、どのパターンで)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Overlap {
    pub a: usize,
    pub b: usize,
    pub pattern_a: String,
    pub pattern_b: String,
}

/// `名前: パターン, パターン` の行を割り当てへ。空行と `#` 始まりは無視。
pub fn parse_assignments(text: &str) -> Vec<Assignment> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (agent, rest) = match line.split_once(':') {
            Some((a, r)) => (a.trim().to_string(), r),
            None => (format!("#{}", out.len() + 1), line),
        };
        let patterns: Vec<String> = rest
            .split([',', ' ', '\t'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            // 行域つき (`src/a.rs#L10-40`) をそのまま受ける。
            .map(normalize_spec)
            .collect();
        if !patterns.is_empty() {
            out.push(Assignment { agent, patterns });
        }
    }
    out
}

/// **配る前に**重なりを全部出す。O(担当数² × パターン数²) だがどれも小さい。
///
/// 行域つきの担当 (`src/a.rs#L10-40`) を受け付ける。判定は [`overlaps`] 1 本
/// なので、**同じファイルでも安全帯 ([`crate::region::SAFE_BAND`]) ぶん
/// 離れていれば重なりとして出ない** = 両方へ配れる。
/// 出力の並びは担当の並び × パターンの並びで決まる = 決定的。
pub fn plan_overlaps(list: &[Assignment]) -> Vec<Overlap> {
    let mut out = Vec::new();
    for i in 0..list.len() {
        for j in (i + 1)..list.len() {
            for pa in &list[i].patterns {
                for pb in &list[j].patterns {
                    if overlaps(pa, pb) {
                        out.push(Overlap {
                            a: i,
                            b: j,
                            pattern_a: pa.clone(),
                            pattern_b: pb.clone(),
                        });
                    }
                }
            }
        }
    }
    out
}

/// 警告だけでは足りない — **使える手**を出す。
///
/// 重なったパターンを後の担当から外した「互いに素な」割り当てを返す。
/// 外した分は誰も持たなくなるので、直列にやるべき部分として一覧に出す。
///
/// **決定性**: 走査は `list` の順 → パターンの順で、同点 (どちらも取れる) は
/// **先に並んでいる担当が勝つ**。呼び出し側 ([`crate::coordinator`]) が
/// 担当をタスク ID の辞書順で並べているので、入力が同じなら答えも必ず同じ。
///
/// 行域つきの担当もそのまま通る — `src/a.rs#L1-20` と `src/a.rs#L40-60` は
/// [`overlaps`] が「重ならない」と答えるので、**両方が残る**。
/// これが並列度をファイル数で頭打ちにしないための肝。
pub fn split_plan(list: &[Assignment]) -> (Vec<Assignment>, Vec<String>) {
    let mut taken: Vec<String> = Vec::new();
    let mut serial: Vec<String> = Vec::new();
    let mut out: Vec<Assignment> = Vec::new();
    for a in list {
        let mut keep = Vec::new();
        for p in &a.patterns {
            if taken.iter().any(|t| overlaps(t, p)) {
                if !serial.contains(p) {
                    serial.push(p.clone());
                }
            } else {
                taken.push(p.clone());
                keep.push(p.clone());
            }
        }
        out.push(Assignment {
            agent: a.agent.clone(),
            patterns: keep,
        });
    }
    (out, serial)
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. 段 (どこまで効いているかを正直に出す)
// ═══════════════════════════════════════════════════════════════════════════

/// 効力の段。**「効いていると思わせて実は勧告」は無いより悪い。**
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tier {
    /// フックが設置済み — 書き込みが実際に止まる
    Enforced,
    /// 台帳はあるがフックが無い — 画面が警告するだけ
    Advisory,
    /// 無効
    #[default]
    Off,
}

impl Tier {
    /// UI に出す短い名前 (tr のキーになる日本語原文)。
    pub fn label(self) -> &'static str {
        match self {
            Tier::Enforced => "強制",
            Tier::Advisory => "勧告",
            Tier::Off => "無効",
        }
    }

    /// 何が起きるかの 1 行。
    pub fn detail(self) -> &'static str {
        match self {
            Tier::Enforced => {
                // **「全部止まる」と読ませない。** 止まるのはフックを設置できた
                // エージェントの書き込みだけで、カタログにフックを持たない
                // エージェント (現状は claude 以外のほぼ全部) は対象外。
                // ここを大きく書くと「強制と出ているのに止まらない」になり、
                // このモジュール自身が「無いより悪い」と呼んでいる状態になる。
                "フックを設置したエージェントの書き込みはブロックされます (フックを持たないエージェントとエディタ外の書き込みは対象外)"
            }
            Tier::Advisory => {
                "所有は記録しますが、ブロックはしません (フックを設置すると強制になります)"
            }
            Tier::Off => "このワークスペースでは何もしていません",
        }
    }

    /// 段に対応する色。
    ///
    /// `theme::Theme::ok` は egui の `Visuals` へ写されていない
    /// (`theme::apply` が移すのは panel / accent / border など) ため、
    /// 「成功」だけは明暗 2 通りをここで持つ。値は
    /// [`tests::段の色はどのテーマでも読める`] が全 11 テーマの背景に対して
    /// コントラスト比を検算している (WCAG AA 大文字 3.0 以上)。
    pub fn color(self, v: &egui::Visuals) -> egui::Color32 {
        match self {
            Tier::Enforced => {
                if v.dark_mode {
                    egui::Color32::from_rgb(0x7e, 0xc6, 0x99)
                } else {
                    egui::Color32::from_rgb(0x11, 0x6b, 0x3a)
                }
            }
            Tier::Advisory => v.warn_fg_color,
            Tier::Off => v.weak_text_color(),
        }
    }
}

/// **実際にブロックできるエージェント**の一覧 (カタログから起こす)。
///
/// 画面に「強制」とだけ出すと、対応していないエージェントを使っている人が
/// 「自分も守られている」と誤解する。誰が対象なのかを必ず名前で出すこと。
pub fn gated_agents() -> Vec<&'static str> {
    crate::agents::HOOK_TARGETS.iter().map(|t| t.bin).collect()
}

/// 段の決め方 (純粋)。
pub fn tier(store_exists: bool, hook_installed: bool) -> Tier {
    match (store_exists, hook_installed) {
        (true, true) => Tier::Enforced,
        (true, false) => Tier::Advisory,
        (false, _) => Tier::Off,
    }
}

/// いまの段を実際に調べる (I/O)。**UI スレッドから直接呼ばない**。
///
/// 2 つのルートを取り違えないこと: 台帳は `key` (元のリポジトリ)、
/// フックの設定ファイル (`.claude/settings.json`) は `tree` (いまの作業ツリー)。
/// 片方で両方を引くと「有効にした直後に無効と出る」(実際に e2e で出した)。
pub fn current_tier(roots: &Roots) -> Tier {
    let store = store_path_in(&store_dir(), &roots.key);
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zai"));
    // 1 つでも設置済みなら「強制」。カタログを回すのでリテラルは持たない。
    let installed = crate::agents::HOOK_TARGETS.iter().any(|t| {
        crate::supervisor::hooks::plan_for(t.bin, &roots.tree, &exe)
            .map(|p| crate::supervisor::hooks::status(&p))
            == Some(crate::supervisor::hooks::HookStatus::Installed)
    });
    tier(enabled(&store), installed)
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. レイアウト (純粋関数 — 極端な寸法でテーブルテストする)
// ═══════════════════════════════════════════════════════════════════════════

/// 1 行ぶんの矩形。**どの幅でも見切れないこと**を関数で保証する。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RowLayout {
    pub owner: egui::Rect,
    pub patterns: egui::Rect,
    pub left: egui::Rect,
    pub actions: egui::Rect,
}

/// 幅が狭いときはボタンをアイコンだけへ縮退させる。
pub fn is_compact(width: f32) -> bool {
    width < COMPACT_WIDTH
}

/// 行のレイアウト。可用領域・最長の持ち主名から列幅を決める。
///
/// 決め方:
/// * 操作列は固定 (狭いときはアイコン 1 個ぶん)
/// * 残り期限は固定
/// * 持ち主は「最長の名前」と「可用幅の 30%」の小さい方、下限あり
/// * パターン列が残り全部を取る (**必ず 0 以上**に切り詰める)
pub fn row_layout(avail: egui::Rect, longest_owner: f32) -> RowLayout {
    const GAP: f32 = 8.0;
    let w = avail.width();
    let actions = if is_compact(w) { 30.0 } else { 76.0 };
    let left = if is_compact(w) { 52.0 } else { 88.0 };
    // 下限 40pt。可用幅が極端に狭いときでも負にしない。
    let owner = longest_owner.clamp(40.0, (w * 0.30).max(40.0));
    // 固定列 + 隙間を引いた残り。負にならないよう 0 で止める。
    let fixed = owner + left + actions + GAP * 3.0;
    let patterns = (w - fixed).max(0.0);
    // 残りが足りないときは持ち主列から削る (パターン列を最低 40pt 確保)。
    let (owner, patterns) = if patterns < 40.0 {
        let want = 40.0f32.min((w - left - actions - GAP * 3.0).max(0.0));
        let o = (w - left - actions - GAP * 3.0 - want).max(0.0);
        (o, want)
    } else {
        (owner, patterns)
    };
    let y = avail.y_range();
    let mut x = avail.left();
    let mut col = |width: f32| {
        let r = egui::Rect::from_x_y_ranges(x..=(x + width), y);
        x += width + GAP;
        r
    };
    RowLayout {
        owner: col(owner),
        patterns: col(patterns),
        left: col(left),
        actions: col(actions),
    }
}

/// 空状態のカード。**利用可能領域の中央**に 1 枚 (下や上に取り残さない)。
pub fn empty_card(avail: egui::Rect) -> egui::Rect {
    let w = (avail.width() * 0.72).clamp(0.0, 420.0).min(avail.width());
    let h = 132.0f32.min(avail.height());
    egui::Rect::from_center_size(avail.center(), egui::vec2(w, h))
}

// ═══════════════════════════════════════════════════════════════════════════
//  10. UI — パレットから開くパネル
// ═══════════════════════════════════════════════════════════════════════════

/// パレットへの登録。**共有ファイルを 1 バイトも触らずに機能が繋がる**入口
/// (`src/features/lease.rs` が `pub use` するだけで build.rs が拾う)。
///
/// 打鍵は割り当てていない — `keybinds::BindAction` は固定長配列 + 件数検査を
/// 持つ最も硬い共有面で、機能ブランチ側から増やすと直列マージが必ず衝突する。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "lease",
    entries: &[crate::feature::Entry {
        icon: "🔐",
        label: "ファイル所有の一覧 — 並列エージェントの衝突を防ぐ",
        id: "lease.list",
    }],
    dispatch: |_app, _ctx, id| match id {
        "lease.list" => {
            open_panel();
            true
        }
        _ => false,
    },
    // パネルはウィンドウとして自分で描く (`app.rs` のビュー列挙に触らない)。
    draw: Some(draw),
    settings: &[
        crate::feature::Setting {
            key: "lease.auto_arm",
            label: "worktree を見つけたらファイル所有ガードを自動で有効にする",
            help: "linked worktree がぶら下がっている / 自分がその中にいるときだけ有効になります。                   単独のリポジトリでは何もしません。",
            default: crate::feature::SettingValue::Bool(true),
        },
        crate::feature::Setting {
            key: "lease.ttl_minutes",
            label: "ファイル所有の寿命 (分)",
            help: "この時間だけ黙っている担当は所有権を失います。                   死んだエージェントにリポジトリを人質へ取らせないための上限です。",
            default: crate::feature::SettingValue::Int(30),
        },
        crate::feature::Setting {
            key: KEY_GATE_READ_CAP,
            label: "行域を出すために読むファイルの上限 (バイト)",
            help: "これを超えるファイルは「同じファイルの違う行」を分け合えず、ファイル全体の担当になります。                   生成コード・lock・データファイルが 1MiB を超えるリポジトリでは、上げるのが行単位に戻す唯一の手段です。                   既定より小さい値は既定へ引き上げます (下げても効かないため)。0 か未設定で既定 1MiB。",
            default: crate::feature::SettingValue::Int(GATE_READ_CAP as i64),
        },
    ],
    ..crate::feature::Feature::DEFAULT
};

/// 台帳の非同期読み取り 1 回ぶん。
struct Snapshot {
    store: Result<Store, String>,
    tier: Tier,
    cost: Duration,
}

/// パネルの状態。**ウィンドウより長生きさせる** (設計原則 1) ため、
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
/// こうすると `app.rs` を 1 バイトも触らずに機能が繋がる。
#[derive(Default)]
struct PanelState {
    open: bool,
    roots: Roots,
    store: Store,
    tier: Tier,
    error: String,
    toast: String,
    /// 走っている読み取り。UI スレッドは**絶対に待たない**。
    pending: Option<Receiver<Snapshot>>,
    last_scan: Option<Instant>,
    last_cost: Option<Duration>,
    /// 事前チェックの入力欄。
    plan_text: String,
    /// 自分で確保するときのパターン。
    claim_text: String,
}

fn state() -> &'static Mutex<PanelState> {
    static S: OnceLock<Mutex<PanelState>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(PanelState::default()))
}

/// GUI が開いているワークスペースのルート。
///
/// `app.rs` へ触らずに済ませるため、**自分自身のインスタンス登録**
/// (`~/.zaivern/instances/<pid>.json`) から引く。登録が無い / 壊れている
/// ときはカレントディレクトリへ落ちる (fail-soft)。
pub fn gui_workspace_root() -> PathBuf {
    let me = std::process::id();
    let found = crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from));
    found.unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// パレットの項目から呼ぶ入口。
pub fn open_panel() {
    let roots = roots_of(&gui_workspace_root());
    if let Ok(mut st) = state().lock() {
        st.open = true;
        st.roots = roots;
        st.last_scan = None; // 開いた回だけ必ず取り直す
        st.toast.clear();
    }
}

/// 台帳の読み取りを**裏のスレッド**へ出す。UI は手元の値を描き続ける。
///
/// git の教訓と同じで、UI スレッドで同期 I/O を撃つと最悪のときにフレームが
/// 止まる (実測: 同期 `git branch --show-current` が 6023ms / 最悪フレーム 4376ms)。
/// 台帳は小さいが、ロック待ち最大 200ms が乗り得るので裏へ出す。
fn spawn_scan(roots: Roots) -> Receiver<Snapshot> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let store = read_store(&store_path_in(&store_dir(), &roots.key));
        let tier = current_tier(&roots);
        let _ = tx.send(Snapshot {
            store,
            tier,
            cost: t0.elapsed(),
        });
    });
    rx
}

/// 毎フレーム呼ばれる描画。**閉じているフレームは 1 ピクセルも触らない**
/// (設計原則 3: アイドル時のコストはゼロ)。
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut action = PanelAction::None;
    egui::Window::new(tr("🔐 ファイル所有の一覧"))
        .collapsible(false)
        .resizable(true)
        .default_width(660.0)
        .default_height(480.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            action = body(ui, &mut st);
        });
    if !open {
        st.open = false;
    }
    apply(&mut st, action);
}

/// パネルが要求した副作用 (描画の中では I/O をしない)。
enum PanelAction {
    None,
    Enable,
    Release(usize),
    Claim,
    Refresh,
}

fn apply(st: &mut PanelState, action: PanelAction) {
    let store_path = store_path_in(&store_dir(), &st.roots.key);
    match action {
        PanelAction::None => {}
        PanelAction::Refresh => st.last_scan = None,
        PanelAction::Enable => {
            st.toast = match enable(&store_path) {
                Ok(()) => tr("このワークスペースでファイル所有リースを有効にしました"),
                Err(e) => e,
            };
            st.last_scan = None;
        }
        PanelAction::Release(i) => {
            let Some(holder) = st.store.leases.get(i).map(|l| l.holder.clone()) else {
                return;
            };
            let n = with_store(&store_path, |s| release(s, &holder));
            st.toast = match n {
                Ok(n) => trf("{n} 件のリースを解放しました", &[("n", n.to_string())]),
                Err(e) => e,
            };
            st.last_scan = None;
        }
        PanelAction::Claim => {
            let patterns: Vec<String> = st
                .claim_text
                .split([',', '\n'])
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect();
            if patterns.is_empty() {
                return;
            }
            let holder = Holder {
                agent: tr("あなた (Zaivern Code)"),
                session: String::new(),
                cwd: normalize_path(&st.roots.tree.to_string_lossy()),
                pid: std::process::id(),
            };
            let now = now_secs();
            let res = with_store(&store_path, |s| {
                try_claim(s, &holder, &patterns, now, DEFAULT_TTL_SECS, &pid_alive)
            });
            st.toast = match res {
                Ok(Claim::Granted(n)) => {
                    st.claim_text.clear();
                    trf("{n} 件のパターンを確保しました", &[("n", n.to_string())])
                }
                Ok(Claim::Refused { owner, pattern, .. }) => trf(
                    "確保できません: 「{pattern}」は {owner} が持っています",
                    &[("pattern", pattern), ("owner", owner)],
                ),
                Err(e) => e,
            };
            st.last_scan = None;
        }
    }
}

/// 非同期の結果を拾い、必要なら次の走査を出す。**待たない**。
fn poll(st: &mut PanelState, ctx: &egui::Context) {
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(snap) => {
                match snap.store {
                    Ok(s) => {
                        st.store = s;
                        st.error.clear();
                    }
                    Err(e) => st.error = e,
                }
                st.tier = snap.tier;
                st.last_cost = Some(snap.cost);
                st.last_scan = Some(Instant::now());
                st.pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if st.pending.is_none() {
        // 間隔は所要時間に応じて自動で空ける (遅い環境で走らせ続けない)。
        let due = st
            .last_scan
            .is_none_or(|t| t.elapsed() >= crate::git::scan_interval(SCAN_BASE, st.last_cost));
        if due {
            st.pending = Some(spawn_scan(st.roots.clone()));
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す。
    ctx.request_repaint_after(Duration::from_millis(250));
}

fn body(ui: &mut egui::Ui, st: &mut PanelState) -> PanelAction {
    let mut action = PanelAction::None;
    let tier_now = st.tier;
    let vis = ui.visuals().clone();
    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(format!("● {}", tr(tier_now.label())))
                .color(tier_now.color(&vis))
                .strong(),
        )
        .on_hover_text(tr(tier_now.detail()));
        // worktree のときは 2 つのルートが別物になる。**それを隠さない** —
        // 「なぜ別フォルダの相手と衝突するのか」がここでしか判らない。
        let hover = if st.roots.tree == st.roots.key {
            st.roots.key.display().to_string()
        } else {
            trf(
                "台帳の単位 (元のリポジトリ): {key}\n作業ツリー: {tree}",
                &[
                    ("key", st.roots.key.display().to_string()),
                    ("tree", st.roots.tree.display().to_string()),
                ],
            )
        };
        ui.label(egui::RichText::new(ellipsize(&st.roots.key.to_string_lossy(), 52)).weak())
            .on_hover_text(hover);
        if ui.button("⟳").on_hover_text(tr("読み直す")).clicked() {
            action = PanelAction::Refresh;
        }
    });
    ui.separator();

    if tier_now == Tier::Off {
        // 空状態は**中央に 1 枚のカード**で (CLAUDE.md「空白は作らない」)。
        let avail = ui.available_rect_before_wrap();
        let card = empty_card(avail);
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(card), |ui| {
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new(tr("このワークスペースでは無効です")).strong());
                ui.label(
                    egui::RichText::new(tr(
                        "有効にすると、書き込みのたびにファイルの所有を記録します。\nフックを設置してあれば、他人が持つファイルへの書き込みは実際に止まります。",
                    ))
                    .weak(),
                );
                if ui.button(tr("このワークスペースで有効にする")).clicked() {
                    action = PanelAction::Enable;
                }
            });
        });
        toast_line(ui, st);
        return action;
    }

    if !st.error.is_empty() {
        ui.label(egui::RichText::new(st.error.clone()).color(vis.error_fg_color));
    }

    egui::ScrollArea::vertical()
        .id_salt("zv-lease-body")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            if let Some(a) = lease_rows(ui, st, &vis) {
                action = a;
            }
            ui.add_space(8.0);
            ui.separator();
            if claim_form(ui, st) {
                action = PanelAction::Claim;
            }
            ui.add_space(8.0);
            ui.separator();
            plan_section(ui, st, &vis);
        });
    toast_line(ui, st);
    action
}

fn toast_line(ui: &mut egui::Ui, st: &PanelState) {
    if !st.toast.is_empty() {
        ui.separator();
        ui.label(egui::RichText::new(st.toast.clone()).weak());
    }
}

fn lease_rows(ui: &mut egui::Ui, st: &PanelState, vis: &egui::Visuals) -> Option<PanelAction> {
    let now = now_secs();
    if st.store.leases.is_empty() {
        ui.label(
            egui::RichText::new(tr(
                "確保中のファイルはありません (エージェントが書き込むと自動で登録されます)",
            ))
            .weak(),
        );
        return None;
    }
    let mut action = None;
    let longest = st
        .store
        .leases
        .iter()
        .map(|l| l.holder.display().chars().count() as f32 * 7.0)
        .fold(40.0f32, f32::max);
    for (i, l) in st.store.leases.iter().enumerate() {
        let w = ui.available_width();
        let row = egui::Rect::from_min_size(ui.next_widget_position(), egui::vec2(w, 20.0));
        let lay = row_layout(row, longest);
        let compact = is_compact(w);
        ui.horizontal(|ui| {
            ui.allocate_ui(egui::vec2(lay.owner.width(), 20.0), |ui| {
                ui.label(ellipsize(&l.holder.display(), 24))
                    .on_hover_text(l.holder.display());
            });
            ui.allocate_ui(egui::vec2(lay.patterns.width(), 20.0), |ui| {
                let joined = l.patterns.join(", ");
                ui.label(egui::RichText::new(ellipsize(&joined, 48)).monospace())
                    .on_hover_text(joined);
            });
            ui.allocate_ui(egui::vec2(lay.left.width(), 20.0), |ui| {
                let left = l.expires_at.saturating_sub(now);
                let txt = crate::instances::humanize_uptime(left);
                let c = if left == 0 {
                    vis.warn_fg_color
                } else {
                    vis.weak_text_color()
                };
                ui.label(egui::RichText::new(txt).color(c))
                    .on_hover_text(tr("この時間を過ぎると自動で解放されます"));
            });
            let label = if compact {
                "✖".to_string()
            } else {
                tr("解放")
            };
            if ui
                .button(label)
                .on_hover_text(tr("このリースを解放する (引き継ぐとき)"))
                .clicked()
            {
                action = Some(PanelAction::Release(i));
            }
        });
    }
    action
}

fn claim_form(ui: &mut egui::Ui, st: &mut PanelState) -> bool {
    let mut go = false;
    ui.label(egui::RichText::new(tr("自分で確保する")).strong());
    ui.horizontal_wrapped(|ui| {
        let w = (ui.available_width() - 120.0).clamp(120.0, 420.0);
        ui.add(
            egui::TextEdit::singleline(&mut st.claim_text)
                .desired_width(w)
                .hint_text(tr("src/auth/**, README.md")),
        );
        if ui
            .button(tr("確保"))
            .on_hover_text(tr("重なりがあれば拒否されます (後勝ちにはしません)"))
            .clicked()
        {
            go = true;
        }
    });
    go
}

fn plan_section(ui: &mut egui::Ui, st: &mut PanelState, vis: &egui::Visuals) {
    ui.label(egui::RichText::new(tr("配る前に重なりを見る")).strong());
    ui.label(
        egui::RichText::new(tr(
            "1 行に「担当: パターン, パターン」。配る前に重なりが判ります",
        ))
        .weak(),
    );
    let w = ui.available_width().max(120.0);
    ui.add(
        egui::TextEdit::multiline(&mut st.plan_text)
            .desired_width(w)
            .desired_rows(3)
            .hint_text("A: src/auth/**\nB: src/ui/**, README.md"),
    );
    let list = parse_assignments(&st.plan_text);
    if list.is_empty() {
        return;
    }
    let ovs = plan_overlaps(&list);
    if ovs.is_empty() {
        ui.label(
            egui::RichText::new(trf(
                "{n} 人の担当は互いに素です。そのまま配れます",
                &[("n", list.len().to_string())],
            ))
            .color(Tier::Enforced.color(vis)),
        );
        return;
    }
    ui.label(
        egui::RichText::new(trf(
            "{n} 件の重なりがあります — このまま配ると、衝突はマージのときまで見えません",
            &[("n", ovs.len().to_string())],
        ))
        .color(vis.warn_fg_color),
    );
    for o in ovs.iter().take(8) {
        let (a, b) = (&list[o.a].agent, &list[o.b].agent);
        ui.label(
            egui::RichText::new(ellipsize(
                &format!("{a} 「{}」 ↔ {b} 「{}」", o.pattern_a, o.pattern_b),
                72,
            ))
            .monospace()
            .weak(),
        );
    }
    let (split, serial) = split_plan(&list);
    ui.label(egui::RichText::new(tr("分割案 (これなら重なりません)")).strong());
    for a in &split {
        let line = if a.patterns.is_empty() {
            trf("{agent}: (割り当て無し)", &[("agent", a.agent.clone())])
        } else {
            format!("{}: {}", a.agent, a.patterns.join(", "))
        };
        ui.label(egui::RichText::new(ellipsize(&line, 72)).monospace());
    }
    if !serial.is_empty() {
        ui.label(
            egui::RichText::new(trf("直列にやる分: {list}", &[("list", serial.join(", "))]))
                .color(vis.warn_fg_color),
        );
    }
}

/// 長い文字列を省略する (全文はホバーで出す)。
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let head: String = s.chars().take(max.saturating_sub(1)).collect();
    format!("{head}…")
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. 編集者ガード — 「このエディタを使っていれば衝突しない」の実体
// ═══════════════════════════════════════════════════════════════════════════
//
//  ここまでの節 (フック経路) が守るのは**外部 CLI エージェントの書き込み**だけ
//  で、**このエディタ自身の保存は素通り**していた。つまり 2 つの worktree で
//  Zaivern Code を開いて同じファイルを直接編集すると、台帳は 1 件も見ずに
//  両方が書けてしまう。それでは「このエディタを使っていれば衝突しない」とは
//  言えない。この節がその穴を塞ぐ。
//
//  ## 3 つの約束
//
//  1. **編集を始めた瞬間に確保する** (開いただけでは取らない)。読むだけの
//     タブが所有権を握ると、閲覧しているあいだ他人が永久に待たされる。
//  2. **UI スレッドを絶対に待たせない**。確保と解放はワーカースレッドで行い、
//     描画は「いま手元にある答え」(古くてよい) だけを見る。CLAUDE.md の
//     「git は UI スレッドで待たない」と同じ規律。
//  3. **保存の直前は fail-closed で確かめる**。ここだけは古い答えを使わない。
//     台帳は tmp → rename で置き換わるので、**ロック無しで読んでも
//     書きかけを見ることはない** — だから同期で読んでも数百マイクロ秒で返る。
//
//  ## なぜ「絶対」と言えるのか / 言えないのか
//
//  同じファイルを 2 人が同時に編集する状況そのものが起きなくなるので、
//  **テキストとしてのマージ衝突は構造的に起こらない**。一方で、別々の
//  ファイルを触った結果が意味的に噛み合わない (API を片方が変え、もう片方が
//  古い呼び方のまま等) ことは防げない。**そこは正直に区別する。**

/// 1 つのファイルについて、いま分かっている所有状態。
///
/// `Pending` の間も**編集は止めない**。確保の往復でキー入力が詰まるくらいなら、
/// answer が返った時点で知らせるほうがよい (保存は必ず [`check_write`] を通る
/// ので、取り損ねたまま書けてしまうことはない)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Own {
    /// 機能が無効、またはスコープ外。関知しない。
    Off,
    /// 確保を依頼したが答えがまだ。
    Pending,
    /// 自分のもの。書いてよい。
    Mine { until: u64 },
    /// 他人のもの。**保存を止める**。
    Taken { owner: String, reason: String },
}

/// 状態が変わったことの知らせ (トーストに出す)。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Notice {
    /// スコープ相対のパス。
    pub rel: String,
    pub own: Own,
}

/// ワーカーへの依頼。
enum Req {
    /// 起動直後に 1 度だけ: 段を「強制」まで引き上げる。
    Enforce,
    Claim(String),
    Release(String),
    ReleaseAll,
}

/// ガードの状態。**ウィンドウより長生きさせる** (設計原則 1) ため、
/// `ZaivernApp` のフィールドではなくモジュール側に置く。
struct GuardState {
    /// 有効か。無効なら全経路が `stat` すら踏まずに抜ける (設計原則 3)。
    armed: bool,
    roots: Roots,
    store: PathBuf,
    holder: Holder,
    /// スコープ相対パス → 所有状態。
    own: BTreeMap<String, Own>,
    /// 未読の知らせ。[`pump`] が回収する。
    notices: Vec<Notice>,
    /// いまの段。**「効いていると思わせて実は勧告」は無いより悪い**ので、
    /// 画面にはこの値をそのまま出す。
    tier: Tier,
    /// リースの寿命 (秒)。`lease.ttl_minutes` 由来。
    ttl: u64,
    tx: Option<Sender<Req>>,
}

impl Default for GuardState {
    fn default() -> Self {
        GuardState {
            armed: false,
            roots: Roots::default(),
            store: PathBuf::new(),
            holder: Holder::default(),
            own: BTreeMap::new(),
            notices: Vec::new(),
            tier: Tier::Off,
            ttl: DEFAULT_TTL_SECS,
            tx: None,
        }
    }
}

fn guard() -> &'static Mutex<GuardState> {
    static G: OnceLock<Mutex<GuardState>> = OnceLock::new();
    G.get_or_init(|| Mutex::new(GuardState::default()))
}

/// このエディタ自身の持ち主。
///
/// **`session` を PID から起こすのが肝**で、これが無いと同じ worktree で
/// 2 つ起動したインスタンスが `cwd` + `agent` の一致で「同じ持ち主」に
/// 見えてしまい、互いの所有を素通りさせる ([`Holder::same`] の規則)。
fn editor_holder(tree: &Path) -> Holder {
    Holder {
        agent: tr("Zaivern Code"),
        session: format!("zai-{}", std::process::id()),
        cwd: normalize_path(&tree.to_string_lossy()),
        pid: std::process::id(),
    }
}

/// この場所に**衝突の危険があるか** (= 自動で有効化してよいか)。
///
/// 危険が無いのに常時有効化すると、単独で使っている人が台帳の読み書きを
/// 払わされる (設計原則 3: アイドル時のコストはゼロ)。逆に危険があるのに
/// 黙っていると「このエディタを使っていれば衝突しない」が嘘になる。
/// 判定は **stat 数回**で、git は 1 回も起動しない。
pub fn risky(roots: &Roots) -> bool {
    // 1. 自分が linked worktree にいる = 元リポジトリを誰かと分け合っている。
    if roots.key != roots.tree {
        return true;
    }
    // 2. 元リポジトリに linked worktree がぶら下がっている。
    let wt = roots.key.join(".git").join("worktrees");
    std::fs::read_dir(&wt).is_ok_and(|mut d| d.next().is_some())
}

/// ワークスペースを開いたときに 1 度だけ呼ぶ。**危険があれば自動で有効化する**。
///
/// 返り値は「このスコープでガードが効いているか」。
pub fn arm(start: &Path, auto: bool, ttl_minutes: i64) -> bool {
    arm_in(&store_dir(), start, auto, ttl_minutes)
}

/// 設定の分をリースの寿命 (秒) へ。極端な値でも壊れないように畳む。
fn ttl_from_minutes(minutes: i64) -> u64 {
    let m = minutes.clamp(1, 24 * 60) as u64;
    m.saturating_mul(60)
}

/// 台帳の置き場所を明示する [`arm`]。
///
/// 本番は [`store_dir`] (`~/.zaivern/leases`) を渡す。**テストが実 `~/.zaivern`
/// に触れないため**に置き場所を引数へ出してある (環境変数で分岐させると、
/// 本番の経路にテスト専用の枝が残る)。
pub fn arm_in(dir: &Path, start: &Path, auto: bool, ttl_minutes: i64) -> bool {
    let roots = roots_of(start);
    let store = store_path_in(dir, &roots.key);
    // 既に有効なら尊重する。無効でも危険があれば自動で有効化する
    // (`lease.auto_arm` を切っている人には勝手に入らない)。
    if !enabled(&store) && auto && risky(&roots) && enable(&store).is_err() {
        return false;
    }
    if !enabled(&store) {
        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
        *g = GuardState::default();
        return false;
    }
    let holder = editor_holder(&roots.tree);
    let ttl = ttl_from_minutes(ttl_minutes);
    // 記号指定を実ファイルへ突き合わせる基準 (`zai` の cwd ではなく作業ツリー)。
    let tree = roots.tree.clone();
    let roots_bg = roots.clone();
    let (tx, rx) = std::sync::mpsc::channel::<Req>();
    {
        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
        g.armed = true;
        g.roots = roots;
        g.store = store.clone();
        g.holder = holder.clone();
        g.own.clear();
        g.notices.clear();
        g.tier = Tier::Advisory;
        g.ttl = ttl;
        g.tx = Some(tx.clone());
    }
    // 段の引き上げ (設定ファイルの読み書き) は**必ず裏で**。
    let _ = tx.send(Req::Enforce);
    // ワーカー。**I/O 中は状態のロックを握らない** (握ると描画が止まる)。
    std::thread::Builder::new()
        .name("zai-lease-guard".into())
        .spawn(move || {
            while let Ok(req) = rx.recv() {
                match req {
                    Req::Enforce => {
                        let t = ensure_enforced(&roots_bg);
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        if g.store != store {
                            break;
                        }
                        g.tier = t;
                    }
                    Req::Claim(rel) => {
                        let own =
                            claim_in(&tree, &store, &holder, &rel, now_secs(), ttl, &pid_alive);
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        // arm し直された後の遅れた答えは捨てる。
                        if g.store != store {
                            break;
                        }
                        g.notices.push(Notice {
                            rel: rel.clone(),
                            own: own.clone(),
                        });
                        g.own.insert(rel, own);
                    }
                    Req::Release(rel) => {
                        let _ = release_one(&store, &holder, &rel);
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        if g.store != store {
                            break;
                        }
                        g.own.remove(&rel);
                    }
                    Req::ReleaseAll => {
                        let _ = with_store(&store, |s| release(s, &holder));
                        let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
                        if g.store != store {
                            break;
                        }
                        g.own.clear();
                    }
                }
            }
        })
        .ok();
    true
}

/// 段を「強制」まで引き上げる。**ワーカースレッド専用** (設定ファイルを触る)。
///
/// 台帳を置くだけでは [`Tier::Advisory`] = 画面が警告するだけで、
/// **エージェントの書き込みは 1 件も止まらない**。「このエディタを使っていれば
/// 衝突しない」と言うためには、エージェント側のフックまで設置して
/// [`Tier::Enforced`] にする必要がある。
///
/// 設置は [`crate::supervisor::hooks::install`] が既存の設定を**バックアップしてから**
/// 行い、[`crate::supervisor::hooks::uninstall`] で元へ戻せる。だから自動でやってよい —
/// 戻せない変更なら聞くべきだが、これは戻せる。
fn ensure_enforced(roots: &Roots) -> Tier {
    let now = current_tier(roots);
    if now != Tier::Advisory {
        return now;
    }
    let exe = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("zai"));
    for t in crate::agents::HOOK_TARGETS {
        let Some(plan) = crate::supervisor::hooks::plan_for(t.bin, &roots.tree, &exe) else {
            continue;
        };
        // 既に入っているものは触らない (ユーザーの設定を上書きしない)。
        if crate::supervisor::hooks::status(&plan)
            == crate::supervisor::hooks::HookStatus::Installed
        {
            continue;
        }
        let _ = crate::supervisor::hooks::install(&plan);
    }
    current_tier(roots)
}

/// いまの段 (**古くてよい**)。画面にそのまま出す。
pub fn tier_now() -> Tier {
    guard()
        .lock()
        .map(|g| if g.armed { g.tier } else { Tier::Off })
        .unwrap_or(Tier::Off)
}

/// ガードを降ろす (ワークスペースを閉じた / テストの後始末)。
///
/// ワーカーは送信端が落ちた時点で `recv` が失敗して自然に終わる。
#[cfg(test)]
pub fn disarm() {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    *g = GuardState::default();
}

/// ガードが効いているか。描画側の早期脱出用。
pub fn armed() -> bool {
    guard().lock().map(|g| g.armed).unwrap_or(false)
}

/// 絶対パスをスコープ相対へ。スコープ外なら `None` = 関知しない。
fn rel_of(g: &GuardState, abs: &Path) -> Option<String> {
    rel_within(&g.roots.tree, abs)
}

/// **いま編集中のファイル集合をまとめて渡す。これが唯一の同期口。**
///
/// 「始まった / 終わった」を対で呼ばせる形にすると、**対の片側を呼び忘れた
/// 経路が 1 つでもあると所有が漏れ続ける** (タブを閉じた・元に戻した・
/// ワークスペースを切り替えた…)。集合を丸ごと渡してもらい、
/// **消えたものはこちらで解放する**ほうが漏れようがない。
///
/// 渡すのは「パスがあって・汚れている」バッファだけ。開いただけのタブを
/// 入れないこと — 読むだけのタブが所有権を握ると、閲覧しているあいだ
/// 他人が待たされる。
pub fn sync_edits(paths: &[PathBuf]) {
    if !armed() {
        return;
    }
    let want: Vec<String> = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        paths.iter().filter_map(|p| rel_of(&g, p)).collect()
    };
    for rel in &want {
        edit_begin_rel(rel);
    }
    let stale: Vec<String> = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        g.own
            .keys()
            .filter(|k| !want.contains(k))
            .cloned()
            .collect()
    };
    for rel in stale {
        edit_end_rel(&rel);
    }
}

/// スコープ相対で確保を依頼する。冪等。
fn edit_begin_rel(rel: &str) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed || g.own.contains_key(rel) {
        return;
    }
    g.own.insert(rel.to_string(), Own::Pending);
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Claim(rel.to_string()));
    }
}

/// スコープ相対で解放する。
fn edit_end_rel(rel: &str) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed || g.own.remove(rel).is_none() {
        return;
    }
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Release(rel.to_string()));
    }
}

#[allow(dead_code)]
fn edit_begin(abs: &Path) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed {
        return;
    }
    let Some(rel) = rel_of(&g, abs) else { return };
    // 既に確保済み / 依頼済みなら何もしない (キー入力ごとに依頼を積まない)。
    if g.own.contains_key(&rel) {
        return;
    }
    g.own.insert(rel.clone(), Own::Pending);
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Claim(rel));
    }
}

/// **もう編集しない** (タブを閉じた / 保存して汚れが消えた)。裏で解放する。
#[allow(dead_code)]
fn edit_end(abs: &Path) {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed {
        return;
    }
    let Some(rel) = rel_of(&g, abs) else { return };
    if g.own.remove(&rel).is_none() {
        return;
    }
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::Release(rel));
    }
}

/// いま分かっている所有状態 (**古くてよい**)。描画から毎フレーム呼ぶ。
pub fn own_of(abs: &Path) -> Own {
    let g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if !g.armed {
        return Own::Off;
    }
    let Some(rel) = rel_of(&g, abs) else {
        return Own::Off;
    };
    g.own.get(&rel).cloned().unwrap_or(Own::Off)
}

/// 背景の答えを回収する。毎フレーム呼び、返ったものをトーストに出す。
pub fn pump() -> Vec<Notice> {
    let mut g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if g.notices.is_empty() {
        return Vec::new();
    }
    std::mem::take(&mut g.notices)
}

/// 終了時 / ワークスペースを閉じるときに自分の所有を全部返す。
///
/// **返し損ねても TTL で必ず回収される**が、返せば次の担当がすぐ入れる。
pub fn release_all() {
    let g = guard().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(tx) = g.tx.as_ref() {
        let _ = tx.send(Req::ReleaseAll);
    }
}

/// **保存の直前の最終確認。** ここが fail-closed の関門。
///
/// 台帳を**ロック無しで**読む: 置き換えは tmp → rename なので書きかけは
/// 見えず、ロック待ち (最大 [`LOCK_WAIT_MS`]) を UI スレッドへ持ち込まずに
/// 済む。台帳が読めない / 壊れているときは **fail-open** で通す —
/// 保存できないほうがユーザーの損害が大きいので、ここは安全側が「通す」。
pub fn check_write(abs: &Path) -> Verdict {
    let (store, holder, tree) = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        if !g.armed {
            return Verdict::Allow;
        }
        (g.store.clone(), g.holder.clone(), g.roots.tree.clone())
    };
    let Some(rel) = rel_within(&tree, abs) else {
        return Verdict::Allow;
    };
    check_one(&store, &holder, &rel, now_secs(), &pid_alive)
}

/// `abs` **とその配下**に他人の所有があるか。フォルダの移動 / 削除の門。
///
/// [`check_write`] は 1 つのパスしか見ないので、**`src/` を消す操作は
/// 誰かが `src/app.rs` を確保していても素通りする**。フォルダごと動かす /
/// 捨てる操作は、中身の所有者にとっては上書きより強い破壊なので、
/// 配下まで見て止める。
///
/// ファイルを渡したときは [`check_write`] と同じ結果になる。
pub fn check_tree(abs: &Path) -> Verdict {
    let (store, holder, tree) = {
        let g = guard().lock().unwrap_or_else(|e| e.into_inner());
        if !g.armed {
            return Verdict::Allow;
        }
        (g.store.clone(), g.holder.clone(), g.roots.tree.clone())
    };
    let Some(rel) = rel_within(&tree, abs) else {
        return Verdict::Allow;
    };
    let Ok(st) = read_store(&store) else {
        // 読めないときの向きは [`check_write`] と揃える (そちらが唯一の規範)。
        return check_write(abs);
    };
    let now = now_secs();
    let alive: &dyn Fn(u32) -> bool = &pid_alive;
    // まず自分自身。次に配下。
    if let Verdict::Deny(m) = decide(&st, &holder, &rel, now, alive) {
        return Verdict::Deny(m);
    }
    let prefix = format!("{rel}/");
    for l in &st.leases {
        if l.holder.same(&holder) || !l.active(now, alive) {
            continue;
        }
        // 配下を指すパターンを 1 つでも持っていれば止める。
        if let Some(hit) = l.patterns.iter().find(|p| p.starts_with(&prefix)) {
            return Verdict::Deny(deny_reason(hit, l, now));
        }
    }
    Verdict::Allow
}

// ── 単体で試せる中身 (シングルトンを経由しない) ────────────────────────────

/// 1 パスを確保して、結果を [`Own`] で返す。
#[cfg(test)]
pub fn claim_one(
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Own {
    claim_for(store, holder, rel, now, DEFAULT_TTL_SECS, alive)
}

/// 寿命を明示する確保。`rel` には `src/a.rs#L10-40` の書き方も渡せる。
///
/// **`with_store_retry` を通す** — ここは UI スレッドではなく、
/// `Req::Claim` を受ける裏のワーカから呼ばれる (`arm_in` のループ)。
/// 混雑で取り逃すと `Own::Off` = 「所有が判らない」に落ちて画面が嘘をつくので、
/// 上限付きで作り直す価値がある。
pub fn claim_for(
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Own {
    // **基準フォルダを求めない。** [`try_claim`] が `#` の無いパスでは
    // [`default_tree`] へ降りないので、ファイル単位の確保 (画面から来る
    // 大多数) は `roots_of` の I/O を 1 バイトも増やさない。
    let pats = vec![rel.to_string()];
    own_of_claim(
        store,
        holder,
        rel,
        now,
        ttl,
        alive,
        with_store_retry(store, |s| try_claim(s, holder, &pats, now, ttl, alive)),
    )
}

/// 基準フォルダを明示する [`claim_for`]。
///
/// エディタ側 ([`arm_in`] のワーカ) は作業ツリーを知っているので必ずこちらを
/// 使う — `zai` の作業フォルダとエディタが開いているフォルダは別物であり、
/// 記号指定 (`src/a.rs#fn:name`) が別のファイルへ当たってしまう。
#[allow(clippy::too_many_arguments)]
pub fn claim_in(
    tree: &Path,
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Own {
    let pats = vec![rel.to_string()];
    own_of_claim(
        store,
        holder,
        rel,
        now,
        ttl,
        alive,
        with_store_retry(store, |s| {
            try_claim_in(tree, s, holder, &pats, now, ttl, alive)
        }),
    )
}

/// 確保の結果を画面用の [`Own`] へ。**2 つの入口で同じ落とし方をする**。
#[allow(clippy::too_many_arguments)]
fn own_of_claim(
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    ttl: u64,
    alive: &dyn Fn(u32) -> bool,
    out: Result<Claim, String>,
) -> Own {
    match out {
        // 台帳を触れないときは fail-open (編集を止めない)。保存前に再度見る。
        Err(_) => Own::Off,
        Ok(Claim::Granted(_)) => Own::Mine {
            until: now.saturating_add(ttl),
        },
        Ok(Claim::Refused { owner, .. }) => {
            let reason = match read_store(store) {
                Ok(s) => match decide(&s, holder, rel, now, alive) {
                    Verdict::Deny(m) => m,
                    Verdict::Allow => String::new(),
                },
                Err(_) => String::new(),
            };
            Own::Taken { owner, reason }
        }
    }
}

/// 1 パスを解放する。
///
/// `rel` にファイルを渡したら、**そのファイルの行域リースも全部落ちる**
/// (`src/a.rs` を手放したのに `src/a.rs#L10-40` が残っていたら、
///  画面の一覧から消えたのに書き込みは止まり続ける = いちばん困る形)。
/// `rel` に行域そのもの (`src/a.rs#L10-40`) を渡せばその 1 件だけ落ちる。
pub fn release_one(store: &Path, holder: &Holder, rel: &str) -> Result<(), String> {
    with_store_retry(store, |s| {
        for l in s.leases.iter_mut() {
            if l.holder.same(holder) {
                // **錨は patterns と同じ添字で落とす。** 片方だけ縮めると
                // 以後ずっと「別の域の錨」で取り直すことになる。
                l.align_anchors();
                let keep: Vec<bool> = l
                    .patterns
                    .iter()
                    .map(|p| p != rel && spec_path(p) != rel)
                    .collect();
                let mut it = keep.iter();
                l.patterns.retain(|_| it.next().copied().unwrap_or(true));
                let mut it = keep.iter();
                l.anchors.retain(|_| it.next().copied().unwrap_or(true));
            }
        }
        s.leases.retain(|l| !l.patterns.is_empty());
    })
}

/// 保存直前の判定 (ロック無しで読む)。
pub fn check_one(
    store: &Path,
    holder: &Holder,
    rel: &str,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Verdict {
    match read_store(store) {
        // **台帳が「在るのに読めない」ときは止める。**
        // ここを許可に倒していたため、台帳を壊す / 権限を落とすだけで
        // 誰でもガードを丸ごと無効化できた (敵対的検証で 7 通り破られた)。
        // 台帳が**無い**ときは `read_store` が空を返す = 機能が無効なので、
        // この腕に来るのは「在るのに読めない」ときだけ。
        // 文面には必ず**戻し方**を書く (でないとユーザーは機能を切るだけ)。
        Err(e) => Verdict::Deny(trf(
            "ファイル所有の台帳を読めないため、安全のため保存を止めました ({err})。\n             対処: (1) 台帳の権限を直す (2) `zai lease list` で状態を見る              (3) このワークスペースでガードを切る (`zai lease disable`)",
            &[("err", e)],
        )),
        Ok(s) => decide(&s, holder, rel, now, alive),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    // ── [高1] 絶対パスはスコープ相対へ畳む / 外なら断る ──────────────────
    #[test]
    fn 絶対パスはスコープ相対へ畳まれる() {
        let tree = unique_temp_dir("zaivern-lease-test", "abs-claim");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        std::fs::write(tree.join("src/a.rs"), "fn a(){}\n").unwrap();
        let abs = tree.join("src/a.rs");
        let got = resolve_spec_arg(&tree, &abs.to_string_lossy()).unwrap();
        // **相対指定とまったく同じ鍵になること**が全て。ここがずれると
        // 「確保しました」と言いながら 1 つも守らない。
        assert_eq!(normalize_spec(&got), normalize_spec("src/a.rs"));
        // 行域は 1 バイトも壊さない。
        let with_frag = format!("{}#L10-20", abs.to_string_lossy());
        assert_eq!(
            normalize_spec(&resolve_spec_arg(&tree, &with_frag).unwrap()),
            normalize_spec("src/a.rs#L10-20")
        );
        // 末尾の `/` (サブツリー) も保つ。
        let dir_spec = format!("{}/", tree.join("src").to_string_lossy());
        assert_eq!(
            normalize_spec(&resolve_spec_arg(&tree, &dir_spec).unwrap()),
            normalize_spec("src/")
        );
        // 相対はそのまま (従来の経路を 1 バイトも変えない)。
        assert_eq!(resolve_spec_arg(&tree, "src/a.rs").unwrap(), "src/a.rs");
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[test]
    fn スコープ外の絶対パスは成功と偽らず断る() {
        let tree = unique_temp_dir("zaivern-lease-test", "abs-outside");
        let other = unique_temp_dir("zaivern-lease-test", "abs-outside-other");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let outside = other.join("x.rs");
        let e = resolve_spec_arg(&tree, &outside.to_string_lossy()).unwrap_err();
        assert!(e.contains("スコープの外"), "理由が判る文言であること: {e}");
        let _ = std::fs::remove_dir_all(&tree);
        let _ = std::fs::remove_dir_all(&other);
    }

    #[test]
    fn ツリー自身を指したら配下ぜんぶになる() {
        let tree = unique_temp_dir("zaivern-lease-test", "abs-root");
        std::fs::create_dir_all(&tree).unwrap();
        let got = resolve_spec_arg(&tree, &tree.to_string_lossy()).unwrap();
        assert_eq!(got, "**");
        let _ = std::fs::remove_dir_all(&tree);
    }

    #[test]
    fn 絶対パスの綴りは三種類とも固定する() {
        // `normalize_path_on` と同じ流儀で、**動いている OS の綴りだけを
        // 検査して満足しない**。Windows のドライブ / UNC も絶対として扱う。
        for p in [
            "/abs/x.rs",
            "C:\\repo\\x.rs",
            "\\\\srv\\share\\x.rs",
            "\\abs\\x.rs",
        ] {
            assert!(is_absolute_any(p), "絶対として扱うべき: {p}");
        }
        for p in ["src/x.rs", "./src/x.rs", "x.rs"] {
            assert!(!is_absolute_any(p), "相対として扱うべき: {p}");
        }
    }

    // ── [中8] submodule は親と台帳を分ける ─────────────────────────────
    #[test]
    fn submoduleは名前の段数によらず自分の台帳を持つ() {
        let root = unique_temp_dir("zaivern-lease-test", "submod");
        let dot = root.join("parent/.git");
        std::fs::create_dir_all(&dot).unwrap();
        // 段数違いを両方固定する。**以前は 1 段だけ親へ畳まれていた**
        // (= 名前の付け方で挙動が変わる、という取り違え)。
        for name in ["flat", "vendor/sub", "deep/nest/sub"] {
            let sub = root.join("parent").join(name);
            std::fs::create_dir_all(&sub).unwrap();
            let up = "../".repeat(name.split('/').count());
            let text = format!("gitdir: {up}.git/modules/{name}\n");
            let got = main_repo_root_from_pointer(&text, &sub).expect("解決できること");
            assert_ne!(
                got,
                canonical_best_effort(&root.join("parent")),
                "submodule ({name}) を親の台帳へ畳まない"
            );
            assert!(
                got.to_string_lossy()
                    .replace('\\', "/")
                    .contains(".git/modules/"),
                "鍵は submodule の git ディレクトリ: {got:?}"
            );
        }
        // 通常の linked worktree は従来どおり**元のリポジトリ**へ寄る。
        let wt = root.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        let gd = root.join("parent/.git/worktrees/w1");
        std::fs::create_dir_all(&gd).unwrap();
        std::fs::write(gd.join("commondir"), "../..\n").unwrap();
        let got = main_repo_root_from_pointer(&format!("gitdir: {}\n", gd.display()), &wt).unwrap();
        assert_eq!(got, canonical_best_effort(&root.join("parent")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn submoduleのworktreeは本体と同じ台帳へ寄る() {
        // submodule から生やした worktree は `commondir` 経由で
        // **submodule 本体と同じ鍵**に着くこと (同じリポジトリなので
        // マージ衝突が起き得る = 台帳を共有すべき組)。
        let root = unique_temp_dir("zaivern-lease-test", "submod-wt");
        let sub_git = root.join("parent/.git/modules/vendor/sub");
        std::fs::create_dir_all(&sub_git).unwrap();
        let body = root.join("parent/vendor/sub");
        std::fs::create_dir_all(&body).unwrap();
        let main_key =
            main_repo_root_from_pointer("gitdir: ../../.git/modules/vendor/sub\n", &body).unwrap();
        let wt_git = sub_git.join("worktrees/w1");
        std::fs::create_dir_all(&wt_git).unwrap();
        std::fs::write(wt_git.join("commondir"), "../..\n").unwrap();
        let wt = root.join("subwt");
        std::fs::create_dir_all(&wt).unwrap();
        let wt_key =
            main_repo_root_from_pointer(&format!("gitdir: {}\n", wt_git.display()), &wt).unwrap();
        assert_eq!(main_key, wt_key, "submodule 本体とその worktree は同じ鍵");
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── [中7] ずらし幅の設定キーは交渉層と同じ綴り ────────────────────
    #[test]
    fn ずらし幅の設定キーは交渉層と同じ綴り() {
        // 定数を import できない (実体が私有 `mod imp`) ので綴りを持っている。
        // **ずれたら黙って既定へ落ちる**ので、実体のソースと突き合わせる。
        // CRLF のチェックアウトでも外れないよう改行を正規化する。
        let src = include_str!("negotiate.rs").replace("\r\n", "\n");
        let needle = format!("pub const KEY_MAX_SHIFT: &str = \"{KEY_MAX_SHIFT}\";");
        assert!(
            src.contains(&needle),
            "negotiate.rs の KEY_MAX_SHIFT と綴りが違う (探した: {needle})"
        );
    }

    // ── [中7] ずらし幅の上限が効く ────────────────────────────────────
    #[test]
    fn ずらし幅の上限を超えたら理由を出して断る() {
        let text: String = (1..=400).map(|i| format!("line {i}\n")).collect();
        let arc: std::sync::Arc<str> = std::sync::Arc::from(text.clone());
        let mut store = Store::default();
        // 先客が 1〜300 行を持っている → 新しい要求 (10-15) は 300 行超えないと入らない。
        let a = holder("A", "sa");
        assert!(matches!(
            try_claim_wants(
                &mut store,
                &a,
                &[Want {
                    spec: "src/a.rs#L1-300".into(),
                    anchor: crate::region::Anchor::default(),
                    text: Some(arc.clone()),
                }],
                100,
                600,
                &dead,
            ),
            Claim::Granted(_)
        ));
        let b = holder("B", "sb");
        let want = Want {
            spec: "src/a.rs#L10-15".into(),
            anchor: crate::region::Anchor::default(),
            text: Some(arc.clone()),
        };
        // 上限 10 行では届かない → **断る。理由に必要な行数と上限が出る。**
        let out = try_claim_wants_shift(
            &mut store.clone(),
            &b,
            &[want.clone()],
            100,
            600,
            &dead,
            Some(10),
        );
        match out {
            ShiftClaim::Refused { owner, .. } => {
                assert!(owner.contains("10"), "上限を出すこと: {owner}");
                assert!(
                    owner.contains("ずらす必要") || owner.contains("上限"),
                    "断る理由が具体的であること: {owner}"
                );
            }
            other => panic!("上限を超えたのに通った: {other:?}"),
        }
        // 上限を十分に取れば通る (上限判定だけが効いていること = 空きはある)。
        let out = try_claim_wants_shift(&mut store, &b, &[want], 100, 600, &dead, Some(1000));
        assert!(matches!(out, ShiftClaim::Granted(_)), "空きはあるので通る");
    }

    #[test]
    fn 上限内のずらしは従来どおり通る() {
        // **上限を入れたせいで今まで通っていたものが落ちない**ことを固定する。
        let text: String = (1..=400).map(|i| format!("line {i}\n")).collect();
        let arc: std::sync::Arc<str> = std::sync::Arc::from(text);
        let mut store = Store::default();
        let a = holder("A", "sa");
        try_claim_wants(
            &mut store,
            &a,
            &[Want {
                spec: "src/a.rs#L10-20".into(),
                anchor: crate::region::Anchor::default(),
                text: Some(arc.clone()),
            }],
            100,
            600,
            &dead,
        );
        let b = holder("B", "sb");
        let out = try_claim_wants_shift(
            &mut store,
            &b,
            &[Want {
                spec: "src/a.rs#L12-18".into(),
                anchor: crate::region::Anchor::default(),
                text: Some(arc),
            }],
            100,
            600,
            &dead,
            Some(50),
        );
        assert!(
            matches!(out, ShiftClaim::Granted(_)),
            "近くへずらせる: {out:?}"
        );
    }

    // ── [中4] 上限超えは「読めない」ではなく「大きすぎる」と言う ────────
    #[test]
    fn 上限超えのファイルは理由を取り違えない() {
        let tree = unique_temp_dir("zaivern-lease-test", "cap");
        std::fs::create_dir_all(tree.join("src")).unwrap();
        let big: String = (0..(GATE_READ_CAP / 16 + 64))
            .map(|i| format!("// pad line {i}\n"))
            .collect();
        assert!(big.len() as u64 > GATE_READ_CAP, "上限を超える大きさを作る");
        std::fs::write(tree.join("src/big.rs"), &big).unwrap();
        match read_capped_ex(&tree.join("src/big.rs"), &tree) {
            FileRead::TooLarge(size, cap) => {
                assert!(size > cap, "実サイズと上限の両方を返す");
            }
            other => panic!("上限超えとして返すこと: {other:?}"),
        }
        // 実在しないものは Unavailable (2 つを同じ値へ潰さない)。
        assert_eq!(
            read_capped_ex(&tree.join("src/nope.rs"), &tree),
            FileRead::Unavailable
        );
        // **行域はそのまま確保できる** (中身を読まなくても成立する)。
        let w = hydrate_in(&tree, "src/big.rs#L10-20").expect("行域は通る");
        assert_eq!(spec_span(&w.spec).map(|s| s.start), Some(10));
        // 記号指定だけが断られ、**文言は「大きすぎる」**であること。
        let e = hydrate_in(&tree, "src/big.rs#fn:nope").unwrap_err();
        assert!(e.contains("上限"), "理由が「上限超え」と判ること: {e}");
        assert!(
            !e.contains("読めませんでした"),
            "「読めない」と取り違えない: {e}"
        );
        // 劣化の告知が出ること (黙って全体所有へ落とさない)。
        let note = degradation_note(&tree, "src/big.rs#L10-20").expect("劣化を告げる");
        assert!(
            note.contains("ファイル全体"),
            "何が起きるかを言うこと: {note}"
        );
        // 小さいファイルでは何も言わない (雑音を足さない)。
        std::fs::write(tree.join("src/small.rs"), "fn a(){}\n").unwrap();
        assert_eq!(degradation_note(&tree, "src/small.rs#L1-2"), None);
        let _ = std::fs::remove_dir_all(&tree);
    }

    // ── [中5] 非 git はサブフォルダごとに台帳を割らない ────────────────
    #[test]
    fn 非gitのサブフォルダは同じルートに寄る() {
        let root = unique_temp_dir("zaivern-lease-test", "nogit");
        let deep = root.join("a/b/c");
        std::fs::create_dir_all(&deep).unwrap();
        // まだ台帳が無い = 「推測」なので `git` は false。
        let r = roots_of(&deep);
        assert!(!r.rooted, "git でも既存の台帳でもないので推測と判る");
        // ルートに台帳があれば、配下は**そこへ寄る** (別々に生えない)。
        let want = canonical_best_effort(&root);
        let ((key, _), rooted) = roots_raw_with(&deep, &|p| p == want);
        assert!(rooted, "既存の台帳があるので確定扱い");
        assert_eq!(key, want, "配下は既存の台帳のあるルートへ寄る");
        // 台帳がどこにも無ければ、3 つの深さが**同じ答え**にはならないが、
        // `git == false` で呼び出し元が断れる (静かに割らない)。
        for d in [root.as_path(), root.join("a").as_path(), deep.as_path()] {
            assert!(!roots_raw_with(d, &|_| false).1, "推測と判ること: {d:?}");
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn gitがあればrootsはgit判定になる() {
        let root = unique_temp_dir("zaivern-lease-test", "withgit");
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let deep = root.join("src/x");
        std::fs::create_dir_all(&deep).unwrap();
        let r = roots_of(&deep);
        assert!(r.rooted, ".git があれば推測ではない");
        assert_eq!(r.key, canonical_best_effort(&root));
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 自分の域は、追従しても手放さない ──────────────────────────────
    #[test]
    fn 自分の域は取り直しで移動しても元の場所を手放さない() {
        // **これが「2 人が同じ行を書く」の原因だった。**
        // 持ち主が自分の行を書き換えると自分の錨が合わなくなり、
        // `region::resolve` が**似た行**へ吸い寄せられて域が「移動」する。
        // 移動した瞬間、元の行が空いたことになって他人が取れてしまう。
        //
        // 実測 (`tools/anyrepo-prove.sh --repo hyperframes --writers 8`,
        // 種 20260818): `README.md#L21` を持つ書き手が同じファイルの別の域を
        // 確保し直した瞬間に L21 → L15 へ移動し、21 行目を 2 人が書いた。
        //
        // 似た行だらけのテキストを使って、その状況をそのまま作る。
        let same_line = "";
        let text: String = (1..=40)
            .map(|i| {
                if i == 15 || i == 21 {
                    format!("{same_line}\n")
                } else {
                    format!("unique line {i}\n")
                }
            })
            .collect();
        let arc: std::sync::Arc<str> = std::sync::Arc::from(text.clone());
        let mut pats = vec!["a.md#L21".to_string()];
        // 錨は「21 行目の中身」= 他の行と見分けが付かない中身。
        let mut ancs = vec![crate::region::capture_anchor(
            &text,
            &crate::region::Span { start: 21, end: 21 },
        )];
        // 同じファイルの**別の**域を確保し直す。
        let want = Want {
            spec: "a.md#L5".to_string(),
            anchor: crate::region::capture_anchor(&text, &crate::region::Span { start: 5, end: 5 }),
            text: Some(arc),
        };
        absorb(&mut pats, &mut ancs, &want);
        // **21 行目を覆っている域が必ず残っていること。**
        let covers21 = pats.iter().any(|p| {
            spec_span(p).is_some_and(|s| {
                s.start <= 21 && (s.end == crate::region::Span::EOF || s.end >= 21)
            })
        });
        assert!(
            covers21,
            "自分の域が 21 行目を手放した (他人が同じ行を取れてしまう): {pats:?}"
        );
    }

    // ── 置き去りロックの奪取は原子的でなければならない ────────────────
    #[test]
    fn 奪取は張りたての別のロックを消さない() {
        // **これが「二重配布」と「取りこぼし」の単一原因だった。**
        // 奪取が `remove_file` + `create_new` の 2 手だったころは、
        //   1. P2 が消す → P2 が張る (P2 が握った)
        //   2. P3 が消す ← **P2 の張りたてが消える** → P3 も張る
        // と進んで 2 人が同時に臨界区間へ入り、後の書き戻しが先の予約を
        // 消していた。**時計に依存しない形**で、その 2 手目を固定する。
        let dir = unique_temp_dir("zaivern-lease-test", "steal-fresh");
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("s.json");
        let lock = store.with_extension("lock");

        // P2 と P3 がどちらも「古いロック old」を観測した状況を作る。
        std::fs::write(&lock, "old").unwrap();
        // P2 が奪う (正当) 。
        steal_stale_lock(&lock, "old");
        assert!(!lock.exists(), "正当な奪取は置き去りを外す");
        // P2 が張り直した = **生きている新しい持ち主**。
        std::fs::write(&lock, "fresh-owner").unwrap();
        // P3 が、古い観測 old のまま遅れて奪いに来る。
        steal_stale_lock(&lock, "old");
        assert!(lock.exists(), "張りたての別のロックを消してはいけない");
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            "fresh-owner",
            "持ち主が入れ替わっていない"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 同じ置き去りを二人が奪っても外れるのは一度だけ() {
        // `rename` は**元が在るときしか成功しない**ので、奪取そのものが
        // 直列化される。同じ観測を持つ 2 人が奪いに行っても、
        // 置き去りが外れるのは 1 度だけで、2 人目は何も壊さない。
        let dir = unique_temp_dir("zaivern-lease-test", "steal-once");
        std::fs::create_dir_all(&dir).unwrap();
        let lock = dir.join("s.lock");
        std::fs::write(&lock, "old").unwrap();
        steal_stale_lock(&lock, "old");
        assert!(!lock.exists(), "1 人目が外す");
        // 2 人目は同じ観測のまま遅れて来る。**何も起きてはいけない。**
        steal_stale_lock(&lock, "old");
        assert!(!lock.exists(), "2 人目は何も壊さない");
        // 新しい持ち主が張ったあとなら、2 人目の遅れた奪取でも消えない。
        std::fs::write(&lock, "fresh").unwrap();
        steal_stale_lock(&lock, "old");
        assert_eq!(
            std::fs::read_to_string(&lock).unwrap(),
            "fresh",
            "新しい持ち主のロックは残る"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 混雑しても同時に握るのは一人だけ() {
        // 置き去りが無い普通の混雑では、誰も奪わないので相互排除は自明に
        // 保たれる。**重なりが起きたら必ず捕まる**形で押さえておく。
        let dir = unique_temp_dir("zaivern-lease-test", "lock-excl");
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("s.json");
        let live = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let worst = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut hs = Vec::new();
        for _ in 0..16 {
            let (store, live, worst) = (store.clone(), live.clone(), worst.clone());
            hs.push(std::thread::spawn(move || {
                use std::sync::atomic::Ordering::SeqCst;
                for _ in 0..8 {
                    if let Ok(g) = acquire_lock_in(&store, LOCK_STALE_MS) {
                        let n = live.fetch_add(1, SeqCst) + 1;
                        worst.fetch_max(n, SeqCst);
                        std::thread::yield_now();
                        live.fetch_sub(1, SeqCst);
                        drop(g);
                    }
                }
            }));
        }
        for h in hs {
            h.join().unwrap();
        }
        assert_eq!(
            worst.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "同時に 2 人が臨界区間へ入った"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 奪われた側は他人のロックを外さない() {
        // 奪取のあと、**奪われた側の `Drop`** が新しい持ち主のロックを
        // 消してしまうと、そこから先はロックが無いのと同じになる。
        let dir = unique_temp_dir("zaivern-lease-test", "steal-drop");
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("s.json");
        let lock = store.with_extension("lock");
        let victim = acquire_lock_in(&store, LOCK_STALE_MS).expect("最初は取れる");
        // 別の持ち主が張り直した状況を作る (中身が変わる = 別の握り)。
        std::fs::write(&lock, "another-owner").unwrap();
        drop(victim);
        assert!(
            lock.exists(),
            "自分のものでないロックを外してはいけない (外すと相互排除が消える)"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 生きているロックは奪わない() {
        let dir = unique_temp_dir("zaivern-lease-test", "steal-live");
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join("s.json");
        let held = acquire_lock_in(&store, LOCK_STALE_MS).expect("取れる");
        // 置き去り閾値 (既定) には遠い = 奪ってはいけない。
        let e = match acquire_lock_in(&store, LOCK_STALE_MS) {
            Err(e) => e,
            Ok(_) => panic!("先客が居るのに 2 人目が握れた = 相互排除が壊れている"),
        };
        assert!(is_lock_busy(&e), "混雑として返すこと: {e}");
        drop(held);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn dead(_: u32) -> bool {
        false
    }
    fn living(_: u32) -> bool {
        true
    }

    fn holder(agent: &str, session: &str) -> Holder {
        Holder {
            agent: agent.into(),
            session: session.into(),
            cwd: format!("/ws/{agent}"),
            pid: 0,
        }
    }

    // ── パス正規化 ────────────────────────────────────────────────

    #[test]
    fn パスは区切りと余分な要素を正規化する() {
        assert_eq!(normalize_path("src\\app.rs"), "src/app.rs");
        assert_eq!(normalize_path("./src//app.rs"), "src/app.rs");
        // **末尾の `/` は「配下ぜんぶ」。** ここを `src` に潰していたため
        // `zai lease claim src/` が 1 件も守らないのに成功を返していた。
        assert_eq!(normalize_path("src/"), "src/**");
        assert_eq!(normalize_path("src/ui/"), "src/ui/**");
        assert_eq!(normalize_path("/"), "");
        // `..` は畳む (確保側と書き込み側で形がずれないように)。
        assert_eq!(normalize_path("src/sub/../mod.rs"), "src/mod.rs");
        assert_eq!(normalize_path("a/b/../../c.rs"), "c.rs");
        // スコープの外へ出る `..` は落とす (外は関知しない)。
        assert_eq!(normalize_path("../../etc/passwd"), "etc/passwd");
        assert_eq!(normalize_path(""), "");
        // 非 ASCII (CJK・空白入り) も壊さない
        assert_eq!(
            normalize_path("ドキュメント/設計 メモ.md"),
            "ドキュメント/設計 メモ.md"
        );
        assert_eq!(
            normalize_path(".\\日本語\\ファイル.rs"),
            "日本語/ファイル.rs"
        );
        // 大小非区別のファイルシステムが既定の OS (Windows / macOS) では畳む。
        // Linux は畳まない — **両側を書く**。
        // macOS を入れていなかったため、開発機で `Foo.rs` と `foo.rs` が
        // 別リースになり同じ物理ファイルへ 2 人が書けていた。
        if cfg!(any(windows, target_os = "macos")) {
            assert_eq!(normalize_path("SRC/App.rs"), "src/app.rs");
        } else {
            assert_eq!(normalize_path("SRC/App.rs"), "SRC/App.rs");
        }
    }

    // ── glob の境界 (ここを間違えると全部狂う) ─────────────────────

    #[test]
    fn パターンが具体パスを覆う条件() {
        let table: &[(&str, &str, bool)] = &[
            ("src/app.rs", "src/app.rs", true),
            ("src/app.rs", "src/other.rs", false),
            ("src/**", "src/app.rs", true),
            ("src/**", "src/a/b/c.rs", true),
            ("src/**", "src", true),
            ("src/**", "tests/a.rs", false),
            ("src/", "src/a.rs", true), // 末尾 / はサブツリー
            ("src", "src/a.rs", false), // ディレクトリ名だけでは配下を含まない
            ("src/*.rs", "src/a.rs", true),
            ("src/*.rs", "src/sub/a.rs", false), // * は / を越えない
            ("**/*.rs", "src/a.rs", true),
            ("**/*.rs", "a.rs", true),
            ("**", "何でも/日本語.rs", true),
            ("src/?.rs", "src/a.rs", true),
            ("src/?.rs", "src/ab.rs", false),
            ("ドキュメント/**", "ドキュメント/設計 メモ.md", true),
        ];
        for (pat, path, want) in table {
            assert_eq!(covers(pat, path), *want, "covers({pat:?}, {path:?})");
        }
    }

    #[test]
    fn パターン同士の重なり判定() {
        let table: &[(&str, &str, bool)] = &[
            ("src/**", "src/a.rs", true),
            ("src/a.rs", "src/a.rs", true),
            ("src/a.rs", "src/b.rs", false),
            // ファイルとその親ディレクトリ
            ("src/", "src/a.rs", true),
            ("src/**", "src/sub/", true),
            // 兄弟は重ならない
            ("src/auth/**", "src/ui/**", false),
            ("src/auth/**", "src/auth/x/y.rs", true),
            // ワイルドカード同士
            ("src/*.rs", "src/a*", true),
            ("src/*.rs", "src/*.md", false),
            ("**/*.rs", "src/**", true),
            ("**", "何でも", true),
            ("a/**/z.rs", "a/b/c/z.rs", true),
            ("a/**/z.rs", "a/b/c/y.rs", false),
            // ** は 0 個のセグメントにも当たる
            ("a/**", "a", true),
            ("a/**/b", "a/b", true),
            // CJK
            ("ドキュメント/**", "ドキュメント/設計.md", true),
            ("ドキュメント/**", "資料/設計.md", false),
        ];
        for (a, b, want) in table {
            assert_eq!(overlaps(a, b), *want, "overlaps({a:?}, {b:?})");
            assert_eq!(overlaps(b, a), *want, "対称でない: ({a:?}, {b:?})");
        }
    }

    #[test]
    fn ワイルドカードが並んでも爆発しない() {
        // 素朴な再帰なら指数になる形。DP なので即返る。
        let a = "*a*a*a*a*a*a*a*a*a*a*";
        let b = "*b*b*b*b*b*b*b*b*b*b*";
        let t0 = Instant::now();
        assert!(overlaps(a, b), "どちらも任意文字列に当たるので重なる");
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "{:?}",
            t0.elapsed()
        );
    }

    #[test]
    fn 覆う関係なら重なる_無ワイルドカードでは一致する() {
        for (pat, path) in [
            ("src/**", "src/a.rs"),
            ("src/a.rs", "src/a.rs"),
            ("**/*.rs", "x/y/z.rs"),
        ] {
            assert!(covers(pat, path));
            assert!(overlaps(pat, path), "覆うなら必ず重なる");
        }
        assert!(!covers("src/a.rs", "src/b.rs"));
        assert!(!overlaps("src/a.rs", "src/b.rs"));
    }

    // ── スコープ ─────────────────────────────────────────────────

    #[test]
    fn worktree_のポインタから元のリポジトリへ寄せる() {
        let got = main_repo_root_from_pointer(
            "gitdir: /repos/proj/.git/worktrees/feat-a\n",
            Path::new("/wt"),
        );
        assert_eq!(got, Some(PathBuf::from("/repos/proj")));
        // Windows 形式
        let got = main_repo_root_from_pointer("gitdir: C:/r/p/.git/worktrees/w1", Path::new("/wt"));
        assert_eq!(got, Some(PathBuf::from("C:/r/p")));
        // 形が違えば推測しない
        assert_eq!(
            main_repo_root_from_pointer("gitdir: /repos/proj/.git", Path::new("/wt")),
            None
        );
        assert_eq!(
            main_repo_root_from_pointer("これは違う", Path::new("/wt")),
            None
        );
    }

    #[test]
    fn git_の無いフォルダでもスコープが決まる() {
        let dir = unique_temp_dir("zaivern", "lease-nogit");
        let sub = dir.join("a/b");
        std::fs::create_dir_all(&sub).expect("mkdir");
        // .git が無ければその場所自身。パニックしない
        let r = roots_of(&sub);
        let want = sub.canonicalize().unwrap_or_else(|_| sub.clone());
        assert_eq!(r.key, want);
        assert_eq!(r.tree, want, "git 管理外では 2 つのルートが一致する");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn worktree_は元のリポジトリと同じ台帳を引く() {
        let base = unique_temp_dir("zaivern", "lease-worktree");
        let main = base.join("proj");
        std::fs::create_dir_all(main.join(".git/worktrees/w1")).expect("mkdir");
        std::fs::create_dir_all(main.join("src")).expect("mkdir");
        let wt = base.join("wt-1/src");
        std::fs::create_dir_all(&wt).expect("mkdir");
        std::fs::write(
            base.join("wt-1/.git"),
            format!("gitdir: {}/.git/worktrees/w1\n", main.display()),
        )
        .expect("write");
        // **ここが競合との差**: worktree でも台帳のキーは元のリポジトリへ寄る
        let a = roots_of(&main.join("src"));
        let b = roots_of(&wt);
        assert_eq!(a.key, b.key, "worktree が別スコープになると衝突を防げない");
        let dir = base.join("leases");
        assert_eq!(store_path_in(&dir, &a.key), store_path_in(&dir, &b.key));
        // **相対化は作業ツリー基準**。ここを key 基準にすると worktree の
        // ファイルが 1 つも当たらず、機能が無言で死ぬ (実際に踏んだ)。
        assert_ne!(a.tree, b.tree, "作業ツリーは別物のはず");
        assert_eq!(
            rel_within(&b.tree, &wt.join("a.rs")),
            Some("src/a.rs".to_string()),
            "worktree のファイルは worktree 基準で相対化する"
        );
        assert_eq!(
            rel_within(&a.key, &wt.join("a.rs")),
            None,
            "元リポジトリ基準では当たらない (これが e2e で踏んだ穴)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn スコープ外のパスは関知しない() {
        let dir = unique_temp_dir("zaivern", "lease-outside");
        let scope = dir.join("ws");
        std::fs::create_dir_all(scope.join("src")).expect("mkdir");
        std::fs::create_dir_all(dir.join("other")).expect("mkdir");
        assert_eq!(
            rel_within(&scope, &scope.join("src")),
            Some("src".to_string())
        );
        assert_eq!(rel_within(&scope, &dir.join("other")), None);
        assert_eq!(rel_within(&scope, &scope), None, "スコープ自身は対象外");
        // **まだ無いファイル** (Write で新規作成される側) も相対化できること。
        // ここが外れると新規ファイルの衝突を 1 件も止められない。
        assert_eq!(
            rel_within(&scope, &scope.join("src/まだ無い.rs")),
            Some("src/まだ無い.rs".to_string())
        );
        assert_eq!(
            rel_within(&scope, &scope.join("新しい階層/深い/file.rs")),
            Some("新しい階層/深い/file.rs".to_string())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 確保 (fail-closed) ────────────────────────────────────────

    #[test]
    fn 競合したら片方だけが勝つ() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        let b = holder("B", "s-b");
        assert_eq!(
            try_claim(&mut s, &a, &["src/**".into()], 100, 600, &dead),
            Claim::Granted(1)
        );
        // 重なるものは**取れない** (後勝ちにしない)
        match try_claim(&mut s, &b, &["src/app.rs".into()], 100, 600, &dead) {
            Claim::Refused { owner, pattern, .. } => {
                assert!(owner.contains('A'), "{owner}");
                assert_eq!(pattern, "src/**");
            }
            other => panic!("競合を通してしまった: {other:?}"),
        }
        // 重ならないものは取れる
        assert_eq!(
            try_claim(&mut s, &b, &["docs/**".into()], 100, 600, &dead),
            Claim::Granted(1)
        );
        assert_eq!(s.leases.len(), 2);
    }

    #[test]
    fn 一部でも重なれば一つも取らない() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        let b = holder("B", "s-b");
        try_claim(&mut s, &a, &["src/**".into()], 0, 600, &dead);
        let before = s.clone();
        let r = try_claim(
            &mut s,
            &b,
            &["docs/x.md".into(), "src/a.rs".into()],
            0,
            600,
            &dead,
        );
        assert!(matches!(r, Claim::Refused { .. }));
        assert_eq!(s, before, "部分的に取れてはいけない (全か無か)");
    }

    #[test]
    fn 同じ持ち主なら追加で取れて期限が伸びる() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        try_claim(&mut s, &a, &["src/a.rs".into()], 0, 600, &dead);
        assert_eq!(
            try_claim(
                &mut s,
                &a,
                &["src/a.rs".into(), "src/b.rs".into()],
                300,
                600,
                &dead
            ),
            Claim::Granted(1),
            "既に持っているパターンは数えない"
        );
        assert_eq!(s.leases.len(), 1);
        assert_eq!(s.leases[0].patterns, vec!["src/a.rs", "src/b.rs"]);
        assert_eq!(s.leases[0].expires_at, 900);
    }

    #[test]
    fn セッションidが違えば同じフォルダでも別人() {
        let mut s = Store::default();
        let mut a = holder("claude", "s-1");
        let mut b = holder("claude", "s-2");
        a.cwd = "/ws".into();
        b.cwd = "/ws".into();
        try_claim(&mut s, &a, &["src/a.rs".into()], 0, 600, &dead);
        assert!(matches!(
            try_claim(&mut s, &b, &["src/a.rs".into()], 0, 600, &dead),
            Claim::Refused { .. }
        ));
    }

    // ── 期限と安全な回収 ──────────────────────────────────────────

    #[test]
    fn 期限切れは回収される() {
        let mut s = Store::default();
        try_claim(
            &mut s,
            &holder("A", "s-a"),
            &["src/**".into()],
            0,
            600,
            &dead,
        );
        prune(&mut s, 599, &dead);
        assert_eq!(s.leases.len(), 1, "期限内は残る");
        prune(&mut s, 601, &dead);
        assert!(s.leases.is_empty(), "期限切れは回収する");
    }

    #[test]
    fn 生きている持ち主には猶予があるが上限がある() {
        let mut s = Store::default();
        let h = Holder {
            pid: 4242,
            ..holder("A", "s-a")
        };
        try_claim(&mut s, &h, &["src/**".into()], 0, 600, &living);
        // 期限切れでも、プロセスが生きている間は猶予
        assert!(
            s.leases[0].active(700, &living),
            "戻ってきた本人から奪わない"
        );
        // **上限がある** (PID 再利用で永久に残らない)
        assert!(
            !s.leases[0].active(600 + RECLAIM_GRACE_SECS + 1, &living),
            "猶予に上限が無いと、再利用された PID で永久に人質になる"
        );
        // 死んでいれば猶予なし
        assert!(!s.leases[0].active(601, &dead));
    }

    #[test]
    fn pid_を持たないリースは_ttl_だけで回収する() {
        let mut s = Store::default();
        try_claim(&mut s, &holder("A", "s-a"), &["x".into()], 0, 10, &living);
        assert_eq!(s.leases[0].holder.pid, 0);
        assert!(!s.leases[0].active(11, &living), "PID が無ければ猶予もない");
    }

    // ── 判定 (フックの心臓) ───────────────────────────────────────

    #[test]
    fn 判定表_自分は通り他人は止まり未所有は通る() {
        let mut s = Store::default();
        let a = holder("A", "s-a");
        let b = holder("B", "s-b");
        try_claim(&mut s, &a, &["src/**".into()], 100, 600, &dead);
        assert_eq!(decide(&s, &a, "src/app.rs", 100, &dead), Verdict::Allow);
        let Verdict::Deny(reason) = decide(&s, &b, "src/app.rs", 100, &dead) else {
            panic!("他人の所有を通してしまった");
        };
        // 理由は**行動できる**内容であること
        assert!(reason.contains("src/app.rs"), "{reason}");
        assert!(reason.contains('A'), "誰が持っているかが無い: {reason}");
        assert!(reason.contains("待つ"), "どうすればよいかが無い: {reason}");
        // 未所有は通る
        assert_eq!(decide(&s, &b, "docs/x.md", 100, &dead), Verdict::Allow);
        // 期限切れも通る
        assert_eq!(decide(&s, &b, "src/app.rs", 9_999, &dead), Verdict::Allow);
    }

    #[test]
    fn 壊れた台帳は_fail_open_で許可する() {
        let dir = unique_temp_dir("zaivern", "lease-broken");
        let store = dir.join("broken.json");
        std::fs::write(&store, "{ これは JSON ではない").expect("write");
        assert!(read_store(&store).is_err(), "壊れているのは検知する");
        // gate は内部エラーで通す (自分のバグでユーザーを止めない)
        let payload = serde_json::json!({
            "session_id": "s-x",
            "cwd": dir.to_string_lossy(),
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": dir.join("a.rs").to_string_lossy() },
        })
        .to_string();
        // 台帳の場所は workspace_key 由来なので、この壊れたファイルとは別。
        // ここでは「読めない入力でも panic しない」ことを見る。
        assert_eq!(gate("claude", "PreToolUse", &payload).exit, 0);
        assert_eq!(
            gate("claude", "PreToolUse", "これは JSON ではない"),
            pass_answer()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 台帳の入出力 ─────────────────────────────────────────────

    #[test]
    fn 台帳は書いて読み戻せる() {
        let dir = unique_temp_dir("zaivern", "lease-io");
        let store = dir.join("s.json");
        assert!(!enabled(&store));
        enable(&store).expect("有効化");
        assert!(enabled(&store));
        with_store(&store, |s| {
            try_claim(s, &holder("A", "s-a"), &["src/**".into()], 5, 600, &dead)
        })
        .expect("確保");
        let got = read_store(&store).expect("読める");
        assert_eq!(got.leases.len(), 1);
        assert_eq!(got.leases[0].patterns, vec!["src/**"]);
        assert_eq!(got.leases[0].acquired_at, 5);
        // 解放
        let n = with_store(&store, |s| release(s, &holder("A", "s-a"))).expect("解放");
        assert_eq!(n, 1);
        assert!(read_store(&store).expect("読める").leases.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ロックは同時に一つしか取れない() {
        let dir = unique_temp_dir("zaivern", "lease-lock");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let g = acquire_lock(&store).expect("1 つ目は取れる");
        let t0 = Instant::now();
        assert!(acquire_lock(&store).is_err(), "2 つ目が取れてはいけない");
        assert!(
            t0.elapsed() < Duration::from_millis(LOCK_WAIT_MS + 300),
            "待ち過ぎ: {:?}",
            t0.elapsed()
        );
        drop(g);
        assert!(acquire_lock(&store).is_ok(), "解放後は取れる");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn 置き去りのロックは奪える() {
        let dir = unique_temp_dir("zaivern", "lease-stale-lock");
        let store = dir.join("s.json");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let lock = store.with_extension("lock");
        std::fs::write(&lock, b"").expect("write");
        // mtime を過去へ倒せない環境もあるので、TTL 判定そのものを検証する。
        let old = std::time::SystemTime::now() - Duration::from_millis(LOCK_STALE_MS * 2);
        let ok = filetime_set(&lock, old);
        if ok {
            assert!(acquire_lock(&store).is_ok(), "古いロックは奪える");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// mtime を過去へ倒す。`File::set_times` は Rust 1.75 で安定した
    /// **移植可能な**手段 (libc へ降りない)。使えない環境では `false`。
    fn filetime_set(path: &Path, when: std::time::SystemTime) -> bool {
        let Ok(f) = std::fs::File::options().write(true).open(path) else {
            return false;
        };
        f.set_times(std::fs::FileTimes::new().set_modified(when))
            .is_ok()
    }

    #[test]
    fn 二つのプロセスが競っても片方しか取れない() {
        // 同一プロセス内の 2 スレッドで、実ファイルのロックを取り合う。
        let dir = unique_temp_dir("zaivern", "lease-race");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let (s1, s2) = (store.clone(), store.clone());
        let h1 = std::thread::spawn(move || {
            with_store(&s1, |s| {
                try_claim(s, &holder("A", "s-a"), &["src/**".into()], 0, 600, &dead)
            })
        });
        let h2 = std::thread::spawn(move || {
            with_store(&s2, |s| {
                try_claim(
                    s,
                    &holder("B", "s-b"),
                    &["src/app.rs".into()],
                    0,
                    600,
                    &dead,
                )
            })
        });
        let r1 = h1.join().expect("join");
        let r2 = h2.join().expect("join");
        let granted = [&r1, &r2]
            .iter()
            .filter(|r| matches!(r, Ok(Claim::Granted(_))))
            .count();
        // 少なくとも片方は取れ、台帳の中身は 1 人ぶんだけ (後勝ちが起きない)
        assert!(granted >= 1, "{r1:?} {r2:?}");
        let got = read_store(&store).expect("読める");
        assert_eq!(
            got.leases.len(),
            1,
            "2 人が同時に所有してはいけない: {got:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── フックの応答 ─────────────────────────────────────────────

    #[test]
    fn 拒否の応答はベンダーのスキーマに一致する() {
        let a = deny_answer("claude", "だめ");
        let v: serde_json::Value = serde_json::from_str(&a.stdout).expect("JSON");
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(v["hookSpecificOutput"]["permissionDecisionReason"], "だめ");
        assert_eq!(a.exit, 0, "JSON は exit 0 のときだけ読まれる");
        // 許可では**何も出さない** ("allow" はユーザーの許可設定を飛び越える)
        assert!(pass_answer().stdout.is_empty());
    }

    #[test]
    fn ペイロードから書き込み先を取り出す() {
        let keys = ["file_path", "notebook_path"];
        let v: serde_json::Value =
            serde_json::from_str(r#"{"tool_input":{"file_path":"/a/b.rs"}}"#).expect("JSON");
        assert_eq!(target_path(&v, &keys), "/a/b.rs");
        let v: serde_json::Value =
            serde_json::from_str(r#"{"tool_input":{"notebook_path":"/a/n.ipynb"}}"#).expect("JSON");
        assert_eq!(target_path(&v, &keys), "/a/n.ipynb");
        let v: serde_json::Value =
            serde_json::from_str(r#"{"tool_input":{"edits":[{"file_path":"/a/c.rs"}]}}"#)
                .expect("JSON");
        assert_eq!(target_path(&v, &keys), "/a/c.rs");
        let v: serde_json::Value = serde_json::from_str(r#"{"tool_input":{}}"#).expect("JSON");
        assert_eq!(target_path(&v, &keys), "");
    }

    #[test]
    fn 書き込み以外のツールとイベントは素通しする() {
        let payload = serde_json::json!({
            "hook_event_name": "PreToolUse",
            "tool_name": "Read",
            "tool_input": { "file_path": "/x/y.rs" },
        })
        .to_string();
        assert_eq!(gate("claude", "PreToolUse", &payload), pass_answer());
        assert_eq!(gate("claude", "PostToolUse", &payload), pass_answer());
        // カタログに無いエージェントも素通し
        assert_eq!(gate("未知のCLI", "PreToolUse", &payload), pass_answer());
    }

    #[test]
    fn 台帳が無いワークスペースでは何もしない() {
        let dir = unique_temp_dir("zaivern", "lease-off");
        std::fs::create_dir_all(dir.join("src")).expect("mkdir");
        let payload = serde_json::json!({
            "session_id": "s-1",
            "cwd": dir.to_string_lossy(),
            "hook_event_name": "PreToolUse",
            "tool_name": "Edit",
            "tool_input": { "file_path": dir.join("src/a.rs").to_string_lossy() },
        })
        .to_string();
        assert_eq!(gate("claude", "PreToolUse", &payload), pass_answer());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 事前の重複検出 ───────────────────────────────────────────

    #[test]
    fn 事前に重なりを見つけて分割案を出す() {
        let list = parse_assignments("A: src/**\nB: src/auth/x.rs, docs/**\n# コメント\n");
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].agent, "A");
        assert_eq!(list[1].patterns, vec!["src/auth/x.rs", "docs/**"]);
        let ovs = plan_overlaps(&list);
        assert_eq!(ovs.len(), 1, "{ovs:?}");
        assert_eq!(ovs[0].pattern_a, "src/**");
        let (split, serial) = split_plan(&list);
        assert_eq!(split[0].patterns, vec!["src/**"]);
        assert_eq!(split[1].patterns, vec!["docs/**"], "重なる分は外れる");
        assert_eq!(serial, vec!["src/auth/x.rs"]);
        // 分割後は 1 件も重ならない
        assert!(plan_overlaps(&split).is_empty());
    }

    #[test]
    fn 互いに素な割り当ては警告しない() {
        let list = parse_assignments("A: src/auth/**\nB: src/ui/**\nC: README.md");
        assert!(plan_overlaps(&list).is_empty());
        let (split, serial) = split_plan(&list);
        assert_eq!(split, list, "重なりが無ければ何も削らない");
        assert!(serial.is_empty());
    }

    // ── 段 ──────────────────────────────────────────────────────

    /// 段の色が全テーマの背景に対して読める (WCAG AA 大文字 = 3.0 以上)。
    ///
    /// `Tier::Enforced` だけは egui の `Visuals` に対応する色が無いので
    /// 明暗 2 通りを直接持っている。**持つなら検算する**。
    #[test]
    fn 段の色はどのテーマでも読める() {
        for t in crate::theme::all() {
            let mut v = if t.dark {
                egui::Visuals::dark()
            } else {
                egui::Visuals::light()
            };
            v.warn_fg_color = t.warn;
            let c = Tier::Enforced.color(&v);
            let ratio = crate::theme::contrast_ratio(c, t.panel);
            assert!(
                ratio >= 3.0,
                "{}: 強制の色 {c:?} が背景 {:?} に対して {ratio:.2} しかない",
                t.name,
                t.panel
            );
        }
    }

    #[test]
    fn 段は正直に出す() {
        assert_eq!(tier(true, true), Tier::Enforced);
        assert_eq!(tier(true, false), Tier::Advisory);
        assert_eq!(tier(false, true), Tier::Off);
        assert_eq!(tier(false, false), Tier::Off);
        for t in [Tier::Enforced, Tier::Advisory, Tier::Off] {
            assert!(!t.label().is_empty());
            assert!(!t.detail().is_empty());
        }
    }

    // ── レイアウト (極端な寸法) ───────────────────────────────────

    fn area(w: f32, h: f32) -> egui::Rect {
        egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, h))
    }

    #[test]
    fn 行のレイアウトはどの幅でも収まり重ならない() {
        for (w, h) in [
            (900.0f32, 700.0f32),
            (1200.0, 300.0),
            (320.0, 240.0),
            (120.0, 60.0),
        ] {
            for longest in [40.0f32, 120.0, 600.0] {
                let row = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(w, 20.0));
                let lay = row_layout(row, longest);
                let rects = [lay.owner, lay.patterns, lay.left, lay.actions];
                for (i, r) in rects.iter().enumerate() {
                    assert!(r.width() >= 0.0, "{w}x{h} 列 {i} の幅が負");
                    assert!(
                        r.left() >= row.left() - 0.01 && r.right() <= row.right() + 0.01,
                        "{w}x{h} 列 {i} が領域外: {r:?} / {row:?}"
                    );
                }
                for i in 1..rects.len() {
                    assert!(
                        rects[i].left() >= rects[i - 1].right() - 0.01,
                        "{w}x{h} 列 {i} が前の列と重なる: {:?} {:?}",
                        rects[i - 1],
                        rects[i]
                    );
                }
            }
        }
    }

    #[test]
    fn 狭いときはボタンがアイコンだけになる() {
        assert!(is_compact(400.0));
        assert!(!is_compact(900.0));
        // 1200x300 は横に広いので縮退しない (縦の狭さは列幅に効かない)
        assert!(!is_compact(1200.0));
    }

    #[test]
    fn 空状態のカードは中央に一枚で領域内に収まる() {
        for (w, h) in [(900.0f32, 700.0f32), (1200.0, 300.0), (200.0, 100.0)] {
            let a = area(w, h);
            let c = empty_card(a);
            assert!(a.contains_rect(c), "{w}x{h}: カードがはみ出す {c:?}");
            assert!(
                (c.center().x - a.center().x).abs() < 0.01
                    && (c.center().y - a.center().y).abs() < 0.01,
                "{w}x{h}: 中央に置くこと"
            );
        }
    }

    #[test]
    fn 長い文字列は省略してホバーへ回す() {
        assert_eq!(ellipsize("abc", 5), "abc");
        assert_eq!(ellipsize("abcdefg", 5), "abcd…");
        // 文字単位で切る (CJK でバイト境界を割らない)
        assert_eq!(ellipsize("日本語のながい名前", 4), "日本語…");
    }
}

#[cfg(test)]
mod guard_tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let d = crate::test_util::unique_temp_dir("lease-guard", tag);
        std::fs::create_dir_all(&d).expect("一時フォルダを作れない");
        d
    }

    /// 生きている扱い / 死んでいる扱いの偽 PID 判定。
    fn alive_all(_: u32) -> bool {
        true
    }
    fn alive_none(_: u32) -> bool {
        false
    }

    fn holder(name: &str, cwd: &str) -> Holder {
        Holder {
            agent: name.to_string(),
            // **worktree ごとに別セッション**にする。ここを同じにすると
            // `Holder::same` が「同じ持ち主」と見なして素通りする。
            session: format!("sess-{name}"),
            cwd: normalize_path(cwd),
            pid: 4242,
        }
    }

    /// **この機能の心臓**: 2 つの worktree が同じファイルを編集しようとしたら、
    /// 2 人目は所有を取れない。
    #[test]
    fn 別のworktreeが同じファイルを編集しようとすると所有が取れない() {
        let dir = tmp("two-trees");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;

        let got_a = claim_one(&store, &a, "src/app.rs", now, &alive_all);
        assert!(
            matches!(got_a, Own::Mine { .. }),
            "先に来た A が取れていない: {got_a:?}"
        );

        let got_b = claim_one(&store, &b, "src/app.rs", now, &alive_all);
        match got_b {
            Own::Taken { owner, reason } => {
                assert!(owner.contains('A'), "持ち主の名前が出ていない: {owner}");
                assert!(
                    reason.contains("src/app.rs"),
                    "拒否理由にパスが出ていない: {reason}"
                );
            }
            other => panic!("B が取れてしまった (衝突が起きる): {other:?}"),
        }
    }

    /// 別々のファイルなら 2 人とも取れる (過剰に締めない)。
    #[test]
    fn 別のファイルなら二人とも所有を取れる() {
        let dir = tmp("disjoint");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        assert!(matches!(
            claim_one(&store, &a, "src/app.rs", now, &alive_all),
            Own::Mine { .. }
        ));
        assert!(matches!(
            claim_one(&store, &b, "src/config.rs", now, &alive_all),
            Own::Mine { .. }
        ));
    }

    /// **保存直前の門は fail-closed。** 他人が持っていたら書かせない。
    #[test]
    fn 保存直前の判定は他人の所有を拒否する() {
        let dir = tmp("check-deny");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/app.rs", now, &alive_all);
        match check_one(&store, &b, "src/app.rs", now, &alive_all) {
            Verdict::Deny(m) => assert!(m.contains("src/app.rs"), "理由が薄い: {m}"),
            Verdict::Allow => panic!("他人のファイルへの保存が通ってしまった"),
        }
        // 持ち主自身は当然通る。
        assert_eq!(
            check_one(&store, &a, "src/app.rs", now, &alive_all),
            Verdict::Allow
        );
    }

    /// **台帳が「在るのに読めない」ときは止める (fail-closed)。**
    ///
    /// 以前はここを許可に倒していたが、それは**台帳を壊すだけで誰でも
    /// ガードを丸ごと無効化できる**ということだった (敵対的検証で 7 通り
    /// 破られた)。判断材料が無いなら書かせない。ただし**戻し方を文面に
    /// 必ず書く** — 出口の無い拒否は、ユーザーが機能を切って終わる。
    #[test]
    fn 読めない台帳では保存を止めて戻し方を示す() {
        let dir = tmp("broken");
        let store = dir.join("store.json");
        std::fs::write(&store, "これは JSON ではない {{{").expect("write");
        let a = holder("A", "/repo/.wt/a");
        match check_one(&store, &a, "src/app.rs", 1_000, &alive_all) {
            Verdict::Deny(m) => {
                assert!(
                    m.contains("lease"),
                    "戻し方 (コマンド) が書かれていない: {m}"
                );
            }
            Verdict::Allow => panic!("読めない台帳で保存が通ってしまった"),
        }
    }

    /// **台帳が無いだけなら止めない。** 「無効」と「壊れている」は別物で、
    /// ここを混ぜると使っていない人の保存まで止まる。
    #[test]
    fn 台帳が無いだけなら保存は止めない() {
        let dir = tmp("absent");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        assert_eq!(
            check_one(&store, &a, "src/app.rs", 1_000, &alive_all),
            Verdict::Allow,
            "台帳が無いだけで保存を止めてはいけない"
        );
    }

    /// 解放したら次の担当がすぐ取れる (待たせた時間がそのまま損害になる)。
    #[test]
    fn 解放すると次の担当が取れる() {
        let dir = tmp("handover");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/app.rs", now, &alive_all);
        release_one(&store, &a, "src/app.rs").expect("解放できない");
        assert!(
            matches!(
                claim_one(&store, &b, "src/app.rs", now, &alive_all),
                Own::Mine { .. }
            ),
            "解放したのに次が取れない"
        );
    }

    /// 死んだ担当のリースは期限で回収される (リポジトリを人質に取らせない)。
    #[test]
    fn 期限切れで死んだ担当の所有は回収される() {
        let dir = tmp("expire");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/app.rs", now, &alive_all);
        // TTL を過ぎ、かつ持ち主のプロセスも死んでいる。
        let later = now + DEFAULT_TTL_SECS + 1;
        assert!(
            matches!(
                claim_one(&store, &b, "src/app.rs", later, &alive_none),
                Own::Mine { .. }
            ),
            "期限切れ + プロセス死亡でも回収されない"
        );
    }

    /// glob で受け持った担当は、その配下の具体ファイルも守る。
    #[test]
    fn globで受け持つと配下の具体ファイルも守られる() {
        let dir = tmp("glob");
        let store = dir.join("store.json");
        let a = holder("A", "/repo/.wt/a");
        let b = holder("B", "/repo/.wt/b");
        let now = 1_000;
        claim_one(&store, &a, "src/ui/**", now, &alive_all);
        assert!(
            matches!(
                check_one(&store, &b, "src/ui/panel.rs", now, &alive_all),
                Verdict::Deny(_)
            ),
            "glob の配下が守られていない"
        );
    }

    /// **危険がないときは自動で有効化しない** (単独利用者に払わせない)。
    #[test]
    fn worktreeが無いリポジトリでは自動で有効化しない() {
        let dir = tmp("solo");
        std::fs::create_dir_all(dir.join(".git")).expect("mkdir .git");
        let roots = roots_of(&dir);
        assert!(!risky(&roots), "単独リポジトリを危険と判定している");
    }

    /// worktree がぶら下がっていれば危険 = 自動で有効化してよい。
    #[test]
    fn worktreeがぶら下がっていれば危険と判定する() {
        let dir = tmp("has-wt");
        std::fs::create_dir_all(dir.join(".git").join("worktrees").join("w1"))
            .expect("mkdir worktrees");
        let roots = roots_of(&dir);
        assert!(risky(&roots), "worktree があるのに危険と判定していない");
    }

    /// 自分が linked worktree の中にいれば危険。
    ///
    /// **`key` と `tree` が別物になる**のがこの状況の目印で、ここを
    /// 取り違えると機能が丸ごと無言で効かなくなる。
    #[test]
    fn linked_worktreeの中にいれば危険と判定する() {
        let base = tmp("linked");
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("w1")).expect("mkdir");
        let wt = base.join("w1");
        std::fs::create_dir_all(&wt).expect("mkdir wt");
        // linked worktree の `.git` は元リポジトリを指すファイル。
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git").join("worktrees").join("w1").display()
            ),
        )
        .expect("write pointer");
        let roots = roots_of(&wt);
        assert_ne!(roots.key, roots.tree, "key と tree が分かれていない");
        assert!(risky(&roots), "linked worktree を危険と判定していない");
    }
}

/// **端から端まで**の検査 — エディタの保存経路が本当に止まるか。
///
/// ここだけはシングルトン (`guard()`) を触るので、`mod guard_tests` の
/// 並列実行と干渉しないよう**このモジュール内で直列化**し、後始末で必ず
/// [`disarm`] する。実 `~/.zaivern` には触れない ([`arm_in`] に一時フォルダを渡す)。
#[cfg(test)]
mod guard_e2e_tests {
    use super::*;

    /// シングルトンを触るテストどうしを直列化する。
    fn serial() -> std::sync::MutexGuard<'static, ()> {
        static M: OnceLock<Mutex<()>> = OnceLock::new();
        M.get_or_init(|| Mutex::new(()))
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// linked worktree を 1 本持つリポジトリを作り、その worktree の場所を返す。
    fn repo_with_worktree(tag: &str) -> (PathBuf, PathBuf) {
        let base = crate::test_util::unique_temp_dir("lease-e2e", tag);
        let main = base.join("main");
        std::fs::create_dir_all(main.join(".git").join("worktrees").join("w1"))
            .expect("mkdir main");
        let wt = base.join("w1");
        std::fs::create_dir_all(wt.join("src")).expect("mkdir wt");
        std::fs::write(
            wt.join(".git"),
            format!(
                "gitdir: {}\n",
                main.join(".git").join("worktrees").join("w1").display()
            ),
        )
        .expect("write pointer");
        (base, wt)
    }

    /// **この機能の本番の主張**: 別の担当が持っているファイルは、
    /// エディタの保存直前の門で止まる。
    #[test]
    fn 他人が持つファイルはエディタの保存門で止まる() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("deny");
        let dir = base.join("ledger");

        // worktree がぶら下がっているので、開いた時点で自動で有効になる。
        assert!(
            arm_in(&dir, &wt, true, 30),
            "worktree があるのに有効化されない"
        );

        // 別の担当 (別ワークツリーのエディタ) が先に確保する。
        let roots = roots_of(&wt);
        let store = store_path_in(&dir, &roots.key);
        let other = Holder {
            agent: "別の担当".into(),
            session: "sess-other".into(),
            cwd: normalize_path("/somewhere/else"),
            pid: 4242,
        };
        assert!(matches!(
            claim_one(&store, &other, "src/app.rs", now_secs(), &|_| true),
            Own::Mine { .. }
        ));

        // エディタが保存しようとする → 止まる。
        let target = wt.join("src").join("app.rs");
        match check_write(&target) {
            Verdict::Deny(m) => {
                assert!(m.contains("別の担当"), "誰が持っているか出ていない: {m}");
                assert!(m.contains("src/app.rs"), "どのファイルか出ていない: {m}");
            }
            Verdict::Allow => panic!("他人が持つファイルへの保存が通ってしまった"),
        }

        // 自分が持っているファイルは当然通る。
        let mine = wt.join("src").join("mine.rs");
        assert_eq!(check_write(&mine), Verdict::Allow, "空きファイルが通らない");

        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// **`lease.auto_arm` を切っている人には勝手に入らない。**
    /// 設定が効かないなら、その設定は無いほうがよい。
    #[test]
    fn 自動有効化を切っていればworktreeがあっても有効にならない() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("no-auto");
        let dir = base.join("ledger");
        assert!(
            !arm_in(&dir, &wt, false, 30),
            "auto_arm を切っているのに有効化された"
        );
        assert!(!armed());
        assert_eq!(
            check_write(&wt.join("src").join("app.rs")),
            Verdict::Allow,
            "切っているのに判断している"
        );
        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 寿命の設定が実際にリースへ載る (極端な値でも壊れない)。
    #[test]
    fn 寿命の設定は畳まれてリースに載る() {
        assert_eq!(ttl_from_minutes(30), 30 * 60);
        // 0 や負数でも「即失効」にはしない (取った瞬間に他人へ奪われる)。
        assert_eq!(ttl_from_minutes(0), 60);
        assert_eq!(ttl_from_minutes(-5), 60);
        // 上限は 24 時間。放置された担当がリポジトリを永久に握らない。
        assert_eq!(ttl_from_minutes(i64::MAX), 24 * 60 * 60);
    }

    /// **フォルダごとの移動 / 削除は、配下の所有者に阻まれる。**
    ///
    /// `check_write` は 1 パスしか見ないので、`src/` を消す操作は
    /// `src/app.rs` の持ち主がいても素通りしていた。消すのは戻せないので、
    /// 上書きより強く守る必要がある。
    #[test]
    fn フォルダの削除は配下の所有者に阻まれる() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("tree");
        let dir = base.join("ledger");
        assert!(arm_in(&dir, &wt, true, 30));

        let roots = roots_of(&wt);
        let store = store_path_in(&dir, &roots.key);
        let other = Holder {
            agent: "別の担当".into(),
            session: "sess-other".into(),
            cwd: normalize_path("/somewhere/else"),
            pid: 4242,
        };
        claim_one(&store, &other, "src/app.rs", now_secs(), &|_| true);

        // 配下を持たれているフォルダは動かせない / 消せない。
        match check_tree(&wt.join("src")) {
            Verdict::Deny(m) => assert!(m.contains("別の担当"), "持ち主が出ていない: {m}"),
            Verdict::Allow => panic!("配下に所有があるフォルダの操作が通ってしまった"),
        }
        // 無関係なフォルダは通る (過剰に止めない)。
        assert_eq!(check_tree(&wt.join("docs")), Verdict::Allow);
        // ファイル単体なら check_write と同じ結果。
        assert!(matches!(
            check_tree(&wt.join("src").join("app.rs")),
            Verdict::Deny(_)
        ));

        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 自分が持っているフォルダは自分で動かせる (自分に阻まれない)。
    #[test]
    fn 自分が持つフォルダは自分で動かせる() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("tree-mine");
        let dir = base.join("ledger");
        assert!(arm_in(&dir, &wt, true, 30));
        // ガードが握っている持ち主そのもので確保する。
        let roots = roots_of(&wt);
        let store = store_path_in(&dir, &roots.key);
        let me = editor_holder(&roots.tree);
        claim_one(&store, &me, "src/app.rs", now_secs(), &|_| true);
        assert_eq!(check_tree(&wt.join("src")), Verdict::Allow);
        disarm();
        let _ = std::fs::remove_dir_all(&base);
    }

    /// ガードを降ろしたら 1 件も判断しない (単独利用者のコストはゼロ)。
    #[test]
    fn 降ろした後は何も判断しない() {
        let _s = serial();
        let (base, wt) = repo_with_worktree("off");
        let dir = base.join("ledger");
        arm_in(&dir, &wt, true, 30);
        disarm();
        assert!(!armed());
        assert_eq!(
            check_write(&wt.join("src").join("app.rs")),
            Verdict::Allow,
            "降ろしたのに判断している"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}

#[cfg(test)]
mod scale_regression_tests {
    use super::*;

    /// **部分木の確保が実際に配下を守る。**
    /// 人が担当表を書くときに最も自然な `src/` が no-op だった
    /// (しかも「確保しました」と返っていた) ので、番人を置く。
    #[test]
    fn 末尾スラッシュの確保は配下を守る() {
        let pat = normalize_path("src/");
        assert!(covers(&pat, &normalize_path("src/app.rs")));
        assert!(covers(&pat, &normalize_path("src/ui/panel.rs")));
        assert!(!covers(&pat, &normalize_path("tests/app.rs")));
        // 事前の重なり検出でも同じ形で効く。
        assert!(overlaps(&pat, &normalize_path("src/app.rs")));
    }

    /// `..` を含む指定でも、実際に書かれるパスと一致する。
    #[test]
    fn 相対のドット二つを含む確保も実パスに当たる() {
        let pat = normalize_path("src/sub/../mod.rs");
        assert_eq!(pat, normalize_path("src/mod.rs"));
        assert!(covers(&pat, &normalize_path("src/mod.rs")));
    }

    /// **`**` が並んでも判定が爆発しない。**
    /// 素の再帰では `**` 8 個で 1 件の判定に 35 秒かかっていた
    /// (= 書き込みの臨界路が丸ごと止まる)。
    #[test]
    fn ワイルドカードが多段でも判定が爆発しない() {
        let pat = "**/**/**/**/**/**/**/**/x.rs";
        let path = "a/b/c/d/e/f/g/h/i/j/k/l/y.rs";
        let t0 = Instant::now();
        assert!(!covers(pat, path), "当たらないはず");
        let dt = t0.elapsed();
        assert!(
            dt < Duration::from_millis(200),
            "多段ワイルドカードで爆発している: {dt:?}"
        );
    }

    /// 混み合って登録できなかったことを、文面ではなく印で見分ける。
    #[test]
    fn 混雑と破損を区別できる() {
        assert!(is_lock_busy(&format!("{LOCK_BUSY}混んでいます")));
        assert!(!is_lock_busy("台帳が壊れています: expected value"));
    }
}

#[cfg(test)]
mod os_rule_tests {
    use super::*;

    /// **3 つの OS の規則を、どのホストからでも固定する。**
    ///
    /// `cfg!` 分岐のままだと、macOS で開発している限り Windows / Linux の
    /// 規則は一度も実行されない。実際に「macOS だけ大小を畳んでいなかった」
    /// ために、開発機で `Foo.rs` と `foo.rs` が別リースになっていた。
    #[test]
    fn 三つのosの正規化規則を表で固定する() {
        // (win_sep, fold_case, 入力, 期待)
        let table: &[(bool, bool, &str, &str)] = &[
            // Windows: 区切りを畳み、大小も畳む
            (true, true, "SRC\\App.rs", "src/app.rs"),
            (true, true, "src\\ui\\", "src/ui/**"),
            // macOS: 同上 (APFS は大小非区別)
            (true, true, "SRC/App.rs", "src/app.rs"),
            // Linux: 大小は畳まない
            (true, false, "SRC/App.rs", "SRC/App.rs"),
            (true, false, "src/ui/", "src/ui/**"),
            // 区切りを畳まない設定 (参考: unix の `\` を名前の一部として扱う)
            (false, false, "src/a\\b.rs", "src/a\\b.rs"),
            // `..` の畳み込みは規則に依らない
            (true, false, "src/sub/../mod.rs", "src/mod.rs"),
            (true, true, "src/sub/../MOD.rs", "src/mod.rs"),
        ];
        for (win_sep, fold, input, want) in table {
            assert_eq!(
                normalize_path_on(input, *win_sep, *fold),
                *want,
                "win_sep={win_sep} fold_case={fold} 入力={input:?}"
            );
        }
    }

    /// 既定は動いている OS の規則を選ぶ (公開シグネチャは据え置き)。
    #[test]
    fn 既定の規則は動いているosに一致する() {
        let want = normalize_path_on("SRC/App.rs", true, cfg!(any(windows, target_os = "macos")));
        assert_eq!(normalize_path("SRC/App.rs"), want);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  行域リースのテスト
//
//  **`gate` を丸ごと通す e2e はここに置けない。** `gate` は
//  `store_dir()` = 実 `~/.zaivern/leases` を引くので、テストから走らせると
//  ユーザーの本物の台帳を書き換えてしまう (CLAUDE.md の「実 `~/.zaivern` に
//  触れない」)。そこで `gate` が使う判断 (`touched_of` → `decide_spans` /
//  `owns_touched` → `try_claim`) を**同じ順序で**直接呼んで固定する。
//  `gate` 自身の素通し経路は `tests` 側の既存テストが押さえている。
// ═══════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod span_tests {
    use super::*;
    use crate::region::{Span, SAFE_BAND};
    use crate::test_util::unique_temp_dir;

    /// `gate` が組み立てているのとまったく同じ「実ファイル → 触れた行域」。
    /// 本番では読んだテキストを錨の取り直しにも使い回すので 2 段になっている。
    fn touched_of(input: &serde_json::Value, abs: &Path) -> Option<Vec<Span>> {
        let FileRead::Text(body) = read_capped_ex(abs, abs.parent().unwrap_or(abs)) else {
            return None;
        };
        touched_in(&body, input)
    }

    fn dead(_: u32) -> bool {
        false
    }

    fn who(agent: &str) -> Holder {
        Holder {
            agent: agent.to_string(),
            session: format!("s-{agent}"),
            cwd: String::new(),
            pid: 0,
        }
    }

    /// **壁が潤沢な作業ツリー** (全行が一意の 800 行のファイルを何本か置く)。
    ///
    /// 門は `crate::region::needs_wall` なので、**本文を読めないファイルでは
    /// 同じファイルの 2 人目を必ず断る** (fail-closed)。行域の配り方そのものを
    /// 測るテストは壁の有無を測っているのではないので、ここを起点にする。
    /// 中身は読むだけなので、テストプロセスで 1 つ作れば足りる。
    fn walled_tree() -> &'static Path {
        static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
        DIR.get_or_init(|| {
            let dir = unique_temp_dir("zaivern", "lease-span-tree");
            std::fs::create_dir_all(dir.join("src")).expect("src を作る");
            let body: String = (1..=800u32).map(|i| format!("line {i}\n")).collect();
            for rel in [
                "a.rs",
                "b.rs",
                "shared.rs",
                "src/a.rs",
                "src/b.rs",
                "src/app.rs",
            ] {
                std::fs::write(dir.join(rel), &body).expect("本文を置く");
            }
            dir
        })
        .as_path()
    }

    /// このモジュールの `try_claim` は [`walled_tree`] を基準にする。
    ///
    /// 素の `super::try_claim` は `default_tree()` (= プロセスの cwd) を見るので、
    /// 本文が読めず **同じファイルの 2 人目が全部 fail-closed で断られる**。
    /// 壁そのものを測るテストは `try_claim_in` を直に呼ぶこと。
    fn try_claim(
        store: &mut Store,
        holder: &Holder,
        patterns: &[String],
        now: u64,
        ttl: u64,
        alive: &dyn Fn(u32) -> bool,
    ) -> Claim {
        try_claim_in(walled_tree(), store, holder, patterns, now, ttl, alive)
    }

    // ── 1. 台帳のスキーマは変えずに行域を載せる ─────────────────────

    /// **古い台帳 (`#L` を含まない) はファイル全体の域として読める。**
    /// 版を上げていないので、これが壊れると既存ユーザーの台帳が全部死ぬ。
    #[test]
    fn 古い台帳は全体の域として読める() {
        let dir = unique_temp_dir("zaivern", "lease-span-old");
        let store = dir.join("s.json");
        // 行域が入る前の版が書いた形をそのまま置く
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(
            &store,
            r#"{"leases":[{"holder":{"agent":"A","session":"s-A","cwd":"","pid":0},
                 "patterns":["src/**","README.md"],"acquired_at":1,"expires_at":9999,"note":""}]}"#,
        )
        .expect("write");
        let st = read_store(&store).expect("読める");
        let l = &st.leases[0];
        assert_eq!(
            l.owned_spans("src/a.rs", None),
            None,
            "行域なし = ファイル全体"
        );
        assert!(l.covers_path("src/a.rs"));
        assert!(l.touches("src/a.rs", &[Span { start: 1, end: 2 }], None));
        // 他人はどの行でも止まる
        assert!(matches!(
            decide_spans(
                &st,
                &who("B"),
                "src/a.rs",
                &[Span::line(500)],
                None,
                100,
                &dead
            ),
            Verdict::Deny(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 行域つきの確保は**そのままの書式**で台帳へ載り、読み戻せる。
    #[test]
    fn 行域は台帳へそのまま載って読み戻せる() {
        let dir = unique_temp_dir("zaivern", "lease-span-io");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let got = with_store(&store, |s| {
            try_claim(
                s,
                &who("A"),
                &["src/a.rs#L10-40".into(), "src/b.rs".into()],
                5,
                600,
                &dead,
            )
        })
        .expect("確保");
        assert_eq!(got, Claim::Granted(2));
        let back = read_store(&store).expect("読める");
        assert_eq!(
            back.leases[0].patterns,
            vec![
                normalize_path("src/a.rs") + "#L10-40",
                normalize_path("src/b.rs")
            ],
            "書式が往復で変わっている"
        );
        assert_eq!(
            back.leases[0].owned_spans(&normalize_path("src/a.rs"), None),
            Some(vec![Span { start: 10, end: 40 }])
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 大小を畳む OS でも `#L` の表記は 1 つに保たれる (`#l` にならない)。
    #[test]
    fn 正規化しても行域の表記は一つに保たれる() {
        for raw in ["SRC/A.rs#L10-40", "src\\a.rs#l10-40", " src/a.rs#L10+31 "] {
            let out = normalize_spec(raw);
            assert!(out.ends_with("#L10-40"), "{raw:?} → {out:?}");
            assert_eq!(spec_span(&out), Some(Span { start: 10, end: 40 }));
            assert_eq!(normalize_spec(&out), out, "冪等でない: {out:?}");
        }
        // 末尾まで / 1 行だけ も往復する
        assert_eq!(normalize_spec("a.rs#L7-"), "a.rs#L7-");
        assert_eq!(normalize_spec("a.rs#L7"), "a.rs#L7");
        // 行域が読めない `#` はパスの一部 (壊さない)
        assert_eq!(normalize_spec("a#b.rs"), normalize_path("a#b.rs"));
    }

    // ── 2. covers / covers_span ────────────────────────────────────

    #[test]
    fn 行番号まで見る版と見ない版の表() {
        // (パターン, パス, 触れた行, covers, covers_span)
        let t = &[
            (
                "src/a.rs#L10-40",
                "src/a.rs",
                vec![Span::line(20)],
                true,
                true,
            ),
            (
                "src/a.rs#L10-40",
                "src/a.rs",
                vec![Span::line(100)],
                true,
                false,
            ),
            // 安全帯 (3 行) の内側は「関わる」— git が 1 ハンクに畳むため
            (
                "src/a.rs#L10-40",
                "src/a.rs",
                vec![Span::line(42)],
                true,
                true,
            ),
            (
                "src/a.rs#L10-40",
                "src/a.rs",
                vec![Span::line(44)],
                true,
                false,
            ),
            (
                "src/a.rs#L10-40",
                "src/b.rs",
                vec![Span::line(20)],
                false,
                false,
            ),
            // 行域なし = どの行でも自分の領分
            ("src/a.rs", "src/a.rs", vec![Span::line(999)], true, true),
            ("src/**", "src/a.rs", vec![Span::line(999)], true, true),
            // 触れた行が判らない = 安全側
            ("src/a.rs#L10-40", "src/a.rs", vec![], true, true),
            // 末尾までの域は後ろ全部
            (
                "src/a.rs#L10-",
                "src/a.rs",
                vec![Span::line(9999)],
                true,
                true,
            ),
        ];
        for (pat, path, touched, want_c, want_s) in t {
            assert_eq!(covers(pat, path), *want_c, "covers({pat:?}, {path:?})");
            assert_eq!(
                covers_span(pat, path, touched),
                *want_s,
                "covers_span({pat:?}, {path:?}, {touched:?})"
            );
        }
    }

    /// **行域を無視して「ファイルごと自分のもの」と誤判定しない。**
    ///
    /// `covers` は行番号を知らない呼び出し元のために `true` を返す
    /// (安全側 = 止める向き) が、`owns_touched` は持っていない行を
    /// 「持っている」とは言わない。ここが逆を向くと、10 行だけ確保した
    /// エージェントがファイル全体を書けてしまう。
    #[test]
    fn 行域しか持っていない者はファイル全体を持っていない() {
        let mut s = Store::default();
        let a = who("A");
        try_claim(&mut s, &a, &["src/a.rs#L1-5".into()], 100, 600, &dead);
        assert!(
            owns_touched(&s, &a, "src/a.rs", Some(&[Span::line(3)]), None, 100, &dead),
            "自分の域は自分のもの"
        );
        assert!(
            !owns_touched(
                &s,
                &a,
                "src/a.rs",
                Some(&[Span::line(500)]),
                None,
                100,
                &dead
            ),
            "持っていない行を自分のものと言っている"
        );
        assert!(
            !owns_touched(&s, &a, "src/a.rs", None, None, 100, &dead),
            "行域が判らない (= 全体) のに通してしまっている"
        );
        // ファイル全体を持っていれば、行域が判らなくても自分のもの
        let mut s2 = Store::default();
        try_claim(&mut s2, &a, &["src/a.rs".into()], 100, 600, &dead);
        assert!(owns_touched(&s2, &a, "src/a.rs", None, None, 100, &dead));
    }

    // ── 3. 確保 — 同じファイルでも違う行なら 2 人が持てる ──────────

    #[test]
    fn 同じファイルでも安全帯ぶん離れていれば二人が持てる() {
        let mut s = Store::default();
        let (a, b) = (who("A"), who("B"));
        assert_eq!(
            try_claim(&mut s, &a, &["src/a.rs#L1-20".into()], 100, 600, &dead),
            Claim::Granted(1)
        );
        assert_eq!(
            try_claim(&mut s, &b, &["src/a.rs#L40-60".into()], 100, 600, &dead),
            Claim::Granted(1),
            "離れた行域が取れていない = 並列度がファイル数で頭打ちになる"
        );
        assert_eq!(s.leases.len(), 2);
        // 互いの域には入れない
        assert!(matches!(
            decide_spans(&s, &b, "src/a.rs", &[Span::line(10)], None, 100, &dead),
            Verdict::Deny(_)
        ));
        assert_eq!(
            decide_spans(&s, &b, "src/a.rs", &[Span::line(50)], None, 100, &dead),
            Verdict::Allow
        );
    }

    #[test]
    fn 安全帯より近い行域は片方しか取れない() {
        let mut s = Store::default();
        let (a, b) = (who("A"), who("B"));
        try_claim(&mut s, &a, &["src/a.rs#L1-20".into()], 100, 600, &dead);
        // 21 行目は間に 0 行しか無い → git が 1 ハンクに畳む
        let got = try_claim(&mut s, &b, &["src/a.rs#L21-30".into()], 100, 600, &dead);
        assert!(matches!(got, Claim::Refused { .. }), "{got:?}");
        // 全体を持とうとしても取れない
        assert!(matches!(
            try_claim(&mut s, &b, &["src/a.rs".into()], 100, 600, &dead),
            Claim::Refused { .. }
        ));
        // 安全帯ちょうど (間に 3 行) なら取れる
        assert_eq!(
            try_claim(&mut s, &b, &["src/a.rs#L24-30".into()], 100, 600, &dead),
            Claim::Granted(1)
        );
        assert_eq!(SAFE_BAND, 3, "安全帯を変えたらこの表も測り直すこと");
    }

    /// 一部でも重なれば 1 つも取らない — **行域でも全か無かを壊さない**。
    #[test]
    fn 行域でも一部が重なれば一つも取らない() {
        let mut s = Store::default();
        let (a, b) = (who("A"), who("B"));
        try_claim(&mut s, &a, &["src/a.rs#L1-20".into()], 100, 600, &dead);
        let got = try_claim(
            &mut s,
            &b,
            &["src/b.rs#L1-20".into(), "src/a.rs#L5-9".into()],
            100,
            600,
            &dead,
        );
        assert!(matches!(got, Claim::Refused { .. }), "{got:?}");
        assert!(
            s.leases.iter().all(|l| !l.holder.same(&b)),
            "部分的に取れてしまっている: {s:?}"
        );
    }

    /// 近接した行域は畳まれる = 台帳が書き込みのたびに伸びない。
    #[test]
    fn 近接した行域は畳まれて台帳が伸びない() {
        let mut s = Store::default();
        let a = who("A");
        for start in [1u32, 5, 9, 13] {
            try_claim(
                &mut s,
                &a,
                &[format!("src/a.rs#L{start}-{}", start + 2)],
                100,
                600,
                &dead,
            );
        }
        assert_eq!(
            s.leases[0].patterns,
            vec!["src/a.rs#L1-15"],
            "隣り合う域が畳まれていない"
        );
        // 離れた域は別のまま
        try_claim(&mut s, &a, &["src/a.rs#L100-110".into()], 100, 600, &dead);
        assert_eq!(
            s.leases[0].patterns,
            vec!["src/a.rs#L1-15", "src/a.rs#L100-110"]
        );
        // 間を埋める域が来たら 1 本になる
        try_claim(&mut s, &a, &["src/a.rs#L16-99".into()], 100, 600, &dead);
        assert_eq!(s.leases[0].patterns, vec!["src/a.rs#L1-110"]);
        // ファイル全体を取ったら行域は畳まれて消える
        try_claim(&mut s, &a, &["src/a.rs".into()], 100, 600, &dead);
        assert_eq!(s.leases[0].patterns, vec!["src/a.rs"]);
        // 既に全体を持っているなら行域を足しても増えない
        assert_eq!(
            try_claim(&mut s, &a, &["src/a.rs#L3-4".into()], 100, 600, &dead),
            Claim::Granted(0)
        );
        assert_eq!(s.leases[0].patterns, vec!["src/a.rs"]);
    }

    /// ファイルを解放したら、その行域リースも一緒に落ちる。
    #[test]
    fn ファイルを解放すると行域も落ちる() {
        let dir = unique_temp_dir("zaivern", "lease-span-rel");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let a = who("A");
        with_store(&store, |s| {
            try_claim(
                s,
                &a,
                &[
                    "src/a.rs#L1-5".into(),
                    "src/a.rs#L100-110".into(),
                    "src/b.rs".into(),
                ],
                5,
                600,
                &dead,
            )
        })
        .expect("確保");
        release_one(&store, &a, &normalize_path("src/a.rs")).expect("解放");
        assert_eq!(
            read_store(&store).expect("読める").leases[0].patterns,
            vec![normalize_path("src/b.rs")],
            "ファイルを手放したのに行域が残っている"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 3b. 後退の回帰テスト (実リポジトリで再現された欠陥) ──────────
    //
    //  出荷済み 0.13.0 では **`#L` を付けた瞬間に保護が消えていた**。
    //  `normalize_path` が仕様文字列を丸ごと畳むので `a.rs#L1-10` が
    //  `a.rs#l1-10` という**別のパス**として台帳へ並び、重なり判定に
    //  一度も掛からなかった。新機能の欠落ではなく**既存の保証の後退**なので、
    //  不変条件を表にして固定する。

    /// **行域を指定しても保護は消えない。** (実測で再現された後退の回帰)
    #[test]
    fn 行域を指定しても保護は消えない() {
        // (先に取る側, 後から取る側, 後から取れてよいか)
        let table: &[(&str, &str, bool)] = &[
            // 1. ファイル全体の保持者が居たら、あらゆる行域は拒否される
            ("a.rs", "a.rs#L1-10", false),
            ("a.rs", "a.rs#L500-600", false),
            ("a.rs", "a.rs#L1-", false),
            ("a.rs", "a.rs#L7", false),
            // 2. 逆向き — 行域の保持者が居たら、ファイル全体は拒否される
            ("a.rs#L1-10", "a.rs", false),
            // 3. 行域どうしは安全帯を挟んで離れているときだけ両方通る
            ("a.rs#L1-10", "a.rs#L5-15", false),  // 重なっている
            ("a.rs#L1-10", "a.rs#L11-20", false), // 隣接 (間 0 行)
            ("a.rs#L1-10", "a.rs#L13-20", false), // 間 2 行 < 安全帯 3
            ("a.rs#L1-10", "a.rs#L14-20", true),  // 間 3 行 = 安全帯
            ("a.rs#L14-20", "a.rs#L1-10", true),  // 順序を変えても同じ
            // 4. 別ファイルは行域があってもいつでも通る
            ("a.rs#L1-10", "b.rs#L1-10", true),
            ("a.rs", "b.rs#L1-10", true),
            // 5. glob は行域があっても安全側 (= 衝突扱い)
            ("src/**", "src/a.rs#L1-10", false),
            ("src/a.rs#L1-10", "src/**", false),
        ];
        for (first, second, want_ok) in table {
            let mut s = Store::default();
            let (a, b) = (who("A"), who("B"));
            assert_eq!(
                try_claim(&mut s, &a, &[(*first).into()], 100, 600, &dead),
                Claim::Granted(1),
                "先に取る側が取れていない: {first:?}"
            );
            let got = try_claim(&mut s, &b, &[(*second).into()], 100, 600, &dead);
            let ok = matches!(got, Claim::Granted(_));
            assert_eq!(
                ok, *want_ok,
                "{first:?} を持つ人が居るとき {second:?} → {got:?}"
            );
            // `overlaps` は対称でなければならない (確保の順で答えが変わらない)
            assert_eq!(
                overlaps(&normalize_spec(first), &normalize_spec(second)),
                overlaps(&normalize_spec(second), &normalize_spec(first)),
                "重なり判定が対称でない: {first:?} / {second:?}"
            );
        }
    }

    /// **正規化は `#L` の手前だけに掛かる。** ここが今回の後退の原因。
    ///
    /// フラグメントまで畳むと `a.rs#L1-10` が `a.rs#l1-10` になり、
    /// パスとして別物になって判定が外れる。
    #[test]
    fn 正規化は行域の手前だけに掛かる() {
        for (win_sep, fold) in [(true, true), (true, false), (false, false)] {
            // パス部分だけが規則を受け、フラグメントは `#L…` のまま
            let path = normalize_path_on("SRC\\A.rs", win_sep, fold);
            let spec = normalize_spec_on("SRC\\A.rs#L10-40", win_sep, fold);
            assert_eq!(
                spec,
                format!("{path}#L10-40"),
                "win_sep={win_sep} fold={fold}"
            );
            // 小文字の `l` で来ても表記は `#L` に揃う
            assert_eq!(
                normalize_spec_on("SRC\\A.rs#l10-40", win_sep, fold),
                format!("{path}#L10-40")
            );
            // そして**同じファイルとして**重なりが検出される
            assert!(
                overlaps(&path, &spec),
                "全体と行域が重ならないと判定された: {path:?} / {spec:?}"
            );
        }
    }

    /// 断る代わりに「ずらす」提案を返す (交渉層が呼ぶ純関数)。
    #[test]
    fn 通らない行域には近くの空きを提案する() {
        let mut s = Store::default();
        try_claim(&mut s, &who("A"), &["a.rs#L10-20".into()], 100, 600, &dead);
        let want = crate::region::parse("a.rs#L15-19").expect("parse");
        let got = suggest_alternative(&s, &want, None).expect("提案が無い");
        // 長さは変えない (5 行欲しいなら 5 行を返す)
        assert_eq!(got.span.expect("span").len(), 5);
        // 提案された場所は本当に空いている
        assert!(!crate::region::conflicts(
            &got,
            &crate::region::parse("a.rs#L10-20").expect("parse"),
            SAFE_BAND
        ));
        // A の直後がいちばん近い (24 行目から)
        assert_eq!(crate::region::render(&got), "a.rs#L24-28");

        // 決定的: 何度呼んでも同じ
        for _ in 0..8 {
            assert_eq!(suggest_alternative(&s, &want, None), Some(got.clone()));
        }
        // 空いているならずらす必要が無い
        assert_eq!(
            suggest_alternative(
                &s,
                &crate::region::parse("a.rs#L100-104").expect("parse"),
                None
            ),
            None
        );
        // 手前が空いていて近ければそちらを選ぶ
        let mut s2 = Store::default();
        try_claim(&mut s2, &who("A"), &["a.rs#L50-60".into()], 100, 600, &dead);
        let near_front = crate::region::parse("a.rs#L48-52").expect("parse");
        let got2 = suggest_alternative(&s2, &near_front, None).expect("提案が無い");
        assert_eq!(crate::region::render(&got2), "a.rs#L42-46");
        // ずらしようが無いものは正直に None
        for spec in ["a.rs", "a.rs#L5-", "src/*.rs#L1-5"] {
            let mut s3 = Store::default();
            try_claim(&mut s3, &who("A"), &["a.rs".into()], 100, 600, &dead);
            assert_eq!(
                suggest_alternative(&s3, &crate::region::parse(spec).expect("parse"), None),
                None,
                "{spec:?}"
            );
        }
    }

    // ── 4. 関門 — 書き込み後の中身から触れた行域を出す ──────────────

    #[test]
    fn 書き込み後の中身をペイロードから作れる() {
        let old = "1\n2\n3\n4\n5\n";
        // Write = 全文置換
        let w = serde_json::json!({ "content": "x\ny\n" });
        assert_eq!(applied_text(old, &w).as_deref(), Some("x\ny\n"));
        // Edit = 1 回置換
        let e = serde_json::json!({ "old_string": "3", "new_string": "three" });
        assert_eq!(
            applied_text(old, &e).as_deref(),
            Some("1\n2\nthree\n4\n5\n")
        );
        // MultiEdit = 連続適用
        let m = serde_json::json!({ "edits": [
            { "old_string": "1", "new_string": "one" },
            { "old_string": "5", "new_string": "five" },
        ]});
        assert_eq!(
            applied_text(old, &m).as_deref(),
            Some("one\n2\n3\n4\nfive\n")
        );
        // replace_all
        let r =
            serde_json::json!({ "old_string": "\n", "new_string": "\n\n", "replace_all": true });
        assert_eq!(applied_text("a\nb\n", &r).as_deref(), Some("a\n\nb\n\n"));
        // 当たらない置換は **推測しない**
        assert_eq!(
            applied_text(
                old,
                &serde_json::json!({ "old_string": "そんな行は無い", "new_string": "x" })
            ),
            None
        );
        // 形が判らない (Bash など) = 判らないと言う
        assert_eq!(
            applied_text(old, &serde_json::json!({ "command": "sed -i s/a/b/ f" })),
            None
        );
    }

    /// 触れた行域は、実ファイルとペイロードから出る。
    /// **読めない / 大きすぎる / 形が判らない ものは `None` = ファイル全体扱い。**
    #[test]
    fn 触れた行域を出せないものはファイル全体として扱う() {
        let dir = unique_temp_dir("zaivern", "lease-span-touch");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let f = dir.join("a.rs");
        let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&f, &body).expect("write");

        // 100 行目だけを書き換える → 触れた域は 100 行目の周り
        let e = serde_json::json!({ "old_string": "line 100", "new_string": "line 100 変更" });
        let got = touched_of(&e, &f).expect("行域が出る");
        assert_eq!(got, vec![Span::line(100)], "{got:?}");

        // 無いファイル = 前の中身が無い → 判らない
        assert_eq!(touched_of(&e, &dir.join("そんなファイルは無い.rs")), None);
        // シェル経由 (中身が判らない) → 判らない
        assert_eq!(
            touched_of(
                &serde_json::json!({ "command": "sed -i '' s/a/b/ a.rs" }),
                &f
            ),
            None
        );
        // 上限より大きいファイル → 読まない
        let big = dir.join("big.rs");
        std::fs::write(&big, vec![b'x'; (GATE_READ_CAP + 1) as usize]).expect("write");
        assert_eq!(
            touched_of(&serde_json::json!({ "content": "y" }), &big),
            None
        );

        // **全文置換で中身が大きく変わるなら、触れた域が広くなるのが正しい。**
        // 200 行を 2 行にすると「書いた後」の座標では 1〜2 行目しか動かないが、
        // 消えた 3〜200 行目こそ他人の領分。両方向を見ないとここが素通りする。
        let w = serde_json::json!({ "content": "まるごと\n別の中身\n" });
        let wide = touched_of(&w, &f).expect("行域が出る");
        assert_eq!(wide, vec![Span { start: 1, end: 200 }], "{wide:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **関門の一連**: 触れた行域 → 他人と衝突するか → 自分の域に収まるか。
    /// `gate` がロックの内外で使う判断そのもの。
    #[test]
    fn 関門は触れた行域だけで通し止めする() {
        let dir = unique_temp_dir("zaivern", "lease-span-gate");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let f = dir.join("a.rs");
        let body: String = (1..=200).map(|n| format!("line {n}\n")).collect();
        std::fs::write(&f, &body).expect("write");
        let rel = "a.rs";

        let mut st = Store::default();
        let (a, b) = (who("A"), who("B"));
        // A が 100 行目付近を確保している
        try_claim(&mut st, &a, &["a.rs#L95-105".into()], 100, 600, &dead);

        // B が 100 行目を書こうとする → 止まる
        let hit = serde_json::json!({ "old_string": "line 100", "new_string": "変更" });
        let t = touched_of(&hit, &f).expect("行域");
        let Verdict::Deny(reason) = decide_spans(&st, &b, rel, &t, None, 100, &dead) else {
            panic!("他人の行域への書き込みを通してしまった");
        };
        assert!(
            reason.contains("a.rs#L100"),
            "どこで止まったかが無い: {reason}"
        );
        assert!(reason.contains('A'), "誰が持っているかが無い: {reason}");
        // **断るだけで終わらせない** — ずらせる先が文面に出る
        assert!(
            reason.contains("ずらす") && reason.contains("a.rs#L"),
            "ずらす提案が出ていない: {reason}"
        );

        // B が 10 行目を書く → 通り、その域だけを自分のものにする
        let far = serde_json::json!({ "old_string": "line 10\n", "new_string": "変更\n" });
        let t2 = touched_of(&far, &f).expect("行域");
        assert_eq!(t2, vec![Span::line(10)]);
        assert_eq!(
            decide_spans(&st, &b, rel, &t2, None, 100, &dead),
            Verdict::Allow
        );
        assert!(!owns_touched(&st, &b, rel, Some(&t2), None, 100, &dead));
        let want: Vec<String> = t2
            .iter()
            .map(|s| format!("{rel}#L{}-{}", s.start, s.end))
            .collect();
        assert_eq!(
            try_claim(&mut st, &b, &want, 100, 600, &dead),
            Claim::Granted(1)
        );
        assert!(owns_touched(&st, &b, rel, Some(&t2), None, 100, &dead));
        // 続けて同じ域を書くならロックすら要らない
        assert!(owns_touched(
            &st,
            &b,
            rel,
            Some(&[Span::line(10)]),
            None,
            100,
            &dead
        ));

        // 全文置換は触れた域が広い → A の域へ掛かるので止まる
        let whole = serde_json::json!({ "content": "全部\n入れ替える\n" });
        let t3 = touched_of(&whole, &f).expect("行域");
        assert!(matches!(
            decide_spans(&st, &b, rel, &t3, None, 100, &dead),
            Verdict::Deny(_)
        ));
        // 行域を決められない書き込み (シェル経由) も同じく止まる
        assert!(matches!(
            decide_spans(&st, &b, rel, &[], None, 100, &dead),
            Verdict::Deny(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 5. 分割・重なり検出 ────────────────────────────────────────

    #[test]
    fn 分割案は同じファイルの離れた行域を両方に残す() {
        let list = parse_assignments("A: src/a.rs#L1-20\nB: src/a.rs#L40-60\nC: src/a.rs#L21-30\n");
        assert_eq!(list.len(), 3);
        let ovs = plan_overlaps(&list);
        // A↔B は離れている / A↔C は近すぎる
        assert_eq!(ovs.len(), 1, "{ovs:?}");
        assert_eq!((ovs[0].a, ovs[0].b), (0, 2));

        let (split, serial) = split_plan(&list);
        assert_eq!(split[0].patterns, vec!["src/a.rs#L1-20"]);
        assert_eq!(
            split[1].patterns,
            vec!["src/a.rs#L40-60"],
            "離れた行域まで外している = 並列度が上がらない"
        );
        assert!(split[2].patterns.is_empty());
        assert_eq!(serial, vec!["src/a.rs#L21-30"]);

        // 決定性: 同じ入力なら何度でも同じ答え
        for _ in 0..8 {
            assert_eq!(split_plan(&list), (split.clone(), serial.clone()));
        }
    }

    // ── 6. 混雑 (busy-deny) ────────────────────────────────────────

    /// **64 体が同時に同じものを確保しに行っても `busy` は出ず、勝者は 1 つ。**
    ///
    /// `docs/conflict-zero.md` が「32 体以上で busy-deny が増える」と記録した
    /// 欠陥の回帰テスト。原因は混雑ではなく**待ち方** ([`LOCK_SPIN_ROUNDS`])。
    /// **Windows の delete pending を取り合いとして扱う。**
    ///
    /// CI の windows-latest が実際にここで落ちた
    /// (`台帳が壊れた: ロックを作れません: Access is denied. (os error 5)`)。
    /// macOS / Linux では 1 度も出ないので、この分岐はテストでしか守れない。
    #[test]
    fn ロックの取り合いはosごとに正しく見分ける() {
        use std::io::{Error, ErrorKind};
        assert!(lock_contended(&Error::new(ErrorKind::AlreadyExists, "x")));
        assert_eq!(
            lock_contended(&Error::new(ErrorKind::PermissionDenied, "x")),
            cfg!(windows),
            "Windows は delete pending を待ちに回す / unix は本物の権限問題として即失敗"
        );
        // 本物の異常は待たない (待っても直らない)
        assert!(!lock_contended(&Error::new(ErrorKind::NotFound, "x")));
        assert!(!lock_contended(&Error::new(ErrorKind::InvalidInput, "x")));
    }

    #[test]
    fn 六十四体が同時に確保してもbusyは出ず勝者は一つ() {
        let dir = unique_temp_dir("zaivern", "lease-busy64");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let n = 64usize;
        let start = std::time::Instant::now();
        let mut hs = Vec::with_capacity(n);
        for i in 0..n {
            let store = store.clone();
            hs.push(std::thread::spawn(move || {
                // **製品と同じ経路で測る。** `zai lease claim` も `gate` も
                // `with_store_retry` を通る。素の `with_store` は内側の
                // 1 回ぶんの primitive で、`busy` はその定義された失敗形
                // なので、そこに busy 0 を求めるのは仕様の取り違えになる
                // (実際、全 4132 件と同時に走らせると素の版は 36 件 busy を
                //  出した — 機械が飽和すれば当然そうなる)。
                with_store_retry(&store, |s| {
                    try_claim(
                        s,
                        &who(&format!("A{i}")),
                        &["shared.rs".into()],
                        100,
                        600,
                        &dead,
                    )
                })
            }));
        }
        let (mut granted, mut refused, mut busy, mut broken) = (0, 0, 0, 0);
        for h in hs {
            match h.join().expect("スレッド") {
                Ok(Claim::Granted(_)) => granted += 1,
                Ok(Claim::Refused { .. }) => refused += 1,
                Err(e) if is_lock_busy(&e) => busy += 1,
                Err(_) => broken += 1,
            }
        }
        let ms = start.elapsed().as_millis();
        assert_eq!(
            busy, 0,
            "混雑して判定できなかった (busy-deny) が {busy} 件 / {ms}ms"
        );
        assert_eq!(broken, 0, "台帳が壊れた");
        assert_eq!(
            granted, 1,
            "勝者が 1 つでない: 取れた {granted} / 断られた {refused}"
        );
        assert_eq!(refused, n - 1);
        assert_eq!(read_store(&store).expect("読める").leases.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **64 体が同じファイルの違う行を同時に取りに行ったら、全員取れる。**
    /// ファイル単位のままなら 1 人しか取れなかった場面。
    ///
    /// ## ここだけ `with_store_retry` を使う理由 (実測)
    /// 上の同じもの争いは **63 人が断られる = 台帳が変わらない**ので、
    /// `with_store` が書き込みを省いて臨界区間が読み取りだけになる。
    /// こちらは 64 人**全員が台帳へ書く**ので臨界区間が数倍長く、
    /// 素の `with_store` (1 回だけ試す) では **64 中 21 件が `busy`** になった。
    /// 本番の入口 (`gate` / `claim_for` / `release_one`) はどれも
    /// [`with_store_retry`] を通るので、テストもそこを通す。
    #[test]
    fn 六十四体が同じファイルの違う行を同時に取れる() {
        let dir = unique_temp_dir("zaivern", "lease-busy64-span");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        let n = 64usize;
        let mut hs = Vec::with_capacity(n);
        for i in 0..n {
            let store = store.clone();
            hs.push(std::thread::spawn(move || {
                // 間を 5 行空ける (安全帯 3 行より広い)
                let s = i as u32 * 10 + 1;
                let pat = format!("shared.rs#L{s}-{}", s + 4);
                with_store_retry(&store, |st| {
                    try_claim(st, &who(&format!("A{i}")), &[pat.clone()], 100, 600, &dead)
                })
            }));
        }
        let (mut granted, mut refused, mut busy) = (0, 0, 0);
        for h in hs {
            match h.join().expect("スレッド") {
                Ok(Claim::Granted(_)) => granted += 1,
                Ok(Claim::Refused { .. }) => refused += 1,
                Err(e) if is_lock_busy(&e) => busy += 1,
                Err(e) => panic!("台帳が壊れた: {e}"),
            }
        }
        assert_eq!(busy, 0, "混雑して判定できなかった (busy-deny) が {busy} 件");
        assert_eq!(
            granted, n,
            "同じファイルの離れた行を同時に持てていない: 取れた {granted} / 断られた {refused}"
        );
        let st = read_store(&store).expect("読める");
        assert_eq!(st.leases.len(), n);
        // **不変条件**: どの 2 人の担当も重ならない
        let regions: Vec<crate::region::Region> = st
            .leases
            .iter()
            .flat_map(|l| l.patterns.iter().map(|p| spec_region(p)))
            .collect();
        assert!(
            crate::region::is_disjoint(&regions, SAFE_BAND),
            "重なった担当が台帳に載った: {:?}",
            crate::region::conflicting_pairs(&regions, SAFE_BAND)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 6. 断らない確保 (`zai lease claim --shift`) ─────────────────
    //
    //  行域が入って「離れた行なら 64 体が 1 ファイルへ同時に書ける」までは
    //  出来ていた。残っていた唯一の弱点が**拒否**で、`tools/coedit-bench.sh`
    //  の crowded 条件は **完了 11 / 拒否 53**。ところが 64 体が要求している
    //  範囲は 934〜1065 行の **132 行**だけで、ファイルは 2000 行 =
    //  **空きが 1868 行**ある。互いに素に置くのに要るのは 573 行なので、
    //  **断られていたのは空きが無いからではなく、誰もずらしていなかったから。**

    /// `tools/coedit-bench.sh` の crowded とまったく同じ担当表。
    ///
    /// 幅 `SAFE_BAND + 3 = 6` 行を stride 2 で `n` 個。
    /// `base = (total - (n-1)*stride - rl) / 2` なので 2000 行 / 64 体なら
    /// **934**、担当 `i` は `[934 + 2(i-1), +5]` で隣同士は必ず重なる。
    fn crowded_plan(n: u32, total: u32) -> Vec<String> {
        let rl = SAFE_BAND + 3;
        let stride = 2u32;
        let base = (total - (n - 1) * stride - rl) / 2;
        (1..=n)
            .map(|i| {
                let s = base + (i - 1) * stride;
                format!("a.rs#L{s}-{}", s + rl - 1)
            })
            .collect()
    }

    /// `total` 行の中身。**行ごとに違う内容**にして錨が一意に決まるようにする。
    fn body(total: u32) -> String {
        (1..=total).map(|i| format!("line {i}\n")).collect()
    }

    /// **書き手を倍にしても成立率が落ちない (出荷経路・設定は既定のまま)。**
    ///
    /// ## この試験が要る理由 (静かな嘘があった)
    ///
    /// 隣の [`tests::crowded_な担当表でも六十四体全部がずらして入る`] は
    /// `max_shift = None` (**上限なし**) で 64/64 を主張していた。ところが
    /// 出荷経路の `zai lease claim --shift` は
    /// [`default_max_shift_in`] = **200 行**を渡す。単体試験が全部緑なのに
    /// `tools/coedit-bench.sh --agents 64 --lines 2000 --layout crowded` は
    /// 6 回とも **51〜54 完了 / 10〜13 拒否**だった。
    /// **上限を外した経路しか測っていなかった。**
    ///
    /// ここは**設定の既定値そのまま**で測り、しかも `n` を倍にしていく。
    /// 固定上限 `m` は `n ≤ 2m/(幅+安全帯)` という体数の天井を作るので、
    /// 倍にすればどこかで必ず成立率が落ちる。[`shift_ceiling`] が混雑ぶんを
    /// 足すので落ちない。
    #[test]
    fn 書き手を倍にしても成立率が落ちない() {
        let dir = unique_temp_dir("zaivern", "lease-shift-scale");
        std::fs::write(dir.join("a.rs"), body(2000)).expect("中身を置く");
        // **出荷経路とまったく同じ既定値**を使う (設定が無ければ 200 行)。
        let cfg = default_max_shift_in(&dir);
        assert!(
            cfg > 0,
            "既定が 0 だと「ずらすな」になり、この試験が空になる"
        );
        let mut rates: Vec<(u32, usize, usize)> = Vec::new();
        for n in [16u32, 32, 64, 128] {
            let plan = crowded_plan(n, 2000);
            let mut store = Store::default();
            let (mut granted, mut refused) = (0usize, 0usize);
            for (i, p) in plan.iter().enumerate() {
                match try_claim_shift_in(
                    &dir,
                    &mut store,
                    &who(&format!("S{n}_{i}")),
                    std::slice::from_ref(p),
                    100,
                    600,
                    &dead,
                    Some(cfg), // ← ここが `None` だと何も守れない
                ) {
                    ShiftClaim::Granted(_) => granted += 1,
                    ShiftClaim::Refused { .. } => refused += 1,
                }
            }
            rates.push((n, granted, refused));
            assert_eq!(
                refused, 0,
                "n={n} 体・上限 {cfg} 行で {refused} 件を断った (完了 {granted})"
            );
            // 台帳の中身も互いに素であること (通した代わりに重ねていない)
            let regions: Vec<crate::region::Region> = store
                .leases
                .iter()
                .flat_map(|l| l.patterns.iter().map(|p| spec_region(p)))
                .collect();
            assert_eq!(regions.len(), n as usize, "畳まれて減っている");
            assert!(
                crate::region::is_disjoint(&regions, SAFE_BAND),
                "n={n} で重なった担当が台帳に載った"
            );
            for r in &regions {
                let s = r.span.expect("行域");
                assert!(s.start >= 1 && s.end <= 2000, "ファイルの外: {s:?}");
            }
        }
        eprintln!("出荷既定 {cfg} 行での (体数, 完了, 拒否): {rates:?}");
    }

    /// **上限に当たったときの文面に、渡すべき数がそのまま出る。**
    #[test]
    fn 上限に当たった文面は渡すべき数まで出す() {
        let dir = unique_temp_dir("zaivern", "lease-shift-msg");
        std::fs::write(dir.join("a.rs"), body(400)).expect("中身を置く");
        let mut store = Store::default();
        // 先客が 1〜300 行 → 要求 L10-15 は 300 行超えないと入らない。
        assert!(matches!(
            try_claim_shift_in(
                &dir,
                &mut store,
                &who("A"),
                &["a.rs#L1-300".to_string()],
                100,
                600,
                &dead,
                Some(10),
            ),
            ShiftClaim::Granted(_)
        ));
        match try_claim_shift_in(
            &dir,
            &mut store,
            &who("B"),
            &["a.rs#L10-15".to_string()],
            100,
            600,
            &dead,
            Some(10),
        ) {
            ShiftClaim::Refused { owner, .. } => {
                eprintln!("文面: {owner}");
                assert!(owner.contains("294"), "必要な距離が出ていない: {owner}");
                assert!(owner.contains("--max-shift"), "渡し方が無い: {owner}");
                assert!(owner.contains(KEY_MAX_SHIFT), "設定キーが無い: {owner}");
                // **`cli::refusal` の文型判定を通る形か。** `:` が無いと
                // 「…通ります が持っています」になる (実バイナリで出た)。
                assert!(
                    owner.contains(':'),
                    "見出しの区切りが無い = 持ち主名として流し込まれる: {owner}"
                );
            }
            other => panic!("上限を超えたのに通った: {other:?}"),
        }
    }

    // ── 交錯 (帯だけでは足りない唯一の形) ───────────────────────────

    /// 周期 `p` の反復本文を `n` 行 (`region::tests::periodic` と同じ作り)。
    fn 反復本文(p: usize, n: u32) -> String {
        const POOL: [&str; 6] = ["```", "code line", "```", "", "---", ""];
        let mut s: String = (0..n)
            .map(|i| format!("{}\n", POOL[(i as usize) % p]))
            .collect();
        s.push_str("tail\n");
        s
    }

    /// `lines` の各行の末尾へ印を足す (1 行ぶんの置換を複数箇所に置く)。
    fn 触る(base: &str, lines: &[u32], tag: &str) -> String {
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

    /// `git merge-tree --write-tree` の答え。`Some(true)` = 衝突。
    ///
    /// **`git merge-file` では測らない。** あちらは `XDL_MERGE_ZEALOUS_ALNUM`
    /// + myers で `git merge` より寛容なので、実際には失敗する組を clean と
    /// 答える (これで測定が狂っていた実績がある)。
    fn 実gitの答え(dir: &Path, base: &str, ours: &str, theirs: &str) -> Option<bool> {
        let repo = dir.join("merge-lab");
        let _ = std::fs::remove_dir_all(&repo);
        std::fs::create_dir_all(&repo).ok()?;
        let run = |args: &[&str]| -> bool {
            std::process::Command::new("git")
                .current_dir(&repo)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", repo.join("no-such-gitconfig"))
                .env("GIT_TERMINAL_PROMPT", "0")
                .args([
                    "-c",
                    "core.autocrlf=false",
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
        let f = repo.join("a.md");
        std::fs::write(&f, base).ok()?;
        run(&["add", "-A"]);
        run(&["commit", "-qm", "base"]);
        run(&["checkout", "-qb", "ours"]);
        std::fs::write(&f, ours).ok()?;
        run(&["commit", "-qam", "ours"]);
        run(&["checkout", "-q", "-"]);
        run(&["checkout", "-qb", "theirs"]);
        std::fs::write(&f, theirs).ok()?;
        run(&["commit", "-qam", "theirs"]);
        let out = std::process::Command::new("git")
            .current_dir(&repo)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", repo.join("no-such-gitconfig"))
            .args(["merge-tree", "--write-tree", "ours", "theirs"])
            .output()
            .ok()?;
        match out.status.code() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None, // 引数を知らない古い git — 判断しない
        }
    }

    /// **確保の関所が交錯を止める。** ここが赤くなったら、`region` が直した
    /// 判定 (`interleaved` / `interleave_safe`) が出荷経路から外れている。
    ///
    /// 組ごとの帯 (`SAFE_BAND`) は全部満たしているのに、`git merge` は衝突する
    /// 配り方を使う (`region::tests::実gitで周期的な本文では…` と同じ形)。
    #[test]
    fn 交錯した確保は関所が断る() {
        let dir = unique_temp_dir("zaivern", "lease-interleave");
        let body = 反復本文(6, 60);
        std::fs::write(dir.join("a.md"), &body).expect("本文を置く");
        let now = now_secs();
        let mut store = Store::default();
        let b = Holder {
            agent: "B".into(),
            cwd: "/b".into(),
            ..Default::default()
        };
        let a = Holder {
            agent: "A".into(),
            cwd: "/a".into(),
            ..Default::default()
        };
        // B が 13 / 25 行目 (自分どうしなので交錯しない)
        for spec in ["a.md#L13-13", "a.md#L25-25"] {
            assert!(
                matches!(
                    try_claim_in(&dir, &mut store, &b, &[spec.into()], now, 600, &|_| false),
                    Claim::Granted(_)
                ),
                "B の {spec} が取れない"
            );
        }
        // 前提: どの組も帯を満たしている (= 帯だけの判定は「素」と言う)
        for l in [13u32, 25] {
            assert!(
                !crate::region::spans_too_close(
                    &crate::region::Span::line(17),
                    &crate::region::Span::line(l),
                    crate::region::SAFE_BAND
                ),
                "前提が崩れている: 17 と {l} は帯を満たすはず"
            );
        }
        // A が 17 行目 = B を上下から挟む
        let got = try_claim_in(
            &dir,
            &mut store,
            &a,
            &["a.md#L17-17".into()],
            now,
            600,
            &|_| false,
        );
        match got {
            Claim::Refused { owner, .. } => assert!(
                owner.contains("交錯"),
                "交錯として断っていない (帯の文面が出ている): {owner}"
            ),
            other => panic!("交錯を通してしまった: {other:?}"),
        }
        // **離しても直らない。** 0.16.0 まではここで `a.md#L40` が通っていたが、
        // それは「交錯していなければ帯で足りる」という誤った門のせいだった
        // (`region::needs_wall` の実測: 削除・挿入が混ざると上下に分かれた組でも
        // `git merge` は衝突する)。錨が 1 本も無い本文では、離しても通らない。
        let far = try_claim_in(
            &dir,
            &mut store,
            &a,
            &["a.md#L40-40".into()],
            now,
            600,
            &|_| false,
        );
        assert!(
            matches!(far, Claim::Refused { .. }),
            "錨が無い本文で離しただけの域を通してしまった: {far:?}"
        );
        // **壁があれば通る。** 同じ配り方でも、境目に一意な行が 1 本あれば済む
        // — 断っているのは「離れていないから」ではなく「壁が無いから」である。
        let dir2 = unique_temp_dir("zaivern", "lease-interleave-wall");
        let mut walled: Vec<String> = 反復本文(6, 60).lines().map(str::to_string).collect();
        for i in [30usize, 34] {
            walled[i] = format!("UNIQ-{i}");
        }
        std::fs::write(dir2.join("a.md"), walled.join("\n") + "\n").expect("本文を置く");
        let mut store2 = Store::default();
        assert!(
            matches!(
                try_claim_in(
                    &dir2,
                    &mut store2,
                    &b,
                    &["a.md#L25-25".into()],
                    now,
                    600,
                    &|_| false
                ),
                Claim::Granted(_)
            ),
            "B の下ごしらえが取れない"
        );
        let walled_ok = try_claim_in(
            &dir2,
            &mut store2,
            &a,
            &["a.md#L40-40".into()],
            now,
            600,
            &|_| false,
        );
        assert!(
            matches!(walled_ok, Claim::Granted(_)),
            "壁があるのに断った: {walled_ok:?}"
        );
    }

    /// **1 本ずつでも壁は要る** (0.16.0 の screening が空けていた穴)。
    ///
    /// 関所には「どちらも 1 本しか持っていないなら交錯は起こり得ない」と
    /// 降りる近道があった。交錯は確かに起こり得ないが、**壁は要る** —
    /// 削除・挿入が混ざると上下に分かれた組でも `git merge` は衝突する
    /// (`crate::region::needs_wall`)。しかも「2 人が 1 本ずつ」は
    /// **いちばん普通の形**なので、そこだけ素通りしていた。
    #[test]
    fn 一本ずつでも壁が無ければ断る() {
        let dir = unique_temp_dir("zaivern", "lease-wall-single");
        // 錨が 1 本も無い本文 (周期 6・末尾も一意でない)
        const POOL: [&str; 6] = ["```", "code line", "```", "", "---", ""];
        let bare: String = (0..300).map(|i| format!("{}\n", POOL[i % 6])).collect();
        std::fs::write(dir.join("a.md"), &bare).expect("本文を置く");
        let now = now_secs();
        let mut store = Store::default();
        let (a, b) = (who("A"), who("B"));
        assert!(matches!(
            try_claim_in(
                &dir,
                &mut store,
                &a,
                &["a.md#L20-25".into()],
                now,
                600,
                &dead
            ),
            Claim::Granted(_)
        ));
        // **1 本ずつ・200 行離れている**のに、壁が無いので断る
        let got = try_claim_in(
            &dir,
            &mut store,
            &b,
            &["a.md#L220-225".into()],
            now,
            600,
            &dead,
        );
        assert!(
            matches!(got, Claim::Refused { .. }),
            "壁が無いのに 1 本ずつだからと通した: {got:?}"
        );
        // 壁を 1 本植えれば通る (「常に断る」へ倒れていない)
        let dir2 = unique_temp_dir("zaivern", "lease-wall-single-ok");
        let mut walled: Vec<String> = bare.lines().map(str::to_string).collect();
        walled[100] = "UNIQ-100".into();
        std::fs::write(dir2.join("a.md"), walled.join("\n") + "\n").expect("本文を置く");
        let mut store2 = Store::default();
        assert!(matches!(
            try_claim_in(
                &dir2,
                &mut store2,
                &a,
                &["a.md#L20-25".into()],
                now,
                600,
                &dead
            ),
            Claim::Granted(_)
        ));
        let ok = try_claim_in(
            &dir2,
            &mut store2,
            &b,
            &["a.md#L220-225".into()],
            now,
            600,
            &dead,
        );
        assert!(
            matches!(ok, Claim::Granted(_)),
            "壁があるのに断った: {ok:?}"
        );
    }

    /// **実 git が衝突する組を、関所が 1 つも取りこぼさない。**
    ///
    /// 向きは片側だけであることに注意する — `interleave_safe` は錨が無ければ
    /// **分からない側へ倒す**ので、「断った ⇒ 必ず衝突する」は成り立たない
    /// (実際に周期 6・60 行の 17 対 13/25 は綺麗に通るが、関所は断る)。
    /// ここが守るのは**見逃しゼロ**のほうで、断りすぎていないことは
    /// [`一意な本文なら交錯していても通す`] が別に押さえる。
    ///
    /// 時間は 1 秒も測らない (git の速さは環境で 10 倍変わる)。
    #[test]
    fn 実gitが衝突する交錯を関所が取りこぼさない() {
        let dir = unique_temp_dir("zaivern", "lease-interleave-git");
        // `region::tests::実gitで周期的な本文では帯を満たしても衝突する` と
        // 同じ形。周期を変えても穴は残る。
        let table: &[(usize, &[u32], u32)] = &[
            (6, &[5, 13, 25], 17),
            (3, &[5, 13, 25], 17),
            (1, &[5, 13, 25], 17),
            (6, &[3, 15, 22, 60], 44),
        ];
        let mut 衝突した = 0u32;
        for (p, theirs, mine) in table {
            let body = 反復本文(*p, 400);
            std::fs::write(dir.join("a.md"), &body).expect("本文を置く");
            // 前提: どの組も帯を満たしている (= 帯だけの判定は「素」と言う)
            for l in theirs.iter() {
                assert!(
                    !crate::region::spans_too_close(
                        &crate::region::Span::line(*mine),
                        &crate::region::Span::line(*l),
                        crate::region::SAFE_BAND
                    ),
                    "前提が崩れている: {mine} と {l} は帯を満たすはず"
                );
            }
            let ours = 触る(&body, &[*mine], "OURS");
            let th = 触る(&body, theirs, "THEIRS");
            let Some(衝突) = 実gitの答え(&dir, &body, &ours, &th) else {
                eprintln!("git merge-tree --write-tree が使えないので飛ばす");
                return;
            };
            if !衝突 {
                continue;
            }
            衝突した += 1;
            let granted = 挟んで確保できるか(&dir, theirs, *mine);
            assert!(
                !granted,
                "実 git が衝突する組を関所が通した: 周期 {p} / 相手 {theirs:?} / 自分 {mine}"
            );
        }
        assert!(
            衝突した > 0,
            "1 件も衝突しないなら、この穴の前提が変わっている (git の側を測り直すこと)"
        );
    }

    /// **断りすぎていない。** 行がすべて違う本文なら、挟んでいても錨が
    /// 立つので確保できる。ここが赤くなったら、交錯の検査が「常に断る」に
    /// なっている (= 帯の意味まで消している)。
    #[test]
    fn 一意な本文なら交錯していても通す() {
        let dir = unique_temp_dir("zaivern", "lease-interleave-unique");
        let body: String = (1..=60).map(|i| format!("行 {i} は他と違う\n")).collect();
        std::fs::write(dir.join("a.md"), &body).expect("本文を置く");
        assert!(
            挟んで確保できるか(&dir, &[13, 25], 17),
            "一意な本文なのに挟んだ域を断った (厳しすぎる)"
        );
        if let Some(衝突) = 実gitの答え(
            &dir,
            &body,
            &触る(&body, &[17], "OURS"),
            &触る(&body, &[13, 25], "THEIRS"),
        ) {
            assert!(!衝突, "一意な本文なのに実 git が衝突した (前提が変わった)");
        }
    }

    /// B が `theirs` を持っている台帳で、A が `mine` 行を取れるか。
    fn 挟んで確保できるか(dir: &Path, theirs: &[u32], mine: u32) -> bool {
        let now = now_secs();
        let mut store = Store::default();
        let b = Holder {
            agent: "B".into(),
            cwd: "/b".into(),
            ..Default::default()
        };
        let a = Holder {
            agent: "A".into(),
            cwd: "/a".into(),
            ..Default::default()
        };
        for l in theirs {
            let spec = format!("a.md#L{l}-{l}");
            assert!(
                matches!(
                    try_claim_in(dir, &mut store, &b, &[spec], now, 600, &|_| false),
                    Claim::Granted(_)
                ),
                "B の {l} 行目が取れない"
            );
        }
        matches!(
            try_claim_in(
                dir,
                &mut store,
                &a,
                &[format!("a.md#L{mine}-{mine}")],
                now,
                600,
                &|_| false
            ),
            Claim::Granted(_)
        )
    }

    /// **本文が読めないときは fail-closed。**
    ///
    /// 錨は元の本文からしか数えられない。読めないときに「帯だけ」へ落とすと、
    /// いちばん判定が効いてほしい場面 (生成物・巨大なデータファイル) でだけ
    /// 静かに緩む。空の錨を必ず `false` にする `region::interleave_safe` と
    /// 同じ向きであることを固定する。
    #[test]
    fn 本文が読めなければ交錯は通さない() {
        // 実ファイルを置かない = `hydrate_in` が `Want::text` を埋められない
        let dir = unique_temp_dir("zaivern", "lease-interleave-noread");
        let now = now_secs();
        let mut store = Store::default();
        let b = Holder {
            agent: "B".into(),
            cwd: "/b".into(),
            ..Default::default()
        };
        let a = Holder {
            agent: "A".into(),
            cwd: "/a".into(),
            ..Default::default()
        };
        for spec in ["nope.md#L13-13", "nope.md#L25-25"] {
            assert!(matches!(
                try_claim_in(&dir, &mut store, &b, &[spec.into()], now, 600, &|_| false),
                Claim::Granted(_)
            ));
        }
        let got = try_claim_in(
            &dir,
            &mut store,
            &a,
            &["nope.md#L17-17".into()],
            now,
            600,
            &|_| false,
        );
        match got {
            Claim::Refused { owner, .. } => {
                assert!(owner.contains("交錯"), "交錯として断っていない: {owner}");
                // **劣化したことを黙らせない** — 読めなかったと文面に出す
                assert!(
                    owner.contains(KEY_GATE_READ_CAP),
                    "読めなかったことが文面に出ていない: {owner}"
                );
            }
            other => panic!("本文が読めないのに交錯を通した: {other:?}"),
        }
    }

    /// `interleave_ok` は本文が読めなければ**必ず断る** (fail-closed の番人)。
    ///
    /// 0.16.0 まではここが「交錯していない組は本文を読まずに通す」だった。
    /// その門は削除・挿入が混ざると見逃す (`region::needs_wall` に実測: 上下に
    /// 分かれた組でも `git merge` は衝突する) ので、**同じファイルを持つ組は
    /// すべて壁を要求する**へ変えた。費用は「読む回数」で抑える (呼び出し側が
    /// ファイルにつき 1 回だけ読む) のであって、門を緩めて買うものではない。
    #[test]
    fn 本文が読めなければ同じファイルの組は通さない() {
        let s = |a: u32, b: u32| crate::region::Span { start: a, end: b };
        // 上下に分かれていても、本文が無ければ壁を確かめられないので断る
        assert!(!interleave_ok(None, &[s(10, 12)], &[s(30, 32)]));
        assert!(!interleave_ok(None, &[s(30, 32)], &[s(10, 12)]));
        // 挟んでいる — 本文が無ければ断る (従来どおり)
        assert!(!interleave_ok(None, &[s(20, 20)], &[s(10, 10), s(30, 30)]));
        // 片方が空 = 境目が無いので壁は要らない
        assert!(interleave_ok(None, &[], &[s(30, 32)]));
        assert!(interleave_ok(None, &[s(10, 12)], &[]));
        // 壁 (ファイル内で唯一の行) があれば通る
        let text = "a\nb\nc\nd\ne\nf\n";
        assert!(interleave_ok(Some(text), &[s(1, 1)], &[s(5, 5)]));
    }

    /// **交錯の検査は、互いに素な配り方の費用を増やさない。**
    ///
    /// 関所は書き込みのたびに走る短命プロセスなので、ここが太ると全員が
    /// 払う。1 回がファイル全長の走査になる [`Lease::live_span_of`] の
    /// **呼び出し回数**で固定する (時間で線を引かない — 環境で 10 倍動く)。
    ///
    /// 実測 (`zai lease claim` を 64 回・互いに素・2000 行):
    /// 直す前 4421ms / 入れた直後 5082ms (+15%) / screening 後 4325ms。
    /// 回数で見れば理由は自明で、交錯の検査が担当 N 人ぶん本文を
    /// 走査し直していた。
    ///
    /// **0.16.0 の screening は撤回した。** 「両方が 1 本ずつなら交錯は
    /// 起こり得ない」は正しいが、**交錯していなくても壁は要る**
    /// (`crate::region::needs_wall` に実測)。飛ばしてよいのは「相手が
    /// そのパスを 1 本も持っていない」ときだけ。費用は代わりに
    /// **錨をファイルにつき 1 回しか数えない** ことで抑えている
    /// (`interleave_ok_anchors`)。ここが数えているのは
    /// [`Lease::live_span_of`] の回数なので、上限は担当の数のままである。
    #[test]
    fn 互いに素な確保では本文を走査し直さない() {
        let dir = unique_temp_dir("zaivern", "lease-interleave-cost");
        let body: String = (1..=2000)
            .map(|i| format!("fn f{i}() {{ {i} }}\n"))
            .collect();
        std::fs::write(dir.join("a.rs"), &body).expect("本文を置く");
        let now = now_secs();
        let mut store = Store::default();
        // 64 体が互いに素な域を 1 本ずつ持つ (ベンチと同じ形)
        for i in 0..64u32 {
            let h = Holder {
                agent: format!("ag{i}"),
                cwd: format!("/w/{i}"),
                ..Default::default()
            };
            let (s0, e0) = (10 + i * 30, 15 + i * 30);
            let got = try_claim_in(
                &dir,
                &mut store,
                &h,
                &[format!("a.rs#L{s0}-{e0}")],
                now,
                600,
                &|_| false,
            );
            assert!(
                matches!(got, Claim::Granted(_)),
                "{i} 番目が取れない: {got:?}"
            );
        }
        // 65 人目。**他人は全員 1 本ずつなので、交錯の検査は 1 回も
        // 本文を走査してはいけない** (帯の検査ぶんだけが残る)。
        let last = Holder {
            agent: "ag64".into(),
            cwd: "/w/64".into(),
            ..Default::default()
        };
        scan_count::reset();
        let got = try_claim_in(
            &dir,
            &mut store,
            &last,
            &["a.rs#L1930-1935".into()],
            now,
            600,
            &|_| false,
        );
        let scans = scan_count::get();
        assert!(
            matches!(got, Claim::Granted(_)),
            "65 人目が取れない: {got:?}"
        );
        // 帯の検査 (`overlaps_live`) が担当ごとに 1 回。交錯の検査ぶんが
        // 乗っていれば 2 倍になる。
        eprintln!("64 体の台帳へ 1 件確保: 本文の走査 {scans} 回");
        assert!(
            scans <= store.leases.len() as u64,
            "交錯の検査が本文の走査を増やしている: {scans} 回 (上限 {})",
            store.leases.len()
        );
    }

    /// **断る理由は必ず「見出し `:` 本文」の形にする。**
    ///
    /// `cli::refusal` は `:` の有無だけで「持ち主の名前」と「理由」を
    /// 見分けて文型を変える。理由に `:` が無いと
    /// 「`… 通ります` **が持っています**」という意味の通らない 1 行になる。
    /// 新しい理由を足した人が気付けるよう、ソースを読んで固定する。
    #[test]
    fn 断る理由は必ず見出しに区切りを持つ() {
        let src = include_str!("lease.rs").replace("\r\n", "\n");
        // `try_claim_wants_shift` が自分で組み立てる理由 (持ち主名ではない) は
        // これ 1 つ。増えたらここも増やすこと。
        let needle = "\"ずらせる上限に当たりました: ";
        assert!(
            src.contains(needle),
            "ずらし上限の理由が見出し付きでない (探した: {needle})"
        );
    }

    /// **ベンチの crowded 条件で「拒否 0」になることの直接の証拠。**
    ///
    /// 行域だけの実測は 完了 11 / 拒否 53 / 衝突 0。ここが 64 / 0 になる。
    ///
    /// **上限なし (`None`) の経路**を押さえる。出荷経路が渡す既定値
    /// (200 行) での成立率は [`tests::書き手を倍にしても成立率が落ちない`]
    /// が別に測る — ここだけだと「上限を外した経路しか測っていない」という
    /// 静かな嘘になる (実際になっていた)。
    #[test]
    fn crowded_な担当表でも六十四体全部がずらして入る() {
        let dir = unique_temp_dir("zaivern", "lease-shift-crowded");
        std::fs::write(dir.join("a.rs"), body(2000)).expect("中身を置く");
        let plan = crowded_plan(64, 2000);
        assert_eq!(plan[0], "a.rs#L934-939", "ベンチと同じ担当表になっていない");
        assert_eq!(plan[63], "a.rs#L1060-1065");
        let mut store = Store::default();
        let (mut granted, mut refused, mut moved) = (0, 0, 0);
        for (i, p) in plan.iter().enumerate() {
            match try_claim_shift_in(
                &dir,
                &mut store,
                &who(&format!("A{i}")),
                std::slice::from_ref(p),
                100,
                600,
                &dead,
                None,
            ) {
                ShiftClaim::Granted(gs) => {
                    assert_eq!(gs.len(), 1, "1 件の要求に 1 件の結果");
                    granted += 1;
                    if gs[0].moved() {
                        moved += 1;
                    }
                }
                ShiftClaim::Refused { .. } => refused += 1,
            }
        }
        assert_eq!(refused, 0, "空きが 1868 行あるのに {refused} 件を断った");
        assert_eq!(granted, 64, "ずらした {moved} 件");
        assert!(moved >= 60, "ほとんど動いていない: {moved} 件");
        // **不変条件 (1)**: 台帳の担当はどの 2 つも互いに素
        let regions: Vec<crate::region::Region> = store
            .leases
            .iter()
            .flat_map(|l| l.patterns.iter().map(|p| spec_region(p)))
            .collect();
        assert_eq!(regions.len(), 64, "畳まれて減っている");
        assert!(
            crate::region::is_disjoint(&regions, SAFE_BAND),
            "重なった担当が台帳に載った: {:?}",
            crate::region::conflicting_pairs(&regions, SAFE_BAND)
        );
        // **不変条件 (2)**: 使った行は 2000 行に収まっている
        for r in &regions {
            let s = r.span.expect("行域");
            assert!(
                s.start >= 1 && s.end <= 2000,
                "ファイルの外へ出した: {}",
                crate::region::render(r)
            );
            assert_eq!(
                s.len(),
                SAFE_BAND + 3,
                "幅が変わった: {}",
                crate::region::render(r)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **決定的**であること。同じ台帳・同じ要求からは 1 バイト違わない結果が出る
    /// (`HashMap` を通すと走査順が実行ごとに変わり、64 体が別々の配置を作って
    ///  「ずらしたのに重なる」が起きる)。
    ///
    /// 規模を 32 体 / 1000 行に落としているのは**速度のため**。錨の取り直しは
    /// 担当数 × 行数で効くので、64 体 / 2000 行を 5 周すると 1 件で 16 秒かかる
    /// (実測)。決まり方は規模に依らないので、64 体ぶんは
    /// `crowded_な担当表でも六十四体全部がずらして入る` が押さえる。
    #[test]
    fn ずらした結果は決定的で安全帯を必ず挟む() {
        let dir = unique_temp_dir("zaivern", "lease-shift-det");
        std::fs::write(dir.join("a.rs"), body(1000)).expect("中身を置く");
        let plan = crowded_plan(32, 1000);
        let run = || {
            let mut store = Store::default();
            let mut out: Vec<String> = Vec::new();
            for (i, p) in plan.iter().enumerate() {
                match try_claim_shift_in(
                    &dir,
                    &mut store,
                    &who(&format!("A{i}")),
                    std::slice::from_ref(p),
                    100,
                    600,
                    &dead,
                    None,
                ) {
                    ShiftClaim::Granted(gs) => out.push(gs[0].spec.clone()),
                    ShiftClaim::Refused { owner, .. } => panic!("断られた: {owner}"),
                }
            }
            out
        };
        let first = run();
        for _ in 0..4 {
            assert_eq!(run(), first, "同じ入力から違う配置が出た");
        }
        let spans: Vec<Span> = first.iter().map(|s| spec_span(s).expect("行域")).collect();
        for i in 0..spans.len() {
            for j in (i + 1)..spans.len() {
                assert!(
                    !crate::region::spans_too_close(&spans[i], &spans[j], SAFE_BAND),
                    "{:?} と {:?} が安全帯より近い",
                    spans[i],
                    spans[j]
                );
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **64 スレッドが同時に `--shift` で取りに行っても重なりは 1 件も出ない。**
    ///
    /// 位置決めを台帳ロックの外でやると、全員が同じ空きを見つけて
    /// 同じ場所を取りに行く。確保はアトミックでなければならない。
    ///
    /// ## 規模を 1 行幅 / 300 行にしている理由 (実測)
    /// 錨の取り直し ([`crate::region::resolve`]) は**呼ぶたびにテキスト全体を
    /// 行へ割り直す**ので、臨界区間は「行数 × 担当数」で伸びる。
    /// 2000 行 × 幅 6 行だと 1 件 30ms・64 体で台帳ロックを 2 秒握ることになり、
    /// **全テスト同時実行の負荷では `with_store_retry` の 30 秒の上限を超えて
    /// busy が出た** (2000 行で 33 件、800 行で 13 件)。
    /// ここが見たいのは「同時に取りに行っても重ならない」ことなので、
    /// 混み具合 (64 体が 64 行の中を取り合う) は保ったまま行数を落とす。
    /// 幅 6 行 / 2000 行の crowded 条件そのものは
    /// `crowded_な担当表でも六十四体全部がずらして入る` が押さえている。
    #[test]
    fn 六十四スレッドが同時にずらしても重なりは生まれない() {
        let dir = unique_temp_dir("zaivern", "lease-shift-64");
        let total = 300u32;
        std::fs::write(dir.join("a.rs"), body(total)).expect("中身を置く");
        let store = dir.join("s.json");
        enable(&store).expect("有効化");
        // 幅 1 行を stride 1 で 64 個 = 全員が互いに安全帯より近い。
        // 互いに素に置くには 64 × (1 + 3) = 256 行要る (300 行なら入る)。
        let base = (total - 63 - 1) / 2;
        let plan: Vec<String> = (0..64).map(|i| format!("a.rs#L{}", base + i)).collect();
        let mut hs = Vec::with_capacity(plan.len());
        for (i, p) in plan.iter().enumerate() {
            let (store, tree, p) = (store.clone(), dir.clone(), p.clone());
            hs.push(std::thread::spawn(move || {
                with_store_retry(&store, |st| {
                    try_claim_shift_in(
                        &tree,
                        st,
                        &who(&format!("A{i}")),
                        std::slice::from_ref(&p),
                        100,
                        600,
                        &dead,
                        None,
                    )
                })
            }));
        }
        let (mut granted, mut refused, mut busy) = (0, 0, 0);
        let mut retry: Vec<String> = Vec::new();
        for (h, p) in hs.into_iter().zip(plan.iter()) {
            match h.join().expect("スレッド") {
                Ok(ShiftClaim::Granted(_)) => granted += 1,
                Ok(ShiftClaim::Refused { .. }) => refused += 1,
                Err(e) if is_lock_busy(&e) => {
                    busy += 1;
                    retry.push(p.clone());
                }
                Err(e) => panic!("台帳が壊れた: {e}"),
            }
        }
        // **`busy` は正しさの破れではない。** 「混んでいて判定できなかった」
        // という意味で、確保も拒否もしていない (台帳は 1 バイトも変わらない)。
        // 実運用の 64 プロセスでは 0 件だが、**全 4273 件のテストと同時に
        // 64 スレッドを走らせる**この環境は実運用より厳しく、機械が飽和すると
        // 数件出る。ここで固定したいのは「同時に取りに行っても重ならない」で
        // あって機械の速さではないので、**呼び出し側と同じように取り直して**
        // から数える (`zai lease claim` も busy なら再実行すれば通る)。
        if busy > 0 {
            eprintln!("負荷で busy が {busy} 件出たので取り直す");
        }
        for p in &retry {
            let out = with_store_retry(&store, |st| {
                try_claim_shift_in(
                    &dir,
                    st,
                    &who(&format!("retry-{p}")),
                    std::slice::from_ref(p),
                    100,
                    600,
                    &dead,
                    None,
                )
            })
            .expect("取り直しは通る");
            match out {
                ShiftClaim::Granted(_) => granted += 1,
                ShiftClaim::Refused { .. } => refused += 1,
            }
        }
        assert_eq!(
            granted, 64,
            "断られた {refused} 件 (busy から取り直したのは {busy} 件)"
        );
        let st = read_store(&store).expect("読める");
        // **担当の数**で数える (持ち主の数ではない)。同じ持ち主が 2 つ確保すると
        // 1 件のリースに 2 つのパターンが入るので、リース数では取りこぼす。
        let owned: usize = st.leases.iter().map(|l| l.patterns.len()).sum();
        assert_eq!(owned, 64, "台帳に載った担当が 64 件でない");
        // **不変条件**: 64 スレッドが同時に走っても、台帳の担当は互いに素
        let regions: Vec<crate::region::Region> = st
            .leases
            .iter()
            .flat_map(|l| l.patterns.iter().map(|p| spec_region(p)))
            .collect();
        assert_eq!(regions.len(), 64);
        assert!(
            crate::region::is_disjoint(&regions, SAFE_BAND),
            "重なった担当が台帳に載った: {:?}",
            crate::region::conflicting_pairs(&regions, SAFE_BAND)
        );
        for r in &regions {
            let sp = r.span.expect("行域");
            assert!(
                sp.end <= total,
                "ファイルの外へ出した: {}",
                crate::region::render(r)
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 空いていればずらさない。**同じ域を取り直しても動かない** (冪等)。
    #[test]
    fn 空いていればずらさず要求どおり取る() {
        let dir = unique_temp_dir("zaivern", "lease-shift-asis");
        std::fs::write(dir.join("a.rs"), body(200)).expect("中身を置く");
        let mut store = Store::default();
        for _ in 0..3 {
            match try_claim_shift_in(
                &dir,
                &mut store,
                &who("A"),
                &["a.rs#L10-20".into()],
                100,
                600,
                &dead,
                None,
            ) {
                ShiftClaim::Granted(gs) => {
                    assert_eq!(gs.len(), 1);
                    assert!(!gs[0].moved(), "空いているのにずらした: {:?}", gs[0]);
                    assert_eq!(gs[0].spec, "a.rs#L10-20");
                }
                ShiftClaim::Refused { owner, .. } => panic!("断られた: {owner}"),
            }
        }
        assert_eq!(store.leases.len(), 1);
        assert_eq!(store.leases[0].patterns, vec!["a.rs#L10-20".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **同じ確保の中で重なっている 2 件も、ずらして両方入る。**
    /// (先に置いた域を数えないと、自分自身と重なった台帳を作ってしまう)
    #[test]
    fn 同じ確保の中の重なりもずらして両方取る() {
        let dir = unique_temp_dir("zaivern", "lease-shift-self");
        std::fs::write(dir.join("a.rs"), body(200)).expect("中身を置く");
        let mut store = Store::default();
        let gs = match try_claim_shift_in(
            &dir,
            &mut store,
            &who("A"),
            &["a.rs#L50-60".into(), "a.rs#L55-65".into()],
            100,
            600,
            &dead,
            None,
        ) {
            ShiftClaim::Granted(gs) => gs,
            ShiftClaim::Refused { owner, .. } => panic!("断られた: {owner}"),
        };
        assert_eq!(gs.len(), 2);
        assert!(!gs[0].moved(), "1 件目は空いている: {:?}", gs[0]);
        assert!(gs[1].moved(), "重なっているのに動かなかった: {:?}", gs[1]);
        assert_eq!(gs[1].spec, "a.rs#L64-74");
        let regions: Vec<crate::region::Region> = store.leases[0]
            .patterns
            .iter()
            .map(|p| spec_region(p))
            .collect();
        assert_eq!(regions.len(), 2, "畳まれた: {:?}", store.leases[0].patterns);
        assert!(crate::region::is_disjoint(&regions, SAFE_BAND));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **挿入点 (幅 0) もずらせる。**
    ///
    /// 幅 0 でも判定では「その行の位置にある点」なので (`Span::probe`)、
    /// 安全帯を挟んだ空きへ動かせる。行域へ化けないことも見る。
    #[test]
    fn 挿入点も空いている場所へずらせる() {
        let dir = unique_temp_dir("zaivern", "lease-shift-insert");
        std::fs::write(dir.join("a.rs"), body(200)).expect("中身を置く");
        let mut store = Store::default();
        assert!(matches!(
            try_claim_shift_in(
                &dir,
                &mut store,
                &who("A"),
                &["a.rs#L100-120".into()],
                100,
                600,
                &dead,
                None,
            ),
            ShiftClaim::Granted(_)
        ));
        let got = match try_claim_shift_in(
            &dir,
            &mut store,
            &who("B"),
            &["a.rs#@110".into()],
            100,
            600,
            &dead,
            None,
        ) {
            ShiftClaim::Granted(gs) => gs[0].spec.clone(),
            ShiftClaim::Refused { owner, .. } => panic!("断られた: {owner}"),
        };
        let s = spec_span(&got).expect("行域");
        assert!(s.is_insert(), "挿入点が行域へ化けた: {got}");
        // 手前 96 と後ろ 124 は要求 110 から同じ距離。**同点は小さいほう。**
        assert_eq!(got, "a.rs#@96", "{got}");
        let regions: Vec<crate::region::Region> = store
            .leases
            .iter()
            .flat_map(|l| l.patterns.iter().map(|p| spec_region(p)))
            .collect();
        assert!(
            crate::region::is_disjoint(&regions, SAFE_BAND),
            "{:?}",
            crate::region::conflicting_pairs(&regions, SAFE_BAND)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **ずらせないものはずらさずに断る。**
    /// 知らない場所を勧めるくらいなら断るほうがよい —
    /// 「台帳では取れているのに書く先が無い」がいちばん気付きにくい壊れ方。
    #[test]
    fn ずらせない要求は従来どおり断る() {
        let dir = unique_temp_dir("zaivern", "lease-shift-nope");
        std::fs::write(dir.join("a.rs"), body(60)).expect("中身を置く");
        // ① 誰かがファイルを丸ごと持っている = 1 行も空いていない
        let mut whole = Store::default();
        try_claim(&mut whole, &who("A"), &["a.rs".into()], 100, 600, &dead);
        for spec in ["a.rs", "a.rs#L10-20", "a.rs#L10-", "a.rs#@10"] {
            assert!(
                matches!(
                    try_claim_shift_in(
                        &dir,
                        &mut whole,
                        &who("B"),
                        &[spec.to_string()],
                        100,
                        600,
                        &dead,
                        None,
                    ),
                    ShiftClaim::Refused { .. }
                ),
                "丸ごと持たれているのに通した: {spec}"
            );
        }
        // ② ファイルが読めない = 行数が分からない
        let mut ghost = Store::default();
        try_claim(
            &mut ghost,
            &who("A"),
            &["ghost.rs#L10-20".into()],
            100,
            600,
            &dead,
        );
        assert!(matches!(
            try_claim_shift_in(
                &dir,
                &mut ghost,
                &who("B"),
                &["ghost.rs#L12-22".into()],
                100,
                600,
                &dead,
                None,
            ),
            ShiftClaim::Refused { .. }
        ));
        // ③ ファイルより広い域はどこにも入らない
        let mut wide = Store::default();
        try_claim(
            &mut wide,
            &who("A"),
            &["a.rs#L10-20".into()],
            100,
            600,
            &dead,
        );
        assert!(matches!(
            try_claim_shift_in(
                &dir,
                &mut wide,
                &who("B"),
                &["a.rs#L1-60".into()],
                100,
                600,
                &dead,
                None,
            ),
            ShiftClaim::Refused { .. }
        ));
        // ④ glob はどのファイルを指すか確定しない
        let mut glob = Store::default();
        try_claim(
            &mut glob,
            &who("A"),
            &["a.rs#L10-20".into()],
            100,
            600,
            &dead,
        );
        assert!(matches!(
            try_claim_shift_in(
                &dir,
                &mut glob,
                &who("B"),
                &["*.rs#L12-22".into()],
                100,
                600,
                &dead,
                None,
            ),
            ShiftClaim::Refused { .. }
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **全か無か。** 1 件でもずらす先が無ければ 1 件も取らない
    /// (台帳の書き換えは最後の `try_claim_wants` 1 回だけ)。
    #[test]
    fn ずらせない一件が混ざったら一件も取らない() {
        let dir = unique_temp_dir("zaivern", "lease-shift-allornone");
        std::fs::write(dir.join("a.rs"), body(200)).expect("中身を置く");
        let mut store = Store::default();
        try_claim(
            &mut store,
            &who("A"),
            &["a.rs#L10-20".into(), "b.rs".into()],
            100,
            600,
            &dead,
        );
        let before = store.clone();
        // `a.rs` はずらせるが、`b.rs` は丸ごと持たれていてずらせない
        assert!(matches!(
            try_claim_shift_in(
                &dir,
                &mut store,
                &who("B"),
                &["a.rs#L12-22".into(), "b.rs".into()],
                100,
                600,
                &dead,
                None,
            ),
            ShiftClaim::Refused { .. }
        ));
        assert_eq!(store, before, "断ったのに台帳が変わった");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `--shift` を通さない経路は**1 バイトも変わらない** —
    /// 同じ台帳・同じ要求で `try_claim` は昔どおり断る。
    #[test]
    fn shift_を通さなければ従来どおり拒否する() {
        let dir = unique_temp_dir("zaivern", "lease-shift-off");
        std::fs::write(dir.join("a.rs"), body(200)).expect("中身を置く");
        let mut store = Store::default();
        try_claim(
            &mut store,
            &who("A"),
            &["a.rs#L50-60".into()],
            100,
            600,
            &dead,
        );
        let snapshot = store.clone();
        assert!(matches!(
            try_claim(
                &mut store,
                &who("B"),
                &["a.rs#L55-65".into()],
                100,
                600,
                &dead
            ),
            Claim::Refused { .. }
        ));
        assert_eq!(store, snapshot, "拒否なのに台帳が変わった");
        // 同じ要求を `--shift` で出せば通る
        assert!(matches!(
            try_claim_shift_in(
                &dir,
                &mut store,
                &who("B"),
                &["a.rs#L55-65".into()],
                100,
                600,
                &dead,
                None,
            ),
            ShiftClaim::Granted(_)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// ずらした先の**錨は打ち直す**。元の錨をそのまま持たせると、
    /// 次の取り直しで**他人の域へ吸い寄せられる** (静かに保証が破れる)。
    #[test]
    fn ずらした先の錨は打ち直される() {
        let dir = unique_temp_dir("zaivern", "lease-shift-anchor");
        std::fs::write(dir.join("a.rs"), body(200)).expect("中身を置く");
        let mut store = Store::default();
        try_claim_shift_in(
            &dir,
            &mut store,
            &who("A"),
            &["a.rs#L50-60".into()],
            100,
            600,
            &dead,
            None,
        );
        let spec = match try_claim_shift_in(
            &dir,
            &mut store,
            &who("B"),
            &["a.rs#L55-65".into()],
            100,
            600,
            &dead,
            None,
        ) {
            ShiftClaim::Granted(gs) => gs[0].spec.clone(),
            ShiftClaim::Refused { owner, .. } => panic!("断られた: {owner}"),
        };
        let b = store
            .leases
            .iter()
            .find(|l| l.holder.same(&who("B")))
            .expect("B のリース");
        let span = spec_span(&spec).expect("行域");
        let text = body(200);
        assert_eq!(b.anchors.len(), 1);
        assert!(!b.anchors[0].is_blank(), "錨が打たれていない");
        // 錨から取り直した域が、ずらした先そのものに戻る
        let mut r = spec_region(&spec);
        r.anchor = b.anchors[0].clone();
        assert_eq!(crate::region::resolve(&r, &text), Some(span));
        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  錨 (アンカー) — 行域を「行番号」ではなく「そこにある内容」に紐づける
//
//  行域リースの弱点は、**台帳が行番号しか覚えていない**ことだった。
//  A が 100 行目付近へ 10 行足すと、B の「200〜260 行目」は実際には
//  「210〜270 行目」へ動く。行番号だけを信じると、次の判定で B は
//  **他人の領域を自分のものだと思い込む** — 拒否も警告も出ないまま
//  「衝突ゼロ」の保証が静かに破れる、いちばん危ない壊れ方である。
//
//  ここは `region::capture_anchor` (確保の瞬間) と `region::resolve`
//  (判定の瞬間) が**本当に繋がっている**ことを実ファイルで固定する。
// ═══════════════════════════════════════════════════════════════════════════
#[cfg(test)]
mod anchor_tests {
    use super::*;
    use crate::region::{Span, SAFE_BAND};
    use crate::test_util::unique_temp_dir;

    fn dead(_: u32) -> bool {
        false
    }

    fn who(agent: &str) -> Holder {
        Holder {
            agent: agent.to_string(),
            session: format!("s-{agent}"),
            cwd: String::new(),
            pid: 0,
        }
    }

    /// `line 1` … `line n` だけのファイル。**同じ内容の行が 1 つも無い**ので、
    /// 錨が当たらなかったときに「曖昧で断った」と「消えていた」を混同しない。
    fn numbered(n: u32) -> String {
        (1..=n).map(|i| format!("line {i}\n")).collect()
    }

    /// `at` 行目の**手前**へ `count` 行差し込む (1 始まり)。
    fn insert_before(text: &str, at: u32, count: u32) -> String {
        let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
        let at = (at.max(1) - 1) as usize;
        for i in 0..count {
            lines.insert(at + i as usize, format!("inserted {i}"));
        }
        lines.join("\n") + "\n"
    }

    fn claim(
        tree: &Path,
        store: &Path,
        holder: &Holder,
        spec: &str,
        now: u64,
    ) -> Result<Claim, String> {
        with_store(store, |s| {
            try_claim_in(tree, s, holder, &[spec.to_string()], now, 600, &dead)
        })
    }

    /// スコープ相対の担当を、いまのテキストで取り直した一覧にする
    /// (不変条件の検査に使う)。
    fn live_regions(st: &Store, rel: &str, text: &str) -> Vec<crate::region::Region> {
        let mut out = Vec::new();
        for l in &st.leases {
            for i in 0..l.patterns.len() {
                let mut r = l.region_at(i);
                if !covers(&l.patterns[i], rel) {
                    continue;
                }
                if let Some(span) = r.span {
                    r.span = Some(crate::region::resolve(&r, text).unwrap_or(span));
                }
                r.anchor = crate::region::Anchor::default();
                out.push(r);
            }
        }
        out
    }

    // ── 1. 後方互換 — 錨を知らない台帳をそのまま読み書きできる ─────────

    /// **`anchors` 欄が無い台帳も、長さがずれた台帳も落ちずに読める。**
    /// `#[serde(default)]` を外したり長さを信じたりすると、既存ユーザーの
    /// 台帳が読めなくなる = ガードが丸ごと無効になる (fail-open の最悪形)。
    #[test]
    fn 錨を知らない古い台帳をそのまま読み書きできる() {
        let dir = unique_temp_dir("zaivern", "lease-anchor-compat");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let store = dir.join("s.json");
        // 版を上げていないので、`anchors` の欄そのものが無い
        let raw = r#"{"leases":[{"holder":{"agent":"old","session":"s-old","cwd":"","pid":0},
            "patterns":["src/a.rs#L10-40","src/b.rs"],
            "acquired_at":1,"expires_at":4102444800,"note":""}]}"#;
        std::fs::write(&store, raw).expect("write");
        let st = read_store(&store).expect("古い台帳が読めない");
        let l = &st.leases[0];
        assert_eq!(l.patterns.len(), 2);
        assert_eq!(l.anchors.len(), 2, "読み込みの一点で長さが揃う");
        assert!(l.anchor_at(0).is_blank(), "錨が無い = 行番号だけで持つ");
        assert!(l.anchor_at(99).is_blank(), "範囲外を引いても落ちない");

        // 錨が無いので、テキストを渡しても判定は**錨が入る前とまったく同じ**。
        let rel = normalize_path("src/a.rs");
        let other = numbered(50);
        assert!(
            l.touches(&rel, &[Span::line(20)], Some(&other)),
            "錨なしの域は台帳の行番号のまま効く"
        );
        assert!(!l.touches(&rel, &[Span::line(200)], Some(&other)));

        // 錨の数が patterns とずれた台帳 (手で編集された / 別版が書いた)。
        let skew = r#"{"leases":[{"holder":{"agent":"skew","session":"s-skew","cwd":"","pid":0},
            "patterns":["src/a.rs#L10-40"],
            "anchors":[{"head":"a","tail":"b","len":31},{"head":"x","tail":"y","len":9},
                       {"head":"z","tail":"w","len":2}],
            "acquired_at":1,"expires_at":4102444800,"note":""}]}"#;
        std::fs::write(&store, skew).expect("write");
        let st = read_store(&store).expect("ずれた台帳が読めない");
        assert_eq!(st.leases[0].anchors.len(), 1, "余った錨は落とす");
        assert_eq!(st.leases[0].anchor_at(0).head, "a", "先頭の対応は保つ");

        // 錨つきの台帳は書いて読み戻せる (往復)。
        let mut back = st.clone();
        back.leases[0].anchors[0].len = 31;
        write_store(&store, &back).expect("書ける");
        assert_eq!(read_store(&store).expect("読める"), back, "往復で変わった");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 2. 本題 — 上に行が入っても持ち主が変わらない ───────────────────

    /// **これが錨を繋いだ証拠。**
    ///
    /// 1. 300 行のファイルを置く
    /// 2. A が `#L200-260` を確保する (ここで錨が打たれる)
    /// 3. 100 行目の手前へ 10 行差し込む → A の担当は **210〜270 行目**として解決される
    /// 4. さらに差し込んで合計 100 行ずらす → A は **300〜360 行目**
    /// 5. B が「200〜260 行目」を要求したら**通る** (A はもうそこに居ない)
    /// 6. B が「300〜360 行目」を要求したら**拒否される** (A がそこに居る)
    ///
    /// 5 と 6 でずらす量を 100 行にしてあるのは算数の都合:
    /// 61 行の域を 10 行ずらしても元の域と 51 行重なるので、
    /// 「元の行番号が空く」ことを見せられない。**元の域と離れる**ところまで
    /// ずらして初めて、行番号ではなく中身で持っていることが確かめられる。
    #[test]
    fn 上に行が挿入されても行域の持ち主は変わらない() {
        let dir = unique_temp_dir("zaivern", "lease-anchor-shift");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("src")).expect("mkdir");
        let abs = tree.join("src").join("a.rs");
        let store = dir.join("s.json");
        let rel = normalize_path("src/a.rs");
        let (a, b) = (who("a"), who("b"));
        let now = 1_000;

        let base = numbered(300);
        std::fs::write(&abs, &base).expect("write");
        assert!(
            matches!(
                claim(&tree, &store, &a, "src/a.rs#L200-260", now).expect("確保"),
                Claim::Granted(1)
            ),
            "A が 200-260 を取れない"
        );
        // 錨は**確保の瞬間**に打たれる。
        let st = read_store(&store).expect("読める");
        let la = st.leases.iter().find(|l| l.holder.same(&a)).expect("A");
        assert_eq!(la.patterns, vec![normalize_spec("src/a.rs#L200-260")]);
        assert_eq!(
            la.anchor_at(0).head,
            "line 200",
            "先頭行の中身を覚えていない"
        );
        assert_eq!(
            la.anchor_at(0).tail,
            "line 260",
            "末尾行の中身を覚えていない"
        );
        assert_eq!(la.anchor_at(0).len, 61);

        // 100 行目の手前へ 10 行 → 210〜270 行目
        let shifted10 = insert_before(&base, 100, 10);
        std::fs::write(&abs, &shifted10).expect("write");
        assert_eq!(
            la.owned_spans(&rel, Some(&shifted10)),
            Some(vec![Span {
                start: 210,
                end: 270
            }]),
            "10 行入っても担当が付いていっていない"
        );
        // 台帳は**1 バイトも書き換えていない** (遅延解決 — 帳簿付けが要らない)。
        assert_eq!(
            la.patterns,
            vec![normalize_spec("src/a.rs#L200-260")],
            "判定のたびに台帳を書き直してはいけない"
        );

        // 合計 100 行 → 300〜360 行目
        let shifted = insert_before(&base, 100, 100);
        std::fs::write(&abs, &shifted).expect("write");
        assert_eq!(
            la.owned_spans(&rel, Some(&shifted)),
            Some(vec![Span {
                start: 300,
                end: 360
            }])
        );

        // B は「元の行番号」を取れる — A はもうそこに居ない。
        match claim(&tree, &store, &b, "src/a.rs#L200-260", now).expect("確保") {
            Claim::Granted(1) => {}
            other => panic!("A が居なくなった行を取れない: {other:?}"),
        }
        // B は「A がいま居る行」を取れない。
        match claim(&tree, &store, &b, "src/a.rs#L300-360", now).expect("確保") {
            Claim::Refused { owner, .. } => assert!(owner.contains('a'), "誰に断られたか: {owner}"),
            other => panic!("A の居る行が取れてしまった: {other:?}"),
        }

        // **不変条件**: 取り直した後でも、どの 2 人の担当も重ならない。
        let st = read_store(&store).expect("読める");
        let regions = live_regions(&st, &rel, &shifted);
        assert!(
            crate::region::is_disjoint(&regions, SAFE_BAND),
            "重なった担当が残った: {:?}",
            crate::region::conflicting_pairs(&regions, SAFE_BAND)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **域が丸ごと消えたら、記録された行番号のまま止め続ける。**
    /// ここを「誰のものでもない」に倒すと 2 人が同じ場所を書けるようになり、
    /// 「ファイル全体」に倒すと持ち主が自分の先頭行を直しただけで
    /// 同じファイルの他の担当を全員締め出す。**どちらも採らない。**
    #[test]
    fn 取り直せない域は記録された行番号のまま効く() {
        let dir = unique_temp_dir("zaivern", "lease-anchor-lost");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("src")).expect("mkdir");
        let abs = tree.join("src").join("a.rs");
        let store = dir.join("s.json");
        let rel = normalize_path("src/a.rs");
        let (a, b) = (who("a"), who("b"));

        std::fs::write(&abs, numbered(300)).expect("write");
        claim(&tree, &store, &a, "src/a.rs#L200-260", 1_000).expect("確保");
        let st = read_store(&store).expect("読める");
        let la = st.leases.iter().find(|l| l.holder.same(&a)).expect("A");

        // 錨にした行を丸ごと消す = 取り直せない
        let gone: String = (1..=150).map(|i| format!("line {i}\n")).collect();
        assert_eq!(crate::region::resolve(&la.region_at(0), &gone), None);
        assert_eq!(
            la.owned_spans(&rel, Some(&gone)),
            Some(vec![Span {
                start: 200,
                end: 260
            }]),
            "取り直せないときは台帳の行番号へ落ちる"
        );
        std::fs::write(&abs, &gone).expect("write");
        match claim(&tree, &store, &b, "src/a.rs#L200-260", 1_000).expect("確保") {
            Claim::Refused { .. } => {}
            other => panic!("持ち主が判らない域を誰でも取れてしまう: {other:?}"),
        }
        // 離れた行はいつもどおり取れる (ファイル全体へは倒していない)。
        assert!(matches!(
            claim(&tree, &store, &b, "src/a.rs#L1-50", 1_000).expect("確保"),
            Claim::Granted(1)
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 3. 記号で指す域 ────────────────────────────────────────────────

    /// `zai lease claim 'src/a.rs#fn:draw_toolbar'` が実ファイルから行域になる。
    /// **直上の doc コメントと属性まで含む** (持ち主が自分の doc を直せないと
    /// 使い物にならない)。
    #[test]
    fn 記号で指した域が実ファイルから行域になる() {
        let dir = unique_temp_dir("zaivern", "lease-anchor-symbol");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("src")).expect("mkdir");
        let store = dir.join("s.json");
        // 2 つの関数は安全帯 (3 行) より離して置く — 隣り合っていると
        // 「記号が読めていない」のか「近すぎて断られた」のか区別が付かない。
        let src = [
            "// あたま",                          // 1
            "fn other() {",                       // 2
            "    1",                              // 3
            "}",                                  // 4
            "// 埋め 1",                          // 5
            "// 埋め 2",                          // 6
            "// 埋め 3",                          // 7
            "// 埋め 4",                          // 8
            "// 埋め 5",                          // 9
            "/// ツールバーを描く",               // 10
            "pub fn draw_toolbar(ui: &mut Ui) {", // 11
            "    let _ = ui;",                    // 12
            "}",                                  // 13
            "",                                   // 14
            "fn tail() {}",                       // 15
        ]
        .join("\n")
            + "\n";
        std::fs::write(tree.join("src").join("a.rs"), &src).expect("write");
        let a = who("a");

        assert!(matches!(
            claim(&tree, &store, &a, "src/a.rs#fn:draw_toolbar", 1_000).expect("確保"),
            Claim::Granted(1)
        ));
        let st = read_store(&store).expect("読める");
        let l = &st.leases[0];
        assert_eq!(
            l.patterns,
            vec![normalize_spec("src/a.rs#L10-13")],
            "記号が行域へ落ちていない (直上の doc コメント込み)"
        );
        assert_eq!(l.anchor_at(0).head, "/// ツールバーを描く");
        assert!(!l.anchor_at(0).is_blank(), "記号で取った域にも錨が要る");

        // 別人はその関数の中を取れないが、離れた関数は取れる。
        let b = who("b");
        for spec in ["src/a.rs#fn:draw_toolbar", "src/a.rs#L11"] {
            assert!(
                matches!(
                    claim(&tree, &store, &b, spec, 1_000).expect("確保"),
                    Claim::Refused { .. }
                ),
                "{spec} が取れてしまった"
            );
        }
        assert!(matches!(
            claim(&tree, &store, &b, "src/a.rs#fn:other", 1_000).expect("確保"),
            Claim::Granted(1)
        ));
        assert_eq!(
            read_store(&store)
                .expect("読める")
                .leases
                .iter()
                .find(|l| l.holder.same(&b))
                .expect("B")
                .patterns,
            vec![normalize_spec("src/a.rs#L2-4")]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **記号が見つからない / ファイルが読めないときは 1 件も確保しない。**
    ///
    /// 黙ってファイル全体として扱うと「関数 1 つ取った」つもりの本人が
    /// 他の担当を全員締め出し、黙って捨てると取れたつもりで誰にも守られない。
    /// どちらも後から気付けないので、**その場で断る**のが唯一正しい。
    #[test]
    fn 記号が見つからないときは一件も確保しない() {
        let dir = unique_temp_dir("zaivern", "lease-anchor-nosym");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("src")).expect("mkdir");
        let store = dir.join("s.json");
        std::fs::write(tree.join("src").join("a.rs"), "fn other() {}\n").expect("write");
        let a = who("a");

        for spec in ["src/a.rs#fn:居ない関数", "src/none.rs#fn:draw_toolbar"] {
            match claim(&tree, &store, &a, spec, 1_000).expect("確保") {
                Claim::Refused { owner, pattern, .. } => {
                    assert_eq!(pattern, spec);
                    assert!(!owner.is_empty(), "理由が空: {spec}");
                }
                other => panic!("{spec} が通ってしまった: {other:?}"),
            }
        }
        // 全か無か — 1 件も載っていない。
        assert!(read_store(&store).expect("読める").leases.is_empty());

        // 同じ確保に読める記号と読めない記号が混ざっても、1 件も取らない。
        let mixed = vec![
            "src/a.rs#fn:other".to_string(),
            "src/a.rs#fn:居ない関数".to_string(),
        ];
        let got = with_store(&store, |s| {
            try_claim_in(&tree, s, &a, &mixed, 1_000, 600, &dead)
        })
        .expect("確保");
        assert!(matches!(got, Claim::Refused { .. }), "{got:?}");
        assert!(read_store(&store).expect("読める").leases.is_empty());

        // **行域指定は読めなくても断らない** — 錨が無いだけで、
        // 錨が入る前とまったく同じ判定へ落ちる。
        assert!(matches!(
            claim(&tree, &store, &a, "src/none.rs#L10-40", 1_000).expect("確保"),
            Claim::Granted(1)
        ));
        assert!(read_store(&store).expect("読める").leases[0]
            .anchor_at(0)
            .is_blank());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── 4. 不変条件 ───────────────────────────────────────────────────

    /// **ファイル全体の保持者が居るなら、同じファイルの行域は 1 つも取れない。**
    /// 逆も同じ (行域を持つ人が居るファイルは丸ごと取れない)。
    /// 錨が入っても、この向きは 1 ミリも変わってはいけない。
    #[test]
    fn 全体と行域は錨が入っても互いに排他のまま() {
        let dir = unique_temp_dir("zaivern", "lease-anchor-whole");
        let tree = dir.join("tree");
        std::fs::create_dir_all(tree.join("src")).expect("mkdir");
        let store = dir.join("s.json");
        std::fs::write(tree.join("src").join("a.rs"), numbered(300)).expect("write");
        let (a, b) = (who("a"), who("b"));

        claim(&tree, &store, &a, "src/a.rs", 1_000).expect("確保");
        for spec in ["src/a.rs#L10-40", "src/a.rs#L900-999", "src/a.rs"] {
            assert!(
                matches!(
                    claim(&tree, &store, &b, spec, 1_000).expect("確保"),
                    Claim::Refused { .. }
                ),
                "全体の持ち主が居るのに {spec} が取れた"
            );
        }
        // 逆向き
        let dir2 = unique_temp_dir("zaivern", "lease-anchor-whole2");
        let store2 = dir2.join("s.json");
        claim(&tree, &store2, &a, "src/a.rs#L10-40", 1_000).expect("確保");
        assert!(matches!(
            claim(&tree, &store2, &b, "src/a.rs", 1_000).expect("確保"),
            Claim::Refused { .. }
        ));
        // ファイル全体には錨を打たない (行番号に依存しないので要らない)。
        let st = read_store(&store).expect("読める");
        assert!(st.leases[0].anchor_at(0).is_blank());
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// **fail-open の範囲は錨が入っても広がらない。**
    /// 台帳が無いだけなら通し、在るのに読めないときは止める。
    #[test]
    fn 台帳が無いだけなら通し壊れていれば止める() {
        let dir = unique_temp_dir("zaivern", "lease-anchor-failopen");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let store = dir.join("s.json");
        let a = who("a");
        // 台帳が無い = 機能が無効
        assert!(matches!(
            check_one(&store, &a, "src/a.rs", 1_000, &dead),
            Verdict::Allow
        ));
        // 在るのに読めない = 止める (戻し方を文面に出す)
        std::fs::write(&store, "{ 壊れた").expect("write");
        match check_one(&store, &a, "src/a.rs", 1_000, &dead) {
            Verdict::Deny(m) => assert!(m.contains("zai lease"), "戻し方が無い: {m}"),
            Verdict::Allow => panic!("壊れた台帳で素通しした"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
