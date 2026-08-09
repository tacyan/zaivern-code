//! チェックポイントと巻き戻し。
//!
//! エージェントへ指示を送る**直前**に作業ツリーの姿を記録し、暴走した変更を
//! 確定的に戻せるようにする。承認キューは「通す前」しか守れないので、
//! 「通した後」の最後の砦がここ。
//!
//! # ユーザーの手元を壊さない
//!
//! スナップショットは **`.git/index` にも作業ツリーにも触れない**。
//! `GIT_INDEX_FILE` で一時 index を指し、その中だけで
//! `read-tree` → `add -A` → `write-tree` → `commit-tree` を行い、
//! 得たコミットを `refs/zaivern/checkpoints/…` へ記録する。
//! ユーザーがステージした内容は前後で 1 ビットも変わらない。
//! `git stash` は使わない (stash スタックは全ワークツリー共有で、
//! 同時に動いている別インスタンスの退避を巻き込む)。
//!
//! # 既知の制限 (UI にも明記すること)
//!
//! - **スナップショットに無かったファイルは消さない。** 復元は「あった物を
//!   戻す」だけで、「無かった物を消す」はしない。消す方向は取り返しが
//!   つかないため、意図的に持たない。
//! - `.gitignore` されたファイルは記録されない (`git add -A` の対象外)。
//!   ビルド成果物まで抱えるとスナップショットが現実的な速度で終わらない。
//!
//! # スレッド
//!
//! git は**必ず裏のスレッド**で走らせ、UI は `mpsc` で結果を受ける。
//! UI スレッドで git の完了を待つと、巨大な作業ツリーでフレームが数秒止まる
//! (`git.rs` の `Git::branch` / `Git::line_marks` と同じ理由)。

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::mpsc::{self, Receiver};

use crate::i18n::{tr, trf};

/// チェックポイントを置く ref の名前空間。`refs/heads/` の外なので
/// ブランチ一覧にも `git log --all` の既定にも現れない。
const REF_PREFIX: &str = "refs/zaivern/checkpoints/";

/// コミットメッセージ (= 件名) の先頭に置く印。
const SUBJECT_PREFIX: &str = "zaivern-checkpoint: ";

/// 件名の中でエージェント名と指示要約を区切る文字列。
/// 両側からこの文字は取り除いてから組むので、分割は一意に決まる。
const FIELD_SEP: &str = " | ";

/// 保持するチェックポイントの上限。超えた分は古い順に ref を消す。
///
/// ref を消してもオブジェクトは即座には消えず、`git gc` の到達不能判定に
/// 委ねられる。数が増えても `for-each-ref` は速いが、一覧が読めなくなるので
/// 人間が選べる件数で頭を打つ。
pub const MAX_CHECKPOINTS: usize = 50;

/// 1 回の復元で書き戻すファイル数の上限。これを超えるものは
/// 「巻き戻し」ではなくブランチ操作の領分なので受け付けない。
const MAX_RESTORE_FILES: usize = 5000;

/// commit-tree に渡す身元。git の設定が無いマシンでも失敗させないため、
/// 環境変数で毎回明示する (ユーザーの `user.name` は読まないし変えない)。
const IDENT_NAME: &str = "Zaivern Code";
const IDENT_EMAIL: &str = "checkpoint@zaivern.invalid";

// ══════════════════════════════════════════════════════════════════
//  データ
// ══════════════════════════════════════════════════════════════════

/// 記録済みのチェックポイント 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Checkpoint {
    /// コミット ID (`commit-tree` の結果)。
    pub sha: String,
    /// `refs/zaivern/checkpoints/…` の完全名。削除に使う。
    pub refname: String,
    /// 記録時刻 (unix 秒)。
    pub at: i64,
    /// どのエージェント (セッション題名)。空なら手動。
    pub agent: String,
    /// 送った指示の要約。空のこともある。
    pub note: String,
    /// 「今」と何件違うか。数え終わるまで `None`。
    pub differs: Option<usize>,
}

/// 復元で何をするかの計画。`git diff --name-status -z` から組む。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestorePlan {
    /// 書き戻すファイル (スナップショットに在った物)。
    pub restore: Vec<String>,
    /// スナップショットに無かったので**触らない**ファイル。
    pub kept: Vec<String>,
}

impl RestorePlan {
    /// 一覧に出す「何件違うか」。触らない物も差分ではあるので数える。
    pub fn total(&self) -> usize {
        self.restore.len() + self.kept.len()
    }
}

/// 裏のスレッドが返す結果。UI はこれを見て表示を決める。
#[derive(Debug, Clone)]
pub enum Done {
    /// 記録した。`announce` が偽なら**通知を出さない**
    /// (指示のたびの自動取得で通知が溢れるのを防ぐ)。
    Captured {
        /// 記録した中身。
        cp: Checkpoint,
        /// 通知を出してよいか (手動取得なら真)。
        announce: bool,
    },
    /// 中身が前回と同一だったので記録しなかった。
    Skipped {
        /// 通知を出してよいか (手動取得なら真)。
        announce: bool,
    },
    /// 一覧が揃った。
    Listed(Vec<Checkpoint>),
    /// 復元した。
    Restored {
        /// 書き戻した件数。
        restored: usize,
        /// スナップショットに無かったので残した件数。
        kept: usize,
    },
    /// 差分が取れた。`(見出し, unified diff)`。
    Diff(String, String),
    /// 失敗。git の文言をそのまま持つ。
    Failed(String),
}

/// 裏のスレッドへ投げる仕事。
enum Task {
    Capture {
        agent: String,
        note: String,
        announce: bool,
    },
    List,
    Diff {
        sha: String,
    },
    Restore {
        sha: String,
    },
}

// ══════════════════════════════════════════════════════════════════
//  純粋関数 (テーブルテストで固定する)
// ══════════════════════════════════════════════════════════════════

