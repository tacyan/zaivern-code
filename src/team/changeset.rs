//! **変更ファイルを実測する** — エージェントの自己申告を証跡にしない。
//!
//! ## なぜ自己申告では足りないのか
//!
//! 完了報告の `changed_files` は、エージェントが**書く内容を自分で決められる**。
//! 書き忘れても、意図的に省いても、こちらから見れば同じ「空の配列」になる。
//! それを担当範囲の照合に使うと、担当外を書き換えたタスクが素通りする —
//! しかも台帳には「担当内しか触っていない」と残るので、後から気付けない。
//!
//! なので照合の根拠は**こちらが測ったもの**にする。自己申告は
//! 「本人が何をしたつもりか」を人が読むための補助情報として残す
//! (食い違ったら、その事実そのものが証跡になる)。
//!
//! ## どう測るか
//!
//! ```text
//! 配る直前      → 基準点 (capture_baseline): 汚れているファイルの指紋
//! エージェント作業
//! 完了報告      → 測る (measure): 指紋が変わった / 増えた / 消えたもの
//! ```
//!
//! 指紋は**内容のハッシュ**。`git status` の状態文字 (`M` / `??`) だけでは
//! 足りない — 基準点でもう `M` だったファイルをもう一度書き換えても
//! `M` のままなので、**変更が 1 バイトも見えない**。
//!
//! ## 並列作業をどう切り分けるか
//!
//! このリポジトリは**複数のエージェントが同じワークスペースで同時に働く**
//! 前提なので、「いまの作業ツリーと HEAD の差分」をそのまま持ち主の成果に
//! すると、隣のタスクの変更を自分のものとして数える。
//!
//! 切り分けは既存の**ファイル所有リース** (`lease`) の担当範囲で行う。
//! [`attribute`] を参照 — 第 2 の競合判定は作らない。
//!
//! ## 測れないときは通さない
//!
//! Git 管理下でなければ、安全に「何が変わったか」を言う手段が無い
//! (作業ツリー全体を歩けば `node_modules` も `target` も舐めることになり、
//! しかも .gitignore を自前で解釈することになる)。**保証を偽らない** —
//! 測れないことをそのまま返し、呼び出し側が人へ渡す。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 基準点に載せるファイル数の上限。
///
/// 超えるほど汚れているツリーは、そもそも「このタスクが何を変えたか」を
/// 言える状態ではない。**黙って一部だけ持たない** — 測れないと言う。
pub const MAX_TRACKED_PATHS: usize = 2_000;

/// 指紋を取るために読むファイルの合計バイト数の上限。
pub const MAX_HASH_BYTES: u64 = 64 * 1024 * 1024;

/// 指紋を作るときに一度に読む大きさ。**ファイルの大きさとは無関係**に
/// この量ずつ流すので、大きなファイルでもメモリは一定で済む。
const HASH_CHUNK_BYTES: usize = 64 * 1024;

/// `git status` を待つ上限。
const GIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// ファイル 1 つの指紋。
///
/// **内容から作る。** 更新時刻や大きさだけでは、同じ長さで書き換えられた
/// ときに変化が見えない。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fingerprint {
    pub hash: u64,
    pub len: u64,
}

/// タスクを配る直前に取った基準点。
///
/// **タスクへ持たせて保存する。** 再起動をまたいでも同じ基準で測れないと、
/// 「再開したら実測できません」で全部人へ渡ることになる。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileBaseline {
    /// 取った時刻 (Unix 秒)。
    #[serde(default)]
    pub taken_at: u64,
    /// 基準点を取った時点のHEAD。空は旧保存形式で、測定不能として扱う。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub head_commit: String,
    /// 汚れていたファイルの指紋 (正規化済みの相対パス → 指紋)。
    #[serde(default)]
    pub entries: BTreeMap<String, Option<Fingerprint>>,
    /// **完全に取れたか。** 取れていない基準点で「担当内だけ」とは言えない。
    #[serde(default)]
    pub complete: bool,
    /// 取れなかった理由 (人へそのまま見せる)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub why: String,
}

impl FileBaseline {
    /// 測れる基準点か。
    pub fn usable(&self) -> bool {
        self.complete && valid_commit_id(&self.head_commit)
    }

