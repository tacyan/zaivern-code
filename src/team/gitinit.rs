//! **Team Run が実測に使える基準点 (baseline) があるか**の判定と、その用意。
//!
//! ## なぜ「Git があるか」では足りないのか
//!
//! 実測 (`changeset`) は `git status` の意味 — 「HEAD と同じかどうか」— を
//! そのまま使う。だから**コミットが 1 つも無いリポジトリでは実測が成立しない**
//! (全ファイルが未追跡として並び、基準点が「全部汚れている」になる)。
//!
//! 前の版は `git init` → `add -A` → commit の順に走らせ、**commit が失敗した
//! 後にもう一度押すと `.git` があるので「準備完了」と誤判定**していた。その
//! 状態では基準点が無いまま Run が走り、完了報告の帰属判定も担当外変更の
//! 判定も壊れる (画面だけが「準備完了」と言う)。
//!
//! そこで見るのは**リポジトリの有無ではなく [`GitState`]**。`NoCommits` は
//! 「準備完了」ではなく「続きから作れる」状態として扱い、成功するまで
//! `needs_git` を下ろさない。
//!
//! ## 利用者のものを壊さない
//!
//! * **index を触らない。** 木は一時 index (`GIT_INDEX_FILE`) の上で組み、
//!   `commit-tree` + `update-ref` で HEAD を作る。途中で失敗しても利用者の
//!   index は 1 バイトも変わらない
//! * **既に HEAD があるリポジトリへは 1 コミットもしない。** 押しても
//!   「もう使える」と答えるだけ
//! * `reset --hard` / 強制 checkout / ファイルの削除は**一切しない**
//! * `.gitignore` は git の `add -A` がそのまま尊重する
//! * **明らかに危険な未追跡ファイル**があれば、黙って履歴へ入れずに
//!   名指しで断る ([`risky_paths`])。除外してから押し直せる

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Team Run から見た Git の状態。**「ある / ない」の 2 値にしない。**
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitState {
    /// Git 管理外 (`.git` が見つからない)。
    Absent,
    /// `git init` 済みだが HEAD が無い (コミットが 1 つも無い)。
    ///
    /// **ここが「準備完了」に見えていたのが不具合の本体。**
    NoCommits,
    /// HEAD あり・working tree clean。
    CleanHead,
    /// HEAD あり・変更あり (Run は始められる — 基準点はその時点で取る)。
    DirtyHead,
    /// bare リポジトリ (作業ツリーが無い)。Run は動かせない。
    Bare,
    /// git そのものが使えない / 応答が読めない。
    Unusable(String),
}

/// この状態でどうするか。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitPlan {
    /// もう使える (何もしない)。
    Ready,
    /// 基準点のコミットを作れば使える (`init` から / 途中から の両方)。
    NeedsBaseline,
    /// 用意できない。理由をそのまま人へ出す。
    Refuse(String),
}

/// ワークスペースと、そこを覆っているリポジトリの位置関係。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitPlace {
    pub state: GitState,
    /// リポジトリの作業ツリーの根 (見つかれば)。
    pub toplevel: Option<PathBuf>,
    /// ワークスペースが根そのものか (`false` = 親のリポジトリの中に居る)。
    pub at_toplevel: bool,
    /// linked worktree (`git worktree add` で作られた側) か。
    pub linked_worktree: bool,
}