/// 件名に入れる 1 行へ畳む。改行・タブ・制御文字・区切り文字を潰し、
/// 空白を 1 個へ寄せ、`max` 文字 (**文字数**であってバイト数ではない) で切る。
///
/// 日本語の指示がそのまま入るので、バイト境界で切ると壊れる。
pub fn one_line(s: &str, max: usize) -> String {
    let mut out = String::new();
    let mut space = true; // 先頭の空白は落とす
    for ch in s.chars() {
        let ch = if ch.is_control() || ch == '\t' || ch == '|' {
            ' '
        } else {
            ch
        };
        if ch == ' ' {
            if !space {
                out.push(' ');
                space = true;
            }
        } else {
            out.push(ch);
            space = false;
        }
    }
    let out = out.trim_end().to_string();
    if out.chars().count() <= max {
        return out;
    }
    if max == 0 {
        return String::new();
    }
    let mut cut: String = out.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

/// コミットメッセージ (= 件名) を組む。
pub fn format_message(agent: &str, note: &str) -> String {
    format!(
        "{SUBJECT_PREFIX}{}{FIELD_SEP}{}",
        one_line(agent, 60),
        one_line(note, 160)
    )
}

/// 件名から `(エージェント, 指示要約)` を戻す。
/// 印が無い / 壊れている物も落とさず、読める形で返す。
pub fn parse_subject(subject: &str) -> (String, String) {
    let body = subject.strip_prefix(SUBJECT_PREFIX).unwrap_or(subject);
    match body.split_once(FIELD_SEP) {
        Some((a, n)) => (a.trim().to_string(), n.trim().to_string()),
        None => (body.trim().to_string(), String::new()),
    }
}

/// `git for-each-ref` の 1 行 (`sha \t unix \t refname \t 件名`) を読む。
/// 壊れた行は黙って捨てる (git のバージョン差で列が減っても落ちない)。
pub fn parse_ref_line(line: &str) -> Option<Checkpoint> {
    let mut it = line.splitn(4, '\t');
    let sha = it.next()?.trim();
    let at: i64 = it.next()?.trim().parse().ok()?;
    let refname = it.next()?.trim();
    let subject = it.next().unwrap_or("");
    if sha.is_empty() || !refname.starts_with(REF_PREFIX) {
        return None;
    }
    let (agent, note) = parse_subject(subject);
    Some(Checkpoint {
        sha: sha.to_string(),
        refname: refname.to_string(),
        at,
        agent,
        note,
        differs: None,
    })
}

/// `for-each-ref` の出力全体を新しい順に並べて返す。
pub fn parse_ref_list(out: &str) -> Vec<Checkpoint> {
    let mut v: Vec<Checkpoint> = out
        .replace("\r\n", "\n")
        .lines()
        .filter_map(parse_ref_line)
        .collect();
    // 同じ秒に複数取れることがあるので、時刻が同じなら sha で決める
    // (並びが実行のたびに揺れると、一覧の選択が飛ぶ)。
    v.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.sha.cmp(&a.sha)));
    v
}

/// 上限を超えた分 (古い方) の refname を返す。新しい順の一覧を渡すこと。
pub fn prune_plan(list: &[Checkpoint], cap: usize) -> Vec<String> {
    if cap == 0 {
        return list.iter().map(|c| c.refname.clone()).collect();
    }
    list.iter().skip(cap).map(|c| c.refname.clone()).collect()
}

/// `git diff --name-status -z <A> <B>` の出力から復元計画を組む。
///
/// 向きは `A`(スナップショット) → `B`(今)。`A` = 追加 (今にしか無い) は
/// **消さない**ので `kept` へ、それ以外 (`M` / `D` / 型変更) は書き戻す。
///
/// `-z` の形は `状態 \0 パス \0 状態 \0 パス \0 …`。
pub fn plan_restore(name_status_z: &str) -> RestorePlan {
    let mut plan = RestorePlan::default();
    let mut it = name_status_z.split('\0');
    while let Some(status) = it.next() {
        if status.is_empty() {
            continue;
        }
        let Some(path) = it.next() else { break };
        if path.is_empty() {
            continue;
        }
        // 状態は先頭 1 文字で決まる (`R100` 等は --no-renames で出ないが、
        // 念のため先頭だけを見る)。
        match status.chars().next() {
            Some('A') => plan.kept.push(path.to_string()),
            _ => plan.restore.push(path.to_string()),
        }
    }
    plan.restore.sort();
    plan.kept.sort();
    plan
}

/// 一覧の 1 行に出す文言。`now` を渡すのは相対時刻をテストで固定するため。
pub fn list_label(c: &Checkpoint, now: i64) -> String {
    let when = crate::git::relative_time(c.at, now);
    let who = if c.agent.is_empty() {
        tr("手動")
    } else {
        c.agent.clone()
    };
    let diff = match c.differs {
        Some(0) => tr("差分なし"),
        Some(n) => trf("{n} 件の差分", &[("n", n.to_string())]),
        None => tr("数え中…"),
    };
    if c.note.is_empty() {
        trf(
            "{when} · {who} · {diff}",
            &[("when", when), ("who", who), ("diff", diff)],
        )
    } else {
        trf(
            "{when} · {who} · {diff} — {note}",
            &[
                ("when", when),
                ("who", who),
                ("diff", diff),
                ("note", c.note.clone()),
            ],
        )
    }
}

// ══════════════════════════════════════════════════════════════════
//  git 実行 (**すべて裏のスレッドから呼ぶこと**)
// ══════════════════════════════════════════════════════════════════

/// `git -C <repo> <args>` を走らせる。`index` を渡すとその一時 index を使う。
///
/// 呼ぶ側がスレッドを用意すること。UI スレッドから呼んではいけない。
fn git(repo: &Path, args: &[&str], index: Option<&Path>) -> Result<String, String> {
    let out = base_cmd(repo, args, index)
        .output()
        .map_err(|e| e.to_string())?;
    finish(out.status.success(), &out.stdout, &out.stderr, args)
}

/// stdin へ NUL 区切りのパスを流し込む版 (`checkout-index --stdin -z`)。
/// コマンドラインの長さ上限に当たらないので、件数が増えても壊れない。
fn git_stdin(
    repo: &Path,
    args: &[&str],
    index: Option<&Path>,
    data: &[u8],
) -> Result<String, String> {
    let mut child = base_cmd(repo, args, index)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())?;
    {
        let mut sin = child.stdin.take().ok_or_else(|| tr("stdin を開けません"))?;
        sin.write_all(data).map_err(|e| e.to_string())?;
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    finish(out.status.success(), &out.stdout, &out.stderr, args)
}

fn base_cmd(repo: &Path, args: &[&str], index: Option<&Path>) -> std::process::Command {
    let mut c = crate::procx::hidden_command("git");
    // 色とパスのエスケープを止める (`git.rs` の run_git_at と同じ理由)。
    c.args(["-c", "color.ui=false", "-c", "core.quotepath=false", "-C"])
        .arg(repo)
        .args(args)
        // commit-tree は身元が無いと失敗する。ユーザーの設定に依存させない。
        .env("GIT_AUTHOR_NAME", IDENT_NAME)
        .env("GIT_AUTHOR_EMAIL", IDENT_EMAIL)
        .env("GIT_COMMITTER_NAME", IDENT_NAME)
        .env("GIT_COMMITTER_EMAIL", IDENT_EMAIL);
    if let Some(p) = index {
        // ここが肝。**ユーザーの `.git/index` を一切触らない**。
        c.env("GIT_INDEX_FILE", p);
    }
    c
}

