//! ガード — **ベンダー非依存の書き込み強制**。git そのものを関所にする。
//!
//! ## なぜ要るのか
//!
//! `crate::lease` の「ファイル所有リース」は既にあるが、強制が効くのは
//! [`crate::agents::HOOK_TARGETS`] に載っている 3 ベンダー (claude / codex /
//! gemini) だけで、カタログには 33 種のエージェントが居る。さらに
//! [`crate::lease::gate`] は `eval` やヒアドキュメントのような**不透明な
//! シェル書き込みを `opaque` として記録するだけで通す**。
//! つまり「並列でも競合しない」という約束は、いまは 3 ベンダーに限定された
//! 約束になっている。
//!
//! ## 解き方: git を関所にする
//!
//! どのエージェントも、どのエディタも、素の `vim` も CI も、**成果を残すには
//! 必ず git を通る**。だから git フックを関所にすればベンダーの協力は要らない。
//! 対象は `pre-commit` / `pre-applypatch` / `pre-merge-commit` の 3 つ
//! (index が確定していて、まだ履歴に入っていない最後の地点)。
//!
//! ## 守っている約束
//!
//! * **既存フックを壊さない** — 元のフックは `<name>.zaivern-prev` へ退避し、
//!   生成したフックが**先に呼んで終了コードを尊重する** (husky / lefthook /
//!   pre-commit framework と共存する)。`exec` しないのはそのため。
//! * **fail-open** — `zai` が見つからない / 動かない / 台帳が壊れている
//!   ときは通す。「ツールを消したらコミットできない」は許されない。
//!   止めるのは**本物の競合**のときだけ (fail-closed)。
//! * **未導入者のコストはゼロ** — 台帳が無いリポジトリでは `stat` 1 回で戻る。
//! * **置き場は git に訊く** — `git rev-parse --git-path hooks`。
//!   `.git/hooks` を直書きすると `core.hooksPath` を設定したリポジトリと
//!   linked worktree で**別の場所へ置いてしまい、無言で効かなくなる**。
//! * **POSIX sh** — Windows でも git 同梱の sh で動く内容だけを書く
//!   (`[[` / `local` / 配列などの bash 依存構文は使わない)。
//!
//! ## 行域まで見る (ここを間違えると、行域オーナーシップが無意味になる)
//!
//! 点検は [`crate::lease::Lease::covers_path`] の**ファイル粒度**で始まったが、
//! それでは同じファイルの**離れた行域**を持つ 2 人が、それぞれ自分の担当だけを
//! 直しても**両方止まる**。alice が `src/app.rs#L20-60`、bob が
//! `src/app.rs#L200-240` を持っているとき、bob のコミットに alice のリースが
//! 当たってしまうためである。安全側の誤りではあるが、
//! **64 体が同時に書けても誰もコミットできない**のなら、行域オーナーシップの
//! 価値はコミットの瞬間に丸ごと消える。最後の砦が行域を理解していない、
//! というのがいちばん大きな穴だった。
//!
//! そこで、フックは **index が確定した状態**で走ることを使う。
//! 「今回のコミットが実際に触った行」は git から取れる:
//!
//! ```text
//! git diff --cached --raw -z --unified=0 -p --no-color --no-ext-diff --no-textconv
//! ```
//!
//! **呼び出しは 1 回だけ。** `--raw -z` の記録 (パスと状態) と `-U0` のパッチ
//! (`@@ -a,b +c,d @@` = 触れた行域) が同じ順で 1 度に返るので、
//! 2 回撃つ必要も、パッチからパスを取り出す必要も無い
//! ([`parse_staged`] が n 番目の `diff --git` を n 番目の記録に対応させる)。
//!
//! | ステージされた変更 | 触れた域 | なぜ |
//! |---|---|---|
//! | 内容の変更 (`M`) | `@@` の行域 | 行で説明できる唯一の形 |
//! | 新規 (`A`) / 削除 (`D`) | **ファイル全体** | 「生やす・消す」は行の操作ではない |
//! | リネーム (`R`) / 複製 (`C`) | **両端ともファイル全体** | 同上。旧パスの担当も守る |
//! | 型変更 (`T`) / モード変更 | **ファイル全体** | 行に現れない変更 |
//! | 二値・`-diff` 属性 | **ファイル全体** | ハンクが 1 つも出ない |
//! | 記録数とパッチの節の数が合わない | **全部ファイル全体** | 解釈できない = 従来の挙動へ退避 |
//!
//! **行域を持てないものはファイル全体**が安全側で、これは行域が入る前の
//! 挙動そのものでもある (退化しても壊れない)。判定は
//! [`crate::region::within`] と [`crate::region::conflicts`] に任せ、
//! 行の算術をここに 2 つ目実装しない。
//!
//! ## 身元 (誰が「自分」か)
//!
//! git のコミットは**作業ツリー単位**の操作なので、ここでの身元も作業ツリー
//! (`git rev-parse --show-toplevel`) で決める。同じツリーの中で 2 体の
//! エージェントが動いていても index は 1 つしか無いため、git から見れば
//! 1 人である。危ないのは「worktree B が、worktree A の担当しているパスを
//! コミットする」形で、台帳のキーは元リポジトリなので**両者は同じ台帳を
//! 共有する** ([`crate::lease::Roots`])。

use std::path::{Path, PathBuf};
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};

use crate::i18n::{tr, trf};
use crate::lease::{self, Lease, Verdict};
use crate::region::{self, Span};

// ═══════════════════════════════════════════════════════════════════════════
//  0. 定数
// ═══════════════════════════════════════════════════════════════════════════

/// 生成したフックの目印。**これが冪等性の全部** — 2 回設置しても二重にならず、
/// `uninstall` は自分のものだけを消せる。版を上げたら旧版は「自分のもの」と
/// 見なされなくなるので、接頭辞までで判定する。
const MARKER_PREFIX: &str = "zaivern-guard:";
/// いま生成する版。
const MARKER: &str = "zaivern-guard:v1";

/// 関所を張るフック。index が確定していて、まだ履歴に入っていない地点。
pub const HOOKS: &[&str] = &["pre-commit", "pre-applypatch", "pre-merge-commit"];

/// 元から居たフックの退避先の接尾辞。
const PREV_SUFFIX: &str = ".zaivern-prev";

/// 拒否理由に並べるパスの上限 (端末を埋め尽くさない)。
const MAX_LISTED: usize = 20;

/// Git LFS のポインタファイルの 1 行目。**仕様で固定されている**
/// (<https://github.com/git-lfs/git-lfs/blob/main/docs/spec.md>)。
const LFS_POINTER_MARK: &str = "version https://git-lfs.github.com/spec/";

/// git がシンボリックリンクに使うモード。`--raw` の 1・2 列目に出る。
const SYMLINK_MODE: &str = "120000";

/// 終了コード。0 = 許可 / 1 = 拒否 / 2 = 使い方の誤り。
const EXIT_OK: i32 = 0;
const EXIT_DENY: i32 = 1;
const EXIT_USAGE: i32 = 2;

// ═══════════════════════════════════════════════════════════════════════════
//  1. エラー
// ═══════════════════════════════════════════════════════════════════════════

/// ガードの操作が失敗した理由。**文面はそのままユーザーに見せる**。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GuardError {
    /// git リポジトリではない / git が見つからない。
    Git(String),
    /// ファイル操作に失敗した。
    Io(String),
    /// 自分の実行ファイルの場所が判らない (フックに書けない)。
    NoExe(String),
    /// この `zai` はまだ `guard` サブコマンドを配線していない。
    ///
    /// **これを黙って通すと事故になる**: [`crate::cli::is_cli_subcommand`] に
    /// `"guard"` が無いと `zai guard check --staged` は「ワークスペース指定の
    /// GUI 起動」として扱われ、**コミットのたびにエディタが立ち上がって
    /// フックが返ってこない**。設置を止めて理由を出すのが正しい。
    NotWired,
}

impl std::fmt::Display for GuardError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GuardError::Git(e) => write!(f, "{}", trf("git を実行できません: {e}", &[("e", e.clone())])),
            GuardError::Io(e) => write!(f, "{}", trf("フックを書けません: {e}", &[("e", e.clone())])),
            GuardError::NoExe(e) => write!(
                f,
                "{}",
                trf(
                    "zai 自身の場所が判らないのでフックに書き込めません: {e}",
                    &[("e", e.clone())]
                )
            ),
            GuardError::NotWired => write!(
                f,
                "{}",
                tr("この zai はまだ `zai guard` サブコマンドを受け付けません。\n\
                    このまま設置すると、コミットのたびにエディタが起動してフックが返らなくなります。\n\
                    対処: 統合済みの zai へ更新してから、もう一度 `zai guard init` を実行してください")
            ),
        }
    }
}

fn io_err(e: std::io::Error) -> GuardError {
    GuardError::Io(e.to_string())
}

// ═══════════════════════════════════════════════════════════════════════════
//  2. 置き場の解決 (git に訊く — ここを直書きすると無言で効かなくなる)
// ═══════════════════════════════════════════════════════════════════════════

/// `start` を含むリポジトリの作業ツリーの頂点。
pub fn repo_root(start: &Path) -> Result<PathBuf, GuardError> {
    let out = crate::worktree::git_out(start, &["rev-parse", "--show-toplevel"])
        .map_err(GuardError::Git)?;
    if out.is_empty() {
        return Err(GuardError::Git(tr("git リポジトリではありません")));
    }
    Ok(PathBuf::from(out))
}