    /// 測れなかったことを表す基準点。
    pub fn unavailable(why: impl Into<String>) -> Self {
        Self {
            taken_at: super::model::now_secs(),
            head_commit: String::new(),
            entries: BTreeMap::new(),
            complete: false,
            why: why.into(),
        }
    }
}

/// 変わり方。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// 基準点には無かった (新規、または rename の行き先)。
    Added,
    /// 内容が変わった。
    Modified,
    /// 消えた (削除、または rename の元)。
    Deleted,
}

/// 実測した変更 1 件。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MeasuredChange {
    /// ワークスペース相対・正規化済みのパス。
    pub path: String,
    pub kind: ChangeKind,
}

/// 測れなかった理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MeasureError {
    /// Git 管理下ではない。
    NotGitRepo,
    /// `git` を起こせない / 失敗した。
    GitFailed(String),
    /// 汚れているファイルが多すぎる。
    TooMany(usize),
    /// 読む量が多すぎる。
    TooLarge,
    /// 基準点が無い / 取れていない。
    NoBaseline(String),
    /// ワークスペースの外を指すパスが出てきた。
    Escapes(String),
}

impl MeasureError {
    pub fn detail(&self) -> String {
        match self {
            MeasureError::NotGitRepo => concat!(
                "ワークスペースが Git 管理下ではないため、",
                "実際に変更されたファイルを測れません"
            )
            .to_string(),
            MeasureError::GitFailed(e) => format!("git で変更を測れません: {e}"),
            MeasureError::TooMany(n) => {
                format!("変更されたファイルが多すぎます ({n} 件)。実測できません")
            }
            MeasureError::TooLarge => "変更の量が多すぎて実測できません".to_string(),
            MeasureError::NoBaseline(w) => {
                if w.is_empty() {
                    "このタスクには実測の基準点がありません".to_string()
                } else {
                    format!("実測の基準点がありません: {w}")
                }
            }
            MeasureError::Escapes(p) => {
                format!("ワークスペースの外を指すパスがあります: {p}")
            }
        }
    }
}

/// 配る直前の基準点を取る。
///
/// **失敗を `Err` で返す。** ただし`git init`直後のunborn branchは、
/// 既存の準備処理が現在の汚れを確認できるよう、理由つきの測定不能な
/// 基準点として返す。空の基準点を測定可能とは扱わない。
pub fn capture_baseline(workspace: &Path) -> Result<FileBaseline, MeasureError> {
    let (head_commit, complete, why) = match current_head(workspace) {
        Ok(head) => (head, true, String::new()),
        Err(_head_error)
            if crate::worktree::git_out(workspace, &["symbolic-ref", "-q", "HEAD"]).is_ok() =>
        {
            // git init直後のunborn branch。従来どおり現在の汚れは記録するが、
            // 比較元commitが無いのでこの基準点を測定可能とは扱わない。
            (
                String::new(),
                false,
                "Git HEADに基準commitがありません".to_string(),
            )
        }
        Err(head_error) => return Err(head_error),
    };
    let paths = dirty_paths(workspace)?;
    let entries = fingerprint_all(workspace, &paths)?;
    Ok(FileBaseline {
        taken_at: super::model::now_secs(),
        head_commit,
        entries,
        complete,
        why,
    })
}

