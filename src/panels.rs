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

use eframe::egui::{self, RichText};

use crate::diff::{self, FileDiff};
use crate::github::{self, GhOutcome, GhRequest, Issue, PullRequest, RepoInfo};
use crate::ide;
use crate::palette::Cmd;
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
                title: format!("PR #{number} 差分"),
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
    // gh が無ければパネルごと無効。壊れた UI を出すより黙って説明する。
    if !github::gh_available() {
        gh_missing_ui(ui, theme);
        return;
    }
    let Some(root) = roots.get(panel.root_idx.min(roots.len().saturating_sub(1))) else {
        ui.label(RichText::new("ワークスペースが開かれていません").color(theme.text_dim));
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
            if ui.button("⟳").on_hover_text("再取得").clicked() {
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
                    "オープンな Pull Request はありません",
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
                empty_state(ui, theme, panel.inflight > 0, "オープンな Issue はありません");
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
            actions.toast = Some((format!("🐙 PR #{number} の差分を取得中…"), true));
        }
    }
}

/// gh が入っていないときの説明。責めない・慌てない文面にする。
fn gh_missing_ui(ui: &mut egui::Ui, theme: &Theme) {
    ui.add_space(6.0);
    ui.label(RichText::new("🐙 GitHub 連携は利用できません").strong());
    ui.add_space(4.0);
    ui.label(
        RichText::new("GitHub CLI (gh) が見つかりませんでした。インストールすると、この場所に Pull Request と Issue の一覧が出ます。")
            .color(theme.text_dim)
            .size(11.5),
    );
    ui.add_space(6.0);
    ui.label(RichText::new("インストール:").color(theme.text_dim).size(11.5));
    ui.label(
        RichText::new("  brew install gh   (macOS)")
            .monospace()
            .color(theme.text)
            .size(11.5),
    );
    ui.label(
        RichText::new("  https://cli.github.com")
            .monospace()
            .color(theme.text_dim)
            .size(11.5),
    );
    ui.add_space(6.0);
    ui.label(
        RichText::new("インストール後は gh auth login で認証し、Zaivern を再起動してください。")
            .color(theme.text_dim)
            .size(11.5),
    );
}

/// 一覧が空のときの表示。取得中と「本当に 0 件」を区別する。
fn empty_state(ui: &mut egui::Ui, theme: &Theme, loading: bool, msg: &str) {
    ui.add_space(8.0);
    let text = if loading { "取得中…" } else { msg };
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
                            RichText::new("差分を取得中…").color(theme.warn).size(10.5),
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
    hit.on_hover_text("クリックで差分をタブに開く").clicked()
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
                        ui.menu_button(RichText::new("⚡ 着手").size(11.0), |ui| {
                            ui.label(
                                RichText::new(
                                    "worktree を切って選んだエージェントで着手します",
                                )
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
                ui.label(RichText::new(format!("🐙 PR #{number} の差分")).strong());
                ui.label(
                    RichText::new(format!(
                        "{} ファイル · +{add} -{del} · 読み取り専用",
                        files.len()
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
                    RichText::new("この PR に差分はありません")
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
        format!("{} {} (暫定)", d.icon, d.label)
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
            format!("外部IDE: {name} で現在のファイルを開く (現在行)"),
            Cmd::OpenInIde(d.key.to_string()),
        ));
        out.push((
            "📂".to_string(),
            format!("外部IDE: {name} でワークスペースを開く"),
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
        return Err(format!("未知の IDE です: {key}"));
    };
    if folder {
        ide::launch_folder(spec, root, false)
            .map(|()| format!("{} {} でフォルダを開きました", spec.icon, spec.label))
            .map_err(|e| format!("{} を起動できませんでした: {e}", spec.label))
    } else {
        let Some(path) = file else {
            return Err("外部 IDE で開けるのは保存済みのファイルだけです".into());
        };
        let (line, col) = ide_line_col(cursor);
        ide::launch_file(spec, path, line, col)
            .map(|()| {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.display().to_string());
                format!("{} {} で {name}:{line} を開きました", spec.icon, spec.label)
            })
            .map_err(|e| format!("{} を起動できませんでした: {e}", spec.label))
    }
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
    fn panel_issues_no_request_when_gh_is_unavailable() {
        // gh_available() は環境依存なので、gh の有無で期待値を分ける。
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
        let mut panel = GithubPanel::default();
        panel.inflight = 1;
        let eff = apply_gh_outcome(&mut panel, GhOutcome::Prs(Vec::new()));
        assert!(matches!(eff, GhEffect::None));
        assert!(panel.last_error.is_none(), "空の一覧は失敗ではない");
        assert_eq!(panel.inflight, 0);
    }

    #[test]
    fn gh_error_is_recorded_and_toasted() {
        let mut panel = GithubPanel::default();
        panel.inflight = 1;
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
        let mut panel = GithubPanel::default();
        panel.pending_diff = Some(3);
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
        let mut panel = GithubPanel::default();
        panel.prs = vec![PullRequest {
            number: 1,
            ..Default::default()
        }];
        panel.prs_requested = true;
        panel.last_error = Some("boom".into());
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

    #[test]
    fn open_in_ide_rejects_unknown_key_and_unsaved_buffer() {
        let err = open_in_ide("no-such-ide", None, (1, 1), Path::new("/tmp"), false)
            .expect_err("未知のキーは失敗する");
        assert!(err.contains("no-such-ide"));

        let err = open_in_ide("cursor", None, (1, 1), Path::new("/tmp"), false)
            .expect_err("パスが無ければ開けない");
        assert!(err.contains("保存済み"));
    }
}