/// フックの置き場。**`.git/hooks` を直書きしない。**
///
/// `core.hooksPath` を設定したリポジトリ (husky / lefthook を使うと普通に
/// 起こる) と linked worktree では、実際に走るフックの場所が `.git/hooks`
/// ではない。git 自身に訊けば全部の場合で正しい場所が返る。
pub fn hooks_dir(repo: &Path) -> Result<PathBuf, GuardError> {
    let out = crate::worktree::git_out(repo, &["rev-parse", "--git-path", "hooks"])
        .map_err(GuardError::Git)?;
    if out.is_empty() {
        return Err(GuardError::Git(tr(
            "フックの置き場を git から取得できません",
        )));
    }
    let p = PathBuf::from(&out);
    // 相対で返るのが既定 (`.git/hooks` / 相対の core.hooksPath)。
    // git は「フックを走らせるディレクトリ = 作業ツリーの頂点」を基準にする。
    Ok(if p.is_absolute() { p } else { repo.join(p) })
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. フック本文 (純粋関数 — テーブルテストで固定する)
// ═══════════════════════════════════════════════════════════════════════════

/// POSIX sh の単一引用符クオート。`'` は `'\''` で閉じ直す。
///
/// **パスは環境ごとに全く違う**ので (日本語ユーザー名・空白・`$`・
/// Windows の `C:/Users/...`)、必ずここを通してから埋め込む。
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// フックの本文を作る。**bash 依存構文を書かない** (Windows の git 同梱 sh で動く)。
///
/// `exe` は設置時に解決した `zai` の絶対パス。PATH に無くても動かすために
/// 埋め込む (パスは [`current_exe_for_hook`] が `std::env::current_exe()`
/// から導出する — 直書きは 1 文字も無い)。
pub fn hook_script(exe: &str) -> String {
    let q = sh_quote(exe);
    format!(
        "#!/bin/sh\n\
         # {MARKER} — `zai guard init` が生成しました。手で編集しないでください。\n\
         #\n\
         # 目的: 他の担当が保有しているファイルを、**どのエージェントからでも**\n\
         #       コミットさせない。git を関所にするのでベンダーの協力が要りません。\n\
         # 解除: zai guard uninstall\n\
         #\n\
         # 約束:\n\
         #   * 元から居たフック ({PREV_SUFFIX}) を先に呼び、終了コードを尊重する\n\
         #     (husky / lefthook / pre-commit framework と共存するため置き換えない)\n\
         #   * zai が見つからない / 起動できないときは通す (fail-open)\n\
         #   * 止めるのは終了コード 1 のときだけ。それ以外はこちらの都合なので通す\n\
         \n\
         __zg_prev=\"$0{PREV_SUFFIX}\"\n\
         if [ -f \"$__zg_prev\" ]; then\n\
         \x20 if [ -x \"$__zg_prev\" ]; then\n\
         \x20   \"$__zg_prev\" \"$@\"\n\
         \x20 else\n\
         \x20   sh \"$__zg_prev\" \"$@\"\n\
         \x20 fi\n\
         \x20 __zg_st=$?\n\
         \x20 if [ \"$__zg_st\" -ne 0 ]; then\n\
         \x20   exit \"$__zg_st\"\n\
         \x20 fi\n\
         fi\n\
         \n\
         __zg_exe={q}\n\
         if [ ! -f \"$__zg_exe\" ]; then\n\
         \x20 exit 0\n\
         fi\n\
         \"$__zg_exe\" guard check --staged\n\
         __zg_st=$?\n\
         if [ \"$__zg_st\" -eq 1 ]; then\n\
         \x20 exit 1\n\
         fi\n\
         exit 0\n"
    )
}

/// この中身は zaivern が生成したものか。**版が違っても自分のものと判る**
/// (旧版を他人のフックと見なして退避すると、退避先が自分のゴミで埋まる)。
pub fn is_ours(text: &str) -> bool {
    text.contains(MARKER_PREFIX)
}

/// フックに書き込む `zai` の絶対パス。
///
/// * `current_exe()` から導出する (ハードコードしない)
/// * symlink は解決する (`~/.local/bin/zai` が実体を指していても動く)
/// * **Windows だけ** `\` を `/` へ寄せる。sh の二重引用符の中で `\` は
///   場合により escape として食われるため。unix ではファイル名に `\` が
///   入り得るので**絶対に置換しない**
pub fn current_exe_for_hook() -> Result<String, GuardError> {
    let p = std::env::current_exe().map_err(|e| GuardError::NoExe(e.to_string()))?;
    let p = p.canonicalize().unwrap_or(p);
    Ok(exe_text(&p))
}

/// [`current_exe_for_hook`] の純粋部分 (OS 差分をテストから両方叩けるように分ける)。
fn exe_text(p: &Path) -> String {
    let s = p.to_string_lossy().to_string();
    if cfg!(windows) {
        s.replace('\\', "/")
    } else {
        s
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 設置 / 解除
// ═══════════════════════════════════════════════════════════════════════════

/// [`install`] の結果。**何をしたかを全部返す** (「入れました」だけだと、
/// 既存フックを連鎖したのか触らなかったのかが判らない)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Installed {
    /// 実際に書いた場所。
    pub hooks_dir: PathBuf,
    /// 新規に設置したフック名。
    pub fresh: Vec<String>,
    /// 既に自分のものだったので貼り直したフック名 (冪等)。
    pub refreshed: Vec<String>,
    /// 元のフックを退避して連鎖したフック名。
    pub chained: Vec<String>,
    /// 他人のフックが居て、退避先も埋まっているので**触らなかった**フック名。
    /// ここを上書きすると、ユーザーのフックを黙って消すことになる。
    pub blocked: Vec<String>,
}

/// [`uninstall`] の結果。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub hooks_dir: PathBuf,
    /// 消した自分のフック。
    pub removed: Vec<String>,
    /// 退避してあった元のフックを戻したもの。
    pub restored: Vec<String>,
    /// 自分のものではないので触らなかったもの。
    pub kept: Vec<String>,
}

/// このリポジトリへガードを設置する。
///
/// `zai` の場所は `current_exe()` から導出して**絶対パスで埋め込む**
/// (PATH に無くても動く)。
pub fn install(repo: &Path) -> Result<Installed, GuardError> {
    // 配線されていない zai を埋め込むと、フックが GUI を起動して
    // コミットが返ってこなくなる。**設置前に必ず確かめる。**
    if !crate::cli::is_cli_subcommand("guard") {
        return Err(GuardError::NotWired);
    }
    install_with(repo, &current_exe_for_hook()?)
}

/// `zai` の場所を明示する [`install`]。
///
/// 別の場所へ入れた `zai` を指したいとき (と、テスト) のための入口。
/// 呼び出し側が場所の正しさに責任を持つので、配線の確認はしない。
pub fn install_with(repo: &Path, exe: &str) -> Result<Installed, GuardError> {
    let dir = hooks_dir(repo)?;
    std::fs::create_dir_all(&dir).map_err(io_err)?;
    let script = hook_script(exe);
    let mut out = Installed {
        hooks_dir: dir.clone(),
        ..Default::default()
    };
    for name in HOOKS {
        let path = dir.join(name);
        let prev = dir.join(format!("{name}{PREV_SUFFIX}"));
        // 非 UTF-8 のフック (コンパイル済みバイナリ) もあり得るので lossy で読む。
        match std::fs::read(&path) {
            Ok(bytes) => {
                let cur = String::from_utf8_lossy(&bytes);
                if is_ours(&cur) {
                    write_hook(&path, &script)?;
                    out.refreshed.push((*name).to_string());
                } else if prev.exists() {
                    // 退避先が埋まっている = 自分のフックが他人に置き換えられた形。
                    // **上書きするとユーザーのフックを黙って消す**ので触らない。
                    out.blocked.push((*name).to_string());
                } else {
                    std::fs::rename(&path, &prev).map_err(io_err)?;
                    write_hook(&path, &script)?;
                    out.chained.push((*name).to_string());
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                write_hook(&path, &script)?;
                out.fresh.push((*name).to_string());
            }
            Err(e) => return Err(io_err(e)),
        }
    }
    Ok(out)
}

/// 自分のフックだけを消し、退避した元のフックを戻す。
pub fn uninstall(repo: &Path) -> Result<Removed, GuardError> {
    let dir = hooks_dir(repo)?;
    let mut out = Removed {
        hooks_dir: dir.clone(),
        ..Default::default()
    };
    for name in HOOKS {
        let path = dir.join(name);
        let prev = dir.join(format!("{name}{PREV_SUFFIX}"));
        let ours = match std::fs::read(&path) {
            Ok(bytes) => is_ours(&String::from_utf8_lossy(&bytes)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
            Err(e) => return Err(io_err(e)),
        };
        if path.exists() && !ours {
            // 他人のフック。**触らない。**
            out.kept.push((*name).to_string());
            continue;
        }
        if ours {
            std::fs::remove_file(&path).map_err(io_err)?;
            out.removed.push((*name).to_string());
        }
        if prev.exists() {
            std::fs::rename(&prev, &path).map_err(io_err)?;
            out.restored.push((*name).to_string());
        }
    }
    Ok(out)
}

/// フック 1 本を書く。**改行は LF 固定**。
///
/// CRLF で書くと Windows の git 同梱 sh が `\r` をコマンド名に含めてしまい
/// (`': command not found'`)、フックが常に失敗する。
fn write_hook(path: &Path, script: &str) -> Result<(), GuardError> {
    std::fs::write(path, script.as_bytes()).map_err(io_err)?;
    set_executable(path)
}

/// 実行権を付ける。**両方の OS を実装する。**
///
/// 実行権が無いフックを git は**黙って無視する** (エラーも出ない)。
/// ここを落とすと「設置したのに 1 度も走らない」という最悪の壊れ方をする。
#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), GuardError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).map_err(io_err)
}

/// Windows には実行ビットが無い。git for Windows は同梱の sh でフックを
/// 起動するので、権限も拡張子も要らない (**何もしないのが正解**)。
#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), GuardError> {
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. 判定 (フックの本番経路)
// ═══════════════════════════════════════════════════════════════════════════

/// ファイル全体を指す行域。`None` と使い分けずに**この 1 本へ寄せる**。
///
/// [`region::conflicts`] は `Some(1..EOF)` を `None` と同じに扱い
/// ([`region::spans_too_close`] が `end == EOF` を必ず重なりとする)、
/// [`region::within`] も `1..EOF` を持っていれば何を触っても収まると答える。
/// つまり「全体」を `Option` の場合分けで持ち回る必要が無い。
const WHOLE: Span = Span {
    start: 1,
    end: Span::EOF,
};

/// git に投げる**唯一の**問い合わせ。
///
/// * `--raw -z` — パスと状態 (`A`/`M`/`D`/`R`/`C`/`T`)。`-z` なので
///   空白・日本語・改行入りのパスでも引用符に包まれない
/// * `--unified=0 -p` — 触れた行域。文脈行が 0 なので `@@` が変更そのもの
/// * `--no-color` — `color.diff = always` の人でも `@@ ` の照合が壊れない
/// * `--no-ext-diff` — `GIT_EXTERNAL_DIFF` が出力を丸ごと差し替えるのを止める
/// * `--no-textconv` — textconv を通すと**行番号が別物になる**
///   (`.gitattributes` の `diff=<driver>` は既定で `git diff` に効く)
const DIFF_ARGS: &[&str] = &[
    "diff",
    "--cached",
    "--raw",
    "-z",
    "--unified=0",
    "-p",
    "--no-color",
    "--no-ext-diff",
    "--no-textconv",
];

/// **なぜ行域を持てなかったか。** 黙って劣化させないための理由。
///
/// 「ファイル全体の担当として判定した」は安全側だが、**そうなったことが
/// 利用者に伝わらないと、なぜ止められたのか判らない**。行域をずらせば通る
/// のか、そもそも行域が効かないファイルなのかで打つ手が正反対になる。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WholeWhy {
    /// 変更の**形**が行で説明できない (新規 / 削除 / リネーム / 複製 /
    /// 型・モード変更 / シンボリックリンク)。これは劣化ではなく定義。
    Shape,
    /// 内容の変更なのに**ハンクが 1 つも出なかった** = 二値 / `-diff` 属性 /
    /// LFS ポインタ / `core.bigFileThreshold` 超えの巨大ファイル。
    /// **これが「黙って劣化した」場合**で、理由を文面に出す。
    NoHunks,
    /// 記録の数とパッチの節の数が合わない = 出力を解釈できなかった。
    /// 行域が入る前の挙動へ丸ごと退避している。
    Unaligned,
    /// **Git LFS のポインタ。** 見えている 3 行 (`version` / `oid` / `size`) は
    /// 中身ではなく所在で、実体は別の場所にある。行番号に意味が無いので
    /// 行域では判定できない。
    ///
    /// **`.gitattributes` を引かずに本文から判る**ので、通す経路の git 呼び出しを
    /// 1 回のまま保てる (`filter=lfs` が設定されているかは、実は関係ない —
    /// 手で置かれたポインタでも中身は同じ意味を持つ)。
    Lfs,
}

/// ステージされた 1 件が「どこを触ったか」。
#[derive(Clone, Debug, PartialEq, Eq)]
enum Touched {
    /// 行では説明できない変更。ファイル全体を触ったものとして扱う = **安全側**。
    Whole(WholeWhy),
    /// 内容の変更。新しい側の行番号 (1 始まり・両端含む・昇順・重なり無し)。
    Lines(Vec<Span>),
}

impl Touched {
    /// 判定に渡す形。**空にはならない** (空だと `within` が無条件に真を返し、
    /// 何も持っていない人が何でも通せてしまう)。
    fn spans(&self) -> &[Span] {
        match self {
            Touched::Whole(_) => std::slice::from_ref(&WHOLE),
            Touched::Lines(v) => v,
        }
    }

    /// 「行域を使えなかったので全体にした」なら、その理由。
    /// 形が行で説明できないもの ([`WholeWhy::Shape`]) は劣化ではないので `None`。
    fn degraded(&self) -> Option<WholeWhy> {
        match self {
            Touched::Whole(WholeWhy::Shape) | Touched::Lines(_) => None,
            Touched::Whole(w) => Some(*w),
        }
    }
}

/// ステージされた 1 件。
#[derive(Clone, Debug, PartialEq, Eq)]
struct StagedChange {
    /// 作業ツリーの頂点からの相対パス。
    path: String,
    touched: Touched,
}

/// `--raw -z` の 1 記録 (パスは R/C なら 2 つ)。
struct Rec {
    paths: Vec<String>,
    /// 行域まで解像度を上げてはいけない形か。
    whole: bool,
}

/// [`DIFF_ARGS`] の生出力を解釈する。**git を呼ばない純粋関数**
/// (テストで極端な入力を固定でき、所要時間もここだけで測れる)。
///
/// ## 形
///
/// ```text
/// :100644 100644 <sha> <sha> M\0src/a.rs\0:000000 100644 <sha> <sha> A\0new.rs\0\0
/// diff --git a/src/a.rs b/src/a.rs
/// @@ -30 +30 @@ ...
/// ```
///
/// 前半 (NUL 区切り) が記録、後半がパッチ。**n 番目の `diff --git` は
/// n 番目の記録**なので、パッチ側からパスを読み直さない。
/// これは事故を 1 つ構造的に消す: パッチの `--- a/x` / `+++ b/x` は
/// 「`--` で始まる行が削除された」ときの本文と**見分けが付かない**し、
/// `diff.mnemonicPrefix` / `core.quotePath` で表記も変わる。
///
/// 数が合わなければ**全部ファイル全体**へ倒す (行域が入る前の挙動)。
fn parse_staged(out: &str) -> Vec<StagedChange> {
    let mut recs: Vec<Rec> = Vec::new();
    let mut patch = String::new();
    let mut it = out.split('\0');
    while let Some(tok) = it.next() {
        let Some(body) = tok.strip_prefix(':') else {
            // 記録の終わり。ここから先はパッチ本文で、NUL は含まれない
            // (NUL を含むファイルは git が二値と見なしてハンクを出さない)。
            //
            // **記録とパッチの間には NUL が 1 つ余分に入る** (最後のパスの
            // 終端 + 区切り)。空の切片をそのまま繋ぐと 1 本目の
            // `diff --git` の行頭に NUL が付き、**最初のファイルの行域が
            // 丸ごと落ちる**。新規ファイルばかりのテストでは全体扱いに
            // 退避して緑のままだったので、ここは実リポジトリの
            // 「行を書き換えた」テストでしか見つからない。
            let mut rest: Vec<&str> = Vec::new();
            if !tok.is_empty() {
                rest.push(tok);
            }
            rest.extend(it);
            patch = rest.join("\0");
            break;
        };
        // ":<旧モード> <新モード> <旧sha> <新sha> <状態>"
        let mut f = body.split(' ');
        let old_mode = f.next().unwrap_or_default();
        let new_mode = f.next().unwrap_or_default();
        let (_, _) = (f.next(), f.next()); // sha は使わない
        let status = f.next().unwrap_or_default();
        let n = usize::from(status.starts_with('R') || status.starts_with('C')) + 1;
        let mut paths = Vec::with_capacity(n);
        for _ in 0..n {
            match it.next() {
                Some(p) if !p.is_empty() => paths.push(p.to_string()),
                _ => break,
            }
        }
        // 行域で説明できるのは「内容だけが変わった」形に限る。
        // モードが動いていたら、たとえ本文も変わっていても全体扱い。
        //
        // **シンボリックリンク (mode 120000) は行を持たない。** 中身は行き先の
        // パス 1 本で、git は 1 行のテキストとして差分を出す (`@@ -1 +1 @@`)。
        // これを行域として受けると「1 行目だけ触った」に見えるが、実際に
        // 起きるのは**そのリンクを通る全てのパスの意味が変わる**ことなので、
        // 行の話ではない。fail-closed でファイル全体へ倒す。
        let link = old_mode == SYMLINK_MODE || new_mode == SYMLINK_MODE;
        recs.push(Rec {
            whole: link || !status.starts_with('M') || old_mode != new_mode,
            paths,
        });
    }

    // パッチを節へ切り分けて、節ごとにハンクの行域を集める。
    let mut sections: Vec<Vec<Span>> = Vec::new();
    // 節ごとに「LFS のポインタだったか」。
    let mut lfs: Vec<bool> = Vec::new();
    for line in patch.split('\n') {
        // CRLF のチェックアウトでも同じ結果になること。
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.starts_with("diff --git ") {
            sections.push(Vec::new());
            lfs.push(false);
        } else if lfs_marked(line) {
            if let Some(f) = lfs.last_mut() {
                *f = true;
            }
        }
        if line.starts_with("@@ ") {
            // `-U0` では本文行が必ず `-` / `+` / `\` で始まるので、
            // 行頭の `@@ ` と `diff --git ` は本文と衝突しない。
            if let (Some(cur), Some(sp)) = (sections.last_mut(), hunk_span(line)) {
                push_span(cur, sp);
            }
        }
    }

    let aligned = sections.len() == recs.len();
    let mut out_v: Vec<StagedChange> = Vec::new();
    for (i, r) in recs.iter().enumerate() {
        let spans = if aligned {
            sections.get(i).cloned().unwrap_or_default()
        } else {
            Vec::new()
        };
        let touched = if r.whole {
            Touched::Whole(WholeWhy::Shape)
        } else if !aligned {
            Touched::Whole(WholeWhy::Unaligned)
        } else if lfs.get(i).copied().unwrap_or(false) {
            Touched::Whole(WholeWhy::Lfs)
        } else if spans.is_empty() {
            Touched::Whole(WholeWhy::NoHunks)
        } else {
            Touched::Lines(spans)
        };
        for p in &r.paths {
            out_v.push(StagedChange {
                path: p.clone(),
                touched: touched.clone(),
            });
        }
    }
    out_v
}

/// この行から「LFS のポインタだ」と判るか。
///
/// ポインタは 3 行 (`version` / `oid` / `size`) しかないので、変更は必ず
/// **本文に `version` 行が出る**か、**ハンク見出しの文脈として出る**
/// (`@@ -2,2 +2,2 @@ version https://…`)。`-U0` では文脈行が本文に出ないので、
/// 見出しの後ろまで見ないと `oid` / `size` だけの変更を取りこぼす
/// (実際にこれで検査が落ちた)。
///
/// 見誤って「ポインタだ」と答えると**ファイル全体の担当として突き合わせる** =
/// 過剰に止める方向なので、外しても衝突は生まない (fail-closed)。
fn lfs_marked(line: &str) -> bool {
    if let Some(rest) = line.strip_prefix("@@ ") {
        return rest
            .split_once("@@ ")
            .is_some_and(|(_, ctx)| ctx.starts_with(LFS_POINTER_MARK));
    }
    line.strip_prefix('+')
        .or_else(|| line.strip_prefix('-'))
        .is_some_and(|b| b.starts_with(LFS_POINTER_MARK))
}

/// `@@ -a,b +c,d @@ …` の**新しい側**だけを行域にする。
///
/// * `+c,d` (`d >= 1`) → `c..c+d-1`
/// * `+c,0` (純粋な削除) → **削除点の直後の行** `c+1`。
///   [`region::touched_spans`] と同じ約束にしてある (2 実装を持たない)
/// * `+0,…` は「ファイルが消えた」形で、そこは状態 `D` が全体扱いにするが、
///   数字だけ来ても 1 行目へ丸めて壊れないようにする
fn hunk_span(line: &str) -> Option<Span> {
    let tok = line.split_whitespace().find(|t| t.starts_with('+'))?;
    let (a, b) = match tok[1..].split_once(',') {
        Some((a, b)) => (a, Some(b)),
        None => (&tok[1..], None),
    };
    let start: u32 = a.parse().ok()?;
    let count: u32 = match b {
        Some(b) => b.parse().ok()?,
        None => 1,
    };
    if count == 0 {
        let at = start.saturating_add(1);
        return Some(Span::line(at));
    }
    let start = start.max(1);
    Some(Span {
        start,
        end: start.saturating_add(count - 1),
    })
}

/// 昇順で来る行域を、[`region::SAFE_BAND`] 以内なら 1 つに畳んで積む。
///
/// 畳むのは [`region::touched_spans`] と揃えるため。畳まないと
/// 「別々の小さな域」に見えて、`within` の判定が本来より甘くなる。
fn push_span(v: &mut Vec<Span>, s: Span) {
    match v.last_mut() {
        Some(last) if s.start.saturating_sub(last.end) <= region::SAFE_BAND => {
            last.end = last.end.max(s.end);
        }
        _ => v.push(s),
    }
}

/// ステージ済みの変更を、触れた行域まで込みで返す。**git は 1 回しか呼ばない**。
///
/// パスは作業ツリーの頂点からの相対で返る (`-C <頂点>` で走らせるので
/// `diff.relative = true` が設定してあっても結果は変わらない)。
/// `-z` なので空白・日本語・改行入りのパスでも引用符に包まれない。
///
/// **リネームは旧パスと新パスの両方**を返す (`--name-only` は新パスしか
/// 出さない)。ガードは旧パスの担当も守らなければならないので、ここが正しい。
///
/// かつては「パスだけを返す `staged_paths`」を別に持っていたが、行域まで
/// 見るようになって**呼ぶ人が居なくなった**ので消した (`never used` は
/// 「作ったのに繋いでいない」の検出器で、鈍らせない)。
fn staged_changes(repo: &Path) -> Result<Vec<StagedChange>, GuardError> {
    let out = crate::worktree::git_out(repo, DIFF_ARGS).map_err(GuardError::Git)?;
    Ok(parse_staged(&out))
}

/// 台帳を突き合わせて判定する。**台帳が無ければ何もしない** (未導入者のコストはゼロ)。
pub fn check_staged(repo: &Path) -> Verdict {
    check_staged_in(repo, &lease::store_dir())
}

/// 台帳の置き場を明示する [`check_staged`] (テストが実 `~/.zaivern` を触らないため)。
///
/// **内部エラーは全部 fail-open。** 台帳が壊れている / git がおかしい、で
/// ユーザーのコミットを止めると、ユーザーは機能ごと切る
/// (切られた機能の保証はゼロ)。`lease::gate` と同じ向き。
pub fn check_staged_in(repo: &Path, ledger_dir: &Path) -> Verdict {
    let roots = lease::roots_of(repo);
    let store = lease::store_path_in(ledger_dir, &roots.key);
    // ここが「使っていない人が払う全コスト」= stat 1 回。
    if !lease::enabled(&store) {
        return Verdict::Allow;
    }
    let Ok(st) = lease::read_store(&store) else {
        return Verdict::Allow; // 台帳が壊れている = こちらの都合
    };
    let Ok(changes) = staged_changes(repo) else {
        return Verdict::Allow; // git が答えない = こちらの都合
    };
    if changes.is_empty() {
        return Verdict::Allow;
    }
    let now = lease::now_secs();
    let alive = |p: u32| crate::instances::pid_alive(p);
    // 錨を数えるための本文。**交錯が見つかったパスでしか呼ばれない**ので、
    // 通す経路 (圧倒的多数) の I/O は増えない。
    let read = |rel: &str| lease::read_capped(&roots.tree.join(rel), &roots.tree);
    let hits = collisions(&st, &roots.tree, &changes, now, &alive, &read);
    if hits.is_empty() {
        return Verdict::Allow; // ここまでで git は 1 回しか呼んでいない
    }
    // **止める直前にだけ** `.gitattributes` を引く。通す経路のコストを
    // 増やさずに、(1) `merge=union` は競合しないので落とし
    // (2) 行域が使えなかった理由を文面に足せる。
    let hits = refine(repo, hits);
    if hits.is_empty() {
        return Verdict::Allow;
    }
    Verdict::Deny(deny_text(&hits, now))
}

/// **1 本のパスへの書き込みが通るか。** `zai guard check --path <ファイル>`。
///
/// コミット時のガード ([`check_staged`]) では**構造的に見えない**穴が 1 つある:
/// リポジトリの中のシンボリックリンクを通って**外**へ書く経路である。
/// git は `beyond a symbolic link` としてそのパスを index に入れないので、
/// フックはその書き込みを一生見ない。にもかかわらず、リンクの先が
/// **別の作業ツリー**なら、それは同じ台帳を共有する誰かのファイルである
/// ([`lease::Roots`] はリンクされた worktree を 1 つの台帳へ寄せる)。
///
/// そこで、書き込む前に問い合わせられる入口をガード側に置く。
/// **行番号を渡せないので、答えはファイル全体として出す** (安全側)。
pub fn check_path(repo: &Path, target: &Path) -> Verdict {
    check_path_in(repo, &lease::store_dir(), target)
}

/// 台帳の置き場を明示する [`check_path`] (テストが実 `~/.zaivern` を触らないため)。
pub fn check_path_in(repo: &Path, ledger_dir: &Path, target: &Path) -> Verdict {
    let roots = lease::roots_of(repo);
    let store = lease::store_path_in(ledger_dir, &roots.key);
    if !lease::enabled(&store) {
        return Verdict::Allow;
    }
    let Ok(st) = lease::read_store(&store) else {
        return Verdict::Allow;
    };
    let abs = if target.is_absolute() {
        target.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| roots.tree.clone())
            .join(target)
    };
    let Some(rel) = rel_under_root(&roots.tree, &crate::pathx::lexical(&abs)) else {
        return Verdict::Allow; // 綴りからしてリポジトリの外 = 関知しない
    };
    if rel.is_empty() {
        return Verdict::Allow;
    }
    let mut links = crate::pathx::LinkResolver::new(&roots.tree);
    let real = match links.resolve(&rel) {
        crate::pathx::Resolved::Inside(r) => r,
        crate::pathx::Resolved::Outside => return Verdict::Deny(escape_text(&rel)),
        // 輪 / 読めない = 判定できない。**判定できないものは通さない。**
        crate::pathx::Resolved::Unknown => return Verdict::Deny(unresolved_text(&rel)),
    };
    // 字句と実体の**両方**で突き合わせる (どちらかに担当が居れば止める)。
    let mut changes = vec![StagedChange {
        path: rel.clone(),
        touched: Touched::Whole(WholeWhy::Shape),
    }];
    if real != rel && !real.is_empty() {
        changes.push(StagedChange {
            path: real,
            touched: Touched::Whole(WholeWhy::Shape),
        });
    }
    let now = lease::now_secs();
    let alive = |p: u32| crate::instances::pid_alive(p);
    let read = |rel: &str| lease::read_capped(&roots.tree.join(rel), &roots.tree);
    let hits = collisions(&st, &roots.tree, &changes, now, &alive, &read);
    if hits.is_empty() {
        Verdict::Allow
    } else {
        Verdict::Deny(deny_text(&hits, now))
    }
}

/// `abs` が作業ツリーの中を指す綴りなら、その相対パス (正規形)。
///
/// **リポジトリの頂点より上だけリンクを解き、頂点より下は解かない。**
/// 上を解くのは、`/var` → `/private/var` (macOS) のようにリポジトリ自体へ
/// 至る道がリンクでも同じツリーだと判るため。下を解かないのは、
/// **解いてしまうと `repo/out -> /外` が「リポジトリの外 = 関知しない」に
/// 落ちて、いま塞ごうとしている抜け道をそのまま素通りさせる**ため。
/// 下側の解決は [`crate::pathx::LinkResolver`] が別に行い、
/// 「中の綴りで外を指している」を [`escape_text`] で止める。
fn rel_under_root(tree: &Path, abs: &Path) -> Option<String> {
    let root = crate::pathx::canonical(tree);
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = abs.to_path_buf();
    loop {
        if crate::pathx::canonical(&cur) == root {
            let mut p = PathBuf::new();
            for r in rest.iter().rev() {
                p.push(r);
            }
            return Some(lease::normalize_path(&p.to_string_lossy()));
        }
        rest.push(cur.file_name()?.to_os_string());
        let parent = cur.parent()?;
        if parent.as_os_str().is_empty() {
            return None;
        }
        cur = parent.to_path_buf();
    }
}

/// リポジトリ内のリンクを通って外へ書こうとしたときの文面。
fn escape_text(rel: &str) -> String {
    trf(
        "書き込みを止めました (Zaivern Code のガード)。\n\
         {rel} はリポジトリの中にある綴りですが、シンボリックリンクを通って\n\
         **リポジトリの外**を指しています。\n\
         \n\
         この経路は git が index に入れない (`beyond a symbolic link`) ので、\n\
         コミット時のガードでは一生見えません。リンクの先が別の作業ツリーなら、\n\
         そこは同じ台帳を共有する誰かの担当です。\n\
         対処:\n\
         \x20 (1) リンクを通さず、実体のパスを直に指定して書く\n\
         \x20 (2) 書き先が本当にリポジトリの外で良いなら、リポジトリの外の絶対パスで書く",
        &[("rel", rel.to_string())],
    )
}

/// リンクが輪になっている / 読めないときの文面。
fn unresolved_text(rel: &str) -> String {
    trf(
        "書き込みを止めました (Zaivern Code のガード)。\n\
         {rel} のシンボリックリンクを解けませんでした (輪になっている / 読めない)。\n\
         **どのファイルへ書くのか判らないものは通しません。**\n\
         対処: リンクを直すか、実体のパスを直に指定して書いてください",
        &[("rel", rel.to_string())],
    )
}