/// 状態から次の一手を決める。**純関数** — 表で固定できる。
pub fn plan_for(p: &GitPlace) -> GitPlan {
    match &p.state {
        GitState::Unusable(why) => GitPlan::Refuse(format!("git を使えません: {why}")),
        GitState::Bare => GitPlan::Refuse(
            "bare リポジトリなので作業ツリーがありません。作業ツリーのあるフォルダを開いてください"
                .to_string(),
        ),
        // HEAD があれば実測は成立する。**汚れていてもよい** — 基準点は
        // 配る直前にその時点の状態で取るので、clean である必要は無い。
        // linked worktree も親のリポジトリ配下も、`git status` は
        // そのフォルダを基準に答えるのでそのまま使える。
        GitState::CleanHead | GitState::DirtyHead => GitPlan::Ready,
        GitState::Absent => GitPlan::NeedsBaseline,
        GitState::NoCommits => {
            // **親のリポジトリに HEAD を作らない。** ワークスペースの外の
            // ファイルまで巻き込むことになる。
            if p.at_toplevel {
                GitPlan::NeedsBaseline
            } else {
                GitPlan::Refuse(format!(
                    "このフォルダは {} のリポジトリの中にありますが、\
                     そのリポジトリにはコミットが 1 つもありません。\
                     先にそちらで最初のコミットを作ってください",
                    p.toplevel
                        .as_ref()
                        .map(|t| t.display().to_string())
                        .unwrap_or_else(|| "親".to_string())
                ))
            }
        }
    }
}

/// **危険そうな未追跡ファイル**の見分け (純関数)。
///
/// 完全な判定は不可能なので、**明らかなもの**だけを止める。止めるのは
/// 「黙って履歴へ入れない」ためで、利用者は `.gitignore` へ足してから
/// 押し直せる。判定は**ファイル名だけ**で行う (中身は読まない)。
///
/// 戻りは止める理由 (`None` = 入れてよい)。
pub fn risky_reason(path: &str) -> Option<&'static str> {
    let lower = path.to_ascii_lowercase();
    let name = lower.rsplit(['/', '\\']).next().unwrap_or(&lower);
    // 見本・ひな型は秘密ではない (`.env.example` を止めると誰も進めない)。
    const SAMPLES: &[&str] = &[".example", ".sample", ".template", ".dist"];
    let is_sample = SAMPLES.iter().any(|s| name.ends_with(s));
    if !is_sample && (name == ".env" || name.starts_with(".env.")) {
        return Some("環境変数ファイル");
    }
    if !is_sample {
        for key in ["id_rsa", "id_dsa", "id_ecdsa", "id_ed25519"] {
            if name == key {
                return Some("SSH 秘密鍵");
            }
        }
        for ext in [".pem", ".key", ".p12", ".pfx", ".jks", ".keystore"] {
            if name.ends_with(ext) {
                return Some("鍵ファイル");
            }
        }
        if name == "credentials" || name == "credentials.json" || name == ".netrc" || name == ".pgpass"
        {
            return Some("認証情報");
        }
        if name.starts_with("service-account") && name.ends_with(".json") {
            return Some("サービスアカウント鍵");
        }
    }
    None
}

