use super::*;

impl ZaivernApp {
    // ─── UI: フリート看板 (kanban) ──────────────────────────────────
    //
    // 判断と描画は kanban.rs 側。ここは「セッションをカードへ写す」
    // 「返ってきた操作を実行する」だけ (orchestration と同じ橋渡し構造)。

    pub(super) fn kanban_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let active = self.agents.active;
        // Cockpit と同じ見張り。同居が 0 なら git を 1 回も叩かない。
        self.sync_conflicts();
        let conflicts = self.conflicts.report().clone();
        let radar = self.conflict_radar.report().clone();
        // PTY 画面の読み直し (parser のロック) は看板が「今」と言ったフレームだけ。
        // 看板を開けっぱなしでもアイドル時のコストがゼロに近くなる。
        let now_ms = self.supervisor.elapsed_ms();
        let fresh_tail = self.kanban_state.sample_due(now_ms);
        let cards: Vec<kanban::Card> = self
            .agents
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let running = s.running();
                let sup = self.supervisor.state_of(s.id);
                let rate_limited = s.rate_limited.is_some();
                // coordinator に割り当て中のタスクをカードのチップに出す
                let task = self
                    .coordinator
                    .tasks()
                    .iter()
                    .find(|t| t.assigned == Some(s.id) && !t.state.is_terminal())
                    .map(|t| t.title.clone());
                kanban::Card {
                    idx: i,
                    id: s.id,
                    icon: if s.icon.is_empty() {
                        "👾".into()
                    } else {
                        s.icon.clone()
                    },
                    title: s.title.clone(),
                    active: i == active,
                    column: kanban::column_for(running, s.attention, rate_limited, sup),
                    state_label: tr(kanban::state_label(running, s.attention, rate_limited, sup)),
                    uptime: s.uptime(),
                    unread: s.has_unread(),
                    rate_limited: s.rate_limited.clone(),
                    attention: s.attention && running,
                    running,
                    sup,
                    // 状態ラダー上位 2 段 (構造化プロトコル / ベンダーフック)。
                    // 生きている間は画面推定を一切使わない (CLAUDE.md 原則 #4)。
                    ladder: self.supervisor.ladder_of(s.id),
                    permission_badge: if s.is_permission_agent() {
                        s.approval_badge()
                    } else {
                        ""
                    },
                    // 指名スーパーエージェント (指揮官)。看板は同じ形のカードが
                    // 並ぶので、どれが指揮官かを枠と冠で一目で分かるようにする。
                    commander: self.super_agent_session == Some(s.id),
                    // 同じ作業ツリーの誰かとファイルを取り合っていたら ⚠。
                    // 取り合っているファイル名はホバーに畳む。
                    // **隔離済み worktree 同士**の衝突予測 (🛰 レーダー) も
                    // 同じ 1 本のバッジに畳む — 同じ意味の印を 2 つ並べない。
                    conflict: [
                        conflicts.has_agent(s.id).then(|| {
                            trf(
                                "⚠ 他のエージェントと同じファイルを触っています: {files}",
                                &[(
                                    "files",
                                    worktree::summarize_labels(&conflicts.labels_for(s.id), 3),
                                )],
                            )
                        }),
                        radar.card_hint(s.id),
                    ]
                    .into_iter()
                    .flatten()
                    .reduce(|a, b| format!("{a}\n{b}")),
                    can_cycle: s.permission_switch_hint().is_some(),
                    // カードの一言 + ホバープレビュー + アクティビティ分類の材料。
                    // サンプリング周期のフレームだけ実際に PTY を読む
                    // (それ以外は空 = kanban 側が前回ぶんを使い回す)。
                    tail_lines: if fresh_tail {
                        s.screen_tail_lines(10, 180)
                    } else {
                        Vec::new()
                    },
                    task,
                }
            })
            .collect();
        let presets: Vec<(String, String)> = self
            .cfg
            .agents
            .iter()
            .map(|p| (p.icon.clone(), p.name.clone()))
            .collect();

        // アクティビティフィード: supervisor の状態遷移履歴を新しい順に混ぜる
        let mut activity: Vec<kanban::ActivityEntry> = Vec::new();
        for s in &self.agents.sessions {
            let icon = if s.icon.is_empty() {
                "👾".to_string()
            } else {
                s.icon.clone()
            };
            for t in self.supervisor.history_of(s.id).iter().rev().take(12) {
                activity.push(kanban::ActivityEntry {
                    age_ms: now_ms.saturating_sub(t.at_ms),
                    icon: icon.clone(),
                    title: s.title.clone(),
                    text: trf("が「{state}」になりました", &[("state", tr(t.to.label()))]),
                    detail: t.reason.clone(),
                    // 遷移先状態の列色で塗る (フラグは関与させない)
                    column: kanban::column_for(true, false, false, Some(t.to)),
                });
            }
        }
        activity.sort_by_key(|e| e.age_ms);
        activity.truncate(60);

        // ライブペイン: 端末描画は Cockpit と同じ道 (`terminal::draw`) をそのまま使う。
        // 看板側は矩形を用意して呼ぶだけ — 端末を再実装しない。
        // 借用は分けて取る (kanban_state と agents.sessions は別フィールド)。
        let mini_font = (self.scaled_terminal_font() - 3.0).clamp(8.0, 14.0);
        let dead: std::collections::HashSet<u64> = self
            .agents
            .sessions
            .iter()
            .map(|s| s.id)
            .filter(|id| self.frame_guard.is_quarantined(&Subview::Session(*id)))
            .collect();
        let kanban_state = &mut self.kanban_state;
        let sessions = &mut self.agents.sessions;
        let live_theme = theme.clone();
        let mut live = |ui: &mut egui::Ui, idx: usize| -> Option<egui::Response> {
            let s = sessions.get_mut(idx)?;
            let sid = s.id;
            if dead.contains(&sid) {
                return None;
            }
            // 1 枚が壊れてもフレーム全体を捨てないための印 (Cockpit と同じ)
            Some(draw_subview(Subview::Session(sid), || {
                terminal::draw(ui, s, &live_theme, mini_font, true, true, false)
            }))
        };
        let acts = kanban::ui(
            kanban_state,
            ui,
            &theme,
            &cards,
            &presets,
            &activity,
            now_ms,
            fresh_tail,
            &mut live,
        );

        for act in acts {
            match act {
                kanban::KanbanAction::Launch(i) => self.launch_preset(i, ctx),
                kanban::KanbanAction::Select(i) => {
                    if i < self.agents.sessions.len() {
                        self.agents.active = i;
                    }
                }
                kanban::KanbanAction::Focus(i) => self.apply_cmd(Cmd::FocusAgent(i), ctx),
                kanban::KanbanAction::Approve(i) => {
                    // ペットの吹き出しと同じ手順: 画面のプロンプトに合った承認キーを
                    // 優先する (Bypass 警告は Enter だと「No, exit」になるため)。
                    let fallback = self.cfg.pet_approve_keys.clone();
                    let sent = self.agents.sessions.get_mut(i).map(|s| {
                        let ok = s.press_pet_approve_button(Some(&fallback));
                        (ok, s.title.clone())
                    });
                    if let Some((true, title)) = sent {
                        self.toast(trf("✔ 承認を送信: {title}", &[("title", title)]), true);
                    }
                }
                kanban::KanbanAction::Deny(i) => {
                    let keys = self.cfg.pet_deny_keys.clone();
                    let sent = self.agents.sessions.get_mut(i).map(|s| {
                        let ok = s.send_text(&keys);
                        if ok {
                            s.resolve_attention();
                        }
                        (ok, s.title.clone())
                    });
                    if let Some((true, title)) = sent {
                        self.toast(trf("✖ 拒否を送信: {title}", &[("title", title)]), true);
                    }
                }
                kanban::KanbanAction::Restart(i) => {
                    if let Err(e) = self.agents.restart(i, ctx) {
                        self.toast(e, false);
                    }
                }
                kanban::KanbanAction::Remove(i) => self.close_agent(i),
                kanban::KanbanAction::CyclePermission(i) => match self.agents.cycle_permission(i) {
                    Some(hint) => self.toast_warn(trf(
                        "🛡 権限モード切替を送信しました（{hint} / 画面を確認してください）",
                        &[("hint", hint.to_string())],
                    )),
                    None => self.toast(tr("このセッションは権限モード切替に未対応です"), false),
                },
                kanban::KanbanAction::Send { idx, text } => {
                    let live = self
                        .agents
                        .sessions
                        .get(idx)
                        .map(|s| (s.id, s.running(), s.title.clone()));
                    match live {
                        Some((id, true, title)) => {
                            if self.queue_submit(submit::Job::user(id, text)) {
                                self.toast(trf("✏ 指示を送信: {title}", &[("title", title)]), true);
                            }
                        }
                        Some((_, false, _)) => self.toast(tr("セッションが終了しています"), false),
                        None => {}
                    }
                }
                kanban::KanbanAction::Broadcast(text) => match self.queue_submit_all(&text) {
                    // None はコスト上限で止めたとき (理由は送信側が説明済み)
                    None => {}
                    Some(0) => self.toast(tr("実行中のエージェントがありません"), false),
                    Some(n) => self.toast(
                        trf("📣 {n} セッションへ送信しました", &[("n", n.to_string())]),
                        true,
                    ),
                },
                kanban::KanbanAction::OpenCockpit => {
                    self.cockpit = true;
                    self.kanban = false;
                }
                kanban::KanbanAction::Close => self.kanban = false,
            }
        }

        // ESC で閉じる (入力欄などにフォーカスが無いときだけ。
        // フルスクリーン救出の ESC は handle_shortcuts 側が先に消費する)。
        if self.kanban
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
            && ctx.memory(|m| m.focused().is_none())
        {
            self.kanban = false;
        }
    }

    // ─── UI: エージェントデッキ (deck) ──────────────────────────────
    //
    // 看板と同じ橋渡し構造: ここは「材料を写す」「返ってきた操作を実行する」だけ。
    // 判断と描画は deck.rs 側にある。

    /// デッキの副題に出す git ブランチ (作業ディレクトリごと・TTL 付きキャッシュ)。
    ///
    /// git は **UI スレッドでは回さない** (git.rs と同じ方針)。1 つの作業
    /// ディレクトリにつき 1 本だけ裏へ投げ、届いたら貼る。まだ届いていない
    /// 間は空文字を返すので、副題は「短縮 cwd だけ」に落ちる (欠落しても壊れない)。
    pub(super) fn deck_branch_of(&mut self, cwd: &Path) -> String {
        /// ブランチは checkout で変わるので、そこそこの周期で取り直す。
        const TTL: Duration = Duration::from_secs(20);
        while let Ok((dir, name)) = self.deck_branch_rx.try_recv() {
            self.deck_branch_pending.remove(&dir);
            self.deck_branches.insert(dir, (name, Instant::now()));
        }
        let cur = self.deck_branches.get(cwd);
        let fresh = matches!(cur, Some((_, at)) if at.elapsed() < TTL);
        let out = cur.map(|(n, _)| n.clone()).unwrap_or_default();
        if !fresh && !self.deck_branch_pending.contains(cwd) && !cwd.as_os_str().is_empty() {
            self.deck_branch_pending.insert(cwd.to_path_buf());
            let tx = self.deck_branch_tx.clone();
            let dir = cwd.to_path_buf();
            std::thread::spawn(move || {
                let name = Self::git_branch_at(&dir).unwrap_or_default();
                let _ = tx.send((dir, name));
            });
        }
        out
    }

    /// `dir` の git ブランチ名 (**バックグラウンド専用**)。
    /// detached HEAD なら短縮 SHA。非 repo / git 不在なら `None`。
    /// `.git/HEAD` を直接読まないのは worktree / submodule で壊れるため。
    pub(super) fn git_branch_at(dir: &Path) -> Option<String> {
        let run = |args: &[&str]| -> Option<String> {
            let out = crate::procx::hidden_command("git")
                .arg("-C")
                .arg(dir)
                .args(args)
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
                .filter(|s| !s.is_empty())
        };
        run(&["branch", "--show-current"]).or_else(|| run(&["rev-parse", "--short", "HEAD"]))
    }

    pub(super) fn deck_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let theme = self.theme.clone();
        let active = self.agents.active;
        let now_ms = self.supervisor.elapsed_ms();
        // PTY 画面の読み直しはデッキが「今」と言ったフレームだけ (看板と同じ作法)。
        let fresh_tail = self.deck_state.sample_due(now_ms);

        // 副題のブランチだけ先に解決しておく (`&mut self` が要るので一覧の外で)。
        let cwds: Vec<PathBuf> = self.agents.sessions.iter().map(|s| s.cwd.clone()).collect();
        let branches: Vec<String> = cwds.iter().map(|c| self.deck_branch_of(c)).collect();

        // 一覧に描くのは「名前」と「ブランチ • 場所」だけ。承認件数・未読・
        // 稼働時間といった状態は看板 (kanban.rs) の担当なので写さない。
        let live: Vec<deck::LiveRow> = self
            .agents
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| deck::LiveRow {
                idx: i,
                id: s.id,
                title: s.title.clone(),
                branch: branches.get(i).cloned().unwrap_or_default(),
                cwd: s.cwd.clone(),
                command: s.command.clone(),
                running: s.running(),
                attention: s.attention && s.running(),
                rate_limited: s.rate_limited.is_some(),
                sup: self.supervisor.state_of(s.id),
                active: i == active,
                tail_lines: if fresh_tail {
                    s.screen_tail_lines(10, 180)
                } else {
                    Vec::new()
                },
            })
            .collect();
        let launchers: Vec<deck::LauncherRow> = self
            .cfg
            .agents
            .iter()
            .enumerate()
            .map(|(i, p)| deck::LauncherRow {
                idx: i,
                icon: p.icon.clone(),
                name: p.name.clone(),
            })
            .collect();
        // 走査中 = ブランチ解決が飛んでいる間だけ。届けば止まるので、
        // アイドル時の予約は 0 枚のまま。
        let scanning = !self.deck_branch_pending.is_empty();

        // ライブ端末は Cockpit / 看板とまったく同じ道 (`terminal::draw`) を通す。
        let mini_font = (self.scaled_terminal_font() - 2.0).clamp(8.0, 16.0);
        let dead: HashSet<u64> = self
            .agents
            .sessions
            .iter()
            .map(|s| s.id)
            .filter(|id| self.frame_guard.is_quarantined(&Subview::Session(*id)))
            .collect();
        let sessions = &mut self.agents.sessions;
        let live_theme = theme.clone();
        let mut draw = |ui: &mut egui::Ui, id: u64| -> Option<egui::Response> {
            let s = sessions.iter_mut().find(|s| s.id == id)?;
            if dead.contains(&id) {
                return None;
            }
            // 1 枚が壊れてもフレーム全体を捨てないための印 (Cockpit と同じ)
            Some(draw_subview(Subview::Session(id), || {
                terminal::draw(ui, s, &live_theme, mini_font, true, true, false)
            }))
        };
        let acts = deck::ui(
            &mut self.deck_state,
            ui,
            &theme,
            &live,
            &launchers,
            now_ms,
            fresh_tail,
            scanning,
            &mut draw,
        );

        for act in acts {
            match act {
                deck::DeckAction::Select(i) => {
                    if i < self.agents.sessions.len() {
                        self.agents.active = i;
                    }
                }
                deck::DeckAction::Launch(i) => self.launch_preset(i, ctx),
                deck::DeckAction::Rename { id, title } => {
                    if let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == id) {
                        s.title = title;
                    }
                    self.persist_session();
                }
                deck::DeckAction::Duplicate(i) => self.duplicate_agent(i, ctx),
                deck::DeckAction::Stop(i) => self.close_agent(i),
                deck::DeckAction::Restart(i) => {
                    if let Err(e) = self.agents.restart(i, ctx) {
                        self.toast(e, false);
                    }
                }
                deck::DeckAction::Reorder { from, to } => self.reorder_agent(from, to),
                deck::DeckAction::Close => self.deck = false,
            }
        }

        // ESC で閉じる (入力欄・端末にフォーカスが無いときだけ)。
        if self.deck
            && ctx.input(|i| i.key_pressed(egui::Key::Escape))
            && ctx.memory(|m| m.focused().is_none())
        {
            self.deck = false;
        }
    }

    /// セッション `i` と同じプリセット・同じ作業ディレクトリでもう 1 本起こす。
    ///
    /// 起動コマンドは**プリセットの素の値**を使う。走っているセッションの
    /// コマンドをそのまま複製すると `--resume <id>` が付いたままになり、
    /// 「同じ会話を 2 枚開く」事故になるため。
    pub(super) fn duplicate_agent(&mut self, i: usize, ctx: &egui::Context) {
        let Some(s) = self.agents.sessions.get(i) else {
            return;
        };
        let (preset_name, cwd, command) = (s.preset_name.clone(), s.cwd.clone(), s.command.clone());
        let idx = self
            .cfg
            .agents
            .iter()
            .position(|p| p.name == preset_name)
            .or_else(|| {
                // プリセット名が変わっていても、同じ CLI のプリセットがあれば拾う
                let bin = agents::spec_for_command(&command).map(|sp| sp.bin)?;
                self.cfg.agents.iter().position(|p| {
                    agents::spec_for_command(&p.command).is_some_and(|x| x.bin == bin)
                })
            });
        match idx {
            Some(n) => {
                let cmd = self.cfg.agents[n].command.clone();
                self.launch_preset_with(n, cmd, &cwd, ctx);
            }
            None => self.toast(
                trf(
                    "複製できません: {name} のプリセットが見つかりません",
                    &[("name", preset_name)],
                ),
                false,
            ),
        }
    }

    /// セッションの並び替え (デッキの ⌥↑ / ⌥↓)。アクティブの指し先も付け替える。
    pub(super) fn reorder_agent(&mut self, from: usize, to: usize) {
        let n = self.agents.sessions.len();
        if from >= n || to >= n || from == to {
            return;
        }
        self.agents.sessions.swap(from, to);
        // 紫枠 (アクティブ) は「同じセッション」を指し続ける
        if self.agents.active == from {
            self.agents.active = to;
        } else if self.agents.active == to {
            self.agents.active = from;
        }
        self.persist_session();
    }

    /// エディタタブをドラッグで並べ替える (`from` → `to`)。
    ///
    /// **掴んでいたタブはアクティブのまま**動く。デッキの `reorder_agent` と
    /// 同じ約束だが、ドラッグは離れた位置へ落とせるので swap ではなく
    /// remove + insert で動かす (間のタブが 1 つずつずれる)。
    pub(super) fn reorder_tab(&mut self, from: usize, to: usize) {
        let n = self.editor.buffers.len();
        if from >= n || to >= n || from == to {
            return;
        }
        let b = self.editor.buffers.remove(from);
        self.editor.buffers.insert(to, b);
        // アクティブは「同じタブ」を指し続ける (添字と ID の取り違え防止)
        if let Some(a) = self.editor.active {
            self.editor.active = Some(reorder_active(a, from, to));
        }
        // 検索のヒット位置はバッファに紐づくので、並びが変わっても持ち越さない
        self.find.current = None;
        self.find_hits = None;
        self.persist_session();
    }

    /// blame のガターをクリックしたコミットの差分をタブで開く。
    ///
    /// 開けない (git が無い / 非 repo / 既に消えたコミット) ときは
    /// **静かに何もしない** — blame はユーザーが明示的に呼んだ機能ではないので、
    /// エラーダイアログもトーストも出さない。
    pub(super) fn open_commit_diff(&mut self, sha: &str) {
        let Some(path) = self
            .editor
            .active
            .and_then(|i| self.editor.buffers[i].path.clone())
        else {
            return;
        };
        let Some((top, _)) = self.gitinfo.locate(&path) else {
            return;
        };
        self.open_commit_diff_at(&top, sha);
    }

    /// チェックポイント一覧を描き、裏のスレッドから返ってきた結果を捌く。
    ///
    /// **アイドル時のコストはゼロ** — 一覧を閉じていれば `ui` は即 return し、
    /// 走行中の仕事が無ければ `poll` も即 return する (再描画も要求しない)。
    pub(super) fn checkpoint_ui(&mut self, ctx: &egui::Context) {
        self.checkpoints.ui(ctx);
        let Some(done) = self.checkpoints.poll() else {
            return;
        };
        match done {
            // 指示のたびの自動取得は黙って済ませる (通知が溢れると読まれない)。
            checkpoint::Done::Captured { cp, announce } => {
                if announce {
                    self.toast(
                        trf(
                            "⏱ チェックポイントを取りました ({sha})",
                            &[("sha", cp.sha.chars().take(8).collect::<String>())],
                        ),
                        true,
                    );
                }
            }
            checkpoint::Done::Skipped { announce } => {
                if announce {
                    self.toast(tr("前回から変更がないので取りませんでした"), true);
                }
            }
            checkpoint::Done::Listed(_) => {}
            checkpoint::Done::Restored { restored, kept } => {
                self.toast(
                    trf(
                        "⏱ {n} 件を書き戻しました (スナップショットに無かった {k} 件はそのまま)",
                        &[("n", restored.to_string()), ("k", kept.to_string())],
                    ),
                    true,
                );
                // 作業ツリーが変わったので、git の色付けと開いているファイルを
                // 取り直す (裏のスキャンへ依頼するだけ。ここでは待たない)。
                self.gitinfo.request_refresh();
                // 開いているタブは既存の外部変更ウォッチャ
                // (`check_external_changes` → `Editor::check_external`) が
                // 読み直す。スロットルを開けて次のティックで必ず拾わせる。
                self.ext_check_at = None;
            }
            checkpoint::Done::Diff(label, text) => {
                if text.trim().is_empty() {
                    self.toast(tr("このチェックポイントと今との差分はありません"), true);
                    return;
                }
                let title = trf("⏱ チェックポイント {sha}", &[("sha", label)]);
                let id = self.editor.open_virtual(
                    title,
                    text,
                    crate::editor::BufferKind::CheckpointDiff,
                );
                // 同じタブを使い回すので古いパース結果は捨てる。
                self.commit_diff_cache.remove(&id);
            }
            checkpoint::Done::Failed(e) => self.toast(e, false),
        }
    }

    /// 🕰 ローカルヒストリの一覧を描き、裏のスレッドの結果を捌く。
    ///
    /// **アイドル時のコストはゼロ** — 一覧を閉じていれば描画は即 return し、
    /// 依頼が 1 つも無ければ受信口すら作られていないので `poll` も即 return する。
    pub(super) fn local_history_ui(&mut self, ctx: &egui::Context) {
        self.local_history.ui(ctx);
        // 復元は「戻した」と「一覧が変わった」を続けて返すので、溜まっている
        // ぶんはこのフレームで全部捌く (次の再描画まで持ち越さない)。
        while let Some(done) = self.local_history.poll() {
            match done {
                local_history::Done::Diff {
                    title,
                    path,
                    old,
                    new,
                } => {
                    // **差分ビューアは 2 つ書かない。** 既存の比較ウィンドウへ渡す。
                    let f = crate::diff::diff_texts(
                        &trf("{p} ({t})", &[("p", path.clone()), ("t", title)]),
                        &trf("{p} (今)", &[("p", path)]),
                        &old,
                        &new,
                    );
                    self.show_compare(tr("🕰 ローカルヒストリ"), f);
                }
                local_history::Done::Restored { .. } => {
                    // 作業ツリーが変わった。git の色付けと開いているタブを
                    // 取り直す (依頼を出すだけ。ここでは待たない)。
                    self.gitinfo.request_refresh();
                    self.ext_check_at = None;
                }
                local_history::Done::Failed(e) => self.toast(e, false),
                local_history::Done::Scanned { .. } | local_history::Done::Loaded(_) => {}
            }
        }
    }

    /// リポジトリを明示して開く版。Git パネルの履歴一覧とパレットの
    /// 履歴コマンドの両方から使う (アクティブなバッファに依らずリポジトリが決まる)。
    pub(super) fn open_commit_diff_at(&mut self, top: &Path, sha: &str) {
        let Ok((title, text)) = git::commit_diff(top, sha) else {
            return;
        };
        let id = self
            .editor
            .open_virtual(title, text, crate::editor::BufferKind::CommitDiff);
        // 同じタブを使い回すことがあるので古いパース結果は捨てる
        self.commit_diff_cache.remove(&id);
        self.persist_session();
    }

    // ─── パレットから撃つ git 操作 (commit / push / pull / 履歴) ───────
    //
    // `git_panel.rs` は commit / push を**意図的にスコープ外**にしている
    // (あちらの冒頭コメント)。ここはそのパネルの中身に触らず、同じ
    // 「別スレッドで走らせて結果だけ受け取る」作法で別に持つ。
    // UI は絶対にブロックしない。

    /// 対象リポジトリ。開いているファイルの所属を最優先し、無ければ
    /// ワークスペースのルートから最初に見つかった git リポジトリ。
    pub(super) fn git_ops_repo(&self) -> Option<PathBuf> {
        if let Some(p) = self.active_file_path() {
            if let Some((top, _)) = self.gitinfo.locate(&p) {
                return Some(top);
            }
        }
        self.roots.iter().find_map(|r| git::discover_toplevel(r))
    }

    /// コミットメッセージの入力を開く。`all` なら追跡中の変更を全部
    /// ステージしてからコミットする (`git commit -a`)。
    pub(super) fn open_commit_prompt(&mut self, all: bool) {
        if self.git_ops_repo().is_none() {
            self.toast(tr("git リポジトリが見つかりません"), false);
            return;
        }
        self.git_ops.commit_open = true;
        self.git_ops.commit_all = all;
        self.git_ops.commit_focus = true;
    }

    /// git のジョブを別スレッドで走らせる。走行中は 1 本だけ。
    pub(super) fn run_git_job(&mut self, job: GitJob, ctx: &egui::Context) {
        if self.git_ops.job.is_some() {
            self.toast(
                trf(
                    "git {label} の実行中です",
                    &[("label", self.git_ops.job_label.clone())],
                ),
                false,
            );
            return;
        }
        let Some(repo) = self.git_ops_repo() else {
            self.toast(tr("git リポジトリが見つかりません"), false);
            return;
        };
        let label = job.label();
        let args = job.args();
        let (tx, rx) = mpsc::channel();
        let ctx2 = ctx.clone();
        let label2 = label.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-git-ops".into())
            .spawn(move || {
                let out = crate::procx::hidden_command("git")
                    .arg("-C")
                    .arg(&repo)
                    .args(&args)
                    .output();
                // git の言い分 (stderr) は加工せずそのまま画面へ出す。
                let msg = match out {
                    Ok(o) if o.status.success() => {
                        (trf("{l} 完了", &[("l", label2.clone())]), true)
                    }
                    Ok(o) => {
                        let err = crate::textenc::decode_output(&o.stderr);
                        let err = if err.trim().is_empty() {
                            crate::textenc::decode_output(&o.stdout)
                        } else {
                            err
                        };
                        (
                            trf(
                                "{l} 失敗: {e}",
                                &[("l", label2.clone()), ("e", first_lines(&err, 3))],
                            ),
                            false,
                        )
                    }
                    Err(e) => (
                        trf(
                            "{l} 失敗: {e}",
                            &[("l", label2.clone()), ("e", e.to_string())],
                        ),
                        false,
                    ),
                };
                let _ = tx.send(msg);
                crate::perf::repaint(&ctx2, "git_job_done");
            });
        match spawned {
            Ok(_) => {
                self.git_ops.job = Some(rx);
                self.git_ops.job_label = label;
            }
            Err(e) => self.toast(
                trf("git を起動できません: {e}", &[("e", e.to_string())]),
                false,
            ),
        }
    }

    /// コミット履歴の一覧を開き、裏で `git log` を取りに行く。
    pub(super) fn open_git_history(&mut self, ctx: &egui::Context) {
        let Some(repo) = self.git_ops_repo() else {
            self.toast(tr("git リポジトリが見つかりません"), false);
            return;
        };
        self.git_ops.history_open = true;
        self.git_ops.history_query.clear();
        if self.git_ops.history_rx.is_some() {
            return; // 取得中
        }
        self.git_ops.history_busy = true;
        self.git_ops.history.clear();
        let (tx, rx) = mpsc::channel();
        let ctx2 = ctx.clone();
        let n = GIT_HISTORY_MAX.to_string();
        let spawned = std::thread::Builder::new()
            .name("zv-git-log".into())
            .spawn(move || {
                let mut out: Vec<(String, String)> = Vec::new();
                // 区切りは 0x1F (Unit Separator)。件名にも著者名にも現れない。
                if let Ok(o) = crate::procx::hidden_command("git")
                    .arg("-C")
                    .arg(&repo)
                    .args([
                        "log",
                        "--no-color",
                        "-n",
                        &n,
                        "--pretty=%h%x1f%an%x1f%ar%x1f%s",
                    ])
                    .output()
                {
                    if o.status.success() {
                        for line in crate::textenc::decode_output(&o.stdout).lines() {
                            let mut it = line.split('\u{1f}');
                            let (Some(sha), Some(an), Some(ar), Some(sub)) =
                                (it.next(), it.next(), it.next(), it.next())
                            else {
                                continue;
                            };
                            out.push((sha.to_string(), format!("{sub}  —  {an} · {ar}")));
                        }
                    }
                }
                let _ = tx.send(out);
                crate::perf::repaint(&ctx2, "git_history_done");
            });
        match spawned {
            Ok(_) => self.git_ops.history_rx = Some(rx),
            Err(e) => {
                self.git_ops.history_busy = false;
                self.toast(
                    trf("git を起動できません: {e}", &[("e", e.to_string())]),
                    false,
                );
            }
        }
    }

    /// 走行中の git ジョブ / 履歴取得の結果を回収する (毎フレーム)。
    pub(super) fn git_ops_poll(&mut self) {
        let done = match &self.git_ops.job {
            Some(rx) => match rx.try_recv() {
                Ok(m) => Some(Some(m)),
                Err(mpsc::TryRecvError::Disconnected) => Some(None),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        if let Some(m) = done {
            self.git_ops.job = None;
            self.git_ops.job_label.clear();
            if let Some((msg, ok)) = m {
                self.toast(msg, ok);
            }
            // 一覧・ガター・レビューを取り直す
            self.gitinfo.request_refresh();
            self.git_panel.invalidate();
            self.review.invalidate();
        }
        let hist = match &self.git_ops.history_rx {
            Some(rx) => match rx.try_recv() {
                Ok(list) => Some(Some(list)),
                Err(mpsc::TryRecvError::Disconnected) => Some(None),
                Err(mpsc::TryRecvError::Empty) => None,
            },
            None => None,
        };
        if let Some(list) = hist {
            self.git_ops.history = list.unwrap_or_default();
            self.git_ops.history_rx = None;
            self.git_ops.history_busy = false;
        }
    }

    /// コミットメッセージの入力窓。Enter で確定、Esc で取り消し。
    pub(super) fn git_commit_window(&mut self, ctx: &egui::Context) {
        if !self.git_ops.commit_open {
            return;
        }
        let all = self.git_ops.commit_all;
        let focus = std::mem::take(&mut self.git_ops.commit_focus);
        let mut msg = std::mem::take(&mut self.git_ops.commit_msg);
        let mut submit = false;
        let mut cancel = false;
        egui::Window::new(if all {
            tr("すべての変更をコミット")
        } else {
            tr("ステージした変更をコミット")
        })
        .collapsible(false)
        .resizable(false)
        .anchor(Align2::CENTER_TOP, egui::vec2(0.0, RENAME_WINDOW_Y))
        .show(ctx, |ui| {
            ui.set_width(GIT_COMMIT_WINDOW_W);
            let r = ui.add(
                egui::TextEdit::singleline(&mut msg)
                    .hint_text(tr("コミットメッセージ"))
                    .desired_width(f32::INFINITY),
            );
            if focus {
                r.request_focus();
            }
            // IME 変換の確定 Enter をコミットに使わない (Windows / Linux 対策)
            let ime = ui.input(|i| i.events.iter().any(|e| matches!(e, egui::Event::Ime(_))));
            if r.lost_focus() && !ime && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                submit = true;
            }
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                if ui.button(tr("コミット")).clicked() {
                    submit = true;
                }
                if ui.button(tr("取り消し")).clicked() {
                    cancel = true;
                }
            });
        });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        self.git_ops.commit_msg = msg;
        if cancel {
            self.git_ops.commit_open = false;
            self.git_ops.commit_msg.clear();
            return;
        }
        if submit {
            let message = self.git_ops.commit_msg.trim().to_string();
            if message.is_empty() {
                self.toast(tr("コミットメッセージを入力してください"), false);
                self.git_ops.commit_focus = true;
                return;
            }
            self.git_ops.commit_open = false;
            self.git_ops.commit_msg.clear();
            self.run_git_job(GitJob::Commit { message, all }, ctx);
        }
    }

    /// コミット履歴の一覧。選ぶとそのコミットの差分タブが開く。
    pub(super) fn git_history_window(&mut self, ctx: &egui::Context) {
        if !self.git_ops.history_open {
            return;
        }
        let theme = self.theme.clone();
        let busy = self.git_ops.history_busy;
        let mut query = std::mem::take(&mut self.git_ops.history_query);
        let mut open = true;
        let mut pick: Option<String> = None;
        let history = std::mem::take(&mut self.git_ops.history);
        egui::Window::new(tr("コミット履歴"))
            .open(&mut open)
            .default_width(REF_WINDOW_W)
            .show(ctx, |ui| {
                if busy {
                    ui.label(
                        RichText::new(tr("読み込み中…"))
                            .color(theme.text_dim)
                            .small(),
                    );
                    return;
                }
                if history.is_empty() {
                    ui.label(
                        RichText::new(tr("コミットがありません"))
                            .color(theme.text_dim)
                            .small(),
                    );
                    return;
                }
                ui.add(
                    egui::TextEdit::singleline(&mut query)
                        .hint_text(tr("件名・著者で絞り込み"))
                        .desired_width(f32::INFINITY),
                );
                let pq = fuzzy::PreparedQuery::new(query.trim());
                egui::ScrollArea::vertical()
                    .max_height(REF_WINDOW_H)
                    .show(ui, |ui| {
                        for (sha, line) in &history {
                            if pq.score(line).is_none() {
                                continue;
                            }
                            // どの幅でも行からはみ出さない (全文はホバー)
                            let r = ui.add(
                                egui::Label::new(RichText::new(format!("{sha}  {line}")).small())
                                    .truncate()
                                    .sense(egui::Sense::click()),
                            );
                            if r.on_hover_text(line).clicked() {
                                pick = Some(sha.clone());
                            }
                        }
                    });
            });
        self.git_ops.history = history;
        self.git_ops.history_query = query;
        self.git_ops.history_open = open;
        if let Some(sha) = pick {
            if let Some(repo) = self.git_ops_repo() {
                self.open_commit_diff_at(&repo, &sha);
            }
            self.git_ops.history_open = false;
        }
    }
}
