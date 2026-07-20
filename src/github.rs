//! GitHub 連携。`gh` CLI を子プロセスとして呼び、結果を JSON で受け取る。
//!
//! ネットワーク待ちで UI が固まらないよう、呼び出しは必ずワーカースレッドで行い
//! 結果は mpsc チャネル経由で UI スレッドへ返す。
//!
//! 設計方針:
//! - プロセス起動 (`run_blocking`) と JSON 解析 (`parse_*`) を分離し、解析側は
//!   純関数としてユニットテストできるようにする。
//! - エラーは握り潰さない。`gh` の stderr は必ず利用者向けの日本語メッセージへ
//!   織り込む (src/git.rs の run_git は stderr を捨てているが、ここでは踏襲しない)。
//! - 空リスト (`[]`) はエラーではなく正常な結果として扱う。
#![allow(dead_code)]

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

/// 既定タイムアウト。`gh pr list` は実測 0.6 秒程度だが、ネットワーク不調に備える。
const DEFAULT_TIMEOUT_SECS: u64 = 30;
/// `gh pr checkout` は fetch を伴うため長めに取る。
const CHECKOUT_TIMEOUT_SECS: u64 = 120;

// ---------------------------------------------------------------------------
// リクエスト / 結果型
// ---------------------------------------------------------------------------

/// UI スレッドで組み立ててワーカースレッドへ渡す要求。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhRequest {
    /// リポジトリの基本情報 (owner / name / 既定ブランチ / URL)。
    RepoView { root: PathBuf },
    /// オープンな Pull Request 一覧。
    PrList { root: PathBuf, limit: usize },
    /// オープンな Issue 一覧。
    IssueList { root: PathBuf, limit: usize },
    /// 指定 PR の unified diff (生テキスト)。
    PrDiff { root: PathBuf, number: u64 },
    /// 指定 PR をローカルへチェックアウト (作業ツリーを変更する)。
    PrCheckout { root: PathBuf, number: u64 },
    /// リモートブランチ一覧。
    BranchList { root: PathBuf },
}

impl GhRequest {
    /// 作業ディレクトリ。
    pub fn root(&self) -> &Path {
        match self {
            GhRequest::RepoView { root }
            | GhRequest::PrList { root, .. }
            | GhRequest::IssueList { root, .. }
            | GhRequest::PrDiff { root, .. }
            | GhRequest::PrCheckout { root, .. }
            | GhRequest::BranchList { root } => root.as_path(),
        }
    }

    /// エラー表示に使う日本語のラベル。
    pub fn label(&self) -> String {
        match self {
            GhRequest::RepoView { .. } => "リポジトリ情報の取得".into(),
            GhRequest::PrList { .. } => "Pull Request 一覧の取得".into(),
            GhRequest::IssueList { .. } => "Issue 一覧の取得".into(),
            GhRequest::PrDiff { number, .. } => format!("PR #{number} の差分取得"),
            GhRequest::PrCheckout { number, .. } => format!("PR #{number} のチェックアウト"),
            GhRequest::BranchList { .. } => "ブランチ一覧の取得".into(),
        }
    }

    /// 作業ツリーを書き換える要求か (UI 側で確認ダイアログを出す判断に使う)。
    pub fn is_mutating(&self) -> bool {
        matches!(self, GhRequest::PrCheckout { .. })
    }

    fn timeout(&self) -> Duration {
        match self {
            GhRequest::PrCheckout { .. } => Duration::from_secs(CHECKOUT_TIMEOUT_SECS),
            _ => Duration::from_secs(DEFAULT_TIMEOUT_SECS),
        }
    }
}

/// Pull Request 1 件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullRequest {
    pub number: u64,
    pub title: String,
    /// 表示名。`author.name` が空なら `author.login` を使う。
    pub author: String,
    pub head_ref: String,
    pub base_ref: String,
    pub is_draft: bool,
    /// "OPEN" / "MERGED" / "CLOSED"。
    pub state: String,
    /// ISO8601 (UTC) のまま保持する。表示時は [`humanize_utc`] を通す。
    pub updated_at: String,
    pub additions: u64,
    pub deletions: u64,
    pub url: String,
}

/// Issue 1 件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Issue {
    pub number: u64,
    pub title: String,
    pub author: String,
    /// "OPEN" / "CLOSED"。
    pub state: String,
    pub labels: Vec<String>,
    pub updated_at: String,
    pub url: String,
}

