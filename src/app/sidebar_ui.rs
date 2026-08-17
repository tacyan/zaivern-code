use super::*;

impl ZaivernApp {
    // ─── UI: sidebar ────────────────────────────────────────────────

    pub(super) fn sidebar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let mut actions = TreeActions::default();
        let mut launch: Option<usize> = None;
        let mut focus: Option<usize> = None;
        let mut restart: Option<usize> = None;
        let mut remove: Option<usize> = None;
        let mut cycle: Option<usize> = None;
        let mut refresh = false;
        let mut nf_root = false;
        let mut nd_root = false;
        // プラグインタブのボタン類 (クロージャの中では記録だけする)
        let mut pl = PluginActions::default();
        let mut git_actions = git_panel::GitActions::default();
        // GitHub パネル用 (クロージャ内で self を可変借用するため先に複製しておく)
        let mut gh_actions = panels::GithubActions::default();
        let gh_roots = self.roots.clone();
        let gh_presets: Vec<(String, String)> = self
            .cfg
            .agents
            .iter()
            .map(|p| (p.icon.clone(), p.name.clone()))
            .collect();
        // 検索タブ (VS Code: ⇧⌘F) のアクション。クロージャ内では記録だけして
        // パネル描画後に self へ反映する
        let mut gsearch_go = false;
        let mut gsearch_jump: Option<(PathBuf, usize)> = None;
        // 置換フローの進み (ドライラン要求 / 実行の確認 / 取りやめ)
        let mut gsearch_replace: Option<ReplaceEvent> = None;
        // セッションタブで押されたもの。実行 (エージェント起動など) は描画後
        let mut sess_action = session_picker::SidebarAction::None;
        // パネルの本文はクロージャの中では読むだけなので、先に借りをほどく
        let panel_texts = self.plugin_panels.clone();
        // Markdown パネルの描画に要るもの (画像キャッシュは借りて後で戻す)
        let mut md_images = std::mem::take(&mut self.md_images);

