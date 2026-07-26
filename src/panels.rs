//! GitHub サイドパネル / PR 差分ビュー / 外部 IDE 連携の描画・組み立てコード。
//!
//! app.rs を肥大化させないため、UI を描く実体はここに置く。app.rs 側は
//! 「タブを 1 つ増やして、この関数を呼ぶ」だけに留めてある。
//!
//! 設計上の要点:
//! - **gh の呼び出しは 1 つも UI スレッドで走らせない。** `github_ui` は
//!   「投げてほしいリクエスト」を `GithubActions` に積むだけで、実際の起動は
//!   app.rs が `github::run_async` で別スレッドへ回す。`gh pr list` は
//!   0.6 秒ほどかかるので、同期呼び出しにすると目に見えて画面が固まる。
//! - gh が無い環境では**パネルごと無効化**して、静かな日本語の説明だけ出す。
//!   毎フレーム失敗してトーストを撒き散らす、というのが一番やってはいけないこと。
//! - PR 一覧が空なのは**エラーではない**。空表示と失敗表示は明確に分ける。
//! - PR 差分タブは読み取り専用。差分のパース結果はバッファ id をキーに
//!   キャッシュし、毎フレーム 1000 行のパーサを回さないようにする。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use eframe::egui::{self, RichText};

use crate::agent_input::{AgentInputBuffer, ComposerTarget};
use crate::diff::{self, FileDiff};
use crate::github::{self, GhOutcome, GhRequest, Issue, PullRequest, RepoInfo};
use crate::i18n::{tr, trf};
use crate::ide;
use crate::palette::Cmd;
use crate::session_picker::{self, PastSession, SidebarAction, SidebarState};
use crate::theme::Theme;

/// 一覧の取得件数上限 (gh 側でも clamp される)。
const LIST_LIMIT: usize = 50;

/// パース済み差分キャッシュの上限。超えたら丸ごと捨てる (タブは高々数枚)。
const DIFF_CACHE_CAP: usize = 16;

// ---------------------------------------------------------------------------
// GitHub パネル
// ---------------------------------------------------------------------------

/// パネルが今どちらの一覧を出しているか。
#[derive(Default, PartialEq, Eq, Clone, Copy)]
pub enum GhTab {
    #[default]
    Prs,
    Issues,
}

/// GitHub パネルの状態。app.rs が 1 フィールドだけ持つ。
#[derive(Default)]
pub struct GithubPanel {
    /// ユーザーが明示的に GitHub 連携を有効にしたか。
    ///
    /// false の間は gh を一切起動しない (存在確認すらしない)。
    /// 起動直後にセッション復元でこのタブが選ばれていただけで
    /// gh/git が走り出すのを防ぐ番人。メニューの「GitHub」からの表示、
    /// またはパネル内の「読み込む」ボタンで true になる。
    /// セッションには保存しない = 毎起動でユーザーの明示操作が要る。
    pub active: bool,
    /// 対象にしているワークスペースルートの添字 (マルチルート対応)。
    pub root_idx: usize,
    pub tab: GhTab,
    pub repo: Option<RepoInfo>,
    pub prs: Vec<PullRequest>,
    pub issues: Vec<Issue>,
    /// 一覧を「もう投げたか」。毎フレーム gh を叩かないための番人。
    repo_requested: bool,
    prs_requested: bool,
    issues_requested: bool,
    /// 走っている gh リクエストの本数 (0 より大きければスピナーを出す)。
    inflight: usize,
    /// 直近の失敗。トーストとは別に、パネル内にも残して原因を追えるようにする。
    pub last_error: Option<String>,
    /// 差分取得中の PR 番号 (二重クリック抑止)。
    pending_diff: Option<u64>,
    /// バッファ id → パース済み差分。
    diff_cache: HashMap<u64, Vec<FileDiff>>,
}

impl GithubPanel {
    /// 取得済みの内容を捨てて、次のフレームで取り直させる。
    /// ルート切り替えと ⟳ の両方から呼ぶ。
    pub fn reset(&mut self) {
        self.repo = None;
        self.prs.clear();
        self.issues.clear();
        self.repo_requested = false;
        self.prs_requested = false;
        self.issues_requested = false;
        self.last_error = None;
    }

    /// 差分のパース結果を捨てる (同じタブへ新しい差分を流し込んだ時)。
    pub fn drop_diff_cache(&mut self, buf_id: u64) {
        self.diff_cache.remove(&buf_id);
    }
}

/// パネルから app.rs へのお願い。app.rs はこれを見て副作用を起こす。
#[derive(Default)]
pub struct GithubActions {
    /// 別スレッドで投げてほしい gh リクエスト。
    pub requests: Vec<GhRequest>,
    /// 画面に出したいメッセージ (本文, 成功なら true)。
    pub toast: Option<(String, bool)>,
    /// 「⚡ 着手」: この Issue 用の worktree を切ってエージェントを起動する
    /// (リポジトリのルート, Issue, プリセット index)。
    pub start_issue: Option<(PathBuf, Issue, usize)>,
}

/// gh の結果を受けて app.rs にやってほしいこと。
pub enum GhEffect {
    None,
    Toast(String, bool),
    /// PR 差分を非ファイルタブとして開く。
    OpenDiff {
        number: u64,
        title: String,
        text: String,
    },
}

/// ワーカースレッドから届いた `GhOutcome` をパネルへ反映する。
///
/// エラーだけを失敗として扱う。**空の一覧は成功**であり、ここでは何も起きない。
pub fn apply_gh_outcome(panel: &mut GithubPanel, out: GhOutcome) -> GhEffect {
    panel.inflight = panel.inflight.saturating_sub(1);
    match out {
        GhOutcome::Repo(r) => {
            panel.repo = Some(r);
            GhEffect::None
        }
        GhOutcome::Prs(v) => {
            panel.prs = v;
            GhEffect::None
        }
        GhOutcome::Issues(v) => {
            panel.issues = v;
            GhEffect::None
        }
        GhOutcome::Diff { number, text } => {
            panel.pending_diff = None;
            GhEffect::OpenDiff {
                number,
                title: trf("PR #{number} 差分", &[("number", number.to_string())]),
                text,
            }
        }
        GhOutcome::Checkout { number, message } => {
            GhEffect::Toast(format!("🐙 PR #{number}: {message}"), true)
        }
        GhOutcome::Branches(_) => GhEffect::None,
        err @ GhOutcome::Error { .. } => {
            panel.pending_diff = None;
            let text = err.error_text().unwrap_or_default();
            panel.last_error = Some(text.clone());
            GhEffect::Toast(format!("🐙 {text}"), false)
        }
    }
}

/// リクエストを 1 本積む (投げた本数を数えておく)。
fn request(panel: &mut GithubPanel, actions: &mut GithubActions, req: GhRequest) {
    panel.inflight += 1;
    actions.requests.push(req);
}

/// ルートの表示名 (末尾のフォルダ名)。
fn root_label(p: &Path) -> String {
    p.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// GitHub サイドパネル本体。
///
/// `roots` はワークスペースのルート一覧 (先頭が primary)。複数あるときは
/// どのルートを見るかユーザーが選べる。
pub fn github_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    panel: &mut GithubPanel,
    roots: &[PathBuf],
    presets: &[(String, String)],
    actions: &mut GithubActions,
) {
    // 明示的に有効化されるまでは gh を一切叩かない (存在確認も含む)。
    // 起動直後にセッション復元でこのタブが出ているだけの状態で
    // gh/git のプロセスが走り出すのを防ぐ。
    if !panel.active {
        gh_inactive_ui(ui, theme, panel);
        return;
    }
    // gh が無ければパネルごと無効。壊れた UI を出すより黙って説明する。
    if !github::gh_available() {
        gh_missing_ui(ui, theme);
        return;
    }
    let Some(root) = roots.get(panel.root_idx.min(roots.len().saturating_sub(1))) else {
        ui.label(RichText::new(tr("ワークスペースが開かれていません")).color(theme.text_dim));
        return;
    };
    let root = root.clone();

    // ── ヘッダ: リポジトリ名 / ルート選択 / 再取得 ──────────────────
    ui.horizontal(|ui| {
        let title = panel
            .repo
            .as_ref()
            .map(|r| format!("🐙 {}", r.slug()))
            .unwrap_or_else(|| "🐙 GitHub".to_string());
        ui.label(RichText::new(title).strong());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button("⟳").on_hover_text(tr("再取得")).clicked() {
                panel.reset();
            }
            if panel.inflight > 0 {
                ui.add(egui::Spinner::new().size(12.0));
            }
        });
    });

    if roots.len() > 1 {
        let cur = panel.root_idx.min(roots.len() - 1);
        let mut next = cur;
        egui::ComboBox::from_id_salt("zv-gh-root")
            .selected_text(root_label(&roots[cur]))
            .width(ui.available_width() - 8.0)
            .show_ui(ui, |ui| {
                for (i, r) in roots.iter().enumerate() {
                    ui.selectable_value(&mut next, i, root_label(r));
                }
            });
        if next != cur {
            panel.root_idx = next;
            panel.reset();
            return;
        }
    }

    // ── PR / Issue 切替 ─────────────────────────────────────────────
    ui.horizontal(|ui| {
        let pr_label = format!("⇄ PR ({})", panel.prs.len());
        let is_label = format!("◎ Issue ({})", panel.issues.len());
        ui.selectable_value(&mut panel.tab, GhTab::Prs, pr_label);
        ui.selectable_value(&mut panel.tab, GhTab::Issues, is_label);
    });
    ui.separator();

    // ── 必要なものだけ、まだ投げていなければ投げる ─────────────────
    if !panel.repo_requested {
        panel.repo_requested = true;
        request(panel, actions, GhRequest::RepoView { root: root.clone() });
    }
    match panel.tab {
        GhTab::Prs if !panel.prs_requested => {
            panel.prs_requested = true;
            request(
                panel,
                actions,
                GhRequest::PrList {
                    root: root.clone(),
                    limit: LIST_LIMIT,
                },
            );
        }
        GhTab::Issues if !panel.issues_requested => {
            panel.issues_requested = true;
            request(
                panel,
                actions,
                GhRequest::IssueList {
                    root: root.clone(),
                    limit: LIST_LIMIT,
                },
            );
        }
        _ => {}
    }

    if let Some(err) = panel.last_error.clone() {
        ui.label(RichText::new(format!("⚠ {err}")).color(theme.err).size(11.5));
        ui.add_space(4.0);
    }

    // ── 一覧 ────────────────────────────────────────────────────────
    let mut want_diff: Option<u64> = None;
    match panel.tab {
        GhTab::Prs => {
            if panel.prs.is_empty() {
                empty_state(
                    ui,
                    theme,
                    panel.inflight > 0,
                    &tr("オープンな Pull Request はありません"),
                );
            }
            for pr in &panel.prs {
                if pr_row(ui, theme, pr, panel.pending_diff == Some(pr.number)) {
                    want_diff = Some(pr.number);
                }
            }
        }
        GhTab::Issues => {
            if panel.issues.is_empty() {
                empty_state(ui, theme, panel.inflight > 0, &tr("オープンな Issue はありません"));
            }
            for is in &panel.issues {
                issue_row(ui, theme, is, presets, &root, actions);
            }
        }
    }

    // 借用の都合で、クリックの反映はループを抜けてから行う (app.rs と同じ流儀)。
    if let Some(number) = want_diff {
        if panel.pending_diff != Some(number) {
            panel.pending_diff = Some(number);
            request(panel, actions, GhRequest::PrDiff { root, number });
            actions.toast = Some((
                trf("🐙 PR #{number} の差分を取得中…", &[("number", number.to_string())]),
                true,
            ));
        }
    }
}