/// 拒否 1 件。**「どのファイルが」では足りない** — 行域を持てるようになった
/// 以上、「自分のどの行が、誰のどの行と重なったか」まで出さないと、
/// ユーザーは何をずらせば通るのか判らない。
#[derive(Clone, Debug)]
struct Hit {
    /// 触った側 (`src/app.rs#L205-206` / 全体なら `src/app.rs`)。
    touched: String,
    /// 重なった相手の担当。台帳の表記をそのまま出す (`zai lease list` と揃う)。
    owned: String,
    lease: Lease,
    /// ステージ済みのパス (作業ツリー相対)。`.gitattributes` を引くのに使う。
    path: String,
    /// 行域が使えず全体扱いになったなら、その理由。**文面に出す。**
    degraded: Option<WholeWhy>,
    /// `.gitattributes` から判った具体的な理由 (`-diff` / `filter=lfs` 等)。
    /// 拒否の直前にだけ引く (通す経路の git 呼び出しは 1 回のまま)。
    attr: String,
    /// **交錯**で止めたなら、その理由 ([`lease::interleave_reason`])。
    ///
    /// 重なり・近すぎ (`None`) と**同じ顔をさせない**ために分けてある。
    /// 交錯は離しても直らないので、「{band} 行以上離せば同時に書けます」と
    /// いう既定の案内をそのまま出すと嘘になる。
    bracketed: Option<String>,
}

/// **点検の芯。** I/O を持たない純粋関数。
///
/// 1 パスにつき **1 件**だけ報告する (最初に重なった相手で打ち切る)。
/// 全部挙げると 1 万ハンクのコミットで文面が爆発するうえ、直す順番は
/// どのみち 1 つずつだからである。
///
/// ## 3 段で決める
///
/// 1. **自分の担当に収まっていれば通す** ([`region::within`])。
///    64 体が自分の行域だけを直してコミットする通常経路がここで終わる
/// 2. 収まっていなければ、**他人の担当と重なるか**を見る
///    ([`region::conflicts`] — 安全帯 [`region::SAFE_BAND`] 込み)
/// 3. 誰の担当でもない行なら通す。[`crate::lease::decide_spans`] (書き込み側の
///    関所) と同じ向きで、ここだけ厳しくすると
///    「書けたのにコミットできない」が生まれる
///
/// ## 決定性
///
/// 台帳の並び順とパッチの順にしか依らない。`HashMap` を 1 つも使わない。
///
/// ## 4 段目 — 交錯 (帯だけでは足りない唯一の形)
///
/// 3 段目までは**組ごと**の判定で、それは今も正しい。足りないのは
/// 「全部の組が帯を満たす ⇒ まとめてマージしても綺麗に通る」という推論の
/// ほうで、触った行が他人の域を**上下から挟んでいる**と、反復的な本文では
/// 帯を何行取っても `git merge` が衝突する
/// ([`crate::region::anchor_lines`] に実測)。
///
/// `text_of` は「そのパスの本文」を返す (錨を数えるのに要る)。
/// **本当に交錯しているときにしか呼ばれない**ので、互いに素な配り方
/// (この関所が普段見る形) では 1 バイトも読まない。`None` を返したら
/// fail-closed (断る) — 理由は [`lease::interleave_ok`] にある。
fn collisions(
    st: &lease::Store,
    tree: &Path,
    changes: &[StagedChange],
    now: u64,
    alive: &dyn Fn(u32) -> bool,
    text_of: &dyn Fn(&str) -> Option<String>,
) -> Vec<Hit> {
    let pats = prepare(st, tree, now, alive);
    let mut hits: Vec<Hit> = Vec::new();
    let mut mine: Vec<Span> = Vec::new();
    let mut others: Vec<&Pat<'_>> = Vec::new();
    for ch in changes {
        let touched = ch.touched.spans();
        let key = lease::normalize_path(&ch.path);
        // このパスに掛かる担当を、自分のものと他人のものへ分ける。
        // 1 つのリースが同じパスの**複数の域**を持てるので、パターンは全部見る。
        mine.clear();
        others.clear();
        for p in &pats {
            if !p.matches(&key, &ch.path) {
                continue;
            }
            if p.mine {
                mine.push(p.span);
            } else {
                others.push(p);
            }
        }
        if region::within(&mine, touched) {
            continue; // 自分の担当の中だけを直した = いちばん多い経路
        }
        if let Some(mut hit) = first_collision(&ch.path, touched, &others) {
            hit.degraded = ch.touched.degraded();
            hits.push(hit);
            continue;
        }
        // 4 段目。**帯を全部通ったあとにだけ**見る。
        if let Some(mut hit) = interleave_collision(&ch.path, touched, &others, text_of) {
            hit.degraded = ch.touched.degraded();
            hits.push(hit);
        }
    }
    hits
}

/// 触った行が他人の担当を**挟んで**いないか (持ち主ごとにまとめて判定)。
///
/// 交錯は「A の域が B の 2 つの域に挟まれている」という**集合の性質**なので、
/// [`first_collision`] のような 1 組ずつの走査では定義できない。持ち主ごとに
/// 域をまとめてから [`lease::interleave_ok`] へ渡す。
///
/// 比べるのは**触った行**であって「自分が持っている行」ではない。
/// 100 行持っていても 1 行しか直していないなら、マージで動くのはその 1 行
/// だけである (持っている域で判定すると、触ってもいない行のせいで断る)。
///
/// 本文は**交錯が見つかったパスにつき 1 回だけ**読む。
fn interleave_collision(
    rel: &str,
    touched: &[Span],
    others: &[&Pat<'_>],
    text_of: &dyn Fn(&str) -> Option<String>,
) -> Option<Hit> {
    // 持ち主ごとに域をまとめる。**台帳の並び順のまま**進めるので決定的。
    let mut seen: Vec<(&Lease, Vec<Span>, &str)> = Vec::new();
    for p in others {
        match seen.iter_mut().find(|(l, _, _)| std::ptr::eq(*l, p.lease)) {
            Some(e) => e.1.push(p.span),
            None => seen.push((p.lease, vec![p.span], p.raw)),
        }
    }
    // 本文は「交錯している持ち主が居る」と分かってから 1 回だけ読む。
    let mut text: Option<Option<String>> = None;
    for (lease, spans, raw) in &seen {
        if !region::needs_wall(touched, spans) {
            continue;
        }
        let t = text.get_or_insert_with(|| text_of(rel));
        if lease::interleave_ok(t.as_deref(), touched, spans) {
            continue;
        }
        return Some(Hit {
            touched: region::hull(touched).map_or_else(|| rel.to_string(), |h| label(rel, h)),
            owned: (*raw).to_string(),
            lease: (*lease).clone(),
            path: rel.to_string(),
            degraded: None,
            attr: String::new(),
            bracketed: Some(lease::interleave_reason(t.is_some())),
        });
    }
    None
}

/// 台帳の 1 パターンを、**1 パスあたり文字列比較 1 回**で捌ける形へ畳んだもの。
struct Pat<'a> {
    lease: &'a Lease,
    /// 台帳の表記そのもの (文面に出す)。
    raw: &'a str,
    /// 正規形のパス部分。`*` / `?` を含まないなら、照合は等値比較で足りる。
    /// glob や記号指定は `None` で、[`lease::covers`] へ回す。
    plain: Option<String>,
    /// **シンボリックリンクを解いた綴り** (字句の綴りと違うときだけ `Some`)。
    ///
    /// 台帳をリンク越しの綴りで書いた人 (`lib/app.rs`、`lib -> src`) と、
    /// git が報告する実体の綴り (`src/app.rs`) は字句では別物なので、
    /// これが無いと**同じ行を 2 人が持てる**。字句と実体の**両方**で照合し、
    /// どちらかが当たれば当たり (減らさないので TOCTOU で緩まない)。
    alias: Option<String>,
    span: Span,
    mine: bool,
}

impl Pat<'_> {
    /// `key` は [`lease::normalize_path`] を通したステージ済みパス
    /// (1 パスにつき 1 回だけ作る)。`raw_path` は glob へ回すときの原文。
    fn matches(&self, key: &str, raw_path: &str) -> bool {
        match &self.plain {
            Some(p) => p == key || self.alias.as_deref() == Some(key),
            None => lease::covers(self.raw, raw_path),
        }
    }
}

/// **台帳の前処理。ここが速さの全部。**
///
/// 素直に「パスごとにリースを全部見る」と、`Lease::active` /
/// [`holder_is_me`] / [`lease::covers`] が **パス数 × リース数**回走る。
/// [`holder_is_me`] は [`Path::canonicalize`] を通る = **システムコール**なので、
/// 400 パス × 200 リースで実測 **5.0 秒**掛かっていた (フックがこれを払うと
/// 人が待つ)。身元と行域はパスに依らないので、リースごとに 1 回で済む。
///
/// パス照合も同じ理由で畳む。[`lease::covers`] はパターンとパスの両方を
/// 毎回セグメントへ割り付け直す (`Vec<String>` の確保 + `**` の DP) が、
/// 実際の台帳は `src/app.rs#L100-160` のような**具体パスが圧倒的多数**で、
/// そこは正規形どうしの等値比較で答えが変わらない
/// ([`seg_covers`][lease::covers] は `*` / `?` が無ければ全セグメント一致を
/// 要求するため)。**これが正しいことは
/// `速い照合と lease::covers は同じ答えを出す` が差分テストで固定する。**
///
/// 実測 (macOS / debug): 400 パス × 200 リースの判定が
/// **5.04 秒 → 7.4 ミリ秒** (685 倍)。20,000 ハンクのコミットでも
/// **4.11 秒 → 80 ミリ秒**。
fn prepare<'a>(
    st: &'a lease::Store,
    tree: &Path,
    now: u64,
    alive: &dyn Fn(u32) -> bool,
) -> Vec<Pat<'a>> {
    let mut out: Vec<Pat<'a>> = Vec::new();
    // シンボリックリンクの解決器。**ディレクトリの解決を憶える**ので、
    // `src/` 配下に 200 個の担当があっても `src` を探るのは 1 回で済む。
    let mut links = crate::pathx::LinkResolver::new(tree);
    for l in &st.leases {
        if !l.active(now, alive) {
            continue;
        }
        let mine = holder_is_me(l, tree);
        for raw in &l.patterns {
            let (plain, span) = match region::parse(raw) {
                Ok(r) => {
                    let p = lease::normalize_path(&r.path);
                    let fast = (!p.contains('*') && !p.contains('?')).then_some(p);
                    (fast, r.span.unwrap_or(WHOLE))
                }
                // 記号指定 (`#fn:draw`) はテキストを見ないと行に落ちない。
                // **読めない指定は全体扱い** — ここで「行域を持っていない」と
                // 答えると誰でも書けてしまうので、失敗は必ず厳しい側へ倒す。
                Err(_) => (None, WHOLE),
            };
            // 実体の綴りは字句の綴りと違うときだけ持つ。**字句を捨てない** —
            // リンクが張り替えられても (TOCTOU) 字句の答えが残るので、
            // 照合が緩む方向へは動かない。
            let alias = plain.as_deref().and_then(|p| match links.resolve(p) {
                crate::pathx::Resolved::Inside(real) if real != p => Some(real),
                // リポジトリの外を指す担当は git がステージし得ないので照合対象外。
                // 解けない (輪 / 読めない) ときも字句の答えをそのまま使う。
                _ => None,
            });
            out.push(Pat {
                lease: l,
                raw,
                plain,
                alias,
                span,
                mine,
            });
        }
    }
    out
}

/// 触れた行域のうち、最初に他人とぶつかったものを返す。
fn first_collision(rel: &str, touched: &[Span], others: &[&Pat<'_>]) -> Option<Hit> {
    for t in touched {
        for p in others {
            if spans_conflict(rel, p.span, *t) {
                return Some(Hit {
                    touched: label(rel, *t),
                    owned: p.raw.to_string(),
                    lease: p.lease.clone(),
                    path: rel.to_string(),
                    degraded: None,
                    attr: String::new(),
                    bracketed: None,
                });
            }
        }
    }
    None
}

/// 2 つの行域が同時に持てないか。**同じ具体パス**の 2 つの [`region::Region`]
/// として [`region::conflicts`] に渡すだけ (行の算術をここに書かない)。
///
/// パスを両側とも `rel` (実在するステージ済みのパス) にするのが肝で、
/// 台帳側の glob をそのまま渡すと `conflicts` が安全側へ倒れて
/// **常に衝突**になる。パスが当たるかどうかは [`lease::covers`] が既に
/// 答えているので、ここは行だけを見ればよい。
///
/// 既知の限界: ファイル名に `*` `?` `[` が入っていると `conflicts` が
/// glob と見なして常に衝突扱いになる (安全側)。`lease::covers` と同じ限界。
fn spans_conflict(rel: &str, own: Span, touched: Span) -> bool {
    let mk = |s: Span| region::Region {
        path: rel.to_string(),
        span: Some(s),
        anchor: region::Anchor::default(),
    };
    region::conflicts(&mk(own), &mk(touched), region::SAFE_BAND)
}

/// 画面に出す「どこを触ったか」。全体なら行番号を付けない。
fn label(rel: &str, s: Span) -> String {
    if s == WHOLE {
        return rel.to_string();
    }
    region::render(&region::Region {
        path: rel.to_string(),
        span: Some(s),
        anchor: region::Anchor::default(),
    })
}

/// このリースの持ち主は「いまコミットしようとしている作業ツリー」か。
///
/// git のコミットは作業ツリー単位なので、身元も作業ツリーで決める
/// (エージェント名では決められない — フックは誰が呼んだかを知らないし、
/// 同じツリーの中に何体居ても index は 1 つしか無い)。
/// エージェントがツリーの**部分ディレクトリ**を cwd にしている場合も
/// 同じツリーとして扱う。
fn holder_is_me(l: &Lease, tree: &Path) -> bool {
    let owner = absolutize(&l.holder.cwd);
    if owner.as_os_str().is_empty() {
        return false;
    }
    // 同じツリー、またはツリーの配下。どちらも**実体まで解決してから**比べる。
    // ここを生の文字列で比べると、macOS の `/var` → `/private/var` のような
    // symlink で**自分の確保を他人のものと誤認して、自分のコミットを止める**
    // (実際にテストで踏んだ)。
    canon_key(&owner) == canon_key(tree) || lease::rel_within(tree, &owner).is_some()
}

/// 台帳に載っている `cwd` を、比較できる絶対パスへ戻す。
///
/// [`lease::normalize_path`] は**先頭の `/` を落とす** (セグメントを畳む実装の
/// 副作用で、台帳のキーとしては問題にならない)。そのままでは相対パスなので
/// `canonicalize` がプロセスの cwd を基準にしてしまう。
/// unix は先頭を戻す。Windows は `c:/users/…` のように既に絶対なので何もしない
/// (`is_absolute` が両方の側を同じ 1 本の判定で捌く)。
fn absolutize(raw: &str) -> PathBuf {
    if raw.is_empty() {
        return PathBuf::new();
    }
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        return p;
    }
    PathBuf::from(format!("/{raw}"))
}

// [`canon_key`] を何回通ったか。**テストだけが読む。**
//
// ここは `Path::canonicalize` = システムコールなので、呼ぶ回数がそのまま
// フックの所要時間になる。潰したかった回帰は「パス × リース回」呼んでいた
// (400 × 200 で 5.04 秒)。**回数を直に数えるのが、この性質を機械の速さに
// 依存せず固定する唯一の方法**である — 実時間で線を引くと、Docker の
// 仮想ファイルシステムや全 4273 件との同時実行で嘘の赤が出る (実際に出た)。
//
// **スレッドごとに数える。** プロセス共通の静的変数にすると、同時に走って
// いる他のテストの呼び出しまで混ざる (実際に 400 回のはずが 800 回になって
// 落ちた)。`canon_key` は呼んだスレッドの上で走るので、スレッドローカルで
// 過不足なく数えられる。
#[cfg(test)]
thread_local! {
    static CANON_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// 実体まで解決してから台帳の正規形へ。比較専用のキー。
fn canon_key(p: &Path) -> String {
    #[cfg(test)]
    CANON_CALLS.with(|c| c.set(c.get() + 1));
    // **`\\?\` を残さない。** Windows の `canonicalize` は verbatim 形式を
    // 返すので、片側だけが解決できたとき (`c:/x/y` と `//?/c:/x/y`) に
    // 文字列が食い違い、**自分の確保を他人のものと誤認して自分のコミットを
    // 止める**。`pathx::canonical` は解決したうえで接頭辞を外す。
    let c = crate::pathx::canonical(p);
    lease::normalize_path(&c.to_string_lossy())
}

/// `.gitattributes` から引いた 1 パスぶんの指定。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Attrs {
    /// `diff` — `unset` なら `-diff` (差分を出さない) 指定。
    diff: String,
    /// `merge` — `union` なら**両方の変更を並べて解決する**ので競合しない。
    merge: String,
    /// `filter` — `lfs` なら中身はポインタ (3 行) で、行番号に意味が無い。
    filter: String,
}

impl Attrs {
    /// この指定なら 2 人が同じ行を触っても衝突しないか。
    ///
    /// `merge=union` は**両方の行を残す**マージドライバなので、
    /// 同じ行域を 2 人が持っても解決はぶつからない (`CHANGELOG` や
    /// `.gitignore` のような追記専用ファイルで実際に使われている)。
    /// **ここだけは止めない**のが正しい — 止めても得るものが無い。
    fn never_conflicts(&self) -> bool {
        self.merge == "union"
    }

    /// 行域が使えなかった具体的な理由 (判れば)。
    fn why_no_lines(&self) -> Option<&'static str> {
        if self.filter == "lfs" || self.diff == "lfs" {
            return Some("Git LFS のポインタファイル");
        }
        if self.diff == "unset" {
            return Some("`.gitattributes` の `-diff` / `binary` 指定");
        }
        None
    }
}

/// `.gitattributes` の `diff` / `merge` / `filter` を **1 回の git で**引く。
///
/// `-z --stdin` なので、空白・日本語・改行入りのパスでも壊れない
/// (出力は `パス\0属性\0値\0` の 3 つ組の繰り返し)。
/// **引けなかったら空を返す** = 何も足さない・何も落とさない (fail-closed)。
fn check_attrs(repo: &Path, paths: &[String]) -> Vec<Attrs> {
    use std::io::Write as _;
    let mut child = match crate::procx::hidden_command("git")
        .arg("-C")
        .arg(repo)
        .args(["check-attr", "-z", "--stdin", "diff", "merge", "filter"])
        .env("LC_ALL", "C")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let feed: Vec<u8> = paths
        .iter()
        .flat_map(|p| p.as_bytes().iter().copied().chain(std::iter::once(0u8)))
        .collect();
    if let Some(mut si) = child.stdin.take() {
        let _ = si.write_all(&feed);
    }
    let Ok(out) = child.wait_with_output() else {
        return Vec::new();
    };
    if !out.status.success() {
        return Vec::new();
    }
    let text = crate::textenc::decode_output(&out.stdout);
    let mut got: Vec<Attrs> = vec![Attrs::default(); paths.len()];
    let toks: Vec<&str> = text.split('\0').collect();
    for tri in toks.chunks(3) {
        let [path, attr, value] = tri else { continue };
        let Some(i) = paths.iter().position(|p| p == path) else {
            continue;
        };
        match *attr {
            "diff" => got[i].diff = (*value).to_string(),
            "merge" => got[i].merge = (*value).to_string(),
            "filter" => got[i].filter = (*value).to_string(),
            _ => {}
        }
    }
    got
}

/// 拒否の候補に `.gitattributes` を当てて、落とすものを落とし理由を足す。
/// **止めると決まった後にしか呼ばない。**
fn refine(repo: &Path, hits: Vec<Hit>) -> Vec<Hit> {
    let mut paths: Vec<String> = Vec::new();
    for h in &hits {
        if !paths.contains(&h.path) {
            paths.push(h.path.clone());
        }
    }
    let attrs = check_attrs(repo, &paths);
    if attrs.len() != paths.len() {
        return hits; // 引けなかった = 何も変えない
    }
    let mut out = Vec::with_capacity(hits.len());
    for mut h in hits {
        let Some(i) = paths.iter().position(|p| *p == h.path) else {
            out.push(h);
            continue;
        };
        if attrs[i].never_conflicts() {
            continue; // union マージは衝突しない
        }
        if h.degraded.is_some() {
            if let Some(why) = attrs[i].why_no_lines() {
                h.attr = why.to_string();
            }
        }
        out.push(h);
    }
    out
}

