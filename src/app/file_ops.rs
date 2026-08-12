use super::*;

impl ZaivernApp {
    // ─── UI: modals & toasts ────────────────────────────────────────

    pub(super) fn close_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(i) = self.pending_close else {
            return;
        };
        if i >= self.editor.buffers.len() {
            self.pending_close = None;
            return;
        }
        let title = self.editor.buffers[i].title.clone();
        let mut decided: Option<u8> = None;

        egui::Window::new(tr("未保存の変更"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(trf(
                    "「{title}」には未保存の変更があります。",
                    &[("title", title.clone())],
                ));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui.button(tr("💾 保存して閉じる")).clicked() {
                        decided = Some(0);
                    }
                    if ui.button(tr("🗑 保存せずに閉じる")).clicked() {
                        decided = Some(1);
                    }
                    if ui.button(tr("キャンセル")).clicked() {
                        decided = Some(2);
                    }
                });
            });

        match decided {
            Some(0) => {
                self.editor.active = Some(i);
                // 保存先を尋ねる (未保存タブ) 場合はダイアログが別スレッドへ回るので、
                // 「保存できたら閉じる」を控えとして預ける
                if self.save_active_with(false, true, false) {
                    self.editor.close(i);
                }
                self.pending_close = None;
                self.persist_session();
            }
            Some(1) => {
                self.editor.close(i);
                self.pending_close = None;
                self.persist_session();
            }
            Some(2) => self.pending_close = None,
            _ => {}
        }
    }

    /// リネーム/移動後、開いているバッファのパス・タイトル・言語を追従させる。
    /// `from` がフォルダの場合は配下のバッファも新パスへ付け替える。
    pub(super) fn retarget_buffers(&mut self, from: &Path, to: &Path) {
        for b in &mut self.editor.buffers {
            let Some(p) = b.path.clone() else { continue };
            let new_path = if p == from {
                to.to_path_buf()
            } else if let Ok(rest) = p.strip_prefix(from) {
                to.join(rest)
            } else {
                continue;
            };
            b.title = new_path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "???".into());
            b.lang = self.highlighter.lang_for(Some(&new_path), &b.text);
            b.path = Some(new_path);
            b.cache = None;
            b.gutter = None;
        }
    }

    // ─── ファイル操作の確認と実行 ─────────────────────────────────
    //
    // **この節の不変条件**: 復元できない fs 操作 (完全削除・置き換えのための
    // 退避) を行う関数は `perform_delete` と `replace_dest` の 2 つだけで、
    // どちらも「ユーザーが確認ダイアログで決めた」分岐からしか呼ばれない。
    // `file_tree::tests::破壊的なファイル操作は確認を経ずに呼ばれない` が
    // app.rs のソースを読んで構造で固定している。順路は必ず:
    //   delete_confirm_ui   → perform_delete
    //   transfer_confirm_ui → drain_transfer → run_transfer_item → replace_dest

    /// 削除の確認 (VS Code の「ゴミ箱に移動しますか?」/「完全に削除しますか?」)。
    /// **複数選択に対応**し、キャンセルすると 1 バイトも書き換わらない。
    pub(super) fn delete_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(req) = self.pending_delete.clone() else {
            return;
        };
        let paths = req.paths.clone();
        let Some(first) = paths.first().cloned() else {
            self.pending_delete = None;
            return;
        };
        let name = self.path_label(&first);
        // is_dir はリンク先を辿るので、種類の判定はリンク自身を見る
        let md = std::fs::symlink_metadata(&first).ok();
        let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
        let is_link = md.as_ref().is_some_and(|m| m.file_type().is_symlink());
        let warn = self.theme.warn;
        let dim = self.theme.text_dim;
        let mut decided: Option<bool> = None;

        let title = if req.permanent {
            tr("完全に削除")
        } else {
            tr("ゴミ箱へ移動")
        };
        // 一覧に出すラベルは先に作る (クロージャの中で self を借りないため)
        let listed: Vec<(String, String)> = paths
            .iter()
            .take(8)
            .map(|p| (self.rel_label(p), p.display().to_string()))
            .collect();
        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                let what = if paths.len() > 1 {
                    trf(
                        "選択中の {n} 件(フォルダは中身ごと)",
                        &[("n", paths.len().to_string())],
                    )
                } else if is_link {
                    trf("リンク「{name}」", &[("name", name.clone())])
                } else if is_dir {
                    trf("フォルダ(中身ごと)「{name}」", &[("name", name.clone())])
                } else {
                    trf("ファイル「{name}」", &[("name", name.clone())])
                };
                ui.label(if req.permanent {
                    trf("{what} を完全に削除しますか？", &[("what", what)])
                } else {
                    trf("{what} をゴミ箱へ移動しますか？", &[("what", what)])
                });
                // 何を消すのかを必ず見せる (長いパスは省略してホバーで全文)
                if paths.len() > 1 {
                    ui.add_space(4.0);
                    for (label, full) in &listed {
                        ui.add(
                            egui::Label::new(RichText::new(format!("• {label}")).small())
                                .truncate(),
                        )
                        .on_hover_text(full);
                    }
                    if paths.len() > listed.len() {
                        ui.label(
                            RichText::new(trf(
                                "ほか {n} 件",
                                &[("n", (paths.len() - listed.len()).to_string())],
                            ))
                            .small(),
                        );
                    }
                }
                ui.add_space(4.0);
                ui.label(
                    RichText::new(if req.permanent {
                        tr("この操作は取り消せません")
                    } else {
                        tr("ゴミ箱から戻せます (ツリーで取り消すと元の場所に戻ります)")
                    })
                    .small()
                    .color(if req.permanent { warn } else { dim }),
                );
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    let label = if req.permanent {
                        tr("🗑 完全に削除")
                    } else {
                        tr("🗑 ゴミ箱へ移動")
                    };
                    if ui.button(RichText::new(label).color(warn)).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("キャンセル")).clicked() {
                        decided = Some(false);
                    }
                });
            });

        match decided {
            Some(true) => {
                self.perform_delete(&paths, req.permanent);
                self.pending_delete = None;
            }
            Some(false) => self.pending_delete = None,
            _ => {}
        }
    }

    /// 削除の実体。**`delete_confirm_ui` の「はい」からしか呼ばない。**
    ///
    /// 2 系統を `delete_to_trash` / `delete_permanently` に分けてあるのは、
    /// **「戻せるもの」と「戻せないもの」を関数の境界で分ける**ため。
    /// 履歴へ積むのは前者の戻り値だけなので、完全削除が誤って
    /// 「取り消せる」ことになる余地が構造的に無い。
    pub(super) fn perform_delete(&mut self, paths: &[PathBuf], permanent: bool) {
        let one = paths.len() == 1;
        let name = paths
            .first()
            .map(|p| self.path_label(p))
            .unwrap_or_default();
        let mut ok = 0usize;
        let mut trashed: Vec<(PathBuf, PathBuf)> = Vec::new();
        let mut unrestorable = 0usize;
        let mut last_err: Option<String> = None;

        for path in paths {
            if permanent {
                match self.delete_permanently(path) {
                    Ok(()) => ok += 1,
                    Err(e) => last_err = Some(e),
                }
                continue;
            }
            match self.delete_to_trash(path) {
                Ok(Some(rf)) => {
                    ok += 1;
                    trashed.push((path.clone(), rf));
                }
                // Windows のごみ箱は中の場所を返さないので取り消せない
                Ok(None) => {
                    ok += 1;
                    unrestorable += 1;
                }
                Err(e) => last_err = Some(e),
            }
        }

        // 履歴へ積むのはゴミ箱行きだけ (完全削除は戻せないので積まない)
        if !trashed.is_empty() {
            self.push_file_op(FileOp::Trash { items: trashed });
        }
        if ok > 0 {
            let msg = if permanent {
                if one {
                    trf("🗑 {name} を削除しました", &[("name", name.clone())])
                } else {
                    trf("🗑 {n} 件を削除しました", &[("n", ok.to_string())])
                }
            } else if one {
                trf("🗑 {name} をゴミ箱へ移動しました", &[("name", name.clone())])
            } else {
                trf("🗑 {n} 件をゴミ箱へ移動しました", &[("n", ok.to_string())])
            };
            if unrestorable > 0 {
                self.toast_warn(msg);
            } else {
                self.toast(msg, true);
            }
        }
        if let Some(e) = last_err {
            self.toast(e, false);
        }
    }

    /// 1 件をゴミ箱へ送る。戻り値は「ゴミ箱の中の実体」= 取り消しで戻せる場所。
    /// **送れなければ何も消さずに理由を返す** (完全削除へは決して落ちない)。
    pub(super) fn delete_to_trash(&mut self, path: &Path) -> Result<Option<PathBuf>, String> {
        let restore_from = file_tree::trash::send(path)?;
        self.after_delete(path);
        Ok(restore_from)
    }

    /// 1 件を完全に削除する。**戻せないので履歴へは何も積まない。**
    /// シンボリックリンクはリンク自体を消す (is_dir はリンク先を辿るため、
    /// ディレクトリへのリンクを remove_dir_all に渡すと必ず失敗する)。
    pub(super) fn delete_permanently(&mut self, path: &Path) -> Result<(), String> {
        // **消すのは戻せない。** 他の担当が編集中のものは、フォルダごとでも
        // 消させない (配下まで見る)。
        if let crate::lease::Verdict::Deny(msg) = crate::lease::check_tree(path) {
            return Err(msg);
        }
        let md = std::fs::symlink_metadata(path).ok();
        let is_link = md.as_ref().is_some_and(|m| m.file_type().is_symlink());
        let is_dir = md.as_ref().is_some_and(|m| m.is_dir());
        let res = if is_link || !is_dir {
            std::fs::remove_file(path)
        } else {
            std::fs::remove_dir_all(path)
        };
        res.map_err(|e| trf("削除できません: {e}", &[("e", e.to_string())]))?;
        self.after_delete(path);
        Ok(())
    }

    /// 消えた/動いたパスの後始末: 開いていたタブを畳み、ツリーを作り直す。
    pub(super) fn after_delete(&mut self, path: &Path) {
        self.detach_buffers_under(path);
        self.tree.invalidate();
        self.tree.deselect_under(path);
        self.persist_session();
    }

    /// `path` 配下を指しているタブの始末。変更なしは閉じ、未保存の変更が
    /// あるものはパスを外して内容を保持する (⌘S で保存先を選び直せる)。
    pub(super) fn detach_buffers_under(&mut self, path: &Path) {
        let mut close: Vec<usize> = Vec::new();
        for (i, b) in self.editor.buffers.iter_mut().enumerate() {
            let Some(p) = b.path.as_ref() else { continue };
            if p == path || p.starts_with(path) {
                if b.dirty() {
                    b.path = None;
                } else {
                    close.push(i);
                }
            }
        }
        for i in close.into_iter().rev() {
            self.editor.close(i);
        }
    }

    /// 表示用のファイル名 (取れなければフルパス)。
    pub(super) fn path_label(&self, p: &Path) -> String {
        p.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| p.display().to_string())
    }

    // ─── 移動 / コピーの確認 ───────────────────────────────────────

    /// 移動/コピーの確認ダイアログ。手順は
    /// 「移動そのものの確認 → 同名衝突の確認 → 実行」の一方通行。
    pub(super) fn transfer_confirm_ui(&mut self, ctx: &egui::Context) {
        // 確認ダイアログは画面の中央に 1 枚だけ。削除の確認が出ているあいだは待つ。
        if self.pending_delete.is_some() {
            return;
        }
        let Some(q) = self.pending_transfer.as_ref() else {
            return;
        };
        // ① D&D の移動そのものの確認 (VS Code の explorer.confirmDragAndDrop)。
        //    設定でオフにできる。貼り付け由来 (from_drag=false) には出さない。
        if q.from_drag
            && q.kind == file_tree::Transfer::Move
            && !q.move_ok
            && self.cfg.confirm_drag_and_drop
        {
            self.transfer_move_confirm_ui(ctx);
            return;
        }
        // ② 確認の要らないものを進める (衝突で止まる)
        self.drain_transfer();
        // ③ 止まっていれば衝突を聞く
        let Some(q) = self.pending_transfer.as_ref() else {
            return;
        };
        if q.idx < q.items.len() {
            self.transfer_clash_confirm_ui(ctx);
        }
    }

    /// 「"X" を "Y" へ移動しますか?」。キャンセルで**ジョブごと捨てる**
    /// (fs は 1 バイトも触っていない)。
    pub(super) fn transfer_move_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(q) = self.pending_transfer.as_ref() else {
            return;
        };
        let n = q.items.len();
        let what = match q.items.first() {
            Some(it) if n == 1 => self.path_label(&it.src),
            _ => trf("{n} 件", &[("n", n.to_string())]),
        };
        let dest = q
            .items
            .first()
            .and_then(|it| it.dest.parent().map(|d| self.path_label(d)))
            .unwrap_or_default();
        let mut dont_ask = q.dont_ask;
        let mut decided: Option<bool> = None;
        let dim = self.theme.text_dim;

        egui::Window::new(tr("移動の確認"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(440.0);
                ui.label(trf(
                    "「{what}」を「{dest}」へ移動しますか？",
                    &[("what", what), ("dest", dest)],
                ));
                ui.add_space(4.0);
                ui.label(
                    RichText::new(tr(
                        "Alt (macOS は Option) を押しながらドラッグするとコピーになります",
                    ))
                    .small()
                    .color(dim),
                );
                ui.add_space(8.0);
                ui.checkbox(&mut dont_ask, tr("今後このメッセージを表示しない"));
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button(tr("➡ 移動する")).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("キャンセル")).clicked() {
                        decided = Some(false);
                    }
                });
            });

        if let Some(q) = self.pending_transfer.as_mut() {
            q.dont_ask = dont_ask;
        }
        match decided {
            Some(true) => {
                if let Some(q) = self.pending_transfer.as_mut() {
                    q.move_ok = true;
                }
                if dont_ask {
                    self.cfg.confirm_drag_and_drop = false;
                    config::save_state(&self.cfg);
                }
            }
            Some(false) => self.pending_transfer = None,
            None => {}
        }
    }

    /// 同名衝突の確認。ファイルの上書き / フォルダのマージ / 種類違いで
    /// 文言を変え、2 件以上あるときだけ「すべてに適用」を出す。
    pub(super) fn transfer_clash_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some(q) = self.pending_transfer.as_ref() else {
            return;
        };
        let Some(item) = q.items.get(q.idx).cloned() else {
            return;
        };
        let Some(clash) = item.clash else { return };
        let name = self.path_label(&item.dest);
        let dest_dir = item
            .dest
            .parent()
            .map(|d| self.path_label(d))
            .unwrap_or_default();
        // 残りに衝突がまだあるときだけ「すべてに適用」を出す(1 件では出さない)
        let rest = q.items[q.idx + 1..]
            .iter()
            .filter(|i| i.clash.is_some())
            .count();
        let mut all = q.all_checked;
        let mut decided: Option<bool> = None;
        let mut cancelled = false;
        let warn = self.theme.warn;
        let dim = self.theme.text_dim;

        let (title, question, note, yes) = match clash {
            Clash::Merge => (
                tr("フォルダの統合"),
                trf(
                    "移動先「{dir}」には同じ名前のフォルダ「{name}」が既にあります。中身を統合しますか？",
                    &[("dir", dest_dir.clone()), ("name", name.clone())],
                ),
                tr("フォルダは上書きされません。中のファイルが同名だったときだけ個別に確認します"),
                tr("📂 統合する"),
            ),
            Clash::Mismatch { dest_is_dir } => (
                tr("種類が違います"),
                trf(
                    "移動先「{dir}」には同じ名前の {kind}「{name}」が既にあります。置き換えますか？",
                    &[
                        ("dir", dest_dir.clone()),
                        (
                            "kind",
                            if dest_is_dir {
                                tr("フォルダ")
                            } else {
                                tr("ファイル")
                            },
                        ),
                        ("name", name.clone()),
                    ],
                ),
                if dest_is_dir {
                    tr("既存のフォルダは中身ごと消えます。この操作は取り消せません")
                } else {
                    tr("既存のファイルは消えます。この操作は取り消せません")
                },
                tr("⚠ 置き換える"),
            ),
            Clash::Overwrite => (
                tr("同じ名前があります"),
                trf(
                    "移動先「{dir}」には同じ名前の「{name}」が既にあります。置き換えますか？",
                    &[("dir", dest_dir.clone()), ("name", name.clone())],
                ),
                tr("既存の内容は上書きされます。この操作は取り消せません"),
                tr("⚠ 置き換える"),
            ),
        };

        egui::Window::new(title)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(460.0);
                ui.label(question);
                ui.add_space(4.0);
                ui.label(
                    RichText::new(note)
                        .small()
                        .color(if matches!(clash, Clash::Merge) {
                            dim
                        } else {
                            warn
                        }),
                );
                if rest > 0 {
                    ui.add_space(8.0);
                    ui.checkbox(
                        &mut all,
                        trf("残り {n} 件すべてに適用する", &[("n", rest.to_string())]),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal_wrapped(|ui| {
                    if ui.button(RichText::new(yes).color(warn)).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("スキップ")).clicked() {
                        decided = Some(false);
                    }
                    if ui.button(tr("キャンセル")).clicked() {
                        cancelled = true;
                    }
                });
            });

        if cancelled {
            // 残りをやめる。既に動かしたぶんは締めへ流して履歴に残す
            // (取り消し可能な状態を保つ)。まだ触っていないものは触らない。
            if let Some(q) = self.pending_transfer.as_mut() {
                q.idx = q.items.len();
            }
            self.finish_transfer();
            return;
        }
        let Some(q) = self.pending_transfer.as_mut() else {
            return;
        };
        q.all_checked = all;
        if let Some(yes) = decided {
            if all {
                q.apply_all = Some(yes);
            } else {
                q.answer = Some(yes);
            }
        }
    }

    /// 確認の要らない項目 (衝突なし / 「すべてに適用」済み / 今答えたもの) を
    /// 実行して先へ進める。答えが要るところで止まる。
    pub(super) fn drain_transfer(&mut self) {
        loop {
            let (item, answer) = {
                let Some(q) = self.pending_transfer.as_mut() else {
                    return;
                };
                if q.idx >= q.items.len() {
                    break;
                }
                let item = q.items[q.idx].clone();
                // 判定は純粋関数へ寄せる (テストで固定してある唯一の実装)
                let answer = file_tree::queue_answer(item.clash, q.answer.take(), q.apply_all);
                (item, answer)
            };
            match answer {
                None => return, // ユーザーに聞く必要がある
                Some(false) => {
                    let Some(q) = self.pending_transfer.as_mut() else {
                        return;
                    };
                    q.idx += 1;
                    q.skipped += 1;
                }
                Some(true) => {
                    // フォルダ同士は上書きではなく「中身の統合」へ展開する
                    if matches!(item.clash, Some(Clash::Merge)) {
                        self.expand_merge_item();
                        continue;
                    }
                    if let Some(q) = self.pending_transfer.as_mut() {
                        q.idx += 1;
                    }
                    self.run_transfer_item(&item);
                }
            }
        }
        self.finish_transfer();
    }

    /// 現在位置のフォルダ衝突を「1 ファイル 1 件」へ展開して差し替える。
    pub(super) fn expand_merge_item(&mut self) {
        let Some(q) = self.pending_transfer.as_ref() else {
            return;
        };
        let at = q.idx;
        let Some(item) = q.items.get(at).cloned() else {
            return;
        };
        match file_tree::expand_merge(&item.src, &item.dest, item.kind) {
            Ok(sub) => {
                let Some(q) = self.pending_transfer.as_mut() else {
                    return;
                };
                q.merge_root = Some((item.src.clone(), item.dest.clone()));
                q.items.splice(at..=at, sub);
                // 中身は 1 件ずつ答え直してもらう
                q.apply_all = None;
                q.answer = None;
                q.all_checked = false;
            }
            Err(msg) => {
                if let Some(q) = self.pending_transfer.as_mut() {
                    q.items.remove(at);
                    q.failed += 1;
                }
                self.toast_warn(msg);
            }
        }
    }

    /// 1 件を実際に動かす。**`drain_transfer` からしか呼ばない**
    /// (= 衝突があるものは確認を通ったものだけがここへ来る)。
    pub(super) fn run_transfer_item(&mut self, item: &TransferItem) {
        // 他の担当が持っているものは動かさない / 上書きしない。
        // **移動は元も消える**ので元と先の両方を見る。フォルダなら配下まで
        // (`check_tree`) — `src/` の移動は `src/app.rs` の持ち主にとって
        // 上書きより強い破壊なので、そこを素通りさせない。
        let mut guarded: Vec<&Path> = vec![item.dest.as_path()];
        if item.kind == file_tree::Transfer::Move {
            guarded.push(item.src.as_path());
        }
        for p in guarded {
            if let crate::lease::Verdict::Deny(msg) = crate::lease::check_tree(p) {
                if let Some(q) = self.pending_transfer.as_mut() {
                    q.failed += 1;
                }
                self.toast_warn(msg);
                return;
            }
        }
        if item.clash.is_some() {
            if let Err(e) = self.replace_dest(&item.dest) {
                if let Some(q) = self.pending_transfer.as_mut() {
                    q.failed += 1;
                }
                self.toast(e, false);
                return;
            }
        }
        if let Some(parent) = item.dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                if let Some(q) = self.pending_transfer.as_mut() {
                    q.failed += 1;
                }
                self.toast(trf("移動できません: {e}", &[("e", e.to_string())]), false);
                return;
            }
        }
        let res = match item.kind {
            file_tree::Transfer::Move => std::fs::rename(&item.src, &item.dest),
            file_tree::Transfer::Copy => file_tree::copy_recursively(&item.src, &item.dest),
        };
        match res {
            Ok(()) => {
                if item.kind == file_tree::Transfer::Move {
                    self.retarget_buffers(&item.src, &item.dest);
                }
                if let Some(q) = self.pending_transfer.as_mut() {
                    q.done += 1;
                    q.last = Some(item.dest.clone());
                    q.moved.push((item.src.clone(), item.dest.clone()));
                }
            }
            Err(e) => {
                if let Some(q) = self.pending_transfer.as_mut() {
                    q.failed += 1;
                }
                self.toast(trf("移動できません: {e}", &[("e", e.to_string())]), false);
            }
        }
    }

    /// 置き換えのために移動先を退ける。**`run_transfer_item` からのみ呼ぶ**
    /// (= 同名衝突をユーザーが「置き換える」で通した後だけ)。
    pub(super) fn replace_dest(&mut self, dest: &Path) -> Result<(), String> {
        // 置き換えは「消してから書く」= 消される側の持ち主に断りが要る。
        if let crate::lease::Verdict::Deny(msg) = crate::lease::check_tree(dest) {
            return Err(msg);
        }
        let Ok(md) = std::fs::symlink_metadata(dest) else {
            return Ok(()); // 既に無い
        };
        let res = if md.file_type().is_symlink() || !md.is_dir() {
            std::fs::remove_file(dest)
        } else {
            std::fs::remove_dir_all(dest)
        };
        res.map_err(|e| trf("置き換えられません: {e}", &[("e", e.to_string())]))?;
        self.detach_buffers_under(dest);
        Ok(())
    }

    /// ジョブの締め: 空になったフォルダを畳み、履歴へ積み、結果を知らせる。
    pub(super) fn finish_transfer(&mut self) {
        let Some(q) = self.pending_transfer.take() else {
            return;
        };
        if q.kind == file_tree::Transfer::Move {
            if let Some((src, dest)) = &q.merge_root {
                file_tree::prune_merged_dirs(src, dest, true);
            }
            if q.done > 0 {
                // 切り取りは移動が成功してからクリップボードを空にする
                self.tree.clear_clipboard();
                self.persist_session();
                self.push_file_op(FileOp::Move {
                    pairs: q.moved.clone(),
                    merge_root: q.merge_root.clone(),
                });
            }
        } else if let Some((src, dest)) = &q.merge_root {
            // コピーの統合: 空フォルダの階層だけ作り直す (元は触らない)
            file_tree::prune_merged_dirs(src, dest, false);
        } else if q.done > 0 {
            if let Some((_, dest)) = q.moved.first() {
                self.push_file_op(FileOp::Create {
                    path: dest.clone(),
                    is_dir: dest.is_dir(),
                });
            }
        }
        self.tree.invalidate();
        if let Some(p) = &q.last {
            self.tree.select(p);
        }
        if q.done > 0 {
            let n = q.done.to_string();
            let msg = if q.kind == file_tree::Transfer::Move {
                trf("➡ {n} 件を移動しました", &[("n", n)])
            } else {
                trf("📋 {n} 件を貼り付けました", &[("n", n)])
            };
            self.toast(msg, true);
        }
        if q.skipped > 0 {
            self.toast_warn(trf(
                "⏭ {n} 件をスキップしました",
                &[("n", q.skipped.to_string())],
            ));
        }
        if q.failed > 0 {
            self.toast(
                trf(
                    "⚠ {n} 件は動かせませんでした",
                    &[("n", q.failed.to_string())],
                ),
                false,
            );
        }
    }

    // ─── ファイル操作の取り消し (エディタ本文の履歴とは別) ───────────

    /// 履歴へ積む (上限は `FileHistory::MAX`)。
    pub(super) fn push_file_op(&mut self, op: FileOp) {
        self.file_history.push(op);
    }

    /// ツリーのメニューに出す「元に戻す: ○○」の表示名。
    pub(super) fn file_undo_hint(&self) -> Option<String> {
        self.file_history.hint()
    }

    /// ファイル操作の取り消し (ツリーがフォーカスを持つときの ⌘Z)。
    ///
    /// エディタ本文の取り消しとは**別の履歴**を戻す。ここへ来る経路は
    /// `TreeActions::undo` だけで、それが立つのは `FileTree::handle_keys` の
    /// 先頭ガードを通ったとき = ツリーがフォーカスを持ち、どの egui ウィジェットも
    /// キーボードフォーカスを持たないときだけ。
    pub(super) fn undo_file_op(&mut self) {
        let Some(op) = self.file_history.pop() else {
            self.toast_warn(tr("取り消せるファイル操作がありません"));
            return;
        };
        match self.revert_file_op(&op) {
            Ok(msg) => {
                self.tree.invalidate();
                self.persist_session();
                self.toast(msg, true);
            }
            // 戻せなかったものは履歴へ戻さない (同じ失敗を繰り返させない)。
            // このとき fs は変わっていない。
            Err(msg) => self.toast(msg, false),
        }
    }

    pub(super) fn revert_file_op(&mut self, op: &FileOp) -> Result<String, String> {
        match op {
            FileOp::Rename { from, to } => {
                self.move_back(to, from)?;
                self.tree.select(from);
                Ok(trf(
                    "↩ 名前の変更を取り消しました: {name}",
                    &[("name", self.path_label(from))],
                ))
            }
            FileOp::Move {
                pairs, merge_root, ..
            } => {
                // 後から動かしたものから戻す
                for (src, dest) in pairs.iter().rev() {
                    self.move_back(dest, src)?;
                }
                if let Some((src, dest)) = merge_root {
                    // 戻した先 (= 元の場所) の階層を作り直し、空になった
                    // 移動先フォルダを畳む
                    file_tree::prune_merged_dirs(dest, src, true);
                }
                if let Some((src, _)) = pairs.first() {
                    self.tree.select(src);
                }
                Ok(trf(
                    "↩ 移動を取り消しました ({n} 件)",
                    &[("n", pairs.len().to_string())],
                ))
            }
            FileOp::Trash { items } => {
                // 後から捨てたものから戻す
                for (original, restore_from) in items.iter().rev() {
                    self.move_back(restore_from, original)?;
                }
                if let Some((original, _)) = items.first() {
                    self.tree.select(original);
                }
                Ok(if items.len() == 1 {
                    trf(
                        "↩ ゴミ箱から戻しました: {name}",
                        &[("name", self.path_label(&items[0].0))],
                    )
                } else {
                    trf(
                        "↩ ゴミ箱から {n} 件を戻しました",
                        &[("n", items.len().to_string())],
                    )
                })
            }
            FileOp::Create { path, .. } => {
                // 作成の取り消し = 消す。**必ず復元できる形 (ゴミ箱) でだけ行う。**
                if std::fs::symlink_metadata(path).is_err() {
                    return Err(trf(
                        "取り消せません: {name} が見つかりません",
                        &[("name", self.path_label(path))],
                    ));
                }
                let restore_from = self.delete_to_trash(path)?;
                if let Some(rf) = restore_from {
                    self.push_file_op(FileOp::Trash {
                        items: vec![(path.clone(), rf)],
                    });
                }
                Ok(trf(
                    "↩ 作成を取り消しました: {name}",
                    &[("name", self.path_label(path))],
                ))
            }
        }
    }

    /// 取り消しのための移動 (実体は `file_tree::move_back`)。
    /// 動かせたときだけ開いているタブを追従させる。
    pub(super) fn move_back(&mut self, from: &Path, to: &Path) -> Result<(), String> {
        file_tree::move_back(from, to)?;
        self.retarget_buffers(from, to);
        Ok(())
    }
}