/// まだ GitHub 連携を有効化していないときの案内。
/// メニューの「GitHub」またはこのボタンを押したときだけ連携が動き出す。
fn gh_inactive_ui(ui: &mut egui::Ui, theme: &Theme, panel: &mut GithubPanel) {
    ui.add_space(6.0);
    ui.label(RichText::new(tr("🐙 GitHub 連携")).strong());
    ui.add_space(4.0);
    ui.label(
        RichText::new(tr("GitHub 連携はまだ読み込まれていません。下のボタンを押すと gh (GitHub CLI) を使って PR / Issue の一覧を取得します。"))
            .color(theme.text_dim)
            .size(11.5),
    );
    ui.add_space(8.0);
    if ui.button(tr("🐙 GitHub 連携を読み込む")).clicked() {
        panel.active = true;
    }
}

/// gh が入っていないときの説明。責めない・慌てない文面にする。
fn gh_missing_ui(ui: &mut egui::Ui, theme: &Theme) {
    ui.add_space(6.0);
    ui.label(RichText::new(tr("🐙 GitHub 連携")).strong());
    ui.add_space(4.0);
    ui.label(
        RichText::new(tr("GitHub CLI (gh) がセットアップされると、PR や Issue の一覧、Git サポート機能がフル活用できます。"))
            .color(theme.text_dim)
            .size(11.5),
    );
    ui.add_space(6.0);
    ui.label(RichText::new(tr("セットアップ手順:")).color(theme.text_dim).size(11.5));
    ui.label(
        RichText::new("  1. brew install gh   (macOS)")
            .monospace()
            .color(theme.text)
            .size(11.5),
    );
    ui.label(
        RichText::new("  2. gh auth login     (ターミナルでログイン認証)")
            .monospace()
            .color(theme.accent)
            .size(11.5),
    );
    ui.add_space(6.0);
    ui.label(
        RichText::new(tr("`gh auth login` 完了後、再取得ボタン(⟳)を押すと Pull Request や Issue が読み込まれます。"))
            .color(theme.text_dim)
            .size(11.5),
    );
}

/// 一覧が空のときの表示。取得中と「本当に 0 件」を区別する。
fn empty_state(ui: &mut egui::Ui, theme: &Theme, loading: bool, msg: &str) {
    ui.add_space(8.0);
    let text = if loading { tr("取得中…") } else { msg.to_string() };
    ui.label(RichText::new(text).color(theme.text_dim).size(11.5));
}

/// PR 1 行。クリックされたら true。
fn pr_row(ui: &mut egui::Ui, theme: &Theme, pr: &PullRequest, busy: bool) -> bool {
    let resp = egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(6.0, 5.0))
        .rounding(6.0)
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    let num = RichText::new(format!("#{}", pr.number))
                        .color(theme.accent)
                        .monospace()
                        .size(11.5);
                    ui.add(egui::Label::new(num).selectable(false));
                    if pr.is_draft {
                        ui.add(
                            egui::Label::new(
                                RichText::new("draft").color(theme.text_dim).size(10.5),
                            )
                            .selectable(false),
                        );
                    }
                    ui.add(
                        egui::Label::new(RichText::new(&pr.title).color(theme.text).size(12.0))
                            .selectable(false),
                    );
                });
                let meta = format!(
                    "{} · {} → {} · +{} -{} · {}",
                    pr.author,
                    pr.head_ref,
                    pr.base_ref,
                    pr.additions,
                    pr.deletions,
                    github::humanize_utc(&pr.updated_at)
                );
                ui.add(
                    egui::Label::new(RichText::new(meta).color(theme.text_dim).size(10.5))
                        .selectable(false),
                );
                if busy {
                    ui.add(
                        egui::Label::new(
                            RichText::new(tr("差分を取得中…")).color(theme.warn).size(10.5),
                        )
                        .selectable(false),
                    );
                }
            });
        })
        .response;
    let hit = ui.interact(
        resp.rect,
        ui.id().with(("zv-gh-pr", pr.number)),
        egui::Sense::click(),
    );
    if hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    hit.on_hover_text(tr("クリックで差分をタブに開く")).clicked()
}

/// Issue 1 行。「⚡ 着手」で worktree + エージェント起動のワンフローが始まる。
fn issue_row(
    ui: &mut egui::Ui,
    theme: &Theme,
    is: &Issue,
    presets: &[(String, String)],
    root: &Path,
    actions: &mut GithubActions,
) {
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(6.0, 5.0))
        .show(ui, |ui| {
            ui.vertical(|ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 5.0;
                    ui.add(
                        egui::Label::new(
                            RichText::new(format!("#{}", is.number))
                                .color(theme.accent)
                                .monospace()
                                .size(11.5),
                        )
                        .selectable(false),
                    );
                    ui.add(
                        egui::Label::new(RichText::new(&is.title).color(theme.text).size(12.0))
                            .selectable(false),
                    );
                    if !presets.is_empty() {
                        ui.menu_button(RichText::new(tr("⚡ 着手")).size(11.0), |ui| {
                            ui.label(
                                RichText::new(tr(
                                    "worktree を切って選んだエージェントで着手します",
                                ))
                                .size(11.0)
                                .color(theme.text_dim),
                            );
                            for (i, (icon, name)) in presets.iter().enumerate() {
                                if ui.button(format!("{icon} {name}")).clicked() {
                                    actions.start_issue =
                                        Some((root.to_path_buf(), is.clone(), i));
                                    ui.close_menu();
                                }
                            }
                        })
                        .response
                        .on_hover_text(
                            "この Issue 専用の git worktree を作成し、\n\
                             そこでエージェントを起動して着手指示を入力欄に入れます",
                        );
                    }
                });
                let labels = if is.labels.is_empty() {
                    String::new()
                } else {
                    format!(" · {}", is.labels.join(", "))
                };
                let meta = format!(
                    "{} · {}{}",
                    is.author,
                    github::humanize_utc(&is.updated_at),
                    labels
                );
                ui.add(
                    egui::Label::new(RichText::new(meta).color(theme.text_dim).size(10.5))
                        .selectable(false),
                );
            });
        });
}

// ---------------------------------------------------------------------------
// PR 差分タブ
// ---------------------------------------------------------------------------

/// PR 差分タブの中身。**読み取り専用**なので TextEdit は一切出さない。
///
/// パース結果はバッファ id をキーにキャッシュする。`diff::parse_unified` は
/// 数千行の差分を毎フレーム舐めることになるため、キャッシュ無しでは重い。
pub fn pr_diff_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    number: u64,
    buf_id: u64,
    text: &str,
    panel: &mut GithubPanel,
) {
    if panel.diff_cache.len() > DIFF_CACHE_CAP {
        panel.diff_cache.clear();
    }
    let files = panel
        .diff_cache
        .entry(buf_id)
        .or_insert_with(|| diff::parse_unified(text));

    let (add, del): (u64, u64) = files.iter().fold((0, 0), |(a, d), f| {
        (a + f.additions as u64, d + f.deletions as u64)
    });
    egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(10.0, 6.0))
        .show(ui, |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(
                    RichText::new(trf("🐙 PR #{number} の差分", &[("number", number.to_string())]))
                        .strong(),
                );
                ui.label(
                    RichText::new(trf(
                        "{n} ファイル · +{add} -{del} · 読み取り専用",
                        &[
                            ("n", files.len().to_string()),
                            ("add", add.to_string()),
                            ("del", del.to_string()),
                        ],
                    ))
                    .color(theme.text_dim)
                    .size(11.0),
                );
            });
        });

    egui::ScrollArea::vertical()
        .id_salt(("zv-pr-diff", buf_id))
        .auto_shrink(false)
        .show(ui, |ui| {
            if files.is_empty() {
                ui.add_space(8.0);
                ui.label(
                    RichText::new(tr("この PR に差分はありません"))
                        .color(theme.text_dim)
                        .size(11.5),
                );
            } else {
                diff::diff_ui(ui, theme, files);
            }
        });
}