/// 基準点からいままでに**実際に変わったもの**を測る。
///
/// 返すのは正規化済みの相対パス。並び順は決定的 (パス順)。
///
/// ## 判定の骨格
///
/// 見るのは `git status` に**載っているかどうか**と、載っているものの
/// 内容。載っていない = HEAD と同じ、という git の意味をそのまま使う:
///
/// | 基準点 | いま | 判定 |
/// | --- | --- | --- |
/// | 載っていない | 載っていない | 変わっていない (どちらも HEAD と同じ) |
/// | 載っていない | 載っている | **変わった** (HEAD と同じだったものが違う) |
/// | 載っている | 載っていない | **変わった** (HEAD の内容へ戻した) |
/// | 載っている | 載っている | 指紋が違えば変わった |
///
/// **指紋の比較だけでは足りない。** 基準点が「汚れているものの指紋」しか
/// 持たない以上、基準点の時点で綺麗だったファイルには指紋が無い。そこを
/// 「無い → 増えた」と読むと、**変更を新規と取り違え、削除は 1 件も
/// 見えなくなる** (実際にそうなった)。
pub fn measure(workspace: &Path, base: &FileBaseline) -> Result<Vec<MeasuredChange>, MeasureError> {
    if !base.usable() {
        let why = if base.why.is_empty() && base.head_commit.is_empty() {
            "基準HEADが保存されていません".to_string()
        } else {
            base.why.clone()
        };
        return Err(MeasureError::NoBaseline(why));
    }
    // 基準commitから現在のindex/working treeまでを直接比較する。これにより、
    // 途中でcommitされて現在の`git status`から消えた成果物も残る。
    let mut merged: BTreeMap<String, [u8; 2]> = changed_paths(workspace, &base.head_commit)?
        .into_iter()
        .map(|entry| (entry.path, entry.code))
        .collect();
    // `git diff`に出ない未追跡ファイルを加える。既に基準commitとの差分に
    // 出るパスは上書きせず、最終状態に対するChangeKindを維持する。
    for entry in dirty_paths(workspace)? {
        merged.entry(entry.path).or_insert(entry.code);
    }
    if merged.len() > MAX_TRACKED_PATHS {
        return Err(MeasureError::TooMany(merged.len()));
    }
    let entries: Vec<DirtyEntry> = merged
        .into_iter()
        .map(|(path, code)| DirtyEntry { path, code })
        .collect();
    let now = fingerprint_all(workspace, &entries)?;
    let codes: BTreeMap<&str, [u8; 2]> = entries
        .iter()
        .map(|e| (e.path.as_str(), e.code))
        .collect();

    let mut keys: Vec<&String> = base.entries.keys().chain(now.keys()).collect();
    keys.sort_unstable();
    keys.dedup();

    let mut out = Vec::new();
    for k in keys {
        let was = base.entries.get(k);
        let is = now.get(k);
        let changed = match (was, is) {
            (None, None) => false,
            (Some(a), Some(b)) => a != b,
            // 片方にしか無い = 「HEAD と同じ」と「違う」の間を移った。
            _ => true,
        };
        if !changed {
            continue;
        }
        out.push(MeasuredChange {
            path: k.clone(),
            // `Some(None)` = `git status` には出ているが実体が無い = 消えた。
            kind: kind_of(codes.get(k.as_str()).copied(), matches!(is, Some(Some(_)))),
        });
    }
    Ok(out)
}

/// `git status` の状態文字から変わり方を決める。
///
/// 状態文字が無い = もう `git status` に出ていない = **HEAD の内容へ
/// 戻した**。増減ではないので `Modified` として扱う。
fn kind_of(code: Option<[u8; 2]>, exists_now: bool) -> ChangeKind {
    let Some(c) = code else {
        return ChangeKind::Modified;
    };
    if c.contains(&b'D') || !exists_now {
        return ChangeKind::Deleted;
    }
    if c.contains(&b'?') || c.contains(&b'A') {
        return ChangeKind::Added;
    }
    ChangeKind::Modified
}

/// **誰の変更かを切り分ける** (純関数)。
///
/// 引数:
///
/// * `measured` — 実測した変更のパス
/// * `mine` — このタスクの担当範囲 (`TeamTask::files`)
/// * `others` — **いま他のタスクが握っている**担当範囲
///
///   計画に載っているだけの範囲を渡してはいけない。渡すと、まだ 1 度も
///   配られていないタスクの範囲が「他人のもの」になり、そこへ書き込んだ
///   違反が担当外として検出されなくなる。呼ぶ側は所有の真実
///   ([`crate::coordinator::occupies`]) から作ること。
///
/// 判定はファイル所有リースの `lease::overlaps` をそのまま使う
/// (第 2 の競合判定を作らない)。リースは範囲が互いに素であることを
/// 保証しているので、**他人が握っている範囲の変更は自分のものではない**
/// と言い切れる。
///
/// 戻り値は `(自分の変更, 担当外の変更)`。担当外には
/// 「誰も握っていない範囲」も入る — **自分ではないと言い切れない**ので、
/// 安全側 (自分のもの = 担当外) へ倒す。
pub fn attribute<'a>(
    measured: &'a [String],
    mine: &[String],
    others: &[String],
) -> (Vec<&'a String>, Vec<&'a String>) {
    let mut ours = Vec::new();
    let mut out_of_scope = Vec::new();
    for p in measured {
        if mine.iter().any(|m| crate::lease::overlaps(m, p)) {
            ours.push(p);
            continue;
        }
        // 他のタスクが握っている範囲なら、そのタスクの成果。
        if others.iter().any(|o| crate::lease::overlaps(o, p)) {
            continue;
        }
        // 誰の範囲でもない = 自分ではないと言えない。
        out_of_scope.push(p);
    }
    (ours, out_of_scope)
}