fn finish(ok: bool, stdout: &[u8], stderr: &[u8], args: &[&str]) -> Result<String, String> {
    if !ok {
        let err = crate::textenc::decode_output(stderr).trim().to_string();
        return Err(if err.is_empty() {
            trf("git {args} が失敗しました", &[("args", args.join(" "))])
        } else {
            err
        });
    }
    Ok(crate::textenc::decode_output(stdout))
}

/// 使い捨ての index ファイル。**必ず temp_dir 配下**に作り、Drop で消す。
struct TempIndex(PathBuf);

impl TempIndex {
    fn new() -> Self {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        // OS ごとの一時領域から導く (パスを直書きしない)。
        Self(std::env::temp_dir().join(format!(
            "zaivern-cp-index-{}-{}-{}",
            std::process::id(),
            nanos,
            n
        )))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        // 消せなくても致命ではない (temp_dir は OS が掃除する)。
        std::fs::remove_file(&self.0).ok();
    }
}

/// 今の作業ツリーを 1 本の tree オブジェクトに固める。**index には触らない**。
///
/// 戻り値は tree の ID。`HEAD` が無いリポジトリ (コミット 0 個) でも通る。
fn write_current_tree(repo: &Path) -> Result<String, String> {
    let idx = TempIndex::new();
    // HEAD があるなら、まずそこから index を起こす (差分の基準を揃える)。
    if head_sha(repo).is_some() {
        git(repo, &["read-tree", "HEAD"], Some(idx.path()))?;
    }
    // 未追跡も削除も拾う。`.gitignore` は尊重される (成果物を抱えない)。
    git(repo, &["add", "-A"], Some(idx.path()))?;
    Ok(git(repo, &["write-tree"], Some(idx.path()))?
        .trim()
        .to_string())
}

fn head_sha(repo: &Path) -> Option<String> {
    git(repo, &["rev-parse", "--verify", "HEAD"], None)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// コミットの tree ID。
fn tree_of(repo: &Path, sha: &str) -> Result<String, String> {
    let spec = format!("{sha}^{{tree}}");
    Ok(git(repo, &["rev-parse", "--verify", &spec], None)?
        .trim()
        .to_string())
}

/// スナップショットを 1 つ取る。中身が直前と同じなら `Ok(None)`。
fn capture(repo: &Path, agent: &str, note: &str) -> Result<Option<Checkpoint>, String> {
    let tree = write_current_tree(repo)?;

    let existing = list(repo)?;
    // 同じ内容なら重複して取らない。直近 1 件だけを見る (往復を増やさない)。
    if let Some(prev) = existing.first() {
        if tree_of(repo, &prev.sha).is_ok_and(|t| t == tree) {
            return Ok(None);
        }
    }

    let msg = format_message(agent, note);
    let mut args: Vec<String> = vec!["commit-tree".into(), tree.clone()];
    if let Some(head) = head_sha(repo) {
        args.push("-p".into());
        args.push(head);
    }
    args.push("-m".into());
    args.push(msg.clone());
    let argv: Vec<&str> = args.iter().map(String::as_str).collect();
    let sha = git(repo, &argv, None)?.trim().to_string();

    let at = crate::git::unix_now();
    // ref 名は数字と `-` だけ。git の ref 名規則に当たらない。
    let refname = format!("{REF_PREFIX}{at}-{}", &sha[..sha.len().min(12)]);
    git(repo, &["update-ref", &refname, &sha], None)?;

    // 上限を超えた古い物を捨てる。失敗しても記録自体は成功扱いにする。
    for dead in prune_plan(&list(repo).unwrap_or_default(), MAX_CHECKPOINTS) {
        git(repo, &["update-ref", "-d", &dead], None).ok();
    }

    let (agent, note) = parse_subject(&msg);
    Ok(Some(Checkpoint {
        sha,
        refname,
        at,
        agent,
        note,
        differs: None,
    }))
}

/// 記録済みの一覧 (新しい順)。差分件数はまだ入っていない。
fn list(repo: &Path) -> Result<Vec<Checkpoint>, String> {
    let out = git(
        repo,
        &[
            "for-each-ref",
            // `%09` = タブ。件名側からタブは除いてあるので 4 列で確実に割れる。
            "--format=%(objectname)%09%(committerdate:unix)%09%(refname)%09%(contents:subject)",
            REF_PREFIX,
        ],
        None,
    )?;
    Ok(parse_ref_list(&out))
}

/// 一覧に差分件数を入れる。tree 同士の比較なので index に依存しない。
fn list_with_counts(repo: &Path) -> Result<Vec<Checkpoint>, String> {
    let mut v = list(repo)?;
    let now = write_current_tree(repo).ok();
    if let Some(now) = now {
        for c in &mut v {
            c.differs = plan_between(repo, &c.sha, &now).ok().map(|p| p.total());
        }
    }
    Ok(v)
}

/// スナップショットと「今」の差分から復元計画を組む。
fn plan_between(repo: &Path, sha: &str, now_tree: &str) -> Result<RestorePlan, String> {
    let out = git(
        repo,
        &[
            "diff",
            "--name-status",
            "-z",
            // 改名を追わない。追うと `R` を書き戻す/残すの判断が二重になる。
            "--no-renames",
            sha,
            now_tree,
        ],
        None,
    )?;
    Ok(plan_restore(&out))
}

/// スナップショットと「今」の unified diff。差分タブに出す本文。
fn diff_text(repo: &Path, sha: &str) -> Result<String, String> {
    let now = write_current_tree(repo)?;
    git(
        repo,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-renames",
            sha,
            &now,
        ],
        None,
    )
}

/// 復元。**作業ツリーだけ**を書き戻し、`.git/index` は触らない。
///
/// 直前に「今」のチェックポイントを取る (戻した後に戻れる)。
fn restore(repo: &Path, sha: &str) -> Result<(usize, usize), String> {
    // 1) まず今を残す。ここが失敗したら復元へ進まない (退路の確保が先)。
    capture(repo, &tr("復元前"), &tr("巻き戻しの直前に自動取得"))?;

    let now = write_current_tree(repo)?;
    let plan = plan_between(repo, sha, &now)?;
    if plan.restore.len() > MAX_RESTORE_FILES {
        return Err(trf(
            "差分が {n} 件あります。巻き戻しの上限は {cap} 件です",
            &[
                ("n", plan.restore.len().to_string()),
                ("cap", MAX_RESTORE_FILES.to_string()),
            ],
        ));
    }
    if plan.restore.is_empty() {
        return Ok((0, plan.kept.len()));
    }

    // 2) 一時 index にスナップショットを読み込み、そこから作業ツリーへ書く。
    //    `checkout-index` は index を見るだけなので、ユーザーの index は無傷。
    let idx = TempIndex::new();
    git(repo, &["read-tree", sha], Some(idx.path()))?;

    let mut data = Vec::new();
    for p in &plan.restore {
        data.extend_from_slice(p.as_bytes());
        data.push(0);
    }
    git_stdin(
        repo,
        &["checkout-index", "-f", "-z", "--stdin"],
        Some(idx.path()),
        &data,
    )?;

    Ok((plan.restore.len(), plan.kept.len()))
}