// ---------------------------------------------------------------------------
// 外部 IDE 連携
// ---------------------------------------------------------------------------

/// 0 始まりの (行, 列) を `ide::build_open_file_args` が要求する 1 始まりへ直す。
///
/// egui の `CCursor::index` / `pcursor.row` は 0 始まりなので、そちらの値を
/// 使う場面ではこれを通す。
pub fn one_based_from_zero(line0: usize, col0: usize) -> (usize, usize) {
    (line0.saturating_add(1), col0.saturating_add(1))
}

/// `Editor::cursor` を IDE へ渡す 1 始まりの (行, 列) に正規化する。
///
/// このエディタの `Editor::cursor` は `code_editor_ui` が `line = 1` から
/// 数え上げるので**既に 1 始まり**。ただし 0 が入り込んだら 0 始まりの値が
/// 紛れたと見なして 1 に丸める — 「1 行目が開けない」より「黙って 1 行目を
/// 開く」方がマシなので。
pub fn ide_line_col(cursor: (usize, usize)) -> (usize, usize) {
    let (line, col) = cursor;
    if line == 0 && col == 0 {
        // (0, 0) は 0 始まりの原点そのもの。0 始まりの値が渡ったと見なして直す。
        return one_based_from_zero(line, col);
    }
    (line.max(1), col.max(1))
}

/// 検出済み IDE のラベル。実機検証が取れていないものは「(暫定)」と明示する。
///
/// 検証済みでない起動引数を「確実に動く」かのように見せない、が方針。
pub fn ide_label(d: &ide::DetectedIde) -> String {
    if d.confirmed && d.identity_verified {
        format!("{} {}", d.icon, d.label)
    } else {
        trf(
            "{icon} {label} (暫定)",
            &[("icon", d.icon.to_string()), ("label", d.label.to_string())],
        )
    }
}

/// コマンドパレットに出す外部 IDE の項目。
///
/// **実際に検出できた IDE だけ**を出す。検出はワーカースレッドで走るので、
/// 起動直後の数フレームは空になることがある (そのうち出てくる)。
pub fn ide_palette_entries() -> Vec<(String, String, Cmd)> {
    let Some(list) = ide::cached() else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(list.len() * 2);
    for d in &list {
        let name = ide_label(d);
        out.push((
            "↗".to_string(),
            trf(
                "外部IDE: {name} で現在のファイルを開く (現在行)",
                &[("name", name.clone())],
            ),
            Cmd::OpenInIde(d.key.to_string()),
        ));
        out.push((
            "📂".to_string(),
            trf("外部IDE: {name} でワークスペースを開く", &[("name", name)]),
            Cmd::OpenFolderInIde(d.key.to_string()),
        ));
    }
    out
}

/// 外部 IDE を起動する。成功/失敗ともユーザーに見せる日本語メッセージを返す。
///
/// `cursor` は `Editor::cursor` の値をそのまま渡してよい (中で 1 始まりへ正規化する)。
pub fn open_in_ide(
    key: &str,
    file: Option<&Path>,
    cursor: (usize, usize),
    root: &Path,
    folder: bool,
) -> Result<String, String> {
    let Some(spec) = ide::spec_by_key(key) else {
        return Err(trf("未知の IDE です: {key}", &[("key", key.to_string())]));
    };
    if folder {
        ide::launch_folder(spec, root, false)
            .map(|()| {
                trf(
                    "{icon} {label} でフォルダを開きました",
                    &[("icon", spec.icon.to_string()), ("label", spec.label.to_string())],
                )
            })
            .map_err(|e| {
                trf(
                    "{label} を起動できませんでした: {e}",
                    &[("label", spec.label.to_string()), ("e", e.to_string())],
                )
            })
    } else {
        let Some(path) = file else {
            return Err(tr("外部 IDE で開けるのは保存済みのファイルだけです"));
        };
        let (line, col) = ide_line_col(cursor);
        ide::launch_file(spec, path, line, col)
            .map(|()| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                trf(
                    "{icon} {label} で {name}:{line} を開きました",
                    &[
                        ("icon", spec.icon.to_string()),
                        ("label", spec.label.to_string()),
                        ("name", name),
                        ("line", line.to_string()),
                    ],
                )
            })
            .map_err(|e| {
                trf(
                    "{label} を起動できませんでした: {e}",
                    &[("label", spec.label.to_string()), ("e", e.to_string())],
                )
            })
    }
}

// ═══════════════════════ セッションサイドバー ═══════════════════════
//
// 形: フォルダを見出しにして、その下に過去の AI 会話を新しい順で並べる。
//
//   📁 zaivern-code                              ⋮  ＋
//    ● 👾 ガター描画のちらつきを直したい            2日
//      🚀 セッション一覧のサイドバーを作る          4日
//      すべて表示 (34)
//
// 【設計】
// - 走査は session_picker::SidebarState がバックグラウンドで回す。ここは
//   キャッシュを読んで描くだけで、フレーム内でファイルシステムに触らない。
// - 行左の点は **未読ではない**。どの保存先も既読/未読を持たないため、
//   「24 時間以内に更新された会話」を意味する (ツールチップでもそう説明する)。
// - エージェントのアイコンは agents.rs のカタログから引く (直書きしない)。

/// セッションサイドバー本体。押された操作を 1 つだけ返す。
///
/// # 配線契約 (app.rs 側)
///
/// - 置き場所: 左サイドバーの **「セッション」タブ** の中身。
///   タブを開いていないフレームでは呼ばないこと (呼ばなければ走査も走らない)。
/// - `folders`: `session_picker::sidebar_folders(&open_roots, &menu_state.folders())` の結果。
///   すなわち **開いているルートが先、その後ろに MRU (重複除去・実在するものだけ)**。
/// - [`SidebarAction::Resume`] は `session_picker::resume_command(&preset_command, &s)` で
///   コマンドを組み立ててから、通常のエージェント起動経路
///   (`agents::merged_env` → `terminal::SpawnSpec`、cwd は `s.cwd`) へ渡す。
/// - [`SidebarAction::NewConversation`] はそのフォルダを cwd にして素のプリセットを起動する。
// app.rs への配線は後続ウェーブ (app.rs は別エージェントが編集中のため触らない)。
#[allow(dead_code)]
pub fn sessions_sidebar_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut SidebarState,
    folders: &[PathBuf],
) -> SidebarAction {
    // 完了した走査の取り込みと、必要なら次の走査の起動 (どちらも UI を止めない)。
    state.refresh_if_stale(folders);

    if folders.is_empty() {
        empty_state(ui, theme, false, &tr("フォルダが開かれていません"));
        return SidebarAction::None;
    }

    let now = SystemTime::now();
    let mut action = SidebarAction::None;
    let mut scroll = egui::ScrollArea::vertical().auto_shrink([false, false]);
    if state.scroll > 0.0 {
        scroll = scroll.vertical_scroll_offset(state.scroll);
    }
    let out = scroll.show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = 2.0;
        for folder in folders {
            let act = folder_group_ui(ui, theme, state, folder, now);
            if act != SidebarAction::None && action == SidebarAction::None {
                action = act;
            }
            ui.add_space(6.0);
        }
        if state.loading() && state.is_empty() {
            empty_state(ui, theme, true, "");
        }
    });
    state.scroll = out.state.offset.y;
    // 折りたたみ / 「すべて表示」は内部状態なので呼び出し側へは返さない。
    action
}

/// フォルダ 1 つぶん (見出し + セッション行 + 「すべて表示」)。
fn folder_group_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    state: &mut SidebarState,
    folder: &Path,
    now: SystemTime,
) -> SidebarAction {
    let mut action = SidebarAction::None;
    let mut toggle_collapse = false;
    let mut toggle_show_all = false;
    let collapsed = state.is_collapsed(folder);
    let (rows, hidden) = state.visible_sessions(folder);
    let total = state.sessions_for(folder).len();

    // ── 見出し行 ──────────────────────────────────────────────
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 4.0;
        let arrow = if collapsed { "▸" } else { "▾" };
        let head = format!("{arrow} 📁 {}", root_label(folder));
        let title = ui.add(
            egui::Label::new(RichText::new(head).color(theme.text).size(12.0).strong())
                .truncate()
                .selectable(false)
                .sense(egui::Sense::click()),
        );
        if title.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        if title
            .on_hover_text(folder.display().to_string())
            .clicked()
        {
            toggle_collapse = true;
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui
                .add(egui::Button::new(RichText::new("＋").size(12.0)).frame(false))
                .on_hover_text(tr("新しい会話"))
                .clicked()
            {
                action = SidebarAction::NewConversation(folder.to_path_buf());
            }
            ui.menu_button(RichText::new("⋮").size(12.0), |ui| {
                if ui.button(tr("フォルダを表示")).clicked() {
                    action = SidebarAction::RevealFolder(folder.to_path_buf());
                    ui.close_menu();
                }
                if ui.button(tr("一覧から外す")).clicked() {
                    action = SidebarAction::CloseFolder(folder.to_path_buf());
                    ui.close_menu();
                }
                ui.separator();
                let label = if collapsed {
                    tr("展開する")
                } else {
                    tr("折りたたむ")
                };
                if ui.button(label).clicked() {
                    toggle_collapse = true;
                    ui.close_menu();
                }
            });
        });
    });

    // ── セッション行 ──────────────────────────────────────────
    if !collapsed {
        if rows.is_empty() {
            ui.indent(("zv-sess-empty", folder), |ui| {
                let msg = if state.loading() {
                    tr("読み込み中…")
                } else {
                    tr("過去の会話はまだありません")
                };
                ui.label(RichText::new(msg).color(theme.text_dim).size(10.5));
            });
        }
        for (i, s) in rows.iter().enumerate() {
            if session_row_ui(ui, theme, s, now, (folder, i)) && action == SidebarAction::None {
                action = SidebarAction::Resume(s.clone());
            }
        }
        if hidden > 0 || state.is_show_all(folder) {
            let label = if state.is_show_all(folder) {
                tr("折りたたむ")
            } else {
                trf("すべて表示 ({n})", &[("n", total.to_string())])
            };
            ui.indent(("zv-sess-more", folder), |ui| {
                if ui
                    .add(
                        egui::Button::new(RichText::new(label).color(theme.accent).size(11.0))
                            .frame(false),
                    )
                    .clicked()
                {
                    toggle_show_all = true;
                }
            });
        }
    }

    if toggle_collapse {
        state.toggle_collapsed(folder);
    }
    if toggle_show_all {
        state.toggle_show_all(folder);
    }
    action
}

