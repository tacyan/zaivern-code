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
    let out =
        crate::worktree::git_out(repo, &["rev-parse", "--git-path", "hooks"]).map_err(GuardError::Git)?;
    if out.is_empty() {
        return Err(GuardError::Git(tr("フックの置き場を git から取得できません")));
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

/// ステージ済みのパス。**`-z` 必須** — 空白 / 日本語 / 改行入りのパスで
/// 壊れないため (`--name-only` は `-z` が無いと引用符でくるんで出す)。
///
/// パスは作業ツリーの頂点からの相対で返る (`-C <頂点>` で走らせるので
/// `diff.relative = true` が設定してあっても結果は変わらない)。
pub fn staged_paths(repo: &Path) -> Result<Vec<String>, GuardError> {
    let out = crate::worktree::git_out(repo, &["diff", "--cached", "--name-only", "-z"])
        .map_err(GuardError::Git)?;
    Ok(out
        .split('\0')
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect())
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
    let Ok(paths) = staged_paths(repo) else {
        return Verdict::Allow; // git が答えない = こちらの都合
    };
    if paths.is_empty() {
        return Verdict::Allow;
    }
    let now = lease::now_secs();
    let alive = |p: u32| crate::instances::pid_alive(p);
    let mut hits: Vec<(String, Lease)> = Vec::new();
    for rel in &paths {
        for l in &st.leases {
            if !l.active(now, &alive) || !l.covers_path(rel) {
                continue;
            }
            if !holder_is_me(l, &roots.tree) {
                hits.push((rel.clone(), l.clone()));
            }
            // 最初に当たったリースで決める (自分のものなら、このパスは通す)。
            break;
        }
    }
    if hits.is_empty() {
        Verdict::Allow
    } else {
        Verdict::Deny(deny_text(&hits, now))
    }
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

/// 実体まで解決してから台帳の正規形へ。比較専用のキー。
fn canon_key(p: &Path) -> String {
    let c = p.canonicalize().unwrap_or_else(|_| p.to_path_buf());
    lease::normalize_path(&c.to_string_lossy())
}

/// 拒否の文面。**「拒否されました」だけでは、ユーザーは機能を切るだけ。**
/// どのパスを・誰が・いつから持っていて・どうすれば良いかを必ず出す。
fn deny_text(hits: &[(String, Lease)], now: u64) -> String {
    let mut lines = String::new();
    for (rel, l) in hits.iter().take(MAX_LISTED) {
        let since = crate::instances::humanize_uptime(now.saturating_sub(l.acquired_at));
        let left = crate::instances::humanize_uptime(l.expires_at.saturating_sub(now));
        lines.push_str(&trf(
            "  {path} — {owner} が保有 ({since}前から / 期限まであと {left}){note}\n",
            &[
                ("path", rel.clone()),
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
    }
    if hits.len() > MAX_LISTED {
        lines.push_str(&trf(
            "  ほか {n} 件\n",
            &[("n", (hits.len() - MAX_LISTED).to_string())],
        ));
    }
    trf(
        "コミットを止めました (Zaivern Code のファイル所有ガード)。\n\
         以下のファイルは、いま別の担当が保有しています:\n\
         \n\
         {list}\n\
         同じファイルを 2 人が同時に触ると、衝突はマージのときまで見えません。\n\
         対処:\n\
         \x20 (1) 相手の完了を待ってから、もう一度コミットする\n\
         \x20 (2) 担当を分ける — 別のファイル / 別のディレクトリを受け持つ\n\
         \x20 (3) 引き継ぐ: `zai lease list` で確認し、`zai lease release --agent <名前>` で解放する\n\
         \x20 (4) このコミットだけ通す: `git commit --no-verify` (衝突は残ります)\n\
         \x20 (5) ガードを外す: `zai guard uninstall`",
        &[("list", lines)],
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
    st.leases = lease::read_store(&store).map(|s| s.leases.len()).unwrap_or(0);
    for name in HOOKS {
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(b) if is_ours(&String::from_utf8_lossy(&b)) => st.installed.push((*name).to_string()),
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
            Err(e) => return usage(&trf("カレントディレクトリが判りません: {e}", &[("e", e.to_string())])),
        },
    };
    match sub {
        "check" => {
            let (staged, rest) = take_flag(&rest, "--staged");
            if !staged {
                return usage(&tr("`--staged` を付けてください: zai guard check --staged"));
            }
            if let Some(x) = rest.first() {
                return usage(&trf("余分な引数です: {x}", &[("x", x.clone())]));
            }
            // ここから先は**何があっても通す**。判定できないことを理由に
            // ユーザーのコミットを止めない。
            let Ok(repo) = repo_root(&start) else {
                return EXIT_OK;
            };
            match check_staged(&repo) {
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
    settings: &[],
    binds: &[],
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
        let roots = lease::roots_of(repo);
        let store = lease::store_path_in(ledger, &roots.key);
        std::fs::create_dir_all(ledger).expect("mkdir ledger");
        lease::enable(&store).expect("enable");
        let holder = lease::Holder {
            agent: "その他の担当".into(),
            session: String::new(),
            cwd: lease::normalize_path(&owner_cwd.to_string_lossy()),
            pid: 0,
        };
        let pats: Vec<String> = patterns.iter().map(|s| (*s).to_string()).collect();
        let now = lease::now_secs();
        lease::with_store(&store, |s| {
            lease::try_claim(s, &holder, &pats, now, lease::DEFAULT_TTL_SECS, &|_| false)
        })
        .expect("claim");
        store
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
        for bad in ["[[", "]]", "function ", "local ", "$'", "==", "&>", "source "] {
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
        let p = PathBuf::from(if cfg!(windows) { r"C:\a b\zai.exe" } else { "/a b/zai" });
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

        assert!(holder_is_me(&lease_for(&tree), &tree), "同じツリーを自分だと判定できない");
        assert!(
            holder_is_me(&lease_for(&sub), &tree),
            "部分ディレクトリで動く担当を別人にしている"
        );
        assert!(!holder_is_me(&lease_for(&other), &tree), "別ツリーを自分にしている");
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
        let text = deny_text(&[("src/app.rs".to_string(), l)], now);
        for needle in [
            "src/app.rs",
            "claude",
            "保有",
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
        let mk = |i: usize| {
            (
                format!("src/f{i}.rs"),
                Lease {
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
            )
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
        write(&dir.join("pre-commit.zaivern-prev"), "#!/bin/sh\necho old\n");
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
        let got = staged_paths(&repo).expect("staged");
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
        assert_eq!(staged_paths(&repo).expect("staged"), vec!["a.txt"]);
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
        assert_eq!(std::fs::read_dir(&ledger).map(|d| d.count()).unwrap_or(0), 0);
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
                assert!(!reason.contains("src/other.rs"), "他人の物でないパスまで挙げている");
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
        assert!(matches!(
            check_staged_in(&repo, &ledger),
            Verdict::Deny(_)
        ));
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
        assert!(!out.status.success(), "実行権の無い元フックが飛ばされている");
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
        assert!(src.contains("#[path = \"../guard.rs\"]"), "実体への path が無い");
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