// ══════════════════════════════════════════════════════════════════
//  UI から使う状態
// ══════════════════════════════════════════════════════════════════

/// チェックポイントの保持と、裏のスレッドとの受け渡し。
pub struct Checkpoints {
    repo: PathBuf,
    list: Vec<Checkpoint>,
    /// 走行中の仕事 (同時に 1 つだけ)。
    job: Option<Receiver<Done>>,
    job_label: String,
    /// 一覧ダイアログを開いているか。
    pub open: bool,
    /// 一覧で選んでいる行。
    selected: usize,
    /// 復元の確認待ち (破壊的操作なので必ず挟む)。
    confirm: Option<Checkpoint>,
    /// 直近の結果表示 (ダイアログ内に残す)。
    status: String,
    /// 一覧を取り直したい。
    stale: bool,
}

impl Checkpoints {
    pub fn new(repo: PathBuf) -> Self {
        Self {
            repo,
            list: Vec::new(),
            job: None,
            job_label: String::new(),
            open: false,
            selected: 0,
            confirm: None,
            status: String::new(),
            stale: true,
        }
    }

    /// ワークスペースが変わったら一覧を捨てる (別リポジトリの sha へ
    /// 復元を撃てないように、飛行中の受信口ごと捨てる)。
    pub fn set_repo(&mut self, repo: PathBuf) {
        if self.repo == repo {
            return;
        }
        self.repo = repo;
        self.list.clear();
        self.job = None;
        self.confirm = None;
        self.selected = 0;
        self.status.clear();
        self.stale = true;
    }