/// セッション 1 行。クリックされたら true。
fn session_row_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    s: &PastSession,
    now: SystemTime,
    key: (&Path, usize),
) -> bool {
    let fresh = session_picker::is_fresh(now, s);
    let age = session_picker::relative_age(now, s.modified);
    let title = if s.summary.is_empty() {
        tr("(要約なし)")
    } else {
        s.summary.clone()
    };
    let resp = egui::Frame::none()
        .inner_margin(egui::Margin::symmetric(6.0, 3.0))
        .rounding(5.0)
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let dot = if fresh { "●" } else { " " };
                ui.add(
                    egui::Label::new(RichText::new(dot).color(theme.accent).size(8.0))
                        .selectable(false),
                );
                ui.add(
                    egui::Label::new(RichText::new(agent_mark(&s.agent_bin)).size(11.0))
                        .selectable(false),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(&age).color(theme.text_dim).size(10.5))
                            .selectable(false),
                    );
                    ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                        ui.add(
                            egui::Label::new(
                                RichText::new(&title).color(theme.text).size(11.5),
                            )
                            .truncate()
                            .selectable(false),
                        );
                    });
                });
            });
        })
        .response;
    let hit = ui.interact(
        resp.rect,
        ui.id().with(("zv-sess-row", key.0, key.1)),
        egui::Sense::click(),
    );
    if hit.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    let hover = if fresh {
        trf(
            "{title}\nクリックで再開 (● = 24 時間以内に更新)",
            &[("title", title.clone())],
        )
    } else {
        trf("{title}\nクリックで再開", &[("title", title.clone())])
    };
    hit.on_hover_text(hover).clicked()
}

/// エージェントの見分けマーク。カタログのアイコン、無ければ bin の頭文字。
fn agent_mark(bin: &str) -> String {
    match crate::agents::spec_for_bin(bin) {
        Some(spec) if !spec.icon.is_empty() => spec.icon.to_string(),
        _ => bin
            .chars()
            .next()
            .map(|c| c.to_ascii_uppercase().to_string())
            .unwrap_or_else(|| "?".to_string()),
    }
}

// ══════════════════════════════════════════════════════════════════════
//  エージェント宛て複数行コンポーザ
//
//  1 行の入力欄では、差分レビューのように改行を含む長い指示がまともに書けず、
//  しかも全員宛て (ブロードキャスト) にしか流せなかった。ここでは
//   ・Enter は改行、⌘ (macOS) / Ctrl (Windows・Linux) + Enter で送信
//   ・宛先を「このエージェント」か「全員」から選べる
//   ・宛先ごとに下書きが残る (エージェントを行き来しても消えない)
//  を満たす部品を用意する。判定はすべて純粋関数に切り出してテストする。
// ══════════════════════════════════════════════════════════════════════

/// コンポーザで押された操作。UI からもテストからも同じ経路を通すために型にする。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComposerPress {
    /// 何も押されていない
    None,
    /// 送信 (ボタン or ⌘/Ctrl+Enter)
    Send,
    /// 取消 (ボタン or Esc)。下書きは**消さない**
    Cancel,
}

/// コンポーザが呼び出し側へ返す行動。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ComposerAction {
    /// 何も起きていない
    None,
    /// 全エージェントへ一斉送信
    Send(String),
    /// セッション ID で指名した 1 体へ送信
    SendTo(u64, String),
    /// 閉じたい (下書きは残っている)
    Cancel,
}

/// このビルドが動いている OS が macOS か。
/// `cfg!` はコンパイル時に決まるので分岐コストはゼロ。判定本体
/// ([`is_send_chord`]) は引数で受け取るので 3 OS 分をテストできる。
fn on_mac() -> bool {
    cfg!(target_os = "macos")
}

/// **送信コード判定** (純粋関数)。
///
/// - macOS: `⌘ + Enter`
/// - Windows / Linux: `Ctrl + Enter`
///
/// Enter 単体は改行なので送信しない。Shift / Alt が乗っている場合も送信しない
/// (`Shift+Enter` を改行として使う指が多く、誤送信は取り返しがつかないため)。
/// macOS で `Ctrl+Enter` を送信にしないのは、ターミナル側が Ctrl 系を
/// 制御文字として使うため — OS ごとの修飾キーの役割を混ぜない。
pub fn is_send_chord(mac: bool, m: &egui::Modifiers, key: egui::Key) -> bool {
    if key != egui::Key::Enter {
        return false;
    }
    if m.shift || m.alt {
        return false;
    }
    if mac {
        m.mac_cmd && !m.ctrl
    } else {
        m.ctrl && !m.mac_cmd
    }
}

/// 送信キーの案内文 (OS で書き分ける)
pub fn send_hint(mac: bool) -> String {
    if mac {
        tr("⌘+Enter で送信 / Enter で改行")
    } else {
        tr("Ctrl+Enter で送信 / Enter で改行")
    }
}

/// 下書きの文字数と行数。
///
/// 行数は改行で割った数 — 末尾が改行なら「カーソルが乗っている次の行」も
/// 数えるので、見たままの行数と一致する。空文字は 0 行。
pub fn composer_stats(text: &str) -> (usize, usize) {
    if text.is_empty() {
        return (0, 0);
    }
    (text.chars().count(), text.split('\n').count())
}

/// 折りたたみ表示に切り替える閾値。
///
/// 複数行で、かつ長いものだけ畳む。1 行の長文はそのまま見えた方が速い。
pub fn should_collapse(text: &str) -> bool {
    let (chars, lines) = composer_stats(text);
    lines > 1 && (lines > 12 || chars > 600)
}

/// 折りたたみ中に出す 1 行サマリ。先頭の中身 + 残りの分量。
pub fn collapsed_summary(text: &str) -> String {
    let (chars, lines) = composer_stats(text);
    let head = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    // 文字境界で切る (日本語が壊れないよう chars() で数える)
    let mut shown: String = head.chars().take(40).collect();
    if head.chars().count() > 40 {
        shown.push('…');
    }
    trf(
        "{head}  — 全 {lines} 行 / {chars} 文字",
        &[
            ("head", shown),
            ("lines", lines.to_string()),
            ("chars", chars.to_string()),
        ],
    )
}

/// コンポーザの状態遷移 (純粋な部分)。
///
/// - `Send`: 下書きをスラッシュコマンド展開込みで取り出し、その宛先の下書きを空にする。
///   中身が空 (または `/clear` のように展開結果が空) なら何も送らない。
/// - `Cancel`: 下書きには**触らない** — 閉じても書きかけは残る。
pub fn composer_action(buf: &mut AgentInputBuffer, press: ComposerPress) -> ComposerAction {
    match press {
        ComposerPress::None => ComposerAction::None,
        ComposerPress::Cancel => ComposerAction::Cancel,
        ComposerPress::Send => {
            if buf.text().trim().is_empty() {
                return ComposerAction::None;
            }
            let target = buf.target();
            let out = buf.submit();
            if out.is_empty() {
                return ComposerAction::None;
            }
            match target {
                ComposerTarget::Broadcast => ComposerAction::Send(out),
                ComposerTarget::Agent(id) => ComposerAction::SendTo(id, out),
            }
        }
    }
}