/// リポジトリの基本情報。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepoInfo {
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub url: String,
}

impl RepoInfo {
    /// "owner/name" 形式。
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

/// リモートブランチ 1 件。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub sha: String,
    pub protected: bool,
}

/// ワーカースレッドから UI スレッドへ返す結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GhOutcome {
    Repo(RepoInfo),
    Prs(Vec<PullRequest>),
    Issues(Vec<Issue>),
    Diff { number: u64, text: String },
    Checkout { number: u64, message: String },
    Branches(Vec<Branch>),
    /// 失敗。`req_label` は [`GhRequest::label`]、`message` は利用者向け日本語。
    Error { req_label: String, message: String },
}

impl GhOutcome {
    pub fn is_error(&self) -> bool {
        matches!(self, GhOutcome::Error { .. })
    }

    /// エラーなら整形済みの 1 行メッセージ。
    pub fn error_text(&self) -> Option<String> {
        match self {
            GhOutcome::Error { req_label, message } => Some(format!("{req_label}に失敗: {message}")),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// gh の存在確認
// ---------------------------------------------------------------------------

/// 0 = 未確認 / 1 = あり / 2 = なし。
static GH_PRESENT: AtomicU8 = AtomicU8::new(0);

/// `gh` が PATH 上にあるか。結果はプロセス内でキャッシュする (毎フレーム呼ばれても安全)。
///
/// GUI アプリはログインシェルの PATH を継承しないことがあるため、
/// `$SHELL -lc 'command -v gh'` で確認する (src/app.rs の `which` と同じ手口)。
pub fn gh_available() -> bool {
    match GH_PRESENT.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let found = probe_gh();
    GH_PRESENT.store(if found { 1 } else { 2 }, Ordering::Relaxed);
    found
}

/// キャッシュを捨てる (ユーザーが後から gh を入れた場合の再確認用)。
pub fn reset_gh_cache() {
    GH_PRESENT.store(0, Ordering::Relaxed);
}

fn probe_gh() -> bool {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let via_shell = Command::new(&shell)
        .arg("-lc")
        .arg("command -v gh")
        .output()
        .map(|o| o.status.success() && !o.stdout.is_empty())
        .unwrap_or(false);
    if via_shell {
        return true;
    }
    // ログインシェルが使えない環境向けのフォールバック。
    Command::new("gh")
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// `gh auth status` を実行して認証済みか調べる。ネットワークアクセスを伴うので
/// 毎フレームではなく明示的な操作 (設定画面など) からのみ呼ぶこと。
pub fn check_auth() -> Result<(), String> {
    if !gh_available() {
        return Err(msg_gh_missing());
    }
    match Command::new("gh")
        .args(["auth", "status"])
        .stdin(Stdio::null())
        .output()
    {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => {
            let stderr = trim_stderr(&String::from_utf8_lossy(&out.stderr));
            Err(msg_not_authenticated(&stderr))
        }
        Err(e) => Err(format!("gh を起動できません: {e}")),
    }
}

// ---------------------------------------------------------------------------
// 非同期実行
// ---------------------------------------------------------------------------

/// バックグラウンドスレッドで `gh` を実行し、完了時に `tx` へ結果を送って再描画を促す。
/// (src/plugins.rs の `run_async` と同じ構造)
pub fn run_async(req: GhRequest, tx: Sender<GhOutcome>, ctx: egui::Context) {
    std::thread::spawn(move || {
        let outcome = run_blocking(&req);
        let _ = tx.send(outcome);
        ctx.request_repaint();
    });
}

/// 同期版。UI スレッドから直接呼んではいけない (1 回あたり数百 ms かかる)。
pub fn run_blocking(req: &GhRequest) -> GhOutcome {
    let label = req.label();
    let fail = |message: String| GhOutcome::Error {
        req_label: label.clone(),
        message,
    };

    if !gh_available() {
        return fail(msg_gh_missing());
    }
    let root = req.root();
    if !root.is_dir() {
        return fail(format!(
            "作業ディレクトリが見つかりません: {}",
            root.display()
        ));
    }

    let args: Vec<String> = match req {
        GhRequest::RepoView { .. } => vec![
            "repo".into(),
            "view".into(),
            "--json".into(),
            "name,owner,defaultBranchRef,url".into(),
        ],
        GhRequest::PrList { limit, .. } => vec![
            "pr".into(),
            "list".into(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            clamp_limit(*limit).to_string(),
            "--json".into(),
            "number,title,author,headRefName,baseRefName,isDraft,state,updatedAt,additions,deletions,url".into(),
        ],
        GhRequest::IssueList { limit, .. } => vec![
            "issue".into(),
            "list".into(),
            "--state".into(),
            "open".into(),
            "--limit".into(),
            clamp_limit(*limit).to_string(),
            "--json".into(),
            "number,title,author,state,labels,updatedAt,url".into(),
        ],
        GhRequest::PrDiff { number, .. } => {
            vec!["pr".into(), "diff".into(), number.to_string()]
        }
        GhRequest::PrCheckout { number, .. } => {
            vec!["pr".into(), "checkout".into(), number.to_string()]
        }
        GhRequest::BranchList { .. } => vec![
            "api".into(),
            "--paginate".into(),
            "repos/{owner}/{repo}/branches?per_page=100".into(),
        ],
    };

    let argv: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let stdout = match capture(root, &argv, req.timeout()) {
        Ok(s) => s,
        Err(message) => return fail(message),
    };

    match req {
        GhRequest::RepoView { .. } => match parse_repo(&stdout) {
            Ok(v) => GhOutcome::Repo(v),
            Err(e) => fail(e),
        },
        GhRequest::PrList { .. } => match parse_prs(&stdout) {
            Ok(v) => GhOutcome::Prs(v),
            Err(e) => fail(e),
        },
        GhRequest::IssueList { .. } => match parse_issues(&stdout) {
            Ok(v) => GhOutcome::Issues(v),
            Err(e) => fail(e),
        },
        GhRequest::BranchList { .. } => match parse_branches(&stdout) {
            Ok(v) => GhOutcome::Branches(v),
            Err(e) => fail(e),
        },
        GhRequest::PrDiff { number, .. } => GhOutcome::Diff {
            number: *number,
            text: stdout,
        },
        GhRequest::PrCheckout { number, .. } => {
            let msg = stdout.trim();
            GhOutcome::Checkout {
                number: *number,
                message: if msg.is_empty() {
                    format!("PR #{number} をチェックアウトしました")
                } else {
                    msg.to_string()
                },
            }
        }
    }
}

fn clamp_limit(limit: usize) -> usize {
    limit.clamp(1, 200)
}

/// `gh` を `root` で実行し stdout を返す。失敗時は利用者向け日本語メッセージ。
///
/// `gh` には `git -C` に相当するオプションがないため `current_dir` で指定する。
fn capture(root: &Path, args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new("gh")
        .args(args)
        .current_dir(root)
        // 対話プロンプト・ページャ・色を確実に無効化する (パイプ越しでも保険)。
        .env("GH_PROMPT_DISABLED", "1")
        .env("GH_PAGER", "cat")
        .env("PAGER", "cat")
        .env("NO_COLOR", "1")
        .env("CLICOLOR", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("gh を起動できません: {e}"))?;

    // stdout/stderr を別スレッドで読み切る (パイプ満杯によるデッドロック回避)。
    let out_rx = child.stdout.take().map(spawn_reader);
    let err_rx = child.stderr.take().map(spawn_reader);

    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(format!(
                        "gh の応答が {} 秒を超えたため中断しました",
                        timeout.as_secs()
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(e) => return Err(format!("gh の終了待ちに失敗しました: {e}")),
        }
    };

    let stdout = out_rx.and_then(|h| h.join().ok()).unwrap_or_default();
    let stderr = err_rx.and_then(|h| h.join().ok()).unwrap_or_default();

    if status.success() {
        return Ok(stdout);
    }
    Err(classify_failure(status.code(), &stderr))
}

fn spawn_reader<R: Read + Send + 'static>(mut r: R) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        String::from_utf8_lossy(&buf).into_owned()
    })
}

// ---------------------------------------------------------------------------
// エラーメッセージ組み立て (純関数 — テスト対象)
// ---------------------------------------------------------------------------

pub fn msg_gh_missing() -> String {
    "GitHub CLI (gh) が見つかりません。https://cli.github.com からインストールしてください。".into()
}

pub fn msg_not_authenticated(detail: &str) -> String {
    let base = "GitHub にログインしていません。ターミナルで `gh auth login` を実行してください。";
    if detail.is_empty() {
        base.into()
    } else {
        format!("{base} (gh: {detail})")
    }
}

/// `gh` の異常終了を利用者向けメッセージへ変換する。
/// stderr は決して捨てず、分類できない場合もそのまま添える。
pub fn classify_failure(code: Option<i32>, stderr: &str) -> String {
    let detail = trim_stderr(stderr);
    let lower = detail.to_ascii_lowercase();

    if detail.is_empty() {
        return match code {
            Some(c) => format!("gh が終了コード {c} で失敗しました (詳細メッセージなし)"),
            None => "gh がシグナルで中断されました".into(),
        };
    }

    // 認証まわり
    if lower.contains("gh auth login")
        || lower.contains("not logged in")
        || lower.contains("authentication")
        || lower.contains("bad credentials")
        || lower.contains("http 401")
    {
        return msg_not_authenticated(&detail);
    }
    // git リポジトリではない
    if lower.contains("not a git repository") || lower.contains("no git repository") {
        return format!("ここは git リポジトリではありません。(gh: {detail})");
    }
    // GitHub リモートがない / base repo を決められない
    if lower.contains("no git remotes")
        || lower.contains("none of the git remotes")
        || lower.contains("could not determine base repository")
        || lower.contains("no remotes found")
    {
        return format!(
            "GitHub のリモートが設定されていません。`git remote add origin <URL>` を実行してください。(gh: {detail})"
        );
    }
    // 権限・存在しない
    if lower.contains("http 404") || lower.contains("could not resolve to a repository") {
        return format!("リポジトリが見つからないか、参照する権限がありません。(gh: {detail})");
    }
    if lower.contains("http 403") || lower.contains("rate limit") {
        return format!("GitHub API に拒否されました (権限不足かレート制限)。(gh: {detail})");
    }
    // ネットワーク
    if lower.contains("dial tcp")
        || lower.contains("no such host")
        || lower.contains("connection refused")
        || lower.contains("i/o timeout")
    {
        return format!("GitHub へ接続できません。ネットワークを確認してください。(gh: {detail})");
    }

    match code {
        Some(c) => format!("gh が終了コード {c} で失敗しました: {detail}"),
        None => format!("gh が中断されました: {detail}"),
    }
}

/// stderr を 1 行にまとめて長さを抑える (空行除去 + 先頭 4 行 + 400 文字上限)。
pub fn trim_stderr(stderr: &str) -> String {
    let joined = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .take(4)
        .collect::<Vec<_>>()
        .join(" / ");
    if joined.chars().count() > 400 {
        let cut: String = joined.chars().take(400).collect();
        format!("{cut}…")
    } else {
        joined
    }
}

// ---------------------------------------------------------------------------
// JSON パース (純関数 — テスト対象)
// ---------------------------------------------------------------------------

fn decode(json: &str, what: &str) -> Result<serde_json::Value, String> {
    let text = json.trim();
    if text.is_empty() {
        return Err(format!("{what}の応答が空でした"));
    }
    serde_json::from_str(text)
        .map_err(|e| format!("{what}の応答を JSON として解釈できませんでした: {e}"))
}

fn as_array<'a>(v: &'a serde_json::Value, what: &str) -> Result<&'a Vec<serde_json::Value>, String> {
    v.as_array()
        .ok_or_else(|| format!("{what}の応答が配列ではありません"))
}