/// 履歴へ入れる前に止めるものの一覧 (名前順・重複なし)。
pub fn risky_paths<'a>(untracked: impl IntoIterator<Item = &'a str>) -> Vec<(String, &'static str)> {
    let mut out: BTreeSet<(String, &'static str)> = BTreeSet::new();
    for p in untracked {
        if let Some(why) = risky_reason(p) {
            out.insert((p.to_string(), why));
        }
    }
    out.into_iter().collect()
}

/// `git` を 1 回走らせる (出力を文字列で返す)。
fn git(ws: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let mut c = std::process::Command::new("git");
    c.args(args)
        .current_dir(ws)
        .stdin(std::process::Stdio::null());
    if let Some(i) = index {
        // **利用者の index を触らない。** この 1 コマンドの間だけ差し替える。
        c.env("GIT_INDEX_FILE", i);
    }
    let out = c.output().map_err(|e| format!("git: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    Err(if err.is_empty() {
        format!("git {} が失敗しました", args.join(" "))
    } else {
        err
    })
}

/// いまの状態を調べる。**git を 1 度も呼べなければ [`GitState::Unusable`]。**
pub fn probe(ws: &Path) -> GitPlace {
    let mut place = GitPlace {
        state: GitState::Absent,
        toplevel: None,
        at_toplevel: false,
        linked_worktree: false,
    };
    // 作業ツリーの中か。ここが通らなければ Git 管理外 (または git が無い)。
    match git(ws, &["rev-parse", "--is-inside-work-tree"], None) {
        Ok(v) if v.trim() == "true" => {}
        Ok(_) => {
            // bare の中に居る (作業ツリーが無い)。
            place.state = GitState::Bare;
            return place;
        }
        Err(e) => {
            // 「リポジトリではない」と「git が無い」を混ぜない。
            place.state = if e.contains("not a git repository") || e.contains("Not a git repository")
            {
                GitState::Absent
            } else if e.starts_with("git: ") {
                GitState::Unusable(e)
            } else {
                GitState::Absent
            };
            return place;
        }
    }
    if git(ws, &["rev-parse", "--is-bare-repository"], None).as_deref() == Ok("true") {
        place.state = GitState::Bare;
        return place;
    }
    place.toplevel = git(ws, &["rev-parse", "--show-toplevel"], None)
        .ok()
        .map(PathBuf::from);
    place.at_toplevel = match (&place.toplevel, std::fs::canonicalize(ws)) {
        (Some(top), Ok(here)) => std::fs::canonicalize(top).map(|t| t == here).unwrap_or(false),
        _ => false,
    };
    // linked worktree は `.git` がファイル (親を指す) になっている。
    place.linked_worktree = place
        .toplevel
        .as_ref()
        .map(|t| t.join(".git").is_file())
        .unwrap_or(false);
    // HEAD があるか。**`rev-parse --verify HEAD` は 1 つ目のコミットが
    // 無いときだけ落ちる**ので、これが「基準点があるか」の答えそのもの。
    if git(ws, &["rev-parse", "--verify", "--quiet", "HEAD"], None).is_err() {
        place.state = GitState::NoCommits;
        return place;
    }
    let dirty = git(ws, &["status", "--porcelain"], None)
        .map(|s| !s.trim().is_empty())
        .unwrap_or(true);
    place.state = if dirty {
        GitState::DirtyHead
    } else {
        GitState::CleanHead
    };
    place
}

/// 基準点を用意したときの結末。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prepared {
    /// もともと使えた (何もしていない)。
    AlreadyReady,
    /// 基準点のコミットを作った。
    Committed,
}

/// **基準点を用意する (やり直せる)。**
///
/// 途中で失敗しても、次にもう一度呼べば続きから作る。利用者の index にも
/// 作業ツリーにも触らない — 木は一時 index の上で組み、`commit-tree` と
/// `update-ref` で HEAD を作る。
///
/// `Err` は「用意できなかった理由」。そのまま人へ出す。
pub fn prepare(ws: &Path) -> Result<Prepared, String> {
    if !ws.is_dir() {
        return Err("ワークスペースがありません".to_string());
    }
    let place = probe(ws);
    match plan_for(&place) {
        GitPlan::Ready => return Ok(Prepared::AlreadyReady),
        GitPlan::Refuse(why) => return Err(why),
        GitPlan::NeedsBaseline => {}
    }
    if place.state == GitState::Absent {
        git(ws, &["init"], None)?;
        // init 直後は必ず HEAD が無い。ここから先は `NoCommits` と同じ道。
    }
    // **危険そうな未追跡ファイルを黙って履歴へ入れない。**
    // `--exclude-standard` が `.gitignore` を尊重するので、除外済みは出ない。
    let untracked = git(ws, &["ls-files", "--others", "--exclude-standard"], None)?;
    let risky = risky_paths(untracked.lines().map(str::trim).filter(|l| !l.is_empty()));
    if !risky.is_empty() {
        let list: Vec<String> = risky
            .iter()
            .take(8)
            .map(|(p, why)| format!("{p} ({why})"))
            .collect();
        let more = risky.len().saturating_sub(list.len());
        return Err(format!(
            "秘密情報らしいファイルが履歴へ入りそうなので中止しました: {}{}。\
             .gitignore へ足すか別の場所へ移してから、もう一度押してください",
            list.join(" / "),
            if more > 0 {
                format!(" ほか {more} 件")
            } else {
                String::new()
            }
        ));
    }
    // 一時 index の上で木を組む (利用者の index は 1 バイトも変えない)。
    let tmp = tmp_index_path(ws);
    let _ = std::fs::remove_file(&tmp);
    let built = (|| -> Result<String, String> {
        git(ws, &["add", "-A"], Some(&tmp))?;
        git(ws, &["write-tree"], Some(&tmp))
    })();
    let _ = std::fs::remove_file(&tmp);
    let tree = built?;
    // **作者名は Run のためだけに与える。** 利用者の global 設定を触らない
    // (`-c` はこの 1 コマンドにしか効かない)。
    let commit = git(
        ws,
        &[
            "-c",
            "user.name=Zaivern",
            "-c",
            "user.email=zaivern@localhost",
            "commit-tree",
            &tree,
            "-m",
            "Zaivern: Team Run の基準点",
        ],
        None,
    )?;
    let head_ref =
        git(ws, &["symbolic-ref", "HEAD"], None).unwrap_or_else(|_| "refs/heads/main".to_string());
    git(ws, &["update-ref", &head_ref, &commit], None)?;
    // ここまで来て初めて利用者の index を HEAD へ揃える。**揃えないと
    // 「HEAD にあるのに index に無い」= 全ファイルが削除扱い**になる。
    // 内容は作業ツリーのまま (`read-tree` は作業ツリーを触らない)。
    git(ws, &["read-tree", "HEAD"], None)?;
    Ok(Prepared::Committed)
}

