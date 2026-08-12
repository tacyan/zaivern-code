use super::*;

impl ZaivernApp {
    // ─── UI: cockpit ────────────────────────────────────────────────

    pub(super) fn cockpit_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let theme = self.theme.clone();
        // 同居しているエージェント同士のファイル衝突を見張る。
        // 同じ作業ツリーに 2 体以上居ないときは git を 1 回も叩かない
        // (= Cockpit を開いていてもアイドルなら追加コストはゼロ)。
        self.sync_conflicts();
        // 押されたボタン類はクロージャの中では記録だけして、描画後に self へ反映する。
        let mut acts = CockpitActions::default();
        let mut orch_acts: Vec<orchestration::OrchAction> = Vec::new();
        let orch_rows = self.orch_rows();
        // 🏁 レースセクションが使うスナップショット (クロージャ内の借用衝突を避ける)
        let mut race_acts: Vec<race::RaceAction> = Vec::new();
        let race_presets: Vec<(String, String)> = self
            .cfg
            .agents
            .iter()
            .map(|p| (p.icon.clone(), p.name.clone()))
            .collect();
        let race_sessions: Vec<(u64, bool)> = self
            .agents
            .sessions
            .iter()
            .map(|s| (s.id, s.running()))
            .collect();

        // 上の見出し帯は「エージェントのタイルに譲る」方針で詰める。
        // 余白 12 + 8×3 = 36px を 8 + 4×3 = 20px へ。
        egui::Frame::none()
            .inner_margin(egui::Margin::same(space::SM))
            .show(ui, |ui| {
                self.cockpit_header_ui(ui, &theme, &mut acts);
                ui.add_space(space::XS);
                self.cockpit_conflicts_ui(ui, &theme);

                // ── 常設をやめたセクション ────────────────────────────
                //
                // 「◇ タスクとメッセージ」と「🏁 プロンプトレース」は、
                // **見出しだけの行が常駐して縦を食う**わりに、ほぼ常に 0 件
                // だった。Cockpit の主役はエージェントのタイルなので、常設表示
                // から外し、**開いているときだけ**描く。
                //
                // 機能は消していない — どちらもコマンドパレット
                // (⌘⇧P →「📮 エージェントへメッセージ」「🏁 プロンプトレース」)
                // から開ける。開くとフォームがここに現れる。
                if self.orch.form_open || self.orch.msg_open {
                    orch_acts = orchestration::cockpit_section(
                        &mut self.orch,
                        ui,
                        &theme,
                        self.coordinator.tasks(),
                        &orch_rows,
                        &orchestration::bus_status(&self.coordinator, &orch_rows),
                        None,
                    );
                    ui.add_space(space::XS);
                }
                if !race::is_idle(&self.race) {
                    race_acts = race::race_section(
                        &mut self.race,
                        ui,
                        &theme,
                        &race_presets,
                        &race_sessions,
                    );
                    ui.add_space(space::XS);
                }

                self.cockpit_super_agent_ui(ui, &theme, &mut acts);
                ui.add_space(space::XS);

                self.cockpit_grid_ui(ui, &theme, &mut acts);
            });