/// 実測した差分を「**この 1 人の成果**」として扱ってよいか。
///
/// [`attribute`] は「担当範囲が宣言されている」ことを前提にした切り分けで、
/// 範囲が 1 つも無いタスクには当てられない。それでも作業ツリー全体の差分を
/// そのまま持ち主の成果にすると、**隣の担当が書いたファイルまで「あなたが
/// 報告し忘れた変更」に数える**。
///
/// 実機の Run (6 体が同じワークスペース・計画に `files` が 1 つも無い) で、
/// 正しく働いた担当への誤った却下が 6 件出た:
///
/// ```text
/// #3 の報告と実測が食い違います (報告に無い変更: index.html, main.js, output/browser_verified.png, plan.md)
/// #1 の報告と実測が食い違います (報告に無い変更: main.js, plan.md, styles.css)
/// ```
///
/// この module の原則「**測れないときは通さない・保証を偽らない**」を、
/// 帰属にも当てる — 帰属できないなら食い違いを主張しない。**検査を
/// 弱めるのではない**: 範囲が宣言されていれば ([`Attribution::ByScope`])、
/// 同時に何人働いていても今までどおり照合する。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Attribution {
    /// 担当範囲が宣言されている。[`attribute`] が切り分ける。
    ByScope,
    /// 範囲は無いが、**同時に働いている担当が他に居ない**。
    /// 作業ツリーの差分はこの 1 人のものだと言い切れる。
    SoleWorker,
    /// 帰属の根拠が無い — 範囲が無いうえに、他に `others` 件の作業が
    /// 同時に進んでいる。
    Unattributable { others: usize },
}

impl Attribution {
    /// 実測を「この担当が変えたもの」として照合・台帳に使ってよいか。
    pub fn can_claim(self) -> bool {
        !matches!(self, Attribution::Unattributable { .. })
    }

    /// 帰属できない理由 (人が読む 1 行)。使えるときは `None`。
    ///
    /// **黙って捨てない。** 何件を帰属できなかったのかまで出す
    /// (`measured` = 実測できた変更の件数)。
    pub fn why(self, measured: usize) -> Option<String> {
        match self {
            Attribution::Unattributable { others } => Some(format!(
                "担当ファイルが宣言されておらず、他に {others} 件の作業が同時に進んでいるため、実測した {measured} 件の変更をこのタスクへ帰属できません"
            )),
            Attribution::ByScope | Attribution::SoleWorker => None,
        }
    }
}

/// 帰属できるかを決める。**I/O を持たない純関数** — 表で固定できる。
///
/// * `scope_len` — このタスクに宣言された担当範囲の数 (`TeamTask::files`)
/// * `other_holders` — いま**別の担当**が手を入れているタスクの数
///
/// 呼ぶ側は `other_holders` を安全側で数えること (担当が分からない
/// 進行中タスクは「別の担当」に数える) — 見落とすと、また誤った却下が出る。
pub fn attribution(scope_len: usize, other_holders: usize) -> Attribution {
    if scope_len > 0 {
        // 範囲があるなら `attribute` が切り分ける。何人居ても関係ない。
        Attribution::ByScope
    } else if other_holders == 0 {
        // 誰も同時に書いていない = 作業ツリーの差分はこの 1 人のもの。
        Attribution::SoleWorker
    } else {
        Attribution::Unattributable {
            others: other_holders,
        }
    }
}