        egui::SidePanel::left("zv-side")
            .resizable(true)
            .default_width(255.0)
            .width_range(180.0..=440.0)
            .show_animated(ctx, self.sidebar_open, |ui| {
                ui.add_space(4.0);
                // タブは横に並べきれないので折り返す。
                // ui.horizontal は折り返さないため、5 つのタブ(合計 500px 超)を
                // 幅 180〜440px のサイドバーへ入れるとパネル外へはみ出し、
                // 最後のタブだけが見えて残りが押し出される。
                // 幅が狭いときはラベルを絵文字だけに縮め、名称はホバーで出す。
                let strip = ui.horizontal_wrapped(|ui| {
                    let narrow = ui.available_width() < 300.0;
                    let n_agents = self.agents.sessions.len();
                    let n_plugins = self.plugins.len();
                    let tabs: [(SidebarTab, String, &str); 7] = [
                        (SidebarTab::Files, "📁".into(), "ファイル"),
                        (SidebarTab::Search, "🔎".into(), "検索"),
                        (SidebarTab::Agents, format!("👾 {n_agents}"), "Agents"),
                        (SidebarTab::Sessions, "💬".into(), "セッション"),
                        (SidebarTab::Plugins, format!("🔌 {n_plugins}"), "プラグイン"),
                        (SidebarTab::Git, "🌿".into(), "Git"),
                        (SidebarTab::GitHub, "🐙".into(), "GitHub"),
                    ];
                    for (tab, short, name) in tabs {
                        let name = tr(name);
                        let label = if narrow {
                            short.clone()
                        } else {
                            format!("{short} {name}")
                        };
                        ui.selectable_value(&mut self.sidebar_tab, tab, label)
                            .on_hover_text(name);
                    }
                });
                let strip_rect = strip.response.rect;
                ui.separator();

                let body = ui.scope(|ui| {
                    match self.sidebar_tab {
                        SidebarTab::Files => {
                            self.sidebar_files_ui(
                                ui,
                                &theme,
                                &mut actions,
                                &mut refresh,
                                &mut nf_root,
                                &mut nd_root,
                            );
                        }
                        SidebarTab::Search => {
                            self.sidebar_search_ui(
                                ui,
                                &theme,
                                &mut gsearch_go,
                                &mut gsearch_jump,
                                &mut gsearch_replace,
                            );
                        }
                        SidebarTab::Agents => {
                            self.sidebar_agents_ui(
                                ui,
                                &theme,
                                &mut launch,
                                &mut focus,
                                &mut restart,
                                &mut remove,
                                &mut cycle,
                            );
                        }
                        SidebarTab::Sessions => {
                            // フォルダ一覧は is_dir() を叩くので、元が変わった時だけ作り直す
                            sess_action = self.sidebar_sessions_ui(ui, &theme);
                        }
                        SidebarTab::Plugins => {
                            self.sidebar_plugins_ui(
                                ui,
                                &theme,
                                &panel_texts,
                                &mut md_images,
                                &mut pl,
                            );
                        }
                        SidebarTab::Git => {
                            // サブタブ: 「変更」(従来の Git パネル) と
                            // 「変更をレビュー」(PR 風のローカルレビュー)。
                            // レビューは左に変更ファイル・右に diff の 2 ペインなので
                            // サイドバーを広げて使う想定 (幅はユーザーが決める)。
                            ui.horizontal(|ui| {
                                for (on_review, label) in
                                    [(false, tr("変更")), (true, tr("変更をレビュー"))]
                                {
                                    let sel = self.git_sub_review == on_review;
                                    if ui
                                        .selectable_label(sel, RichText::new(label).small())
                                        .clicked()
                                    {
                                        self.git_sub_review = on_review;
                                    }
                                }
                            });
                            ui.separator();
                            // both: 長いパス名でサイドバーが横に突き破るのを防ぐ
                            // (sidebar_files_ui の zv-tree と同じ理由)
                            egui::ScrollArea::both()
                                .id_salt("zv-git")
                                .auto_shrink(false)
                                .show(ui, |ui| {
                                    if self.git_sub_review {
                                        self.review.ui(ui, &theme, &mut git_actions);
                                    } else {
                                        self.git_panel.ui(ui, &theme, &mut git_actions);
                                    }
                                });
                        }
                        SidebarTab::GitHub => {
                            // both: 長い PR/Issue タイトルでの横突き破り防止 (zv-tree と同じ理由)
                            egui::ScrollArea::both()
                                .id_salt("zv-github")
                                .auto_shrink(false)
                                .show(ui, |ui| {
                                    panels::github_ui(
                                        ui,
                                        &theme,
                                        &mut self.github,
                                        &gh_roots,
                                        &gh_presets,
                                        &mut gh_actions,
                                    );
                                });
                        }
                    }
                });
                // ガイドツアーのアンカー: 「いま見えているタブ」だけ申告する。
                // タブ本体が細いとき (空の一覧) でも押す場所が分かるよう、
                // タブ列そのものを併せた矩形を渡す。
                let tab_rect = strip_rect.union(body.response.rect);
                let id = match self.sidebar_tab {
                    SidebarTab::Files => Some(AnchorId::FileTree),
                    SidebarTab::Search => Some(AnchorId::SearchTab),
                    SidebarTab::Sessions => Some(AnchorId::SessionsTab),
                    SidebarTab::Plugins => Some(AnchorId::PluginsTab),
                    SidebarTab::Git => Some(AnchorId::GitTab),
                    SidebarTab::GitHub => Some(AnchorId::GitHubTab),
                    // Agents タブに対応する手順は無い (ツアーは Cockpit で説明する)
                    SidebarTab::Agents => None,
                };
                if let Some(id) = id {
                    tutorial::anchor(ui.ctx(), id, tab_rect);
                }
                // 「変更をレビュー」サブタブも差分ビューの一種。差分タブを
                // 開いていなくても手順が空振りしないよう、ここでも申告する。
                if self.sidebar_tab == SidebarTab::Git && self.git_sub_review {
                    tutorial::anchor(ui.ctx(), AnchorId::DiffView, body.response.rect);
                }
            });

        // gh の呼び出しは 1 本残らずワーカースレッドへ回す
        if !gh_actions.requests.is_empty() {
            let reqs = std::mem::take(&mut gh_actions.requests);
            self.dispatch_gh(reqs, ctx);
        }
        if let Some((msg, ok)) = gh_actions.toast {
            self.toast(msg, ok);
        }
        if let Some((root, issue, preset_idx)) = gh_actions.start_issue {
            self.start_issue_flow(&root, &issue, preset_idx, ctx);
        }
        // レビュー済みの印が変わったらセッションへ書き戻す
        // (印が残らないとレビューは「有限」でなくなる)。
        if self.review.take_reviewed_dirty() {
            self.persist_session();
        }
        if let Some((msg, ok)) = git_actions.toast {
            self.toast(msg, ok);
        }
        if let Some(dir) = git_actions.open_path {
            self.open_workspace(dir, ctx);
        }
        // レビュー画面の「エディタで開く」。open_path (ワークスペース切替) とは別物。
        if let Some(file) = git_actions.open_file {
            self.open_path(&file);
        }
        // レビュー画面のインラインコメント → エージェントの入力欄へ。
        // 差分ビューの「エージェントに送る」と同じ経路 (送信はしない)。
        if let Some(prompt) = git_actions.review_prompt {
            self.take_review_prompt(prompt);
        }
        // Git パネルの履歴一覧をクリック → そのコミットの差分を既存の差分ビューで開く。
        if let Some((top, sha)) = git_actions.open_commit {
            self.open_commit_diff_at(&top, &sha);
        }

        self.md_images = md_images;