    /// 走行中か。多重起動を避ける判定と、UI の「実行中」表示に使う。
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }

    /// 保持件数 (Cockpit のバッジ)。
    pub fn count(&self) -> usize {
        self.list.len()
    }

    /// エージェントへ指示を送る**直前**に 1 件取る。
    ///
    /// 走行中なら黙って諦める — 一斉送信で N 体ぶん呼ばれるが、
    /// スナップショットは作業ツリー全体の写しなので 1 回取れば足りる。
    /// 中身が前回と同じかどうかの判定は裏のスレッド側 (tree の比較) が行う。
    pub fn capture_before_submit(&mut self, agent: &str, note: &str, ctx: &egui::Context) {
        self.start_snapshot(agent, note, false, ctx);
    }

    /// 手動で 1 件取る (パレット「チェックポイント: 今すぐ取る」/ 一覧のボタン)。
    /// こちらは結果を通知で知らせる (押した手応えが要る)。
    pub fn capture_now(&mut self, ctx: &egui::Context) {
        self.start_snapshot(&tr("手動"), &tr("手動で取得"), true, ctx);
    }

    fn start_snapshot(&mut self, agent: &str, note: &str, announce: bool, ctx: &egui::Context) {
        self.spawn(
            Task::Capture {
                agent: agent.to_string(),
                note: note.to_string(),
                announce,
            },
            tr("チェックポイントを記録中…"),
            ctx,
        );
    }

    /// 一覧を開く (パレット / メニューから)。
    pub fn open_list(&mut self, ctx: &egui::Context) {
        self.open = true;
        self.stale = true;
        self.refresh(ctx);
    }

    /// 明示要求があるときだけ取り直す。**開いている間だけ呼ぶこと**
    /// (アイドル時に git を走らせない = 常時再描画もしない)。
    fn refresh(&mut self, ctx: &egui::Context) {
        if !self.stale || self.busy() {
            return;
        }
        self.stale = false;
        self.spawn(Task::List, tr("一覧を取得中…"), ctx);
    }

    fn spawn(&mut self, task: Task, label: String, ctx: &egui::Context) {
        if self.busy() {
            return;
        }
        let (tx, rx) = mpsc::channel();
        let repo = self.repo.clone();
        let ctx = ctx.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-checkpoint".into())
            .spawn(move || {
                let done = run_task(&repo, task);
                let _ = tx.send(done);
                ctx.request_repaint();
            });
        if spawned.is_ok() {
            self.job = Some(rx);
            self.job_label = label;
        } else {
            self.status = tr("スレッドを起動できませんでした");
        }
    }

    /// 完了した仕事を回収する。**待たない** (`try_recv` のみ)。
    ///
    /// 表示だけで済む物はここで内部状態へ畳み、`Done` をそのまま返す。
    /// app 側は `Done::Diff` (差分タブを開く) と通知の出し分けに使う。
    pub fn poll(&mut self) -> Option<Done> {
        let rx = self.job.as_ref()?;
        let done = match rx.try_recv() {
            Ok(d) => d,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.job = None;
                return None;
            }
            Err(mpsc::TryRecvError::Empty) => return None,
        };
        self.job = None;
        self.job_label.clear();
        match &done {
            Done::Captured { cp, .. } => {
                self.status = trf(
                    "チェックポイントを取りました ({sha})",
                    &[("sha", short(&cp.sha))],
                );
                self.stale = true;
            }
            Done::Skipped { .. } => self.status = tr("前回から変更がないので取りませんでした"),
            Done::Listed(v) => {
                self.list = v.clone();
                self.selected = self.selected.min(self.list.len().saturating_sub(1));
            }
            Done::Restored { restored, kept } => {
                self.status = trf(
                    "{n} 件を書き戻しました (スナップショットに無かった {k} 件はそのまま)",
                    &[("n", restored.to_string()), ("k", kept.to_string())],
                );
                self.stale = true;
            }
            Done::Diff(..) => self.status.clear(),
            Done::Failed(e) => self.status = e.clone(),
        }
        Some(done)
    }

    /// 一覧ダイアログを描く。
    ///
    /// **描画中に git の完了は待たない**。走らせるのは裏のスレッドへ投げるまで。
    pub fn ui(&mut self, ctx: &egui::Context) {
        if !self.open {
            return;
        }
        self.refresh(ctx);
        let mut open = self.open;
        let now = crate::git::unix_now();
        // 画面が狭くても枠からはみ出さない。
        let screen = ctx.screen_rect();
        egui::Window::new(tr("チェックポイント"))
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width((screen.width() * 0.7).clamp(360.0, 760.0))
            .max_width((screen.width() - 32.0).max(280.0))
            .max_height((screen.height() - 32.0).max(200.0))
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| self.body(ui, now));
        self.open = open;
        if !self.open {
            self.confirm = None;
        }
    }

    fn body(&mut self, ui: &mut egui::Ui, now: i64) {
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.busy(), egui::Button::new(tr("今すぐ取る")))
                .on_hover_text(tr("今の作業ツリーを記録します"))
                .clicked()
            {
                self.capture_now(ui.ctx());
            }
            if ui
                .add_enabled(!self.busy(), egui::Button::new(tr("再読み込み")))
                .clicked()
            {
                self.stale = true;
            }
            if self.busy() {
                ui.spinner();
                ui.label(self.job_label.clone());
            }
        });

        // 制限は必ず出す。「後から作った物も消える」と思われる方が危ない。
        ui.label(
            egui::RichText::new(tr(
                "復元はスナップショットに在ったファイルを書き戻すだけです。後から作られたファイルは消しません。.gitignore された物は記録されません。",
            ))
            .small()
            .weak(),
        );
        if !self.status.is_empty() {
            ui.label(egui::RichText::new(self.status.clone()).small());
        }
        ui.separator();

        // 確認は一覧の上に重ねず、同じ枠の中で**置き換える**
        // (2 つの画面が同時に描かれる事故を構造的に起こさないため)。
        if let Some(target) = self.confirm.clone() {
            self.confirm_body(ui, &target, now);
            return;
        }

        if self.list.is_empty() {
            // 空状態は 1 枚のカードで中央に。高さは確保しない。
            ui.vertical_centered(|ui| {
                ui.add_space(20.0);
                ui.label(if self.busy() {
                    tr("読み込み中…")
                } else {
                    tr("まだチェックポイントはありません")
                });
                ui.add_space(20.0);
            });
            return;
        }

        let avail = ui.available_width();
        let row_h = ui.spacing().interact_size.y;
        // 省略幅は決め打ちにせず、今のフォントから測る (UI 拡大率で変わる)。
        let char_w = ui.fonts(|f| f.glyph_width(&egui::TextStyle::Body.resolve(ui.style()), 'M'));
        let selected = self.selected;
        let mut clicked: Option<usize> = None;
        {
            let list = &self.list;
            egui::ScrollArea::vertical()
                .id_salt("zv-checkpoint-list")
                .max_height(320.0)
                .show(ui, |ui| {
                    for (i, c) in list.iter().enumerate() {
                        let full = list_label(c, now);
                        let shown = elide(&full, avail, char_w);
                        let resp = ui.add_sized(
                            egui::vec2(avail.max(80.0), row_h),
                            egui::SelectableLabel::new(selected == i, shown),
                        );
                        if resp.on_hover_text(full).clicked() {
                            clicked = Some(i);
                        }
                    }
                });
        }
        if let Some(i) = clicked {
            self.selected = i;
        }

        ui.separator();
        let sel = self.list.get(self.selected).cloned();
        ui.horizontal_wrapped(|ui| {
            let on = sel.is_some() && !self.busy();
            if ui
                .add_enabled(on, egui::Button::new(tr("差分を見る")))
                .clicked()
            {
                if let Some(c) = &sel {
                    self.spawn(
                        Task::Diff { sha: c.sha.clone() },
                        tr("差分を取得中…"),
                        ui.ctx(),
                    );
                }
            }
            if ui
                .add_enabled(on, egui::Button::new(tr("この時点へ戻す…")))
                .clicked()
            {
                self.confirm = sel.clone();
            }
        });
    }

    /// 破壊的操作なので必ず確認を挟む。
    fn confirm_body(&mut self, ui: &mut egui::Ui, target: &Checkpoint, now: i64) {
        ui.label(egui::RichText::new(tr("この時点へ戻しますか?")).strong());
        ui.label(list_label(target, now));
        ui.label(
            egui::RichText::new(tr(
                "作業ツリーのファイルが書き換わります。ステージ済みの内容 (index) は変わりません。戻す直前に「今」のチェックポイントを自動で取るので、戻した後に戻れます。",
            ))
            .small(),
        );
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            if ui
                .add_enabled(!self.busy(), egui::Button::new(tr("戻す")))
                .clicked()
            {
                let sha = target.sha.clone();
                self.spawn(Task::Restore { sha }, tr("巻き戻し中…"), ui.ctx());
                self.confirm = None;
            }
            if ui.button(tr("やめる")).clicked() {
                self.confirm = None;
            }
            if self.busy() {
                ui.spinner();
            }
        });
    }
}

/// sha を人が読める長さへ。
fn short(sha: &str) -> String {
    sha.chars().take(8).collect()
}

/// 可用幅に入る桁数 (純関数)。`char_w` は実測のグリフ幅。
///
/// **収まらない事故を防ぐ側へ倒す** — CJK は 1 文字が 2 桁ぶんの幅を食うので、
/// 桁数をそのまま文字数と見なすと溢れる。半分に見積もっておけば、
/// 全角でも半角でも右端で切れない (足りない分はホバーで全文が読める)。
pub fn label_cols(avail_w: f32, char_w: f32) -> usize {
    if !(char_w > 0.0) || !avail_w.is_finite() || avail_w <= 0.0 {
        return MIN_LABEL_COLS;
    }
    ((avail_w / char_w / 2.0) as usize).max(MIN_LABEL_COLS)
}

/// 省略しても意味が残る最低桁数。これを下回るほど狭いときは、
/// 短く切るより溢れる方がまし (何のチェックポイントか分からなくなる)。
const MIN_LABEL_COLS: usize = 12;

/// 幅に収まらない文言を省略する。**全文はホバーで見せる**こと。
fn elide(s: &str, avail_w: f32, char_w: f32) -> String {
    one_line(s, label_cols(avail_w, char_w))
}

/// 裏のスレッドの本体。
fn run_task(repo: &Path, task: Task) -> Done {
    match task {
        Task::Capture {
            agent,
            note,
            announce,
        } => match capture(repo, &agent, &note) {
            Ok(Some(cp)) => Done::Captured { cp, announce },
            Ok(None) => Done::Skipped { announce },
            Err(e) => Done::Failed(e),
        },
        Task::List => match list_with_counts(repo) {
            Ok(v) => Done::Listed(v),
            Err(e) => Done::Failed(e),
        },
        Task::Diff { sha } => match diff_text(repo, &sha) {
            Ok(t) => Done::Diff(short(&sha), t),
            Err(e) => Done::Failed(e),
        },
        Task::Restore { sha } => match restore(repo, &sha) {
            Ok((restored, kept)) => Done::Restored { restored, kept },
            Err(e) => Done::Failed(e),
        },
    }
}