/// パスがワークスペースの内側に**実際に**収まっているか。
///
/// 形だけの検査 (`..` を数える) では足りない。ワークスペースの中に
/// 外を指すシンボリックリンクがあれば、`link/x` は形の上では内側だが
/// 実体は外にある。**存在する親までを辿って**確かめる。
pub fn inside_workspace(workspace: &Path, rel: &str) -> bool {
    if rel.is_empty() {
        return false;
    }
    let p = Path::new(rel);
    if p.is_absolute() {
        return false;
    }
    // Windows のドライブ指定 (`C:x`)。
    let b = rel.as_bytes();
    if b.len() >= 2 && b[1] == b':' {
        return false;
    }
    let root = canon(workspace);
    let target = workspace.join(p);
    // **見るのは「親がどこか」**。最後の 1 個は辿らない。
    //
    // 最後まで辿ると、workspace の中にある「外を指すシンボリックリンク」
    // そのものが外だと判定される。しかしそのリンクは作業ツリーの中の
    // ファイルで、書き換えれば作業ツリーが変わる。一方 `link/secret.txt`
    // は親 (`link`) が外を指しているので、実体は外にある。
    // この 2 つを分けられるのが「親で見る」形。
    let Some(parent) = target.parent() else {
        return false;
    };
    if target.file_name().is_none() {
        // `a/..` のように最後が `..` — 辿れないので断る。
        return false;
    }
    // 親が実在するならそのまま辿る。無いなら**存在する一番近い先祖**まで
    // 辿り、残りを形で足す (これから作られるファイルも判定できるように)。
    let mut cur = parent.to_path_buf();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let resolved_parent = loop {
        if let Ok(c) = std::fs::canonicalize(&cur) {
            let mut out = c;
            for seg in tail.iter().rev() {
                // 途中に `..` が残っている = 辿れない。断る。
                if seg == ".." {
                    return false;
                }
                out.push(seg);
            }
            break out;
        }
        let Some(name) = cur.file_name().map(|x| x.to_os_string()) else {
            return false;
        };
        tail.push(name);
        let Some(up) = cur.parent().map(|x| x.to_path_buf()) else {
            return false;
        };
        cur = up;
    };
    resolved_parent.starts_with(&root)
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

// ── git ──────────────────────────────────────────────────────────────

/// 現在のHEADを完全なcommit IDで取る。失敗・空値を基準点にしない。
fn current_head(workspace: &Path) -> Result<String, MeasureError> {
    if crate::git::discover_toplevel(workspace).is_none() {
        return Err(MeasureError::NotGitRepo);
    }
    let out = run_git(workspace, &["rev-parse", "--verify", "HEAD^{commit}"])?;
    let head = std::str::from_utf8(&out)
        .map_err(|_| MeasureError::GitFailed("HEADをUTF-8として読めません".into()))?
        .trim()
        .to_string();
    if !valid_commit_id(&head) {
        return Err(MeasureError::GitFailed("HEADの形式が不正です".into()));
    }
    Ok(head)
}

/// 保存データ由来の値をGitの引数へ渡せる、完全なSHA-1/SHA-256 IDか。
fn valid_commit_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

/// 基準commitと現在のindex/working treeの差分。renameは削除+追加に分ける。
fn changed_paths(workspace: &Path, base_commit: &str) -> Result<Vec<DirtyEntry>, MeasureError> {
    let out = run_git(
        workspace,
        &[
            "diff",
            "--name-status",
            "-z",
            "--no-renames",
            "--no-ext-diff",
            "--relative",
            base_commit,
            "--",
        ],
    )?;
    let mut fields = out.split(|byte| *byte == 0).filter(|field| !field.is_empty());
    let mut entries = Vec::new();
    while let Some(status) = fields.next() {
        let path = fields.next().ok_or_else(|| {
            MeasureError::GitFailed("git diffのname-status出力が途中で切れています".into())
        })?;
        if status.len() != 1 {
            return Err(MeasureError::GitFailed(
                "git diffの状態コードを解釈できません".into(),
            ));
        }
        let path = std::str::from_utf8(path)
            .map_err(|_| MeasureError::GitFailed("ファイル名をUTF-8として読めません".into()))?;
        if path.is_empty() {
            continue;
        }
        if !inside_workspace(workspace, path) {
            return Err(MeasureError::Escapes(path.to_string()));
        }
        entries.push(DirtyEntry {
            path: crate::lease::normalize_path(path),
            code: [status[0], b' '],
        });
        if entries.len() > MAX_TRACKED_PATHS {
            return Err(MeasureError::TooMany(entries.len()));
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    Ok(entries)
}

/// 作業ツリーで**HEAD と違うファイル**の一覧 (追跡外も含む)。
///
/// `--no-renames` を付けるのは、rename を「消えた + 増えた」の 2 件として
/// 受け取るため。片方だけが担当範囲の中、という形を見逃さない。
fn dirty_paths(workspace: &Path) -> Result<Vec<DirtyEntry>, MeasureError> {
    if crate::git::discover_toplevel(workspace).is_none() {
        return Err(MeasureError::NotGitRepo);
    }
    // **シェルを通さない。** 引数は分けて渡す (文字列を組み立てない)。
    let out = run_git(
        workspace,
        &[
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--no-renames",
            "--ignore-submodules=none",
        ],
    )?;
    let mut entries: Vec<DirtyEntry> = Vec::new();
    for rec in out.split(|b| *b == 0) {
        if rec.len() < 4 {
            continue;
        }
        // `XY<空白>パス`
        let code = [rec[0], rec[1]];
        let Ok(s) = std::str::from_utf8(&rec[3..]) else {
            // 名前が UTF-8 で無い。**黙って飛ばさない** — 測れないと言う。
            return Err(MeasureError::GitFailed(
                "ファイル名を UTF-8 として読めません".into(),
            ));
        };
        if s.is_empty() {
            continue;
        }
        if !inside_workspace(workspace, s) {
            return Err(MeasureError::Escapes(s.to_string()));
        }
        entries.push(DirtyEntry {
            path: crate::lease::normalize_path(s),
            code,
        });
        if entries.len() > MAX_TRACKED_PATHS {
            return Err(MeasureError::TooMany(entries.len()));
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    entries.dedup_by(|a, b| a.path == b.path);
    Ok(entries)
}

/// `git status` の 1 行 (パスと状態文字)。
struct DirtyEntry {
    path: String,
    code: [u8; 2],
}

/// `git` を起こす (シェル無し・時限つき)。
fn run_git(workspace: &Path, args: &[&str]) -> Result<Vec<u8>, MeasureError> {
    let mut c = crate::procx::hidden_command("git");
    c.args(args)
        .current_dir(workspace)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = c.spawn().map_err(|e| MeasureError::GitFailed(e.to_string()))?;
    let out = wait_capped(child)?;
    if !out.status.success() {
        let msg = String::from_utf8_lossy(&out.stderr);
        return Err(MeasureError::GitFailed(
            msg.lines().next().unwrap_or("失敗").trim().to_string(),
        ));
    }
    Ok(out.stdout)
}

/// 時限つきで待つ。**無期限に待たない** — git が固まったら測れないと言う。
fn wait_capped(mut child: std::process::Child) -> Result<std::process::Output, MeasureError> {
    let start = std::time::Instant::now();
    // 出力は先に別スレッドで吸う (パイプが詰まると `try_wait` が永久に
    // `None` を返す)。
    let mut out = child.stdout.take();
    let mut err = child.stderr.take();
    let out_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(r) = out.as_mut() {
            use std::io::Read;
            let _ = r.read_to_end(&mut b);
        }
        b
    });
    let err_h = std::thread::spawn(move || {
        let mut b = Vec::new();
        if let Some(r) = err.as_mut() {
            use std::io::Read;
            let _ = r.read_to_end(&mut b);
        }
        b
    });
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let stdout = out_h.join().unwrap_or_default();
                let stderr = err_h.join().unwrap_or_default();
                return Ok(std::process::Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Err(e) => return Err(MeasureError::GitFailed(e.to_string())),
            Ok(None) => {}
        }
        if start.elapsed() >= GIT_TIMEOUT {
            crate::procx::kill_tree(child.id());
            let _ = child.wait();
            return Err(MeasureError::GitFailed("git が応答しません".into()));
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

// ── 指紋 ─────────────────────────────────────────────────────────────

/// 汚れているものの指紋を作る。
///
/// **消えているファイルも鍵として載せる** (値は `None`)。載せないと
/// 「`git status` に出ている = HEAD と違う」という事実そのものが落ちて、
/// 削除が 1 件も見えなくなる。
fn fingerprint_all(
    workspace: &Path,
    entries: &[DirtyEntry],
) -> Result<BTreeMap<String, Option<Fingerprint>>, MeasureError> {
    let mut out = BTreeMap::new();
    let mut budget = MAX_HASH_BYTES;
    for e in entries {
        let abs = workspace.join(&e.path);
        out.insert(e.path.clone(), fingerprint(&abs, &mut budget)?);
    }
    Ok(out)
}

/// 1 ファイルの指紋。**シンボリックリンクは辿らない** — リンクそのものの
/// 向き先が変わったことを、内容の変化として見たい。
///
/// 予算を引数で受けるのは、**予算切れの振る舞いを試験から直に引ける**ように
/// するため (`MAX_HASH_BYTES` ぶんのファイルを書かないと試せない作りにすると、
/// その関門は重すぎて誰も回さなくなる)。
pub(crate) fn fingerprint(abs: &Path, budget: &mut u64) -> Result<Option<Fingerprint>, MeasureError> {
    let Ok(meta) = std::fs::symlink_metadata(abs) else {
        return Ok(None);
    };
    if meta.is_dir() {
        return Ok(None);
    }
    if meta.is_symlink() {
        let target = std::fs::read_link(abs).unwrap_or_default();
        let bytes = target.to_string_lossy();
        return Ok(Some(Fingerprint {
            hash: crate::history::fnv1a64(bytes.as_bytes()),
            len: bytes.len() as u64,
        }));
    }
    // **大きくても内容で見る。** 「大きいものは長さだけ」にすると、
    // 10MB のファイルを**同じ長さの別の内容**へ書き換えた変更が
    // `Fingerprint` として等しくなり、「変更なし」と読める。それは
    // 「自己申告ではなくこちらが測る」という保証そのものを崩す。
    // 読む量は予算 (`MAX_HASH_BYTES`) で抑え、**足りなければ測れないと言う**。
    hash_stream(abs, budget)
}

/// ファイルを**少しずつ読んで**指紋を作る。
///
/// 一括で読むと 1 ファイルぶんがそのままメモリに載るので、固定の
/// バッファで流す。
///
/// 予算は**読んだぶんだけ**引き、判定も読みながら 1 か所でだけ行う。
/// `symlink_metadata` の長さで先に断る形にすると、その分岐は
/// 「メタ情報が嘘だったとき」にしか通らない枝を別に抱えることになる
/// (通らない枝は、そのうち壊れても誰も気付かない)。読む総量は
/// どちらにしても予算で抑えられている。
fn hash_stream(abs: &Path, budget: &mut u64) -> Result<Option<Fingerprint>, MeasureError> {
    use std::io::Read;

    let Ok(file) = std::fs::File::open(abs) else {
        return Ok(None);
    };
    let mut r = std::io::BufReader::new(file);
    let mut buf = [0u8; HASH_CHUNK_BYTES];
    let mut hasher = crate::history::Fnv1a64::default();
    let mut len: u64 = 0;
    loop {
        let n = match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => return Ok(None),
        };
        let n64 = n as u64;
        if *budget < n64 {
            // 読み切れないまま指紋を返すと、**途中までが一致した別の内容**を
            // 同じものとして扱うことになる。測れないと言う。
            return Err(MeasureError::TooLarge);
        }
        *budget -= n64;
        len += n64;
        hasher.update(&buf[..n]);
    }
    Ok(Some(Fingerprint {
        hash: hasher.finish(),
        len,
    }))
}

#[cfg(test)]
mod git_measure_tests {
    use super::*;

    fn repo(tag: &str) -> Option<PathBuf> {
        let root = crate::test_util::unique_temp_dir("zai-team-changeset", tag);
        std::fs::create_dir_all(&root).ok()?;
        std::fs::write(root.join("modified.txt"), "before\n").ok()?;
        std::fs::write(root.join("deleted.txt"), "delete me\n").ok()?;
        let ok = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(&root)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        if !ok(&["init", "-q"])
            || !ok(&["config", "user.email", "team@example.invalid"])
            || !ok(&["config", "user.name", "Team"])
            || !ok(&["config", "commit.gpgsign", "false"])
            || !ok(&["add", "modified.txt", "deleted.txt"])
            || !ok(&["commit", "-q", "-m", "base"])
        {
            std::fs::remove_dir_all(&root).ok();
            return None;
        }
        Some(root)
    }

    #[test]
    fn commit済みのadd_modify_deleteと後続の未commit変更を重複なく測る() {
        let Some(root) = repo("committed-and-working") else {
            return;
        };
        let base = capture_baseline(&root).expect("基準点");
        assert!(!base.head_commit.is_empty(), "基準HEADを保存していない");

        std::fs::write(root.join("modified.txt"), "committed\n").unwrap();
        std::fs::write(root.join("added.txt"), "added\n").unwrap();
        std::fs::remove_file(root.join("deleted.txt")).unwrap();
        run_git(&root, &["add", "-A"]).unwrap();
        run_git(&root, &["commit", "-q", "-m", "result"]).unwrap();

        let committed = measure(&root, &base).expect("commit済み差分を測れる");
        assert_eq!(
            committed,
            vec![
                MeasuredChange {
                    path: "added.txt".into(),
                    kind: ChangeKind::Added,
                },
                MeasuredChange {
                    path: "deleted.txt".into(),
                    kind: ChangeKind::Deleted,
                },
                MeasuredChange {
                    path: "modified.txt".into(),
                    kind: ChangeKind::Modified,
                },
            ],
            "cleanなcommit済み成果物がchangesetから消えた"
        );

        std::fs::write(root.join("modified.txt"), "working\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "untracked\n").unwrap();
        let both = measure(&root, &base).expect("commit済みと未commitを一緒に測れる");
        assert_eq!(
            both.iter().filter(|c| c.path == "modified.txt").count(),
            1,
            "同じファイルをcommit済み差分と未commit差分で二重計上した"
        );
        assert!(both.iter().any(|c| c.path == "untracked.txt" && c.kind == ChangeKind::Added));
        assert!(both.iter().any(|c| c.path == "added.txt" && c.kind == ChangeKind::Added));
        assert!(both.iter().any(|c| c.path == "deleted.txt" && c.kind == ChangeKind::Deleted));
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 基準headが無い旧baselineと不正値はgitへ渡さず測定不能にする() {
        let Some(root) = repo("legacy-baseline") else {
            return;
        };
        for head_commit in [String::new(), "--output=/outside".to_string()] {
            let old = FileBaseline {
                taken_at: 1,
                head_commit,
                entries: BTreeMap::new(),
                complete: true,
                why: String::new(),
            };
            assert!(!old.usable());
            assert!(matches!(measure(&root, &old), Err(MeasureError::NoBaseline(_))));
        }
        std::fs::remove_dir_all(&root).ok();
    }
}


#[cfg(test)]
mod attribution_tests {
    use super::*;

    /// **表で固定する。** 判定はこの 3 つしかない — 分岐を増やすと
    /// 「どういうときに食い違いを主張しているのか」が読めなくなる。
    #[test]
    fn 帰属できるかは担当範囲と同時に働く人数だけで決まる() {
        // (担当範囲の数, 他に働いている担当の数, 期待, 照合してよいか)
        let table: &[(usize, usize, Attribution, bool)] = &[
            // 範囲があるなら `attribute` が切り分ける。**検査は弱めない** —
            // 何人が同時に働いていても今までどおり照合する。
            (1, 0, Attribution::ByScope, true),
            (1, 5, Attribution::ByScope, true),
            (3, 63, Attribution::ByScope, true),
            // 範囲は無いが独りで働いている = 作業ツリーの差分はこの人のもの。
            (0, 0, Attribution::SoleWorker, true),
            // 範囲が無く、隣でも書かれている = 帰属の根拠が無い。
            (0, 1, Attribution::Unattributable { others: 1 }, false),
            (0, 5, Attribution::Unattributable { others: 5 }, false),
        ];
        for (scope, others, want, claim) in table {
            let got = attribution(*scope, *others);
            assert_eq!(got, *want, "scope={scope} others={others}");
            assert_eq!(
                got.can_claim(),
                *claim,
                "scope={scope} others={others} → {got:?}"
            );
        }
    }

    #[test]
    fn 帰属できないときは理由が残る() {
        // **黙って捨てない。** 何人が同時に働いていて、何件を帰属
        // できなかったのかまで人へ出す。
        let why = attribution(0, 5).why(4).expect("理由が空のまま捨てた");
        assert!(why.contains('5'), "同時に働いている人数が出ていない: {why}");
        assert!(why.contains('4'), "測れた件数が出ていない: {why}");
        assert!(why.contains("帰属"), "何が起きたのか伝わらない: {why}");
        // 帰属できるときは理由を出さない (出すと盤面が濁る)。
        assert_eq!(attribution(2, 5).why(4), None, "範囲があるのに濁らせた");
        assert_eq!(attribution(0, 0).why(4), None, "独りなのに濁らせた");
    }
}