        // 有効/無効の切り替えを保存し、登録内容を作り直す
        if let Some((name, enabled)) = pl.toggle {
            // **言語パックは同時に 1 つだけ。** 素直に有効化すると
            // english-mode と korean-mode が両方「有効」になり、どちらが効いて
            // いるのかが画面から読み取れなくなる (実際に効くのは名前順の先頭で、
            // 表示と食い違う)。言語パックの切り替えは「表示言語を選ぶ」ことに
            // 読み替え、`set_ui_language` に一本化する — あちらが他の言語パックを
            // 無効へ倒し、`config.toml` の `ui_language` まで揃えてくれる。
            let as_language = self
                .language_plugins()
                .into_iter()
                .find(|(n, _)| *n == name)
                .map(|(_, lang)| {
                    if enabled {
                        lang
                    } else {
                        locale::AUTO.to_string()
                    }
                });
            if let Some(target) = as_language {
                self.set_ui_language(&target, ctx);
            } else {
                self.cfg.plugins.set_enabled(&name, enabled);
                self.cfg.global_plugins.set_enabled(&name, enabled);
                if let Err(e) = config::save_plugins_section(&self.cfg) {
                    self.toast(trf("設定の保存に失敗: {e}", &[("e", e)]), false);
                }
                self.rebuild_plugins();
                let verb = tr(if enabled { "有効" } else { "無効" });
                self.toast(
                    trf(
                        "🔌 {name} を{verb}にしました",
                        &[("name", name), ("verb", verb)],
                    ),
                    true,
                );
            }
        }
        // 設定値の変更を保存し、実行中のプラグインへも反映する
        if let Some((name, key, value)) = pl.setting {
            self.cfg.plugins.set_setting(&name, &key, &value);
            self.cfg.global_plugins.set_setting(&name, &key, &value);
            if let Err(e) = config::save_plugins_section(&self.cfg) {
                self.toast(trf("設定の保存に失敗: {e}", &[("e", e.to_string())]), false);
            }
            if let Some(vals) = self.cfg.plugins.settings.get(&name).cloned() {
                if let Some(p) = self.plugins.iter_mut().find(|p| p.name == name) {
                    p.apply_settings(&vals);
                }
            }
        }
        // パネルの手動更新
        if let Some((name, panel)) = pl.panel_refresh {
            self.refresh_panel(&name, &panel, ctx);
        }

        // 検索タブのアクション (横断検索の開始 / 結果へのジャンプ / 置換)
        if gsearch_go {
            self.start_global_search();
        }
        if let Some((path, line)) = gsearch_jump {
            self.jump_to_lsp_pos(&path, line, 0);
        }
        if std::mem::take(&mut self.gsearch.open_multi) {
            self.open_search_multibuffer();
        }
        if let Some(ev) = gsearch_replace {
            self.advance_replace(ev);
        }
        // セッションタブのアクション (再開 / 新規会話 / フォルダを開く / 閉じる)
        if sess_action != session_picker::SidebarAction::None {
            self.apply_session_sidebar(sess_action, ctx);
        }