fn str_at(v: &serde_json::Value, key: &str) -> String {
    v.get(key).and_then(|x| x.as_str()).unwrap_or("").to_string()
}

fn u64_at(v: &serde_json::Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn bool_at(v: &serde_json::Value, key: &str) -> bool {
    v.get(key).and_then(|x| x.as_bool()).unwrap_or(false)
}

/// `author` オブジェクトから表示名を取り出す。`name` が空なら `login`、
/// どちらも無ければ "(unknown)"。
pub fn author_display(author: Option<&serde_json::Value>) -> String {
    let Some(a) = author else {
        return "(unknown)".into();
    };
    let name = a.get("name").and_then(|x| x.as_str()).unwrap_or("").trim();
    if !name.is_empty() {
        return name.to_string();
    }
    let login = a.get("login").and_then(|x| x.as_str()).unwrap_or("").trim();
    if !login.is_empty() {
        return login.to_string();
    }
    "(unknown)".into()
}

/// `gh pr list --json number,title,author,headRefName,baseRefName,isDraft,state,updatedAt,additions,deletions,url`
/// の出力をパースする。空配列 `[]` は空 Vec (エラーではない)。
pub fn parse_prs(json: &str) -> Result<Vec<PullRequest>, String> {
    let v = decode(json, "Pull Request 一覧")?;
    let arr = as_array(&v, "Pull Request 一覧")?;
    Ok(arr
        .iter()
        .map(|it| PullRequest {
            number: u64_at(it, "number"),
            title: str_at(it, "title"),
            author: author_display(it.get("author")),
            head_ref: str_at(it, "headRefName"),
            base_ref: str_at(it, "baseRefName"),
            is_draft: bool_at(it, "isDraft"),
            state: str_at(it, "state"),
            updated_at: str_at(it, "updatedAt"),
            additions: u64_at(it, "additions"),
            deletions: u64_at(it, "deletions"),
            url: str_at(it, "url"),
        })
        .collect())
}

/// `gh issue list --json number,title,author,state,labels,updatedAt,url` の出力をパースする。
pub fn parse_issues(json: &str) -> Result<Vec<Issue>, String> {
    let v = decode(json, "Issue 一覧")?;
    let arr = as_array(&v, "Issue 一覧")?;
    Ok(arr
        .iter()
        .map(|it| Issue {
            number: u64_at(it, "number"),
            title: str_at(it, "title"),
            author: author_display(it.get("author")),
            state: str_at(it, "state"),
            labels: it
                .get("labels")
                .and_then(|x| x.as_array())
                .map(|ls| {
                    ls.iter()
                        .filter_map(|l| {
                            // 文字列そのままの場合と {name: ...} の場合の両方を許容する。
                            l.as_str()
                                .map(str::to_string)
                                .or_else(|| l.get("name").and_then(|n| n.as_str()).map(str::to_string))
                        })
                        .filter(|s| !s.is_empty())
                        .collect()
                })
                .unwrap_or_default(),
            updated_at: str_at(it, "updatedAt"),
            url: str_at(it, "url"),
        })
        .collect())
}

/// `gh repo view --json name,owner,defaultBranchRef,url` の出力をパースする。
pub fn parse_repo(json: &str) -> Result<RepoInfo, String> {
    let v = decode(json, "リポジトリ情報")?;
    if !v.is_object() {
        return Err("リポジトリ情報の応答がオブジェクトではありません".into());
    }
    let name = str_at(&v, "name");
    if name.is_empty() {
        return Err("リポジトリ情報に name が含まれていません".into());
    }
    Ok(RepoInfo {
        owner: v
            .get("owner")
            .map(|o| {
                o.get("login")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string()
            })
            .unwrap_or_default(),
        name,
        default_branch: v
            .get("defaultBranchRef")
            .and_then(|d| d.get("name"))
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string(),
        url: str_at(&v, "url"),
    })
}

/// `gh api repos/{owner}/{repo}/branches` の出力をパースする。
pub fn parse_branches(json: &str) -> Result<Vec<Branch>, String> {
    let v = decode(json, "ブランチ一覧")?;
    let arr = as_array(&v, "ブランチ一覧")?;
    Ok(arr
        .iter()
        .filter_map(|it| {
            let name = str_at(it, "name");
            if name.is_empty() {
                return None;
            }
            Some(Branch {
                name,
                sha: it
                    .get("commit")
                    .and_then(|c| c.get("sha"))
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string(),
                protected: bool_at(it, "protected"),
            })
        })
        .collect())
}

/// "2026-07-19T01:35:13Z" → "2026-07-19 01:35"。解釈できなければ入力をそのまま返す。
pub fn humanize_utc(ts: &str) -> String {
    let b = ts.as_bytes();
    if b.len() >= 16 && b[10] == b'T' {
        format!("{} {}", &ts[..10], &ts[11..16])
    } else {
        ts.to_string()
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // gh が実際に返す生の出力 (このセッションで cli/cli に対して採取したもの)。
    const REAL_PRS: &str = r#"[{"additions":32,"author":{"id":"U_kgDOBn1FSg","is_bot":false,"login":"AdarshJ173","name":"A.Adarsh Jagannath"},"baseRefName":"trunk","deletions":4,"headRefName":"feat/pr-create-json-output","isDraft":false,"number":13918,"state":"OPEN","title":"feat(pr create): add --json and --jq flags for machine-readable output","updatedAt":"2026-07-19T01:35:13Z","url":"https://github.com/cli/cli/pull/13918"},{"additions":68,"author":{"id":"U_kgDOCO0SBg","is_bot":false,"login":"Devil1716","name":""},"baseRefName":"trunk","deletions":1,"headRefName":"fix/issue-search-pagination-limit","isDraft":false,"number":13913,"state":"OPEN","title":"Fix issue search pagination writing reduced page size to wrong var","updatedAt":"2026-07-19T07:23:29Z","url":"https://github.com/cli/cli/pull/13913"}]"#;

    const REAL_ISSUES: &str = r#"[{"author":{"id":"U_kgDOC4Ws3g","is_bot":false,"login":"danielfikko","name":"daniel"},"labels":[{"id":"LA_kwDODKw3uc7QD3p7","name":"needs-triage","description":"needs to be reviewed","color":"D6393F"}],"number":13921,"state":"OPEN","title":"`--clipboard` no longer works with `gh auth login`","updatedAt":"2026-07-20T12:51:19Z","url":"https://github.com/cli/cli/issues/13921"},{"author":{"id":"U_kgDOEXIQ4Q","is_bot":false,"login":"scorpiomaster066-art","name":""},"labels":[],"number":13920,"state":"OPEN","title":"gh auth login stores token in plain text without prior warning .","updatedAt":"2026-07-20T06:38:26Z","url":"https://github.com/cli/cli/issues/13920"}]"#;

    const REAL_REPO: &str = r#"{"defaultBranchRef":{"name":"main"},"name":"zaivern-code","owner":{"id":"MDQ6VXNlcjg5ODA0NTQ=","login":"tacyan"},"url":"https://github.com/tacyan/zaivern-code"}"#;

    // ---- parse_prs ----

    #[test]
    fn parse_prs_real_output() {
        let prs = parse_prs(REAL_PRS).expect("parse");
        assert_eq!(prs.len(), 2);
        let p = &prs[0];
        assert_eq!(p.number, 13918);
        assert_eq!(p.author, "A.Adarsh Jagannath");
        assert_eq!(p.head_ref, "feat/pr-create-json-output");
        assert_eq!(p.base_ref, "trunk");
        assert!(!p.is_draft);
        assert_eq!(p.state, "OPEN");
        assert_eq!(p.additions, 32);
        assert_eq!(p.deletions, 4);
        assert_eq!(p.url, "https://github.com/cli/cli/pull/13918");
        assert_eq!(p.updated_at, "2026-07-19T01:35:13Z");
    }

    #[test]
    fn parse_prs_empty_author_name_falls_back_to_login() {
        let prs = parse_prs(REAL_PRS).expect("parse");
        assert_eq!(prs[1].author, "Devil1716");
    }

    #[test]
    fn parse_prs_empty_list_is_ok_not_error() {
        let prs = parse_prs("[]").expect("empty list must be Ok");
        assert!(prs.is_empty());
        // 前後の空白があっても同じ。
        assert!(parse_prs("  [] \n").expect("ws").is_empty());
    }

    #[test]
    fn parse_prs_missing_optional_fields_use_defaults() {
        let prs = parse_prs(r#"[{"number":7,"title":"t"}]"#).expect("parse");
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].number, 7);
        assert_eq!(prs[0].title, "t");
        assert_eq!(prs[0].author, "(unknown)");
        assert_eq!(prs[0].head_ref, "");
        assert_eq!(prs[0].additions, 0);
        assert!(!prs[0].is_draft);
    }

    #[test]
    fn parse_prs_null_author_is_unknown() {
        let prs = parse_prs(r#"[{"number":1,"author":null}]"#).expect("parse");
        assert_eq!(prs[0].author, "(unknown)");
    }

    #[test]
    fn parse_prs_malformed_json_is_error() {
        let e = parse_prs("{not json").unwrap_err();
        assert!(e.contains("JSON"), "{e}");
        assert!(e.contains("Pull Request 一覧"), "{e}");
    }

    #[test]
    fn parse_prs_empty_string_is_error() {
        let e = parse_prs("   ").unwrap_err();
        assert!(e.contains("応答が空"), "{e}");
    }

    #[test]
    fn parse_prs_object_instead_of_array_is_error() {
        let e = parse_prs(r#"{"number":1}"#).unwrap_err();
        assert!(e.contains("配列ではありません"), "{e}");
    }

    // ---- parse_issues ----

    #[test]
    fn parse_issues_real_output() {
        let issues = parse_issues(REAL_ISSUES).expect("parse");
        assert_eq!(issues.len(), 2);
        assert_eq!(issues[0].number, 13921);
        assert_eq!(issues[0].author, "daniel");
        assert_eq!(issues[0].labels, vec!["needs-triage".to_string()]);
        assert_eq!(issues[0].state, "OPEN");
        assert_eq!(issues[1].author, "scorpiomaster066-art");
        assert!(issues[1].labels.is_empty());
    }

    #[test]
    fn parse_issues_accepts_plain_string_labels() {
        let issues = parse_issues(r#"[{"number":1,"labels":["bug","p1"]}]"#).expect("parse");
        assert_eq!(issues[0].labels, vec!["bug".to_string(), "p1".to_string()]);
    }

    #[test]
    fn parse_issues_empty_and_malformed() {
        assert!(parse_issues("[]").expect("empty").is_empty());
        assert!(parse_issues("[[[").is_err());
    }

    // ---- parse_repo ----

    #[test]
    fn parse_repo_real_output() {
        let r = parse_repo(REAL_REPO).expect("parse");
        assert_eq!(r.owner, "tacyan");
        assert_eq!(r.name, "zaivern-code");
        assert_eq!(r.default_branch, "main");
        assert_eq!(r.url, "https://github.com/tacyan/zaivern-code");
        assert_eq!(r.slug(), "tacyan/zaivern-code");
    }

    #[test]
    fn parse_repo_missing_default_branch_is_tolerated() {
        let r = parse_repo(r#"{"name":"x","owner":{"login":"o"}}"#).expect("parse");
        assert_eq!(r.default_branch, "");
        assert_eq!(r.url, "");
    }

    #[test]
    fn parse_repo_without_name_is_error() {
        let e = parse_repo(r#"{"owner":{"login":"o"}}"#).unwrap_err();
        assert!(e.contains("name"), "{e}");
    }

    #[test]
    fn parse_repo_array_is_error() {
        assert!(parse_repo("[]").is_err());
        assert!(parse_repo("nope").is_err());
    }

    // ---- parse_branches ----

    #[test]
    fn parse_branches_basic() {
        let json = r#"[{"name":"main","commit":{"sha":"abc123"},"protected":true},
                       {"name":"dev","commit":{"sha":"def456"},"protected":false}]"#;
        let bs = parse_branches(json).expect("parse");
        assert_eq!(bs.len(), 2);
        assert_eq!(bs[0].name, "main");
        assert_eq!(bs[0].sha, "abc123");
        assert!(bs[0].protected);
        assert!(!bs[1].protected);
    }

    #[test]
    fn parse_branches_skips_nameless_and_allows_empty() {
        assert!(parse_branches("[]").expect("empty").is_empty());
        let bs = parse_branches(r#"[{"commit":{"sha":"x"}},{"name":"ok"}]"#).expect("parse");
        assert_eq!(bs.len(), 1);
        assert_eq!(bs[0].name, "ok");
        assert_eq!(bs[0].sha, "");
    }

    // ---- エラーメッセージ組み立て ----

    #[test]
    fn classify_failure_detects_not_authenticated() {
        let m = classify_failure(
            Some(4),
            "To get started with GitHub CLI, please run:  gh auth login\n",
        );
        assert!(m.contains("gh auth login"), "{m}");
        assert!(m.contains("ログインしていません"), "{m}");
    }

    #[test]
    fn classify_failure_detects_not_a_git_repo() {
        let m = classify_failure(Some(1), "fatal: not a git repository (or any of the parent directories): .git");
        assert!(m.contains("git リポジトリではありません"), "{m}");
        // stderr は捨てずに添える。
        assert!(m.contains("not a git repository"), "{m}");
    }

    #[test]
    fn classify_failure_detects_no_github_remote() {
        let m = classify_failure(
            Some(1),
            "none of the git remotes configured for this repository point to a known GitHub host",
        );
        assert!(m.contains("リモートが設定されていません"), "{m}");
        let m2 = classify_failure(Some(1), "could not determine base repository");
        assert!(m2.contains("リモートが設定されていません"), "{m2}");
    }

    #[test]
    fn classify_failure_detects_404_403_and_network() {
        assert!(classify_failure(Some(1), "HTTP 404: Not Found").contains("権限がありません"));
        assert!(classify_failure(Some(1), "HTTP 403: rate limit exceeded").contains("レート制限"));
        assert!(classify_failure(Some(1), "dial tcp: lookup api.github.com: no such host")
            .contains("接続できません"));
    }

    #[test]
    fn classify_failure_unknown_stderr_is_surfaced_verbatim() {
        let m = classify_failure(Some(2), "something unexpected happened");
        assert!(m.contains("something unexpected happened"), "{m}");
        assert!(m.contains("終了コード 2"), "{m}");
    }

    #[test]
    fn classify_failure_without_stderr_still_reports_code() {
        let m = classify_failure(Some(9), "   \n\n ");
        assert!(m.contains("終了コード 9"), "{m}");
        assert!(classify_failure(None, "").contains("シグナル"));
    }

    #[test]
    fn trim_stderr_joins_and_caps() {
        assert_eq!(trim_stderr("a\n\n b \nc"), "a / b / c");
        assert_eq!(trim_stderr(""), "");
        let long = "x".repeat(1000);
        let t = trim_stderr(&long);
        assert_eq!(t.chars().count(), 401); // 400 文字 + 省略記号
        assert!(t.ends_with('…'));
        // 5 行以上は 4 行で打ち切る。
        assert_eq!(trim_stderr("1\n2\n3\n4\n5"), "1 / 2 / 3 / 4");
    }

    #[test]
    fn msg_helpers() {
        assert!(msg_gh_missing().contains("cli.github.com"));
        assert_eq!(msg_not_authenticated(""), msg_not_authenticated(""));
        assert!(!msg_not_authenticated("").contains("(gh:"));
        assert!(msg_not_authenticated("detail").contains("(gh: detail)"));
    }

    // ---- リクエスト / 結果型 ----

    #[test]
    fn request_labels_and_mutating_flag() {
        let root = PathBuf::from("/tmp");
        assert_eq!(
            GhRequest::PrList {
                root: root.clone(),
                limit: 10
            }
            .label(),
            "Pull Request 一覧の取得"
        );
        assert_eq!(
            GhRequest::PrDiff {
                root: root.clone(),
                number: 42
            }
            .label(),
            "PR #42 の差分取得"
        );
        assert!(GhRequest::PrCheckout {
            root: root.clone(),
            number: 1
        }
        .is_mutating());
        assert!(!GhRequest::RepoView { root: root.clone() }.is_mutating());
        assert_eq!(GhRequest::BranchList { root: root.clone() }.root(), root);
    }

    #[test]
    fn outcome_error_text() {
        let o = GhOutcome::Error {
            req_label: "Issue 一覧の取得".into(),
            message: "boom".into(),
        };
        assert!(o.is_error());
        assert_eq!(o.error_text().unwrap(), "Issue 一覧の取得に失敗: boom");
        assert!(!GhOutcome::Prs(vec![]).is_error());
        assert!(GhOutcome::Prs(vec![]).error_text().is_none());
    }

    #[test]
    fn clamp_limit_bounds() {
        assert_eq!(clamp_limit(0), 1);
        assert_eq!(clamp_limit(30), 30);
        assert_eq!(clamp_limit(99_999), 200);
    }

    #[test]
    fn humanize_utc_formats_or_passes_through() {
        assert_eq!(humanize_utc("2026-07-19T01:35:13Z"), "2026-07-19 01:35");
        assert_eq!(humanize_utc(""), "");
        assert_eq!(humanize_utc("garbage"), "garbage");
    }

    #[test]
    fn run_blocking_rejects_missing_directory() {
        let out = run_blocking(&GhRequest::RepoView {
            root: PathBuf::from("/definitely/does/not/exist/zaivern"),
        });
        match out {
            GhOutcome::Error { req_label, message } => {
                assert_eq!(req_label, "リポジトリ情報の取得");
                // gh 未導入環境ではそちらのメッセージが先に出る。
                assert!(
                    message.contains("作業ディレクトリが見つかりません")
                        || message.contains("GitHub CLI (gh) が見つかりません"),
                    "{message}"
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }
    }
}
