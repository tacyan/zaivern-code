use super::*;

// ─── VS Code 準拠メニューバーの実処理 ──────────────────────────────
//
// メニュー UI (menu_bar.rs) が返す Cmd の実体。ここにまとめて置くのは
// 並行セッションとの diff 衝突を app.rs 本体の impl から分離するため。

impl ZaivernApp {
    /// メニューバー描画用の表示状態スナップショットを作る。
    pub(super) fn build_menu_info(&self, ctx: &egui::Context) -> menu_bar::MenuInfo {
        let active = self.editor.active.map(|i| &self.editor.buffers[i]);
        let active_path = active.and_then(|b| b.path.clone());
        let run_label = active_path.as_ref().and_then(|p| {
            let root = self
                .tree
                .root_for(p)
                .map(|r| r.to_path_buf())
                .unwrap_or_else(|| self.primary_root().to_path_buf());
            menu_bar::runner_for(p, &root).map(|_| {
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                trf("{name} を実行", &[("name", name)])
            })
        });
        let themes = self.theme_entries();
        let plugin_commands: Vec<(usize, usize, String, String)> = self
            .plugins
            .iter()
            .enumerate()
            .flat_map(|(pi, p)| {
                p.commands.iter().enumerate().map(move |(ci, c)| {
                    (pi, ci, c.icon.clone(), format!("{}: {}", p.name, c.title))
                })
            })
            .take(40)
            .collect();
        // tasks.json 由来のタスク。走らせられない理由はここで確定させ、
        // メニュー側はグレーアウトとホバーに使うだけにする。
        let json_tasks =
            menu_bar::task_rows(&self.tasks_cache.doc, active_path.as_deref(), cfg!(windows));
        let build_target = self.build_target();
        menu_bar::MenuInfo {
            sidebar_open: self.sidebar_open,
            terminal_open: self.agents.panel_open,
            cockpit_open: self.cockpit,
            kanban_open: self.kanban,
            deck_open: self.deck,
            problems_open: self.problems_open,
            fullscreen: ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
                || self.fake_fullscreen.is_some(),
            auto_save: self.menu_state.auto_save,
            word_wrap: self.cfg.word_wrap,
            show_whitespace: self.cfg.show_whitespace,
            minimap: self.cfg.minimap,
            breadcrumbs: self.cfg.breadcrumbs,
            git_blame: self.cfg.git_blame,
            has_editor: self.editor.active.is_some(),
            editor_split: self.panes.is_split(),
            has_file: active_path.is_some(),
            md_preview: self.md_preview,
            roots: self.roots.clone(),
            recent_folders: self.menu_state.folders(),
            recent_files: self.menu_state.files(),
            plugin_commands,
            agent_presets: self
                .cfg
                .agents
                .iter()
                .enumerate()
                .map(|(i, p)| (i, p.icon.clone(), p.name.clone()))
                .collect(),
            themes,
            line_ending: self
                .editor
                .active
                .map(|_| self.active_line_ending().label()),
            ui_zoom: self.cfg.ui_zoom,
            file_zoom: self
                .editor
                .active
                .map(|i| zoom::clamp(self.editor.buffers[i].zoom)),
            text_scale: zoom::clamp(self.cfg.text_scale),
            trim_trailing_on_save: self.save_trim_trailing,
            trim_final_newlines_on_save: self.save_trim_final_newlines,
            final_newline_on_save: self.save_final_newline,
            build_task: build_target.as_ref().map(|(l, ..)| l.clone()),
            build_from_tasks_json: build_target.as_ref().is_some_and(|t| t.4),
            detected_task: menu_bar::build_task_for(&self.agent_cwd()).map(|(l, _)| l),
            json_tasks,
            tasks_error: self.tasks_cache.doc.error.clone(),
            run_label,
        }
    }

    pub(super) fn touch_recent_file(&mut self, p: &Path) {
        self.menu_state.touch_file(p);
        recent::save(&self.menu_state);
    }

