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
//! 1. **`src/cli.rs`**: `zai lease claim src/a.rs#L10-40` は**そのまま動く**
//!    ([`try_claim`] が [`normalize_spec`] を通すため)。直したいのは
//!    `HELP_LEASE` の文面だけで、`<パターン...>` の説明へ
//!    「`src/a.rs#L10-40` のように行域も指定できます」を足すと入口が完成する。
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
        Ok(r) if r.span.is_some() => &t[..r.path.len()],
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
        Ok(r) if r.span.is_some() => {
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
    if !covers(pattern, path) {
        return false;
    }
    let Some(own) = spec_span(pattern) else {
        return true; // ファイル全体 = どの行でも自分の領分
    };
    if touched.is_empty() {
        return true; // 触れた行が判らない = 安全側へ倒す
    }
    touched
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
        if ra.span.is_some() || rb.span.is_some() {
            return crate::region::conflicts(&ra, &rb, crate::region::SAFE_BAND);
        }
    }
    seg_overlap(&segments(a), &segments(b))
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
    let (key, tree) = roots_raw(start);
    Roots {
        key: key.canonicalize().unwrap_or(key),
        tree: tree.canonicalize().unwrap_or(tree),
    }
}

fn roots_raw(start: &Path) -> (PathBuf, PathBuf) {
    let base = start.canonicalize().unwrap_or_else(|_| start.to_path_buf());
    for dir in base.ancestors() {
        let dot = dir.join(".git");
        if dot.is_dir() {
            return (dir.to_path_buf(), dir.to_path_buf());
        }
        if dot.is_file() {
            let main = std::fs::read_to_string(&dot)
                .ok()
                .and_then(|t| main_repo_root_from_pointer(&t, dir))
                .unwrap_or_else(|| dir.to_path_buf());
            return (main, dir.to_path_buf());
        }
    }
    (base.clone(), base)
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
    pub fn touches(&self, rel: &str, touched: &[crate::region::Span]) -> bool {
        self.patterns.iter().any(|p| covers_span(p, rel, touched))
    }

    /// `rel` について、このリースが持っている行域。
    ///
    /// `None` = **ファイル全体**を持っている (行域で切り分ける必要が無い)。
    /// `Some(vec![])` = このパスは 1 行も持っていない。
    /// 並びはパターンの登録順のまま = 決定的。
    pub fn owned_spans(&self, rel: &str) -> Option<Vec<crate::region::Span>> {
        let mut out = Vec::new();
        for p in &self.patterns {
            if !covers(p, rel) {
                continue;
            }
            match spec_span(p) {
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
fn absorb(pats: &mut Vec<String>, want: &str) -> bool {
    let w = spec_region(want);
    let mut same: Vec<crate::region::Span> = Vec::new();
    for p in pats.iter() {
        let r = spec_region(p);
        if r.path != w.path {
            continue;
        }
        match r.span {
            // 既にファイル全体を持っている = 行域を足す意味が無い
            None => return false,
            Some(s) => same.push(s),
        }
    }
    let merged: Vec<crate::region::Span> = match w.span {
        // 足すのが「ファイル全体」なら、同じパスの行域は全部畳まれる
        None => Vec::new(),
        Some(w) => {
            let mut cur = w;
            let mut rest = same;
            loop {
                let mut hit = false;
                let mut next = Vec::new();
                for a in rest.drain(..) {
                    if crate::region::spans_too_close(&a, &cur, crate::region::SAFE_BAND) {
                        cur = hull(a, cur);
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
            rest.sort();
            rest.dedup();
            rest
        }
    };
    let render = |s: Option<crate::region::Span>| {
        crate::region::render(&crate::region::Region {
            path: w.path.clone(),
            span: s,
            anchor: crate::region::Anchor::default(),
        })
    };
    // 同じパスの最初の位置へ畳んだ結果を置き、残りは落とす
    // (並べ直さないので、既に整っていれば台帳は 1 バイトも変わらない)。
    let mut out: Vec<String> = Vec::with_capacity(pats.len() + 1);
    let mut placed = false;
    for p in pats.iter() {
        if spec_region(p).path != w.path {
            out.push(p.clone());
            continue;
        }
        if !placed {
            placed = true;
            if merged.is_empty() {
                out.push(render(None));
            } else {
                out.extend(merged.iter().map(|s| render(Some(*s))));
            }
        }
    }
    if !placed {
        if merged.is_empty() {
            out.push(render(None));
        } else {
            out.extend(merged.iter().map(|s| render(Some(*s))));
        }
    }
    let changed = out != *pats;
    *pats = out;
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
    prune(store, now, alive);
    let wanted: Vec<String> = patterns
        .iter()
        .map(|p| normalize_spec(p))
        .filter(|p| !p.is_empty())
        .collect();
    for l in store.leases.iter().filter(|l| !l.holder.same(holder)) {
        for w in &wanted {
            if let Some(hit) = l.patterns.iter().find(|p| overlaps(p, w)) {
                return Claim::Refused {
                    owner: l.holder.display(),
                    pattern: hit.clone(),
                    until: l.expires_at,
                };
            }
        }
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
        let mut added = 0;
        for w in wanted {
            if absorb(&mut mine.patterns, &w) {
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
    let mut n = 0;
    for w in &wanted {
        if absorb(&mut patterns, w) {
            n += 1;
        }
    }
    store.leases.push(Lease {
        holder: holder.clone(),
        patterns,
        acquired_at: now,
        expires_at: expires,
        note: String::new(),
    });
    Claim::Granted(n)
}

/// **断る代わりに「ずらす」提案を出す純関数。**
///
/// 拒否は正しいが、拒否しか返せないと並列度は上がらない。実測 (crowded 条件)
/// では行域リースが 55 件を断っており、そのうち多くは
/// **すぐ近くの空いている行へずらせば通る**。ここはその候補を 1 つ返す。
///
/// * `None` — ずらす必要が無い (`want` がそのまま取れる) か、
///   ずらしようが無い (ファイル全体 / 末尾までの域 / glob /
///   **誰かがそのファイルを丸ごと持っている**)
/// * `Some(r)` — `r` なら誰とも重ならない。**長さは `want` と同じ**
///
/// 選び方は決定的: 候補は「占有域の直後」「占有域の直前」「先頭」だけを見て、
/// `want.start` にいちばん近いもの、同点なら**行番号が小さいほう**。
///
/// **`store` は呼び出し側が [`prune`] 済みであること** — 引数に `now` が
/// 無いのは、交渉層 (メッシュ) が判定済みの台帳を渡す前提だから。
pub fn suggest_alternative(
    store: &Store,
    want: &crate::region::Region,
) -> Option<crate::region::Region> {
    use crate::region::{Span, SAFE_BAND};
    let span = want.span?; // ファイル全体はずらせない
    if span.end == Span::EOF || span.is_empty() {
        return None; // 長さが決まらない
    }
    if want.path.contains(['*', '?', '[']) {
        return None; // glob はどのファイルを指すか確定しない
    }
    let len = span.len();
    let mut busy: Vec<Span> = Vec::new();
    for l in &store.leases {
        for p in &l.patterns {
            if !covers(p, &want.path) {
                continue;
            }
            match spec_span(p) {
                None => return None, // 丸ごと持たれている = ずらす先が無い
                Some(s) => busy.push(s),
            }
        }
    }
    let free = |s: &Span| {
        busy.iter()
            .all(|b| !crate::region::spans_too_close(b, s, SAFE_BAND))
    };
    if free(&span) {
        return None; // そのまま取れる
    }
    let mut cands: Vec<u32> = vec![1];
    for b in &busy {
        // 直後 (安全帯を空ける) と、直前 (同じ長さが入る位置)
        cands.push(b.end.saturating_add(SAFE_BAND).saturating_add(1));
        cands.push(b.start.saturating_sub(SAFE_BAND).saturating_sub(len).max(1));
    }
    cands.retain(|s| *s >= 1);
    cands.sort_unstable();
    cands.dedup();
    let mut best: Option<(u32, u32)> = None;
    for s in cands {
        let c = Span {
            start: s,
            end: s.saturating_add(len - 1),
        };
        if !free(&c) {
            continue;
        }
        let d = s.abs_diff(span.start);
        if best.is_none_or(|(bd, bs)| d < bd || (d == bd && s < bs)) {
            best = Some((d, s));
        }
    }
    let (_, start) = best?;
    Some(crate::region::Region {
        path: want.path.clone(),
        span: Some(Span {
            start,
            end: start.saturating_add(len - 1),
        }),
        anchor: crate::region::Anchor::default(),
    })
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
pub fn decide_spans(
    store: &Store,
    holder: &Holder,
    rel: &str,
    touched: &[crate::region::Span],
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Verdict {
    for l in &store.leases {
        if !l.active(now, alive) || l.holder.same(holder) {
            continue;
        }
        if l.touches(rel, touched) {
            let mut reason = deny_reason(&touched_label(rel, touched), l, now);
            // **断るだけで終わらせない。** 同じ長さが入る近くの空きを一緒に出す。
            // 拒否しか返せないと、エージェントは待つか諦めるかしかできない。
            if let Some(alt) =
                wanted_region(rel, touched).and_then(|w| suggest_alternative(store, &w))
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
pub fn owns_touched(
    store: &Store,
    holder: &Holder,
    rel: &str,
    touched: Option<&[crate::region::Span]>,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> bool {
    let mut mine: Vec<crate::region::Span> = Vec::new();
    let mut any = false;
    for l in &store.leases {
        if !l.active(now, alive) || !l.holder.same(holder) {
            continue;
        }
        match l.owned_spans(rel) {
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
    serde_json::from_str(&raw).map_err(|e| format!("台帳が壊れています: {e}"))
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
struct LockGuard(PathBuf);

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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

fn acquire_lock(store: &Path) -> Result<LockGuard, String> {
    let path = store.with_extension("lock");
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("台帳フォルダを作れません: {e}"))?;
    }
    let deadline = Instant::now() + Duration::from_millis(LOCK_WAIT_MS);
    let mut attempt = 0u32;
    loop {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(_) => return Ok(LockGuard(path)),
            Err(e) if e.kind() != std::io::ErrorKind::AlreadyExists => {
                return Err(format!("ロックを作れません: {e}"))
            }
            Err(_) => {}
        }
        // クラッシュで置き去りになったロックは奪う (でないと永久に詰まる)。
        let stale = std::fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|d| d.as_millis() as u64 > LOCK_STALE_MS);
        if stale {
            let _ = std::fs::remove_file(&path);
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

/// 判定のためにファイルを読む。**上限付き** ([`GATE_READ_CAP`])。
fn read_capped(abs: &Path) -> Option<String> {
    let m = std::fs::metadata(abs).ok()?;
    if !m.is_file() || m.len() > GATE_READ_CAP {
        return None;
    }
    std::fs::read_to_string(abs).ok()
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
fn touched_of(input: &serde_json::Value, abs: &Path) -> Option<Vec<crate::region::Span>> {
    let old = read_capped(abs)?;
    let new = applied_text(&old, input)?;
    let band = crate::region::SAFE_BAND;
    let mut all = crate::region::touched_spans(&old, &new, band);
    all.extend(crate::region::touched_spans(&new, &old, band));
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
/// 1. 書き込み先の**現在の中身**を読む ([`read_capped`] — 上限 [`GATE_READ_CAP`])
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
    let spans: Vec<Option<Vec<crate::region::Span>>> =
        if editing && targets.len() == 1 && !write.opaque {
            let input = v.get("tool_input").unwrap_or(&v);
            vec![touched_of(input, &targets[0].0)]
        } else {
            vec![None; targets.len()]
        };
    let now = now_secs();
    let alive: &dyn Fn(u32) -> bool = &pid_alive;
    // 1 パスぶんの判定 (ロックの内でも外でも同じ規則を使う — 2 実装は必ずズレる)。
    let judge = |st: &Store, i: usize| -> Verdict {
        match &spans[i] {
            Some(t) => decide_spans(st, &holder, &rels[i], t, now, alive),
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
                return deny_answer(agent, &reason);
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
            && (0..targets.len())
                .all(|i| owns_touched(&st, &holder, &rels[i], spans[i].as_deref(), now, alive))
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
        let need = !(0..targets.len())
            .all(|i| owns_touched(st, &holder, &rels[i], spans[i].as_deref(), now, alive));
        if refresh || need {
            // **触れた域だけ**を確保する (`try_claim` は全か無か)。
            // 行域を決められなかったパスは、パスそのもの = ファイル全体。
            let want: Vec<String> = (0..targets.len())
                .flat_map(|i| match &spans[i] {
                    None => vec![rels[i].clone()],
                    Some(t) => t
                        .iter()
                        .map(|s| {
                            crate::region::render(&crate::region::Region {
                                path: rels[i].clone(),
                                span: Some(*s),
                                anchor: crate::region::Anchor::default(),
                            })
                        })
                        .collect(),
                })
                .collect();
            let _ = try_claim(st, &holder, &want, now, DEFAULT_TTL_SECS, alive);
        }
        Verdict::Allow
    });
    match outcome {
        Ok(Verdict::Deny(reason)) => {
            log_line(
                &dir,
                &format!("deny {} {}", holder.display(), rels.join(" ")),
            );
            deny_answer(agent, &reason)
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
    ],
    binds: &[],
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
                        let own = claim_for(&store, &holder, &rel, now_secs(), ttl, &pid_alive);
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
    let pats = vec![rel.to_string()];
    match with_store_retry(store, |s| try_claim(s, holder, &pats, now, ttl, alive)) {
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
                l.patterns.retain(|p| p != rel && spec_path(p) != rel);
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
        assert_eq!(l.owned_spans("src/a.rs"), None, "行域なし = ファイル全体");
        assert!(l.covers_path("src/a.rs"));
        assert!(l.touches("src/a.rs", &[Span { start: 1, end: 2 }]));
        // 他人はどの行でも止まる
        assert!(matches!(
            decide_spans(&st, &who("B"), "src/a.rs", &[Span::line(500)], 100, &dead),
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
            back.leases[0].owned_spans(&normalize_path("src/a.rs")),
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
            owns_touched(&s, &a, "src/a.rs", Some(&[Span::line(3)]), 100, &dead),
            "自分の域は自分のもの"
        );
        assert!(
            !owns_touched(&s, &a, "src/a.rs", Some(&[Span::line(500)]), 100, &dead),
            "持っていない行を自分のものと言っている"
        );
        assert!(
            !owns_touched(&s, &a, "src/a.rs", None, 100, &dead),
            "行域が判らない (= 全体) のに通してしまっている"
        );
        // ファイル全体を持っていれば、行域が判らなくても自分のもの
        let mut s2 = Store::default();
        try_claim(&mut s2, &a, &["src/a.rs".into()], 100, 600, &dead);
        assert!(owns_touched(&s2, &a, "src/a.rs", None, 100, &dead));
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
            decide_spans(&s, &b, "src/a.rs", &[Span::line(10)], 100, &dead),
            Verdict::Deny(_)
        ));
        assert_eq!(
            decide_spans(&s, &b, "src/a.rs", &[Span::line(50)], 100, &dead),
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
        let got = suggest_alternative(&s, &want).expect("提案が無い");
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
            assert_eq!(suggest_alternative(&s, &want), Some(got.clone()));
        }
        // 空いているならずらす必要が無い
        assert_eq!(
            suggest_alternative(&s, &crate::region::parse("a.rs#L100-104").expect("parse")),
            None
        );
        // 手前が空いていて近ければそちらを選ぶ
        let mut s2 = Store::default();
        try_claim(&mut s2, &who("A"), &["a.rs#L50-60".into()], 100, 600, &dead);
        let near_front = crate::region::parse("a.rs#L48-52").expect("parse");
        let got2 = suggest_alternative(&s2, &near_front).expect("提案が無い");
        assert_eq!(crate::region::render(&got2), "a.rs#L42-46");
        // ずらしようが無いものは正直に None
        for spec in ["a.rs", "a.rs#L5-", "src/*.rs#L1-5"] {
            let mut s3 = Store::default();
            try_claim(&mut s3, &who("A"), &["a.rs".into()], 100, 600, &dead);
            assert_eq!(
                suggest_alternative(&s3, &crate::region::parse(spec).expect("parse")),
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
        let Verdict::Deny(reason) = decide_spans(&st, &b, rel, &t, 100, &dead) else {
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
        assert_eq!(decide_spans(&st, &b, rel, &t2, 100, &dead), Verdict::Allow);
        assert!(!owns_touched(&st, &b, rel, Some(&t2), 100, &dead));
        let want: Vec<String> = t2
            .iter()
            .map(|s| format!("{rel}#L{}-{}", s.start, s.end))
            .collect();
        assert_eq!(
            try_claim(&mut st, &b, &want, 100, 600, &dead),
            Claim::Granted(1)
        );
        assert!(owns_touched(&st, &b, rel, Some(&t2), 100, &dead));
        // 続けて同じ域を書くならロックすら要らない
        assert!(owns_touched(
            &st,
            &b,
            rel,
            Some(&[Span::line(10)]),
            100,
            &dead
        ));

        // 全文置換は触れた域が広い → A の域へ掛かるので止まる
        let whole = serde_json::json!({ "content": "全部\n入れ替える\n" });
        let t3 = touched_of(&whole, &f).expect("行域");
        assert!(matches!(
            decide_spans(&st, &b, rel, &t3, 100, &dead),
            Verdict::Deny(_)
        ));
        // 行域を決められない書き込み (シェル経由) も同じく止まる
        assert!(matches!(
            decide_spans(&st, &b, rel, &[], 100, &dead),
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
}