#[cfg(test)]
#[allow(non_snake_case)]
mod tests {
    use super::*;

    fn cp(at: i64, sha: &str, agent: &str, note: &str, differs: Option<usize>) -> Checkpoint {
        Checkpoint {
            sha: sha.to_string(),
            refname: format!("{REF_PREFIX}{at}-{sha}"),
            at,
            agent: agent.to_string(),
            note: note.to_string(),
            differs,
        }
    }

    // ── 純粋関数 ────────────────────────────────────────────────

    #[test]
    fn one_lineは改行と区切りを潰して文字数で切る() {
        // (入力, 上限, 期待)
        let table: &[(&str, usize, &str)] = &[
            ("  hello   world  ", 40, "hello world"),
            ("a\nb\tc", 40, "a b c"),
            // 区切り文字は必ず潰す (件名の分割が一意に決まらなくなるため)
            ("agent | name", 40, "agent name"),
            // 制御文字は 1 個ずつ空白へ潰す (ANSI の残骸は文字として残るが、
            // 件名の分割と行の高さは壊れない)。
            ("x\u{1b}[0my", 40, "x [0my"),
            ("", 10, ""),
            ("abcdef", 6, "abcdef"),
            ("abcdef", 5, "abcd…"),
            ("abcdef", 0, ""),
            // **文字数**で切る。バイト境界で切ると日本語が壊れる
            ("日本語のとても長い指示文", 5, "日本語の…"),
            ("日本語", 3, "日本語"),
        ];
        for (input, max, want) in table {
            assert_eq!(&one_line(input, *max), want, "one_line({input:?}, {max})");
        }
    }

    #[test]
    fn 件名は組んで元へ戻せる() {
        let table: &[(&str, &str)] = &[
            ("claude-1", "テストを直して"),
            ("", ""),
            ("エージェント 甲", "日本語の指示\nの 2 行目"),
            ("a|b", "c|d"),
        ];
        for (agent, note) in table {
            let msg = format_message(agent, note);
            assert!(msg.starts_with(SUBJECT_PREFIX), "印が付く: {msg}");
            let (a, n) = parse_subject(&msg);
            assert_eq!(a, one_line(agent, 60), "エージェントが戻る");
            assert_eq!(n, one_line(note, 160), "指示要約が戻る");
        }
    }

    #[test]
    fn 印の無い件名も落とさず読む() {
        assert_eq!(
            parse_subject("なにかのコミット"),
            ("なにかのコミット".to_string(), String::new())
        );
    }

    #[test]
    fn for_each_refの行を読む() {
        let good = format!("abc123\t1700000000\t{REF_PREFIX}1700000000-abc123\tzaivern-checkpoint: claude | 直して");
        let c = parse_ref_line(&good).expect("読める");
        assert_eq!(c.sha, "abc123");
        assert_eq!(c.at, 1_700_000_000);
        assert_eq!(c.agent, "claude");
        assert_eq!(c.note, "直して");
        assert_eq!(c.differs, None, "件数は後から入れる");

        // 壊れた行は黙って捨てる (git のバージョン差で列が減っても落ちない)
        for bad in [
            "",
            "abc123",
            "abc123\tnotanumber\trefs/zaivern/checkpoints/x\ts",
            // 名前空間の外の ref は拾わない
            "abc123\t1\trefs/heads/main\ts",
            "\t1\trefs/zaivern/checkpoints/x\ts",
        ] {
            assert!(parse_ref_line(bad).is_none(), "捨てる: {bad:?}");
        }
    }

    #[test]
    fn 一覧は新しい順で同秒でも並びが揺れない() {
        let raw = format!(
            "aaa\t100\t{p}100-aaa\tzaivern-checkpoint: a | x\n\
             ccc\t300\t{p}300-ccc\tzaivern-checkpoint: c | z\n\
             bbb\t100\t{p}100-bbb\tzaivern-checkpoint: b | y\n",
            p = REF_PREFIX
        );
        let v = parse_ref_list(&raw);
        assert_eq!(
            v.iter().map(|c| c.sha.as_str()).collect::<Vec<_>>(),
            vec!["ccc", "bbb", "aaa"],
        );
        // CRLF のチェックアウトでも同じ結果
        assert_eq!(parse_ref_list(&raw.replace('\n', "\r\n")), v);
    }

    #[test]
    fn 上限を超えた古い物だけを間引く() {
        let list: Vec<Checkpoint> = (0..5)
            .map(|i| cp(100 - i, &format!("s{i}"), "a", "n", None))
            .collect();
        // (上限, 消す件数)
        let table: &[(usize, usize)] = &[(10, 0), (5, 0), (3, 2), (1, 4), (0, 5)];
        for (cap, want) in table {
            let dead = prune_plan(&list, *cap);
            assert_eq!(dead.len(), *want, "cap={cap}");
            // 消すのは必ず**古い方**から
            for d in &dead {
                assert!(
                    list.iter().rev().take(*want).any(|c| &c.refname == d),
                    "古い側だけを消す: {d}"
                );
            }
        }
        assert!(prune_plan(&[], 3).is_empty(), "空でも落ちない");
    }

    #[test]
    fn 復元計画は追加されたファイルを消さない() {
        // `git diff --name-status -z A B` の生の形
        let z = "M\0src/a.rs\0A\0src/new.rs\0D\0src/gone.rs\0T\0link\0";
        let plan = plan_restore(z);
        assert_eq!(
            plan.restore,
            vec!["link".to_string(), "src/a.rs".into(), "src/gone.rs".into()],
            "変更・削除・型変更は書き戻す",
        );
        assert_eq!(
            plan.kept,
            vec!["src/new.rs".to_string()],
            "スナップショットに無かった物は触らない",
        );
        assert_eq!(plan.total(), 4);

        // 空・末尾欠け・空パスでも落ちない
        for broken in ["", "\0", "M\0", "M\0\0A\0x\0"] {
            let p = plan_restore(broken);
            assert!(p.total() <= 1, "壊れた入力: {broken:?}");
        }
    }

