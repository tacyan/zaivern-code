use super::*;

impl ZaivernApp {
    // ─── UI: terminal panel ─────────────────────────────────────────

    pub(super) fn terminal_panel(&mut self, ctx: &egui::Context) {
        let theme = self.theme.clone();
        // デッキ表示中も畳む: 同じ端末を 2 か所で描かせない (egui の Id が衝突する)
        // うえ、デッキは端末を全高で見せる画面なので下部パネルは邪魔になる。
        // 中央ビューが Cockpit / デッキ / 看板のときは畳む (同じ端末を 2 か所で
        // 描かせない = egui の Id 衝突と絵の重なりを構造的に防ぐ)。
        // 看板は「全エージェントを俯瞰する」画面なので、下部 300px の中ではなく
        // 中央パネル全面に出す (下部パネルの中だと 1/3 しか見えなかった)。
        let show = self.agents.panel_open && self.center == CenterView::Editor;
        let mut launch: Option<usize> = None;
        let mut restart: Option<usize> = None;
        let mut remove: Option<usize> = None;
        let mut cycle: Option<usize> = None;
        let mut open_log: Option<PathBuf> = None;
        // 承認キューのキー操作は描画後にまとめて実行する (描画中に
        // self.agents を可変で借りると、端末描画と衝突するため)。
        let mut approval_cmds: Vec<(u64, agents::approvals::Command)> = Vec::new();
        // MCP パネルの要求 (ファイルを開く / 書き戻す / 再走査) も描画後に実行する。
        let mut mcp_action = mcp::McpAction::None;
        // Skills パネルの要求 (開く / 送る / コピー / 再走査) も描画後に実行する。
        let mut skills_action = skills::SkillAction::None;
        let mut spec_action = spec::SpecAction::None;

        let panel = egui::TopBottomPanel::bottom("zv-terminal")
            .resizable(true)
            .default_height(300.0)
            .min_height(140.0)
            .frame(
                egui::Frame::none()
                    .fill(theme.panel)
                    .inner_margin(egui::Margin::same(6.0)),
            )
            .show_animated(ctx, show, |ui| {
                // egui のボトムパネルは「中身が実際に使った矩形」を次フレームの
                // 高さとして保存するため、中身がパネル高さを埋め切らないと
                // リサイズバーが毎フレームずり落ちていく (看板タブのチャート等)。
                // 先に全高を消費してドラッグした高さを常に維持する。
                ui.set_min_height(ui.available_height());
                ui.horizontal(|ui| {
                    let controls_w = 150.0;
                    egui::ScrollArea::horizontal()
                        .id_salt("term-tabs")
                        .max_width((ui.available_width() - controls_w).max(120.0))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                let active_ix = self.agents.active;
                                let mut set_active: Option<usize> = None;
                                let mut set_unread: Option<usize> = None;
                                let mut set_read: Option<usize> = None;
                                for (i, s) in self.agents.sessions.iter().enumerate() {
                                    let dot = if s.running() {
                                        if s.attention {
                                            RichText::new("●").size(10.0).color(theme.warn)
                                        } else {
                                            RichText::new("●").size(10.0).color(theme.ok)
                                        }
                                    } else {
                                        RichText::new("○").size(10.0).color(theme.err)
                                    };
                                    ui.label(dot);
                                    let badge = if s.is_permission_agent() {
                                        s.approval_badge()
                                    } else {
                                        ""
                                    };
                                    let r = ui.selectable_label(
                                        i == active_ix,
                                        format!("{}{} {}", badge, s.icon, s.title),
                                    );
                                    if s.has_unread() && i != active_ix {
                                        ui.label(
                                            RichText::new("◆").size(9.0).color(theme.accent),
                                        )
                                        .on_hover_text(tr("最後に見てから新しい出力があります"));
                                    }
                                    if let Some(line) = &s.rate_limited {
                                        ui.label(RichText::new("⏳").color(theme.warn))
                                            .on_hover_text(trf(
                                                "レート制限/使用上限: {line}",
                                                &[("line", line.clone())],
                                            ));
                                    }
                                    if r.clicked() {
                                        set_active = Some(i);
                                    }
                                    r.context_menu(|ui| {
                                        if let Some(hint) = s.permission_switch_hint() {
                                            if ui.button(format!("🛡 {}", tr(hint))).clicked() {
                                                cycle = Some(i);
                                                ui.close_menu();
                                            }
                                        }
                                        if s.has_unread() {
                                            if ui.button(tr("✓ 既読にする")).clicked() {
                                                set_read = Some(i);
                                                ui.close_menu();
                                            }
                                        } else if ui
                                            .button(tr("📩 あとで見る (未読にする)"))
                                            .clicked()
                                        {
                                            set_unread = Some(i);
                                            ui.close_menu();
                                        }
                                        if ui.button(tr("⟳ 再起動")).clicked() {
                                            restart = Some(i);
                                            ui.close_menu();
                                        }
                                        if ui.button(tr("✕ 閉じる")).clicked() {
                                            remove = Some(i);
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
                                        // 後回し宣言 = 次に待機へ戻ったら
                                        // もう一度だけ鳴らす
                                        self.work_gate.forget(id);
                                    }
                                }
                                if let Some(i) = set_read {
                                    if let Some(s) = self.agents.sessions.get_mut(i) {
                                        s.acknowledge();
                                    }
                                }
                                if let Some(i) = set_active {
                                    self.agents.active = i;
                                    // タブで選び直したら、その端末をアクティブな
                                    // 入力先 (フォーカス) にする。看板 / MCP /
                                    // Skills タブ表示中なら端末ビューへ戻す。
                                    self.kanban = false;
                                    self.mcp_view = false;
                                    self.skills_view = false;
                self.spec_view = false;
                                    self.term_focus_pending = true;
                                    self.agents.sessions[i].acknowledge();
                                }
                            });
                        });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button("⌄").on_hover_text(trf("パネルを隠す ({key})", &[("key", self.key_hint(BindAction::ToggleTerminal))])).clicked() {
                            self.agents.panel_open = false;
                        }
                        // 承認タブ: 統合承認キュー。待ち件数を数字で添える
                        // (0 件でもタブ自体は出す — どこにあるか分からなくなるため)
                        let n_pending = self.agents.approvals.pending_len();
                        let ap_label = if n_pending > 0 {
                            format!("{} {n_pending}", tr("🛡 承認"))
                        } else {
                            tr("🛡 承認")
                        };
                        let ap_btn = ui.selectable_label(
                            self.approvals_view,
                            RichText::new(ap_label).color(if n_pending > 0 {
                                theme.warn
                            } else {
                                theme.text
                            }),
                        );
                        if ap_btn
                            .on_hover_text(tr(
                                "統合承認キュー — 全エージェントの承認要求を 1 本にまとめて捌きます\n\
                                 Y=承認 / A=この種別を全て承認 / ⇧A=常に許可 / N=拒否 / ⇧N=常に拒否",
                            ))
                            .clicked()
                        {
                            self.approvals_view = !self.approvals_view;
                            if self.approvals_view {
                                self.kanban = false;
                                self.mcp_view = false;
                                self.skills_view = false;
                self.spec_view = false;
                            }
                        }
                        // MCP タブ: 全エージェント横断の MCP サーバ一覧。
                        // 件数は **0 のとき出さない** (常に 0 のバッジを作らない)。
                        let mcp_label = match self.mcp.badge() {
                            Some(n) => format!("{} {n}", tr("🔌 MCP")),
                            None => tr("🔌 MCP"),
                        };
                        if ui
                            .selectable_label(self.mcp_view, RichText::new(mcp_label))
                            .on_hover_text(tr(
                                "MCP サーバ — .mcp.json / .cursor / .vscode / \
                                 .claude.json / .codex / .gemini を横断して一覧します\n\
                                 env の値は表示しません (キー名と設定済みかだけ)",
                            ))
                            .clicked()
                        {
                            self.mcp_view = !self.mcp_view;
                            if self.mcp_view {
                                self.kanban = false;
                                self.approvals_view = false;
                                self.skills_view = false;
                self.spec_view = false;
                                // 開いた回だけ読み直す (毎フレーム I/O にしない)
                                self.mcp.invalidate();
                            }
                        }
                        // Skills タブ: Skill と slash command を 1 枚の表に。
                        // 件数は **0 のとき出さない** (常に 0 のバッジを作らない)。
                        let sk_label = match self.skills.badge() {
                            Some(n) => format!("{} {n}", tr("🧩 Skills")),
                            None => tr("🧩 Skills"),
                        };
                        if ui
                            .selectable_label(self.skills_view, RichText::new(sk_label))
                            .on_hover_text(tr(
                                "Skills / slash command — .claude/skills と .claude/commands を \
                                 プロジェクト・ユーザー・プラグイン横断で一覧します\n\
                                 コマンドはそのままエージェントへ送れます",
                            ))
                            .clicked()
                        {
                            self.skills_view = !self.skills_view;
                            if self.skills_view {
                                self.kanban = false;
                                self.approvals_view = false;
                                self.mcp_view = false;
                                // 開いた回だけ読み直す (毎フレーム I/O にしない)
                                self.skills.invalidate();
                            }
                        }
                        // Spec タブ: 仕様の差分と「陳腐化の疑い」。
                        // 件数は **疑いがあるときだけ** 出す (常に 0 のバッジを作らない)。
                        let sp_label = match self.spec.badge() {
                            Some(n) => format!("{} {n}", tr("📐 Spec")),
                            None => tr("📐 Spec"),
                        };
                        let sp_btn = ui.selectable_label(
                            self.spec_view,
                            RichText::new(sp_label).color(if self.spec.badge().is_some() {
                                theme.warn
                            } else {
                                theme.text
                            }),
                        );
                        if sp_btn
                            .on_hover_text(tr(
                                "spec 駆動開発 — 変更は差分 (ADDED / MODIFIED / REMOVED) で書き、\n\
                                 統べているコードが動いたのに要件の文が動いていないものを\n\
                                 「陳腐化の疑い」として出します (判定は裏のスレッド)",
                            ))
                            .clicked()
                        {
                            self.spec_view = !self.spec_view;
                            if self.spec_view {
                                self.kanban = false;
                                self.approvals_view = false;
                                self.mcp_view = false;
                                self.skills_view = false;
                                // 開いた回だけ取り直す (アイドル時に走らせない)
                                self.spec.invalidate();
                            }
                        }
                        ui.menu_button("📜", |ui| {
                            ui.label(
                                RichText::new(tr("ターミナルログ (再起動しても残ります)"))
                                    .size(11.5)
                                    .color(theme.text_dim),
                            );
                            let logs = crate::session::list_term_logs(self.primary_root());
                            if logs.is_empty() {
                                ui.label(
                                    RichText::new(tr("まだありません")).color(theme.text_dim),
                                );
                            }
                            for p in logs.into_iter().take(30) {
                                let name = p
                                    .file_stem()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default();
                                let size = std::fs::metadata(&p)
                                    .map(|m| m.len() / 1024)
                                    .unwrap_or(0);
                                if ui.button(format!("📜 {name}  ({size} KB)")).clicked() {
                                    open_log = Some(p);
                                    ui.close_menu();
                                }
                            }
                        })
                        .response
                        .on_hover_text(tr("前回セッションのターミナルログを開く"));
                        ui.menu_button("＋", |ui| {
                            for (i, p) in self.cfg.agents.iter().enumerate() {
                                if ui.button(format!("{} {}", p.icon, p.name)).clicked() {
                                    launch = Some(i);
                                    ui.close_menu();
                                }
                            }
                        });
                        if !self.agents.sessions.is_empty() {
                            if ui.button("✕").on_hover_text(tr("セッションを閉じる")).clicked() {
                                remove = Some(self.agents.active);
                            }
                            if ui.button("⟳").on_hover_text(tr("再起動")).clicked() {
                                restart = Some(self.agents.active);
                            }
                            let permission_hint = self
                                .agents
                                .sessions
                                .get(self.agents.active)
                                .and_then(|s| s.permission_switch_hint());
                            if let Some(hint) = permission_hint {
                                if ui
                                    .button("🛡")
                                    .on_hover_text(trf(
                                        "{hint}\n\
                                         実行中セッションの画面表示を確認してください",
                                        &[("hint", hint.to_string())],
                                    ))
                                    .clicked()
                                {
                                    cycle = Some(self.agents.active);
                                }
                            }
                        }
                    });
                });

                ui.add_space(4.0);

                let font = self.scaled_terminal_font();
                let want_focus = self.term_focus_pending;
                // 予約を下ろすのは、この後で実際に端末を描くときだけ。
                // 「🛡 承認」タブ表示中やセッションが 0 件の間に消してしまうと、
                // タブ切替やエージェントを閉じた直後のフォーカス要求が握り潰され、
                // どこにも入力が届かなくなる。
                let view = bottom_view(
                    self.approvals_view,
                    self.mcp_view,
                    self.skills_view,
                    self.spec_view,
                );
                if view == BottomView::Terminal && !self.agents.sessions.is_empty() {
                    self.term_focus_pending = false;
                }
                match view {
                    BottomView::Approvals => {
                        // 「🛡 承認」タブ: 端末の代わりに統合承認キューを敷き詰める。
                        // フィールドを別々に借りる (agents は読み取り、表示状態は可変)。
                        let out = panels::approvals_ui(
                            ui,
                            &theme,
                            &self.agents.approvals,
                            &mut self.approvals_expanded,
                            &mut self.approvals_audit,
                            self.approvals_audit_cache.as_deref(),
                            now_epoch_secs(),
                        );
                        approval_cmds = out.commands;
                        if out.reload_audit {
                            // 描画中に I/O はしない。控えを捨てて、描画後に読み直す。
                            self.approvals_audit_cache = None;
                        }
                    }
                    BottomView::Mcp => {
                        // 「🔌 MCP」タブ: 走査は描画の外 (この下) で行う。
                        mcp_action = mcp::ui(ui, &theme, &mut self.mcp);
                    }
                    BottomView::Skills => {
                        // 「🧩 Skills」タブ: 走査は描画の外 (この下) で行う。
                        skills_action = skills::ui(ui, &theme, &mut self.skills);
                    }
                    BottomView::Spec => {
                        // 「📐 Spec」タブ: 走査は描画の外 (この下) で、しかも裏のスレッド。
                        spec_action = spec::ui(ui, &theme, &mut self.spec);
                    }
                    BottomView::Terminal => {
                        if let Some(s) = self.agents.active_session() {
                            let resp = terminal::draw(ui, s, &theme, font, true, true, true);
                            // タブ選択でアクティブになった端末へ、その場でフォーカスを渡す。
                            if want_focus {
                                resp.request_focus();
                            }
                        } else {
                            ui.vertical_centered(|ui| {
                                ui.add_space(20.0);
                                ui.label(
                                    RichText::new(tr(
                                        "セッションがありません — ＋ から起動してください",
                                    ))
                                    .color(theme.text_dim),
                                );
                            });
                        }
                    }
                }
            });
        if let Some(p) = &panel {
            tutorial::anchor(ctx, AnchorId::TerminalPanel, p.response.rect);
        }

        // 承認キューの実行 (描画後)。ここで初めて self を丸ごと可変で使える。
        for (id, cmd) in approval_cmds {
            self.resolve_approval(id, cmd);
        }
        // 監査ログの読み込みも描画の外で 1 回だけ (毎フレーム I/O にしない)。
        if self.approvals_view && self.approvals_audit && self.approvals_audit_cache.is_none() {
            let dir = self.agents.approvals.log_dir();
            self.approvals_audit_cache = Some(agents::approvals::read_audit_tail(
                &dir,
                APPROVAL_AUDIT_TAIL,
            ));
        }
        // MCP パネルの実行 (描画後)。走査も**このビューを出している間だけ** 1 回。
        self.apply_mcp_action(mcp_action);
        if self.mcp_view && !self.mcp.scanned {
            self.mcp.inventory = mcp::scan(&self.roots);
            self.mcp.scanned = true;
        }
        // Skills パネルの実行 (描画後)。走査も**このビューを出している間だけ** 1 回。
        self.apply_skills_action(skills_action, ctx);
        if self.skills_view && !self.skills.scanned {
            self.skills.entries = skills::scan(&self.roots);
            self.skills.scanned = true;
        }
        // spec パネルの実行 (描画後)。走査も**このビューを出している間だけ**で、
        // 中身は裏のスレッドへ逃げる (`poll` は決して待たない)。
        self.apply_spec_action(spec_action);
        if self.spec_view {
            let root = self.primary_root().to_path_buf();
            self.spec.poll(&root);
        }

        if let Some(i) = launch {
            self.launch_preset(i, ctx);
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
        if let Some(p) = open_log {
            self.open_term_log(&p);
        }
    }

    /// ターミナル生ログを ANSI 除去して読み取り専用の新規タブで開く。
    pub(super) fn open_term_log(&mut self, path: &Path) {
        // ローテート済みの直前分 (.old) があれば先頭へ繋げ、時系列で読めるようにする
        let old = std::fs::read(path.with_extension("log.old")).unwrap_or_default();
        let cur = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                self.toast(trf("ログを読めません: {e}", &[("e", e.to_string())]), false);
                return;
            }
        };
        let mut raw = old;
        raw.extend_from_slice(&cur);
        // 端末の生ログ。UTF-8 が基本だが、コードページ 932 のまま動く古いツールの
        // 出力が混ざることがあるので textenc へ通す (末尾が切れていても頭は読める)。
        let text = crate::textenc::decode_output(&raw);
        let clean: String = supervisor::strip_ansi(&text)
            .chars()
            .filter(|c| *c != '\r' && (*c >= ' ' || *c == '\n' || *c == '\t'))
            .collect();
        let title = format!(
            "📜 {}",
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default()
        );
        self.editor.new_untitled();
        let ed = self.edit_step();
        if let Some(i) = self.editor.active {
            let b = &mut self.editor.buffers[i];
            b.title = title;
            b.apply_edit(clean, ed);
        }
        self.cockpit = false;
    }

    // ─── 統合承認キュー (パネルの外側の実行部) ─────────────────────

    /// ボトムパネルを開いて「🛡 承認」ビューへ切り替える
    /// (ステータスバーのバッジ・パレットコマンドの共通の入口)。
    pub(super) fn open_approvals_panel(&mut self) {
        self.agents.panel_open = true;
        self.cockpit = false;
        self.kanban = false;
        self.mcp_view = false;
        self.skills_view = false;
        self.spec_view = false;
        self.approvals_view = true;
    }

    // ─── spec 駆動開発 (パネルの外側の実行部) ──────────────────────

    /// ボトムパネルを開いて「📐 Spec」ビューへ切り替える
    /// (コマンドパレット / タブの共通の入口)。
    /// spec パネルを開く。`pub(crate)` なのは [`crate::feature`] のレジストリ
    /// から呼ぶため (機能側が `app.rs` を編集せずに済むようにする配線口)。
    pub(crate) fn open_spec_panel(&mut self) {
        self.agents.panel_open = true;
        self.cockpit = false;
        self.kanban = false;
        self.approvals_view = false;
        self.mcp_view = false;
        self.skills_view = false;
        self.spec_view = true;
        // 開いた回だけ取り直す (アイドル時に走らせない)
        self.spec.invalidate();
    }

    /// spec パネルを「陳腐化の疑いだけ」に絞って開く。
    ///
    /// フィールドを `pub(crate)` にして機能側から触らせるのではなく、
    /// **操作をメソッドとして 1 つ出す**。内部表現を外へ漏らさずに済み、
    /// レジストリ側のクロージャも 1 行で書ける。
    pub(crate) fn open_spec_stale(&mut self) {
        self.open_spec_panel();
        self.spec.focus_stale();
    }

    /// spec パネルが積んだ要求を実行する。**描画の外でだけ呼ぶ** (I/O があるため)。
    pub(super) fn apply_spec_action(&mut self, action: spec::SpecAction) {
        match action {
            spec::SpecAction::None => {}
            spec::SpecAction::Rescan => self.spec.invalidate(),
            spec::SpecAction::Open(path) => self.open_path(&path),
            spec::SpecAction::Hand(text) => self.send_to_agent(text),
            spec::SpecAction::Write(req) => {
                let root = self.primary_root().to_path_buf();
                match spec::apply_write(req, &root) {
                    Ok(msg) => {
                        self.toast(msg, true);
                        // 書いた結果を次の走査で必ず拾う
                        self.spec.invalidate();
                    }
                    Err(e) => self.toast(e, false),
                }
            }
        }
    }

    // ─── MCP サーバ管理 (パネルの外側の実行部) ─────────────────────

    /// ボトムパネルを開いて「🔌 MCP」ビューへ切り替える
    /// (コマンドパレット / メニューの共通の入口)。
    pub(super) fn open_mcp_panel(&mut self) {
        self.agents.panel_open = true;
        self.cockpit = false;
        self.kanban = false;
        self.approvals_view = false;
        self.skills_view = false;
        self.spec_view = false;
        self.mcp_view = true;
        // 開いた回だけ読み直す (アイドル時に I/O しない)
        self.mcp.invalidate();
    }

    /// MCP パネルが積んだ要求を実行する。**描画の外でだけ呼ぶ** (I/O があるため)。
    pub(super) fn apply_mcp_action(&mut self, action: mcp::McpAction) {
        match action {
            mcp::McpAction::None => {}
            mcp::McpAction::Rescan => self.mcp.invalidate(),
            mcp::McpAction::Open(path) => {
                self.mcp.notice = None;
                self.open_path(&path);
            }
            mcp::McpAction::Toggle {
                path,
                source,
                name,
                disable,
            } => {
                match mcp::write_toggle(&path, source, &name, disable) {
                    Ok(()) => {
                        let msg = trf(
                            "{name} を{state}にしました (控え: {bak})",
                            &[
                                ("name", name.clone()),
                                ("state", if disable { tr("無効") } else { tr("有効") }),
                                (
                                    "bak",
                                    mcp::backup_path(&path)
                                        .file_name()
                                        .map(|s| s.to_string_lossy().into_owned())
                                        .unwrap_or_default(),
                                ),
                            ],
                        );
                        self.mcp.notice = Some((msg.clone(), true));
                        self.toast(msg, true);
                    }
                    Err(why) => {
                        let msg = trf(
                            "{name} を切り替えられません: {why}",
                            &[("name", name.clone()), ("why", why)],
                        );
                        self.mcp.notice = Some((msg.clone(), false));
                        self.toast(msg, false);
                    }
                }
                // 書けても書けなくても、実ファイルの今の姿を読み直す
                self.mcp.invalidate();
            }
        }
    }

    // ─── Skills / slash command (パネルの外側の実行部) ─────────────

    /// ボトムパネルを開いて「🧩 Skills」ビューへ切り替える
    /// (コマンドパレットの入口)。
    pub(super) fn open_skills_panel(&mut self) {
        self.agents.panel_open = true;
        self.cockpit = false;
        self.kanban = false;
        self.approvals_view = false;
        self.mcp_view = false;
        self.spec_view = false;
        self.skills_view = true;
        // 開いた回だけ読み直す (アイドル時に I/O しない)
        self.skills.invalidate();
    }

    /// Skills パネルが積んだ要求を実行する。**描画の外でだけ呼ぶ**。
    pub(super) fn apply_skills_action(&mut self, action: skills::SkillAction, ctx: &egui::Context) {
        match action {
            skills::SkillAction::None => {}
            skills::SkillAction::Rescan => self.skills.invalidate(),
            skills::SkillAction::Open(path) => self.open_path(&path),
            // slash command は `/名前 ` がそのまま呼び出し方なので、
            // ファイル送信 (@path) と同じ経路でアクティブな端末へ流す。
            skills::SkillAction::Send(text) => self.send_to_agent(text),
            skills::SkillAction::CopyPath(path) => {
                let p = path.display().to_string();
                ctx.copy_text(p.clone());
                self.toast(trf("📋 パスをコピーしました: {p}", &[("p", p)]), true);
            }
        }
    }

    /// 承認パネルで押された 1 コマンドを実行する。
    ///
    /// 3 つの副作用をここ 1 か所に集める:
    /// 1. `Resolution.replies` を持ち主の PTY セッションへ流す
    /// 2. `Resolution.policy` を config.toml の `[[approval_policies]]` へ残し、
    ///    エンジンへ配り直す
    /// 3. `refused_always` (権限昇格) を**はっきり伝える** — どんなポリシーでも
    ///    自動承認にはできない、という約束が破られていないことを利用者に見せる
    pub(super) fn resolve_approval(&mut self, id: u64, cmd: agents::approvals::Command) {
        let res = self.agents.approvals.apply(id, cmd);
        let (approve_keys, deny_keys) = agents::approvals::reply_keys();
        let mut sent = 0usize;
        let mut lost = 0usize;
        for (sid, action) in &res.replies {
            // **ACP (構造化プロトコル) の要求が先。** PTY のセッション ID とは
            // 別空間 (`acp::ACP_SESSION_ID_BASE`) なので取り違えは起きない。
            if self.acp.reply(*sid, *action) {
                sent += 1;
                continue;
            }
            let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == *sid) else {
                lost += 1;
                continue;
            };
            let ok = match action {
                agents::approvals::ReplyAction::None => false,
                agents::approvals::ReplyAction::Approve => {
                    s.press_pet_approve_button(Some(&approve_keys))
                }
                agents::approvals::ReplyAction::Deny => {
                    let ok = s.send_text(&deny_keys);
                    if ok {
                        s.resolve_attention();
                    }
                    ok
                }
            };
            if ok {
                sent += 1;
            } else {
                lost += 1;
            }
        }
        // 消えた要求の折りたたみ状態は残さない (ID は使い回されないが、無限に貯めない)
        self.approvals_expanded
            .retain(|k| self.agents.approvals.get(*k).is_some());
        // 監査ログの控えは古くなった
        self.approvals_audit_cache = None;

        if let Some(p) = res.policy.clone() {
            self.persist_approval_policy(&p);
        }
        if res.refused_always {
            self.toast_warn(tr(
                "⛔ 権限昇格は「常に許可」にできません — この 1 件だけ承認しました",
            ));
        }
        if sent > 0 {
            self.toast(
                trf("🛡 {n} 件の応答を送信しました", &[("n", sent.to_string())]),
                true,
            );
        }
        if lost > 0 {
            self.toast_warn(trf(
                "⚠ {n} 件は届きませんでした (セッションが終了しています)",
                &[("n", lost.to_string())],
            ));
        }
    }

    /// 生成されたポリシーを config.toml の `[[approval_policies]]` へ残す。
    ///
    /// `append_agent_preset` と同じ流儀 — **既存の行は 1 文字も触らず**末尾へ
    /// 1 ブロック足すだけなので、利用者の手書きコメントも並び順も残る。
    /// 書けなくてもアプリは止めない (このセッションの間は効いている)。
    pub(super) fn persist_approval_policy(&mut self, p: &agents::approvals::Policy) {
        let (scope, target) = p.scope.to_toml();
        let entry = config::ApprovalPolicy {
            kind: p.kind.as_str().to_string(),
            scope: scope.to_string(),
            target,
            decision: p.decision.as_str().to_string(),
        };
        // 同じ内容が既にあれば足さない (押すたびに config.toml が伸びない)
        if !self.cfg.approval_policies.contains(&entry) {
            self.cfg.approval_policies.push(entry.clone());
            if let Err(e) = append_approval_policy(&entry) {
                self.toast_warn(trf("承認ポリシーを保存できません: {e}", &[("e", e)]));
            }
        }
        // エンジンへ配り直す (次の要求から効く)
        config::publish_approval_policies(&self.cfg);
        self.toast(
            trf(
                "🛡 ポリシーを保存: {kind} / {scope} → {decision}",
                &[
                    ("kind", tr(p.kind.label())),
                    ("scope", scope.to_string()),
                    ("decision", p.decision.as_str().to_string()),
                ],
            ),
            true,
        );
    }
}
