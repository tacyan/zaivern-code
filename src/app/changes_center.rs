//! 🗒 変更一覧 (中央ビュー) の配線。
//!
//! **描くのは [`crate::changes_view`]、中身は控え。ここは繋ぐだけ。**
//!
//! 中身は `remote_api` が持つ控え ([`super::remote_api::changes_snapshot`]) を
//! そのまま読む。スマホの「変更一覧」と**同じ 1 本**なので、PC とスマホで
//! 件数や増減が食い違うことが起こらない (真実の在り処を 1 つに保つ)。
//! 控えの取り直しは裏スレッドなので、**この関数は 1 度も git を起こさない**
//! (git を UI スレッドで待たない、の実装)。

use super::*;

impl ZaivernApp {
    /// 変更一覧の中央ビュー。
    pub(super) fn changes_ui(&mut self, ui: &mut egui::Ui) {
        use crate::changes_view as cv;
        let theme = self.theme.clone();
        let Some(top) = self.git_ops_repo() else {
            let msg = tr("git リポジトリではありません");
            let v = cv::View {
                rows: &[],
                added: 0,
                removed: 0,
                truncated: false,
                err: Some(&msg),
                pending: false,
            };
            self.changes_draw(ui, &theme, &v);
            return;
        };
        // 控えは Arc なので、借用を跨がないようここで 1 度だけ取る。
        match super::remote_api::changes_snapshot(&top) {
            None => {
                let v = cv::View {
                    rows: &[],
                    added: 0,
                    removed: 0,
                    truncated: false,
                    err: None,
                    pending: true,
                };
                self.changes_draw(ui, &theme, &v);
            }
            Some(Err(e)) => {
                let e = e.clone();
                let v = cv::View {
                    rows: &[],
                    added: 0,
                    removed: 0,
                    truncated: false,
                    err: Some(&e),
                    pending: false,
                };
                self.changes_draw(ui, &theme, &v);
            }
            Some(Ok(snap)) => {
                let rows: Vec<cv::FileRow<'_>> = snap
                    .files
                    .iter()
                    .map(|f| cv::FileRow {
                        rel: &f.rel,
                        status: f.status,
                        added: f.added,
                        removed: f.removed,
                        binary: f.binary,
                        truncated: f.truncated,
                        hunks: &f.hunks,
                    })
                    .collect();
                let v = cv::View {
                    rows: &rows,
                    added: snap.added,
                    removed: snap.removed,
                    truncated: snap.truncated,
                    err: None,
                    pending: false,
                };
                self.changes_draw(ui, &theme, &v);
            }
        }
    }

    /// 描いて、出てきた伝票を実行する (描画中に `self` を触らないため)。
    fn changes_draw(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        v: &crate::changes_view::View<'_>,
    ) {
        use crate::changes_view as cv;
        let acts = cv::ui(&mut self.changes_state, ui, theme, v);
        for a in acts {
            match a {
                cv::Action::Open { rel, line } => {
                    // 開く先は既存の入口 (`open_path_at`)。ここで開き方を
                    // 作り直すと、タブ・エンコーディング・折り返しの扱いが
                    // 二重になる。
                    let Some(top) = self.git_ops_repo() else {
                        continue;
                    };
                    let path = top.join(&rel);
                    self.changes_state.last_open = Some((rel, line));
                    self.open_path_at(&path, line, 0);
                }
                cv::Action::OpenAll => self.open_changes_multibuffer(),
                // 控えは間隔で取り直すので、ここは「次のフレームで見に行く」だけ。
                // 自分で git を起こすと UI スレッドが待つ。
                cv::Action::Refresh => super::remote_api::changes_invalidate(),
            }
        }
    }
}