/// 「行域が使えなかった」を人へ伝える 1 行。**黙って劣化させない。**
fn degrade_note(h: &Hit) -> Option<String> {
    let why = h.degraded?;
    let reason = if !h.attr.is_empty() {
        h.attr.clone()
    } else {
        match why {
            WholeWhy::NoHunks => tr("二値ファイル、または git が差分を出さない大きさ"),
            WholeWhy::Unaligned => tr("git の差分出力を解釈できませんでした"),
            WholeWhy::Lfs => {
                tr("Git LFS のポインタ — 実体は別の場所にあり、行番号に意味がありません")
            }
            WholeWhy::Shape => return None,
        }
    };
    Some(trf(
        "    ※ このファイルは行域で判定できないため**全体**の担当として突き合わせました ({reason})\n",
        &[("reason", reason)],
    ))
}

/// 拒否の文面。**「拒否されました」だけでは、ユーザーは機能を切るだけ。**
/// どの行を・誰の何と重ねたか・いつから持っていて・どうすれば良いかを必ず出す。
///
/// 行域が入ってからは「どのファイル」では足りない。**同じファイルでも
/// 離れていれば通る**のだから、ずらすべき行が判らないと打つ手が
/// 「待つ」しか残らない。
fn deny_text(hits: &[Hit], now: u64) -> String {
    let mut lines = String::new();
    for h in hits.iter().take(MAX_LISTED) {
        let l = &h.lease;
        let since = crate::instances::humanize_uptime(now.saturating_sub(l.acquired_at));
        let left = crate::instances::humanize_uptime(l.expires_at.saturating_sub(now));
        // **壁が無いのは「重なる」ではない。** 重なっていないからこそ帯を
        // 通ってしまったので、同じ動詞を使うと直し方を取り違える。
        let verb = if h.bracketed.is_some() {
            "と壁なしで並んでいます"
        } else {
            "と重なります"
        };
        lines.push_str(&trf(
            "  {path} — {owner} が保有する {own} {verb} ({since}前から / 期限まであと {left}){note}\n",
            &[
                ("verb", tr(verb)),
                ("path", h.touched.clone()),
                ("own", h.owned.clone()),
                ("owner", l.holder.display()),
                ("since", since),
                ("left", left),
                (
                    "note",
                    if l.note.is_empty() {
                        String::new()
                    } else {
                        trf(" / 目的: {note}", &[("note", l.note.clone())])
                    },
                ),
            ],
        ));
        if let Some(note) = degrade_note(h) {
            lines.push_str(&note);
        }
        // **交錯は「離せば直る」ではない。** 下の既定の案内と食い違うので、
        // その 1 件だけ理由を上書きして出す。
        if let Some(why) = &h.bracketed {
            lines.push_str(&trf("    ※ {why}\n", &[("why", why.clone())]));
        }
    }
    if hits.len() > MAX_LISTED {
        lines.push_str(&trf(
            "  ほか {n} 件\n",
            &[("n", (hits.len() - MAX_LISTED).to_string())],
        ));
    }
    trf(
        "コミットを止めました (Zaivern Code の行域オーナーシップ)。\n\
         今回ステージした変更が、別の担当が保有している行に掛かっています:\n\
         \n\
         {list}\n\
         同じ行を 2 人が同時に触ると、衝突はマージのときまで見えません。\n\
         (同じファイルでも {band} 行以上離れていれば同時に書けます。止めているのは重なった分だけです)\n\
         対処:\n\
         \x20 (1) 相手の完了を待ってから、もう一度コミットする\n\
         \x20 (2) 担当を分ける — 別の行域 / 別のファイル / 別のディレクトリを受け持つ\n\
         \x20 (3) 引き継ぐ: `zai lease list` で確認し、`zai lease release --agent <名前>` で解放する\n\
         \x20 (4) このコミットだけ通す: `git commit --no-verify` (衝突は残ります)\n\
         \x20 (5) ガードを外す: `zai guard uninstall`",
        &[("list", lines), ("band", region::SAFE_BAND.to_string())],
    )
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. 状態
// ═══════════════════════════════════════════════════════════════════════════

/// `zai guard status` が返すもの。
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Status {
    /// 作業ツリーの頂点。
    pub repo: String,
    /// フックの置き場 (`core.hooksPath` / linked worktree でも実際の場所)。
    pub hooks_dir: String,
    /// 自分のフックが入っているもの。
    pub installed: Vec<String>,
    /// 他人のフックが居て、まだ設置していないもの。
    pub foreign: Vec<String>,
    /// 何も入っていないもの。
    pub missing: Vec<String>,
    /// 元のフックを退避して連鎖しているもの。
    pub chained: Vec<String>,
    /// 台帳ファイル。
    pub ledger: String,
    /// このリポジトリでファイル所有リースが有効か。
    pub ledger_enabled: bool,
    /// 台帳に載っている確保の件数。
    pub leases: usize,
    /// この `zai` が `guard` サブコマンドを受け付けるか (配線の有無)。
    pub cli_wired: bool,
}

/// 現状を集める。**フックを 1 本も走らせない** (読むだけ)。
pub fn status(repo: &Path) -> Result<Status, GuardError> {
    status_in(repo, &lease::store_dir())
}

/// 台帳の置き場を明示する [`status`]。
pub fn status_in(repo: &Path, ledger_dir: &Path) -> Result<Status, GuardError> {
    let dir = hooks_dir(repo)?;
    let roots = lease::roots_of(repo);
    let store = lease::store_path_in(ledger_dir, &roots.key);
    let mut st = Status {
        repo: repo.display().to_string(),
        hooks_dir: dir.display().to_string(),
        ledger: store.display().to_string(),
        ledger_enabled: lease::enabled(&store),
        cli_wired: crate::cli::is_cli_subcommand("guard"),
        ..Default::default()
    };
    st.leases = lease::read_store(&store)
        .map(|s| s.leases.len())
        .unwrap_or(0);
    for name in HOOKS {
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(b) if is_ours(&String::from_utf8_lossy(&b)) => {
                st.installed.push((*name).to_string())
            }
            Ok(_) => st.foreign.push((*name).to_string()),
            Err(_) => st.missing.push((*name).to_string()),
        }
        if dir.join(format!("{name}{PREV_SUFFIX}")).exists() {
            st.chained.push((*name).to_string());
        }
    }
    Ok(st)
}

/// 人が読む形の [`Status`]。
pub fn render_status(st: &Status) -> String {
    let list = |v: &[String]| {
        if v.is_empty() {
            tr("(なし)")
        } else {
            v.join(" ")
        }
    };
    let mut out = trf(
        "リポジトリ: {repo}\nフックの置き場: {hooks}\n設置済み: {ok}\n他人のフック: {foreign}\n未設置: {missing}\n連鎖中: {chained}\n台帳: {ledger} ({enabled} / 確保 {n} 件)",
        &[
            ("repo", st.repo.clone()),
            ("hooks", st.hooks_dir.clone()),
            ("ok", list(&st.installed)),
            ("foreign", list(&st.foreign)),
            ("missing", list(&st.missing)),
            ("chained", list(&st.chained)),
            ("ledger", st.ledger.clone()),
            (
                "enabled",
                if st.ledger_enabled {
                    tr("有効")
                } else {
                    tr("無効")
                },
            ),
            ("n", st.leases.to_string()),
        ],
    );
    if !st.cli_wired {
        out.push('\n');
        out.push_str(&tr(
            "警告: この zai は `zai guard` を受け付けません。フックは通す側 (fail-open) に倒れます",
        ));
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. CLI
// ═══════════════════════════════════════════════════════════════════════════

/// `zai guard --help` の本文。
pub const HELP: &str = "\
guard (ベンダー非依存の書き込み強制 — git を関所にします):
  zai guard init [--repo <パス>]        pre-commit / pre-applypatch / pre-merge-commit を設置
  zai guard check --staged [--repo <パス>]
                                        ステージ済みのパスを台帳と突き合わせる (フックが呼びます)
  zai guard check --path <ファイル> [--repo <パス>]
                                        書き込む前に 1 本のパスを突き合わせる
                                        (リポジトリ内のリンクを通って外へ出る経路も止めます)
  zai guard status [--json] [--repo <パス>]
                                        設置状況と台帳の状態
  zai guard uninstall [--repo <パス>]   自分のフックだけを消し、退避した元のフックを戻す

終了コード: 0 = 許可 / 1 = 拒否 / 2 = 使い方の誤り
--repo を省くとカレントディレクトリから git rev-parse --show-toplevel で解決します。
";

/// `zai guard <sub>` の実体。argv は `"guard"` の**次**から渡される。
///
/// 戻り値は終了コード (0 = 許可 / 1 = 拒否 / 2 = 使い方の誤り)。
/// **`check` は失敗しても 0 を返す** — 内部エラーでコミットを止めない
/// (fail-open) というこの機能の約束が、ここに出る。
pub fn cli_main(argv: &[String]) -> i32 {
    if argv.iter().any(|a| a == "--help" || a == "-h") {
        println!("{}", HELP.trim_end());
        return EXIT_OK;
    }
    let sub = argv.first().map(String::as_str).unwrap_or("");
    let rest: &[String] = if argv.is_empty() { &[] } else { &argv[1..] };
    let (repo_opt, rest) = take_opt(rest, "--repo");
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
    match sub {
        "check" => {
            let (staged, rest) = take_flag(&rest, "--staged");
            let (path, rest) = take_opt(&rest, "--path");
            if staged && path.is_some() {
                return usage(&tr("`--staged` と `--path` は同時に使えません"));
            }
            if !staged && path.is_none() {
                return usage(&tr(
                    "`--staged` か `--path <ファイル>` を付けてください: zai guard check --staged",
                ));
            }
            if let Some(x) = rest.first() {
                return usage(&trf("余分な引数です: {x}", &[("x", x.clone())]));
            }
            // ここから先は**何があっても通す**。判定できないことを理由に
            // ユーザーのコミットを止めない。
            let Ok(repo) = repo_root(&start) else {
                return EXIT_OK;
            };
            let verdict = match &path {
                Some(p) => check_path(&repo, Path::new(p)),
                None => check_staged(&repo),
            };
            match verdict {
                Verdict::Allow => EXIT_OK,
                Verdict::Deny(reason) => {
                    eprintln!("{reason}");
                    EXIT_DENY
                }
            }
        }
        "init" => {
            if let Some(x) = rest.first() {
                return usage(&trf("余分な引数です: {x}", &[("x", x.clone())]));
            }
            let repo = match repo_root(&start) {
                Ok(r) => r,
                Err(e) => return usage(&e.to_string()),
            };
            match install(&repo) {
                Ok(done) => {
                    println!("{}", render_installed(&done));
                    EXIT_OK
                }
                Err(e) => usage(&e.to_string()),
            }
        }
        "uninstall" => {
            if let Some(x) = rest.first() {
                return usage(&trf("余分な引数です: {x}", &[("x", x.clone())]));
            }
            let repo = match repo_root(&start) {
                Ok(r) => r,
                Err(e) => return usage(&e.to_string()),
            };
            match uninstall(&repo) {
                Ok(done) => {
                    println!("{}", render_removed(&done));
                    EXIT_OK
                }
                Err(e) => usage(&e.to_string()),
            }
        }
        "status" => {
            let (json, rest) = take_flag(&rest, "--json");
            if let Some(x) = rest.first() {
                return usage(&trf("余分な引数です: {x}", &[("x", x.clone())]));
            }
            let repo = match repo_root(&start) {
                Ok(r) => r,
                Err(e) => return usage(&e.to_string()),
            };
            match status(&repo) {
                Ok(st) => {
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&st).unwrap_or_else(|_| "{}".into())
                        );
                    } else {
                        println!("{}", render_status(&st));
                    }
                    EXIT_OK
                }
                Err(e) => usage(&e.to_string()),
            }
        }
        "" => usage(&tr(
            "guard のサブコマンドを指定してください: init / check / status / uninstall",
        )),
        other => usage(&trf(
            "不明な guard サブコマンドです: {other}",
            &[("other", other.to_string())],
        )),
    }
}

// **統合担当が `cli.rs` から呼ぶ入口の型を、コンパイル時に固定する。**
//
// 2 つの役目がある:
//
// 1. 署名がずれたらここで落ちる (`"guard" => crate::features::guard::cli_main(rest)`
//    の 1 行を足すだけで繋がる、という約束を型で担保する)。
// 2. `src/features/guard.rs` の再エクスポートを**実際に使う**。これが無いと
//    配線されるまで `unused import` が出続け、本物の「作ったのに繋いでいない」
//    警告がその中に埋もれる (CLAUDE.md の検出器を鈍らせない)。
const _: fn(&[String]) -> i32 = crate::features::guard::cli_main;

fn usage(msg: &str) -> i32 {
    eprintln!("{msg}\n\n{}", HELP.trim_end());
    EXIT_USAGE
}

/// `--name <値>` を 1 つ取り出す (`cli.rs` の同名ヘルパは private なので持つ)。
fn take_opt(args: &[String], name: &str) -> (Option<String>, Vec<String>) {
    let mut value = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == name {
            value = it.next().cloned();
        } else {
            rest.push(a.clone());
        }
    }
    (value, rest)
}

/// `--flag` を 1 つ取り出す。
fn take_flag(args: &[String], name: &str) -> (bool, Vec<String>) {
    let mut found = false;
    let mut rest = Vec::new();
    for a in args {
        if a == name {
            found = true;
        } else {
            rest.push(a.clone());
        }
    }
    (found, rest)
}

/// 設置結果を人が読む形へ。
pub fn render_installed(d: &Installed) -> String {
    let mut out = trf(
        "ガードを設置しました: {dir}",
        &[("dir", d.hooks_dir.display().to_string())],
    );
    let add = |out: &mut String, label: &str, v: &[String]| {
        if !v.is_empty() {
            out.push('\n');
            out.push_str(&trf(
                label,
                &[("names", v.join(" ")), ("n", v.len().to_string())],
            ));
        }
    };
    add(&mut out, "  新規: {names}", &d.fresh);
    add(&mut out, "  更新 (既に設置済み): {names}", &d.refreshed);
    add(
        &mut out,
        "  既存フックを退避して連鎖: {names} (元の中身は <名前>.zaivern-prev から先に呼ばれます)",
        &d.chained,
    );
    add(
        &mut out,
        "  触りませんでした: {names} — 他人のフックが居て、退避先も埋まっています。手で片付けてから再実行してください",
        &d.blocked,
    );
    out
}

/// 解除結果を人が読む形へ。
pub fn render_removed(d: &Removed) -> String {
    let mut out = trf(
        "ガードを解除しました: {dir}",
        &[("dir", d.hooks_dir.display().to_string())],
    );
    for (label, v) in [
        ("  消しました: {names}", &d.removed),
        ("  元のフックを戻しました: {names}", &d.restored),
        ("  他人のフックなので触りませんでした: {names}", &d.kept),
    ] {
        if !v.is_empty() {
            out.push('\n');
            out.push_str(&trf(label, &[("names", v.join(" "))]));
        }
    }
    out
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. 機能レジストリ (パレットからの到達経路)
// ═══════════════════════════════════════════════════════════════════════════

/// パレットからの到達経路。打鍵は割り当てていない
/// (設置は一度きりの操作なので、パレット 1 経路で足りる)。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "guard",
    entries: &[crate::feature::Entry {
        icon: "🛡",
        label: "ガード: このリポジトリを競合ゼロにする",
        id: "guard.init",
    }],
    dispatch: |_app, ctx, id| match id {
        "guard.init" => {
            open_panel(ctx.clone());
            true
        }
        _ => false,
    },
    // 窓は中央ビューに属さないオーバーレイなので毎フレームここから描く。
    // **閉じているフレームは 1 命令も走らない** (設計原則 3)。
    draw: Some(draw),
    ..crate::feature::Feature::DEFAULT
};

/// パネルが表示する 1 回ぶんの結果。
struct Report {
    title: String,
    body: String,
    /// 直前の状態 (設置の前後で何が変わったかを同じ窓で見せる)。
    status: Option<Status>,
}

#[derive(Default)]
struct Panel {
    open: bool,
    /// 走っている作業。**UI スレッドは絶対に待たない。**
    pending: Option<Receiver<Report>>,
    title: String,
    body: String,
    status: Option<Status>,
}

fn panel() -> &'static Mutex<Panel> {
    static P: OnceLock<Mutex<Panel>> = OnceLock::new();
    P.get_or_init(|| Mutex::new(Panel::default()))
}

/// パレットの項目から呼ぶ入口。**git はここで待たない** — 裏のスレッドへ出す
/// (`git rev-parse` は混んだリポジトリで数秒返らないことが実測されている)。
fn open_panel(ctx: egui::Context) {
    let Ok(mut p) = panel().lock() else { return };
    p.open = true;
    p.title = tr("設置しています…");
    p.body.clear();
    p.pending = Some(spawn(ctx, Job::Install));
}

/// 裏で走らせる作業。
#[derive(Clone, Copy)]
enum Job {
    Install,
    Uninstall,
    Refresh,
}

fn spawn(ctx: egui::Context, job: Job) -> Receiver<Report> {
    let (tx, rx) = std::sync::mpsc::channel();
    // 名前を付ける (パニックログとプロファイラで出所が判る)。
    // **起こせなかったときも rx をそのまま返す** — `tx` がクロージャごと落ちて
    // 受信側が Disconnected を見るので、窓が「実行中」のまま固まらない。
    let _ = std::thread::Builder::new()
        .name("zaivern-guard".into())
        .spawn(move || {
            let _ = tx.send(run_job(job));
            ctx.request_repaint();
        });
    rx
}