        self.apply_cockpit_actions(ctx, &theme, &orch_rows, acts, orch_acts);
        self.apply_race_actions(race_acts, ctx);
    }

    /// 衝突の見張りへ現在のエージェント (ID・作業ツリー・生死) を渡す。
    /// Cockpit / 看板を描くフレームでだけ呼ぶ (閉じている間は 1 命令も走らない)。
    pub(super) fn sync_conflicts(&mut self) {
        let live: Vec<(u64, PathBuf, bool)> = self
            .agents
            .sessions
            .iter()
            .map(|s| (s.id, s.cwd.clone(), s.running()))
            .collect();
        self.conflicts.update(&live);
        // 🛰 レーダーは**隔離済み**の worktree 同士を見る (同居は上の担当)。
        // ディスク使用量は窓を開けている間だけ測る (閉じていればゼロコスト)。
        // 結果が差し替わっても**自分からは再描画を要求しない** (`ConflictWatch`
        // と同じ約束)。エージェントが動いていれば PTY 出力で描き直しが起きるし、
        // 全員止まっていれば描き直す理由が無い = アイドルのコストはゼロ。
        let specs = self.radar_specs();
        let _ = self.conflict_radar.update(&specs, self.radar_open);
    }

    /// レーダーが見張るワークツリーの一覧 (git を 1 回も呼ばない)。
    ///
    /// **稼働中で worktree 隔離されたエージェント** を、同じリポジトリごとに
    /// まとめ、いちばん大きな束を選ぶ。そこへ本体のリポジトリ自身も 1 本
    /// (ID 0) として加える — ユーザー自身の未コミット変更もエージェントと
    /// ぶつかるので、隠すと「後で発見させない」という目的に反する。
    pub(super) fn radar_specs(&self) -> Vec<conflict::TreeSpec> {
        let mut by_repo: HashMap<PathBuf, Vec<conflict::TreeSpec>> = HashMap::new();
        for s in self.agents.sessions.iter().filter(|s| s.running()) {
            let Some(wt) = self.agent_worktrees.get(&s.id) else {
                continue;
            };
            by_repo
                .entry(worktree::path_key(&wt.repo))
                .or_default()
                .push(conflict::TreeSpec {
                    id: s.id,
                    label: format!("{} {}", s.icon, s.title),
                    branch: wt.branch.clone(),
                    dir: wt.dir.clone(),
                });
        }
        // 同じ画面に 2 つのリポジトリの行列を混ぜない (共通ベースが無い)。
        let Some((_, mut specs)) = by_repo
            .into_iter()
            .max_by_key(|(k, v)| (v.len(), k.clone()))
        else {
            return Vec::new();
        };
        specs.sort_by_key(|s| s.id);
        // 本体リポジトリ。エージェントの worktree の `repo` から取るので、
        // ワークスペースをどこに開いていても正しい相手になる。
        if let Some(repo) = self
            .agent_worktrees
            .values()
            .find(|w| specs.iter().any(|s| s.dir == w.dir))
            .map(|w| w.repo.clone())
        {
            let name = repo
                .file_name()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| tr("リポジトリ本体"));
            specs.insert(
                0,
                conflict::TreeSpec {
                    id: 0,
                    label: trf("📁 {name} (本体)", &[("name", name)]),
                    branch: String::new(),
                    dir: repo,
                },
            );
        }
        specs
    }

    /// 🛰 衝突レーダーの開閉 ([`crate::conflict::FEATURE`] から呼ばれる)。
    pub(crate) fn toggle_conflict_radar(&mut self) {
        self.radar_open = !self.radar_open;
        if self.radar_open {
            // 開くたびに絞り込みは解く (前回選んだ組が残っていると
            // 「開いたのに何も出ない」に見える)。
            self.radar_pair = None;
        }
    }

    /// 🛰 衝突レーダーの窓。**閉じているときは 1 命令も走らない**。
    pub(crate) fn conflict_radar_ui(&mut self, ctx: &egui::Context) {
        if !self.radar_open {
            return;
        }
        // Cockpit を閉じたまま窓だけ開いた場合も更新が止まらないよう、
        // ここでも 1 段だけ進める (二重に呼んでも走査は 1 本しか起きない)。
        let specs = self.radar_specs();
        let _ = self.conflict_radar.update(&specs, true);
        let theme = self.theme.clone();
        let mut open = true;
        let mut pair = self.radar_pair;
        let acts = conflict::radar_window(ctx, &theme, &mut open, &self.conflict_radar, &mut pair);
        self.radar_pair = pair;
        if !open {
            self.radar_open = false;
        }
        for a in acts {
            match a {
                conflict::RadarAction::Open(path, line) => self.open_path_at(&path, line, 1),
                conflict::RadarAction::Close => self.radar_open = false,
            }
        }
    }

    /// 衝突バッジのツールチップ (ファイル名と、取り合っている相手の名前)。
    pub(super) fn conflict_tooltip(&self) -> String {
        let rep = self.conflicts.report();
        let mut lines = vec![trf(
            "{n} 体が同じ作業ツリーで同じファイルを触っています",
            &[("n", rep.agents().len().to_string())],
        )];
        for f in rep.files.iter().take(CONFLICT_ROWS_MAX) {
            let who: Vec<String> = f
                .agents
                .iter()
                .filter_map(|id| self.agents.sessions.iter().find(|s| s.id == *id))
                .map(|s| format!("{} {}", s.icon, s.title))
                .collect();
            lines.push(format!("• {} — {}", f.label, who.join(" ・ ")));
        }
        let more = rep.file_count().saturating_sub(CONFLICT_ROWS_MAX);
        if more > 0 {
            lines.push(trf("… 他 {n} 件", &[("n", more.to_string())]));
        }
        lines.push(tr("🌿 worktree 隔離で起動すると、この取り合いは起きません"));
        lines.join("\n")
    }

    /// 衝突の詳細行。**閉じているときと 0 件のときは高さを 1 px も取らない**。
    pub(super) fn cockpit_conflicts_ui(&mut self, ui: &mut egui::Ui, theme: &Theme) {
        if !self.conflict_detail || self.conflicts.report().is_empty() {
            return;
        }
        let rep = self.conflicts.report();
        let rows: Vec<(String, String)> = rep
            .files
            .iter()
            .take(CONFLICT_ROWS_MAX)
            .map(|f| {
                let who: Vec<String> = f
                    .agents
                    .iter()
                    .filter_map(|id| self.agents.sessions.iter().find(|s| s.id == *id))
                    .map(|s| format!("{} {}", s.icon, s.title))
                    .collect();
                (f.label.clone(), who.join(" ・ "))
            })
            .collect();
        let more = rep.file_count().saturating_sub(rows.len());
        egui::Frame::none()
            .fill(theme.panel_alt)
            .rounding(4.0)
            .inner_margin(egui::Margin::same(space::SM))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.label(
                    RichText::new(tr("⚠ 同じファイルを複数のエージェントが触っています"))
                        .color(theme.warn)
                        .strong(),
                );
                for (file, who) in rows {
                    ui.horizontal(|ui| {
                        ui.add(egui::Label::new(RichText::new(&file).monospace()).truncate())
                            .on_hover_text(&file);
                        ui.add(
                            egui::Label::new(
                                RichText::new(who.clone()).small().color(theme.text_dim),
                            )
                            .truncate(),
                        )
                        .on_hover_text(who);
                    });
                }
                if more > 0 {
                    ui.label(
                        RichText::new(trf("… 他 {n} 件", &[("n", more.to_string())]))
                            .small()
                            .color(theme.text_dim),
                    );
                }
                ui.label(
                    RichText::new(tr(
                        "🌿 worktree 隔離で起動すると、同じファイルの取り合いは起きません",
                    ))
                    .small()
                    .color(theme.text_dim),
                );
            });
        ui.add_space(space::XS);
    }

    /// Cockpit のヘッダー行 (タイトル・稼働数・閉じる/全切替・Agent 起動・
    /// 一斉送信・音声)。押されたボタンは acts に記録だけして呼び出し側で反映する。
    pub(super) fn cockpit_header_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        acts: &mut CockpitActions,
    ) {
        // 幅が足りないとき (エディタと分割している / 窓が狭い) は、
        // 文字を落としてアイコンだけにする。右端で切れて押せなくなるのを防ぐ。
        let compact = cockpit_header_compact(ui.available_width());

        // コンポーザの姿 (1 行帯 / 複数行フォーム)。▾ を押したかどうかだけ憶えて
        // おき、実際の判定は本文込みで panels 側の純粋関数に任せる。
        let expand_id = ui.make_persistent_id("cockpit_composer_expand");
        let mut expand = ui.memory(|m| m.data.get_temp::<bool>(expand_id).unwrap_or(false));
        let expanded = panels::composer_wants_expand(self.agent_input_buf.text(), expand);
        let target: Option<(u64, String)> = self
            .agents
            .sessions
            .get(self.agents.active)
            .map(|s| (s.id, format!("{} {}", s.icon, s.title)));
        // 宛先チップは**全エージェント**を横一列で出す (入力欄の下)。
        // 複製して同名が並んだときは #1 #2 … を足して必ず見分けられるようにする。
        let names: Vec<String> = self
            .agents
            .sessions
            .iter()
            .map(|s| format!("{} {}", s.icon, s.title))
            .collect();
        let targets: Vec<(u64, String)> = self
            .agents
            .sessions
            .iter()
            .map(|s| s.id)
            .zip(panels::disambiguate_labels(&names))
            .collect();
        // 「⏸ 停止中」チップの件数。`&mut self.agent_input_buf` を借りる前に
        // 数え終えておく (借用が重なるため)。
        let stalled = self.stalled_session_ids().len();

        // ── `@` コンテキスト参照の材料 ──────────────────────────
        // **ここでは I/O を 1 つもしない。** 索引・シンボル・ブランチ名は
        // すべて裏で取り終えて手元にある値で、診断もメモリ上の集計。
        // 借用が重ならないよう、`&mut self.agent_input_buf` を取る前に
        // 必要なものを局所変数へ移しておく (`mem::take` はポインタ交換だけ)。
        let m_root = self.roots.first().cloned().unwrap_or_default();
        let m_terms: Vec<(u64, String)> = self
            .agents
            .sessions
            .iter()
            .map(|s| (s.id, format!("{} {}", s.icon, s.title)))
            .collect();
        // 診断の集計はピッカーが開いている間だけ (毎フレーム全診断を舐めない)。
        let m_problems = if self.mention.is_open() {
            self.collect_problems().len()
        } else {
            0
        };
        // git があるかは**裏でキャッシュ済みのブランチ名**で判定する
        // (ここで `git rev-parse` を撃つと UI スレッドが数秒止まる)。
        let m_repo = self.git_branch().is_some().then(|| m_root.clone());
        let m_trunc = self.index_truncated;
        let m_busy = self.lsp_symbols_busy || self.index_rx.is_some();
        let m_rels = std::mem::take(&mut self.mention_rels);
        let m_syms = std::mem::take(&mut self.mention_syms);
        let msrc = mention::Source {
            root: &m_root,
            files: &m_rels,
            files_truncated: m_trunc,
            symbols: &m_syms,
            symbols_busy: m_busy,
            terminals: &m_terms,
            problems: m_problems,
            repo: m_repo.as_deref(),
        };
        // `self` は下のクロージャ群が丸ごと借りるので、ピッカーの状態も
        // 一旦手元へ引き取る (`mem::take` は構造体の移動だけで割り当てはしない)。
        let mut mstate = std::mem::take(&mut self.mention);
        let mut mhook = mention::Hook {
            state: &mut mstate,
            source: &msrc,
        };
        // ヘッダー行に埋め込めたか。埋め込めなかった分だけ下に別行で出す。
        let mut inline_done = false;
        let mut composer = panels::ComposerAction::None;

        ui.horizontal(|ui| {
            ui.label(
                RichText::new(if compact {
                    "🎛"
                } else {
                    "🎛 Agent Cockpit"
                })
                .size(if compact { 16.0 } else { 20.0 })
                .strong()
                .color(theme.accent),
            );
            let running = self.agents.running_count();
            let total = self.agents.sessions.len();
            ui.label(
                RichText::new(if compact {
                    format!("{running}/{total}")
                } else {
                    trf(
                        "{running} 稼働中 / {total} セッション",
                        &[
                            ("running", running.to_string()),
                            ("total", total.to_string()),
                        ],
                    )
                })
                .color(theme.text_dim),
            );
            // ── ファイル衝突のバッジ ────────────────────────────────
            // **0 件のときは 1 ピクセルも出さない**。押すと詳細が下に開く
            // (勝手には開かない = 画面が突然変わらない)。
            let conflict_n = self.conflicts.report().file_count();
            if conflict_n > 0 {
                let tip = self.conflict_tooltip();
                let hit = ui
                    .selectable_label(
                        self.conflict_detail,
                        RichText::new(if compact {
                            format!("⚠{conflict_n}")
                        } else {
                            trf("⚠ {n} ファイル競合", &[("n", conflict_n.to_string())])
                        })
                        .color(theme.warn)
                        .strong(),
                    )
                    .on_hover_text(tip);
                if hit.clicked() {
                    self.conflict_detail = !self.conflict_detail;
                }
            }
            // ── 🛰 衝突レーダー (隔離済み worktree 同士) ──────────────
            // **綺麗なときは静かである**。警報が 0 件なら 1 ピクセルも出さない。
            let radar_n = self.conflict_radar.report().alarm_files();
            if radar_n > 0 {
                let hit = ui
                    .selectable_label(
                        self.radar_open,
                        RichText::new(if compact {
                            format!("🛰{radar_n}")
                        } else {
                            trf("🛰 {n} ファイル衝突予測", &[("n", radar_n.to_string())])
                        })
                        .color(theme.err)
                        .strong(),
                    )
                    .on_hover_text(tr(
                        "別々の worktree で走っているエージェントが、マージすると\n\
                         衝突するファイルを触っています。押すと衝突レーダーが開きます。",
                    ));
                if hit.clicked() {
                    // パレット経由と同じ入口を通す (絞り込みの解除も揃う)。
                    self.toggle_conflict_radar();
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button(if compact {
                        "✕".to_string()
                    } else {
                        tr("✕ 閉じる")
                    })
                    .on_hover_text(tr("Cockpit を閉じる"))
                    .clicked()
                {
                    self.cockpit = false;
                }
                if ui
                    .button(if compact {
                        "📋".to_string()
                    } else {
                        tr("📋 看板")
                    })
                    .on_hover_text(tr("フリート看板へ切替"))
                    .clicked()
                {
                    self.kanban = true;
                    self.cockpit = false;
                }
                if ui
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
                    acts.cycle_all = true;
                }
                // 巻き戻し。承認キューを通した**後**に暴走した変更を戻す唯一の
                // 足場なので、Cockpit から 1 打で開けるようにする。件数は
                // 一覧を開くまで数えないため、0 のときは数字を出さない。
                let cp_n = self.checkpoints.count();
                let cp_label = match (compact, cp_n) {
                    (true, _) => "⏱".to_string(),
                    (false, 0) => tr("⏱ 巻き戻し"),
                    (false, n) => trf("⏱ 巻き戻し ({n})", &[("n", n.to_string())]),
                };
                if ui
                    .button(RichText::new(cp_label).color(theme.text_dim))
                    .on_hover_text(tr(
                        "指示を送る直前の作業ツリーを記録しています。選んだ時点へ戻せます (後から作られたファイルは消しません)",
                    ))
                    .clicked()
                {
                    acts.checkpoints = true;
                }
                ui.menu_button(if compact { "＋" } else { "＋ Agent" }, |ui| {
                    for (i, p) in self.cfg.agents.iter().enumerate() {
                        if ui.button(format!("{} {}", p.icon, p.name)).clicked() {
                            acts.launch = Some(i);
                            ui.close_menu();
                        }
                    }
                });
                // 音声で全エージェントの入力欄へ入れる (送信は各自 Enter)
                let rec = self.voice.session.is_some();
                if rec
                    && ui
                        .button(RichText::new("⏹").color(theme.err).strong())
                        .on_hover_text(tr("音声入力を止める"))
                        .clicked()
                {
                    acts.voice_stop = true;
                }
                if ui
                    .selectable_label(
                        rec && self.voice.target == voice::Target::Broadcast,
                        if rec { "🔴" } else { "🎤" },
                    )
                    .on_hover_text(tr("音声入力 → 全エージェントの入力欄へ\n\
                         ⏹ を押すまで話した内容が入り続けます。\n\
                         送信はされないので、自分で Enter を押してください"))
                    .clicked()
                {
                    acts.voice_all = true;
                }

                // 1 行帯のコンポーザは**ヘッダー行の余りに畳み込む**。
                // ここが右端まで詰まっているときだけ下に別行で出す (下記)。
                // 複数行フォームは背が高いので決してこの行には入れない
                // (横並びの 1 行に押し込まれ、見出しの下に数百 px の空白ができる)。
                if composer_fits_header(expanded, ui.available_width()) {
                    composer = panels::agent_composer_inline_ui(
                        ui,
                        theme,
                        &mut self.agent_input_buf,
                        target.as_ref().map(|(id, t)| (*id, t.as_str())),
                        &mut expand,
                        &mut mhook,
                    );
                    inline_done = true;
                }
            });
        });

        // ── エージェント宛てコンポーザ (宛先つき) ──
        //
        // 1 行のブロードキャスト欄を置き換えたもの。**宛先を選べる**のが要点で、
        // 「1 体に向けたレビュー指示が全エージェントへ飛ぶ」漏れをここで止める。
        // 下書きは宛先ごとに分かれて残るので、切り替えても書きかけが消えない。
        //
        // 既定は 1 行帯で、ヘッダー行の余白に畳み込む (上の `inline_done`)。
        // 複数行フォームだけは**見出し行の外**に出す。中に入れると複数行の
        // テキスト欄が横並びの 1 行に押し込まれ、右端の細い帯へ折り返されて
        // 見出しの下に数百 px の空白ができる (ボタンも右端で切れる)。
        // 1 行帯をヘッダーに畳み込めたときも、**エージェントが 2 体以上いれば
        // 宛先チップは出す** — 「複数起動したのに横に並んで選べない」を潰す。
        // 1 体以下なら選ぶ余地がないので 1 行も使わない。
        if inline_done && targets.len() >= 2 {
            panels::composer_target_chips(ui, theme, &mut self.agent_input_buf, &targets, stalled);
        }

        if !inline_done {
            composer = if expanded {
                panels::agent_composer_ui(
                    ui,
                    theme,
                    &mut self.agent_input_buf,
                    target.as_ref().map(|(id, t)| (*id, t.as_str())),
                    &targets,
                    stalled,
                    &mut expand,
                    &mut mhook,
                )
            } else {
                // ヘッダーが詰まっていた場合の逃げ場 (窓が狭いとき)。
                panels::agent_composer_inline_ui(
                    ui,
                    theme,
                    &mut self.agent_input_buf,
                    target.as_ref().map(|(id, t)| (*id, t.as_str())),
                    &mut expand,
                    &mut mhook,
                )
            };
        }
        // ここでピッカーの状態を返す (`mhook` の借用はこの行で終わる)。
        let mention::Hook { .. } = mhook;
        self.mention = mstate;
        // 添付チップ (印・解決先・**1 件ごとの概算トークン**)。空なら何も描かない。
        if let Some(token) = mention::chips_ui(ui, theme, self.mention.ledger()) {
            let stripped = mention::strip_token(self.agent_input_buf.text(), &token);
            self.agent_input_buf.set_text(stripped);
        }
        self.mention_rels = m_rels;
        self.mention_syms = m_syms;
        // 端末の末尾・診断は App しか持っていないので、ここで詰めて返す。
        if let Some(need) = self.mention.take_need() {
            self.serve_mention_need(need);
        }
        ui.memory_mut(|m| m.data.insert_temp(expand_id, expand));
        match composer {
            panels::ComposerAction::Send(t) => acts.broadcast = Some(t),
            panels::ComposerAction::SendStalled(t) => acts.broadcast_stalled = Some(t),
            panels::ComposerAction::SendTo(id, t) => acts.send_to = Some((id, t)),
            panels::ComposerAction::Cancel => {
                // 入力欄からフォーカスを外すだけ (下書きは消さない)。
                // ID を当てにせず egui のテキスト入力そのものを畳む。
                ui.memory_mut(|m| m.stop_text_input());
            }
            panels::ComposerAction::None => {}
        }
    }

    /// Cockpit: 監視役 LLM (スーパーエージェント) の選択セクション。
    /// 変更は acts に記録だけして呼び出し側で反映する。
    pub(super) fn cockpit_super_agent_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        acts: &mut CockpitActions,
    ) {
        // ── 監視役 LLM (スーパーエージェント) の選択 ──────────────
        egui::CollapsingHeader::new(
            RichText::new(tr("💡 スーパーエージェント (指揮役)"))
                .strong()
                .color(theme.text),
        )
        .id_salt("super-agent-section")
        .show(ui, |ui| {
            ui.label(
                RichText::new(tr(
                    "決定論的な見張り (停滞・ループ・エラー多発の検知) は、この設定に\
                     関わらず常に動き、異常はここと通知で知らせます。ここでは\
                     いま起動しているエージェントの中からどれでも 1 体を『指揮官』に\
                     指名できます (作業の途中でもいつでも交代できます)。指揮官が\
                     `@対象: 指示` (全員へは `@all:`) と書くと、その内容が **📮 通知**\
                     としてユーザーへ届きます。どのエージェントの入力欄にも自動では\
                     書き込みません — 指示を実際に流すかはユーザーが決めます\
                     (Cockpit の一斉送信や各端末への手入力で)。停止・再起動などの\
                     破壊的な操作を自動でエージェントへ投げることもしません。\
                     指名したエージェントは普通の作業にもそのまま使えますし、\
                     そのセッション自身も見張りの対象です。",
                ))
                .size(12.0)
                .color(theme.text_dim),
            );
            ui.add_space(6.0);

            let cur_cmd = self.cfg.super_agent.command.trim().to_string();
            let cur_title = self.cfg.super_agent.session_title.trim().to_string();
            let none_label = tr("なし（監視のみ・指揮しない）");
            let cur_label = if cur_cmd.is_empty() && cur_title.is_empty() {
                none_label.clone()
            } else if !cur_title.is_empty() {
                // セッション指名中: 起動中ならアイコン付き、居なければ待機表示。
                self.agents
                    .sessions
                    .iter()
                    .find(|s| s.title.trim() == cur_title)
                    .map(|s| format!("{} {}", s.icon, s.title))
                    .unwrap_or_else(|| {
                        trf("{title}（未起動）", &[("title", cur_title.clone())])
                    })
            } else {
                // 旧形式 (コマンドのみ指定): プリセット名で表示する。
                self.cfg
                    .agents
                    .iter()
                    .find(|p| p.command.trim() == cur_cmd)
                    .map(|p| format!("{} {}", p.icon, p.name))
                    .unwrap_or_else(|| cur_cmd.clone())
            };

            egui::ComboBox::from_id_salt("super-agent-pick")
                .selected_text(cur_label)
                .width(320.0)
                .show_ui(ui, |ui| {
                    if ui
                        .selectable_label(
                            cur_cmd.is_empty() && cur_title.is_empty(),
                            none_label.as_str(),
                        )
                        .clicked()
                    {
                        acts.super_pick = Some((String::new(), String::new()));
                    }
                    if self.agents.sessions.is_empty() {
                        ui.label(
                            RichText::new(tr(
                                "起動中のエージェントがいません — \
                                 セッションを起動するとここに並びます",
                            ))
                            .size(12.0)
                            .color(theme.text_dim),
                        );
                    }
                    // いま居るセッションを名前で並べる (途中からの指名・交代用)。
                    // 指揮は画面を読むだけなので、起動しているエージェントなら
                    // どれでも選べる (素のシェルだけは誤検出しやすいので除外)。
                    for s in self.agents.sessions.iter() {
                        let label = format!("{} {}", s.icon, s.title);
                        let why = if !s.running() {
                            Some(tr("終了しています (再起動すると選べます)"))
                        } else {
                            commander_reject_reason(&s.command)
                        };
                        match why {
                            None => {
                                if ui
                                    .selectable_label(
                                        cur_title == s.title.trim(),
                                        label,
                                    )
                                    .clicked()
                                {
                                    acts.super_pick = Some((
                                        s.command.trim().to_string(),
                                        s.title.trim().to_string(),
                                    ));
                                }
                            }
                            Some(why) => {
                                ui.add_enabled(
                                    false,
                                    egui::Button::new(
                                        RichText::new(trf(
                                            "{label} — 選べません",
                                            &[("label", label)],
                                        ))
                                        .color(theme.text_dim),
                                    )
                                    .frame(false),
                                )
                                .on_disabled_hover_text(tr(&why));
                            }
                        }
                    }
                });

            ui.horizontal(|ui| {
                let mut en = self.cfg.super_agent.enabled;
                if ui
                    .checkbox(&mut en, tr("指揮を有効にする"))
                    .on_hover_text(tr(
                        "OFF にすると指揮だけ止まります。決定論的な見張りは動き続けます",
                    ))
                    .changed()
                {
                    acts.super_enabled = Some(en);
                }
            });

            ui.add_space(4.0);
            let appointed =
                self.cfg.super_agent.enabled && (!cur_cmd.is_empty() || !cur_title.is_empty());
            if let Some(id) = self.super_agent_session {
                // 指揮官が実際に動いている。セッションは毎フレーム引き直して
                // いるので、この ID は今この瞬間の指揮官を指す。
                let head = self
                    .agents
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| {
                        trf(
                            "✅ 指揮官: {icon} {title}  (#{id})",
                            &[
                                ("icon", s.icon.to_string()),
                                ("title", s.title.clone()),
                                ("id", id.to_string()),
                            ],
                        )
                    })
                    .unwrap_or_default();
                ui.label(RichText::new(head).color(theme.ok));
                ui.label(
                    RichText::new(tr(
                        "このセッションが `@対象: 指示` (全員へは `@all:`) と書くと、\
                         その内容が 📮 通知としてユーザーへ届きます。エージェントの\
                         入力欄へ自動で書き込むことはありません",
                    ))
                    .size(12.0)
                    .color(theme.text_dim),
                );
            } else if appointed {
                // 指名済みだが、その相手がまだ (もう) 起動していない。
                let wait = if cur_title.is_empty() {
                    tr(
                        "指揮官セッションを待っています — 選んだ CLI でセッションを起動すると指揮を始めます",
                    )
                } else {
                    trf(
                        "指揮官セッション『{title}』を待っています — \
                         同じ名前のセッションが起動すると指揮を始めます",
                        &[("title", cur_title.clone())],
                    )
                };
                ui.label(RichText::new(wait).size(12.0).color(theme.warn));
            } else {
                ui.label(
                    RichText::new(tr(
                        "指揮官: なし（決定論的な見張りだけが動いています）",
                    ))
                    .color(theme.text_dim),
                );
            }
        });
    }

    /// Cockpit: セッションのグリッド描画 (空のときはプリセット起動の案内)。
    /// 押されたボタンは acts に記録だけして呼び出し側で反映する。
    /// Cockpit の空状態 — **カード 1 枚を利用可能領域の中央に置く**。
    ///
    /// 旧実装は `vertical_centered` + 概算の上詰めで、上のセクションの高さが
    /// 変わるたびにカードが上下し、狭い窓では起動ボタンが下端を突き抜けて
    /// 押せなくなっていた。矩形は [`panels::empty_card`] が決め (可用領域に
    /// 必ず収まる)、ここはその中を描くだけにしてある。
    pub(super) fn cockpit_empty_state_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        acts: &mut CockpitActions,
    ) {
        let presets: Vec<config::AgentPreset> = self.cfg.agents.clone();
        // **見えている範囲**で中央寄せする。`available_rect_before_wrap` だけだと
        // 親の割り当てが画面より高いときにカードが下へ突き抜けて切れる
        // (下端のボタンが押せない)。clip_rect と交差させて実際の可視域に収める。
        let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let l = panels::empty_card(avail, presets.len());
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(l.card), |ui| {
            egui::Frame::none()
                .fill(theme.panel_alt)
                .stroke(egui::Stroke::new(1.0_f32, theme.border))
                .rounding(egui::Rounding::same(10.0))
                .inner_margin(egui::Margin::same(space::MD))
                .show(ui, |ui| {
                    ui.set_width(l.card.width() - space::MD * 2.0);
                    let mut body = |ui: &mut egui::Ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new("🎛").size(52.0));
                            ui.label(
                                RichText::new(tr("エージェントがまだいません"))
                                    .size(18.0)
                                    .color(theme.text),
                            );
                            ui.label(
                                RichText::new(tr("プリセットから並列セッションを起動しましょう"))
                                    .color(theme.text_dim),
                            );
                        });
                        ui.add_space(space::MD);
                        // 起動ボタンは幅に応じて段組みする (縦に伸ばして下端を
                        // 突き抜けさせない)。行ごとに中央寄せ。
                        for row in 0..l.rows {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = space::SM;
                                let used =
                                    l.btn_w * l.cols as f32 + space::SM * (l.cols as f32 - 1.0);
                                ui.add_space(((ui.available_width() - used) * 0.5).max(0.0));
                                for col in 0..l.cols {
                                    let i = row * l.cols + col;
                                    let Some(p) = presets.get(i) else { break };
                                    let label = format!("{} {}", p.icon, p.name);
                                    if ui
                                        .add_sized(
                                            [l.btn_w, panels::EMPTY_BTN_H],
                                            egui::Button::new(RichText::new(&label).size(13.0))
                                                .wrap_mode(egui::TextWrapMode::Truncate),
                                        )
                                        .on_hover_text(&label)
                                        .clicked()
                                    {
                                        acts.launch = Some(i);
                                    }
                                }
                            });
                            ui.add_space(space::SM);
                        }
                    };
                    if l.scroll {
                        // どうしても入らない窓では、はみ出させずにスクロールへ逃がす
                        egui::ScrollArea::vertical()
                            .id_salt("cockpit-empty-state")
                            .auto_shrink([false, false])
                            .show(ui, body);
                    } else {
                        body(ui);
                    }
                });
        });
    }

    pub(super) fn cockpit_grid_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
        acts: &mut CockpitActions,
    ) {
        // 分割レイアウトの正規化 (毎フレーム): 消えたセッションのリーフを落とし、
        // フォーカスを隣へ移し、1 枚に戻ったタイルは分割そのものを畳む。
        self.normalize_splits();
        // 分割の**子ペイン**はタイルとしては並べない (親タイルの中に描かれる)。
        // 分割が 1 つも無ければこの集合は空で、以降の並びは今日と完全に同じ。
        let tiles = self.cockpit_tiles();
        let n = tiles.len();
        if n == 0 {
            self.cockpit_empty_state_ui(ui, theme, acts);
            return;
        }

        let avail = ui.available_size();
        let mini_font = (self.scaled_terminal_font() - 3.0).clamp(8.0, 14.0);
        // 6 枚以上でも 1 枚ずつは読める高さを保つ。入り切らないぶんは
        // 縦スクロールで見せる (潰さない)。
        let g = cockpit_grid_metrics(avail, n, grid_comfort_cell_h(mini_font));
        let (cols, rows, cell_w, cell_h) = (g.cols, g.rows, g.cell_w, g.cell_h);
        let scrolls = g.scrolls(avail.y);

        // アクティブが変わったフレームだけ、そのタイルを見える位置へ運ぶ。
        // 毎フレーム運ぶと自分でスクロールできなくなるので、直前に運んだ
        // セッションを憶えて「変わったとき」だけにする。
        let active_id = self.active_id();
        let follow = scrolls && active_id.is_some() && active_id != self.cockpit_followed;
        self.cockpit_followed = active_id;

        if scrolls {
            // 既定の浮動バーは触るまで完全に透明で、「まだ下にタイルがある」
            // ことに気付けない。幅は取らせない (レイアウトを動かさない) まま、
            // 薄く常時見せる。
            let sc = &mut ui.spacing_mut().scroll;
            sc.dormant_background_opacity = 0.35;
            sc.dormant_handle_opacity = 0.8;
        }

        egui::ScrollArea::vertical()
            .id_salt("cockpit-grid")
            .auto_shrink(false)
            .show(ui, |ui| {
                for row in 0..rows {
                    ui.horizontal(|ui| {
                        for col in 0..cols {
                            let Some(&i) = tiles.get(row * cols + col) else {
                                continue;
                            };
                            // 分割タイルは「中のどれかがアクティブ」なら
                            // アクティブ扱いにする (紫枠がタイルから消えない)。
                            let active = i == self.agents.active
                                || self.active_id().is_some_and(|a| {
                                    self.splits
                                        .get(&self.agents.sessions[i].id)
                                        .is_some_and(|l| l.contains(a))
                                });
                            // 未読 (最後に見てから新しい出力がある) のタイルは
                            // アクセント色のリングで囲う。未読が 1 件も無ければ
                            // 枠の太さも色も**1 ピクセルも変わらない**。
                            let stroke = if active {
                                egui::Stroke::new(2.0_f32, theme.accent)
                            } else if self.agents.sessions[i].has_unread() {
                                egui::Stroke::new(1.5_f32, theme.accent)
                            } else {
                                egui::Stroke::new(1.0_f32, theme.border)
                            };
                            // セル内の余白クリックでも選択できるようにする。
                            // egui のヒットテストは同一レイヤーでは「後に登録
                            // したウィジェット」が勝つため、描画後に全面
                            // ui.interact を掛けるとセル内のボタンやミニ
                            // ターミナルへのクリックをすべて奪ってしまう。
                            // UiBuilder::sense はコンテナの判定を子より先に
                            // 登録するので、余白クリックだけを拾える。
                            let cell = ui.scope_builder(
                                egui::UiBuilder::new()
                                    .id_salt(("cockpit-cell-select", i))
                                    .sense(egui::Sense::click()),
                                |ui| {
                            egui::Frame::none()
                                .fill(theme.panel_alt)
                                .stroke(stroke)
                                .rounding(egui::Rounding::same(8.0))
                                .inner_margin(egui::Margin::same(8.0))
                                .show(ui, |ui| {
                                    // Frame は親 (horizontal な行) のレイアウトを
                                    // 継承するため、明示的に縦積みへ切り替える。
                                    // これが無いとヘッダーとターミナルが横に並び
                                    // 画面外へはみ出す。
                                    ui.vertical(|ui| {
                                    ui.set_width(cell_w - 18.0);
                                    ui.set_height(cell_h - 18.0);
                                    // このタイルが隔離中かは、セッションを可変で
                                    // 借りる前に確かめておく (借用の都合)。
                                    let sid = self.agents.sessions[i].id;
                                    let tile_dead = self
                                        .frame_guard
                                        .is_quarantined(&Subview::Session(sid));
                                    let (err_col, dim_col) =
                                        (self.theme.err, self.theme.text_dim);
                                    // ヘッダの間だけセッションを可変で借りる。
                                    // ここでスコープを閉じないと、分割タイルの
                                    // 描画 (複数セッションを触る) と衝突する。
                                    {
                                    let s = &mut self.agents.sessions[i];
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
                                        ui.label(
                                            RichText::new(format!(
                                                "{}{} {}",
                                                badge, s.icon, s.title
                                            ))
                                            .strong()
                                            .color(theme.text),
                                        );
                                        if s.has_unread() && !active {
                                            ui.label(
                                                RichText::new("◆")
                                                    .size(9.0)
                                                    .color(theme.accent),
                                            )
                                            .on_hover_text(tr(
                                                "最後に見てから新しい出力があります",
                                            ));
                                        }
                                        if let Some(line) = &s.rate_limited {
                                            ui.label(
                                                RichText::new("⏳")
                                                    .color(theme.warn),
                                            )
                                            .on_hover_text(trf(
                                                "レート制限/使用上限: {line}",
                                                &[("line", line.clone())],
                                            ));
                                        }
                                        ui.label(
                                            RichText::new(s.uptime())
                                                .size(10.5)
                                                .color(theme.text_dim),
                                        );
                                        let permission_hint = s.permission_switch_hint();
                                        ui.with_layout(
                                            egui::Layout::right_to_left(
                                                egui::Align::Center,
                                            ),
                                            |ui| {
                                                if ui
                                                    .small_button("✕")
                                                    .on_hover_text(tr("閉じる"))
                                                    .clicked()
                                                {
                                                    acts.remove = Some(i);
                                                }
                                                if ui
                                                    .small_button("⟳")
                                                    .on_hover_text(tr("再起動"))
                                                    .clicked()
                                                {
                                                    acts.restart = Some(i);
                                                }
                                                if let Some(hint) = permission_hint {
                                                    if ui
                                                        .small_button("🛡")
                                                        .on_hover_text(hint)
                                                        .clicked()
                                                    {
                                                        acts.cycle = Some(i);
                                                    }
                                                }
                                                if ui
                                                    .small_button("🔍")
                                                    .on_hover_text(tr(
                                                        "下部パネルにフォーカス",
                                                    ))
                                                    .clicked()
                                                {
                                                    acts.focus = Some(i);
                                                }
                                                if ui
                                                    .small_button(
                                                        if self.voice.target == voice::Target::Session(sid)
                                                            && self.voice.session.is_some()
                                                        {
                                                            "🔴"
                                                        } else {
                                                            "🎤"
                                                        },
                                                    )
                                                    .on_hover_text(tr(
                                                        "このエージェントへ音声入力\n\
                                                         話した内容がこのタブの入力欄に入ります。\n\
                                                         送信されないので、確認して Enter を押してください",
                                                    ))
                                                    .clicked()
                                                {
                                                    acts.voice = Some(sid);
                                                    acts.select = Some(i);
                                                }
                                                // 端末分割への入口 (これが無いと
                                                // 分割はキーを知らない人に届かない)。
                                                // 分割中はペイン数を出す — 「今
                                                // 何枚あるか」が枠線だけでは分から
                                                // ないため。ズーム中は ◎ を添える。
                                                let split_label = match self
                                                    .splits
                                                    .get(&sid)
                                                    .filter(|l| !l.is_empty())
                                                {
                                                    Some(l) if l.zoomed() => {
                                                        format!("⊞{}◎", l.len())
                                                    }
                                                    Some(l) => format!("⊞{}", l.len()),
                                                    None => "⊞".to_string(),
                                                };
                                                // 狭いタイルでは畳む (ボタン列が
                                                // 見切れない)。キー操作は残る。
                                                if ui.available_width() >= 26.0
                                                    && ui
                                                    .small_button(split_label)
                                                    .on_hover_text(tr(
                                                        "このタイルを右へ分割して、新しいエージェントを起動\n\
                                                         起動するものは 👾 Agent ＋ からの新規起動と同じです\n\
                                                         (既定プリセット・作業フォルダはワークスペース)。\n\
                                                         分割中は各ペインのヘッダに ◎ (拡大) と ✕ (閉じる) が出ます。\n\
                                                         キーは ⌘⌥⇧→ 右へ分割 / ⌘⌥⇧↓ 下へ分割 / ⌘⌥N シェル /\n\
                                                         ⌘⌥W 閉じる / ⌘⌥←↑↓→ 移動 / ⌘⌥Z 拡大 / ⌘⌥E 等分 /\n\
                                                         ⌘⌥⇧H ⌘⌥⇧L 幅調整 (Windows・Linux は ⌘ の代わりに Ctrl)",
                                                    ))
                                                    .clicked()
                                                {
                                                    acts.split.push((
                                                        sid,
                                                        terminal::SplitAction::SplitWith {
                                                            dir: terminal::SplitDir::Horizontal,
                                                            preset: terminal::PanePreset::NewAgent,
                                                        },
                                                    ));
                                                }
                                            },
                                        );
                                    });
                                    }
                                    // 分割の操作キーは**アクティブなタイルだけ**が
                                    // 受ける。端末より先に食うので、消費したキーは
                                    // PTY へ流れない (Ctrl+C / Ctrl+D は
                                    // split_key_action が None を返すため素通り)。
                                    // まだ分割していないタイルでも受けること —
                                    // 受けないと「⊞ を 1 回押すまでキーが効かない」
                                    // という説明のつかない状態になる。
                                    if active && !tile_dead {
                                        if let Some(a) = take_split_key(ui.ctx()) {
                                            acts.split.push((sid, a));
                                        }
                                    }
                                    // 隔離中のタイルは描かない。1 枚が壊れている
                                    // だけでフレーム全体を捨てると、他のエージェント
                                    // まで固まって見えてしまうため、ここだけ外す。
                                    if tile_dead {
                                        Self::quarantine_placeholder_ui(
                                            ui,
                                            &Subview::Session(sid).label(),
                                            err_col,
                                            dim_col,
                                        );
                                    } else if self.splits.contains_key(&sid) {
                                        self.cockpit_split_tile_ui(
                                            ui, sid, mini_font, theme, acts,
                                        );
                                    } else {
                                    let s = &mut self.agents.sessions[i];
                                    let term = draw_subview(Subview::Session(sid), || {
                                        terminal::draw(
                                            ui, s, theme, mini_font, true, true, false,
                                        )
                                    });
                                    // ミニターミナルをクリックして入力を始めた
                                    // セッションへ、アクティブ (紫枠) を追従させる。
                                    if term.clicked()
                                        || term.drag_started()
                                        || term.gained_focus()
                                    {
                                        acts.select = Some(i);
                                    }
                                    }
                                    });
                                });
                                },
                            );
                            // 画面外にあるタイルがアクティブになったら、そこまで
                            // スクロールして見せる (キーやサイドバーで切り替えた
                            // ときに「選んだはずのタイルが見えない」を防ぐ)。
                            // align=None = 見えるようになる最小移動なので、
                            // 既に見えているタイルを選んでも画面は動かない。
                            if follow && active {
                                ui.scroll_to_rect(cell.response.rect, None);
                            }
                            // セル内のどこを押しても (タイトル文字・各ボタン・
                            // ミニターミナル含め) 紫枠のアクティブ選択が追従する。
                            // contains_pointer は子ウィジェットに覆われていても
                            // true になるだけでイベントは奪わないため、クリック
                            // 自体は各ボタン・ターミナルがそのまま処理できる。
                            if cell.response.clicked()
                                || (cell.response.contains_pointer()
                                    && ui.input(|i| i.pointer.primary_pressed()))
                            {
                                acts.select = Some(i);
                            }
                        }
                    });
                }
            });
    }

    // ── 端末分割 (Cockpit タイルの中の分割ペイン) ────────────────────────
    //
    // モデルは `terminal.rs` の [`terminal::SplitLayout`] (純粋なデータ構造)。
    // ここはその「所有者」として、セッションの起動・後始末・保存とだけ繋ぐ。

    /// いまアクティブなセッションの ID。
    pub(super) fn active_id(&self) -> Option<u64> {
        self.agents.sessions.get(self.agents.active).map(|s| s.id)
    }

    /// Cockpit のグリッドに**タイルとして**並べるセッションの添字。
    ///
    /// 分割の子ペインは親タイルの中に描かれるので、ここからは外す。
    /// 分割が 1 つも無ければ `0..len` そのまま — 今日と 1 枚も変わらない。
    pub(super) fn cockpit_tiles(&self) -> Vec<usize> {
        let ids: Vec<u64> = self.agents.sessions.iter().map(|s| s.id).collect();
        split_tile_indices(&ids, &self.splits)
    }

    /// 分割レイアウトを整える (毎フレーム + セッションを閉じた直後)。
    ///
    /// 1. 消えたセッションのリーフを落とし、フォーカスを最も近い生存ペインへ移す
    /// 2. 1 枚に戻ったタイルは分割そのものを畳む (= 今日と同じ描画経路へ戻す)
    /// 3. タイルのキーを木の**先頭リーフ**へ揃える (先頭を閉じても迷子にならない)
    pub(super) fn normalize_splits(&mut self) {
        if self.splits.is_empty() {
            self.split_rect.clear();
            return;
        }
        let live: HashSet<u64> = self.agents.sessions.iter().map(|s| s.id).collect();
        self.splits = normalize_split_map(std::mem::take(&mut self.splits), &live);
        self.split_rect.retain(|k, _| self.splits.contains_key(k));
    }

    /// 分割タイル 1 枚を描く (ペインが 2 枚以上のときだけ通る経路)。
    #[allow(clippy::too_many_arguments)]
    pub(super) fn cockpit_split_tile_ui(
        &mut self,
        ui: &mut egui::Ui,
        sid: u64,
        mini_font: f32,
        theme: &Theme,
        acts: &mut CockpitActions,
    ) {
        let (_, area) = ui.allocate_space(ui.available_size());
        self.split_rect.insert(sid, area);

        // ヘッダの中身と隔離状態は**先に**作る。`chrome` と `leaf` の 2 つの
        // クロージャが同時にセッション列を可変で借りられないため。
        let leaves = self
            .splits
            .get(&sid)
            .map(|l| l.leaves())
            .unwrap_or_default();
        let panes: Vec<(u64, terminal::PaneChrome, bool)> = leaves
            .iter()
            .map(|pid| {
                let dead = self.frame_guard.is_quarantined(&Subview::Session(*pid));
                let c = self
                    .agents
                    .sessions
                    .iter()
                    .find(|s| s.id == *pid)
                    .map(|s| terminal::PaneChrome {
                        icon: s.icon.clone(),
                        title: s.title.clone(),
                        dot: Some(if !s.running() {
                            theme.err
                        } else if s.attention {
                            theme.warn
                        } else {
                            theme.ok
                        }),
                    })
                    .unwrap_or_default();
                (*pid, c, dead)
            })
            .collect();

        let (err_col, dim_col) = (self.theme.err, self.theme.text_dim);
        let sessions = &mut self.agents.sessions;
        let Some(layout) = self.splits.get_mut(&sid) else {
            return;
        };
        let out = terminal::draw_split(
            ui,
            layout,
            area,
            terminal::GUTTER,
            theme,
            &mut |pid| {
                panes
                    .iter()
                    .find(|(x, _, _)| *x == pid)
                    .map(|(_, c, _)| c.clone())
                    .unwrap_or_default()
            },
            &mut |ui, _body, pid, _focused| {
                if panes.iter().any(|(x, _, dead)| *x == pid && *dead) {
                    Self::quarantine_placeholder_ui(
                        ui,
                        &Subview::Session(pid).label(),
                        err_col,
                        dim_col,
                    );
                    return;
                }
                let Some(s) = sessions.iter_mut().find(|s| s.id == pid) else {
                    return;
                };
                draw_subview(Subview::Session(pid), || {
                    terminal::draw(ui, s, theme, mini_font, true, true, false);
                });
            },
        );

        // ヘッダの ✕ は「そのペイン」を閉じる。フォーカスを移してから
        // ClosePane を積むことで、閉じる対象を 1 本の経路に統一する。
        if let Some(pid) = out.close {
            if let Some(l) = self.splits.get_mut(&sid) {
                l.set_focus(pid);
            }
            acts.split.push((sid, terminal::SplitAction::ClosePane));
        }
        if out.changed {
            // 新しい矩形を既存のコアレッサ経由で PTY へ流す。
            // ここで同期 resize を呼ぶと UI スレッドが ConPTY を待つ。
            let (_, cw, ch) = terminal::cell_metrics(ui, mini_font);
            if let Some(l) = self.splits.get(&sid) {
                let focus = l.focus();
                terminal::apply_sizes(l, area, terminal::GUTTER, cw, ch, &mut |pid, r, c| {
                    if let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == pid) {
                        s.resize(r, c);
                    }
                });
                // クリックでフォーカスが移ったペインへ紫枠も追従させる。
                if let Some(f) = focus {
                    if let Some(ix) = self.agents.sessions.iter().position(|s| s.id == f) {
                        acts.select = Some(ix);
                    }
                }
            }
        }
    }

    /// 分割操作 1 つを適用する (セッションの起動・後始末を伴うので描画後)。
    pub(super) fn apply_split_action(
        &mut self,
        tile: u64,
        action: terminal::SplitAction,
        ctx: &egui::Context,
    ) {
        use terminal::SplitAction;
        // 操作対象は「そのタイルのフォーカス中ペイン」。分割がまだ無いタイル
        // (⊞ の初回) はタイル自身が対象。
        let focused = self
            .splits
            .get(&tile)
            .and_then(|l| l.focus())
            .unwrap_or(tile);
        match action {
            SplitAction::SplitWith { dir, preset } => {
                // 置き場所 (どのペインの隣か) だけが分割の仕事。**何を起こすかは
                // 新規起動と 1 か所も変えない** — 既定プリセットを、ワークスペースの
                // 作業フォルダで起こす (親のプリセット・親の cwd は引き継がない)。
                if !self.agents.sessions.iter().any(|s| s.id == focused) {
                    return;
                }
                let Some(ix) = split_preset_index(&self.cfg.agents, preset) else {
                    self.toast(tr("起動できるエージェントの登録がありません"), false);
                    return;
                };
                let before = self.agents.sessions.len();
                // 分割はタイルの中で完結する操作なので、下部パネルを勝手に
                // 開かない (「画面が突然変わらない」)。起動側の副作用だけ戻す。
                let panel_was = self.agents.panel_open;
                self.launch_preset(ix, ctx);
                self.agents.panel_open = panel_was;
                if self.agents.sessions.len() == before {
                    return; // 起動に失敗した (トーストは launch 側が出す)
                }
                let Some(new_id) = self.agents.sessions.last().map(|s| s.id) else {
                    return;
                };
                let layout = self
                    .splits
                    .entry(tile)
                    .or_insert_with(|| terminal::SplitLayout::single(tile));
                layout.set_focus(focused);
                layout.split_focused(dir, new_id);
                self.normalize_splits();
                self.persist_session();
            }
            SplitAction::ClosePane => {
                // 先に木から外してフォーカスを兄弟へ渡してから reap する。
                if let Some(l) = self.splits.get_mut(&tile) {
                    l.close_leaf(focused);
                }
                if let Some(ix) = self.agents.sessions.iter().position(|s| s.id == focused) {
                    self.close_agent(ix);
                }
                self.normalize_splits();
                self.persist_session();
            }
            SplitAction::Focus(dir) => {
                let area = self.split_rect.get(&tile).copied();
                if let (Some(area), Some(l)) = (area, self.splits.get_mut(&tile)) {
                    if l.focus_dir(dir, area, terminal::GUTTER) {
                        if let Some(f) = l.focus() {
                            if let Some(ix) = self.agents.sessions.iter().position(|s| s.id == f) {
                                self.agents.active = ix;
                            }
                        }
                    }
                }
            }
            SplitAction::Zoom => {
                if let Some(l) = self.splits.get_mut(&tile) {
                    l.zoom_focused();
                }
            }
            SplitAction::Equalize => {
                if let Some(l) = self.splits.get_mut(&tile) {
                    l.equalize();
                }
            }
            SplitAction::Resize { grow } => {
                if let Some(l) = self.splits.get_mut(&tile) {
                    let step = if grow {
                        terminal::RESIZE_STEP
                    } else {
                        -terminal::RESIZE_STEP
                    };
                    l.resize_focused(step);
                }
            }
        }
    }

    /// 保存用: タイル `sid` の分割を 1 行の文字列にする (リーフ = 生ログのパス)。
    /// 分割していないタイルは空文字 — 既存のセッションファイルと同じ形のまま。
    pub(super) fn split_line_for(&self, sid: u64) -> String {
        let Some(layout) = self.splits.get(&sid) else {
            return String::new();
        };
        layout
            .to_rec(&mut |id| {
                self.agents
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .and_then(|s| s.log_path.as_ref())
                    .map(|p| p.to_string_lossy().into_owned())
                    .filter(|k| !k.is_empty())
            })
            .to_line()
    }

    /// 復元: 保存された分割行を、いま起きているセッションへ張り直す。
    /// 引けなかったリーフ (復元されなかったシェル等) は黙って落ちる。
    pub(super) fn restore_splits(&mut self, lines: &[String]) {
        let sessions = &self.agents.sessions;
        let found = split_map_from_lines(lines, &mut |key| {
            sessions
                .iter()
                .find(|s| {
                    s.log_path
                        .as_ref()
                        .is_some_and(|p| p.to_string_lossy() == key)
                })
                .map(|s| s.id)
        });
        self.splits.extend(found);
        self.normalize_splits();
    }

    /// Cockpit 描画後に、記録されたアクションを self へ反映する
    /// (クロージャを閉じた後に適用するのが app.rs の作法)。
    pub(super) fn apply_cockpit_actions(
        &mut self,
        ctx: &egui::Context,
        theme: &Theme,
        orch_rows: &[orchestration::SessionRow],
        acts: CockpitActions,
        mut orch_acts: Vec<orchestration::OrchAction>,
    ) {
        if let Some(text) = acts.broadcast {
            // `@` 添付を本文へ展開してから流す (印だけでは中身が届かない)。
            let text = self.expand_mentions(&text, None, ComposerTarget::Broadcast);
            // None はコスト上限で止めたとき。理由は送信側が説明済みなので
            // 「宛先がいない」と嘘を重ねない。
            match self.queue_submit_all(&text) {
                None => {}
                Some(0) => self.toast(tr("実行中のエージェントがありません"), false),
                Some(n) => self.toast(
                    trf("📣 {n} セッションへ送信しました", &[("n", n.to_string())]),
                    true,
                ),
            }
        }
        // 止まっているものだけへの一斉送信。作業中は巻き込まないので、
        // 「全員へ送ると進行中の作業まで分断される」を避けられる。
        if let Some(text) = acts.broadcast_stalled {
            let text = self.expand_mentions(&text, None, ComposerTarget::Stalled);
            match self.queue_submit_stalled(&text) {
                None => {}
                Some(0) => self.toast(tr("止まっているエージェントはありません"), false),
                Some(n) => self.toast(
                    trf(
                        "⏸ 止まっている {n} セッションへ送信しました",
                        &[("n", n.to_string())],
                    ),
                    true,
                ),
            }
        }
        // 宛先を指名した送信は**その 1 体だけ**へ届ける (broadcast は通らない)
        if let Some((id, text)) = acts.send_to {
            let text = self.expand_mentions(&text, Some(id), ComposerTarget::Agent(id));
            let live = self
                .agents
                .sessions
                .iter()
                .find(|s| s.id == id)
                .map(|s| (s.running(), s.title.clone()));
            match live {
                Some((true, title)) => {
                    if self.queue_submit(submit::Job::user(id, text)) {
                        self.toast(trf("✏ 送信: {title}", &[("title", title)]), true);
                    }
                }
                Some((false, _)) => self.toast(tr("セッションが終了しています"), false),
                None => self.toast(tr("宛先のセッションが見つかりません"), false),
            }
        }
        if acts.voice_stop {
            self.stop_voice();
        }
        if let Some(id) = acts.voice {
            self.apply_cmd(Cmd::VoiceInput(voice::Target::Session(id)), ctx);
        }
        if acts.voice_all {
            self.apply_cmd(Cmd::VoiceInput(voice::Target::Broadcast), ctx);
        }
        if acts.cycle_all {
            self.apply_cmd(Cmd::CyclePermissionAll, ctx);
        }
        if acts.checkpoints {
            self.apply_cmd(Cmd::CheckpointList, ctx);
        }
        if let Some(i) = acts.cycle {
            match self.agents.cycle_permission(i) {
                Some(hint) => self.toast_warn(trf(
                    "🛡 権限モード切替を送信しました（{hint} / 画面を確認してください）",
                    &[("hint", hint.to_string())],
                )),
                None => self.toast(tr("このセッションは権限モード切替に未対応です"), false),
            }
        }
        if let Some(i) = acts.launch {
            self.launch_preset(i, ctx);
        }
        if let Some(i) = acts.select {
            if i < self.agents.sessions.len() {
                self.agents.active = i;
            }
        }
        if let Some(i) = acts.focus {
            self.apply_cmd(Cmd::FocusAgent(i), ctx);
        }
        if let Some(i) = acts.restart {
            if let Err(e) = self.agents.restart(i, ctx) {
                self.toast(e, false);
            }
        }
        if let Some(i) = acts.remove {
            self.close_agent(i);
        }
        // 端末分割 (キー / ⊞ / ペインヘッダの ✕・◎) の適用。
        for (tile, action) in acts.split {
            self.apply_split_action(tile, action, ctx);
        }
        // タスク作成 / メッセージ送信のフォームと、押されたボタンの適用。
        let prev_task_target = self.orch.target;
        let prev_msg_target = self.orch.msg_target;
        // ディスパッチ前チェックの材料 (レーダーが既に持っている逆引き表)。
        // 宛先に指名した相手は外す — 自分が持っているファイルを自分へ
        // 警告しても意味が無い。
        let exclude = match self.orch.target {
            orchestration::TaskTarget::Session(id) => Some(id),
            orchestration::TaskTarget::Auto => None,
        };
        let owners = self.conflict_radar.report().all_owners(exclude);
        orch_acts.extend(orchestration::task_form_ui(
            &mut self.orch,
            ctx,
            theme,
            orch_rows,
            &owners,
        ));
        orch_acts.extend(orchestration::message_form_ui(
            &mut self.orch,
            ctx,
            theme,
            orch_rows,
        ));
        // 指示の宛先で特定のエージェントを選んだら (または送ったら)、
        // そのセッションへアクティブ (紫枠) を移す。
        let mut picked: Option<u64> = None;
        if self.orch.target != prev_task_target {
            if let orchestration::TaskTarget::Session(id) = self.orch.target {
                picked = Some(id);
            }
        }
        if self.orch.msg_target != prev_msg_target {
            if let orchestration::MsgTarget::Session(id) = self.orch.msg_target {
                picked = Some(id);
            }
        }
        for a in &orch_acts {
            match a {
                orchestration::OrchAction::CreateTask {
                    target: orchestration::TaskTarget::Session(id),
                    ..
                }
                | orchestration::OrchAction::SendMessage {
                    to: orchestration::MsgTarget::Session(id),
                    ..
                } => picked = Some(*id),
                _ => {}
            }
        }
        if let Some(id) = picked {
            if let Some(ix) = self.agents.sessions.iter().position(|s| s.id == id) {
                self.agents.active = ix;
            }
        }
        self.orch_apply(orch_acts);

        // 監視役 LLM の変更を反映する (閉じた後に適用するのが app.rs の作法)。
        let mut super_changed = false;
        if let Some((cmd, title)) = acts.super_pick {
            // 「なし」を選んだら相談自体を止める。エージェントを選んだ場合は、
            // わざわざもう 1 か所チェックを入れさせない。
            self.cfg.super_agent.enabled = !cmd.is_empty();
            self.cfg.super_agent.command = cmd;
            self.cfg.super_agent.session_title = title;
            super_changed = true;
        }
        if let Some(en) = acts.super_enabled {
            self.cfg.super_agent.enabled = en;
            super_changed = true;
        }
        if super_changed {
            self.apply_super_agent();
            config::save_state(&self.cfg);
        }
    }
}