    #[test]
    fn 省略桁数は可用幅とグリフ幅から決まる() {
        // (可用幅, グリフ幅, 期待桁数)
        let table: &[(f32, f32, usize)] = &[
            (720.0, 8.0, 45),
            (360.0, 8.0, 22),
            // 狭くても最低桁は割らない (何の記録か分からなくなるため)
            (100.0, 8.0, 12),
            (0.0, 8.0, 12),
            // UI 拡大率で字が大きくなれば桁は減る
            (720.0, 16.0, 22),
            // 壊れた値でも落ちない
            (720.0, 0.0, 12),
            (f32::NAN, 8.0, 12),
            (-5.0, 8.0, 12),
        ];
        for (w, cw, want) in table {
            assert_eq!(label_cols(*w, *cw), *want, "label_cols({w}, {cw})");
        }
        // 省略した結果は必ず桁数以内 (= 右端で切れない)
        let long = "あ".repeat(200);
        for w in [120.0_f32, 360.0, 900.0, 1600.0] {
            let shown = elide(&long, w, 8.0);
            assert!(
                shown.chars().count() <= label_cols(w, 8.0),
                "幅 {w} で溢れない"
            );
        }
    }

    #[test]
    fn 一覧の1行は誰がいつ何件を出す() {
        let now = 1_700_003_600;
        // 数え終わっていない間も高さは変えない (「数え中…」を出す)
        let pending = cp(now - 60, "s", "claude-1", "テストを直して", None);
        let l = list_label(&pending, now);
        assert!(l.contains("claude-1"), "エージェントが出る: {l}");
        assert!(l.contains("テストを直して"), "指示が出る: {l}");

        let zero = cp(now - 60, "s", "claude-1", "", Some(0));
        let l0 = list_label(&zero, now);
        assert!(!l0.contains('—'), "指示が空なら区切りも出さない: {l0}");

        let many = cp(now - 60, "s", "", "x", Some(7));
        let lm = list_label(&many, now);
        assert!(lm.contains('7'), "件数が出る: {lm}");
        assert!(lm.contains(&tr("手動")), "エージェント無しは手動: {lm}");
    }

    // ── 番人 ────────────────────────────────────────────────────

    /// git を UI スレッドで待つ経路を作らない (`git.rs` の同名テストと同じ趣旨)。
    #[test]
    fn UIスレッドから同期でgitを撃つ経路が残っていない() {
        let src = include_str!("checkpoint.rs").replace("\r\n", "\n");
        // `Checkpoints` の描画/受信側 (`impl Checkpoints`) には git の実行が無く、
        // 走らせるのは `spawn` (= 裏のスレッド) 経由だけであること。
        let imp = src
            .split("impl Checkpoints {")
            .nth(1)
            .expect("impl Checkpoints");
        let imp = imp.split("\n}\n").next().expect("impl の終端");

        // git を実際に走らせるのは `spawn` の中 (= 裏のスレッド) だけ。
        // ここを切り出してから、残り全部に git 実行が無いことを見る。
        let (before, rest) = imp.split_once("    fn spawn(").expect("fn spawn");
        let (spawn_body, after) = rest.split_once("\n    }\n").expect("spawn の終端");
        assert!(
            spawn_body.contains("std::thread::Builder::new()") && spawn_body.contains("run_task("),
            "spawn が git を裏のスレッドで走らせていない"
        );
        let ui_side = format!("{before}{after}");
        // `git` / `git_stdin` は git を走らせる唯一の入口なので、これが
        // UI 側に無いことがそのまま「同期で撃っていない」の証明になる。
        for needle in [
            "git(",
            "git_stdin(",
            "run_task(",
            "capture(",
            "restore(",
            "list_with_counts(",
        ] {
            assert!(
                !ui_side.contains(needle),
                "impl Checkpoints が {needle} を直接呼んでいる (UI スレッドで git を待つ)"
            );
        }
        // 受信は必ず try_recv (待たない)。
        assert!(imp.contains("try_recv"), "結果の回収が try_recv でない");
        assert!(
            !imp.contains(".recv()"),
            "UI スレッドがチャネルで待っている"
        );
        // stash はワークツリー横断で他インスタンスを巻き込むので使わない。
        assert!(!src.contains("\"stash\""), "git stash を使っている");
    }

    // ── 実リポジトリ ────────────────────────────────────────────