fn run_job(job: Job) -> Report {
    let start = lease::gui_workspace_root();
    let repo = match repo_root(&start) {
        Ok(r) => r,
        Err(e) => {
            return Report {
                title: tr("できませんでした"),
                body: e.to_string(),
                status: None,
            }
        }
    };
    let body = match job {
        Job::Install => match install(&repo) {
            Ok(d) => render_installed(&d),
            Err(e) => e.to_string(),
        },
        Job::Uninstall => match uninstall(&repo) {
            Ok(d) => render_removed(&d),
            Err(e) => e.to_string(),
        },
        Job::Refresh => String::new(),
    };
    Report {
        title: tr("🛡 ガード (git を関所にする)"),
        body,
        status: status(&repo).ok(),
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
            Ok(r) => {
                p.title = r.title;
                p.body = r.body;
                p.status = r.status;
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
    let title = if p.title.is_empty() {
        tr("🛡 ガード (git を関所にする)")
    } else {
        p.title.clone()
    };
    egui::Window::new(title)
        // **題名から ID を切り離す。** egui の `Window` は既定で題名を ID に
        // 使うので、進捗表示で題名が変わるたびに位置と大きさを失う。
        .id(egui::Id::new("guard.panel"))
        .collapsible(false)
        .resizable(true)
        .default_width(620.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            ui.set_max_width(ui.available_width());
            ui.label(tr(
                "どのエージェントでも、どのエディタでも、成果を残すには git を通ります。\
                 そこへ関所を置くので、ベンダーの対応を待たずに「他の担当が持っているファイルの\
                 コミット」を止められます。",
            ));
            ui.separator();
            if !p.body.is_empty() {
                egui::ScrollArea::vertical()
                    .id_salt("guard.body")
                    .max_height(240.0)
                    .show(ui, |ui| {
                        ui.label(&p.body);
                    });
            }
            if let Some(st) = &p.status {
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("guard.status")
                    .max_height(200.0)
                    .show(ui, |ui| {
                        ui.label(render_status(st));
                    });
            }
            ui.separator();
            // 狭い幅でも見切れないよう折り返す。
            ui.horizontal_wrapped(|ui| {
                if ui.button(tr("設置し直す")).clicked() {
                    job = Some(Job::Install);
                }
                if ui.button(tr("解除する")).clicked() {
                    job = Some(Job::Uninstall);
                }
                if ui.button(tr("状態を取り直す")).clicked() {
                    job = Some(Job::Refresh);
                }
            });
        });
    if !open {
        p.open = false;
    }
    if let Some(j) = job {
        p.title = tr("実行しています…");
        p.body.clear();
        p.pending = Some(spawn(ctx.clone(), j));
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    // ───────────────────────── 下ごしらえ ─────────────────────────

    /// 実 git リポジトリを一時ディレクトリに作る。git が無い環境では `None`。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        let dir = crate::test_util::unique_temp_dir("zaivern-guard-test", tag);
        let ok = Command::new("git")
            .arg("-C")
            .arg(&dir)
            .args(["init", "--quiet"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !ok {
            std::fs::remove_dir_all(&dir).ok();
            return None;
        }
        for (k, v) in [
            ("user.email", "guard@example.invalid"),
            ("user.name", "guard"),
            ("commit.gpgsign", "false"),
        ] {
            let _ = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(["config", k, v])
                .status();
        }
        Some(dir)
    }

    fn write(path: &Path, text: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).expect("mkdir");
        }
        std::fs::write(path, text).expect("write");
    }

    fn git(repo: &Path, args: &[&str]) -> std::process::Output {
        Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .expect("git を起動できない")
    }

    /// `zai guard check --staged` の**振る舞いだけ**を真似る sh の替え玉。
    ///
    /// テストバイナリは `zai` ではないので (単体テストに `CARGO_BIN_EXE_*` は
    /// 無い)、フックへ本物の実行ファイルを埋め込めない。そこで
    /// 「argv を記録して、指定の終了コードと文面を返す」だけの替え玉を置く。
    /// **本物の `cli_main` が同じ argv で同じ終了コードを返すこと**は
    /// 別のテスト (`cli_の終了コードは0と1と2の3種類だけ`) で固定するので、
    /// git → フック → CLI → 拒否 の鎖は全ての環が押さえられる。
    fn stub_zai(dir: &Path, code: i32, message: &str) -> PathBuf {
        let p = dir.join("zai-stub");
        write(
            &p,
            &format!(
                "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$(dirname \"$0\")/argv.log\"\n\
                 printf '%s\\n' {msg} >&2\nexit {code}\n",
                msg = sh_quote(message),
            ),
        );
        make_exec(&p);
        p
    }

    #[cfg(unix)]
    fn make_exec(p: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    /// Windows には実行ビットが無い (git 同梱の sh が shebang を見て起動する)。
    #[cfg(not(unix))]
    fn make_exec(_p: &Path) {}

    /// 台帳を temp に作り、`owner_cwd` が `patterns` を保有している状態にする。
    fn seed_ledger(ledger: &Path, repo: &Path, owner_cwd: &Path, patterns: &[&str]) -> PathBuf {
        seed_holder(ledger, repo, "その他の担当", owner_cwd, patterns)
    }

    /// 同じ台帳へ担当を**何人でも**足せる形。行域オーナーシップの検査は
    /// 「同じファイルに 2 人」を作れないと 1 つも書けない。
    ///
    /// 確保が拒まれたら即座に落とす — 下ごしらえが黙って失敗すると、
    /// 「誰も持っていないから通った」を「行域で通った」と読み違える。
    fn seed_holder(
        ledger: &Path,
        repo: &Path,
        agent: &str,
        owner_cwd: &Path,
        patterns: &[&str],
    ) -> PathBuf {
        let roots = lease::roots_of(repo);
        let store = lease::store_path_in(ledger, &roots.key);
        std::fs::create_dir_all(ledger).expect("mkdir ledger");
        lease::enable(&store).expect("enable");
        let holder = lease::Holder {
            agent: agent.into(),
            session: String::new(),
            cwd: lease::normalize_path(&owner_cwd.to_string_lossy()),
            pid: 0,
        };
        let pats: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
        let now = lease::now_secs();
        // **基準フォルダは `repo`。** 素の `try_claim` はプロセスの cwd を見る
        // ので本文が読めず、壁の判定 (`region::needs_wall`) が fail-closed で
        // 全部断る。下ごしらえは実際の作業ツリーを起点にすること。
        let got = lease::with_store(&store, |s| {
            lease::try_claim_in(
                repo,
                s,
                &holder,
                &pats,
                now,
                lease::DEFAULT_TTL_SECS,
                &|_| false,
            )
        })
        .expect("claim");
        assert!(
            matches!(got, lease::Claim::Granted(_)),
            "{agent} が {patterns:?} を確保できない: {got:?}"
        );
        store
    }

    /// 300 行のファイルを 1 つ持つリポジトリ。行域の検査はこの土台の上で行う。
    fn repo_with_300_lines(tag: &str) -> Option<(PathBuf, Vec<String>)> {
        let repo = temp_repo(tag)?;
        let lines: Vec<String> = (1..=300).map(|i| format!("line {i}")).collect();
        write(&repo.join("f.txt"), &format!("{}\n", lines.join("\n")));
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());
        Some((repo, lines))
    }

    /// `lines` の `n` 行目 (1 始まり) を書き換えて index へ載せる。
    fn restage_line(repo: &Path, lines: &[String], n: usize, text: &str) {
        let mut v = lines.to_vec();
        v[n - 1] = text.to_string();
        write(&repo.join("f.txt"), &format!("{}\n", v.join("\n")));
        assert!(git(repo, &["add", "-A"]).status.success());
    }

    // ───────────────────────── 純粋部分 ─────────────────────────

    #[test]
    fn sh_quote_は単一引用符を閉じ直す() {
        assert_eq!(sh_quote("/usr/bin/zai"), "'/usr/bin/zai'");
        assert_eq!(sh_quote("/a b/zai"), "'/a b/zai'");
        // `'` が入っても壊れない (実在し得る: /Users/O'Brien/bin/zai)
        assert_eq!(sh_quote("/O'B/zai"), "'/O'\\''B/zai'");
        // sh のメタ文字はクオート内なので展開されない
        assert_eq!(sh_quote("/x$HOME/`z`/zai"), "'/x$HOME/`z`/zai'");
    }

    #[test]
    fn フック本文はposix_shで書かれている() {
        let s = hook_script("/opt/zai");
        assert!(s.starts_with("#!/bin/sh\n"), "shebang が違う");
        assert!(s.contains(MARKER), "目印が無いと冪等にできない");
        assert!(!s.contains('\r'), "CRLF だと Windows の sh が壊れる");
        // bash 依存構文を 1 つも使わない (Windows 同梱 sh は dash 相当)
        for bad in [
            "[[",
            "]]",
            "function ",
            "local ",
            "$'",
            "==",
            "&>",
            "source ",
        ] {
            assert!(!s.contains(bad), "bash 依存構文が混ざっている: {bad}");
        }
        // 元のフックを exec せずに呼び、終了コードを尊重する
        assert!(s.contains("$0.zaivern-prev"));
        assert!(
            !s.lines().any(|l| l.trim_start().starts_with("exec ")),
            "元のフックを exec で置き換えると、後続の自分の判定が走らない"
        );
        assert!(s.contains("exit \"$__zg_st\""));
        // zai は絶対パスで、クオートして埋める
        assert!(s.contains("__zg_exe='/opt/zai'"));
        assert!(s.contains("guard check --staged"));
        // fail-open: 実行ファイルが無ければ通す / 1 以外は通す
        assert!(s.contains("if [ ! -f \"$__zg_exe\" ]"));
        assert!(s.contains("-eq 1"));
        assert!(s.trim_end().ends_with("exit 0"));
    }

    #[test]
    fn 自分のフックかどうかを目印で判定する() {
        assert!(is_ours(&hook_script("/x/zai")));
        assert!(is_ours("#!/bin/sh\n# zaivern-guard:v99 未来版\n"));
        assert!(!is_ours("#!/bin/sh\nnpx husky\n"));
        assert!(!is_ours(""));
    }

    #[test]
    fn 実行ファイルのパスはwindowsだけ区切りを寄せる() {
        // unix ではファイル名に `\` が入り得るので**絶対に置換しない**
        let p = PathBuf::from(if cfg!(windows) {
            r"C:\a b\zai.exe"
        } else {
            "/a b/zai"
        });
        let s = exe_text(&p);
        if cfg!(windows) {
            assert_eq!(s, "C:/a b/zai.exe");
        } else {
            assert_eq!(s, "/a b/zai");
        }
        assert!(!s.is_empty());
    }

    /// **回帰テスト**: 台帳が保存している形 (`lease::normalize_path` 済み) の
    /// `cwd` から、自分の作業ツリーだと判定できること。
    ///
    /// `normalize_path` は**先頭の `/` を落とす**ので、素直に `PathBuf::from`
    /// すると相対パスになり `canonicalize` がプロセスの cwd を基準にする。
    /// さらに macOS では `/var` → `/private/var` の symlink があるため、
    /// 生の文字列比較だと**自分の確保で自分のコミットを止めてしまう**
    /// (実際にこの実装の最初の版で踏んだ)。
    #[test]
    fn 台帳が保存する形のcwdから自分の作業ツリーだと判る() {
        let tree = crate::test_util::unique_temp_dir("zaivern-guard-test", "ident");
        let sub = tree.join("src").join("deep");
        std::fs::create_dir_all(&sub).expect("mkdir");
        let other = crate::test_util::unique_temp_dir("zaivern-guard-test", "ident-other");

        let lease_for = |cwd: &Path| Lease {
            holder: lease::Holder {
                agent: "x".into(),
                session: String::new(),
                cwd: lease::normalize_path(&cwd.to_string_lossy()),
                pid: 0,
            },
            patterns: vec!["a".into()],
            anchors: Vec::new(),
            acquired_at: 0,
            expires_at: u64::MAX,
            note: String::new(),
        };

        assert!(
            holder_is_me(&lease_for(&tree), &tree),
            "同じツリーを自分だと判定できない"
        );
        assert!(
            holder_is_me(&lease_for(&sub), &tree),
            "部分ディレクトリで動く担当を別人にしている"
        );
        assert!(
            !holder_is_me(&lease_for(&other), &tree),
            "別ツリーを自分にしている"
        );
        // cwd 不明 (空) の持ち主は「自分ではない」= fail-closed 側
        let mut nameless = lease_for(&tree);
        nameless.holder.cwd = String::new();
        assert!(!holder_is_me(&nameless, &tree));
    }

    #[test]
    fn 先頭の区切りを落とされたcwdを絶対パスへ戻す() {
        if cfg!(windows) {
            // Windows は `c:/users/…` の形で既に絶対 (ドライブ付き)
            assert_eq!(absolutize("c:/users/x/r"), PathBuf::from("c:/users/x/r"));
        } else {
            assert_eq!(
                absolutize("var/folders/x/r"),
                PathBuf::from("/var/folders/x/r")
            );
            assert_eq!(absolutize("/already/abs"), PathBuf::from("/already/abs"));
        }
        assert_eq!(absolutize(""), PathBuf::new());
    }

    #[test]
    fn 拒否の文面は誰がいつからどうすればを全部出す() {
        let now = lease::now_secs();
        let l = Lease {
            holder: lease::Holder {
                agent: "claude".into(),
                session: "abcdef123456".into(),
                cwd: "/w/a".into(),
                pid: 0,
            },
            patterns: vec!["src/app.rs".into()],
            anchors: Vec::new(),
            acquired_at: now.saturating_sub(600),
            expires_at: now + 1200,
            note: "認証まわりの整理".into(),
        };
        let text = deny_text(
            &[Hit {
                bracketed: None,
                touched: "src/app.rs#L100-160".into(),
                owned: "src/app.rs#L120-140".into(),
                lease: l,
                path: "src/app.rs".into(),
                degraded: None,
                attr: String::new(),
            }],
            now,
        );
        for needle in [
            "src/app.rs#L100-160",
            "src/app.rs#L120-140",
            "claude",
            "保有",
            "重なります",
            "目的: 認証まわりの整理",
            "zai lease release",
            "git commit --no-verify",
            "zai guard uninstall",
        ] {
            assert!(text.contains(needle), "拒否文面に {needle} が無い:\n{text}");
        }
    }

    #[test]
    fn 拒否の一覧は上限で打ち切る() {
        let now = lease::now_secs();
        let mk = |i: usize| Hit {
            bracketed: None,
            touched: format!("src/f{i}.rs"),
            owned: format!("src/f{i}.rs"),
            lease: Lease {
                holder: lease::Holder {
                    agent: "x".into(),
                    session: String::new(),
                    cwd: "/w/a".into(),
                    pid: 0,
                },
                patterns: vec![format!("src/f{i}.rs")],
                anchors: Vec::new(),
                acquired_at: now,
                expires_at: now + 60,
                note: String::new(),
            },
            path: format!("src/f{i}.rs"),
            degraded: None,
            attr: String::new(),
        };
        let hits: Vec<_> = (0..MAX_LISTED + 5).map(mk).collect();
        let text = deny_text(&hits, now);
        assert!(text.contains("ほか 5 件"), "打ち切りの表示が無い:\n{text}");
        assert!(!text.contains(&format!("src/f{}.rs", MAX_LISTED + 1)));
    }

    #[test]
    fn 引数の取り出しは順序に依らない() {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        let (r, rest) = take_opt(&v(&["--repo", "/x", "--staged"]), "--repo");
        assert_eq!(r.as_deref(), Some("/x"));
        assert_eq!(rest, v(&["--staged"]));
        let (r, rest) = take_opt(&v(&["--staged", "--repo", "/x"]), "--repo");
        assert_eq!(r.as_deref(), Some("/x"));
        assert_eq!(rest, v(&["--staged"]));
        // 値が無い `--repo` で panic しない
        let (r, rest) = take_opt(&v(&["--repo"]), "--repo");
        assert_eq!(r, None);
        assert!(rest.is_empty());
        let (f, rest) = take_flag(&v(&["--json", "x"]), "--json");
        assert!(f);
        assert_eq!(rest, v(&["x"]));
    }

    // ───────────────────────── 設置 / 解除 ─────────────────────────

    #[test]
    fn 設置は冪等で二重にならない() {
        let Some(repo) = temp_repo("idem") else {
            return; // git が無い環境ではスキップ
        };
        let first = install_with(&repo, "/opt/zai").expect("install");
        assert_eq!(first.fresh.len(), HOOKS.len(), "3 本とも新規のはず");
        assert!(first.chained.is_empty());
        let hook = first.hooks_dir.join("pre-commit");
        let a = std::fs::read_to_string(&hook).expect("read");

        let second = install_with(&repo, "/opt/zai").expect("install 2");
        assert!(second.fresh.is_empty(), "2 回目は新規が無いはず");
        assert_eq!(second.refreshed.len(), HOOKS.len());
        let b = std::fs::read_to_string(&hook).expect("read 2");
        assert_eq!(a, b, "2 回設置すると中身が変わってしまっている");
        assert_eq!(a.matches(MARKER).count(), 1, "目印が二重になっている");

        // 実行権が付いている (unix)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&hook).expect("meta").permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "実行権が無いと git は黙って無視する");
        }

        let rm = uninstall(&repo).expect("uninstall");
        assert_eq!(rm.removed.len(), HOOKS.len());
        assert!(!hook.exists(), "自分のフックが残っている");
    }

    #[test]
    fn フックの置き場はcore_hookspathに従う() {
        let Some(repo) = temp_repo("hookspath") else {
            return;
        };
        assert_eq!(
            hooks_dir(&repo).expect("hooks"),
            repo.join(".git").join("hooks"),
            "既定は .git/hooks"
        );
        assert!(git(&repo, &["config", "core.hooksPath", "myhooks"])
            .status
            .success());
        assert_eq!(
            hooks_dir(&repo).expect("hooks 2"),
            repo.join("myhooks"),
            "core.hooksPath を無視すると、設置したのに 1 度も走らない"
        );
        let done = install_with(&repo, "/opt/zai").expect("install");
        assert!(repo.join("myhooks").join("pre-commit").exists());
        assert_eq!(done.hooks_dir, repo.join("myhooks"));
    }

    #[test]
    fn 既存フックは退避先が埋まっていたら触らない() {
        let Some(repo) = temp_repo("blocked") else {
            return;
        };
        let dir = hooks_dir(&repo).expect("hooks");
        std::fs::create_dir_all(&dir).expect("mkdir");
        write(&dir.join("pre-commit"), "#!/bin/sh\necho husky\n");
        write(
            &dir.join("pre-commit.zaivern-prev"),
            "#!/bin/sh\necho old\n",
        );
        let done = install_with(&repo, "/opt/zai").expect("install");
        assert_eq!(done.blocked, vec!["pre-commit".to_string()]);
        assert_eq!(
            std::fs::read_to_string(dir.join("pre-commit")).expect("read"),
            "#!/bin/sh\necho husky\n",
            "ユーザーのフックを黙って消してはいけない"
        );
    }

    #[test]
    fn 解除は他人のフックを消さない() {
        let Some(repo) = temp_repo("keep") else {
            return;
        };
        let dir = hooks_dir(&repo).expect("hooks");
        std::fs::create_dir_all(&dir).expect("mkdir");
        write(&dir.join("pre-commit"), "#!/bin/sh\necho husky\n");
        let rm = uninstall(&repo).expect("uninstall");
        assert_eq!(rm.kept, vec!["pre-commit".to_string()]);
        assert!(dir.join("pre-commit").exists());
    }

    // ───────────────────────── ステージ済みパス ─────────────────────────

    #[test]
    fn ステージ済みパスは空白と日本語と改行で壊れない() {
        let Some(repo) = temp_repo("staged") else {
            return;
        };
        // 改行入りのファイル名は Windows で作れないので OS で分ける。
        let mut names: Vec<String> = vec![
            "src/main.rs".into(),
            "src/日 本語/a b.rs".into(),
            "docs/'quote'.md".into(),
        ];
        if !cfg!(windows) {
            names.push("odd/line\nbreak.txt".into());
        }
        for n in &names {
            write(&repo.join(n), "x\n");
        }
        assert!(git(&repo, &["add", "-A"]).status.success());
        let got: Vec<String> = staged_changes(&repo)
            .expect("staged")
            .into_iter()
            .map(|c| c.path)
            .collect();
        for n in &names {
            assert!(got.contains(n), "{n} が拾えていない: {got:?}");
        }
        assert_eq!(got.len(), names.len());
    }

    #[test]
    fn 最初のコミットでもステージ済みパスが取れる() {
        let Some(repo) = temp_repo("first") else {
            return;
        };
        // HEAD がまだ無い状態。ここで落ちると「最初の 1 回だけ効かない」になる。
        write(&repo.join("a.txt"), "x\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert_eq!(
            staged_changes(&repo).expect("staged"),
            vec![StagedChange {
                path: "a.txt".into(),
                // HEAD が無いので全部が新規 = ファイル全体
                touched: Touched::Whole(WholeWhy::Shape),
            }]
        );
    }

    // ───────────────────────── 判定 ─────────────────────────

    #[test]
    fn 台帳が無ければ何もしない() {
        let Some(repo) = temp_repo("noledger") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "noledger-l");
        write(&repo.join("src/app.rs"), "x\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert_eq!(check_staged_in(&repo, &ledger), Verdict::Allow);
        // 台帳ファイルを作っていないこと (未導入者のコストはゼロ)
        assert_eq!(
            std::fs::read_dir(&ledger).map(|d| d.count()).unwrap_or(0),
            0
        );
    }

    #[test]
    fn 他人が保有しているパスをステージしたら拒否する() {
        let Some(repo) = temp_repo("deny") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "deny-l");
        let other = crate::test_util::unique_temp_dir("zaivern-guard-test", "deny-other");
        seed_ledger(&ledger, &repo, &other, &["src/app.rs"]);

        write(&repo.join("src/app.rs"), "x\n");
        write(&repo.join("src/other.rs"), "y\n");
        assert!(git(&repo, &["add", "-A"]).status.success());

        match check_staged_in(&repo, &ledger) {
            Verdict::Deny(reason) => {
                assert!(reason.contains("src/app.rs"), "{reason}");
                assert!(reason.contains("その他の担当"), "{reason}");
                assert!(
                    !reason.contains("src/other.rs"),
                    "他人の物でないパスまで挙げている"
                );
            }
            Verdict::Allow => panic!("他人が保有しているのに通ってしまった"),
        }
    }

    #[test]
    fn 自分の作業ツリーが保有しているなら通す() {
        let Some(repo) = temp_repo("mine") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "mine-l");
        // 保有者の cwd = このリポジトリ自身 (= コミットしようとしている作業ツリー)
        seed_ledger(&ledger, &repo, &repo, &["src/app.rs"]);
        write(&repo.join("src/app.rs"), "x\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert_eq!(
            check_staged_in(&repo, &ledger),
            Verdict::Allow,
            "自分の確保で自分のコミットを止めてはいけない"
        );

        // 部分ディレクトリを cwd にしている担当も同じツリー扱い
        let ledger2 = crate::test_util::unique_temp_dir("zaivern-guard-test", "mine-l2");
        seed_ledger(&ledger2, &repo, &repo.join("src"), &["src/app.rs"]);
        assert_eq!(check_staged_in(&repo, &ledger2), Verdict::Allow);
    }

    #[test]
    fn 期限切れのリースは止めない() {
        let Some(repo) = temp_repo("expired") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "expired-l");
        let other = crate::test_util::unique_temp_dir("zaivern-guard-test", "expired-other");
        let store = seed_ledger(&ledger, &repo, &other, &["src/app.rs"]);
        // 期限を過去へ倒す (pid=0 なので猶予も効かない)
        lease::with_store(&store, |s| {
            for l in &mut s.leases {
                l.expires_at = 1;
            }
        })
        .expect("expire");
        write(&repo.join("src/app.rs"), "x\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert_eq!(check_staged_in(&repo, &ledger), Verdict::Allow);
    }

    #[test]
    fn 台帳が壊れていても止めない() {
        let Some(repo) = temp_repo("broken") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "broken-l");
        let other = crate::test_util::unique_temp_dir("zaivern-guard-test", "broken-other");
        let store = seed_ledger(&ledger, &repo, &other, &["src/app.rs"]);
        std::fs::write(&store, b"{ this is not json").expect("corrupt");
        write(&repo.join("src/app.rs"), "x\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert_eq!(
            check_staged_in(&repo, &ledger),
            Verdict::Allow,
            "台帳の破損でコミットを止めると、ユーザーは機能ごと切る"
        );
    }

    #[test]
    fn ディレクトリ確保は配下のパスに効く() {
        let Some(repo) = temp_repo("subtree") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "subtree-l");
        let other = crate::test_util::unique_temp_dir("zaivern-guard-test", "subtree-other");
        seed_ledger(&ledger, &repo, &other, &["src/auth/"]);
        write(&repo.join("src/auth/token.rs"), "x\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)));
    }

    // ───────────────────────── 行域 (この機能の芯) ─────────────────────────

    /// **この修正そのもの。** 同じファイルの離れた行域を持つ 2 人が、
    /// それぞれ自分の担当だけを直してコミットできる。
    ///
    /// 直前まではファイル粒度 (`Lease::covers_path`) で見ていたので、
    /// **両方止まっていた**。64 体が同時に書けても誰もコミットできないなら、
    /// 行域オーナーシップの価値はコミットの瞬間に丸ごと消える。
    #[test]
    fn 離れた行域を持つ2人はどちらも自分の担当をコミットできる() {
        let Some((repo, lines)) = repo_with_300_lines("span-ok") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "span-ok-l");
        let alice_cwd = crate::test_util::unique_temp_dir("zaivern-guard-test", "span-ok-alice");
        // alice は別の作業ツリー、bob は**いまコミットしようとしているツリー**。
        seed_holder(&ledger, &repo, "alice", &alice_cwd, &["f.txt#L20-60"]);
        seed_holder(&ledger, &repo, "bob", &repo, &["f.txt#L200-240"]);

        // bob が自分の域の中だけを直す → 通る (ここが今まで止まっていた)
        restage_line(&repo, &lines, 220, "bob の担当を直した");
        assert_eq!(
            check_staged_in(&repo, &ledger),
            Verdict::Allow,
            "離れた行域を持つ 2 人が居るだけで、自分の担当のコミットが止まっている"
        );

        // 境界ちょうど (200 行目 / 240 行目) も自分の域
        restage_line(&repo, &lines, 200, "上の端");
        assert_eq!(check_staged_in(&repo, &ledger), Verdict::Allow);
        restage_line(&repo, &lines, 240, "下の端");
        assert_eq!(check_staged_in(&repo, &ledger), Verdict::Allow);
    }

    /// はみ出したら止まる。**誰の・どの行と重なったか**まで文面に出る。
    #[test]
    fn 他人の行域へはみ出したら止まり誰と重なったかが出る() {
        let Some((repo, lines)) = repo_with_300_lines("span-ng") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "span-ng-l");
        let alice_cwd = crate::test_util::unique_temp_dir("zaivern-guard-test", "span-ng-alice");
        seed_holder(&ledger, &repo, "alice", &alice_cwd, &["f.txt#L20-60"]);
        seed_holder(&ledger, &repo, "bob", &repo, &["f.txt#L200-240"]);

        restage_line(&repo, &lines, 30, "alice の担当へはみ出した");
        match check_staged_in(&repo, &ledger) {
            Verdict::Deny(reason) => {
                assert!(
                    reason.contains("f.txt#L30"),
                    "触った行が出ていない:\n{reason}"
                );
                assert!(
                    reason.contains("f.txt#L20-60"),
                    "相手の域が出ていない:\n{reason}"
                );
                assert!(
                    reason.contains("alice"),
                    "相手の名前が出ていない:\n{reason}"
                );
            }
            Verdict::Allow => panic!("他人の行域へはみ出したのに通ってしまった"),
        }
    }

    /// 安全帯。**離れていれば通り、`SAFE_BAND` より近ければ止まる。**
    /// git の diff は既定で 3 行の文脈を付けるので、3 行未満しか離れていない
    /// 2 つの変更は xdiff が 1 ハンクに畳んで衝突にする。
    #[test]
    fn 安全帯より近い行は他人の域と見なす() {
        let Some((repo, lines)) = repo_with_300_lines("band") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "band-l");
        let alice_cwd = crate::test_util::unique_temp_dir("zaivern-guard-test", "band-alice");
        seed_holder(&ledger, &repo, "alice", &alice_cwd, &["f.txt#L20-60"]);

        // 61 / 62 / 63 行目は alice の 60 行目から 3 行未満 → 止まる
        for n in [61usize, 62, 63] {
            restage_line(&repo, &lines, n, "近すぎる");
            assert!(
                matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)),
                "{n} 行目は安全帯の中なので止めるべき"
            );
        }
        // 64 行目は間に 3 行 (61,62,63) あるので通る
        restage_line(&repo, &lines, 64, "ここからは別の担当");
        assert_eq!(
            check_staged_in(&repo, &ledger),
            Verdict::Allow,
            "{}行以上離れているのに止めている",
            region::SAFE_BAND
        );
    }

    /// 誰も持っていない行は通す。書き込み側の関所
    /// ([`lease::decide_spans`]) と同じ向きで、ここだけ厳しくすると
    /// 「書けたのにコミットできない」が生まれる。
    #[test]
    fn 誰も持っていない行は通す() {
        let Some((repo, lines)) = repo_with_300_lines("free") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "free-l");
        let alice_cwd = crate::test_util::unique_temp_dir("zaivern-guard-test", "free-alice");
        seed_holder(&ledger, &repo, "alice", &alice_cwd, &["f.txt#L20-60"]);
        // bob はリースを 1 つも持っていない
        restage_line(&repo, &lines, 280, "誰の担当でもない行");
        assert_eq!(check_staged_in(&repo, &ledger), Verdict::Allow);
    }

    /// ファイル全体の担当が居れば**従来どおり**止まる (行域が無い旧来のリース)。
    #[test]
    fn ファイル全体の担当が居れば行域に関わらず止まる() {
        let Some((repo, lines)) = repo_with_300_lines("whole") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "whole-l");
        let alice_cwd = crate::test_util::unique_temp_dir("zaivern-guard-test", "whole-alice");
        seed_holder(&ledger, &repo, "alice", &alice_cwd, &["f.txt"]);
        restage_line(&repo, &lines, 280, "どこを触っても alice の領分");
        assert!(matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)));
    }

    /// 行を**消した**ときは、削除点の直後の行を触ったものとして扱う
    /// ([`region::touched_spans`] と同じ約束)。
    #[test]
    fn 行の削除も担当の中なら通り外なら止まる() {
        let Some((repo, lines)) = repo_with_300_lines("del") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "del-l");
        let alice_cwd = crate::test_util::unique_temp_dir("zaivern-guard-test", "del-alice");
        seed_holder(&ledger, &repo, "alice", &alice_cwd, &["f.txt#L20-60"]);
        seed_holder(&ledger, &repo, "bob", &repo, &["f.txt#L200-240"]);

        let del = |n: usize| {
            let mut v = lines.clone();
            v.remove(n - 1);
            write(&repo.join("f.txt"), &format!("{}\n", v.join("\n")));
            assert!(git(&repo, &["add", "-A"]).status.success());
        };
        del(220);
        assert_eq!(check_staged_in(&repo, &ledger), Verdict::Allow);
        del(30);
        assert!(matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)));
    }

    // ─────────────── 行域を持てない変更 (安全側 = ファイル全体) ───────────────

    /// **新規作成は行域では説明できない。** 「その行を書き換える」のではなく
    /// 「ファイルを生やす」操作なので、そのパスを一部でも持っている人が
    /// 居るなら止める。
    #[test]
    fn 新規ファイルはファイル全体として扱う() {
        let Some(repo) = temp_repo("add") else {
            return;
        };
        write(&repo.join("seed.txt"), "x\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());

        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "add-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "add-alice");
        // 行番号だけ見れば 3 行のファイルと 500 行目は重ならない。
        // それでも「生やす」操作は全体なので止まる。
        seed_holder(&ledger, &repo, "alice", &alice, &["new.txt#L500-600"]);
        write(&repo.join("new.txt"), "a\nb\nc\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)));
    }

    /// 削除・リネーム・モード変更・二値は全部「ファイル全体」。
    /// **リネームは旧パスの担当も守る** (`--name-only` は新パスしか出さないので、
    /// そこを信じると旧パスの持ち主を素通りさせる)。
    #[test]
    fn 削除とリネームとモードと二値はファイル全体として扱う() {
        let Some(repo) = temp_repo("shapes") else {
            return;
        };
        for (n, t) in [
            ("gone.txt", "1\n2\n3\n"),
            ("old.txt", "r\n"),
            ("mode.txt", "m\n"),
        ] {
            write(&repo.join(n), t);
        }
        std::fs::write(repo.join("bin.dat"), [0u8, 1, 2, 0, 255]).expect("bin");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());

        std::fs::remove_file(repo.join("gone.txt")).expect("rm");
        assert!(git(&repo, &["mv", "old.txt", "new.txt"]).status.success());
        std::fs::write(repo.join("bin.dat"), [255u8, 0, 9, 0, 1]).expect("bin2");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                repo.join("mode.txt"),
                std::fs::Permissions::from_mode(0o755),
            )
            .expect("chmod");
        }
        assert!(git(&repo, &["add", "-A"]).status.success());

        let changes = staged_changes(&repo).expect("changes");
        let touched = |p: &str| {
            changes
                .iter()
                .find(|c| c.path == p)
                .unwrap_or_else(|| panic!("{p} が拾えていない: {changes:?}"))
                .touched
                .clone()
        };
        let shape = Touched::Whole(WholeWhy::Shape);
        assert_eq!(touched("gone.txt"), shape, "削除");
        assert_eq!(touched("old.txt"), shape, "リネーム元も守る");
        assert_eq!(touched("new.txt"), shape, "リネーム先");
        // 二値の**内容変更**は「行で説明できない形」ではなく
        // 「ハンクが 1 つも出なかった」= **黙って劣化した**側。
        // 形で全体になったもの (削除・リネーム) と区別できること。
        assert_eq!(
            touched("bin.dat"),
            Touched::Whole(WholeWhy::NoHunks),
            "二値の内容変更は劣化として記録する"
        );
        assert_eq!(
            touched("bin.dat").degraded(),
            Some(WholeWhy::NoHunks),
            "劣化は文面へ出す対象"
        );
        assert_eq!(touched("gone.txt").degraded(), None, "形は劣化ではない");
        #[cfg(unix)]
        assert_eq!(touched("mode.txt"), shape, "モード変更");

        // 旧パスの担当が止められること (ここが `--name-only` では抜ける)
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "shapes-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "shapes-alice");
        seed_holder(&ledger, &repo, "alice", &alice, &["old.txt"]);
        assert!(matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)));
    }

    // ─────────── e2e: 本物の zai と本物の git commit で通す / 止める ───────────

    /// テストバイナリの隣に居る**本物の `zai`**。
    ///
    /// 単体テストに `CARGO_BIN_EXE_*` は無い (統合テストだけの仕組み) ので、
    /// `target/<profile>/deps/zai-<hash>` の 2 つ上から拾う。
    /// `cargo test` / `cargo nextest run` は bin も作るので CI では必ず居る。
    /// `cargo test --bin zai` だけを叩いた手元では居ないことがあるので、
    /// **飛ばしたことを必ず出す** (黙って緑になる試験は嘘をつく)。
    // 隣の `zai` を拾う関所は `test_util` に 1 つだけ持つ。
    // **版の照合だけでは古いバイナリを捕まえられない** — 版が同じまま中身が
    // 古い残骸で `guard` の実フック試験が「はみ出したのに通った」と嘘の赤を
    // 出した (実際に起きた)。`test_util::real_zai` は mtime まで見る。
    fn real_zai() -> Option<PathBuf> {
        crate::test_util::real_zai("実フック試験")
    }

    /// 子プロセスの `~` を temp へ寄せる (実 `~/.zaivern` に触れないため)。
    /// `dirs::home_dir` は unix が `$HOME`、Windows が `USERPROFILE` を見る。
    fn with_home<'a>(cmd: &'a mut Command, home: &Path) -> &'a mut Command {
        cmd.env("HOME", home).env("USERPROFILE", home)
    }

    /// **この機能の最終的な証拠。** 実リポジトリ・実フック・本物の `zai` で、
    /// 離れた行域を持つ担当が居ても `git commit` が通り、はみ出すと止まる。
    #[test]
    fn 本物のフック越しに離れた行域はコミットでき重なると止まる() {
        let Some((repo, lines)) = repo_with_300_lines("hook-span") else {
            return;
        };
        let Some(zai) = real_zai() else {
            eprintln!("本物の zai が見つからないので飛ばす (cargo test / nextest なら作られる)");
            return;
        };
        let home = crate::test_util::unique_temp_dir("zaivern-guard-test", "hook-span-home");
        let ledger = home.join(".zaivern").join("leases");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "hook-span-alice");
        seed_holder(&ledger, &repo, "alice", &alice, &["f.txt#L20-60"]);
        seed_holder(&ledger, &repo, "bob", &repo, &["f.txt#L200-240"]);
        install_with(&repo, &exe_text(&zai)).expect("設置");

        let commit = |msg: &str| {
            let t0 = std::time::Instant::now();
            let out = with_home(
                Command::new("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(["commit", "-m", msg]),
                &home,
            )
            .output()
            .expect("git commit");
            (out, t0.elapsed())
        };

        // bob が自分の域だけを直す → **通る** (ここが今まで止まっていた)
        restage_line(&repo, &lines, 220, "bob の担当を直した");
        let (ok, dt_ok) = commit("bob own region");
        assert!(
            ok.status.success(),
            "自分の行域だけのコミットが止まった:\n{}",
            String::from_utf8_lossy(&ok.stderr)
        );

        // alice の域へはみ出す → **止まる**。誰と重なったかが出る
        restage_line(&repo, &lines, 30, "alice の担当へはみ出した");
        let (ng, dt_ng) = commit("bob overflow");
        let err = String::from_utf8_lossy(&ng.stderr);
        assert!(!ng.status.success(), "はみ出したのに通った:\n{err}");
        for needle in ["alice", "f.txt#L20-60", "f.txt#L30"] {
            assert!(err.contains(needle), "文面に {needle} が無い:\n{err}");
        }
        eprintln!("フック込みの git commit: 許可 {dt_ok:?} / 拒否 {dt_ng:?}");
    }

    // ───────────────────────── パッチの読み取り (純粋) ─────────────────────────

    #[test]
    fn ハンクの見出しから新しい側の行域を起こす() {
        // (見出し, 期待する域)
        let table: &[(&str, Option<Span>)] = &[
            ("@@ -30 +30 @@ line 29", Some(Span::line(30))),
            (
                "@@ -205,2 +205,2 @@",
                Some(Span {
                    start: 205,
                    end: 206,
                }),
            ),
            ("@@ -151,0 +151 @@", Some(Span::line(151))),
            // 純粋な削除 (`+c,0`) は削除点の**直後**の行
            ("@@ -100 +99,0 @@ line 99", Some(Span::line(100))),
            ("@@ -1,3 +0,0 @@", Some(Span::line(1))),
            ("@@ -0,0 +1,2 @@", Some(Span { start: 1, end: 2 })),
            ("@@ こわれた @@", None),
        ];
        for (line, want) in table {
            assert_eq!(hunk_span(line), *want, "{line}");
        }
    }

    #[test]
    fn 近すぎるハンクは1つの域へ畳む() {
        let mut v = Vec::new();
        for s in [
            Span::line(10),
            Span::line(12), // 差 2 = SAFE_BAND 以内 → 畳む
            Span { start: 40, end: 45 },
            Span::line(48), // 差 3 = SAFE_BAND 以内 → 畳む
            Span::line(100),
        ] {
            push_span(&mut v, s);
        }
        assert_eq!(
            v,
            vec![
                Span { start: 10, end: 12 },
                Span { start: 40, end: 48 },
                Span::line(100),
            ]
        );
    }

    /// 生の出力を組み立てて、記録とパッチの対応が付いていることを固定する。
    /// **1 本目のファイルの行域が落ちる**事故 (記録とパッチの間の余分な NUL)
    /// はここでしか捕まらない。
    #[test]
    fn 記録とパッチの節はn番目どうしで対応する() {
        let raw = concat!(
            ":100644 100644 aaaaaaa bbbbbbb M\0src/a.rs\0",
            ":100644 100644 ccccccc ddddddd M\0src/b.rs\0",
            "\0"
        );
        let patch = concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "index aaaaaaa..bbbbbbb 100644\n",
            "--- a/src/a.rs\n",
            "+++ b/src/a.rs\n",
            "@@ -30 +30 @@ fn f()\n",
            "-old\n",
            "+new\n",
            "diff --git a/src/b.rs b/src/b.rs\n",
            "index ccccccc..ddddddd 100644\n",
            "--- a/src/b.rs\n",
            "+++ b/src/b.rs\n",
            "@@ -200,3 +200,3 @@\n",
            "-a\n-b\n-c\n+x\n+y\n+z\n",
        );
        let got = parse_staged(&format!("{raw}{patch}"));
        assert_eq!(
            got,
            vec![
                StagedChange {
                    path: "src/a.rs".into(),
                    touched: Touched::Lines(vec![Span::line(30)]),
                },
                StagedChange {
                    path: "src/b.rs".into(),
                    touched: Touched::Lines(vec![Span {
                        start: 200,
                        end: 202
                    }]),
                },
            ]
        );
    }

    /// 本文に `diff --git` や `@@` そっくりの行があっても崩れない
    /// (`-U0` では本文行が必ず `-` / `+` で始まる)。
    #[test]
    fn パッチ本文がdiffそっくりでも節を切り違えない() {
        let raw = ":100644 100644 aaaaaaa bbbbbbb M\0doc/patch.md\0\0";
        let patch = concat!(
            "diff --git a/doc/patch.md b/doc/patch.md\n",
            "--- a/doc/patch.md\n",
            "+++ b/doc/patch.md\n",
            "@@ -5,2 +5,2 @@\n",
            "-diff --git a/x b/x\n",
            "-@@ -1,9999 +1,9999 @@\n",
            "+diff --git a/y b/y\n",
            "+@@ -1,9999 +1,9999 @@\n",
        );
        assert_eq!(
            parse_staged(&format!("{raw}{patch}")),
            vec![StagedChange {
                path: "doc/patch.md".into(),
                touched: Touched::Lines(vec![Span { start: 5, end: 6 }]),
            }]
        );
    }

    /// CRLF でチェックアウトしたリポジトリでも同じ答えになること。
    #[test]
    fn crlfのパッチでも同じ行域になる() {
        let raw = ":100644 100644 aaaaaaa bbbbbbb M\0src/a.rs\0\0";
        let lf = concat!(
            "diff --git a/src/a.rs b/src/a.rs\n",
            "@@ -30,2 +30,2 @@\n",
            "-a\n-b\n+x\n+y\n",
        );
        let crlf = lf.replace('\n', "\r\n");
        assert_eq!(
            parse_staged(&format!("{raw}{lf}")),
            parse_staged(&format!("{raw}{crlf}"))
        );
    }

    /// 数が合わなければ**全部ファイル全体**へ倒す (行域が入る前の挙動)。
    #[test]
    fn 記録とパッチの数が合わなければ全部ファイル全体へ倒す() {
        let raw = concat!(
            ":100644 100644 aaaaaaa bbbbbbb M\0src/a.rs\0",
            ":100644 100644 ccccccc ddddddd M\0src/b.rs\0",
            "\0"
        );
        // 節が 1 つしか無い
        let patch = "diff --git a/src/a.rs b/src/a.rs\n@@ -30 +30 @@\n-o\n+n\n";
        let got = parse_staged(&format!("{raw}{patch}"));
        assert_eq!(got.len(), 2);
        assert!(got
            .iter()
            .all(|c| c.touched == Touched::Whole(WholeWhy::Unaligned)));
    }

    #[test]
    fn lfsのポインタは本文からも見出しの文脈からも判る() {
        // 1 行目 (`version`) が変わったとき = 本文に出る
        assert!(lfs_marked("+version https://git-lfs.github.com/spec/v1"));
        assert!(lfs_marked("-version https://git-lfs.github.com/spec/v1"));
        // `oid` / `size` だけが変わったとき = 見出しの文脈に出る
        assert!(lfs_marked(
            "@@ -2,2 +2,2 @@ version https://git-lfs.github.com/spec/v1"
        ));
        // 紛らわしい行を拾わない
        for line in [
            "--- a/version https://git-lfs.github.com/spec/v1",
            "+++ b/x",
            "@@ -1 +1 @@ fn main() {",
            "+// version https://git-lfs.github.com/spec/v1 と書いてあるだけ",
            "diff --git a/x b/x",
        ] {
            assert!(!lfs_marked(line), "{line}");
        }
    }

    #[test]
    fn 空の出力からは何も出さない() {
        assert!(parse_staged("").is_empty());
    }

    /// **速い照合を入れた以上、`lease::covers` とズレていないことを固定する。**
    /// 2 実装を持つと必ずズレるので、片方を持つなら差分テストを番人に置く。
    #[test]
    fn 速い照合とlease_coversは同じ答えを出す() {
        let pats = [
            "src/app.rs",
            "src/app.rs#L10-40",
            "src/app.rs#L1-",
            "src/auth/",
            "src/**",
            "src/*.rs",
            "src/**/mod.rs",
            "src/a?.rs",
            "src//./app.rs",
            "src/sub/../app.rs",
            "docs/読み物.md",
            "src/app.rs#fn:draw",
            "",
        ];
        let paths = [
            "src/app.rs",
            "src/App.rs",
            "src/auth/token.rs",
            "src/mod.rs",
            "src/sub/mod.rs",
            "src/ab.rs",
            "docs/読み物.md",
            "README.md",
            "src",
        ];
        for pat in pats {
            let p = match region::parse(pat) {
                Ok(r) => {
                    let n = lease::normalize_path(&r.path);
                    (!n.contains('*') && !n.contains('?')).then_some(n)
                }
                Err(_) => None,
            };
            let Some(plain) = p else { continue };
            for path in paths {
                assert_eq!(
                    plain == lease::normalize_path(path),
                    lease::covers(pat, path),
                    "速い照合がズレた: パターン {pat:?} / パス {path:?}"
                );
            }
        }
    }

    // ───────────────────────── 速さ (フックは人を待たせる場所) ─────────────────────────

    /// **大きなコミットでも線形に収まること。** 上限を数字で固定する。
    ///
    /// 測るのは git を呼ばない純粋部分 ([`parse_staged`] + [`collisions`])。
    /// git の起動時間は環境で 10 倍動くので混ぜると意味のある上限にならない。
    /// 負荷で揺れるため**複数回の最小値**で比べる (このリポジトリの流儀)。
    ///
    /// 実測 (macOS / debug ビルド・リース 200 件): 1,000 ハンク **11.4ms** /
    /// 20,000 ハンク **79.7ms**。入力 20 倍で 7 倍にしかならないのは、
    /// リースの前処理 ([`prepare`]) がハンク数に依らないため。
    #[test]
    fn 大きなコミットでも判定が線形に収まる() {
        let build = |files: usize, hunks: usize| {
            let mut raw = String::new();
            let mut patch = String::new();
            for f in 0..files {
                raw.push_str(&format!(":100644 100644 aaaaaaa bbbbbbb M\0src/f{f}.rs\0"));
                patch.push_str(&format!("diff --git a/src/f{f}.rs b/src/f{f}.rs\n"));
                for h in 0..hunks {
                    let at = 10 + h * 10;
                    patch.push_str(&format!("@@ -{at},2 +{at},2 @@\n-a\n-b\n+x\n+y\n"));
                }
            }
            raw.push('\0');
            format!("{raw}{patch}")
        };
        let store = {
            let mut s = lease::Store::default();
            let now = lease::now_secs();
            for i in 0..200 {
                s.leases.push(Lease {
                    holder: lease::Holder {
                        agent: format!("a{i}"),
                        session: String::new(),
                        cwd: format!("/w/{i}"),
                        pid: 0,
                    },
                    patterns: vec![format!("src/g{i}.rs#L1-40")],
                    anchors: Vec::new(),
                    acquired_at: now,
                    expires_at: now + 3600,
                    note: String::new(),
                });
            }
            s
        };
        let now = lease::now_secs();
        let tree = Path::new("/w/me");
        let run = |text: &str| {
            let t0 = std::time::Instant::now();
            let ch = parse_staged(text);
            let hits = collisions(&store, tree, &ch, now, &|_| false, &|_| None);
            assert!(hits.is_empty());
            t0.elapsed()
        };
        let small = build(20, 50); // 1,000 ハンク
        let big = build(400, 50); // 20,000 ハンク (20 倍)
        let mut t_small = std::time::Duration::MAX;
        let mut t_big = std::time::Duration::MAX;
        for _ in 0..3 {
            t_small = t_small.min(run(&small));
            t_big = t_big.min(run(&big));
        }
        eprintln!("判定の所要: 1,000 ハンク {t_small:?} / 20,000 ハンク {t_big:?}");
        // 上限は「人を待たせない」で切る。20 倍の入力で 1 秒を超えたら、
        // どこかで線形を外している (リースとの総当たりを増やした等)。
        assert!(
            t_big < std::time::Duration::from_secs(1),
            "20,000 ハンクの判定に {t_big:?} 掛かっている"
        );
        // 線形の証拠。20 倍の入力に対して 60 倍まで許す (測定の揺れの分)。
        let budget = t_small * 60 + std::time::Duration::from_millis(50);
        assert!(
            t_big < budget,
            "入力 20 倍で {t_big:?} (1 倍は {t_small:?}) — 線形を外している"
        );
    }

    /// 台帳に行域が 1 つも無いリポジトリでも、従来どおりの速さで戻ること。
    /// (行域の解釈は `region::parse` を 1 パターンにつき 1 回しか通さない)
    #[test]
    fn 行域が無い台帳でも従来どおりの速さで戻る() {
        let mut s = lease::Store::default();
        let now = lease::now_secs();
        for i in 0..200 {
            s.leases.push(Lease {
                holder: lease::Holder {
                    agent: format!("a{i}"),
                    session: String::new(),
                    cwd: format!("/w/{i}"),
                    pid: 0,
                },
                patterns: vec![format!("src/g{i}.rs")], // 行域なし
                anchors: Vec::new(),
                acquired_at: now,
                expires_at: now + 3600,
                note: String::new(),
            });
        }
        let changes: Vec<StagedChange> = (0..400)
            .map(|i| StagedChange {
                path: format!("src/f{i}.rs"),
                touched: Touched::Whole(WholeWhy::Shape),
            })
            .collect();

        // **時間で測らない。** 比で見ても、全 4273 件と同時に走らせると
        // 4 回に 1 回落ちた (2 点の測定が別の瞬間なので、負荷の谷と山を
        // 引くと比が跳ねる)。守りたいのは速さではなく
        // **`canonicalize` (システムコール) を何回呼ぶか**で、潰したかった
        // 回帰はそこを「パス × リース回」呼んでいた (400 × 200 で 5.04 秒)。
        // 回数なら機械の速さに 1 ミリも依存しない。
        CANON_CALLS.with(|c| c.set(0));
        let hits = collisions(&s, Path::new("/w/me"), &changes, now, &|_| false, &|_| None);
        assert!(hits.is_empty());
        let calls = CANON_CALLS.with(|c| c.get());
        eprintln!("400 パス × 200 リース: canonicalize {calls} 回");
        // **積で増えないこと**が要件。パスごと 1 回・リースごと 1 回までは要る
        // (実測 400 回 = 変更パス 1 つにつき 1 回)。回帰していた頃は
        // パス × リース = 400 × 200 = 80,000 回で、上限を桁で超える。
        let cap = changes.len() + s.leases.len() + 8;
        assert!(
            calls <= cap,
            "canonicalize がパス × リースで増えている: {calls} 回 (上限 {cap})"
        );
    }

    // ───────────────── 抜け道 (リンク / 属性 / 改行) ─────────────────

    /// リンクを 1 本作る。作れない環境 (Windows で開発者モードが無い等) は `false`。
    /// **両方の OS を実装する** — 片側だけ `cfg` を書くと、その OS では
    /// 一度もコンパイルされないまま「動くはず」になる。
    fn link(target: &Path, at: &Path, dir: bool) -> bool {
        #[cfg(unix)]
        {
            let _ = dir;
            std::os::unix::fs::symlink(target, at).is_ok()
        }
        #[cfg(windows)]
        {
            if dir {
                std::os::windows::fs::symlink_dir(target, at).is_ok()
            } else {
                std::os::windows::fs::symlink_file(target, at).is_ok()
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let (_, _, _) = (target, at, dir);
            false
        }
    }

    /// `src/f.txt` (300 行) と `lib -> src` を持つリポジトリ。
    /// リンクを作れない環境では `None` (検査を飛ばす)。
    fn repo_with_link(tag: &str) -> Option<(PathBuf, Vec<String>)> {
        let repo = temp_repo(tag)?;
        let lines: Vec<String> = (1..=300).map(|i| format!("line {i}")).collect();
        write(&repo.join("src/f.txt"), &format!("{}\n", lines.join("\n")));
        if !link(Path::new("src"), &repo.join("lib"), true) {
            std::fs::remove_dir_all(&repo).ok();
            return None;
        }
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());
        Some((repo, lines))
    }

    fn restage_at(repo: &Path, rel: &str, lines: &[String], n: usize, text: &str) {
        let mut v = lines.to_vec();
        v[n - 1] = text.to_string();
        write(&repo.join(rel), &format!("{}\n", v.join("\n")));
        assert!(git(repo, &["add", "-A"]).status.success());
    }

    /// **本命の穴。** 担当表をリンク越しの綴り (`lib/f.txt`) で書いた人が居ると、
    /// git が報告する実体の綴り (`src/f.txt`) と字句では別物なので、
    /// 別人が同じ行を触ってもガードが素通りしていた。
    #[test]
    fn リンク越しの綴りで確保された担当を実体のパスで止める() {
        let Some((repo, lines)) = repo_with_link("link-alias") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-alias-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-alias-a");
        // alice はリンク越しの綴りで確保している
        seed_holder(&ledger, &repo, "alice", &alice, &["lib/f.txt#L100-160"]);

        // 実体の綴りで、alice の域の中を触る → 止まる
        restage_at(&repo, "src/f.txt", &lines, 120, "bob wrote here");
        let v = check_staged_in(&repo, &ledger);
        assert!(
            matches!(v, Verdict::Deny(_)),
            "リンク越しの担当を実体のパスで止められていない: {v:?}"
        );

        // 離れた行なら通る (別名の解決が「常に止める」へ倒れていないこと)
        restage_at(&repo, "src/f.txt", &lines, 10, "far away");
        assert!(matches!(check_staged_in(&repo, &ledger), Verdict::Allow));

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&ledger).ok();
    }

    /// **交錯した書き込みを、コミットの関所が止める。**
    ///
    /// 触った 1 行が他人の 2 つの域に挟まれている形。組ごとの帯
    /// ([`region::SAFE_BAND`]) は全部満たしているので、**帯だけの判定では
    /// 素通りする**。ここが赤くなったら `region` が直した判定
    /// (`interleaved` / `interleave_safe`) が出荷経路から外れている。
    #[test]
    fn 交錯したコミットを止める() {
        let Some(repo) = temp_repo("interleave") else {
            return;
        };
        // 周期 6 の反復本文 — 錨 (ファイル内で唯一の行) が 1 本も無い。
        const POOL: [&str; 6] = ["```", "code line", "```", "", "---", ""];
        let lines: Vec<String> = (0..300).map(|i| POOL[i % 6].to_string()).collect();
        write(&repo.join("src/f.txt"), &format!("{}\n", lines.join("\n")));
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());

        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "interleave-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "interleave-a");
        seed_holder(
            &ledger,
            &repo,
            "alice",
            &alice,
            &["src/f.txt#L13-13", "src/f.txt#L25-25"],
        );
        // 前提: どの組も帯を満たす = 帯だけの判定は「素」と言う
        for l in [13u32, 25] {
            assert!(
                !region::spans_too_close(
                    &Span::line(17),
                    &Span::line(l),
                    region::SAFE_BAND
                ),
                "前提が崩れている: 17 と {l} は帯を満たすはず"
            );
        }
        // 17 行目 = alice の 2 つの域の**間**を触る
        restage_at(&repo, "src/f.txt", &lines, 17, "bob wrote here");
        match check_staged_in(&repo, &ledger) {
            Verdict::Deny(text) => {
                assert!(text.contains("交錯"), "交錯として断っていない: {text}");
                // **「近すぎる」と同じ顔をさせない** — 離しても直らない
                assert!(
                    text.contains("離しても直りません"),
                    "離せば直ると読める文面のまま: {text}"
                );
                assert!(
                    text.contains("と壁なしで並んでいます"),
                    "「重なります」のままになっている: {text}"
                );
            }
            v => panic!("交錯を通してしまった: {v:?}"),
        }
        // **離れた行でも、錨が 1 本も無い本文なら断る。** 0.16.0 まではここが
        // 通っていたが、削除・挿入が混ざると上下に分かれた組でも `git merge` は
        // 衝突する (`region::needs_wall` に実測)。
        restage_at(&repo, "src/f.txt", &lines, 200, "far away");
        assert!(
            matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)),
            "錨が無い本文で離れているだけの書き込みを通した"
        );
        // **壁があれば通る** (常に断るへ倒れていないこと)。同じ配置のまま、
        // 境目に一意な行を 2 本だけ植える。
        let mut walled = lines.clone();
        for i in [100usize, 150] {
            walled[i] = format!("UNIQ-{i}");
        }
        write(&repo.join("src/f.txt"), &format!("{}\n", walled.join("\n")));
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "wall"]).status.success());
        restage_at(&repo, "src/f.txt", &walled, 200, "far away");
        assert!(
            matches!(check_staged_in(&repo, &ledger), Verdict::Allow),
            "壁があるのに断った"
        );
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&ledger).ok();
    }

    /// 本文の読み取りは**ファイルにつき多くても 1 回**。
    ///
    /// 関所は書き込みのたびに走る短命プロセスなので、`anchor_lines`
    /// (ファイル全体の走査) を持ち主の数だけ払ってはいけない (設計原則 3)。
    /// 時間ではなく**読み取りの呼び出し回数**で固定する。
    ///
    /// 0.16.0 まではここが「交錯していなければ 0 回」だった。その門は
    /// 見逃す (`region::needs_wall` に実測) ので、いまは同じファイルを持つ
    /// 他人が居れば必ず 1 回読む。**他人が居なければ今も 0 回**。
    #[test]
    fn 本文はファイルにつき一度だけ読む() {
        let mut s = lease::Store::default();
        let now = lease::now_secs();
        s.leases.push(Lease {
            holder: lease::Holder {
                agent: "alice".into(),
                session: String::new(),
                cwd: "/w/alice".into(),
                pid: 0,
            },
            // 触る行 (100) を挟まない 2 つの域
            patterns: vec!["src/f.rs#L200-210".into(), "src/f.rs#L300-310".into()],
            anchors: Vec::new(),
            acquired_at: now,
            expires_at: now + 3600,
            note: String::new(),
        });
        let changes = vec![StagedChange {
            path: "src/f.rs".into(),
            touched: Touched::Lines(vec![Span {
                start: 100,
                end: 100,
            }]),
        }];
        // 同じファイルに他人の域が 2 本あっても、読むのは 1 回だけ。
        let reads = std::cell::Cell::new(0u32);
        let hits = collisions(&s, Path::new("/w/me"), &changes, now, &|_| false, &|_| {
            reads.set(reads.get() + 1);
            // 壁 (200 行目までのどこかで唯一の行) がある本文を返す
            Some((1..=400).map(|i| format!("line {i}\n")).collect::<String>())
        });
        assert!(hits.is_empty(), "壁があるのに止めた: {hits:?}");
        assert_eq!(reads.get(), 1, "同じファイルを持ち主の数だけ読んでいる");

        // **他人が同じファイルを持っていなければ 0 回のまま。**
        let other = vec![StagedChange {
            path: "src/g.rs".into(),
            touched: Touched::Lines(vec![Span {
                start: 100,
                end: 100,
            }]),
        }];
        let reads2 = std::cell::Cell::new(0u32);
        let hits2 = collisions(&s, Path::new("/w/me"), &other, now, &|_| false, &|_| {
            reads2.set(reads2.get() + 1);
            None
        });
        assert!(hits2.is_empty(), "別のファイルなのに止めた: {hits2:?}");
        assert_eq!(reads2.get(), 0, "他人が持っていないファイルの本文を読んだ");
    }

    /// リポジトリの中の綴りでリポジトリの外へ書く経路。
    ///
    /// **git はこの経路を index に入れない** (`beyond a symbolic link`) ので、
    /// コミット時のガードは一生見ない = 塞ぐ前は書き放題だった。
    /// 書く前に問い合わせる入口 (`zai guard check --path`) で止める。
    #[test]
    fn リポジトリ内のリンクで外へ出る書き込みは止める() {
        let Some((repo, _)) = repo_with_link("link-escape") else {
            return;
        };
        let away = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-escape-away");
        std::fs::create_dir_all(&away).expect("mkdir");
        if !link(&away, &repo.join("out"), true) {
            std::fs::remove_dir_all(&repo).ok();
            return;
        }
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-escape-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-escape-a");
        seed_holder(&ledger, &repo, "alice", &alice, &["src/f.txt"]);

        // 塞ぐ前の姿: リンク越しに書いても git は何も拾わない
        write(&away.join("x.rs"), "written through the link\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(
            matches!(check_staged_in(&repo, &ledger), Verdict::Allow),
            "コミット時のガードにはこの経路が見えない (だから書く前に止める)"
        );

        // 塞いだ後: 書く前の問い合わせが止める
        match check_path_in(&repo, &ledger, &repo.join("out/x.rs")) {
            Verdict::Deny(t) => assert!(t.contains("外"), "文面に理由が無い:\n{t}"),
            v => panic!("リンク越しの脱出が止まっていない: {v:?}"),
        }
        // リポジトリの外の絶対パスは関知しない (ここまで止めると誤爆する)
        assert!(matches!(
            check_path_in(&repo, &ledger, &away.join("x.rs")),
            Verdict::Allow
        ));

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&away).ok();
        std::fs::remove_dir_all(&ledger).ok();
    }

    /// **判定できないものは通さない** (fail-closed)。
    #[test]
    fn 輪になったリンクへの書き込みは通さない() {
        let Some((repo, _)) = repo_with_link("link-loop") else {
            return;
        };
        if !link(Path::new("b"), &repo.join("a"), false)
            || !link(Path::new("a"), &repo.join("b"), false)
        {
            std::fs::remove_dir_all(&repo).ok();
            return;
        }
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-loop-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-loop-a");
        seed_holder(&ledger, &repo, "alice", &alice, &["src/f.txt"]);
        match check_path_in(&repo, &ledger, &repo.join("a")) {
            Verdict::Deny(t) => assert!(t.contains("解けません"), "文面に理由が無い:\n{t}"),
            v => panic!("解けないリンクを通している: {v:?}"),
        }
        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&ledger).ok();
    }

    /// リンク**自体**の書き換えは 1 行のテキスト変更に見えるが、実際に起きるのは
    /// 「そのリンクを通る全てのパスの意味が変わる」ことなので、行の話ではない。
    #[test]
    fn リンク自体の書き換えはファイル全体として扱う() {
        let Some((repo, _)) = repo_with_link("link-retarget") else {
            return;
        };
        write(&repo.join("src/g.txt"), "g\n");
        if !link(Path::new("src/f.txt"), &repo.join("ln"), false) {
            std::fs::remove_dir_all(&repo).ok();
            return;
        }
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "link"]).status.success());

        std::fs::remove_file(repo.join("ln")).expect("rm");
        assert!(link(Path::new("src/g.txt"), &repo.join("ln"), false));
        assert!(git(&repo, &["add", "-A"]).status.success());

        let changes = staged_changes(&repo).expect("changes");
        let got = changes
            .iter()
            .find(|c| c.path == "ln")
            .unwrap_or_else(|| panic!("ln が拾えていない: {changes:?}"));
        assert_eq!(
            got.touched,
            Touched::Whole(WholeWhy::Shape),
            "リンクの張り替えを 1 行の変更として扱っている"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// `.` / `..` / 区切り / 大小の違いでゲートを迂回できないこと。
    /// 大小の期待値は本文のとおり FS 探針で分岐する (cfg では書かない)。
    #[test]
    fn ドットと区切りと大小の違いでゲートを回避できない() {
        let Some((repo, _)) = repo_with_link("link-spelling") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-spelling-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "link-spelling-a");
        seed_holder(&ledger, &repo, "alice", &alice, &["src/f.txt"]);

        let deny = |rel: &str| {
            matches!(
                check_path_in(&repo, &ledger, &repo.join(rel)),
                Verdict::Deny(_)
            )
        };
        for spelling in [
            "src/f.txt",
            "./src/f.txt",
            "src/./f.txt",
            "src/sub/../f.txt",
            "lib/f.txt", // リンク越しの綴り
        ] {
            assert!(deny(spelling), "{spelling} で回避できてしまう");
        }
        // 担当が居ないパスは通る (「常に止める」へ倒れていないこと)
        assert!(!deny("src/other.txt"));

        // 大小を畳む環境だけ、綴りの大小でも同じ実体になる。
        // 期待値は cfg (OS) ではなく**製品と同じ探針**で分岐する — 畳むかは
        // `worktree::fs_case_insensitive()` (実 FS 検査・プロセスに 1 回) が
        // 決めるので、cfg で書くと Docker-on-Mac (Linux + 大小非区別マウント)
        // でだけ嘘の赤が出る (pathx::tests::大小の違いで別物にならない と同型)。
        assert_eq!(
            deny("SRC/F.TXT"),
            crate::worktree::fs_case_insensitive(),
            "大小の扱いが FS 探針の答えと食い違っている"
        );
        // Windows の区切りでも迂回できない
        #[cfg(windows)]
        assert!(deny(r"src\f.txt"));

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&ledger).ok();
    }

    /// `merge=union` は**両方の行を残す**ドライバなので、同じ行域を 2 人が
    /// 触っても解決はぶつからない。ここを止めても得るものが無い。
    #[test]
    fn merge_unionのファイルは同じ行を重ねても止めない() {
        let Some(repo) = temp_repo("attr-union") else {
            return;
        };
        let lines: Vec<String> = (1..=60).map(|i| format!("line {i}")).collect();
        write(&repo.join(".gitattributes"), "u.txt merge=union\n");
        for f in ["u.txt", "n.txt"] {
            write(&repo.join(f), &format!("{}\n", lines.join("\n")));
        }
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());

        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "attr-union-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "attr-union-a");
        seed_holder(
            &ledger,
            &repo,
            "alice",
            &alice,
            &["u.txt#L1-50", "n.txt#L1-50"],
        );

        restage_at(&repo, "u.txt", &lines, 10, "union は衝突しない");
        assert!(
            matches!(check_staged_in(&repo, &ledger), Verdict::Allow),
            "merge=union を止めている"
        );
        // 同じ形でも属性の無いファイルは止まる (対照)
        restage_at(&repo, "n.txt", &lines, 10, "こちらは止まる");
        assert!(matches!(check_staged_in(&repo, &ledger), Verdict::Deny(_)));

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&ledger).ok();
    }

    /// **黙って劣化させない。** 行域が使えず全体扱いになったら、
    /// その理由を拒否の文面に出す (でないと何をずらせば通るのか判らない)。
    #[test]
    fn 行域を使えなかった理由を拒否の文面に出す() {
        let Some(repo) = temp_repo("attr-degrade") else {
            return;
        };
        write(&repo.join(".gitattributes"), "*.bin -diff\n");
        std::fs::write(repo.join("a.bin"), [0u8, 1, 2, 3, 4]).expect("write");
        write(
            &repo.join("big.psd"),
            "version https://git-lfs.github.com/spec/v1\noid sha256:abc\nsize 1\n",
        );
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());

        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "attr-degrade-l");
        let alice = crate::test_util::unique_temp_dir("zaivern-guard-test", "attr-degrade-a");
        seed_holder(&ledger, &repo, "alice", &alice, &["a.bin", "big.psd"]);

        // `-diff` 属性 = ハンクが 1 つも出ない
        std::fs::write(repo.join("a.bin"), [9u8, 9, 9]).expect("write");
        assert!(git(&repo, &["add", "-A"]).status.success());
        let changes = staged_changes(&repo).expect("changes");
        assert_eq!(
            changes
                .iter()
                .find(|c| c.path == "a.bin")
                .map(|c| &c.touched),
            Some(&Touched::Whole(WholeWhy::NoHunks))
        );
        match check_staged_in(&repo, &ledger) {
            Verdict::Deny(t) => {
                assert!(t.contains("行域で判定できない"), "劣化が伝わらない:\n{t}");
                assert!(t.contains("-diff"), "理由が出ていない:\n{t}");
            }
            v => panic!("止まっていない: {v:?}"),
        }

        // LFS のポインタ = 3 行だが中身は別の場所にある
        write(
            &repo.join("big.psd"),
            "version https://git-lfs.github.com/spec/v1\noid sha256:def\nsize 2\n",
        );
        assert!(git(&repo, &["add", "-A"]).status.success());
        let changes = staged_changes(&repo).expect("changes");
        assert_eq!(
            changes
                .iter()
                .find(|c| c.path == "big.psd")
                .map(|c| &c.touched),
            Some(&Touched::Whole(WholeWhy::Lfs)),
            "LFS のポインタを普通のテキストとして行域で扱っている"
        );
        match check_staged_in(&repo, &ledger) {
            Verdict::Deny(t) => assert!(t.contains("LFS"), "LFS だと伝わらない:\n{t}"),
            v => panic!("止まっていない: {v:?}"),
        }

        std::fs::remove_dir_all(&repo).ok();
        std::fs::remove_dir_all(&ledger).ok();
    }

    /// CRLF / BOM / 混在改行でも行番号がずれないこと。
    /// (`region.rs` が CRLF を吸収しているのと同じ答えになる)
    #[test]
    fn crlfとbomが混ざっても行番号がずれない() {
        let Some(repo) = temp_repo("eol") else {
            return;
        };
        let body = |mark: &str| {
            let mut s = String::from("\u{feff}"); // BOM
            for i in 1..=60 {
                // 奇数行は CRLF、偶数行は LF (混在)
                let eol = if i % 2 == 1 { "\r\n" } else { "\n" };
                let text = if i == 30 { mark } else { "line" };
                s.push_str(&format!("{text} {i}{eol}"));
            }
            s
        };
        write(&repo.join("m.txt"), &body("line"));
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());
        write(&repo.join("m.txt"), &body("CHANGED"));
        assert!(git(&repo, &["add", "-A"]).status.success());

        let changes = staged_changes(&repo).expect("changes");
        let got = changes
            .iter()
            .find(|c| c.path == "m.txt")
            .unwrap_or_else(|| panic!("m.txt が拾えていない: {changes:?}"));
        assert_eq!(
            got.touched,
            Touched::Lines(vec![Span { start: 30, end: 30 }]),
            "BOM と混在改行で行番号がずれている"
        );
        std::fs::remove_dir_all(&repo).ok();
    }

    /// 別名の解決を足しても、探るシステムコールが**パス × リースで増えない**こと。
    /// **実時間ではなく回数で固定する** (Docker の仮想 FS では時間が嘘をつく)。
    #[test]
    fn 別名の解決はパスとリースの積で増えない() {
        let mut s = lease::Store::default();
        let now = lease::now_secs();
        for i in 0..200 {
            s.leases.push(Lease {
                holder: lease::Holder {
                    agent: format!("a{i}"),
                    session: String::new(),
                    cwd: format!("/w/{i}"),
                    pid: 0,
                },
                patterns: vec![format!("src/g{i}.rs#L1-40")],
                anchors: Vec::new(),
                acquired_at: now,
                expires_at: now + 3600,
                note: String::new(),
            });
        }
        let changes: Vec<StagedChange> = (0..400)
            .map(|i| StagedChange {
                path: format!("src/f{i}.rs"),
                touched: Touched::Whole(WholeWhy::Shape),
            })
            .collect();
        let _ = crate::pathx::link_probes_take();
        let hits = collisions(&s, Path::new("/w/me"), &changes, now, &|_| false, &|_| None);
        assert!(hits.is_empty());
        let probes = crate::pathx::link_probes_take();
        eprintln!("400 パス × 200 リース: リンクを探る {probes} 回");
        // リースごと 1 回 + ディレクトリ 1 回まで。積で増えていれば桁で超える。
        let cap = s.leases.len() + 8;
        assert!(
            probes <= cap,
            "別名の解決がパス × リースで増えている: {probes} 回 (上限 {cap})"
        );
    }

    // ───────────────────────── e2e: 実際にコミットが止まる ─────────────────────────

    #[test]
    fn フックを設置すると実際にコミットが拒否される() {
        let Some(repo) = temp_repo("e2e") else {
            return;
        };
        // 1 本目のコミット (フック設置前) は普通に通す
        write(&repo.join("a.txt"), "1\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "init"]).status.success());

        // 「拒否する zai」を設置してコミットすると、止まる
        let bin = crate::test_util::unique_temp_dir("zaivern-guard-test", "e2e-bin");
        let deny_msg = "コミットを止めました (Zaivern Code のファイル所有ガード)";
        let stub = stub_zai(&bin, EXIT_DENY, deny_msg);
        install_with(&repo, &stub.to_string_lossy()).expect("install");

        write(&repo.join("b.txt"), "2\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        let out = git(&repo, &["commit", "-m", "should be blocked"]);
        assert!(!out.status.success(), "フックがコミットを止めていない");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains(deny_msg), "拒否理由が出ていない: {err}");
        // 本当にコミットされていない
        let log = git(&repo, &["log", "--oneline"]);
        assert_eq!(
            String::from_utf8_lossy(&log.stdout).lines().count(),
            1,
            "止めたはずのコミットが入っている"
        );
        // フックが渡した argv が CLI の契約どおりであること
        let argv = std::fs::read_to_string(bin.join("argv.log")).expect("argv.log");
        assert!(
            argv.lines().any(|l| l.trim() == "guard check --staged"),
            "フックが呼んだ引数が違う: {argv}"
        );

        // 「通す zai」へ差し替えると通る
        let stub = stub_zai(&bin, EXIT_OK, "");
        install_with(&repo, &stub.to_string_lossy()).expect("reinstall");
        assert!(
            git(&repo, &["commit", "-m", "allowed"]).status.success(),
            "許可のときまで止めている"
        );
    }

    #[test]
    fn zaiが居なくてもコミットは通る() {
        let Some(repo) = temp_repo("failopen") else {
            return;
        };
        let gone = repo.join("no-such-dir").join("zai");
        install_with(&repo, &gone.to_string_lossy()).expect("install");
        write(&repo.join("a.txt"), "1\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        let out = git(&repo, &["commit", "-m", "fail-open"]);
        assert!(
            out.status.success(),
            "ツールを消したらコミットできない、は許されない: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn 使い方の誤り以外の終了コードでも通す() {
        let Some(repo) = temp_repo("exit2") else {
            return;
        };
        let bin = crate::test_util::unique_temp_dir("zaivern-guard-test", "exit2-bin");
        // 2 = 使い方の誤り。**こちらの都合なのでコミットは通す。**
        let stub = stub_zai(&bin, EXIT_USAGE, "usage");
        install_with(&repo, &stub.to_string_lossy()).expect("install");
        write(&repo.join("a.txt"), "1\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        assert!(git(&repo, &["commit", "-m", "x"]).status.success());
    }

    #[test]
    fn 既存フックと連鎖する() {
        let Some(repo) = temp_repo("chain") else {
            return;
        };
        let dir = hooks_dir(&repo).expect("hooks");
        std::fs::create_dir_all(&dir).expect("mkdir");
        // husky 相当の既存フック。走った証拠を残し、成功で返す。
        let marker = repo.join("husky-ran.txt");
        let existing = dir.join("pre-commit");
        write(
            &existing,
            &format!(
                "#!/bin/sh\nprintf 'ran\\n' > {}\nexit 0\n",
                sh_quote(&existing_marker_path(&marker))
            ),
        );
        make_exec(&existing);

        let bin = crate::test_util::unique_temp_dir("zaivern-guard-test", "chain-bin");
        let stub = stub_zai(&bin, EXIT_OK, "");
        let done = install_with(&repo, &stub.to_string_lossy()).expect("install");
        assert_eq!(done.chained, vec!["pre-commit".to_string()]);

        write(&repo.join("a.txt"), "1\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        let out = git(&repo, &["commit", "-m", "chained"]);
        assert!(
            out.status.success(),
            "連鎖で落ちた: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(marker.exists(), "元のフックが呼ばれていない");

        // 元のフックが失敗したら、こちらも止まる (終了コードを尊重する)
        write(
            &dir.join(format!("pre-commit{PREV_SUFFIX}")),
            "#!/bin/sh\nexit 3\n",
        );
        write(&repo.join("b.txt"), "2\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        let out = git(&repo, &["commit", "-m", "prev fails"]);
        assert!(!out.status.success(), "元のフックの失敗を握り潰している");

        // 解除すると元のフックが戻る
        let rm = uninstall(&repo).expect("uninstall");
        assert_eq!(rm.restored, vec!["pre-commit".to_string()]);
        assert!(std::fs::read_to_string(&existing)
            .expect("read")
            .contains("exit 3"));
    }

    /// 実行権の無い元フックでも `sh` 経由で呼べること (husky の一部が該当する)。
    #[test]
    fn 実行権の無い元フックもsh経由で呼ぶ() {
        let Some(repo) = temp_repo("noexec") else {
            return;
        };
        let dir = hooks_dir(&repo).expect("hooks");
        std::fs::create_dir_all(&dir).expect("mkdir");
        write(&dir.join("pre-commit"), "#!/bin/sh\nexit 7\n"); // chmod しない
        let bin = crate::test_util::unique_temp_dir("zaivern-guard-test", "noexec-bin");
        let stub = stub_zai(&bin, EXIT_OK, "");
        install_with(&repo, &stub.to_string_lossy()).expect("install");
        write(&repo.join("a.txt"), "1\n");
        assert!(git(&repo, &["add", "-A"]).status.success());
        let out = git(&repo, &["commit", "-m", "x"]);
        assert!(
            !out.status.success(),
            "実行権の無い元フックが飛ばされている"
        );
    }

    fn existing_marker_path(p: &Path) -> String {
        p.to_string_lossy().replace('\\', "/")
    }

    // ───────────────────────── CLI ─────────────────────────

    #[test]
    fn cli_の終了コードは0と1と2の3種類だけ() {
        let v = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        // 使い方の誤り
        assert_eq!(cli_main(&[]), EXIT_USAGE);
        assert_eq!(cli_main(&v(&["ないよ"])), EXIT_USAGE);
        assert_eq!(cli_main(&v(&["check"])), EXIT_USAGE, "--staged 必須");
        assert_eq!(cli_main(&v(&["check", "--staged", "余分"])), EXIT_USAGE);
        // help は 0
        assert_eq!(cli_main(&v(&["--help"])), EXIT_OK);

        let Some(repo) = temp_repo("cli") else {
            return;
        };
        let r = repo.to_string_lossy().to_string();
        // 台帳が無いリポジトリ → 許可 (実 ~/.zaivern には触れない: 存在確認のみ)
        assert_eq!(cli_main(&v(&["check", "--staged", "--repo", &r])), EXIT_OK);
        // git リポジトリでない場所でも check は通す (fail-open)
        let plain = crate::test_util::unique_temp_dir("zaivern-guard-test", "cli-plain");
        assert_eq!(
            cli_main(&v(&[
                "check",
                "--staged",
                "--repo",
                &plain.to_string_lossy()
            ])),
            EXIT_OK
        );
        // status は読むだけなので通る
        assert_eq!(cli_main(&v(&["status", "--repo", &r])), EXIT_OK);
        assert_eq!(cli_main(&v(&["status", "--json", "--repo", &r])), EXIT_OK);
        // git リポジトリでない場所の status は使い方の誤り
        assert_eq!(
            cli_main(&v(&["status", "--repo", &plain.to_string_lossy()])),
            EXIT_USAGE
        );
        // uninstall は何も入っていなくても成功する
        assert_eq!(cli_main(&v(&["uninstall", "--repo", &r])), EXIT_OK);
    }

    #[test]
    fn statusはフックと台帳の状態を返す() {
        let Some(repo) = temp_repo("status") else {
            return;
        };
        let ledger = crate::test_util::unique_temp_dir("zaivern-guard-test", "status-l");
        let st = status_in(&repo, &ledger).expect("status");
        assert_eq!(st.missing.len(), HOOKS.len());
        assert!(st.installed.is_empty());
        assert!(!st.ledger_enabled);

        let other = crate::test_util::unique_temp_dir("zaivern-guard-test", "status-other");
        seed_ledger(&ledger, &repo, &other, &["src/"]);
        install_with(&repo, "/opt/zai").expect("install");
        let st = status_in(&repo, &ledger).expect("status 2");
        assert_eq!(st.installed.len(), HOOKS.len());
        assert!(st.missing.is_empty());
        assert!(st.ledger_enabled);
        assert_eq!(st.leases, 1);
        // 表示は空にならない
        let text = render_status(&st);
        assert!(text.contains("pre-commit"), "{text}");
        assert!(serde_json::to_string(&st).is_ok());
    }

    /// **配線の有無を明示する。**
    ///
    /// `cli.rs` の `is_cli_subcommand` に `"guard"` が無いあいだ、
    /// `zai guard check --staged` は「ワークスペース指定の GUI 起動」として
    /// 扱われる。その状態でフックを設置すると**コミットのたびにエディタが
    /// 立ち上がってフックが返らない**ので、`install` は必ず止める。
    /// 統合担当が `cli.rs` へ配線した瞬間、このテストは自動でもう一方の枝を守る。
    #[test]
    fn 配線されるまでinstallは自分を埋め込まない() {
        let Some(repo) = temp_repo("wired") else {
            return;
        };
        let wired = crate::cli::is_cli_subcommand("guard");
        match install(&repo) {
            Err(GuardError::NotWired) => assert!(
                !wired,
                "配線済みなのに NotWired を返した (install が古い判定を見ている)"
            ),
            other => {
                assert!(
                    wired,
                    "未配線なのに設置しようとした: {other:?} — フックが GUI を起動してしまう"
                );
                // 配線済みなら実際に設置できて、フックには自分の絶対パスが入る
                let done = other.expect("install");
                let text =
                    std::fs::read_to_string(done.hooks_dir.join("pre-commit")).expect("read");
                assert!(text.contains(&current_exe_for_hook().expect("exe")));
            }
        }
    }

    // ───────────────────────── レジストリ ─────────────────────────

    #[test]
    fn 機能登録の形が正しい() {
        assert_eq!(FEATURE.module, "guard");
        for e in FEATURE.entries {
            assert!(
                e.id.starts_with("guard."),
                "ID にモジュール接頭辞が無い: {}",
                e.id
            );
            assert!(!e.icon.trim().is_empty());
            assert!(!e.label.trim().is_empty());
        }
        assert!(FEATURE.draw.is_some(), "窓を描く経路が要る");
    }

    /// **共有ファイルを 1 バイトも触っていない**ことの番人。
    ///
    /// `src/features/guard.rs` は登録だけで、実体は `src/guard.rs`。
    /// ここが崩れると、並列ブランチが同じ行を奪い合う元の形へ戻る。
    #[test]
    fn 登録ファイルは再エクスポートだけ() {
        let src = include_str!("features/guard.rs").replace("\r\n", "\n");
        assert!(
            src.contains("#[path = \"../guard.rs\"]"),
            "実体への path が無い"
        );
        // **完全一致で固定しないこと。** 再エクスポートする物が増えるたびに
        // 落ちると、統合担当が「意図は満たしているのにテストが赤い」状態に
        // 追い込まれ、番人ごと消される (実際に `HELP` を足したとき落ちた)。
        // 見るのは「`pub use imp::{..}` の 1 行で、必要な物が載っている」だけ。
        let reexport = src
            .lines()
            .find(|l| l.trim_start().starts_with("pub use imp::{"))
            .unwrap_or_else(|| panic!("`pub use imp::{{..}}` の再エクスポートが無い"));
        for item in ["cli_main", "FEATURE"] {
            assert!(
                reexport.contains(item),
                "{item} が再エクスポートされていない: {reexport}"
            );
        }
        // 定義を写経していないこと (2 か所に持つとズレる)
        assert!(
            !src.contains("crate::feature::Feature {"),
            "FEATURE の定義を登録側へ写経している"
        );
    }
}