/// **エージェント宛て複数行コンポーザ**。
///
/// `target` には「いま宛先にできるエージェント」を `(セッション ID, 表示名)` で渡す。
/// 渡されていればそのエージェント宛てが既定 (ユーザーが自分で全員宛てを選んだ場合は
/// その選択を尊重する)。`None` なら全員宛てのみ。
///
/// キー入力は**テキスト欄にフォーカスがあるときだけ**触る。フォーカスが無い間は
/// イベントに一切手を出さないので、ターミナル側のキーを横取りしない。
pub fn agent_composer_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    buf: &mut AgentInputBuffer,
    target: Option<(u64, &str)>,
) -> ComposerAction {
    // 宛先をアクティブなエージェントに追従させる
    buf.sync_target(target.map(|(id, _)| id));

    // 送信先が変わっても入力欄の同一性 (= フォーカス) は保つので ID は固定
    let te_id = ui.make_persistent_id("agent_composer_text");
    let focused = ui.memory(|m| m.has_focus(te_id));

    let mut press = ComposerPress::None;
    if focused {
        let mac = on_mac();
        ui.input_mut(|i| {
            // 送信コードは TextEdit に届く前に抜き取る (改行が入ってしまわないように)
            let mut hit = false;
            i.events.retain(|e| {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = e
                {
                    if is_send_chord(mac, modifiers, *key) {
                        hit = true;
                        return false;
                    }
                }
                true
            });
            if hit {
                press = ComposerPress::Send;
            } else if i.consume_key(egui::Modifiers::NONE, egui::Key::Escape) {
                press = ComposerPress::Cancel;
            }
        });
    }

    // ── 宛先セレクタ ──────────────────────────────────────────
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        ui.label(RichText::new(tr("送信先")).color(theme.text_dim).size(11.0));

        if let Some((id, label)) = target {
            let sel = buf.target() == ComposerTarget::Agent(id);
            let txt = RichText::new(format!("▸ {label}"))
                .size(11.5)
                .color(if sel { theme.accent } else { theme.text_dim });
            if ui.selectable_label(sel, txt).clicked() {
                buf.pin_broadcast(false);
                buf.set_target(ComposerTarget::Agent(id));
            }
        }

        let bsel = buf.target().is_broadcast();
        let btxt = RichText::new(tr("📢 全エージェント"))
            .size(11.5)
            .color(if bsel { theme.warn } else { theme.text_dim });
        if ui.selectable_label(bsel, btxt).clicked() {
            buf.pin_broadcast(true);
            buf.set_target(ComposerTarget::Broadcast);
        }

        let others = buf.pending_draft_count();
        if others > 0 {
            ui.label(
                RichText::new(trf("他 {n} 件の下書き", &[("n", others.to_string())]))
                    .color(theme.text_dim)
                    .size(10.5),
            );
        }
    });

    if buf.target().is_broadcast() && target.is_some() {
        // 誤爆が一番痛いので、全員宛てのときだけ明示的に注意を出す
        ui.label(
            RichText::new(tr("⚠ 起動中のすべてのエージェントへ送られます"))
                .color(theme.warn)
                .size(10.5),
        );
    }

    // ── 本文 (長い貼り付けは畳む) ─────────────────────────────
    let long = should_collapse(buf.text());
    let collapse_id = ui.make_persistent_id(("agent_composer_collapsed", buf.target()));
    let mut collapsed = long && ui.memory(|m| m.data.get_temp::<bool>(collapse_id).unwrap_or(true));

    if long {
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 6.0;
            let label = if collapsed { tr("▸ 展開") } else { tr("▾ 折りたたむ") };
            if ui.small_button(label).clicked() {
                collapsed = !collapsed;
                ui.memory_mut(|m| m.data.insert_temp(collapse_id, collapsed));
            }
            ui.label(
                RichText::new(collapsed_summary(buf.text()))
                    .color(theme.text_dim)
                    .size(11.0),
            );
        });
    }

    if !collapsed {
        let mut text = buf.text().to_string();
        let changed = egui::ScrollArea::vertical()
            .id_salt("agent_composer_scroll")
            .max_height(168.0)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.add(
                    egui::TextEdit::multiline(&mut text)
                        .id(te_id)
                        .hint_text(tr("エージェントへの指示…"))
                        .desired_rows(3)
                        .desired_width(f32::INFINITY)
                        .font(egui::FontId::proportional(13.0)),
                )
                .changed()
            })
            .inner;
        if changed {
            buf.set_text(text);
        }
    }

    // ── カウンタ + ボタン ─────────────────────────────────────
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing.x = 6.0;
        let (chars, lines) = composer_stats(buf.text());
        ui.label(
            RichText::new(trf(
                "{c} 文字 / {l} 行",
                &[("c", chars.to_string()), ("l", lines.to_string())],
            ))
            .color(theme.text_dim)
            .size(10.5),
        );
        ui.label(
            RichText::new(send_hint(on_mac()))
                .color(theme.text_dim)
                .size(10.5),
        );
        let can_send = !buf.text().trim().is_empty();
        if ui
            .add_enabled(can_send, egui::Button::new(tr("送信")).small())
            .clicked()
        {
            press = ComposerPress::Send;
        }
        if ui.small_button(tr("閉じる")).clicked() {
            press = ComposerPress::Cancel;
        }
    });

    composer_action(buf, press)
}

// ---------------------------------------------------------------------------
// 統合承認キューのパネル (engine: crate::agents::approvals)
// ---------------------------------------------------------------------------

/// キー 1 打 → 承認コマンド。**UI もテストもここだけを通る**。
///
/// 割り当ては `approvals` モジュール doc の「描画契約」に従う:
/// `Y` 承認 / `A` 同種を全部承認 / `⇧A` 常に許可 / `N` 拒否 / `⇧N` 常に拒否。
/// 他のキーは `None` — パネルは何も食べないので、下の端末へそのまま流れる。
pub fn approval_key_command(
    key: egui::Key,
    shift: bool,
) -> Option<crate::agents::approvals::Command> {
    use crate::agents::approvals::Command;
    match (key, shift) {
        (egui::Key::Y, _) => Some(Command::Approve),
        (egui::Key::A, false) => Some(Command::ApproveAllOfKind),
        (egui::Key::A, true) => Some(Command::ApproveKindForAgentAlways),
        (egui::Key::N, false) => Some(Command::Deny),
        (egui::Key::N, true) => Some(Command::DenyKindForAgentAlways),
        _ => None,
    }
}

/// 承認要求 1 件の要約行に添える経過時間 (「3 分前」)。
///
/// `now` も引数で受けるので、時計に依存せずテストできる。
pub fn approval_age_label(created_at: u64, now: u64) -> String {
    let secs = now.saturating_sub(created_at);
    if secs < 60 {
        trf("{n} 秒前", &[("n", secs.to_string())])
    } else if secs < 3600 {
        trf("{n} 分前", &[("n", (secs / 60).to_string())])
    } else {
        trf("{n} 時間前", &[("n", (secs / 3600).to_string())])
    }
}

/// 承認パネルの描画結果。app.rs が描画後にまとめて実行する。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ApprovalsOutcome {
    /// `(要求 ID, コマンド)` を順に `ApprovalQueue::apply` へ渡す。
    pub commands: Vec<(u64, crate::agents::approvals::Command)>,
    /// 監査ログを読み直してほしい (タブを開いた / 「更新」を押した)。
    pub reload_audit: bool,
}

/// **統合承認キューのパネル**。承認待ちを 1 行ずつ出し、キーとボタンで捌く。
///
/// - 引数はすべて借り物で、この関数は**何も実行しない** ([`ApprovalsOutcome`] を返すだけ)。
///   PTY への送信も config への永続化も app.rs の仕事 (承認の副作用を
///   描画コードに混ぜない = テストできる形を保つため)。
/// - 監査ログは `audit` が `Some` のときだけ描く。**読み込みはここではしない**
///   (毎フレーム I/O を避けるため、app.rs が控えを渡す)。
pub fn approvals_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    queue: &crate::agents::approvals::ApprovalQueue,
    expanded: &mut std::collections::HashSet<u64>,
    show_audit: &mut bool,
    audit: Option<&[crate::agents::approvals::AuditEntry]>,
    now_secs: u64,
) -> ApprovalsOutcome {
    use crate::agents::approvals::Command;
    let mut out = ApprovalsOutcome::default();

    ui.horizontal(|ui| {
        let n = queue.pending_len();
        ui.label(
            RichText::new(trf("🛡 承認待ち: {n} 件", &[("n", n.to_string())]))
                .strong()
                .color(if n > 0 { theme.warn } else { theme.text_dim }),
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let was = *show_audit;
            if ui
                .selectable_label(was, tr("📜 監査ログ"))
                .on_hover_text(tr(
                    "この Zaivern が下した判断の記録 (~/.zaivern/approvals.jsonl の末尾)",
                ))
                .clicked()
            {
                *show_audit = !was;
                // 開いた瞬間だけ読み直す (閉じている間は 1 バイトも読まない)
                out.reload_audit = *show_audit;
            }
            if *show_audit && ui.small_button("⟳").on_hover_text(tr("読み直す")).clicked() {
                out.reload_audit = true;
            }
        });
    });
    ui.separator();

    if *show_audit {
        audit_ui(ui, theme, audit);
        return out;
    }

    // ── キー操作は「いちばん古い 1 件」に効く (キューの順に捌ける) ──
    //
    // **どこかに文字入力のフォーカスがある間は一切触らない**。本文エディタや
    // 検索欄で「y」と打っただけで承認が飛ぶ、という事故を防ぐための歯止め。
    let typing = ui.ctx().memory(|m| m.focused().is_some());
    let head = queue.pending().next().map(|r| r.id);
    if let (Some(id), false) = (head, typing) {
        // 拾ったキーは**取り除く** (二重に効かないように)。
        let mut pressed: Vec<(egui::Key, bool)> = Vec::new();
        ui.input_mut(|i| {
            i.events.retain(|e| match e {
                egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if !modifiers.command && !modifiers.ctrl && !modifiers.alt => {
                    if approval_key_command(*key, modifiers.shift).is_some() {
                        pressed.push((*key, modifiers.shift));
                        false
                    } else {
                        true
                    }
                }
                _ => true,
            });
        });
        for (key, shift) in pressed {
            if let Some(cmd) = approval_key_command(key, shift) {
                out.commands.push((id, cmd));
            }
        }
    }

    if queue.pending_len() == 0 {
        ui.vertical_centered(|ui| {
            ui.add_space(18.0);
            ui.label(
                RichText::new(tr(
                    "承認待ちはありません — エージェントが許可を求めるとここに並びます",
                ))
                .color(theme.text_dim),
            );
        });
        return out;
    }

    ui.label(
        RichText::new(tr(
            "Y=承認 / A=この種別を全て承認 / ⇧A=このエージェントの この種別を常に許可 / N=拒否 / ⇧N=常に拒否",
        ))
        .size(11.0)
        .color(theme.text_dim),
    );
    ui.add_space(2.0);

    egui::ScrollArea::vertical()
        .id_salt("zv-approvals")
        .auto_shrink(false)
        .show(ui, |ui| {
            for (i, req) in queue.pending().enumerate() {
                let head = i == 0;
                egui::Frame::none()
                    .fill(if head { theme.panel_alt } else { theme.bg })
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        if head { theme.warn } else { theme.border },
                    ))
                    .rounding(egui::Rounding::same(6.0))
                    .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(req.kind.icon()).size(15.0));
                            ui.label(
                                RichText::new(tr(req.kind.label()))
                                    .strong()
                                    .color(theme.text),
                            );
                            ui.label(
                                RichText::new(format!("👾 {}", req.agent_bin))
                                    .size(11.0)
                                    .color(theme.text_dim),
                            );
                            ui.label(
                                RichText::new(approval_age_label(req.created_at, now_secs))
                                    .size(11.0)
                                    .color(theme.text_dim),
                            );
                            if req.never_auto {
                                ui.label(
                                    RichText::new(tr("⛔ 権限昇格 — 常に許可にはできません"))
                                        .size(11.0)
                                        .color(theme.err),
                                );
                            }
                        });
                        ui.label(RichText::new(&req.summary).color(theme.text));

                        let open = expanded.contains(&req.id);
                        if ui
                            .small_button(if open { tr("▾ 詳細") } else { tr("▸ 詳細") })
                            .clicked()
                        {
                            if open {
                                expanded.remove(&req.id);
                            } else {
                                expanded.insert(req.id);
                            }
                        }
                        if open {
                            if !req.detail.trim().is_empty() {
                                ui.label(
                                    RichText::new(&req.detail).size(11.5).color(theme.text_dim),
                                );
                            }
                            if !req.raw_prompt_excerpt.trim().is_empty() {
                                ui.label(
                                    RichText::new(tr("画面の抜粋:"))
                                        .size(11.0)
                                        .color(theme.text_dim),
                                );
                                ui.label(
                                    RichText::new(&req.raw_prompt_excerpt)
                                        .monospace()
                                        .size(11.0)
                                        .color(theme.text_dim),
                                );
                            }
                        }

                        ui.horizontal_wrapped(|ui| {
                            let mut btn = |ui: &mut egui::Ui, label: String, tip: &str, c: Command| {
                                if ui.small_button(label).on_hover_text(tip).clicked() {
                                    out.commands.push((req.id, c));
                                }
                            };
                            btn(ui, tr("✔ 承認 (Y)"), &tr("この 1 件だけ許可します"), Command::Approve);
                            btn(
                                ui,
                                tr("✔✔ 同種を全て (A)"),
                                &tr("いま待っている同じ種別をまとめて承認します"),
                                Command::ApproveAllOfKind,
                            );
                            btn(
                                ui,
                                tr("🛡 常に許可 (⇧A)"),
                                &tr(
                                    "以後このエージェントのこの種別を自動で許可します (config.toml に残ります)",
                                ),
                                Command::ApproveKindForAgentAlways,
                            );
                            btn(ui, tr("✖ 拒否 (N)"), &tr("この 1 件を断ります"), Command::Deny);
                            btn(
                                ui,
                                tr("⛔ 常に拒否 (⇧N)"),
                                &tr(
                                    "以後このエージェントのこの種別を自動で断ります (config.toml に残ります)",
                                ),
                                Command::DenyKindForAgentAlways,
                            );
                        });
                    });
                ui.add_space(4.0);
            }
        });

    out
}