    /// `git init` 済みの使い捨てリポジトリ。git が無い環境では `None`。
    fn temp_repo(tag: &str) -> Option<PathBuf> {
        let base = crate::test_util::unique_temp_dir("zaivern-cp-test", tag);
        let dir = base.join("repo");
        std::fs::create_dir_all(&dir).expect("create repo dir");
        let run = |args: &[&str]| -> bool {
            crate::procx::hidden_command("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !run(&["init", "--quiet"]) {
            std::fs::remove_dir_all(&base).ok();
            return None; // git が無い環境ではスキップ
        }
        // 実行環境の設定に依存させない (CI と手元で同じ結果にする)。
        run(&["config", "user.name", "cp test"]);
        run(&["config", "user.email", "cp@test.invalid"]);
        run(&["config", "core.autocrlf", "false"]);
        run(&["config", "commit.gpgsign", "false"]);
        Some(dir)
    }

    fn write(repo: &Path, rel: &str, body: &str) {
        let p = repo.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).expect("mkdir");
        }
        std::fs::write(p, body).expect("write");
    }

    fn read(repo: &Path, rel: &str) -> Option<String> {
        std::fs::read_to_string(repo.join(rel)).ok()
    }

    fn git_ok(repo: &Path, args: &[&str]) {
        let owned: Vec<String> = args.iter().map(|s| (*s).to_string()).collect();
        let argv: Vec<&str> = owned.iter().map(String::as_str).collect();
        git(repo, &argv, None).unwrap_or_else(|e| panic!("git {args:?}: {e}"));
    }

    #[test]
    fn 実リポジトリでスナップショットを取り巻き戻せる() {
        let Some(repo) = temp_repo("roundtrip") else {
            return; // git が無い環境ではスキップ
        };

        write(&repo, "a.txt", "one\n");
        git_ok(&repo, &["add", "a.txt"]);
        git_ok(&repo, &["commit", "-m", "init", "--quiet"]);

        // 記録したい姿: a.txt を書き換え、未追跡の c.txt を置く
        write(&repo, "a.txt", "two\n");
        write(&repo, "sub/c.txt", "see\n");

        let snap = capture(&repo, "claude-1", "テストを直して")
            .expect("capture")
            .expect("1 件目は必ず取れる");
        assert!(!snap.sha.is_empty());
        assert_eq!(snap.agent, "claude-1");

        // 同じ内容なら重複して取らない
        assert!(
            capture(&repo, "claude-1", "同じ内容")
                .expect("capture 2")
                .is_none(),
            "中身が同じなら記録しない",
        );

        // エージェントが暴走した後の姿
        write(&repo, "a.txt", "three\n");
        std::fs::remove_file(repo.join("sub/c.txt")).expect("rm c");
        write(&repo, "d.txt", "new\n");

        let before = list(&repo).expect("list");
        assert_eq!(before.len(), 1, "記録は 1 件");
        assert_eq!(before[0].sha, snap.sha, "一覧に出る sha は記録した物と同じ");

        let (restored, kept) = restore(&repo, &snap.sha).expect("restore");
        assert_eq!(restored, 2, "a.txt と sub/c.txt を書き戻す");
        assert_eq!(kept, 1, "d.txt は触らない");

        assert_eq!(read(&repo, "a.txt").as_deref(), Some("two\n"), "書き戻る");
        assert_eq!(
            read(&repo, "sub/c.txt").as_deref(),
            Some("see\n"),
            "消えたファイルも戻る",
        );
        assert_eq!(
            read(&repo, "d.txt").as_deref(),
            Some("new\n"),
            "**スナップショットに無かったファイルは消さない**",
        );

        // 戻す直前に「今」が記録されているので、戻した後に戻れる
        let after = list(&repo).expect("list 2");
        assert_eq!(after.len(), 2, "復元前チェックポイントが増えている");
        let undo = after
            .iter()
            .find(|c| c.sha != snap.sha)
            .expect("復元前の 1 件");
        let (n, k) = restore(&repo, &undo.sha).expect("undo");
        assert_eq!(n, 1, "a.txt を戻し直す");
        assert_eq!(read(&repo, "a.txt").as_deref(), Some("three\n"), "戻り直す");
        // 「無かった物は消さない」は往路でも復路でも同じ。復元前チェックポイント
        // には sub/c.txt が無いが、**消さずに残す** (取り返しがつかないため)。
        assert_eq!(k, 1, "復元前に無かった sub/c.txt は残す");
        assert_eq!(
            read(&repo, "sub/c.txt").as_deref(),
            Some("see\n"),
            "戻り直しても消えない",
        );

        std::fs::remove_dir_all(repo.parent().unwrap_or(&repo)).ok();
    }

    #[test]
    fn スナップショットはユーザーのindexを壊さない() {
        let Some(repo) = temp_repo("index") else {
            return; // git が無い環境ではスキップ
        };
        write(&repo, "a.txt", "one\n");
        git_ok(&repo, &["add", "a.txt"]);
        git_ok(&repo, &["commit", "-m", "init", "--quiet"]);

        // ユーザーが「一部だけ」ステージした状態を作る
        write(&repo, "staged.txt", "staged\n");
        write(&repo, "unstaged.txt", "unstaged\n");
        git_ok(&repo, &["add", "staged.txt"]);

        let index_path = repo.join(".git").join("index");
        let before_bytes = std::fs::read(&index_path).expect("read index");
        let before_ls = git(&repo, &["ls-files", "-s"], None).expect("ls-files");

        capture(&repo, "claude-1", "何か")
            .expect("capture")
            .expect("1 件");
        write(&repo, "a.txt", "two\n");
        let snap = list(&repo).expect("list")[0].sha.clone();
        restore(&repo, &snap).expect("restore");

        assert_eq!(
            std::fs::read(&index_path).expect("read index 2"),
            before_bytes,
            "`.git/index` のバイト列が 1 ビットも変わらない",
        );
        assert_eq!(
            git(&repo, &["ls-files", "-s"], None).expect("ls-files 2"),
            before_ls,
            "ステージ済みの内容が変わらない",
        );
        // 一時 index は temp_dir 配下に作られ、後片付けされている
        assert!(
            !repo.join(".git").join("zaivern-cp-index").exists(),
            "リポジトリ内に一時 index を作らない",
        );

        std::fs::remove_dir_all(repo.parent().unwrap_or(&repo)).ok();
    }

    /// エージェントがディレクトリごと消しても戻せる
    /// (`checkout-index` が親ディレクトリを作り直せるか、の実測)。
    #[test]
    fn ディレクトリごと消えても復元できる() {
        let Some(repo) = temp_repo("rmdir") else {
            return; // git が無い環境ではスキップ
        };
        write(&repo, "keep.txt", "k\n");
        write(&repo, "deep/nest/x.txt", "x\n");
        write(&repo, "deep/y.txt", "y\n");
        let snap = capture(&repo, "claude-1", "消す前")
            .expect("capture")
            .expect("取れる");

        std::fs::remove_dir_all(repo.join("deep")).expect("rmdir deep");
        assert!(!repo.join("deep").exists());

        let (n, _) = restore(&repo, &snap.sha).expect("restore");
        assert_eq!(n, 2, "消えた 2 ファイルを書き戻す");
        assert_eq!(read(&repo, "deep/nest/x.txt").as_deref(), Some("x\n"));
        assert_eq!(read(&repo, "deep/y.txt").as_deref(), Some("y\n"));

        std::fs::remove_dir_all(repo.parent().unwrap_or(&repo)).ok();
    }

    #[test]
    fn コミットが1つも無いリポジトリでも取れる() {
        let Some(repo) = temp_repo("nohead") else {
            return; // git が無い環境ではスキップ
        };
        write(&repo, "a.txt", "one\n");
        let snap = capture(&repo, "", "初回")
            .expect("capture")
            .expect("HEAD が無くても取れる");
        write(&repo, "a.txt", "two\n");
        let (n, _) = restore(&repo, &snap.sha).expect("restore");
        assert_eq!(n, 1);
        assert_eq!(read(&repo, "a.txt").as_deref(), Some("one\n"));
        std::fs::remove_dir_all(repo.parent().unwrap_or(&repo)).ok();
    }

    #[test]
    fn 上限を超えると古いrefが実際に消える() {
        let Some(repo) = temp_repo("prune") else {
            return; // git が無い環境ではスキップ
        };
        write(&repo, "a.txt", "0\n");
        git_ok(&repo, &["add", "a.txt"]);
        git_ok(&repo, &["commit", "-m", "init", "--quiet"]);

        // 上限そのものは 50 件。ここは間引きの経路が動くことだけを見る
        // (50 回のスナップショットは遅いので、prune_plan の単体側で網羅する)。
        for i in 0..3 {
            write(&repo, "a.txt", &format!("{i}\n"));
            capture(&repo, "a", "n").expect("capture").expect("取れる");
        }
        let all = list(&repo).expect("list");
        assert_eq!(all.len(), 3);
        for dead in prune_plan(&all, 1) {
            git_ok(&repo, &["update-ref", "-d", &dead]);
        }
        assert_eq!(list(&repo).expect("list 2").len(), 1, "ref が実際に消える");

        std::fs::remove_dir_all(repo.parent().unwrap_or(&repo)).ok();
    }
}
