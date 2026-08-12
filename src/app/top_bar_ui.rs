use super::*;

impl ZaivernApp {
    // ─── UI: top bar ────────────────────────────────────────────────

    pub(super) fn top_bar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let mut cmds: Vec<Cmd> = Vec::new();
        let branch = self.git_branch();

        // tasks.json は TTL 内なら読み直さない (メニューを組む前に 1 回だけ)
        self.refresh_tasks_cache();
        // VS Code 準拠メニューバーの表示状態スナップショット (描画用の読み取り専用)
        let menu_info = self.build_menu_info(ctx);

        let bar = egui::TopBottomPanel::top("zv-top")
            .exact_height(42.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0)),
            )
            .show(ctx, |ui| {
                // 幅が足りないときは右側を縮退させる。縮退しないと右側が
                // メニューバーの上に重なって両方読めなくなる。
                let density = top_bar_density(ui.available_width());
                ui.horizontal_centered(|ui| {
                    self.top_bar_left(ui, &theme, &menu_info, &branch, &mut cmds);

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if density == TopBarDensity::Overflow {
                            // 装飾系 (テーマ / リモート / 音声 / ペット) は 1 つの
                            // 「⋯」へ畳む。エージェント操作だけは常に表に残す。
                            ui.menu_button("⋯", |ui| {
                                self.top_bar_theme_menu(ui, &mut cmds);
                                self.top_bar_remote_and_voice(ui, &theme, &mut cmds);
                                self.top_bar_pet_menu(ui, &mut cmds);
                            })
                            .response
                            .on_hover_text(tr("テーマ・スマホリモート・音声・ペット"));
                        } else {
                            self.top_bar_theme_menu(ui, &mut cmds);
                            self.top_bar_remote_and_voice(ui, &theme, &mut cmds);
                            self.top_bar_pet_menu(ui, &mut cmds);
                        }
                        self.top_bar_agent_controls(ui, &theme, density, &mut cmds);
                    });
                });
            });
        // ガイドツアーへ「ツールバーはここ」と申告する (非表示なら申告しないだけ)
        tutorial::anchor(ctx, AnchorId::Toolbar, bar.response.rect);

        // 起動バー (⌃1〜⌃9)。**割り当てが 0 件ならパネルごと作らない** —
        // 高さも枠線も 1px も取らせない (空のセクションは見出しごと消す)。
        self.quick_bar_ui(ctx, &theme, &mut cmds);

        // ブランチ切り替え: 走り終わったジョブの回収 → 新しい要求の実行。
        // (メニューを閉じていてもジョブは走り続けるので毎フレーム見る)
        if let Some((msg, ok)) = self.branch_nav.poll_job() {
            self.toast(msg, ok);
            if ok {
                self.after_branch_switch();
            }
        }
        if let Some(target) = self.branch_nav.take_request() {
            self.begin_branch_switch(target, ctx);
        }
        if self.branch_nav.take_review_request() {
            // 「変更をレビュー」= Git タブのレビューサブタブを開くだけ。
            self.sidebar_open = true;
            self.sidebar_tab = SidebarTab::Git;
            self.git_sub_review = true;
        }

        for c in cmds {
            self.apply_cmd(c, ctx);
        }
    }

    /// 起動バー (⌃1〜⌃9)。**割り当てが 0 件なら 1px も描かない**。
    ///
    /// 番号 → プリセットの対応は [`Self::quick_slots`] (= 純粋関数
    /// `config::quick_launch_slots`) だけが決める。並べ替え・取り外し・追加は
    /// 右クリックメニューから行い、その場で state.toml へ書く。
    pub(super) fn quick_bar_ui(&mut self, ctx: &egui::Context, theme: &Theme, cmds: &mut Vec<Cmd>) {
        let slots = self.quick_slots();
        if slots.is_empty() {
            return; // 空のセクションは高さを取らない (パネルごと作らない)
        }
        let labels: Vec<String> = slots
            .iter()
            .filter_map(|i| self.cfg.agents.get(*i))
            .map(|p| format!("{} {}", p.icon, p.name))
            .collect();
        if labels.len() != slots.len() {
            return; // 設定が壊れている間は何も描かない
        }
        let mut edit: Option<QuickBarEdit> = None;
        let mut add_req: Vec<usize> = Vec::new();
        egui::TopBottomPanel::top("zv-quick-launch")
            .exact_height(QUICK_BAR_H + 8.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.bg)
                    .inner_margin(egui::Margin::symmetric(10.0, 4.0)),
            )
            .show(ctx, |ui| {
                let label_ws: Vec<f32> = labels
                    .iter()
                    .map(|l| {
                        ui.fonts(|f| {
                            f.layout_no_wrap(
                                l.clone(),
                                egui::FontId::proportional(11.5),
                                theme.text,
                            )
                            .size()
                            .x
                        })
                    })
                    .collect();
                let plan = quick_bar_plan(ui.available_width(), &label_ws);
                // 行は必ず可用幅に収める (`quick_bar_plan` が入る個数まで削っている)。
                ui.set_max_width(plan.used_w().min(ui.available_width()));
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = QUICK_CHIP_GAP;
                    for ix in 0..plan.shown {
                        let slot = ix + 1;
                        let preset = slots[ix];
                        let hint = crate::keybinds::quick_launch_action(slot)
                            .map(|a| self.key_hint(a))
                            .unwrap_or_default();
                        let icon = self
                            .cfg
                            .agents
                            .get(preset)
                            .map(|p| p.icon.clone())
                            .unwrap_or_default();
                        let text = match plan.icons_only {
                            true => format!("{slot}{icon}"),
                            false => format!("{slot} {}", labels[ix]),
                        };
                        let btn = ui.add_sized(
                            [plan.chip_w, QUICK_BAR_H],
                            egui::Button::new(RichText::new(text).size(11.5).color(theme.text))
                                .fill(theme.panel),
                        );
                        if btn.clicked() {
                            cmds.push(Cmd::QuickLaunch(slot));
                        }
                        btn.clone().on_hover_text(trf(
                            "{name} を起動 ({key})",
                            &[("name", labels[ix].clone()), ("key", hint)],
                        ));
                        btn.context_menu(|ui| {
                            if ui.button(tr("🌿 専用ツリーで起動")).clicked() {
                                cmds.push(Cmd::QuickLaunchIsolated(slot));
                                ui.close_menu();
                            }
                            ui.separator();
                            if ix > 0 && ui.button(tr("◀ 番号を 1 つ前へ")).clicked() {
                                edit = Some(QuickBarEdit::MoveLeft(ix));
                                ui.close_menu();
                            }
                            if ix + 1 < slots.len() && ui.button(tr("番号を 1 つ後へ ▶")).clicked()
                            {
                                edit = Some(QuickBarEdit::MoveRight(ix));
                                ui.close_menu();
                            }
                            if ui.button(tr("✕ 起動バーから外す")).clicked() {
                                edit = Some(QuickBarEdit::Remove(ix));
                                ui.close_menu();
                            }
                            ui.separator();
                            for (i, p) in self.cfg.agents.iter().enumerate() {
                                if slots.contains(&i) {
                                    continue;
                                }
                                if ui
                                    .button(trf(
                                        "＋ {name} を末尾へ",
                                        &[("name", format!("{} {}", p.icon, p.name))],
                                    ))
                                    .clicked()
                                {
                                    add_req.push(i);
                                    ui.close_menu();
                                }
                            }
                            if ui.button(tr("↺ 並びを既定へ戻す")).clicked() {
                                edit = Some(QuickBarEdit::Reset);
                                ui.close_menu();
                            }
                        });
                    }
                });
            });
        if let Some(e) = edit {
            self.edit_quick_slots(e);
        }
        for i in add_req {
            self.edit_quick_slots(QuickBarEdit::Add(i));
        }
    }

    /// エージェント名のリネーム窓。**開いていないときは 1px も描かない**。
    /// ここで付けた名前は `manual_titles` に載り、自動命名が二度と上書きしない。
    pub(super) fn rename_agent_ui(&mut self, ctx: &egui::Context) {
        let Some((id, mut buf)) = self.rename_agent.clone() else {
            return;
        };
        let mut open = true;
        let mut commit = false;
        let mut cancel = false;
        // このセッション自身の CLI が命名を担えるか (別の相手へは投げない)。
        let gen = self
            .agents
            .sessions
            .iter()
            .find(|s| s.id == id)
            .and_then(|s| crate::agents::title_generator_for_command(&s.command));
        let auto_on = self.cfg.auto_name_sessions;
        egui::Window::new(tr("エージェント名の変更"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(320.0);
                let te = ui.add(
                    egui::TextEdit::singleline(&mut buf)
                        .id_salt(("zv-rename-agent", id))
                        .desired_width(ui.available_width()),
                );
                ui.label(
                    RichText::new(tr("手で付けた名前は自動命名に上書きされません"))
                        .size(10.5)
                        .weak(),
                );
                // どの CLI が自動命名を担えるか。**確認方法まで出す** —
                // 「対応」と書いてあるのに実機で確かめていない、を作らないため。
                if let Some(g) = gen {
                    ui.label(
                        RichText::new(trf(
                            "自動命名: {bin} が担当 ({state})",
                            &[
                                ("bin", g.bin.to_string()),
                                (
                                    "state",
                                    match auto_on {
                                        true => tr("有効"),
                                        false => tr("設定で無効"),
                                    },
                                ),
                            ],
                        ))
                        .size(10.5)
                        .weak(),
                    )
                    .on_hover_text(trf(
                        "非対話実行: {bin} {args}\n確認方法: {ver}",
                        &[
                            ("bin", g.bin.to_string()),
                            ("args", g.args.to_string()),
                            ("ver", g.verified.to_string()),
                        ],
                    ));
                }
                ui.horizontal(|ui| {
                    if ui.button(tr("変更")).clicked()
                        || (te.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)))
                    {
                        commit = true;
                    }
                    if ui.button(tr("やめる")).clicked() {
                        cancel = true;
                    }
                });
            });
        if commit {
            let name = buf.trim().to_string();
            if !name.is_empty() {
                if let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == id) {
                    s.title = name;
                }
                // 手動が常に勝つ: 以後この相手へは自動命名を撃たない。
                self.manual_titles.insert(id);
                self.persist_session();
            }
            self.rename_agent = None;
        } else if cancel || !open {
            self.rename_agent = None;
        } else {
            self.rename_agent = Some((id, buf));
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    //  セッションの自動命名 (cmux 由来)
    //
    //  ターンが終わった瞬間に **そのエージェント自身の CLI** へ
    //  「2〜5 語の題名を」と頼む。送るのは**ユーザーが自分で送った指示文の
    //  冒頭だけ** — コードもエージェントの出力も画面の中身も送らない。
    //  既定はオフ。失敗したら黙って従来名のまま。手動名は絶対に上書きしない。
    // ═══════════════════════════════════════════════════════════════════

    /// 毎フレームの自動命名の面倒 (結果の反映 → ターン境界の検出 → 依頼)。
    ///
    /// 既定オフのときは結果の回収だけして即戻るので、追加コストは 0
    /// (`Namer::poll` は `try_recv` を 1 回舐めるだけ)。
    pub(super) fn auto_name_tick(&mut self, ctx: &egui::Context) {
        // ① 届いた結果を先に反映する (オフに切り替えた後も取りこぼさない)。
        for n in self.namer.poll() {
            // 走らせている間に手で付けられていたら手動が勝つ。
            // 生成に失敗 (None) なら黙って従来名のまま。判断は純関数に集約。
            let manual = self.manual_titles.contains(&n.session_id);
            let mut changed = false;
            if let Some(s) = self
                .agents
                .sessions
                .iter_mut()
                .find(|s| s.id == n.session_id)
            {
                let next = apply_named_title(&s.title, manual, n.title);
                changed = next != s.title;
                s.title = next;
            }
            if changed {
                self.persist_session();
            }
        }
        if !self.cfg.auto_name_sessions {
            return; // 既定オフ: ここから先は 1 行も走らない
        }
        // ② ターン境界を見る。`output_advanced` は scan_attention が間引いて
        //    更新している値なので、ここで追加のコストは発生しない。
        let now_ms = (ctx.input(|i| i.time).max(0.0) * 1000.0) as u64;
        let mut want: Vec<(u64, &'static crate::agents::TitleGen, String)> = Vec::new();
        for s in self.agents.sessions.iter() {
            let ended = self
                .turns
                .observe(s.id, s.output_advanced(), now_ms, AUTO_NAME_QUIET_MS);
            // **そのセッション自身の CLI** しか引けない (別の相手へ投げない)。
            let gen = crate::agents::title_generator_for_command(&s.command);
            // 送るのはユーザー自身が打った指示文の冒頭だけ。
            let brief = s
                .last_prompt
                .as_deref()
                .map(crate::agents::naming::brief)
                .filter(|b| !b.is_empty());
            let sig = brief.as_deref().map(auto_name_signature);
            let go = should_auto_name(AutoNameSignals {
                enabled: self.cfg.auto_name_sessions,
                turn_ended: ended,
                running: s.running(),
                manual: self.manual_titles.contains(&s.id),
                has_generator: gen.is_some(),
                has_brief: brief.is_some(),
                already_named: sig.is_some() && self.named_for.get(&s.id) == sig.as_ref(),
            });
            if !go {
                continue;
            }
            let (Some(gen), Some(brief), Some(sig)) = (gen, brief, sig) else {
                continue;
            };
            self.named_for.insert(s.id, sig);
            want.push((s.id, gen, brief));
        }
        for (id, gen, brief) in want {
            self.namer.request(id, gen, brief, ctx.clone());
        }
    }

    /// トップバー左側: ロゴ・VS Code 準拠メニューバー・ブランチ切り替え。
    /// ボタン類は cmds に記録だけして呼び出し側で self へ反映する。
    pub(super) fn top_bar_left(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        menu_info: &menu_bar::MenuInfo,
        branch: &Option<String>,
        cmds: &mut Vec<Cmd>,
    ) {
        ui.label(
            RichText::new("⚡ ZAIVERN")
                .strong()
                .size(16.0)
                .color(theme.accent),
        );
        ui.separator();

        // VS Code と同じ 8 メニュー
        // (ファイル/編集/選択/表示/移動/実行/ターミナル/ヘルプ)
        // menu_bar::ui は Vec<Cmd> しか返さないので、矩形は scope で測る。
        let menus = ui.scope(|ui| menu_bar::ui(ui, menu_info, &self.keys));
        tutorial::anchor(ui.ctx(), AnchorId::MenuBar, menus.response.rect);
        let mut menu_cmds = menus.inner;
        cmds.append(&mut menu_cmds);

        if let Some(b) = branch {
            self.branch_button(ui, theme, b);
        }
    }

    /// トップバー: 🌿 ブランチボタン (押すと切り替えピッカーが開く)。
    ///
    /// git は **1 本も UI スレッドで動かさない**。開いている間だけ
    /// [`git::BranchNav`] が裏で一覧を取り、選んだ先は
    /// [`git::BranchSnapshot::plan_switch`] の判断を通してから別スレッドで実行する。
    pub(super) fn branch_button(&mut self, ui: &mut egui::Ui, theme: &Theme, current: &str) {
        let busy = self.branch_nav.busy();
        let label = if busy {
            format!("🌿 {current} …")
        } else {
            format!("🌿 {current} ▾")
        };
        let color = if busy { theme.warn } else { theme.text_dim };
        let menu = ui.menu_button(RichText::new(label).color(color), |ui| {
            self.branch_menu_ui(ui, theme);
        });
        let open = menu.inner.is_some();
        self.branch_nav.set_open(open);
        menu.response.on_hover_text(if busy {
            trf(
                "{b} へ切り替え中…",
                &[("b", self.branch_nav.job_label().to_string())],
            )
        } else {
            tr("ブランチを切り替え")
        });
    }

    /// ブランチピッカーの中身 (ローカル → 区切り → リモート追跡)。
    pub(super) fn branch_menu_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        ui.set_min_width(300.0);
        let ctx = ui.ctx().clone();
        // 開いている間だけ収集する (閉じていれば git は 1 本も起動しない)。
        self.branch_nav.ensure_fresh(&ctx);

        let want_focus = self.branch_nav.take_focus_request();
        let edit = ui.add(
            egui::TextEdit::singleline(&mut self.branch_nav.filter)
                .desired_width(f32::INFINITY)
                .hint_text(tr("ブランチを絞り込み")),
        );
        if want_focus {
            edit.request_focus();
        }

        // 直近の拒否理由 (汚れている / マージ中 / 別 worktree)。
        if let Some(block) = self.branch_nav.block.clone() {
            ui.add_space(2.0);
            ui.label(RichText::new(block.message()).color(theme.warn).size(11.0));
            if block.offers_review()
                && ui
                    .button(RichText::new(tr("変更をレビュー")).size(11.0))
                    .clicked()
            {
                self.branch_nav.request_review();
                ui.close_menu();
            }
        }

        let Some(snap) = self.branch_nav.snapshot() else {
            ui.add_space(4.0);
            ui.label(
                RichText::new(tr("読み込み中…"))
                    .color(theme.text_dim)
                    .size(11.0),
            );
            return;
        };

        // 押す前に分かるように、切り替えを止める条件は先に出しておく。
        let note = |ui: &mut egui::Ui, s: String| {
            ui.label(RichText::new(s).color(theme.text_dim).size(10.5));
        };
        if let Some(what) = &snap.in_progress {
            note(ui, trf("{what}の途中です", &[("what", what.clone())]));
        } else if snap.dirty_total > 0 {
            note(
                ui,
                trf(
                    "未コミットの変更が {n} 件あります (切り替えは止めます)",
                    &[("n", snap.dirty_total.to_string())],
                ),
            );
        }
        if let Some(d) = &snap.detached {
            note(ui, d.clone());
        }
        ui.separator();

        let filter = self.branch_nav.filter.clone();
        let mut chosen: Option<git::SwitchTarget> = None;
        egui::ScrollArea::vertical()
            .id_salt("zv-branch-pick")
            .max_height(320.0)
            .show(ui, |ui| {
                let mut shown = 0usize;
                for b in &snap.local {
                    if !git::matches_filter(&b.name, &filter) {
                        continue;
                    }
                    shown += 1;
                    let held = b.other_worktree || snap.holders.iter().any(|(n, _)| *n == b.name);
                    let mark = if b.current {
                        "●"
                    } else if held {
                        "⧉"
                    } else {
                        " "
                    };
                    let color = if b.current {
                        theme.accent
                    } else if held {
                        theme.text_dim
                    } else {
                        theme.text
                    };
                    let row = ui.add(
                        egui::Button::new(
                            RichText::new(format!("{mark} {}", b.name))
                                .size(11.5)
                                .color(color),
                        )
                        .frame(false),
                    );
                    let row = if held {
                        row.on_hover_text(tr("別の作業ツリーで開かれています"))
                    } else {
                        row
                    };
                    if row.clicked() && !b.current {
                        chosen = Some(git::SwitchTarget::Local(b.name.clone()));
                    }
                }

                let remotes: Vec<&String> = snap
                    .remote
                    .iter()
                    .filter(|r| git::matches_filter(r, &filter))
                    .collect();
                if !remotes.is_empty() {
                    ui.separator();
                    ui.label(
                        RichText::new(tr("リモート追跡 (選ぶと追跡ブランチを作ります)"))
                            .color(theme.text_dim)
                            .size(10.0),
                    );
                    for r in remotes {
                        shown += 1;
                        if ui
                            .add(
                                egui::Button::new(
                                    RichText::new(format!("  {r}")).size(11.5).color(theme.text),
                                )
                                .frame(false),
                            )
                            .clicked()
                        {
                            chosen = Some(git::SwitchTarget::Remote(r.clone()));
                        }
                    }
                }

                if shown == 0 {
                    ui.label(
                        RichText::new(tr("一致するブランチがありません"))
                            .color(theme.text_dim)
                            .size(11.0),
                    );
                }
            });

        // 上限で切ったことは黙らない (「無い」と「出していない」を区別する)。
        let cut = snap.local_total.saturating_sub(snap.local.len())
            + snap.remote_total.saturating_sub(snap.remote.len());
        if cut > 0 {
            ui.label(
                RichText::new(trf(
                    "最近の {n} 件だけ出しています (ほか {cut} 件は絞り込みで探してください)",
                    &[
                        ("n", git::BRANCH_LIST_CAP.to_string()),
                        ("cut", cut.to_string()),
                    ],
                ))
                .color(theme.text_dim)
                .size(10.0),
            );
        }

        if let Some(t) = chosen {
            self.branch_nav.select(t);
            // 断られたときはメニューを開いたままにして理由を読ませる。
            if self.branch_nav.block.is_none() {
                ui.close_menu();
            }
        }
    }

    /// ブランチ切り替えを開始する (判断は git.rs の純関数に任せる)。
    pub(super) fn begin_branch_switch(&mut self, target: git::SwitchTarget, ctx: &egui::Context) {
        let Some(snap) = self.branch_nav.snapshot() else {
            return;
        };
        let label = match &target {
            git::SwitchTarget::Local(n) => n.clone(),
            git::SwitchTarget::Remote(r) => r.clone(),
        };
        match snap.plan_switch(&target) {
            Ok(argv) => self.branch_nav.start_switch(argv, label, ctx),
            Err(block) => {
                self.toast(block.message(), false);
                self.branch_nav.block = Some(block);
            }
        }
    }

    /// 切り替えが成功した後に「ブランチに依存しているもの」を作り直す。
    /// どれも次のフレームで自前のバックグラウンド経路が走るだけで、
    /// ここで git を叩くことはない。
    pub(super) fn after_branch_switch(&mut self) {
        self.gitinfo.request_refresh(); // ファイルツリーの status 装飾
        self.gitinfo.invalidate_branch(); // ツールバーのブランチ名
        self.tree.invalidate(); // ファイルの中身が入れ替わる
        self.git_panel.invalidate();
        self.review.invalidate();
        self.branch_nav.invalidate();
        // ブランチが変わっても一覧の対象フォルダは同じ (このフォルダのみ) だが、
        // 会話そのものは増減し得るので走査キャッシュだけ捨てる
        self.sidebar_sessions.invalidate();
    }

    /// メニュー用のテーマ一覧を作る (同梱テーマ → カスタムテーマ JSON の順)。
    ///
    /// トップバーの 🎨 とメニューバーの「表示 > 配色テーマ」が同じ一覧を見るので、
    /// 段組みも選択状態も 2 か所で食い違わない。
    pub(super) fn theme_entries(&self) -> Vec<menu_bar::ThemeEntry> {
        let mut out: Vec<menu_bar::ThemeEntry> = theme::all()
            .into_iter()
            .map(|t| menu_bar::ThemeEntry {
                selected: t.name == self.cfg.theme,
                group: if t.dark {
                    menu_bar::ThemeGroup::Dark
                } else {
                    menu_bar::ThemeGroup::Light
                },
                name: t.name,
                label: t.label,
            })
            .collect();
        // カスタムテーマは中身を読むまで明暗が判らないので独立した段に置く。
        for (label, path) in self.custom_themes.iter().take(60) {
            out.push(menu_bar::ThemeEntry {
                name: path.clone(),
                label: format!("🔌 {label}"),
                selected: self.cfg.theme == *path,
                group: menu_bar::ThemeGroup::Custom,
            });
        }
        out
    }

    /// トップバー: 🎨 テーマ選択メニュー (プラグインのカスタムテーマ含む)。
    pub(super) fn top_bar_theme_menu(&self, ui: &mut egui::Ui, cmds: &mut Vec<Cmd>) {
        let themes = self.theme_entries();
        let menu = ui.menu_button("🎨", |ui| {
            menu_bar::theme_menu_ui(ui, &themes, cmds);
        });
        tutorial::anchor(ui.ctx(), AnchorId::ThemeMenu, menu.response.rect);
        menu.response
            .on_hover_text(tr("テーマ（プラグインのカスタムテーマも使えます）"));
    }

    /// トップバー: 📱 スマホリモートと 🎤 音声入力まわり。
    pub(super) fn top_bar_remote_and_voice(
        &self,
        ui: &mut egui::Ui,
        theme: &Theme,
        cmds: &mut Vec<Cmd>,
    ) {
        // スマホリモート (QR コード表示)。
        // Windows で受信が許可されていないと分かっているときは ⚠ を添える
        // (📱 を開かないと理由が分からない、という状態を作らないため)
        // SSH トンネル中は 127.0.0.1 でしか待ち受けないので、受信許可は無関係。
        // それでも ⚠ を出すと「直せない警告」を突きつけることになる。
        let lan_mode = self
            .remote
            .as_ref()
            .map(|r| r.bind == remote::Bind::Lan)
            .unwrap_or(true);
        let blocked = lan_mode && self.fw.needs_allow();
        let mut icon = RichText::new(if blocked { "📱⚠" } else { "📱" });
        if blocked {
            icon = icon.color(theme.warn);
        }
        let remote_btn = ui.selectable_label(self.remote_open, icon);
        tutorial::anchor(ui.ctx(), AnchorId::RemoteButton, remote_btn.rect);
        if remote_btn
            .on_hover_text(if blocked {
                tr(
                    "⚠ Windows のファイアウォールが受信をブロックしています\n\u{3000}\
                    押して開く画面のボタンで許可できます (それまでスマホからは繋がりません)",
                )
            } else {
                tr("スマホから操作 — QR コードを表示\n\
                     同じ Wi-Fi のスマホで読み取るだけで、編集・保存・\n\
                     エージェント操作(Claude の承認も)ができます\n\
                     🎤 音声入力: PC は Cockpit 各タブの 🎤 /\n\
                     ブロードキャスト欄の 🎤、スマホは「エージェント」タブ")
            })
            .clicked()
        {
            cmds.push(Cmd::ToggleRemote);
        }

        // 音声入力: 🎤 で開始、隣の ⏹ で停止。押している間だけの
        // 録音キーは無し — ボタンだけで完結する
        let rec = self.voice.session.is_some();
        if rec
            && ui
                .button(RichText::new("⏹").color(theme.err).strong())
                .on_hover_text(tr("音声入力を止める"))
                .clicked()
        {
            cmds.push(Cmd::VoiceStop);
        }
        let voice_btn = ui.selectable_label(
            rec,
            RichText::new(if rec { "🔴" } else { "🎤" }).color(if rec {
                theme.err
            } else {
                theme.text
            }),
        );
        tutorial::anchor(ui.ctx(), AnchorId::VoiceButton, voice_btn.rect);
        if voice_btn
            .on_hover_text(if rec {
                tr("録音中 — もう一度押すと止まります")
            } else {
                // この PC で実際に通る経路を先に見せる (押してから
                // 「使えません」と言われるのを避ける)
                trf(
                    "音声入力を始める\n\
                     ⏹ を押すまで、話した内容が入力欄に入り続けます\n\
                     (Enter は送られないので、確認して自分で送信)\n\
                     {hint}",
                    &[(
                        "hint",
                        voice::route_hint(
                            &self.cfg.voice_engine,
                            &self.cfg.voice_lang,
                            &self.cfg.voice_command,
                        )
                        .to_string(),
                    )],
                )
            })
            .clicked()
        {
            let t = voice::Target::from_name(&self.cfg.voice_target);
            cmds.push(Cmd::VoiceInput(t));
        }
        ui.menu_button("▾", |ui| {
            ui.label(
                RichText::new(tr("話した内容は入力欄に入るだけです。\n\
                     送信されるのは自分で Enter を押したときだけ。"))
                .size(11.0)
                .color(theme.text_dim),
            );
            ui.separator();
            if ui
                .button(if rec {
                    tr("⏹ 録音を止める")
                } else {
                    tr("🎤 いま録音する (アクティブなエージェントへ)")
                })
                .clicked()
            {
                let t = voice::Target::from_name(&self.cfg.voice_target);
                cmds.push(Cmd::VoiceInput(t));
                ui.close_menu();
            }
            ui.separator();
            // 届け先。録音中は HUD からも切り替えられる
            let cur = if rec {
                self.voice.target
            } else {
                voice::Target::from_name(&self.cfg.voice_target)
            };
            ui.label(RichText::new(tr("届け先")).size(11.0).color(theme.text_dim));
            for (t, label) in [
                (voice::Target::Active, "🎯 アクティブなエージェント"),
                (
                    voice::Target::Broadcast,
                    "📣 全エージェントへブロードキャスト",
                ),
            ] {
                if ui.radio(cur == t, tr(label)).clicked() {
                    cmds.push(Cmd::SetVoiceTarget(t));
                    ui.close_menu();
                }
            }
            ui.menu_button(
                trf("🌐 言語: {lang}", &[("lang", self.cfg.voice_lang.clone())]),
                |ui| {
                    for (code, label) in [
                        ("ja-JP", "日本語"),
                        ("en-US", "English (US)"),
                        ("zh-CN", "中文"),
                        ("ko-KR", "한국어"),
                    ] {
                        if ui.radio(self.cfg.voice_lang == code, label).clicked() {
                            cmds.push(Cmd::SetVoiceLang(code.to_string()));
                            ui.close_menu();
                        }
                    }
                },
            );
            ui.menu_button(
                if self.cfg.voice_keyword.is_empty() {
                    tr("🗣 合図で送信: なし (常に手動 Enter)")
                } else {
                    trf(
                        "🗣 合図で送信: 「{keyword}」",
                        &[("keyword", self.cfg.voice_keyword.clone())],
                    )
                },
                |ui| {
                    ui.label(
                        RichText::new(tr("この言葉で終わったときだけ Enter まで送ります"))
                            .size(11.0)
                            .color(theme.text_dim),
                    );
                    for kw in ["", "送信", "送って", "オーケー"] {
                        let sel = self.cfg.voice_keyword == kw;
                        let label = if kw.is_empty() {
                            tr("なし")
                        } else {
                            kw.to_string()
                        };
                        if ui.radio(sel, label).clicked() {
                            cmds.push(Cmd::SetVoiceKeyword(kw.to_string()));
                            ui.close_menu();
                        }
                    }
                },
            );
            ui.separator();
            ui.menu_button(
                trf(
                    "⚙ エンジン: {engine}",
                    &[("engine", self.cfg.voice_engine.clone())],
                ),
                |ui| {
                    for (v, label) in [
                        ("auto", "自動 (この OS に合わせる)"),
                        ("mac", "macOS 内蔵の音声認識"),
                        ("powershell", "Windows 標準の音声認識"),
                        ("browser", "ブラウザの音声入力ページ"),
                        ("command", "外部コマンド (config.toml の voice_command)"),
                        ("off", "無効"),
                    ] {
                        if ui.radio(self.cfg.voice_engine == v, tr(label)).clicked() {
                            cmds.push(Cmd::SetVoiceEngine(v.to_string()));
                            ui.close_menu();
                        }
                    }
                },
            );
        })
        .response
        .on_hover_text(tr("音声入力 — キーを押している間だけ録音し、\n\
             認識テキストをエージェントの入力欄へ挿入します。\n\
             Enter は送られないので、確認してから自分で送信できます。"));
    }

    /// トップバー: 🐾 ペットメニュー (表示切替・画像変更)。
    pub(super) fn top_bar_pet_menu(&self, ui: &mut egui::Ui, cmds: &mut Vec<Cmd>) {
        // ペットメニュー(表示切替・画像変更)
        ui.menu_button("🐾", |ui| {
            let show = self.cfg.show_pet;
            if ui
                .selectable_label(
                    show,
                    if show {
                        tr("🐾 表示中")
                    } else {
                        tr("🐾 非表示")
                    },
                )
                .clicked()
            {
                cmds.push(Cmd::TogglePet);
                ui.close_menu();
            }
            ui.separator();
            if ui.button(tr("🖼 画像を変更…")).clicked() {
                cmds.push(Cmd::SetPetImage);
                ui.close_menu();
            }
            if ui.button(tr("↺ 既定の絵に戻す")).clicked() {
                cmds.push(Cmd::ResetPetImage);
                ui.close_menu();
            }
            if ui.button(tr("🐾 位置を右下に戻す")).clicked() {
                cmds.push(Cmd::ResetPetPos);
                ui.close_menu();
            }
            ui.separator();
            // 見た目バリアント(ラジオ選択。候補は pet::PetVariant から生成)
            ui.menu_button(tr("🎭 見た目"), |ui| {
                for (v, label) in [
                    (pet::PetVariant::Blocky, "🟦 ブロック"),
                    (pet::PetVariant::Crab, "🐾 カニ"),
                    (pet::PetVariant::Cat, "🐱 ネコ"),
                    (pet::PetVariant::Cloud, "☁ クラウド"),
                ] {
                    if ui
                        .radio(self.cfg.pet_variant == v.name(), tr(label))
                        .clicked()
                    {
                        cmds.push(Cmd::SetPetVariant(v.name().to_string()));
                        ui.close_menu();
                    }
                }
            });
            // 表示スケール(ラジオ選択)
            ui.menu_button(tr("📏 サイズ"), |ui| {
                for (v, label) in [(0.75f32, "小"), (1.0, "中"), (1.4, "大")] {
                    let sel = (self.cfg.pet_scale - v).abs() < 0.01;
                    if ui.radio(sel, tr(label)).clicked() {
                        cmds.push(Cmd::SetPetScale(v));
                        ui.close_menu();
                    }
                }
            });
            ui.separator();
            // 挙動の切替(チェックボックス。cfg は apply_cmd 側で保存)
            let mut roam = self.cfg.pet_free_roam;
            if ui.checkbox(&mut roam, tr("🚶 うろうろ散歩")).clicked() {
                cmds.push(Cmd::TogglePetFreeRoam);
            }
            let mut sleep = self.cfg.pet_sleep;
            if ui.checkbox(&mut sleep, tr("💤 居眠り")).clicked() {
                cmds.push(Cmd::TogglePetSleep);
            }
            let mut sounds = self.cfg.pet_sounds;
            if ui
                .checkbox(&mut sounds, tr("🔔 効果音"))
                .on_hover_text(tr("ペット自身の音 (ホップ等)。通知音とは別です"))
                .clicked()
            {
                cmds.push(Cmd::TogglePetSounds);
            }
            // 通知音は `Config` の機能設定 (notifications.sound) が真実源。
            // ここは設定画面 (⚙) の行への近道であって、別の状態ではない
            // (`set_notify_sound` が同じ書き戻し経路を通る)。
            let mut notify_sound = self.notify_sound_enabled();
            if ui
                .checkbox(&mut notify_sound, tr("🔊 通知音"))
                .on_hover_text(tr(
                    "OS 通知に音を付けるか。⚙ 設定の「通知音を鳴らす」と同じ設定です",
                ))
                .clicked()
            {
                cmds.push(Cmd::Feature(
                    crate::features::notifications::ID_TOGGLE_SOUND,
                ));
            }
            let mut bubbles = self.cfg.pet_bubbles;
            if ui.checkbox(&mut bubbles, tr("💬 承認バブル")).clicked() {
                cmds.push(Cmd::TogglePetBubbles);
            }
            let mut auto_yes = self.cfg.pet_auto_yes;
            if ui.checkbox(&mut auto_yes, tr("⚡ 自動YES")).clicked() {
                cmds.push(Cmd::TogglePetAutoYes);
            }
        })
        .response
        .on_hover_text(tr("デスクトップペット 🐾 の表示・画像変更"));
    }

    /// トップバー: エージェント関連 (権限一括切替・既定承認モード・Cockpit・
    /// エージェント起動・コマンドパレット・稼働数表示)。
    pub(super) fn top_bar_agent_controls(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        density: TopBarDensity,
        cmds: &mut Vec<Cmd>,
    ) {
        let compact = density.compact();
        // 実行中の対応エージェントを一括で権限モード切替
        if self.agents.running_count() > 0
            && ui
                .button(
                    RichText::new(if compact {
                        "🛡".to_string()
                    } else {
                        tr("🛡 全切替")
                    })
                    .color(theme.ok),
                )
                .on_hover_text(tr(
                    "実行中の Claude/Codex/Antigravity に権限モード切替を送信します。\n\
                     Claude/Antigravity は Shift+Tab、Codex は /permissions を送ります",
                ))
                .clicked()
        {
            cmds.push(Cmd::CyclePermissionAll);
        }

        // 承認モード切替(次回起動の既定)。クリックで 承認→全自動→Agent優先 を順送り。
        // **いちばん狭いときは出さない** — 重なって読めないより出さない方が事故が
        // 少ない (⌘P のコマンドパレットから同じ切替ができる)。
        let mode = self.cfg.approval_mode.as_str();
        let (ap_label, next_mode, highlight) = match mode {
            "auto" => (
                RichText::new(tr("⚡ 既定:全自動"))
                    .color(theme.warn)
                    .strong(),
                "agent",
                true,
            ),
            "agent" => (
                RichText::new(tr("👾 既定:Agent優先"))
                    .color(theme.ok)
                    .strong(),
                "ask",
                true,
            ),
            _ => (
                RichText::new(tr("🛡 既定:承認")).color(theme.ok),
                "auto",
                false,
            ),
        };
        if density != TopBarDensity::Overflow {
            // 狭いときは絵文字だけ残す (色と絵文字でモードは判別できる)
            let ap_label = if compact {
                let icon: String = ap_label.text().chars().take(1).collect();
                RichText::new(icon).color(match mode {
                    "auto" => theme.warn,
                    "agent" => theme.ok,
                    _ => theme.ok,
                })
            } else {
                ap_label
            };
            let perm_btn = ui.selectable_label(highlight, ap_label);
            tutorial::anchor(ui.ctx(), AnchorId::PermissionMode, perm_btn.rect);
            if perm_btn
            .on_hover_text(tr(
                "「次に起動する」エージェント (Claude/Codex/Antigravity) の既定権限モード\n\
                 🛡 承認 = 操作のたびに許可が必要（bypass フラグを除去）\n\
                 ⚡ 全自動 = すべて自動YES（bypass フラグを付与）\n\
                 👾 Agent優先 = Agent欄プリセットのコマンドどおり（(全自動) プリセットのみ自動YES）\n\
                 クリックで 承認→全自動→Agent優先 の順に切替\n\
                 ※ 実行中のセッションは各行の 🛡 ボタンで個別に切替できます",
            ))
            .clicked()
        {
            cmds.push(Cmd::SetApproval(next_mode.into()));
        }
        }

        let cockpit = ui.selectable_label(
            self.cockpit,
            RichText::new(if compact { "🎛" } else { "🎛 Cockpit" }),
        );
        tutorial::anchor(ui.ctx(), AnchorId::CockpitButton, cockpit.rect);
        if cockpit
            .on_hover_text(trf(
                "全エージェント一覧 ({key})",
                &[("key", self.key_hint(BindAction::ToggleCockpit))],
            ))
            .clicked()
        {
            cmds.push(Cmd::ToggleCockpit);
        }

        let kanban = ui.selectable_label(
            self.kanban,
            RichText::new(if compact {
                "📋".to_string()
            } else {
                tr("📋 看板")
            }),
        );
        tutorial::anchor(ui.ctx(), AnchorId::KanbanButton, kanban.rect);
        if kanban
            .on_hover_text(trf(
                "フリート看板 — 全エージェントの状況を俯瞰 ({key})",
                &[("key", self.key_hint(BindAction::ToggleKanban))],
            ))
            .clicked()
        {
            cmds.push(Cmd::ToggleKanban);
        }

        // エージェントデッキ (縦 1 本)。Cockpit=格子 / 看板=レーン と並べて
        // 「もう 1 つの見方」として同じ場所から選べるようにする。
        let deck = ui.selectable_label(
            self.deck,
            RichText::new(if compact {
                "▤".to_string()
            } else {
                tr("▤ デッキ")
            }),
        );
        tutorial::anchor(ui.ctx(), AnchorId::DeckButton, deck.rect);
        if deck
            .on_hover_text(trf(
                "エージェントデッキ — 稼働中と過去のセッションを縦 1 本で管理 ({key})",
                &[("key", self.key_hint(BindAction::ToggleDeck))],
            ))
            .clicked()
        {
            cmds.push(Cmd::ToggleDeck);
        }

        let new_agent = ui.menu_button(if compact { "👾＋" } else { "👾 Agent ＋" }, |ui| {
            for (i, p) in self.cfg.agents.clone().into_iter().enumerate() {
                if ui.button(format!("{} {}", p.icon, p.name)).clicked() {
                    cmds.push(Cmd::NewAgent(i));
                    ui.close_menu();
                }
            }
            // ── worktree 隔離で起動 ────────────────────────────────
            // 同じ作業ツリーを共有させないので、ファイルの取り合いが起きない。
            // worktree は git の機能なので、git リポジトリでなければ選べない
            // (理由はホバーで出す — 押せないボタンを無言で置かない)。
            ui.separator();
            let isolated_label = tr("🌿 worktree 隔離で起動…");
            if worktree::looks_like_git_repo(&self.agent_cwd()) {
                let m = ui.menu_button(isolated_label, |ui| {
                    for (i, p) in self.cfg.agents.clone().into_iter().enumerate() {
                        if ui.button(format!("{} {}", p.icon, p.name)).clicked() {
                            cmds.push(Cmd::NewAgentIsolated(i));
                            ui.close_menu();
                        }
                    }
                });
                m.response.on_hover_text(tr(
                    "このエージェント専用の git worktree (ブランチ agent/…) を切って、\n\
                     そこを作業フォルダにして起動します。他のエージェントと\n\
                     同じファイルを取り合いません",
                ));
            } else {
                ui.add_enabled(false, egui::Button::new(isolated_label))
                    .on_disabled_hover_text(tr(
                        "このフォルダは git リポジトリではないので worktree を作れません",
                    ));
            }
            // 稼働中が 1 体も居ないときは 1 行も使わない (常に出るだけのボタンを作らない)。
            if self.agents.running_count() > 0
                && ui
                    .button(tr("🛑 全エージェントを停止…"))
                    .on_hover_text(tr(
                        "稼働中のエージェントをプロセスツリーごと止めます（確認あり）",
                    ))
                    .clicked()
            {
                cmds.push(Cmd::StopAllAgents);
                ui.close_menu();
            }

            ui.separator();
            // エージェントと同じ場所から呼び出せる「指揮統制の看板」。
            if ui
                .button(tr("📋 フリート看板 — 全員の状況を俯瞰"))
                .on_hover_text(tr("エージェントをカンバン方式で指揮統制する画面を開く"))
                .clicked()
            {
                cmds.push(Cmd::ToggleKanban);
                ui.close_menu();
            }
            if ui
                .button(tr("➕ エージェントを追加…"))
                .on_hover_text(tr("対応している CLI エージェントの一覧から選んで足す"))
                .clicked()
            {
                cmds.push(Cmd::OpenAgentPicker);
                ui.close_menu();
            }
        });
        tutorial::anchor(ui.ctx(), AnchorId::NewAgentButton, new_agent.response.rect);
        new_agent.response.on_hover_text(trf(
            "エージェントを起動 ({key})",
            &[("key", self.key_hint(BindAction::NewAgent))],
        ));

        if ui
            .button("🔍")
            .on_hover_text(trf(
                "コマンドパレット ({files} / {cmds})",
                &[
                    ("files", self.key_hint(BindAction::PaletteFiles)),
                    ("cmds", self.key_hint(BindAction::PaletteCommands)),
                ],
            ))
            .clicked()
        {
            self.palette.open_files();
        }

        let running = self.agents.running_count();
        if running > 0 {
            ui.label(
                RichText::new(if compact {
                    format!("●{running}")
                } else {
                    trf("● {running} 稼働中", &[("running", running.to_string())])
                })
                .color(theme.ok),
            )
            .on_hover_text(trf("{n} 稼働中", &[("n", running.to_string())]));
        }
    }

    // ─── UI: status bar ─────────────────────────────────────────────

    pub(super) fn status_bar(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let branch = self.git_branch();
        self.gitinfo.refresh_if_stale();
        let dirty = self.gitinfo.dirty_count();
        let mut toggle_cockpit = false;
        let mut open_quota = false;
        // クロージャ内で self を再度借りないよう、要るものは先に取り出す
        let (quota_sev, quota_tip) = self.quota_status();
        let fmt_label = self.text_format_label();
        let mut convert_eol: Option<crate::textenc::LineEnding> = None;
        // ステータスバーのインデントメニューで選ばれたもの
        // (クロージャの中では記録だけして、パネル描画後に self へ反映する)
        let mut indent_action: Option<IndentAction> = None;
        // 統合承認キューの待ち件数バッジ (押すとボトムパネルの承認ビューを開く)
        let approvals_pending = self.agents.approvals.pending_len();
        let mut open_approvals = false;
        // 🎯 追従バッジ — **追従しているときだけ**出す。追従は画面が勝手に
        // 動く唯一の機能なので、「いま誰を追っているか」は常に見えていること。
        let follow_badge: Option<(String, bool)> = self.follow.target().and_then(|id| {
            self.agents.sessions.iter().find(|s| s.id == id).map(|s| {
                let icon = if s.icon.is_empty() {
                    "👾"
                } else {
                    s.icon.as_str()
                };
                (format!("{icon} {}", s.title), self.follow.is_paused())
            })
        });
        // いま追っている場所 (ファイル:行)。まだ 1 度も飛んでいなければ空。
        let follow_at: String = self
            .follow
            .spot()
            .map(|sp| {
                let name = sp
                    .path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| sp.path.display().to_string());
                format!("{name}:{}", sp.line)
            })
            .unwrap_or_default();
        let mut follow_click = false;
        let follow_keys = (
            self.key_hint(BindAction::FollowAgent),
            self.key_hint(BindAction::FollowResume),
        );
        // ◆ 未読バッジ — 0 件のときは 1 ピクセルも描かない。
        let unread_count = self
            .agents
            .sessions
            .iter()
            .filter(|s| s.has_unread())
            .count();
        let unread_key = self.key_hint(BindAction::NextUnread);
        let mut jump_unread = false;
        // トークン消費と推定コスト。消費ゼロなら None = 1 ピクセルも出さない。
        let token_badges = self.token_badges();
        let want_token_detail = self.token_detail;
        let mut toggle_token_detail = false;
        // コスト上限。**上限を設定していなければ None = 1 ピクセルも出さない**。
        let cost_badge = self.cost_badge();
        let mut open_cost_settings = false;
        // Pro の解錠判定は license::is_pro **1 か所だけ**を通す。
        // 未ライセンス時は 1 ピクセルも出さない (常に何かを表示するバッジは作らない)。
        let pro_badge = license::is_pro(&self.license_status).then(|| match &self.license_status {
            license::LicenseStatus::Valid { sub, exp, .. } => trf(
                "Pro ライセンス — {sub} ・ 期限 {exp}",
                &[
                    ("sub", sub.clone()),
                    (
                        "exp",
                        exp.map(license::format_unix_date)
                            .unwrap_or_else(|| tr("無期限")),
                    ),
                ],
            ),
            _ => tr("Pro ライセンス"),
        });
        // ズームのバッジ。**等倍のときは 1 ピクセルも出さない** —
        // 常に "100%" が居座ると、変わっていないことを見張るための場所になる。
        let ui_zoom = self.cfg.ui_zoom;
        let file_zoom = self.file_zoom();
        let mut zoom_cmd: Option<Cmd> = None;
        let bar = egui::TopBottomPanel::bottom("zv-status")
            .exact_height(26.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.panel_alt)
                    .inner_margin(egui::Margin::symmetric(10.0, 4.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_centered(|ui| {
                    let dim = |s: String| RichText::new(s).size(11.5).color(theme.text_dim);
                    ui.label(dim(format!("📂 {}", roots_label(&self.roots))));
                    if let Some(b) = &branch {
                        ui.label(dim(format!("🌿 {b}")));
                        if dirty > 0 {
                            ui.label(
                                RichText::new(format!("±{dirty}"))
                                    .size(11.5)
                                    .color(theme.warn),
                            );
                        }
                    }
                    // プラグインが set_status で出した文字列
                    if !self.plugin_status.trim().is_empty() {
                        ui.label(
                            RichText::new(format!(
                                "🔌 {}",
                                notify::truncate_chars(self.plugin_status.trim(), 80)
                            ))
                            .size(11.5)
                            .color(theme.ok),
                        );
                    }

                    // 承認待ちバッジ — 0 件のときは 1 ピクセルも出さない
                    // (「何も起きていないのに赤い印がある」を作らないため)
                    if approvals_pending > 0
                        && ui
                            .button(
                                RichText::new(format!("🛡 {approvals_pending}"))
                                    .size(11.5)
                                    .color(theme.warn)
                                    .strong(),
                            )
                            .on_hover_text(tr(
                                "承認待ちの要求があります — 押すと承認キューを開きます",
                            ))
                            .clicked()
                    {
                        open_approvals = true;
                    }

                    // 🎯 追従中だけ出るバッジ。押すと (追従中) 解除 / (停止中) 再開。
                    if let Some((who, paused)) = &follow_badge {
                        let (mark, col) = if *paused {
                            ("⏸", theme.warn)
                        } else {
                            ("🎯", theme.accent)
                        };
                        let mut tip = if *paused {
                            trf(
                                "{who} の追従は一時停止中 (自分でスクロールしたため) — 再開: {key}",
                                &[("who", who.clone()), ("key", follow_keys.1.clone())],
                            )
                        } else {
                            trf(
                                "{who} を追従中 — 解除: {key}",
                                &[("who", who.clone()), ("key", follow_keys.0.clone())],
                            )
                        };
                        if !follow_at.is_empty() {
                            tip.push('\n');
                            tip.push_str(&trf("直近の編集: {at}", &[("at", follow_at.clone())]));
                        }
                        if ui
                            .button(
                                RichText::new(format!(
                                    "{mark} {}",
                                    notify::truncate_chars(who, 20)
                                ))
                                .size(11.5)
                                .color(col),
                            )
                            .on_hover_text(tip)
                            .clicked()
                        {
                            follow_click = true;
                        }
                    }

                    // ◆ 未読バッジ — 0 件のときは 1 ピクセルも描かない
                    if unread_count > 0
                        && ui
                            .button(
                                RichText::new(format!("◆ {unread_count}"))
                                    .size(11.5)
                                    .color(theme.accent)
                                    .strong(),
                            )
                            .on_hover_text(trf(
                                "未読のエージェントが {n} 件 — 押すと次の未読へ ({key})",
                                &[("n", unread_count.to_string()), ("key", unread_key.clone())],
                            ))
                            .clicked()
                    {
                        jump_unread = true;
                    }

                    // トークン消費 / 推定コスト。
                    // 「どの幅でも見切れない」ための判断は純粋関数
                    // [`quota::token_badge_layout`] に任せ、ここは結果に従うだけ。
                    if let Some(badges) = &token_badges {
                        let widths: Vec<f32> = badges
                            .detail
                            .iter()
                            .map(|(t, _)| badge_width_px(t))
                            .collect();
                        // `available_width` は行の残り全部を返すが、この後に
                        // 右詰めの列 (テーマ / 行桁 / Pro …) が同じ行へ入る。
                        // 全部を使い切ると右側と食い合うので、左側の取り分だけを
                        // 予算として渡す。
                        let budget = ui.available_width() * TOKEN_BADGE_MAX_FRACTION;
                        let lay = coordinator::quota::token_badge_layout(
                            (budget, ui.available_height()),
                            &widths,
                            badge_width_px(&badges.compact.0),
                            TOKEN_BADGE_H,
                            TOKEN_BADGE_GAP,
                            want_token_detail,
                        );
                        if lay.visible {
                            let shown: Vec<&(String, String)> = if lay.compact {
                                vec![&badges.compact]
                            } else {
                                badges.detail.iter().take(lay.rects.len()).collect()
                            };
                            for (text, tip) in shown {
                                let r = ui.add(
                                    egui::Label::new(
                                        RichText::new(text).size(11.5).color(theme.text_dim),
                                    )
                                    .sense(egui::Sense::click()),
                                );
                                if r.on_hover_text(tip.clone()).clicked() {
                                    toggle_token_detail = true;
                                }
                            }
                        }
                    }

                    // コスト上限。上限が未設定なら `cost_badge` が None なので
                    // ここは丸ごと飛ぶ (常に 0 を出すバッジを作らない)。
                    // 幅の判断はトークンバッジと同じ作法 — この後に右詰めの列
                    // (テーマ / 行桁 / Pro …) が同じ行へ入るので、残り全部では
                    // なく左側の取り分だけを予算にする (どの幅でも見切れない)。
                    if let Some((text, tip, color)) = &cost_badge {
                        let budget = ui.available_width() * TOKEN_BADGE_MAX_FRACTION;
                        if budget >= badge_width_px(text) {
                            let r = ui.add(
                                egui::Label::new(RichText::new(text).size(11.5).color(*color))
                                    .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text(tip.clone()).clicked() {
                                open_cost_settings = true;
                            }
                        }
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.label(dim("Zaivern v0.2".into()));
                        if let Some(tip) = &pro_badge {
                            ui.label(RichText::new("✨ Pro").size(11.5).color(theme.accent))
                                .on_hover_text(tip.clone());
                        }
                        if let Some(r) = &self.remote {
                            ui.label(dim(format!("📱 :{}", r.port)));
                        }
                        let (ap_text, ap_color) = match self.cfg.approval_mode.as_str() {
                            "auto" => ("⚡ 全自動", theme.warn),
                            "agent" => ("👾 Agent優先", theme.ok),
                            _ => ("🛡 承認", theme.ok),
                        };
                        ui.label(RichText::new(tr(ap_text)).size(11.5).color(ap_color));
                        ui.label(dim(self.theme.label.clone()));
                        // ズームは等倍から外れているときだけ出す。押せば元に戻る。
                        // 画面全体とファイル単位は別のバッジにする — どちらが
                        // 効いているのか分からないまま倍率だけ出ても直せない。
                        if !zoom::is_default(file_zoom) {
                            let r = ui.add(
                                egui::Label::new(
                                    RichText::new(format!("🔎 {}", zoom::label(file_zoom)))
                                        .size(11.5)
                                        .color(theme.accent),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text(tr("このファイルだけのズーム — 押すと解除します"))
                                .clicked()
                            {
                                zoom_cmd = Some(Cmd::FileZoomReset);
                            }
                        }
                        if !zoom::is_default(ui_zoom) {
                            let r = ui.add(
                                egui::Label::new(
                                    RichText::new(format!("🔍 {}", zoom::label(ui_zoom)))
                                        .size(11.5)
                                        .color(theme.accent),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text(tr("画面全体のズーム — 押すと 100% に戻します"))
                                .clicked()
                            {
                                zoom_cmd = Some(Cmd::ZoomReset);
                            }
                        }
                        // 文字サイズ倍率も等倍のときは 1 ピクセルも描かない
                        // (常に 100% と出るバッジを置かない)。
                        let text_scale = zoom::clamp(self.cfg.text_scale);
                        if !zoom::is_default(text_scale) {
                            let r = ui.add(
                                egui::Label::new(
                                    RichText::new(format!("🔠 {}", zoom::label(text_scale)))
                                        .size(11.5)
                                        .color(theme.accent),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text(tr(
                                "文字サイズ (レイアウトは変えていません) — 押すと 100% に戻します",
                            ))
                            .clicked()
                            {
                                zoom_cmd = Some(Cmd::TextSizeReset);
                            }
                        }
                        let (ln, col) = self.editor.cursor;
                        if let Some(i) = self.editor.active {
                            ui.label(dim(format!("Ln {ln}, Col {col}")));
                            ui.label(dim(self.editor.buffers[i].lang.clone()));
                            // 読み取り専用 (巨大ファイル / 差分タブ) は必ず見せる。
                            // 打っても入らない理由が分からないのが一番困るため。
                            if self.editor.buffers[i].read_only() {
                                ui.label(
                                    RichText::new(tr("🔒 読み取り専用"))
                                        .size(11.5)
                                        .color(theme.warn),
                                );
                            }
                            // ブックマーク件数 (0 件のときは出さない)
                            let bm = self.editor.buffers[i].bookmarks.len();
                            if bm > 0 {
                                ui.label(dim(format!("◆ {bm}")));
                            }
                            // インデント (VS Code の「スペース: 4」)。押すと切替。
                            // 打ち込めないタブ (差分 / 画像 / 巨大ファイル) では
                            // 変えても意味が無いので 1 ピクセルも出さない。
                            if !self.editor.buffers[i].read_only() {
                                let ind = self.editor.buffers[i].indent;
                                let label = trf(
                                    "{kind}: {n}",
                                    &[
                                        ("kind", tr(if ind.tabs { "タブ" } else { "スペース" })),
                                        ("n", ind.width.to_string()),
                                    ],
                                );
                                ui.menu_button(
                                    RichText::new(label).size(11.5).color(theme.text_dim),
                                    |ui| {
                                        ui.set_min_width(240.0);
                                        indent_menu_ui(ui, ind, &mut indent_action);
                                    },
                                )
                                .response
                                .on_hover_text(tr(
                                    "このタブのインデント — 押すと切り替えます\n\u{3000}\
                                     (開いたときに本文から推定しています)",
                                ));
                            }
                            if self.format_on_save {
                                ui.label(dim(tr("🛠 保存時に整形")));
                            }
                            // 「UTF-8 / CRLF」— 何で読んだか & 何で保存されるか。
                            // 押すと改行コードの変換メニューが出る (VS Code と同じ位置)。
                            if let Some(label) = &fmt_label {
                                let enc = self.editor.buffers[i].encoding;
                                let color = if enc.is_legacy() {
                                    theme.warn
                                } else {
                                    theme.text_dim
                                };
                                ui.menu_button(
                                    RichText::new(label.clone()).size(11.5).color(color),
                                    |ui| {
                                        ui.set_min_width(200.0);
                                        ui.label(RichText::new(tr("改行コードを変換")).strong());
                                        for (le, name) in [
                                            (crate::textenc::LineEnding::Lf, "LF (Unix)"),
                                            (crate::textenc::LineEnding::Crlf, "CRLF (Windows)"),
                                            (crate::textenc::LineEnding::Cr, "CR (旧 Mac)"),
                                        ] {
                                            if ui.button(tr(name)).clicked() {
                                                convert_eol = Some(le);
                                                ui.close_menu();
                                            }
                                        }
                                    },
                                )
                                .response
                                .on_hover_text(tr(
                                    "この文字コード・改行コードのまま保存します\n\u{3000}\
                                     (表せない文字を足したときだけ UTF-8 で保存します)",
                                ));
                            }
                        }
                        // LSP 診断件数
                        let (derr, dwarn) = self.diag_counts;
                        if derr > 0 {
                            ui.label(
                                RichText::new(format!("⛔ {derr}"))
                                    .size(11.5)
                                    .color(theme.err),
                            );
                        }
                        if dwarn > 0 {
                            ui.label(
                                RichText::new(format!("⚠ {dwarn}"))
                                    .size(11.5)
                                    .color(theme.warn),
                            );
                        }
                        // プラン使用量 (最も深刻な助言の色。押すと明細)
                        if let Some(sev) = quota_sev {
                            let r = ui.add(
                                egui::Label::new(
                                    RichText::new(quota_severity_icon(sev)).size(11.5).color(
                                        match sev {
                                            0 => theme.text_dim,
                                            1 => theme.warn,
                                            _ => theme.err,
                                        },
                                    ),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text(quota_tip.clone()).clicked() {
                                open_quota = true;
                            }
                        }
                        let total = self.agents.sessions.len();
                        let running = self.agents.running_count();
                        if total > 0 {
                            let r = ui.add(
                                egui::Label::new(
                                    RichText::new(format!("👾 {running}/{total}"))
                                        .size(11.5)
                                        .color(if running > 0 {
                                            theme.ok
                                        } else {
                                            theme.text_dim
                                        }),
                                )
                                .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text(tr("Cockpit を開く")).clicked() {
                                toggle_cockpit = true;
                            }
                        }
                    });
                });
            });
        tutorial::anchor(ctx, AnchorId::StatusBar, bar.response.rect);

        if open_approvals {
            self.open_approvals_panel();
        }
        if toggle_cockpit {
            self.cockpit = !self.cockpit;
        }
        if follow_click {
            if self.follow.is_paused() {
                self.resume_follow_agent();
            } else {
                self.toggle_follow_agent();
            }
        }
        if jump_unread {
            self.jump_next_unread();
        }
        if open_quota {
            self.quota_open = true;
        }
        if toggle_token_detail {
            self.token_detail = !self.token_detail;
        }
        if open_cost_settings {
            // バッジから上限の編集へ 1 クリックで届くようにする
            // (「なぜ止まっているのか」から「どこを直すのか」へ迷わせない)。
            self.open_cost_settings();
        }
        if let Some(le) = convert_eol {
            self.editor_op(ctx, EditOp::NormalizeEol(le));
        }
        if let Some(a) = indent_action {
            self.apply_indent_action(a, ctx);
        }
        if let Some(c) = zoom_cmd {
            self.apply_cmd(c, ctx);
        }
        self.quota_window_ui(ctx);
    }

    /// ステータスバー用のプラン使用量サマリ。
    /// 返り値: (最悪の深刻さ。アカウントが 1 件も無ければ None, ツールチップ本文)。
    /// ステータスバーへ出すトークン/コストの材料。
    ///
    /// **消費がゼロなら `None`** — 「変化していないものは 1px も出さない」。
    /// 出すのは集計値だけで、プロンプト本文は一切載せない。
    pub(super) fn token_badges(&self) -> Option<TokenBadges> {
        use coordinator::quota;
        let per_agent = self.quota.tokens();
        if per_agent.is_empty() {
            return None;
        }
        let prices = &self.cfg.pricing;
        let cur = prices.currency.clone();
        let total = self.quota.tokens_total()?;
        if total.is_zero() {
            return None;
        }
        let window = tr("直近 24 時間");
        // 金額は単価表が有効なときだけ。無効なら「推定不可」で埋めない。
        let money = |est: &quota::CostEstimate| -> Option<String> {
            prices.enabled.then(|| est.label(&cur))
        };
        let breakdown = |u: &quota::TokenUsage| {
            trf(
                "入力 {i} / 出力 {o} / キャッシュ書 {cw} / キャッシュ読 {cr}",
                &[
                    ("i", quota::short_tokens(u.input)),
                    ("o", quota::short_tokens(u.output)),
                    ("cw", quota::short_tokens(u.cache_write)),
                    ("cr", quota::short_tokens(u.cache_read)),
                ],
            )
        };
        let total_cost = self.quota.cost_total(prices)?;
        let mut compact_text = format!("🪙 {}", quota::short_tokens(total.total()));
        if let Some(m) = money(&total_cost) {
            compact_text.push_str(&format!(" · {m}"));
        }
        // ホバーで初めて内訳。行そのものは短く保つ。
        let mut tip = vec![
            trf("トークン消費 ({window})", &[("window", window.clone())]),
            breakdown(&total),
        ];
        for a in per_agent {
            let est = quota::estimate_cost(a, prices);
            let mut line = format!("  {} {}", a.label, quota::short_tokens(a.total.total()));
            if let Some(m) = money(&est) {
                line.push_str(&format!(" · {m}"));
            }
            if a.truncated {
                line.push_str(&tr(" (読み切れず・実際はこれ以上)"));
            }
            tip.push(line);
        }
        if prices.enabled {
            tip.push(tr(
                "金額は推定です (単価は設定 [pricing] から。通信はしません)",
            ));
        }
        tip.push(tr("押すとエージェント別の表示に切り替わります"));
        let compact_tip = tip.join("\n");

        let detail = per_agent
            .iter()
            .map(|a| {
                let est = quota::estimate_cost(a, prices);
                let mut text = format!("🪙 {} {}", a.label, quota::short_tokens(a.total.total()));
                if let Some(m) = money(&est) {
                    text.push_str(&format!(" · {m}"));
                }
                let mut t = vec![
                    trf(
                        "{label} — {n} 回のやり取り ({window})",
                        &[
                            ("label", a.label.clone()),
                            ("n", a.turns.to_string()),
                            ("window", window.clone()),
                        ],
                    ),
                    breakdown(&a.total),
                ];
                for (model, u) in &a.by_model {
                    let name = if model.is_empty() {
                        tr("(モデル不明)")
                    } else {
                        model.clone()
                    };
                    t.push(format!("  {name}: {}", quota::short_tokens(u.total())));
                }
                if !est.is_complete() {
                    t.push(trf(
                        "単価が設定に無いモデルがあります: {models}",
                        &[("models", est.unknown_models.join(", "))],
                    ));
                }
                t.push(tr("押すと合算表示に戻ります"));
                (text, t.join("\n"))
            })
            .collect();

        Some(TokenBadges {
            compact: (compact_text, compact_tip),
            detail,
        })
    }

    /// ステータスバーのコスト上限バッジ `(本文, ツールチップ, 色)`。
    ///
    /// **上限が 1 つも設定されていなければ `None`** — そのとき画面には
    /// 1 ピクセルも出ない (常に 0 を表示するバッジを作らない)。
    pub(super) fn cost_badge(&self) -> Option<(String, String, egui::Color32)> {
        use coordinator::quota::{BudgetState, LimitAction};
        let st = self.cost_alert.as_ref()?;
        let cur = &self.cfg.pricing.currency;
        let (icon, color) = match st.state {
            BudgetState::Over => ("⛔", self.theme.err),
            BudgetState::Warn => ("⚠", self.theme.warn),
            BudgetState::Normal => ("💰", self.theme.text_dim),
        };
        let text = format!("{icon} {}", st.short_label(cur));
        let limits = self.cfg.cost_limits();
        let (session, today) = self.cost_spent;
        let mut tip = vec![
            trf(
                "コスト上限 — {scope} ({pct}%)",
                &[
                    ("scope", st.kind.label()),
                    ("pct", ((st.fraction() * 100.0).round() as i64).to_string()),
                ],
            ),
            // 設定してある上限だけを並べる (未設定の行は出さない)。
            // 「今の消費」は 2 つとも出す — どちらで引っかかったのかが分かる。
        ];
        for s in limits.evaluate(session, today) {
            tip.push(format!("  {} {}", s.kind.label(), s.short_label(cur)));
        }
        tip.push(tr(
            "金額は推定です (単価は設定 [pricing] から。通信はしません)",
        ));
        if limits.action == LimitAction::Stop {
            tip.push(tr("上限に達している間は新規の送信を止めます (設定: stop)"));
        }
        tip.push(tr("押すと上限の設定を開きます"));
        Some((text, tip.join("\n"), color))
    }

    /// コスト上限の設定を開く (設定ウィンドウを「コスト」で絞った状態)。
    pub(super) fn open_cost_settings(&mut self) {
        self.settings_open = true;
        self.settings_ui.only_modified = false;
        // 絞り込み語はキー名の共通接頭辞から作る — 画面のラベル
        // (翻訳で変わる) をベタ書きしない。
        self.settings_ui.query = "cost_".into();
    }

    pub(super) fn quota_status(&self) -> (Option<u8>, String) {
        let now = std::time::SystemTime::now();
        let accounts = self.quota.accounts(now);
        if accounts.is_empty() {
            return (None, String::new());
        }
        let advice = self.quota.advice(now);
        let worst = self.quota.worst_advice(now).severity();
        let mut lines: Vec<String> = vec![tr("プラン使用量 (クリックで明細)")];
        for u in &accounts {
            lines.push(trf(
                "{account}: {used} / {proj}",
                &[
                    ("account", u.account.clone()),
                    ("used", quota_usage_label(u)),
                    ("proj", quota_projection_label(u.projection)),
                ],
            ));
        }
        for (_, a) in advice.iter().filter(|(_, a)| a.severity() > 0) {
            lines.push(format!("⚠ {}", a.message()));
        }
        (Some(worst), lines.join("\n"))
    }

    /// ステータスバーの「UTF-8 / CRLF」表示。エディタタブが無ければ None。
    ///
    /// 改行の集計は本文の全走査なので、同じバッファ・同じ長さのうちは
    /// [`LE_RECOUNT`] 間隔でしか数え直さない (毎フレーム走査しない)。
    pub(super) fn text_format_label(&mut self) -> Option<String> {
        /// 改行コードを数え直す最短間隔。
        const LE_RECOUNT: Duration = Duration::from_millis(400);
        let i = self.editor.active?;
        let b = self.editor.buffers.get(i)?;
        let (id, len) = (b.id, b.text.len());
        let stale = match &self.le_cache {
            Some((cid, clen, at, _)) => *cid != id || *clen != len || at.elapsed() >= LE_RECOUNT,
            None => true,
        };
        if stale {
            let le = crate::textenc::detect_line_ending(&b.text);
            self.le_cache = Some((id, len, Instant::now(), le));
        }
        let le = self.le_cache.as_ref().map(|(_, _, _, l)| *l)?;
        let enc = self.editor.buffers[i].encoding;
        // 混在しているときは LineEnding::label が内訳 (「CRLF (LF 3行混在)」) まで返す
        Some(format!("{} / {}", enc.name(), le.label()))
    }

    /// アクティブバッファの改行コード (キャッシュがあればそれ。無ければ数え直す)。
    pub(super) fn active_line_ending(&self) -> crate::textenc::LineEnding {
        let Some(i) = self.editor.active else {
            return crate::textenc::LineEnding::Lf;
        };
        let Some(b) = self.editor.buffers.get(i) else {
            return crate::textenc::LineEnding::Lf;
        };
        match &self.le_cache {
            Some((cid, clen, _, le)) if *cid == b.id && *clen == b.text.len() => *le,
            _ => crate::textenc::detect_line_ending(&b.text),
        }
    }
}