    /// 「このファイルは他の担当が持っている」帯。**持てているときは 1 ピクセルも
    /// 描かない** (UI 原則: 空白は作らない / 常に 0 を表示するバッジを作らない)。
    ///
    /// 保存が止まってから理由を知るのでは遅い — 編集を始めた時点で出す。
    pub(super) fn lease_banner_ui(&mut self, ui: &mut egui::Ui) {
        let Some(path) = self
            .editor
            .active
            .and_then(|i| self.editor.buffers.get(i))
            .and_then(|b| b.path.clone())
        else {
            return;
        };
        let crate::lease::Own::Taken { owner, .. } = crate::lease::own_of(&path) else {
            return;
        };
        let warn = self.theme.warn;
        let avail = ui.available_width();
        egui::Frame::none()
            .fill(warn.gamma_multiply(0.18))
            .inner_margin(egui::Margin::symmetric(8.0, 4.0))
            .show(ui, |ui| {
                ui.set_width(avail - 16.0);
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("🔒").color(warn));
                    // 狭い幅でも見切れないよう、名前は縮めてホバーで全文を出す。
                    let text = trf(
                        "{owner} が編集中です — このまま保存すると止まります",
                        &[("owner", owner.clone())],
                    );
                    let room = ((avail - 140.0) / 7.0).max(8.0) as usize;
                    ui.label(egui::RichText::new(crate::lease::ellipsize(&text, room)).color(warn))
                        .on_hover_text(&text);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(tr("所有一覧")).clicked() {
                            crate::lease::open_panel();
                        }
                    });
                });
            });
    }

    /// 編集中のファイル所有を台帳と揃える (毎フレーム)。
    ///
    /// **「編集を始めた / 終えた」を対で呼ぶ形にしない**のが要点。対の片側を
    /// 呼び忘れた経路が 1 つでもあると所有が漏れ続ける (タブを閉じた・元に
    /// 戻した・ワークスペースを切り替えた…)。**いま汚れているバッファの集合**を
    /// 丸ごと渡し、消えたぶんの解放は [`crate::lease::sync_edits`] に任せる。
    ///
    /// ガードが無効なスコープでは [`crate::lease::armed`] が `false` を返して
    /// 即座に抜ける = 単独で使う人のアイドルコストはゼロ (設計原則 3)。
    pub(super) fn sync_lease_ownership(&mut self) {
        // **ここが唯一の張り替え地点。** かつては `apply_roots` に置いていたが、
        // あれは「フォルダを開き直した」ときにしか通らないので、**通常起動では
        // 一度も有効にならなかった** (他リポジトリでの実測で発覚: 3/3 で台帳が
        // 空のままだった)。毎フレームの比較 1 回に置き換えて、起動・切り替え・
        // セッション復元のどの経路でも必ず張られるようにする。
        let root = self.primary_root().to_path_buf();
        if self.lease_armed_for.as_deref() != Some(root.as_path()) {
            // 前のワークスペースの所有は返してから移る (別リポジトリの台帳へ
            // 解放を撃たないため、順序が大事)。
            crate::lease::release_all();
            crate::lease::arm(
                &root,
                self.cfg.feature_bool("lease.auto_arm"),
                self.cfg.feature_i64("lease.ttl_minutes"),
            );
            self.lease_armed_for = Some(root);
            self.lease_armed_notified = false;
        }
        if !crate::lease::armed() {
            return;
        }
        let editing: Vec<PathBuf> = self
            .editor
            .buffers
            .iter()
            .filter(|b| !b.kind.read_only() && b.dirty())
            .filter_map(|b| b.path.clone())
            .collect();
        crate::lease::sync_edits(&editing);
        // 段が決まったら 1 度だけ知らせる。**「強制」と「勧告」を区別して出す** —
        // 効いていると思わせて実は警告だけ、が最悪なので (lease.rs の設計方針)。
        if !self.lease_armed_notified {
            let t = crate::lease::tier_now();
            if t != crate::lease::Tier::Off {
                self.lease_armed_notified = true;
                let msg = trf(
                    "🔐 ファイル所有ガード: {tier} — {detail}\n　ブロックできるエージェント: {who}",
                    &[
                        ("tier", tr(t.label())),
                        ("detail", tr(t.detail())),
                        ("who", crate::lease::gated_agents().join(", ")),
                    ],
                );
                if t == crate::lease::Tier::Enforced {
                    self.toast(msg, true);
                } else {
                    self.toast_warn(msg);
                }
            }
        }
        // 確保の答えが返ったものだけ知らせる。**取れた**ことは黙っている
        // (毎回出すと通知だけで画面が埋まる)。取れなかったときは、編集を
        // 続ける前に必ず気付いてほしいので警告で出す。
        for n in crate::lease::pump() {
            if let crate::lease::Own::Taken { reason, .. } = n.own {
                self.toast_warn(reason);
            }
        }
    }

    /// 開いている全タブを保存 (VS Code: すべて保存)。無題タブは対象外。
    pub(super) fn save_all(&mut self, ctx: &egui::Context) {
        let mut saved = 0usize;
        let mut untitled = 0usize;
        let mut hooked: Vec<usize> = Vec::new();
        for i in 0..self.editor.buffers.len() {
            let b = &self.editor.buffers[i];
            if b.kind.read_only() || !b.dirty() {
                continue;
            }
            let Some(path) = b.path.clone() else {
                untitled += 1;
                continue;
            };
            if self.editor.buffers[i].write_to(&path).is_ok() {
                let b = &mut self.editor.buffers[i];
                b.mark_saved();
                b.disk_mtime = disk_mtime(&path);
                b.conflict_notified = None;
                saved += 1;
                hooked.push(i);
            }
        }
        for i in hooked {
            self.run_on_save_hooks(i, ctx);
        }
        if saved > 0 {
            self.persist_session();
            // 保存した本文の退避はもう要らない (ゴミを残さない)
            self.hotexit_flush();
            self.toast(
                trf(
                    "💾 {n} 件のファイルを保存しました",
                    &[("n", saved.to_string())],
                ),
                true,
            );
        }
        if untitled > 0 {
            self.toast(
                tr("無題のタブは「名前を付けて保存」で保存してください"),
                false,
            );
        }
    }

    /// 自動保存 (VS Code: afterDelay 相当)。2 秒ごとに、ファイルに紐付く
    /// 未保存バッファを黙って書き出す。on_save フック (整形等) は入力中の
    /// 割り込みになるため走らせない。
    pub(super) fn autosave_tick(&mut self) {
        if !self.menu_state.auto_save {
            return;
        }
        if self
            .autosave_at
            .map(|t| (t.elapsed().as_millis() as u64) < AUTOSAVE_MS)
            .unwrap_or(false)
        {
            return;
        }
        self.autosave_at = Some(Instant::now());
        let mut saved_any = false;
        for b in &mut self.editor.buffers {
            if b.kind.read_only() || !b.dirty() {
                continue;
            }
            let Some(path) = b.path.clone() else { continue };
            if b.write_to(&path).is_ok() {
                b.mark_saved();
                b.disk_mtime = disk_mtime(&path);
                b.conflict_notified = None;
                saved_any = true;
            }
        }
        if saved_any {
            self.persist_session();
        }
    }

    /// アクティブなファイルをディスクの内容へ戻す (未保存の編集は破棄)。
    pub(super) fn revert_active(&mut self) {
        let Some(i) = self.editor.active else { return };
        let b = &mut self.editor.buffers[i];
        let Some(path) = b.path.clone() else {
            self.toast(tr("ファイルに紐付いていないタブは元に戻せません"), false);
            return;
        };
        // 開くときと同じ経路で読む。UTF-8 決め打ちだと CP932 のファイルだけ
        // 「読み直せませんでした」になり、元に戻す操作そのものが使えなくなる。
        let Ok(raw) = std::fs::read(&path) else {
            self.toast(tr("ディスクから読み直せませんでした"), false);
            return;
        };
        let (text, encoding) = crate::textenc::decode_bytes(&raw);
        if text == b.text {
            self.toast(tr("ディスクの内容と同じです"), true);
            return;
        }
        // ディスク側の符号化に合わせ直す (次の保存で元の形へ書き戻せるように)
        b.encoding = encoding;
        // ディスクへ戻す = 未保存の編集を捨てる操作なので履歴も畳む
        b.reset_text(text);
        b.disk_mtime = disk_mtime(&path);
        b.conflict_notified = None;
        let title = b.title.clone();
        self.queue_lsp_change(i);
        self.toast(
            trf(
                "↺ {title} をディスクの内容に戻しました",
                &[("title", title)],
            ),
            true,
        );
    }

    /// すべてのエディタタブを閉じる。未保存タブは確認ダイアログに回す。
    /// **ピン留めしたタブは残す** — 「誤って閉じない」がピン留めの意味なので、
    /// 一括操作こそ効かせてはいけない (VS Code と同じ)。
    pub(super) fn close_all_tabs(&mut self) {
        let mut kept_dirty = 0usize;
        let pinned = self.panes.pinned_bufs();
        let mut kept_pinned = 0usize;
        for i in (0..self.editor.buffers.len()).rev() {
            if pinned.contains(&self.editor.buffers[i].id) {
                kept_pinned += 1;
                continue;
            }
            if self.editor.buffers[i].dirty() && !self.editor.buffers[i].kind.read_only() {
                kept_dirty += 1;
                continue;
            }
            self.editor.close(i);
        }
        self.persist_session();
        if kept_pinned > 0 {
            self.toast(
                trf(
                    "📌 ピン留めした {n} タブは残しました",
                    &[("n", kept_pinned.to_string())],
                ),
                true,
            );
        }
        if kept_dirty > 0 {
            // 1 件ずつ既存の確認ダイアログへ (最初の未保存タブを対象にする)
            self.pending_close = self
                .editor
                .buffers
                .iter()
                .position(|b| b.dirty() && !b.kind.read_only());
            self.toast(
                trf(
                    "未保存の {n} タブは確認してから閉じます",
                    &[("n", kept_dirty.to_string())],
                ),
                false,
            );
        }
    }

    /// メニュー操作をエディタの egui TextEdit へ届ける。
    /// メニュークリックでフォーカスが外れているため、直接イベントを送らず
    /// 「次フレーム冒頭でフォーカス復帰 + イベント注入」のキューに積む。
    pub(super) fn push_editor_event(&mut self, ev: egui::Event, mutates: bool) {
        let Some(i) = self.editor.active else { return };
        if mutates && self.editor.buffers[i].kind.read_only() {
            self.toast(tr("このタブは読み取り専用です"), false);
            return;
        }
        self.pending_editor_events.push(ev);
    }

    /// キューされたイベントをフレーム冒頭で注入する (update() から毎フレーム)。
    /// TextEdit は同一フレーム内でフォーカスがあるときだけイベントを処理する。
    pub(super) fn flush_editor_events(&mut self, ctx: &egui::Context) {
        if self.pending_editor_events.is_empty() {
            return;
        }
        let Some(i) = self.editor.active else {
            self.pending_editor_events.clear();
            return;
        };
        let ed_id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
        ctx.memory_mut(|m| m.request_focus(ed_id));
        for ev in std::mem::take(&mut self.pending_editor_events) {
            ctx.input_mut(|inp| inp.events.push(ev));
        }
    }

    pub(super) fn select_all_active(&mut self, _ctx: &egui::Context) {
        let Some(i) = self.editor.active else { return };
        let len = self.editor.buffers[i].text.chars().count();
        // pending_select は描画側でフォーカス復帰まで面倒を見てくれる
        self.pending_select = Some((0, len));
    }

    /// アクティブなエディタのカーソル位置 (char)。TextEdit の状態から読む。
    pub(super) fn active_cursor_char(&self, ctx: &egui::Context) -> usize {
        let Some(i) = self.editor.active else {
            return 0;
        };
        let ed_id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
        egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| r.primary.index)
            .unwrap_or(0)
    }

    pub(super) fn active_file_path(&self) -> Option<PathBuf> {
        self.editor
            .active
            .and_then(|i| self.editor.buffers[i].path.clone())
    }

    // ── ナビゲーション履歴 (戻る / 進む) ──

    pub(super) fn nav_push(&mut self, path: PathBuf, cursor: usize) {
        if self
            .nav_history
            .get(self.nav_index)
            .map(|(p, c)| *p == path && c.abs_diff(cursor) < 5)
            .unwrap_or(false)
        {
            return;
        }
        self.nav_history.truncate(self.nav_index + 1);
        self.nav_history.push((path, cursor));
        if self.nav_history.len() > 100 {
            self.nav_history.remove(0);
        }
        self.nav_index = self.nav_history.len() - 1;
    }

    pub(super) fn nav_go(&mut self, delta: i64) {
        if self.nav_history.is_empty() {
            self.toast(tr("移動履歴がまだありません"), false);
            return;
        }
        let target = self.nav_index as i64 + delta;
        if target < 0 || target >= self.nav_history.len() as i64 {
            return;
        }
        self.nav_index = target as usize;
        let (path, cursor) = self.nav_history[self.nav_index].clone();
        if self.active_file_path().as_deref() != Some(path.as_path()) {
            if let Some(bi) = self
                .editor
                .buffers
                .iter()
                .position(|b| b.path.as_deref() == Some(path.as_path()))
            {
                self.editor.active = Some(bi);
            } else {
                self.open_path(&path);
            }
        }
        self.jump_to_char(cursor, 0);
    }

    /// アクティブバッファ内の char 位置へ移動 (選択 + 画面中央へスクロール)。
    pub(super) fn jump_to_char(&mut self, pos: usize, len: usize) {
        let Some(i) = self.editor.active else { return };
        let text = &self.editor.buffers[i].text;
        let max = text.chars().count();
        let pos = pos.min(max);
        self.pending_select = Some((pos, (pos + len).min(max)));
        let line = editor_ops::line_of_char(text, pos);
        self.pending_scroll =
            Some((line as f32 * self.last_row_h - self.last_view_h * 0.4).max(0.0));
    }

    /// 履歴に積みながら別ファイルの LSP 位置へ移動する (定義ジャンプ・問題パネル)。
    pub(super) fn jump_to_lsp_pos(&mut self, path: &Path, line: usize, col: usize) {
        // 現在位置を履歴へ (エディタのカーソルは (行, 桁) で保持されている)
        if let (Some(cur), Some(i)) = (self.active_file_path(), self.editor.active) {
            let (line0, col0) = self.editor.cursor;
            let cur_char = editor_ops::char_index_at(&self.editor.buffers[i].text, line0, col0);
            self.nav_push(cur, cur_char);
        }
        if self.active_file_path().as_deref() != Some(path) {
            self.open_path(path);
        }
        let Some(i) = self.editor.active else { return };
        let ch = lsp::lsp_pos_to_char_index(&self.editor.buffers[i].text, line, col);
        self.nav_push(path.to_path_buf(), ch);
        self.jump_to_char(ch, 0);
    }

    pub(super) fn cycle_tab(&mut self, dir: i64) {
        let n = self.editor.buffers.len();
        if n == 0 {
            return;
        }
        let cur = self.editor.active.unwrap_or(0) as i64;
        self.editor.active = Some((cur + dir).rem_euclid(n as i64) as usize);
        self.persist_session();
    }

    /// ⌃Tab / ⌃⇧Tab。
    ///
    /// `tab_switch_mru`(既定オン) なら **押している間だけ候補一覧を出し、
    /// 修飾キーを離したところで確定する** (VS Code / Zed と同じ)。
    /// オフなら従来どおりの位置巡回 ([`Self::cycle_tab`]) — どちらの経路も残す。
    pub(super) fn switch_tab(&mut self, dir: i64) {
        if !self.cfg.tab_switch_mru {
            self.cycle_tab(dir);
            return;
        }
        match self.tab_switcher.as_mut() {
            Some(s) => s.step(dir),
            None => {
                let pane = self.panes.focus_id();
                let order = self.panes.mru_order(pane);
                // 候補が 1 枚しか無いなら `start` が None を返す = 枠を出さない。
                self.tab_switcher = editor_split::TabSwitcher::start(pane, order, dir);
            }
        }
    }

    /// ⌃Tab の切替を毎フレーム見張り、**修飾キーが離れたフレームで確定する**。
    ///
    /// egui は押されている修飾キーを `InputState::modifiers` に持つので、
    /// `ctrl` が false へ落ちた最初のフレームが「離した瞬間」。
    /// Esc は取り消し (何も切り替えない)。押している間は候補が閉じられていないか
    /// 確認し、閉じられていたら候補から落とす。
    pub(super) fn tab_switcher_tick(&mut self, ctx: &egui::Context) {
        let Some(pane) = self.tab_switcher.as_ref().map(|s| s.pane) else {
            return;
        };
        let alive: Vec<u64> = self
            .panes
            .pane(pane)
            .map(|p| p.tabs.clone())
            .unwrap_or_default();
        let (held, cancel) = ctx.input(|i| {
            (
                i.modifiers.ctrl,
                i.events.iter().any(|e| {
                    matches!(
                        e,
                        egui::Event::Key {
                            key: egui::Key::Escape,
                            pressed: true,
                            ..
                        }
                    )
                }),
            )
        });
        if cancel {
            self.tab_switcher = None;
            return;
        }
        if let Some(s) = self.tab_switcher.as_mut() {
            if !s.retain_alive(&alive) {
                self.tab_switcher = None;
                return;
            }
        }
        if held {
            return;
        }
        // 離した = 確定。
        let pick = self.tab_switcher.take().and_then(|s| s.pick());
        if let Some(buf) = pick {
            self.activate_buf(pane, buf);
        }
    }

    /// バッファ ID を指定してペインのアクティブタブを切り替える。
    pub(super) fn activate_buf(&mut self, pane: editor_split::PaneId, buf: u64) {
        self.panes.set_focus(pane);
        if let Some(p) = self.panes.pane_mut(pane) {
            if let Some(at) = p.tabs.iter().position(|x| *x == buf) {
                p.active = at;
                p.touch(buf);
            }
        }
        self.cur_pane = pane;
        self.editor.active = self.editor.buffers.iter().position(|b| b.id == buf);
        // ヒット位置は本文に紐づくので、別のバッファへ移ったら捨てる。
        self.find.current = None;
        self.find.wrapped = None;
        self.persist_session();
    }

    /// ⌃Tab を押している間の候補一覧 (画面中央の 1 枚のカード)。
    pub(super) fn tab_switcher_ui(&mut self, ctx: &egui::Context) {
        let Some(sw) = self.tab_switcher.clone() else {
            return;
        };
        let items: Vec<(String, String)> = sw
            .order
            .iter()
            .filter_map(|b| {
                let buf = self.editor.buffers.iter().find(|x| x.id == *b)?;
                let icon = file_tree::icon_for(&buf.title);
                let hint = buf
                    .path
                    .as_ref()
                    .map(|p| self.rel_label(p))
                    .unwrap_or_default();
                Some((format!("{icon} {}", buf.title), hint))
            })
            .collect();
        let title = trf(
            "{key} で切り替え · 離して確定",
            &[("key", self.key_hint(BindAction::SwitchTab))],
        );
        editor_split::tab_switcher_overlay(ctx, &self.theme, &title, &items, sw.sel);
        // 押している間はキーイベントが来ないので、こちらから描き直しを頼む。
        // (切替が終われば要求も止まる = アイドル時のコストはゼロ)
        crate::perf::repaint(ctx, "tab_switcher_ui");
    }

    // ── ジャンプ系 ──

    pub(super) fn goto_definition(&mut self, ctx: &egui::Context) {
        let Some(i) = self.editor.active else { return };
        let b = &self.editor.buffers[i];
        let Some(path) = b.path.clone() else {
            self.toast(tr("ファイルに紐付いていないタブでは使えません"), false);
            return;
        };
        let lang = b.lang.clone();
        let text = b.text.clone();
        let key = self.lsp_key_for(&path, &lang);
        let Some(client) = self.lsp.get(&key) else {
            self.toast(tr("この言語の LSP サーバーが起動していません"), false);
            return;
        };
        if !client.is_ready() {
            self.toast(
                tr("LSP サーバーの準備中です — 少し待ってからもう一度"),
                false,
            );
            return;
        }
        let cursor = self.active_cursor_char(ctx);
        let (line, col) = lsp::char_index_to_lsp_pos(&text, cursor);
        client.request_definition(&path, line, col);
        self.awaiting_definition = Some(key);
    }

    /// F12 の応答を毎フレーム確認して、届いたらジャンプする。
    pub(super) fn poll_definition_result(&mut self) {
        let Some(key) = self.awaiting_definition.clone() else {
            return;
        };
        let Some(client) = self.lsp.get(&key) else {
            self.awaiting_definition = None;
            return;
        };
        let Some(resp) = client.poll_definition() else {
            return;
        };
        self.awaiting_definition = None;
        match resp {
            Some(loc) => self.jump_to_lsp_pos(&loc.path.clone(), loc.line, loc.col),
            None => self.toast(tr("定義が見つかりませんでした"), false),
        }
    }

    pub(super) fn goto_bracket(&mut self, ctx: &egui::Context) {
        let Some(i) = self.editor.active else { return };
        let cur = self.active_cursor_char(ctx);
        let text = self.editor.buffers[i].text.clone();
        match editor_ops::matching_bracket(&text, cur) {
            Some(pos) => self.jump_to_char(pos, 0),
            None => self.toast(tr("対応する括弧が見つかりませんでした"), false),
        }
    }

    // ── 実行 / ターミナル ──

    /// 任意のシェルコマンドを新しいターミナルセッションとして起動する。
    pub(super) fn spawn_command_terminal(
        &mut self,
        name: String,
        command: String,
        cwd: &Path,
        ctx: &egui::Context,
    ) {
        self.spawn_command_terminal_env(name, command, cwd, &[], ctx);
    }

    /// [`Self::spawn_command_terminal`] に環境変数を足したもの
    /// (tasks.json の `options.env` 用。**起動の仕組みはこの 1 箇所だけ**)。
    pub(super) fn spawn_command_terminal_env(
        &mut self,
        name: String,
        command: String,
        cwd: &Path,
        env: &[(String, String)],
        ctx: &egui::Context,
    ) {
        use crate::agents::Approval;
        let preset = config::AgentPreset {
            name: name.clone(),
            command,
            icon: "▶".into(),
            cwd: Some(cwd.display().to_string()),
            env: env.iter().cloned().collect::<HashMap<_, _>>(),
        };
        match self.agents.launch(
            &preset,
            cwd,
            Approval::from_mode(&self.cfg.approval_mode),
            ctx,
        ) {
            Ok(()) => {
                self.agents.panel_open = true;
                self.toast(trf("▶ {name} を開始しました", &[("name", name)]), true);
            }
            Err(e) => self.toast(e, false),
        }
    }

    pub(super) fn run_active_file(&mut self, ctx: &egui::Context) {
        let Some(i) = self.editor.active else { return };
        let Some(path) = self.editor.buffers[i].path.clone() else {
            self.toast(
                trf(
                    "先にファイルとして保存してください ({key})",
                    &[("key", self.key_hint(BindAction::Save))],
                ),
                false,
            );
            return;
        };
        if self.editor.buffers[i].dirty() {
            self.save_active(false);
        }
        let root = self
            .tree
            .root_for(&path)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| self.primary_root().to_path_buf());
        let Some(cmd) = menu_bar::runner_for(&path, &root) else {
            self.toast(tr("この拡張子の実行コマンドには対応していません"), false);
            return;
        };
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "file".into());
        self.spawn_command_terminal(trf("Run {name}", &[("name", name)]), cmd, &root, ctx);
    }

    // ── tasks.json ──

    /// `.vscode/tasks.json` を必要なときだけ読み直す。
    ///
    /// 描画のたびに呼ばれる前提なので、**TTL 内かつ同じルートなら何もしない**
    /// (設計原則 3: アイドル時のコストはゼロ)。
    pub(super) fn refresh_tasks_cache(&mut self) {
        let root = self.agent_cwd();
        let fresh = self.tasks_cache.root.as_deref() == Some(root.as_path())
            && self
                .tasks_cache
                .read_at
                .is_some_and(|t| t.elapsed() < TASKS_TTL);
        if fresh {
            return;
        }
        self.tasks_cache.doc = tasks::load_tasks(&root);
        self.tasks_cache.root = Some(root);
        self.tasks_cache.read_at = Some(std::time::Instant::now());
    }

    /// tasks.json のタスクを実行できる 1 行へ解決する。
    /// 未対応の `${...}` を含む・パースで弾かれた等の理由なら `Err(理由)`。
    pub(super) fn resolve_json_task(&self, t: &tasks::TaskDef) -> Result<String, String> {
        let file = self
            .editor
            .active
            .and_then(|i| self.editor.buffers[i].path.clone());
        tasks::resolve(t, file.as_deref(), cfg!(windows))
    }

    /// ⇧⌘B が実際に走らせる対象。tasks.json の既定ビルドタスクを優先し、
    /// 走らせられない (未対応変数など) ときだけ自動検出へ落ちる。
    /// メニューのラベルはこの戻り値から作る — **表示と実行を必ず一致させる**。
    ///
    /// 戻り値: (ラベル, コマンド, 作業ディレクトリ, 環境変数, tasks.json 由来か)
    pub(super) fn build_target(
        &self,
    ) -> Option<(String, String, PathBuf, Vec<(String, String)>, bool)> {
        if let Some(t) = self.tasks_cache.doc.default_build() {
            if let Ok(cmd) = self.resolve_json_task(t) {
                return Some((t.label.clone(), cmd, t.cwd.clone(), t.env.clone(), true));
            }
        }
        let root = self.agent_cwd();
        menu_bar::build_task_for(&root).map(|(l, c)| (l, c, root, Vec::new(), false))
    }

    /// tasks.json の n 番目のタスクを実行する (メニュー / コマンドパレット共通)。
    pub(super) fn run_json_task(&mut self, idx: usize, ctx: &egui::Context) {
        let Some(t) = self.tasks_cache.doc.tasks.get(idx).cloned() else {
            self.toast(tr("そのタスクは見つかりませんでした"), false);
            return;
        };
        // 解決できないタスクは走らせない。理由を出す (黙って壊れたコマンドを撃たない)。
        let cmd = match self.resolve_json_task(&t) {
            Ok(c) => c,
            Err(why) => {
                self.toast(why, false);
                return;
            }
        };
        let reveal = t.reveal;
        self.spawn_command_terminal_env(t.label, cmd, &t.cwd, &t.env, ctx);
        if !reveal {
            self.agents.panel_open = false;
        }
    }

    pub(super) fn run_build_task(&mut self, ctx: &egui::Context) {
        // 起動直後 (まだ 1 度も描いていない) でも tasks.json を見てから決める。
        self.refresh_tasks_cache();
        // 検出もビルドも作業フォルダ基準 (メニューのラベルと同じ判定にする)
        let Some((label, cmd, cwd, env, _)) = self.build_target() else {
            self.toast(
                tr("ビルドタスクを検出できませんでした (Cargo.toml / package.json / Makefile)"),
                false,
            );
            return;
        };
        self.spawn_command_terminal_env(label, cmd, &cwd, &env, ctx);
    }

    /// 選択テキスト (無ければカーソル行) をアクティブなターミナルの入力欄へ送る。
    /// プロジェクト方針により Enter は送らない — 実行はユーザーが確定する。
    pub(super) fn run_selection(&mut self, ctx: &egui::Context) {
        let Some(i) = self.editor.active else { return };
        let text = self.editor.buffers[i].text.clone();
        let ed_id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
        let sel = egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| {
                let (a, b) = (r.primary.index, r.secondary.index);
                (a.min(b), a.max(b))
            })
            .filter(|(a, b)| a != b)
            .map(|(a, b)| {
                let sb = editor_ops::char_to_byte(&text, a);
                let eb = editor_ops::char_to_byte(&text, b);
                text[sb..eb].to_string()
            });
        let payload = match sel {
            Some(s) => s,
            None => {
                // 選択が無ければカーソル行 (VS Code: Run Selected Text と同じ流儀)
                let cur = self.active_cursor_char(ctx);
                let line = editor_ops::line_of_char(&text, cur);
                text.split('\n').nth(line).unwrap_or("").to_string()
            }
        };
        if payload.trim().is_empty() {
            self.toast(tr("送るテキストがありません"), false);
            return;
        }
        self.send_to_agent(payload);
    }

    pub(super) fn new_terminal(&mut self, ctx: &egui::Context) {
        match self
            .cfg
            .agents
            .iter()
            .position(|p| p.command.trim().is_empty())
        {
            Some(i) => {
                self.launch_preset(i, ctx);
                self.agents.panel_open = true;
            }
            None => {
                let root = self.agent_cwd();
                self.spawn_command_terminal(tr("Shell"), String::new(), &root, ctx);
            }
        }
    }

    // ── ファイル横断検索 (サイドバー検索タブ) ──

    pub(super) fn start_global_search(&mut self) {
        if self.gsearch.query.trim().is_empty() || self.gsearch.running {
            return;
        }
        let opts = self
            .gsearch
            .options(Some(self.primary_root().to_path_buf()));
        let files: Vec<PathBuf> = self.file_index.iter().map(|f| f.abs.clone()).collect();
        // 正規表現のコンパイルは spawn_with_options が**同期で**返すので、
        // 壊れたパターンはここで赤字にする (literal に落として黙って別の結果を出さない)
        match file_search::spawn_with_options(files, opts) {
            Ok(rx) => {
                self.gsearch.error = None;
                self.gsearch.rx = Some(rx);
                self.gsearch.running = true;
                self.gsearch.searched = true;
                self.gsearch.results.clear();
                self.gsearch.marks.clear();
            }
            Err(e) => {
                self.gsearch.error = Some(tr(&e.to_string()));
                self.gsearch.rx = None;
                self.gsearch.running = false;
            }
        }
    }

    pub(super) fn poll_global_search(&mut self) {
        let done = match &self.gsearch.rx {
            Some(rx) => match rx.try_recv() {
                Ok((hits, scanned)) => {
                    self.gsearch.results = hits;
                    self.gsearch.scanned = scanned;
                    true
                }
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => true,
            },
            None => false,
        };
        if done {
            self.gsearch.rx = None;
            self.gsearch.running = false;
            self.recompute_search_marks();
        }
        self.poll_global_replace();
    }

    /// 表示用スニペットの中のマッチ範囲を数え直す (検索が終わった直後に 1 度だけ)。
    ///
    /// `Hit.col` / `Hit.len` は**元の行**のバイト位置なので、そのままでは
    /// 先頭空白を落としたスニペットへは当たらない。元の行を読み直すのは
    /// ディスクアクセスなので、同じ条件のマッチャをスニペットへ当て直す。
    /// `len` は「1 マッチの長さ」の検算に使う (食い違ったら強調しない)。
    pub(super) fn recompute_search_marks(&mut self) {
        self.gsearch.marks.clear();
        if self.gsearch.results.is_empty() {
            return;
        }
        let opts = self
            .gsearch
            .options(Some(self.primary_root().to_path_buf()));
        let Ok(m) = file_search::Matcher::compile(&opts) else {
            return;
        };
        self.gsearch.marks = self
            .gsearch
            .results
            .iter()
            .map(|h| {
                let all = m.find_all(&h.text);
                // スニペット側で 1 つも当たらない (行が切り詰められた等) なら素で描く
                if all.iter().any(|(s, e)| e - s == h.len) {
                    all
                } else {
                    Vec::new()
                }
            })
            .collect();
    }

    // ── ワークスペース一括置換 (検索タブ) ──

    /// 置換フローを 1 歩進める。**書き込みは Confirm を経た後にしか起きない**。
    pub(super) fn advance_replace(&mut self, ev: ReplaceEvent) {
        let next = self.gsearch.phase.next(&ev);
        // 状態が進まなかった (順序外の出来事) なら何も起こさない
        if next == self.gsearch.phase && !matches!(ev, ReplaceEvent::Cancel) {
            return;
        }
        let dry = match ev {
            ReplaceEvent::Start => true,
            ReplaceEvent::Confirm => false,
            _ => {
                self.gsearch.phase = next;
                if matches!(ev, ReplaceEvent::Cancel) {
                    self.gsearch.replace_rx = None;
                }
                return;
            }
        };
        let req = file_search::ReplaceRequest {
            options: self
                .gsearch
                .options(Some(self.primary_root().to_path_buf())),
            replacement: self.gsearch.replace.clone(),
            dry_run: dry,
        };
        let files: Vec<PathBuf> = self.file_index.iter().map(|f| f.abs.clone()).collect();
        let (tx, rx) = mpsc::channel::<ReplaceMsg>();
        // 走査も書き込みもワーカースレッドで (UI スレッドはファイルに触らない)
        let spawned = std::thread::Builder::new()
            .name("zv-replace".into())
            .spawn(move || {
                let _ = tx.send(file_search::replace_all(&files, &req).map_err(|e| e.to_string()));
            });
        if spawned.is_err() {
            self.toast(tr("置換ワーカーを起動できませんでした"), false);
            return;
        }
        self.gsearch.error = None;
        self.gsearch.replace_rx = Some(rx);
        self.gsearch.phase = next;
    }

    /// 置換ワーカーの結果を取り込む (毎フレーム。届いていなければ即戻る)。
    pub(super) fn poll_global_replace(&mut self) {
        let msg = match &self.gsearch.replace_rx {
            Some(rx) => match rx.try_recv() {
                Ok(m) => Some(m),
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Some(Err(tr("置換が中断されました"))),
            },
            None => return,
        };
        self.gsearch.replace_rx = None;
        let Some(msg) = msg else { return };
        match msg {
            Ok(rep) => {
                let (files, hits) = (rep.files_changed, rep.replacements);
                let ev = if rep.dry_run {
                    ReplaceEvent::DryRunDone { files, hits }
                } else {
                    ReplaceEvent::ExecuteDone { files, hits }
                };
                self.gsearch.phase = self.gsearch.phase.next(&ev);
                if !rep.dry_run {
                    for e in rep.errors.iter().take(3) {
                        self.toast_warn(format!("{}: {}", e.path.display(), e.message));
                    }
                    if hits > 0 {
                        self.toast(
                            trf(
                                "🔁 {files} ファイル / {hits} 箇所を置換しました",
                                &[("files", files.to_string()), ("hits", hits.to_string())],
                            ),
                            true,
                        );
                        // 開いているタブは既存の外部変更ウォッチャ
                        // (Editor::check_external) が拾って読み直す。
                        // ここでは結果一覧だけ最新の中身へ合わせ直す。
                        self.start_global_search();
                    }
                }
            }
            Err(e) => {
                self.gsearch.phase = self.gsearch.phase.next(&ReplaceEvent::Failed);
                self.gsearch.error = Some(tr(&e));
            }
        }
    }

    // (検索パネルの描画は free 関数 global_search_panel — サイドバーの
    //  クロージャ内で self 全体を借りずに済ませるため)

    // ── 置換 (検索バーの置換行) ──

    /// いまのヒットを置換して次を検索する。ヒットが無ければまず検索する。
    ///
    /// 正規表現モードでは置換文字列の `$1` / `${name}` が展開される (VS Code 準拠)。
    pub(super) fn replace_current(&mut self) {
        let Some(i) = self.editor.active else { return };
        if self.find.query.is_empty() {
            return;
        }
        if self.editor.buffers[i].kind.read_only() {
            self.toast(tr("このタブは読み取り専用です"), false);
            return;
        }
        let text = self.editor.buffers[i].text.clone();
        self.refresh_find_hits(i, hash_str(&text));
        // 現在位置が本文と食い違っていたら、まず探し直す (古い範囲を書き潰さない)
        let Some(ix) = self.current_hit_index() else {
            self.find_step(true);
            return;
        };
        let Some((hit, matcher)) = self
            .find_hits
            .as_ref()
            .map(|c| (c.hits[ix], c.matcher.clone()))
        else {
            return;
        };
        let (s, e) = hit.range();
        // `$1` の展開はヒットのある**行**の中で解決する (走査も行単位のため)。
        // 行末の CR は走査対象から外してあるので、ここでも同じ扱いにする。
        let ls = text[..s].rfind('\n').map_or(0, |p| p + 1);
        let le = text[e..].find('\n').map_or(text.len(), |p| e + p);
        let line = text[ls..le].trim_end_matches('\r');
        let rep = find_buffer::expand(&matcher, line, (s - ls, e - ls), &self.find.replace);
        let mut nt = String::with_capacity(text.len() + rep.len());
        nt.push_str(&text[..s]);
        nt.push_str(&rep);
        nt.push_str(&text[e..]);
        // 1 置換 = 1 段。取り消すと置換前のヒットを選んだ状態へ戻る。
        let ed = self
            .edit_step()
            .with_sel_before(byte_range_to_char_range(&text, &(s..e)));
        self.editor.buffers[i].apply_edit(nt, ed);
        // 置換した結果の直後から次を探す (置換文字列が検索語を含んでも無限ループしない)
        self.find.anchor = s + rep.len();
        self.find.current = None;
        // 本文を変えたのでヒット一覧は捨てる (次フレームのハッシュ更新を待たない)
        self.find_hits = None;
        self.queue_lsp_change(i);
        self.find_step(true);
    }

    /// このファイルのヒットをすべて置換する。
    pub(super) fn replace_all_in_active(&mut self) {
        let Some(i) = self.editor.active else { return };
        if self.find.query.is_empty() {
            return;
        }
        if self.editor.buffers[i].kind.read_only() {
            self.toast(tr("このタブは読み取り専用です"), false);
            return;
        }
        let text = self.editor.buffers[i].text.clone();
        self.refresh_find_hits(i, hash_str(&text));
        let Some(matcher) = self.find_hits.as_ref().map(|c| {
            if let Some(err) = &c.error {
                Err(err.clone())
            } else {
                Ok(c.matcher.clone())
            }
        }) else {
            return;
        };
        let matcher = match matcher {
            Ok(m) => m,
            // 不正な正規表現は 1 バイトも書かずに理由を出す
            Err(e) => {
                self.toast(e, false);
                return;
            }
        };
        let (nt, n) = find_buffer::replace_all(&text, &matcher, &self.find.replace);
        if n == 0 {
            self.toast(tr("見つかりませんでした"), false);
            return;
        }
        // 「すべて置換」は何件当たっても**全体で 1 段** (⌘Z 1 回で丸ごと戻る)
        let ed = self.edit_step();
        self.editor.buffers[i].apply_edit(nt, ed);
        self.find.current = None;
        self.find.wrapped = None;
        self.find_hits = None;
        self.queue_lsp_change(i);
        self.toast(trf("{n} 件置換しました", &[("n", n.to_string())]), true);
    }
}