/// 監査ログ (末尾から新しい順) の表示。読み込みは呼び出し側の仕事。
fn audit_ui(
    ui: &mut egui::Ui,
    theme: &Theme,
    audit: Option<&[crate::agents::approvals::AuditEntry]>,
) {
    let Some(rows) = audit else {
        ui.label(RichText::new(tr("読み込み中…")).color(theme.text_dim));
        return;
    };
    if rows.is_empty() {
        ui.vertical_centered(|ui| {
            ui.add_space(18.0);
            ui.label(
                RichText::new(tr("まだ記録がありません — 承認/拒否すると 1 行ずつ残ります"))
                    .color(theme.text_dim),
            );
        });
        return;
    }
    egui::ScrollArea::vertical()
        .id_salt("zv-approvals-audit")
        .auto_shrink(false)
        .show(ui, |ui| {
            // 末尾 = 新しい。読む人には新しい方が上のほうが早い。
            for e in rows.iter().rev() {
                ui.horizontal(|ui| {
                    let allow = e.decision.starts_with("allow");
                    ui.label(
                        RichText::new(if allow { "✔" } else { "✖" })
                            .color(if allow { theme.ok } else { theme.err }),
                    );
                    ui.label(
                        RichText::new(format!("{} / {} / {}", e.agent, e.kind, e.source))
                            .size(11.0)
                            .color(theme.text_dim),
                    );
                    ui.label(RichText::new(&e.summary).size(11.5).color(theme.text));
                });
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::editor::{BufferKind, Editor};
    use crate::github::PullRequest;

    // ── カーソルの 0 始まり → 1 始まり 変換 ───────────────────────

    #[test]
    fn zero_based_cursor_becomes_one_based() {
        // 先頭 (0,0) は 1 行 1 列であって 0 行 0 列ではない。
        assert_eq!(one_based_from_zero(0, 0), (1, 1));
        assert_eq!(one_based_from_zero(41, 7), (42, 8));
    }

    #[test]
    fn editor_cursor_is_already_one_based_and_passes_through() {
        // Editor::cursor は 1 始まりで保持されるので素通し。
        assert_eq!(ide_line_col((1, 1)), (1, 1));
        assert_eq!(ide_line_col((42, 8)), (42, 8));
    }

    #[test]
    fn zero_in_editor_cursor_is_clamped_not_wrapped() {
        // 0 が紛れ込んでも 1 に丸める (underflow も 0 行目送出もしない)。
        assert_eq!(ide_line_col((0, 0)), (1, 1));
        assert_eq!(ide_line_col((0, 5)), (1, 5));
    }

    #[test]
    fn converted_cursor_reaches_ide_args_as_one_based() {
        // 変換した値が実際に argv へ 1 始まりで載ることまで確かめる。
        let spec = ide::spec_by_key("cursor").expect("cursor spec");
        let (line, col) = one_based_from_zero(0, 0);
        let args = ide::build_open_file_args(spec, Path::new("/tmp/a.rs"), line, col);
        assert!(
            args.iter().any(|a| a.ends_with("/tmp/a.rs:1:1")),
            "args = {args:?}"
        );
    }

    // ── 差分タブは読み取り専用 ─────────────────────────────────────

    #[test]
    fn pr_diff_buffer_is_read_only_and_has_no_path() {
        let mut ed = Editor::new();
        let id = ed.open_virtual(
            "PR #7 差分".into(),
            "diff --git a/x b/x\n".into(),
            BufferKind::PrDiff { number: 7 },
        );
        let b = &ed.buffers[0];
        assert_eq!(b.id, id);
        assert!(b.kind.read_only());
        // path が None なので、保存 / LSP / git ガターのどれも対象にしない。
        assert!(b.path.is_none());
        assert!(!b.dirty());
    }

    #[test]
    fn path_dependent_paths_skip_a_diff_tab_without_panicking() {
        let mut ed = Editor::new();
        ed.open_virtual(
            "PR #7 差分".into(),
            "diff --git a/x b/x\n".into(),
            BufferKind::PrDiff { number: 7 },
        );
        // 外部変更チェック (mtime を触る) は path 無しを黙って読み飛ばす。
        assert!(ed.check_external().is_empty());
        // ディスク再読み込みも同様 (unwrap で落ちない)。
        assert!(!ed.reload_from_disk(0));
        // LSP / git ガターが使う path の取り出しは None を返すだけ。
        assert!(ed.buffers[0].path.as_deref().is_none());
        // 通常ファイルタブは従来どおり編集可。
        ed.new_untitled();
        assert!(!ed.buffers[1].kind.read_only());
    }

    #[test]
    fn reopening_same_pr_reuses_the_tab() {
        let mut ed = Editor::new();
        let a = ed.open_virtual("PR #7 差分".into(), "old".into(), BufferKind::PrDiff { number: 7 });
        let b = ed.open_virtual("PR #7 差分".into(), "new".into(), BufferKind::PrDiff { number: 7 });
        assert_eq!(a, b, "同じ PR は同じタブを使い回す");
        assert_eq!(ed.buffers.len(), 1);
        assert_eq!(ed.buffers[0].text, "new");
        // 別 PR は別タブ。
        ed.open_virtual("PR #8 差分".into(), "x".into(), BufferKind::PrDiff { number: 8 });
        assert_eq!(ed.buffers.len(), 2);
    }

    #[test]
    fn closing_a_diff_tab_keeps_the_editor_consistent() {
        let mut ed = Editor::new();
        ed.new_untitled();
        ed.open_virtual("PR #7 差分".into(), "d".into(), BufferKind::PrDiff { number: 7 });
        ed.close(1);
        assert_eq!(ed.buffers.len(), 1);
        assert_eq!(ed.active, Some(0));
    }

    // ── gh が無いときはパネルを無効化 ───────────────────────────────

    #[test]
    fn panel_never_requests_until_user_activates_it() {
        // 起動直後 (active = false) は gh の有無に関わらず一切投げない。
        let ctx = egui::Context::default();
        let mut panel = GithubPanel::default();
        let mut actions = GithubActions::default();
        let theme = crate::theme::by_name("dark");
        let roots = vec![PathBuf::from(".")];

        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                github_ui(ui, &theme, &mut panel, &roots, &[], &mut actions);
            });
        });

        assert!(actions.requests.is_empty(), "有効化前に gh を叩いてはいけない");
        assert!(panel.last_error.is_none());
    }

    #[test]
    fn panel_issues_no_request_when_gh_is_unavailable() {
        // gh_available() は環境依存なので、gh の有無で期待値を分ける。
        let ctx = egui::Context::default();
        let mut panel = GithubPanel {
            active: true, // 明示的に有効化された後の挙動を見る
            ..Default::default()
        };
        let mut actions = GithubActions::default();
        let theme = crate::theme::by_name("dark");
        let roots = vec![PathBuf::from(".")];

        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                github_ui(ui, &theme, &mut panel, &roots, &[], &mut actions);
            });
        });

        if github::gh_available() {
            // gh があるときは repo + PR 一覧を投げる (どちらも非同期)。
            assert!(!actions.requests.is_empty());
            assert!(actions
                .requests
                .iter()
                .any(|r| matches!(r, GhRequest::PrList { .. })));
        } else {
            // gh が無いときは一切投げない。説明文を出して終わり。
            assert!(actions.requests.is_empty());
            assert!(panel.last_error.is_none());
        }
    }

    #[test]
    fn missing_gh_ui_never_panics_and_asks_for_nothing() {
        let ctx = egui::Context::default();
        let theme = crate::theme::by_name("dark");
        let _ = ctx.run(Default::default(), |ctx| {
            egui::CentralPanel::default().show(ctx, |ui| {
                gh_missing_ui(ui, &theme);
            });
        });
    }

    // ── 結果の反映 ──────────────────────────────────────────────────

    #[test]
    fn empty_pr_list_is_not_an_error() {
        let mut panel = GithubPanel {
            inflight: 1,
            ..Default::default()
        };
        let eff = apply_gh_outcome(&mut panel, GhOutcome::Prs(Vec::new()));
        assert!(matches!(eff, GhEffect::None));
        assert!(panel.last_error.is_none(), "空の一覧は失敗ではない");
        assert_eq!(panel.inflight, 0);
    }

    #[test]
    fn gh_error_is_recorded_and_toasted() {
        let mut panel = GithubPanel {
            inflight: 1,
            ..Default::default()
        };
        let eff = apply_gh_outcome(
            &mut panel,
            GhOutcome::Error {
                req_label: "Pull Request 一覧の取得".into(),
                message: "not authenticated".into(),
            },
        );
        match eff {
            GhEffect::Toast(msg, ok) => {
                assert!(!ok);
                assert!(msg.contains("not authenticated"));
            }
            _ => panic!("エラーはトーストになるはず"),
        }
        assert!(panel.last_error.is_some());
    }

    #[test]
    fn diff_outcome_asks_for_a_non_file_tab() {
        let mut panel = GithubPanel {
            pending_diff: Some(3),
            ..Default::default()
        };
        let eff = apply_gh_outcome(
            &mut panel,
            GhOutcome::Diff {
                number: 3,
                text: "diff --git a/x b/x\n".into(),
            },
        );
        match eff {
            GhEffect::OpenDiff { number, title, .. } => {
                assert_eq!(number, 3);
                assert!(title.contains("#3"));
            }
            _ => panic!("差分はタブとして開くはず"),
        }
        assert!(panel.pending_diff.is_none());
    }

    #[test]
    fn reset_clears_lists_and_rearms_the_fetch() {
        let mut panel = GithubPanel {
            prs: vec![PullRequest {
                number: 1,
                ..Default::default()
            }],
            prs_requested: true,
            last_error: Some("boom".into()),
            ..Default::default()
        };
        panel.reset();
        assert!(panel.prs.is_empty());
        assert!(!panel.prs_requested);
        assert!(panel.last_error.is_none());
    }

    #[test]
    fn ide_label_marks_unverified_entries_as_best_effort() {
        let verified = ide::DetectedIde {
            key: "cursor",
            label: "Cursor",
            icon: "🖱",
            bin_path: "/x/cursor".into(),
            version: None,
            identity_verified: true,
            confirmed: true,
        };
        assert_eq!(ide_label(&verified), "🖱 Cursor");

        let guessed = ide::DetectedIde {
            identity_verified: false,
            ..verified.clone()
        };
        assert!(ide_label(&guessed).contains("暫定"));

        let unconfirmed = ide::DetectedIde {
            confirmed: false,
            ..verified.clone()
        };
        assert!(ide_label(&unconfirmed).contains("暫定"));
    }

    /// 実際に gh を叩いて PR 一覧の経路を検証する。ネットワークと gh 認証を
    /// 使うので既定では走らせない。
    ///
    /// このリポジトリ自身は PR が 0 件なので、それでは「空を返した」以上のことが
    /// 分からない。**PR が実在する公開リポジトリ (cli/cli)** を remote に持つ
    /// 一時リポジトリを作り、パースまで通ることを確かめる。
    ///
    ///   cargo test -- --ignored panels::tests::live_gh_pr_list
    #[test]
    #[ignore = "gh とネットワークが要る"]
    fn live_gh_pr_list_against_a_repo_that_has_prs() {
        use crate::test_util::unique_temp_dir;
        use std::process::Command;

        assert!(github::gh_available(), "gh が見つからない");
        let dir = unique_temp_dir("zaivern-gh-live", "prlist");
        for args in [
            vec!["init", "-q"],
            vec!["remote", "add", "origin", "https://github.com/cli/cli.git"],
        ] {
            let ok = Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .status()
                .expect("git 起動")
                .success();
            assert!(ok, "git {args:?} に失敗");
        }

        let out = github::run_blocking(&GhRequest::PrList {
            root: dir.clone(),
            limit: 5,
        });
        let prs = match out {
            GhOutcome::Prs(v) => v,
            other => panic!("PR 一覧が返らなかった: {other:?}"),
        };
        assert!(!prs.is_empty(), "cli/cli には open PR があるはず");
        for pr in &prs {
            assert!(pr.number > 0);
            assert!(!pr.title.is_empty());
            assert!(!pr.author.is_empty());
            assert!(pr.url.contains("cli/cli"));
            eprintln!(
                "#{} {} — {} ({} → {}) +{} -{}",
                pr.number, pr.title, pr.author, pr.head_ref, pr.base_ref, pr.additions, pr.deletions
            );
        }

        // 先頭の PR の差分もパースまで通す (PR 差分タブが載せる経路そのもの)。
        let n = prs[0].number;
        match github::run_blocking(&GhRequest::PrDiff {
            root: dir,
            number: n,
        }) {
            GhOutcome::Diff { number, text } => {
                assert_eq!(number, n);
                let files = diff::parse_unified(&text);
                assert!(!files.is_empty(), "差分が 1 ファイルもパースできなかった");
                eprintln!("PR #{n} の差分: {} ファイル", files.len());
            }
            other => panic!("差分が返らなかった: {other:?}"),
        }
    }

    /// 実機の IDE 検出結果を目で確かめる。各 IDE につきシェルを 1 回起動するので
    /// 既定では走らせない (環境によって結果も変わる)。
    ///
    ///   cargo test -- --ignored --nocapture panels::tests::live_ide_detection
    #[test]
    #[ignore = "実機の PATH に依存する"]
    fn live_ide_detection_reports_what_is_actually_installed() {
        ide::invalidate_cache();
        let found = ide::detect_installed();
        for d in &found {
            eprintln!(
                "{:<16} bin={:<40} verified={} confirmed={} label={}",
                d.key,
                d.bin_path,
                d.identity_verified,
                d.confirmed,
                ide_label(d)
            );
            // 起動はせず、組み立てる argv だけ確かめる (デスクトップを汚さない)。
            // カーソル (12, 5) はエディタ内部でも 1 始まりなのでそのまま 12 行目。
            let (line, col) = ide_line_col((12, 5));
            let args = ide::build_open_file_args(d.spec(), Path::new("/tmp/a.rs"), line, col);
            eprintln!("    argv: {} {}", d.spec().bin, args.join(" "));
            assert!(
                args.iter().any(|a| a.contains("12")),
                "1 始まりの行番号が argv に載っていない: {args:?}"
            );
        }
        // 検出結果はそのままパレット項目になる (1 IDE につき ファイル / フォルダ の 2 本)。
        assert_eq!(ide_palette_entries().len(), found.len() * 2);
        // 検出できなかった IDE をパレットに出さないこと。
        for (_, label, _) in ide_palette_entries() {
            assert!(
                found.iter().any(|d| label.contains(d.label)),
                "未検出の IDE が項目に混ざっている: {label}"
            );
        }
    }

    // ── セッションサイドバー ─────────────────────────────────────

    #[test]
    fn agent_mark_comes_from_catalog_not_literals() {
        // カタログに載っている bin はアイコンをそのまま使う
        for bin in ["claude", "codex", "agy"] {
            let spec = crate::agents::spec_for_bin(bin).expect("カタログに無い");
            assert_eq!(agent_mark(bin), spec.icon);
            assert!(!agent_mark(bin).is_empty());
        }
        // 3 エージェントが並んでも見分けが付く (アイコンが衝突していない)
        let marks: Vec<String> = ["claude", "codex", "agy"].iter().map(|b| agent_mark(b)).collect();
        let uniq: std::collections::HashSet<&String> = marks.iter().collect();
        assert_eq!(uniq.len(), marks.len(), "アイコンが重複している: {marks:?}");
        // 未知の bin は頭文字へフォールバック
        assert_eq!(agent_mark("zzz-unknown"), "Z");
        assert_eq!(agent_mark(""), "?");
    }

    #[test]
    fn folder_header_label_is_the_folder_name() {
        assert_eq!(root_label(Path::new("/Users/me/dev/zaivern-code")), "zaivern-code");
        assert_eq!(root_label(Path::new("/")), "/");
    }

    #[test]
    fn open_in_ide_rejects_unknown_key_and_unsaved_buffer() {
        let err = open_in_ide("no-such-ide", None, (1, 1), Path::new("/tmp"), false)
            .expect_err("未知のキーは失敗する");
        assert!(err.contains("no-such-ide"));

        let err = open_in_ide("cursor", None, (1, 1), Path::new("/tmp"), false)
            .expect_err("パスが無ければ開けない");
        assert!(err.contains("保存済み"));
    }

    // ── 複数行コンポーザ ─────────────────────────────────────

    /// 修飾キーの組み立て。`command` は egui と同じ規則
    /// (macOS では ⌘、それ以外では Ctrl と連動) で埋める。
    fn mods(mac: bool, ctrl: bool, cmd: bool, shift: bool, alt: bool) -> egui::Modifiers {
        egui::Modifiers {
            alt,
            ctrl,
            shift,
            mac_cmd: mac && cmd,
            command: if mac { mac && cmd } else { ctrl },
        }
    }

    #[test]
    fn send_chord_is_cmd_enter_on_mac_and_ctrl_enter_elsewhere() {
        // (mac, ctrl, cmd, shift, alt, key, 送信するか)
        let cases: &[(bool, bool, bool, bool, bool, egui::Key, bool)] = &[
            // --- macOS: ⌘+Enter だけが送信 ---
            (true, false, true, false, false, egui::Key::Enter, true),
            (true, false, false, false, false, egui::Key::Enter, false), // Enter 単体 = 改行
            (true, true, false, false, false, egui::Key::Enter, false), // Ctrl は端末側の役目
            (true, false, true, true, false, egui::Key::Enter, false),  // ⌘+Shift は誤爆防止で無効
            (true, false, true, false, true, egui::Key::Enter, false),  // ⌘+Alt も無効
            (true, true, true, false, false, egui::Key::Enter, false),  // Ctrl 同時押しは無効
            (true, false, true, false, false, egui::Key::A, false),     // Enter 以外は無関係
            // --- Windows / Linux: Ctrl+Enter だけが送信 ---
            (false, true, false, false, false, egui::Key::Enter, true),
            (false, false, false, false, false, egui::Key::Enter, false), // Enter 単体 = 改行
            (false, true, false, true, false, egui::Key::Enter, false),   // Ctrl+Shift は無効
            (false, true, false, false, true, egui::Key::Enter, false),   // Ctrl+Alt は無効
            (false, false, false, true, false, egui::Key::Enter, false),  // Shift+Enter = 改行
            (false, true, false, false, false, egui::Key::Escape, false),
        ];
        for &(mac, ctrl, cmd, shift, alt, key, want) in cases {
            let m = mods(mac, ctrl, cmd, shift, alt);
            assert_eq!(
                is_send_chord(mac, &m, key),
                want,
                "mac={mac} ctrl={ctrl} cmd={cmd} shift={shift} alt={alt} key={key:?}"
            );
        }
    }

    #[test]
    fn send_hint_names_the_key_of_the_running_os() {
        assert!(send_hint(true).contains('⌘'));
        assert!(send_hint(false).contains("Ctrl"));
        // どちらの OS でも「Enter は改行」だと分かる
        assert!(send_hint(true).contains("改行") && send_hint(false).contains("改行"));
    }

    #[test]
    fn composer_stats_counts_visible_lines_and_chars() {
        assert_eq!(composer_stats(""), (0, 0));
        assert_eq!(composer_stats("あいう"), (3, 1));
        assert_eq!(composer_stats("あ\nい"), (3, 2));
        // 末尾の改行はカーソルが乗る行として数える (見たままに合わせる)
        assert_eq!(composer_stats("あ\n"), (2, 2));
    }

    #[test]
    fn only_long_multiline_drafts_collapse() {
        assert!(!should_collapse(""));
        assert!(!should_collapse(&"あ".repeat(900)), "1 行なら長くても畳まない");
        assert!(!should_collapse("1\n2\n3"), "数行なら畳まない");
        assert!(should_collapse(&"行\n".repeat(20)), "行数が多ければ畳む");
        let wide = format!("見出し\n{}", "あ".repeat(700));
        assert!(should_collapse(&wide), "複数行かつ長文なら畳む");
    }

    #[test]
    fn collapsed_summary_shows_head_and_size_without_breaking_japanese() {
        let text = format!("レビュー対応のお願い\n{}", "あ".repeat(700));
        let s = collapsed_summary(&text);
        assert!(s.contains("レビュー対応のお願い"));
        assert!(s.contains("2 行"), "全体の行数が出る: {s}");
        assert!(s.contains("711 文字"), "全体の文字数が出る: {s}");

        // 先頭行が長いときは 40 文字で省略 (バイト境界で割らない)
        let long_head = format!("{}\nx", "漢".repeat(80));
        let s2 = collapsed_summary(&long_head);
        assert!(s2.contains('…'));
        assert!(s2.starts_with(&"漢".repeat(40)));

        // 空行から始まっていても最初の中身を拾う
        assert!(collapsed_summary("\n\n本文です\nx").contains("本文です"));
    }

    #[test]
    fn composer_send_returns_text_and_clears_only_that_draft() {
        let mut b = AgentInputBuffer::new();
        b.set_target(ComposerTarget::Broadcast);
        b.set_text("全員へ: 進捗を教えて");
        b.set_target(ComposerTarget::Agent(7));
        b.set_text("7 番へ: この関数を直して");

        // 何も押さなければ何も起きない
        assert_eq!(composer_action(&mut b, ComposerPress::None), ComposerAction::None);
        assert_eq!(b.text(), "7 番へ: この関数を直して");

        // 送信 → 宛先つきで返り、その宛先の下書きだけ空になる
        assert_eq!(
            composer_action(&mut b, ComposerPress::Send),
            ComposerAction::SendTo(7, "7 番へ: この関数を直して".to_string())
        );
        assert_eq!(b.text(), "");
        assert_eq!(
            b.draft_for(ComposerTarget::Broadcast),
            "全員へ: 進捗を教えて",
            "他の宛先の下書きは巻き添えにならない"
        );

        // 全員宛てに切り替えて送ると Send になる
        b.set_target(ComposerTarget::Broadcast);
        assert_eq!(
            composer_action(&mut b, ComposerPress::Send),
            ComposerAction::Send("全員へ: 進捗を教えて".to_string())
        );
        assert_eq!(b.text(), "");
    }

    #[test]
    fn composer_cancel_keeps_the_draft() {
        let mut b = AgentInputBuffer::new();
        b.set_target(ComposerTarget::Agent(3));
        b.set_text("書きかけ\n途中まで");

        assert_eq!(composer_action(&mut b, ComposerPress::Cancel), ComposerAction::Cancel);
        assert_eq!(b.text(), "書きかけ\n途中まで", "取消では下書きを消さない");

        // 閉じて別のエージェントを触ってから戻ってきても残っている
        b.set_target(ComposerTarget::Agent(4));
        b.set_text("別件");
        b.set_target(ComposerTarget::Agent(3));
        assert_eq!(b.text(), "書きかけ\n途中まで");
    }

    #[test]
    fn composer_send_ignores_blank_and_swallowing_slash_commands() {
        let mut b = AgentInputBuffer::new();
        b.set_text("   \n  ");
        assert_eq!(composer_action(&mut b, ComposerPress::Send), ComposerAction::None);

        // /clear は展開結果が空なので送らない (下書きだけ消える)
        b.set_text("/clear");
        assert_eq!(composer_action(&mut b, ComposerPress::Send), ComposerAction::None);
        assert_eq!(b.text(), "");
    }

    #[test]
    fn composer_send_round_trips_multiline_japanese_with_trailing_newline() {
        let src = "以下のレビューコメントに対応してください:\n\n@src/app.rs:42\n> 境界値がずれています\n直してテストも足してください。\n";
        let mut b = AgentInputBuffer::new();
        // レビュー対象のエージェント宛てに置く → その宛先へそのまま飛ぶ
        b.set_draft_for(ComposerTarget::Agent(11), src);
        b.set_target(ComposerTarget::Agent(11));
        assert_eq!(
            composer_action(&mut b, ComposerPress::Send),
            ComposerAction::SendTo(11, src.to_string()),
            "改行も末尾の 1 行も 1 文字違わず届く"
        );
    }

    // ── 統合承認キューのパネル ─────────────────────────────────

    /// 経過時間の表示は 秒 → 分 → 時 で切り替わる (時計に依存しない)。
    #[test]
    fn 承認要求の経過時間は単位が切り替わる() {
        // tr/trf は辞書が無ければ原文のままなので、数字と単位だけ見る
        assert!(approval_age_label(100, 130).contains("30"));
        assert!(approval_age_label(100, 130).contains("秒"));
        assert!(approval_age_label(0, 90).contains("分"));
        assert!(approval_age_label(0, 7200).contains("時間"));
        // 時計が巻き戻っても負にならない (0 秒前)
        assert!(approval_age_label(500, 100).contains("0"));
    }

    /// パネルは自分では何も実行しない — 依頼を積んで返すだけ。
    /// (PTY への送信も config への保存も app.rs 側の仕事)
    #[test]
    fn 承認パネルは副作用を持たない() {
        let src = include_str!("panels.rs");
        let body = src.split("pub fn approvals_ui(").nth(1).expect("パネルがある");
        let head = &body[..body.find("\n/// ").unwrap_or(body.len())];
        for forbidden in ["std::fs::", "send_text", "read_audit_tail", "save_state"] {
            assert!(
                !head.contains(forbidden),
                "描画関数が {forbidden} を直接呼んでいる (毎フレーム副作用になる)"
            );
        }
        assert!(head.contains("ApprovalsOutcome"), "依頼を返していない");
    }

    /// 文字入力中は承認キーを**一切**拾わない。
    /// エディタで「y」と打っただけで承認が飛ぶ、という事故の歯止め。
    #[test]
    fn 入力中は承認キーを拾わない() {
        let src = include_str!("panels.rs");
        let body = src.split("pub fn approvals_ui(").nth(1).expect("パネルがある");
        let head = &body[..body.find("\n/// ").unwrap_or(body.len())];
        assert!(
            head.contains("let typing = ui.ctx().memory(|m| m.focused().is_some());"),
            "フォーカスの有無を見ていない"
        );
        assert!(
            head.contains("if let (Some(id), false) = (head, typing)"),
            "入力中でも拾ってしまう"
        );
        // 拾ったキーは取り除く (下の層で二重に効かない)
        assert!(head.contains("i.events.retain("), "キーを消費していない");
    }
}