        if pl.new_plugin {
            self.apply_cmd(Cmd::NewPlugin, ctx);
        }
        if pl.install {
            self.apply_cmd(Cmd::InstallPlugin, ctx);
        }
        if pl.rescan {
            self.apply_cmd(Cmd::RescanPlugins, ctx);
        }
        if let Some(dir) = pl.uninstall {
            match plugins::uninstall(&dir) {
                Ok(()) => {
                    self.rebuild_plugins();
                    self.toast(tr("🗑 プラグインをアンインストールしました"), true);
                }
                Err(e) => self.toast(
                    trf("アンインストール失敗: {e}", &[("e", e.to_string())]),
                    false,
                ),
            }
        }
        if let Some(pi) = pl.export {
            let root = self.primary_root().to_path_buf();
            let res = self.plugins.get(pi).map(|p| plugins::export(p, &root));
            match res {
                Some(Ok(path)) => self.toast(
                    trf(
                        "📤 エクスポートしました: {path}",
                        &[("path", path.display().to_string())],
                    ),
                    true,
                ),
                Some(Err(e)) => {
                    self.toast(trf("エクスポート失敗: {e}", &[("e", e.to_string())]), false)
                }
                None => {}
            }
        }
        if let Some(path) = pl.open {
            self.open_path(&path);
        }
        if let Some((pi, ci)) = pl.run {
            self.apply_cmd(Cmd::RunPlugin(pi, ci), ctx);
        }
        if let Some(t) = pl.theme {
            self.apply_cmd(Cmd::SetTheme(t), ctx);
        }
        self.apply_tree_actions(actions, refresh, nf_root, nd_root, ctx);
        if let Some(i) = launch {
            self.launch_preset(i, ctx);
        }
        if let Some(i) = focus {
            self.apply_cmd(Cmd::FocusAgent(i), ctx);
        }
        if let Some(i) = restart {
            if let Err(e) = self.agents.restart(i, ctx) {
                self.toast(e, false);
            }
        }
        if let Some(i) = cycle {
            match self.agents.cycle_permission(i) {
                Some(hint) => self.toast_warn(trf(
                    "🛡 権限モード切替を送信しました（{hint} / 画面を確認してください）",
                    &[("hint", hint.to_string())],
                )),
                None => self.toast(tr("このセッションは権限モード切替に未対応です"), false),
            }
        }
        if let Some(i) = remove {
            self.close_agent(i);
        }
    }

    /// サイドバー: ファイルタブ (ツリー + 新規作成/再読み込みボタン)。
    /// 押されたボタンは記録だけして呼び出し側で self へ反映する。
    pub(super) fn sidebar_files_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        actions: &mut TreeActions,
        refresh: &mut bool,
        nf_root: &mut bool,
        nd_root: &mut bool,
    ) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(roots_label(&self.roots)).strong());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("⟳").on_hover_text(tr("再読み込み")).clicked() {
                    *refresh = true;
                }
                if ui.button("📂").on_hover_text(tr("新規フォルダ")).clicked() {
                    *nd_root = true;
                }
                if ui.button("➕").on_hover_text(tr("新規ファイル")).clicked() {
                    *nf_root = true;
                }
            });
        });
        // 横スクロールも許可する: 長いファイル名 (Windows のホームにある
        // NTUSER.DAT{...}.regtrans-ms 等) が縦専用スクロールだと収まらず、
        // サイドバーの「中身の矩形」がパネル幅を突き破って伸びてしまう。
        // egui の SidePanel は中身の矩形で次パネルの開始位置を決めるため、
        // 突き破った分だけ中央領域が右へ押され、間が未描画 (真っ黒) になる。
        // アクティブファイル追従 (VS Code の explorer.autoReveal): ツリーへ通知
        let active = self.active_file_path();
        self.tree.set_active_file(active.as_deref());
        // ファイル操作の設定と取り消し履歴の状態もツリーへ渡す
        // (値の持ち主は config / App 側。ツリーは表示と要求だけ)
        let undo_hint = self.file_undo_hint();
        self.tree.set_file_ops_state(
            self.cfg.confirm_drag_and_drop,
            self.cfg.enable_trash,
            undo_hint,
        );
        // ツリーの絞り込み入力。スクロール領域の外に置き、流れて消えないようにする
        self.tree.filter_ui(ui, theme);
        egui::ScrollArea::both()
            .id_salt("zv-tree")
            .auto_shrink(false)
            .show(ui, |ui| {
                self.tree.ui(ui, theme, &self.gitinfo, actions);
            });
    }

    /// サイドバー: 検索タブ (VS Code: ⇧⌘F)。開始/ジャンプは記録だけして
    /// パネル描画後に呼び出し側で self へ反映する。
    pub(super) fn sidebar_search_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        go: &mut bool,
        jump: &mut Option<(PathBuf, usize)>,
        replace: &mut Option<ReplaceEvent>,
    ) {
        let before = (
            self.gsearch.case_sensitive,
            self.gsearch.whole_word,
            self.gsearch.regex,
        );
        let (g, j, r) = global_search_panel(ui, theme, &mut self.gsearch, &self.file_index);
        let after = (
            self.gsearch.case_sensitive,
            self.gsearch.whole_word,
            self.gsearch.regex,
        );
        if before != after {
            self.save_search_prefs(ui.ctx());
        }
        *go |= g;
        if j.is_some() {
            *jump = j;
        }
        if r.is_some() {
            *replace = r;
        }
    }

    /// サイドバー: Agents タブ (セッション一覧 + プリセット起動)。
    /// 押されたボタンは Option に記録だけして呼び出し側で反映する。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn sidebar_agents_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        launch: &mut Option<usize>,
        focus: &mut Option<usize>,
        restart: &mut Option<usize>,
        remove: &mut Option<usize>,
        cycle: &mut Option<usize>,
    ) {
        egui::ScrollArea::vertical()
            .id_salt("zv-agents")
            .auto_shrink(false)
            .show(ui, |ui| {
                let mut set_unread: Option<usize> = None;
                let mut rename_req: Option<usize> = None;
                for (i, s) in self.agents.sessions.iter().enumerate() {
                    let active = i == self.agents.active;
                    let frame = egui::Frame::none()
                        .fill(if active {
                            theme.accent_soft
                        } else {
                            Color32::TRANSPARENT
                        })
                        .rounding(egui::Rounding::same(6.0))
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0));
                    // 行の余白クリックでフォーカスできるようにする。
                    // 後掛けの ui.interact は行内の ✕/⟳/🛡 ボタンへの
                    // クリックを奪う (ヒットテストは後登録が勝つ) ため、
                    // UiBuilder::sense で行の判定を先に登録する。
                    let fr = ui.scope_builder(
                        egui::UiBuilder::new()
                            .id_salt(("agent-row", s.id))
                            .sense(egui::Sense::click()),
                        |ui| {
                            frame.show(ui, |ui| {
                                ui.horizontal(|ui| {
                                    let dot = if s.running() {
                                        if s.attention {
                                            RichText::new("●").color(theme.warn)
                                        } else {
                                            RichText::new("●").color(theme.ok)
                                        }
                                    } else {
                                        RichText::new("○").color(theme.err)
                                    };
                                    ui.label(dot);
                                    let badge = if s.is_permission_agent() {
                                        s.approval_badge()
                                    } else {
                                        ""
                                    };
                                    let permission_hint = s.permission_switch_hint();
                                    // 選択可能ラベルはクリックを吸ってしまい
                                    // 行クリック (フォーカス) が効かなくなるので、
                                    // タイトルは文字選択を切ってクリックを行へ通す
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(format!(
                                                "{}{} {}",
                                                badge, s.icon, s.title
                                            ))
                                            .color(theme.text),
                                        )
                                        .selectable(false),
                                    );
                                    if s.has_unread() && !active {
                                        ui.label(RichText::new("◆").size(9.0).color(theme.accent))
                                            .on_hover_text(tr(
                                                "最後に見てから新しい出力があります",
                                            ));
                                    }
                                    if let Some(line) = &s.rate_limited {
                                        ui.label(RichText::new("⏳").color(theme.warn))
                                            .on_hover_text(trf(
                                                "レート制限/使用上限: {line}",
                                                &[("line", line.clone())],
                                            ));
                                    }
                                    ui.with_layout(
                                        egui::Layout::right_to_left(egui::Align::Center),
                                        |ui| {
                                            // **ボタンの ID はセッション ID から作る。**
                                            // この行には出たり消えたりするラベル
                                            // (◆ 未読 / ⏳ レート制限) と条件付きの 🛡
                                            // が混ざっている。egui 0.29 の
                                            // `small_button` は自動採番で ID を作るので、
                                            // 押した瞬間と離した瞬間で 1 個ずれると
                                            // **✕ を押したのに ⟳ が発火する**。
                                            // 再現は `e2e::widget_id_shift_tests`。
                                            let btn = |ui: &mut egui::Ui, key: &'static str, label: &str| {
                                                ui.push_id((s.id, key), |ui| {
                                                    ui.small_button(label)
                                                })
                                                .inner
                                            };
                                            if btn(ui, "close", "✕").clicked() {
                                                *remove = Some(i);
                                            }
                                            if btn(ui, "restart", "⟳").clicked() {
                                                *restart = Some(i);
                                            }
                                            if let Some(hint) = permission_hint {
                                                if btn(ui, "perm", "🛡")
                                                    .on_hover_text(hint)
                                                    .clicked()
                                                {
                                                    *cycle = Some(i);
                                                }
                                            }
                                            ui.label(
                                                RichText::new(s.uptime())
                                                    .size(10.5)
                                                    .color(theme.text_dim),
                                            );
                                        },
                                    );
                                });
                            });
                        },
                    );
                    let resp = fr.response;
                    if resp.clicked() {
                        *focus = Some(i);
                    }
                    resp.context_menu(|ui| {
                        if !s.has_unread() && ui.button(tr("📩 あとで見る (未読にする)")).clicked()
                        {
                            set_unread = Some(i);
                            ui.close_menu();
                        }
                        if ui.button(tr("🔍 フォーカス")).clicked() {
                            *focus = Some(i);
                            ui.close_menu();
                        }
                        // 手で付けた名前は自動命名に**絶対に**上書きされない。
                        if ui
                            .button(tr("✏️ 名前を変更…"))
                            .on_hover_text(tr(
                                "手で付けた名前は、ターン終了時の自動命名に上書きされません",
                            ))
                            .clicked()
                        {
                            rename_req = Some(i);
                            ui.close_menu();
                        }
                    });
                }
                if let Some(i) = set_unread {
                    let id = self.agents.sessions.get_mut(i).map(|s| {
                        s.mark_unread();
                        s.id
                    });
                    if let Some(id) = id {
                        // 後回し宣言 = 次に待機へ戻ったらもう一度だけ鳴らす
                        self.work_gate.forget(id);
                    }
                }
                if let Some(i) = rename_req {
                    self.begin_rename_agent(i);
                }

                ui.add_space(8.0);
                ui.label(RichText::new(tr("── プリセット ──")).color(theme.text_dim));
                for (i, p) in self.cfg.agents.iter().enumerate() {
                    if ui
                        .add_sized(
                            [ui.available_width(), 26.0],
                            egui::Button::new(format!("{} {}", p.icon, p.name)),
                        )
                        .clicked()
                    {
                        *launch = Some(i);
                    }
                }
            });
    }

    /// サイドバー: プラグインタブ。ボタン類は PluginActions に記録だけして
    /// パネル描画後に呼び出し側で self へ反映する。
    pub(super) fn sidebar_plugins_ui(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        panel_texts: &HashMap<(String, String), String>,
        md_images: &mut markdown::ImageCache,
        pl: &mut PluginActions,
    ) {
        self.sidebar_plugins_toolbar_ui(ui, theme, pl);
        ui.separator();
        egui::ScrollArea::vertical()
            .id_salt("zv-plugins")
            .auto_shrink(false)
            .show(ui, |ui| {
                if self.plugins.is_empty() {
                    ui.label(
                        RichText::new(tr("プラグインがありません。➕ から自作できます"))
                            .color(theme.text_dim),
                    );
                }
                for (pi, p) in self.plugins.iter().enumerate() {
                    egui::Frame::none()
                        .rounding(egui::Rounding::same(6.0))
                        .inner_margin(egui::Margin::symmetric(8.0, 6.0))
                        .fill(theme.panel_alt)
                        .show(ui, |ui| {
                            self.plugin_card_header_ui(ui, theme, pl, pi, p);
                            // 有効/無効。無効にするとコマンド・フック・
                            // パネル・テーマ・スニペットを一切登録しない
                            let mut enabled = p.enabled;
                            if ui
                                .checkbox(&mut enabled, tr("有効"))
                                .on_hover_text(tr(
                                    "外すとコマンド・フック・パネル・テーマを読み込みません",
                                ))
                                .changed()
                            {
                                pl.toggle = Some((p.name.clone(), enabled));
                            }
                            if let Some(err) = &p.error {
                                ui.label(
                                    RichText::new(format!("⚠ {err}"))
                                        .size(10.5)
                                        .color(theme.warn),
                                );
                                return;
                            }
                            if !p.enabled {
                                ui.label(
                                    RichText::new(tr("(無効)")).size(10.5).color(theme.text_dim),
                                );
                                return;
                            }
                            if !p.description.is_empty() {
                                ui.label(
                                    RichText::new(&p.description)
                                        .size(10.5)
                                        .color(theme.text_dim),
                                );
                            }
                            // 構文定義を持つプラグインだけ 🔤 を足す
                            // (常に 0 が並ぶバッジは作らない)
                            let langs = self.plugin_langs.get(&p.name);
                            let lang_badge = match langs {
                                Some(v) if !v.is_empty() => format!("  🔤{}", v.len()),
                                _ => String::new(),
                            };
                            let counts = ui.label(
                                RichText::new(format!(
                                    "▶{}  🪝{}  📋{}  🎨{}  ✂{}{}{}",
                                    p.commands.len(),
                                    p.hooks.len(),
                                    p.panels.len(),
                                    p.themes.len(),
                                    p.snippet_files.len(),
                                    lang_badge,
                                    if p.author.is_empty() {
                                        String::new()
                                    } else {
                                        format!("  by {}", p.author)
                                    }
                                ))
                                .size(10.5)
                                .color(theme.text_dim),
                            );
                            if let Some(v) = langs.filter(|v| !v.is_empty()) {
                                counts.on_hover_text(trf(
                                    "追加される言語 ({n}): {list}",
                                    &[("n", v.len().to_string()), ("list", v.join(", "))],
                                ));
                            }
                            for (ci, c) in p.commands.iter().enumerate() {
                                let btn = ui.small_button(format!("{} {}", c.icon, c.title));
                                let btn = match &c.keybind {
                                    Some(k) => btn.on_hover_text(k),
                                    None => btn,
                                };
                                if btn.clicked() {
                                    pl.run = Some((pi, ci));
                                }
                            }
                            for (label, path) in &p.themes {
                                if ui.small_button(format!("🎨 {label}")).clicked() {
                                    pl.theme = Some(path.to_string_lossy().to_string());
                                }
                            }

                            self.plugin_settings_ui(ui, theme, pl, pi, p);

                            self.plugin_panels_ui(ui, theme, panel_texts, md_images, pl, p);
                        });
                    ui.add_space(4.0);
                }
            });
    }

    /// サイドバー: プラグインタブ上部のツールバー (新規/インストール/再スキャン) と
    /// 説明行。ボタンは PluginActions に記録するだけ。
    pub(super) fn sidebar_plugins_toolbar_ui(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        pl: &mut PluginActions,
    ) {
        ui.horizontal(|ui| {
            if ui
                .button(tr("➕ 新規作成"))
                .on_hover_text(tr("プラグインのテンプレート一式を生成"))
                .clicked()
            {
                pl.new_plugin = true;
            }
            if ui
                .button(tr("📦 インストール…"))
                .on_hover_text(tr(".zvplug / .zip を取り込む"))
                .clicked()
            {
                pl.install = true;
            }
            if ui.button("⟳").on_hover_text(tr("再スキャン")).clicked() {
                pl.rescan = true;
            }
        });
        ui.label(
            RichText::new(tr(
                "コマンド・テーマ・スニペットを 1 フォルダで。📤 で配布用 .zvplug を作成",
            ))
            .size(10.5)
            .color(theme.text_dim),
        );
    }

    /// プラグインカードの見出し行 (名前/バージョン/API と右寄せの
    /// アンインストール/エクスポート/plugin.toml ボタン)。
    pub(super) fn plugin_card_header_ui(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        pl: &mut PluginActions,
        pi: usize,
        p: &plugins::Plugin,
    ) {
        ui.horizontal(|ui| {
            ui.label(RichText::new(&p.name).strong().color(theme.text));
            ui.label(
                RichText::new(format!("v{}", p.version))
                    .size(10.5)
                    .color(theme.text_dim),
            );
            ui.label(
                RichText::new(format!("API{}", p.api))
                    .size(10.0)
                    .color(theme.text_dim),
            )
            .on_hover_text(tr("マニフェストの api バージョン"));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .small_button("🗑")
                    .on_hover_text(tr("アンインストール"))
                    .clicked()
                {
                    pl.uninstall = Some(p.dir.clone());
                }
                if ui
                    .small_button("📤")
                    .on_hover_text(tr("配布用 .zvplug をエクスポート"))
                    .clicked()
                {
                    pl.export = Some(pi);
                }
                if ui
                    .small_button("📝")
                    .on_hover_text(tr("plugin.toml を開く"))
                    .clicked()
                {
                    pl.open = Some(p.dir.join("plugin.toml"));
                }
            });
        });
    }

    /// プラグインの設定 ([[setting]]) — 変更したその場で保存する。
    pub(super) fn plugin_settings_ui(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        pl: &mut PluginActions,
        pi: usize,
        p: &plugins::Plugin,
    ) {
        if !p.settings.is_empty() {
            ui.add_space(2.0);
            egui::CollapsingHeader::new(tr("⚙ 設定"))
                .id_salt(("zv-plset", pi))
                .show(ui, |ui| {
                    for s in &p.settings {
                        let label = if s.label.is_empty() {
                            s.key.clone()
                        } else {
                            s.label.clone()
                        };
                        let cur = p.setting(&s.key);
                        match s.kind {
                            plugins::SettingType::Bool => {
                                let mut on = cur.trim() == "true";
                                if ui.checkbox(&mut on, label).changed() {
                                    pl.setting =
                                        Some((p.name.clone(), s.key.clone(), on.to_string()));
                                }
                            }
                            _ => {
                                ui.label(RichText::new(label).size(10.5).color(theme.text_dim));
                                let mut v = cur.clone();
                                let te = egui::TextEdit::singleline(&mut v)
                                    // ID は設定キーから作る。省くと egui は
                                    // **並び順から自動採番**するので、設定が
                                    // 増減するとカーソルが別の欄へ移る。
                                    .id_salt(("zv-plset-val", &s.key))
                                    .password(s.secret)
                                    .desired_width(f32::INFINITY);
                                if ui.add(te).changed() {
                                    // 型に合わない入力は保存しない
                                    if s.kind.accepts(&v) {
                                        pl.setting = Some((p.name.clone(), s.key.clone(), v));
                                    }
                                }
                            }
                        }
                    }
                });
        }
    }

    /// プラグインのパネル ([[panel]]) — 本文をそのまま描く。
    pub(super) fn plugin_panels_ui(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        panel_texts: &HashMap<(String, String), String>,
        md_images: &mut markdown::ImageCache,
        pl: &mut PluginActions,
        p: &plugins::Plugin,
    ) {
        let md_base = self.scaled_editor_font();
        let hl = self.highlighter;
        for pa in &p.panels {
            ui.add_space(2.0);
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(format!("{} {}", pa.icon, pa.title))
                        .size(11.0)
                        .strong()
                        .color(theme.text),
                );
                if !pa.run.trim().is_empty()
                    && ui
                        .small_button("⟳")
                        .on_hover_text(tr("このパネルを更新"))
                        .clicked()
                {
                    pl.panel_refresh = Some((p.name.clone(), pa.id.clone()));
                }
            });
            let key = (p.name.clone(), pa.id.clone());
            match panel_texts.get(&key) {
                Some(t) if !t.trim().is_empty() => match pa.format {
                    plugins::PanelFormat::Markdown => {
                        let mut rctx = markdown::RenderCtx {
                            dir: None,
                            images: &mut *md_images,
                        };
                        markdown::render(ui, theme, hl, md_base, t, &mut rctx);
                    }
                    plugins::PanelFormat::Text => {
                        ui.label(RichText::new(t).monospace().size(11.0).color(theme.text));
                    }
                },
                _ => {
                    ui.label(
                        RichText::new(tr("(内容なし)"))
                            .size(10.5)
                            .color(theme.text_dim),
                    );
                }
            }
        }
    }

    /// ファイルツリー由来のアクション (開く/新規作成/リネーム/貼り付け等) を
    /// 描画後にまとめて反映する。
    pub(super) fn apply_tree_actions(
        &mut self,
        actions: TreeActions,
        refresh: bool,
        nf_root: bool,
        nd_root: bool,
        ctx: &egui::Context,
    ) {
        if refresh {
            self.apply_cmd(Cmd::RefreshTree, ctx);
        }
        if let Some(p) = actions.open {
            // ツリーの 1 回クリックは**プレビュー** — 眺めるだけでタブが増えない。
            // 同じファイルをもう一度押す / 編集する / ピン留めすれば確定タブになる。
            self.open_path_preview(&p);
        }
        if let Some(t) = actions.send_to_agent {
            self.send_to_agent(t);
        }
        if nf_root {
            // VS Code 同様、ツリーの選択位置(フォルダ/ファイルの親)へ作る
            self.tree.start_new_file(self.tree.new_entry_dir());
        }
        if nd_root {
            self.tree.start_new_dir(self.tree.new_entry_dir());
        }
        if let Some(p) = actions.create_file {
            let res = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&p)
                .map(|_| ());
            match res {
                Ok(()) => {
                    self.tree.invalidate();
                    self.tree.select(&p);
                    self.open_path(&p);
                    self.push_file_op(FileOp::Create {
                        path: p.clone(),
                        is_dir: false,
                    });
                    self.toast(
                        trf("➕ {path} を作成しました", &[("path", self.rel_label(&p))]),
                        true,
                    );
                }
                Err(e) => self.toast(trf("作成できません: {e}", &[("e", e.to_string())]), false),
            }
        }
        if let Some(p) = actions.create_dir {
            if p.exists() {
                self.toast(
                    trf("既に存在します: {path}", &[("path", self.rel_label(&p))]),
                    false,
                );
            } else {
                match std::fs::create_dir(&p) {
                    Ok(()) => {
                        self.tree.invalidate();
                        self.tree.select(&p);
                        self.push_file_op(FileOp::Create {
                            path: p.clone(),
                            is_dir: true,
                        });
                        self.toast(
                            trf("📂 {path} を作成しました", &[("path", self.rel_label(&p))]),
                            true,
                        );
                    }
                    Err(e) => self.toast(
                        trf("フォルダを作成できません: {e}", &[("e", e.to_string())]),
                        false,
                    ),
                }
            }
        }
        if let Some((from, to)) = actions.rename {
            // 大文字小文字だけのリネーム: APFS/HFS+/NTFS は case-insensitive
            // なので to.exists() が真になるが、これは同一ファイル。拒否せず
            // fs::rename に任せる (case-only rename は OS 側が正しく扱う)。
            let case_only = from.parent() == to.parent()
                && from.file_name().zip(to.file_name()).is_some_and(|(a, b)| {
                    a != b
                        && a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
                });
            if to.exists() && !case_only {
                self.toast(
                    trf("既に存在します: {path}", &[("path", self.rel_label(&to))]),
                    false,
                );
            } else {
                match std::fs::rename(&from, &to) {
                    Ok(()) => {
                        self.retarget_buffers(&from, &to);
                        self.tree.invalidate();
                        self.tree.select(&to);
                        self.persist_session();
                        self.push_file_op(FileOp::Rename {
                            from: from.clone(),
                            to: to.clone(),
                        });
                        self.toast(
                            trf("✏ {path} に変更しました", &[("path", self.rel_label(&to))]),
                            true,
                        );
                    }
                    Err(e) => self.toast(
                        trf("名前を変更できません: {e}", &[("e", e.to_string())]),
                        false,
                    ),
                }
            }
        }
        // 移動/コピー (D&D と ⌘C/⌘X → ⌘V)。複数選択でも 1 ジョブ。
        // **ここでは fs を触らない。** 確認ダイアログ (transfer_confirm_ui) を
        // 通った項目だけが run_transfer_item へ行く。
        if let Some(job) = actions.transfer {
            self.pending_transfer = Some(TransferQueue::new(job));
        }
        if let Some(msg) = actions.notice {
            self.toast_warn(msg);
        }
        if let Some(req) = actions.delete {
            if !req.paths.is_empty() {
                // 設定でゴミ箱を切っているときは、押した場所によらず完全削除
                self.pending_delete = Some(DeleteRequest {
                    permanent: req.permanent || !self.cfg.enable_trash,
                    ..req
                });
            }
        }
        if actions.undo {
            self.undo_file_op();
        }
        if let Some(v) = actions.set_confirm_dnd {
            self.cfg.confirm_drag_and_drop = v;
            config::save_state(&self.cfg);
        }
        if let Some(v) = actions.set_use_trash {
            self.cfg.enable_trash = v;
            config::save_state(&self.cfg);
        }
    }
}