/// 一時 index の置き場 (`.git` の中。同時に押しても衝突しない名前)。
fn tmp_index_path(ws: &Path) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // `.git` が無い段 (init 前) では呼ばれないが、無ければ一時ディレクトリへ。
    let base = ws.join(".git");
    let dir = if base.is_dir() {
        base
    } else {
        std::env::temp_dir()
    };
    dir.join(format!(
        "zaivern-baseline-index.{}.{}",
        std::process::id(),
        stamp
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実験用のフォルダ (実 git を使う)。
    fn lab(tag: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-team-gitinit", tag)
    }

    fn run(ws: &Path, args: &[&str]) -> bool {
        std::process::Command::new("git")
            .args(args)
            .current_dir(ws)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    fn init_repo(ws: &Path) -> bool {
        if !run(ws, &["init", "-q"]) {
            return false;
        }
        run(ws, &["config", "user.email", "t@example.invalid"]);
        run(ws, &["config", "user.name", "t"]);
        true
    }

    fn git_available() -> bool {
        std::process::Command::new("git")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// **状態 → 次の一手**を表で固定する (git を呼ばない純関数)。
    #[test]
    fn 状態から次の一手が決まる() {
        let place = |state: GitState, at_top: bool| GitPlace {
            state,
            toplevel: Some(PathBuf::from("/w")),
            at_toplevel: at_top,
            linked_worktree: false,
        };
        // HEAD があれば使える (clean でも dirty でも、worktree でも親配下でも)
        for st in [GitState::CleanHead, GitState::DirtyHead] {
            assert_eq!(plan_for(&place(st.clone(), true)), GitPlan::Ready, "{st:?}");
            assert_eq!(plan_for(&place(st.clone(), false)), GitPlan::Ready, "{st:?}");
        }
        // Git 管理外 → 作る
        assert_eq!(
            plan_for(&place(GitState::Absent, false)),
            GitPlan::NeedsBaseline
        );
        // **HEAD 無しは「準備完了」ではない。** 続きから作る。
        assert_eq!(
            plan_for(&place(GitState::NoCommits, true)),
            GitPlan::NeedsBaseline
        );
        // 親のリポジトリに HEAD が無いときは断る (外のファイルを巻き込む)
        assert!(matches!(
            plan_for(&place(GitState::NoCommits, false)),
            GitPlan::Refuse(_)
        ));
        // bare と git が無い環境も断る
        assert!(matches!(
            plan_for(&place(GitState::Bare, true)),
            GitPlan::Refuse(_)
        ));
        assert!(matches!(
            plan_for(&place(GitState::Unusable("no git".into()), true)),
            GitPlan::Refuse(_)
        ));
    }

    /// **秘密情報らしい名前**の見分け (中身は読まない)。
    #[test]
    fn 危険そうな未追跡ファイルを名前で見分ける() {
        let risky: &[&str] = &[
            ".env",
            ".env.local",
            "config/.env.production",
            "id_rsa",
            "deep/dir/id_ed25519",
            "certs/server.pem",
            "a/b/private.key",
            "keystore.jks",
            "credentials.json",
            ".netrc",
            "service-account-prod.json",
        ];
        for p in risky {
            assert!(risky_reason(p).is_some(), "{p} を通した");
        }
        let fine: &[&str] = &[
            ".env.example",
            ".env.sample",
            ".env.template",
            "src/main.rs",
            "id_rsa.pub",
            "README.md",
            "keys.md",
            "package.json",
            "environment.rs",
        ];
        for p in fine {
            assert_eq!(risky_reason(p), None, "{p} を止めた");
        }
        // 一覧は名前順・重複なし
        let got = risky_paths([".env", "src/a.rs", ".env"]);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].0, ".env");
    }

    /// **Git 管理外から基準点を作れる。** 作業ツリーは 1 バイトも変えない。
    #[test]
    fn git管理外から基準点を作る() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        let ws = lab("from-scratch");
        std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
        std::fs::create_dir_all(ws.join("sub")).unwrap();
        std::fs::write(ws.join("sub/b.rs"), "// b").unwrap();
        assert_eq!(probe(&ws).state, GitState::Absent);
        assert_eq!(prepare(&ws).expect("作れる"), Prepared::Committed);
        let p = probe(&ws);
        assert_eq!(p.state, GitState::CleanHead, "作った直後は clean のはず");
        assert!(p.at_toplevel);
        // 中身はそのまま
        assert_eq!(
            std::fs::read_to_string(ws.join("a.rs")).unwrap(),
            "fn main() {}"
        );
        // 実測の基準点として使える
        let base = super::super::changeset::capture_baseline(&ws).expect("基準点");
        assert!(base.usable());
        // もう一度押しても何もしない
        assert_eq!(prepare(&ws).expect("再実行"), Prepared::AlreadyReady);
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **commit だけ失敗した後でも、次の実行で HEAD を作れる。**
    ///
    /// 失敗は決定的に作る: `commit-tree` は `user.email` が無くても `-c` で
    /// 渡すので落ちない。そこで**`git init` 済み・HEAD 無し**という、
    /// 前の版が「準備完了」と誤判定していた状態そのものを作って続きを見る。
    #[test]
    fn init済みhead無しからやり直せる() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        let ws = lab("resume");
        std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
        assert!(init_repo(&ws), "git init できる");
        // **ここが誤判定されていた状態。**
        assert_eq!(probe(&ws).state, GitState::NoCommits);
        assert_ne!(
            plan_for(&probe(&ws)),
            GitPlan::Ready,
            "HEAD 無しを準備完了と判定した"
        );
        // **害はここに出る。** HEAD が無いと `git status` は全ファイルを
        // 未追跡として並べるので、基準点が「全部汚れている」になる。
        let before = super::super::changeset::capture_baseline(&ws).expect("取れはする");
        assert!(
            !before.entries.is_empty(),
            "前提が崩れている: HEAD 無しなのに綺麗な基準点が取れた"
        );
        // 続きから作れる
        assert_eq!(prepare(&ws).expect("続きから作れる"), Prepared::Committed);
        assert_eq!(probe(&ws).state, GitState::CleanHead);
        // 基準点が「綺麗」になる = 実測が意味を持つ。
        let after = super::super::changeset::capture_baseline(&ws).expect("基準点");
        assert!(after.usable());
        assert!(
            after.entries.is_empty(),
            "基準点を作ったのに汚れたまま: {:?}",
            after.entries
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **既に HEAD があるリポジトリへは 1 コミットもしない。**
    /// staged の中身も working tree も変えない。
    #[test]
    fn 既存headのリポジトリを勝手にcommitしない() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        let ws = lab("existing-head");
        assert!(init_repo(&ws), "git init");
        std::fs::write(ws.join("a.rs"), "one").unwrap();
        assert!(run(&ws, &["add", "a.rs"]));
        assert!(run(&ws, &["commit", "-qm", "first"]));
        let head_before = git(&ws, &["rev-parse", "HEAD"], None).expect("HEAD");
        // 人が途中まで staging している状態を作る
        std::fs::write(ws.join("b.rs"), "two").unwrap();
        std::fs::write(ws.join("c.rs"), "three").unwrap();
        assert!(run(&ws, &["add", "b.rs"]));
        let staged_before = git(&ws, &["diff", "--cached", "--name-only"], None).expect("staged");
        assert_eq!(staged_before, "b.rs", "前提: b.rs だけ staged");

        assert_eq!(prepare(&ws).expect("使える"), Prepared::AlreadyReady);
        assert_eq!(
            git(&ws, &["rev-parse", "HEAD"], None).expect("HEAD"),
            head_before,
            "既存のリポジトリへコミットした"
        );
        assert_eq!(
            git(&ws, &["diff", "--cached", "--name-only"], None).expect("staged"),
            staged_before,
            "利用者の staging を変えた"
        );
        assert_eq!(std::fs::read_to_string(ws.join("c.rs")).unwrap(), "three");
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **`.gitignore` のものは履歴へ入れない。**
    #[test]
    fn gitignore対象は基準点へ入れない() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        let ws = lab("ignore");
        std::fs::write(ws.join(".gitignore"), "secret.txt\ntarget/\n").unwrap();
        std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(ws.join("secret.txt"), "とても大事").unwrap();
        std::fs::create_dir_all(ws.join("target")).unwrap();
        std::fs::write(ws.join("target/big.bin"), "x".repeat(1024)).unwrap();
        assert_eq!(prepare(&ws).expect("作れる"), Prepared::Committed);
        let tracked = git(&ws, &["ls-tree", "-r", "--name-only", "HEAD"], None).expect("一覧");
        let files: Vec<&str> = tracked.lines().collect();
        assert!(files.contains(&"a.rs"), "{files:?}");
        assert!(files.contains(&".gitignore"), "{files:?}");
        assert!(!files.contains(&"secret.txt"), "無視対象を入れた: {files:?}");
        assert!(
            !files.iter().any(|f| f.starts_with("target/")),
            "無視対象を入れた: {files:?}"
        );
        // 中身は消えていない
        assert_eq!(
            std::fs::read_to_string(ws.join("secret.txt")).unwrap(),
            "とても大事"
        );
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **秘密情報らしいファイルがあれば、黙ってコミットせず名指しで断る。**
    /// 断った後も利用者のファイルは 1 つも消えない。
    #[test]
    fn 秘密情報らしいファイルは無断でcommitしない() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        let ws = lab("secrets");
        std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(ws.join(".env"), "TOKEN=abcdef").unwrap();
        let err = prepare(&ws).expect_err("止めるべき");
        assert!(err.contains(".env"), "何を止めたのか言っていない: {err}");
        assert!(err.contains(".gitignore"), "逃げ道を示していない: {err}");
        // **HEAD は作られていない** (中途半端に進めない)
        assert_eq!(probe(&ws).state, GitState::NoCommits);
        // ファイルは無事
        assert_eq!(std::fs::read_to_string(ws.join(".env")).unwrap(), "TOKEN=abcdef");
        assert_eq!(
            std::fs::read_to_string(ws.join("a.rs")).unwrap(),
            "fn main() {}"
        );
        // 除外すれば通る
        std::fs::write(ws.join(".gitignore"), ".env\n").unwrap();
        assert_eq!(prepare(&ws).expect("除外したら作れる"), Prepared::Committed);
        let tracked = git(&ws, &["ls-tree", "-r", "--name-only", "HEAD"], None).expect("一覧");
        assert!(!tracked.lines().any(|f| f == ".env"), "{tracked}");
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **親のリポジトリ配下と linked worktree を正しく扱う。**
    #[test]
    fn 親リポジトリ配下とworktreeを見分ける() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        // 親に HEAD がある → 子フォルダはそのまま使える (コミットしない)
        let parent = lab("parent");
        assert!(init_repo(&parent), "git init");
        std::fs::write(parent.join("a.rs"), "one").unwrap();
        assert!(run(&parent, &["add", "-A"]));
        assert!(run(&parent, &["commit", "-qm", "first"]));
        let child = parent.join("sub");
        std::fs::create_dir_all(&child).unwrap();
        let p = probe(&child);
        assert!(matches!(p.state, GitState::CleanHead | GitState::DirtyHead));
        assert!(!p.at_toplevel, "親配下だと分かっていない");
        assert_eq!(plan_for(&p), GitPlan::Ready);
        let head_before = git(&parent, &["rev-parse", "HEAD"], None).expect("HEAD");
        assert_eq!(prepare(&child).expect("使える"), Prepared::AlreadyReady);
        assert_eq!(
            git(&parent, &["rev-parse", "HEAD"], None).expect("HEAD"),
            head_before,
            "親のリポジトリへコミットした"
        );

        // 親に HEAD が無い → 断る (外のファイルを巻き込まない)
        let bare_parent = lab("parent-no-head");
        assert!(init_repo(&bare_parent), "git init");
        let bare_child = bare_parent.join("sub");
        std::fs::create_dir_all(&bare_child).unwrap();
        let p = probe(&bare_child);
        assert_eq!(p.state, GitState::NoCommits);
        assert!(!p.at_toplevel);
        assert!(prepare(&bare_child).is_err(), "親へ HEAD を作ってしまった");
        assert_eq!(probe(&bare_parent).state, GitState::NoCommits);

        // linked worktree → HEAD があるのでそのまま使える
        let wt = parent.parent().unwrap_or(Path::new(".")).join(format!(
            "{}-wt",
            parent.file_name().and_then(|n| n.to_str()).unwrap_or("wt")
        ));
        if run(
            &parent,
            &["worktree", "add", "-q", &wt.to_string_lossy(), "-b", "zv-wt"],
        ) {
            let p = probe(&wt);
            assert!(
                matches!(p.state, GitState::CleanHead | GitState::DirtyHead),
                "worktree の状態を読めない: {p:?}"
            );
            assert!(p.linked_worktree, "linked worktree だと分かっていない");
            assert_eq!(plan_for(&p), GitPlan::Ready);
            assert_eq!(prepare(&wt).expect("使える"), Prepared::AlreadyReady);
            let _ = run(&parent, &["worktree", "remove", "--force", &wt.to_string_lossy()]);
            std::fs::remove_dir_all(&wt).ok();
        } else {
            println!("[skip] worktree を作れませんでした");
        }
        std::fs::remove_dir_all(&parent).ok();
        std::fs::remove_dir_all(&bare_parent).ok();
    }

    /// **bare リポジトリは断る。**
    #[test]
    fn bareリポジトリは断る() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        let ws = lab("bare");
        assert!(run(&ws, &["init", "-q", "--bare"]), "bare init");
        assert_eq!(probe(&ws).state, GitState::Bare);
        assert!(prepare(&ws).is_err(), "bare で作ってしまった");
        std::fs::remove_dir_all(&ws).ok();
    }

    /// **一時 index を残さない。** 成功しても失敗しても。
    #[test]
    fn 一時indexを残さない() {
        if !git_available() {
            println!("[skip] git がありません");
            return;
        }
        let ws = lab("tmp-index");
        std::fs::write(ws.join("a.rs"), "fn main() {}").unwrap();
        assert_eq!(prepare(&ws).expect("作れる"), Prepared::Committed);
        let left: Vec<String> = std::fs::read_dir(ws.join(".git"))
            .expect("読める")
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("zaivern-baseline-index"))
            .collect();
        assert!(left.is_empty(), "一時 index が残った: {left:?}");
        std::fs::remove_dir_all(&ws).ok();
    }
}
