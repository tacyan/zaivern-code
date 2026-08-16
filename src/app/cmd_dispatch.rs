use super::*;

impl ZaivernApp {
    pub(super) fn apply_cmd(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            // feature.rs のレジストリ経由。**ここが唯一のディスパッチ口**で、
            // 機能が何個増えてもこのアームは 1 つのまま (並列開発の衝突対策。
            // 経緯は feature.rs の冒頭)。未知の ID を黙って捨てると
            // 「押したのに無反応」になるので、必ずユーザーへ知らせる。
            Cmd::Feature(id) => {
                if !crate::feature::dispatch(self, ctx, id) {
                    self.toast_warn(trf(
                        "機能 {id} は登録されていません",
                        &[("id", id.to_string())],
                    ));
                }
            }
            // 差分ビュー: 表示モードの切替と変更箇所のジャンプ。
            // ctx へ書くだけで、実際の反映は差分ビュー自身が同フレームで行う。
            Cmd::ToggleDiffView => {
                let next = crate::diff::diff_mode(ctx).toggled();
                crate::diff::set_diff_mode(ctx, next);
                self.cfg.diff_view = next.config_str().into();
                config::save_state(&self.cfg);
                self.toast(trf("差分の表示: {m}", &[("m", next.label())]), true);
            }
            Cmd::DiffNextChange => crate::diff::request_jump(ctx, 1),
            Cmd::DiffPrevChange => crate::diff::request_jump(ctx, -1),
            // ⏱ チェックポイント (巻き戻し)。git は全て裏のスレッドで走るので、
            // ここは要求を出すだけ。結果は `checkpoint_ui` が受ける。
            Cmd::CheckpointList => self.checkpoints.open_list(ctx),
            Cmd::CheckpointNow => self.checkpoints.capture_now(ctx),
            // 🕰 ローカルヒストリ。走査も復元も裏のスレッドなので、ここは
            // 要求を出すだけ。結果は `local_history_ui` が受ける。
            Cmd::LocalHistoryOpen => {
                let p = self
                    .editor
                    .active
                    .and_then(|i| self.editor.buffers[i].path.clone());
                self.local_history.open_for(p.as_deref(), ctx);
            }
            // `]f` / `[f`: **ファイル間**のジャンプ (並列レビューの単位)。
            // 依頼を ctx に置くだけ。消化するのはレビュー画面自身なので、
            // 画面が出ていなければ 1 フレームで腐って何も起きない。
            Cmd::DiffNextFile => {
                self.open_review_panel();
                crate::diff::request_file_jump(ctx, 1);
            }
            Cmd::DiffPrevFile => {
                self.open_review_panel();
                crate::diff::request_file_jump(ctx, -1);
            }
            Cmd::DiffMarkViewed => {
                self.open_review_panel();
                crate::diff::request_mark_viewed(ctx);
            }
            // 折りたたみ / ブックマーク / テーブル表示 / レビュー / LSP
            Cmd::OpenReview
            | Cmd::SetReviewBase(_)
            | Cmd::SetReviewMode(_)
            | Cmd::CompareWithSaved
            | Cmd::SelectForCompare
            | Cmd::CompareWithSelected
            | Cmd::ToggleFold
            | Cmd::FoldAll
            | Cmd::UnfoldAll
            | Cmd::FoldLevel(_)
            | Cmd::ToggleBookmark
            | Cmd::NextBookmark
            | Cmd::PrevBookmark
            | Cmd::ClearBookmarks
            | Cmd::MarkToggleMnemonic
            | Cmd::MarksPanel
            | Cmd::MarkJump
            | Cmd::MarkJumpDigit(_)
            | Cmd::MarksClearAll
            | Cmd::ReopenClosedTab
            | Cmd::ToggleTableView
            | Cmd::LspCompletion
            | Cmd::LspReferences
            | Cmd::LspSymbols
            | Cmd::LspRename
            | Cmd::LspFormat
            | Cmd::LspCodeAction
            | Cmd::LspSignatureHelp
            | Cmd::ToggleLspHighlight
            | Cmd::ToggleFormatOnSave => self.apply_cmd_editor_extras(cmd, ctx),
            Cmd::Save
            | Cmd::SaveAs
            | Cmd::CloseTab
            | Cmd::OpenFileDialog
            | Cmd::OpenRecentFolder(_)
            | Cmd::OpenRecentFile(_)
            | Cmd::ClearRecent
            | Cmd::SaveAll
            | Cmd::ToggleAutoSave
            | Cmd::RevertFile
            | Cmd::TogglePinTab
            | Cmd::CloseAllTabs => self.apply_cmd_file(cmd, ctx),
            Cmd::Undo
            | Cmd::Redo
            | Cmd::CutSelection
            | Cmd::CopySelection
            | Cmd::PasteClipboard
            | Cmd::SelectAll
            | Cmd::ToggleLineComment
            | Cmd::DuplicateLine
            | Cmd::MoveLineUp
            | Cmd::MoveLineDown
            | Cmd::TransformCase(_)
            | Cmd::SortLines(_)
            | Cmd::DedupeLines
            | Cmd::FormatJsonSelection
            | Cmd::OpenReplace => self.apply_cmd_edit(cmd, ctx),
            Cmd::SplitEditorRight
            | Cmd::SplitEditorDown
            | Cmd::UnsplitEditor
            | Cmd::FocusNextPane
            | Cmd::FocusEditorPane(_)
            | Cmd::MoveTabToNextPane
            | Cmd::GlobalSearch
            | Cmd::GlobalReplace
            | Cmd::ToggleSearchCase
            | Cmd::ToggleSearchWholeWord
            | Cmd::ToggleSearchRegex
            | Cmd::ShowSessions
            | Cmd::ShowQuota
            | Cmd::ToggleFailover
            | Cmd::ToggleTrimTrailingOnSave
            | Cmd::ToggleFinalNewlineOnSave
            | Cmd::ToggleTrimFinalNewlinesOnSave
            | Cmd::ConvertLineEnding(_)
            | Cmd::OpenCommandPalette
            | Cmd::OpenFilePalette
            | Cmd::ShowExplorer
            | Cmd::ShowGitHubTab
            | Cmd::ToggleProblems
            | Cmd::NextProblem
            | Cmd::PrevProblem
            | Cmd::ToggleInlineDiagnostics
            | Cmd::ToggleInlayHints
            | Cmd::ToggleFullScreen
            | Cmd::NavBack
            | Cmd::NavForward
            | Cmd::NextTab
            | Cmd::PrevTab
            | Cmd::SwitchTab
            | Cmd::SwitchTabBack
            | Cmd::ToggleTabSwitchMru
            | Cmd::TogglePreviewTabs
            | Cmd::GoToDefinition
            | Cmd::GoToBracket
            | Cmd::GoToLine
            | Cmd::GoToLineAt(_, _)
            | Cmd::GoToLspPos(_, _) => self.apply_cmd_view_nav(cmd, ctx),
            Cmd::RunActiveFile
            | Cmd::RunBuildTask
            | Cmd::RunJsonTask(_)
            | Cmd::RunSelection
            | Cmd::NewTerminal
            | Cmd::ShowShortcuts
            | Cmd::ShowAbout
            | Cmd::ShowWhatsNew
            | Cmd::OpenInIde(_)
            | Cmd::OpenFolderInIde(_)
            | Cmd::NewFile
            | Cmd::NewWindow
            | Cmd::NewWindowFolder
            | Cmd::OpenFolder
            | Cmd::AddFolder
            | Cmd::AddFolderPath(_)
            | Cmd::RemoveFolder(_) => self.apply_cmd_run_workspace(cmd, ctx),
            Cmd::ToggleTerminal
            | Cmd::ToggleCockpit
            | Cmd::ToggleKanban
            | Cmd::ToggleDeck
            | Cmd::OpenAgentPicker
            | Cmd::NewTask
            | Cmd::OpenRace
            | Cmd::EvalRace
            | Cmd::SendAgentMessage
            | Cmd::ToggleMdPreview
            | Cmd::ToggleSidebar
            | Cmd::OpenGitPanel
            | Cmd::GitCommit(_)
            | Cmd::GitPush
            | Cmd::GitPull
            | Cmd::GitHistory
            | Cmd::OpenSearchMultibuffer
            | Cmd::OpenProblemsMultibuffer
            | Cmd::OpenChangesMultibuffer
            | Cmd::OpenFind
            | Cmd::NewAgent(_)
            | Cmd::FocusAgent(_)
            | Cmd::RestartAgent
            | Cmd::KillAgent
            | Cmd::NewAgentIsolated(_)
            | Cmd::QuickLaunch(_)
            | Cmd::QuickLaunchIsolated(_)
            | Cmd::RenameAgent(_)
            | Cmd::ToggleFollowAgent
            | Cmd::ResumeFollowAgent
            | Cmd::NextUnreadAgent
            | Cmd::DeferUnreadAgent
            | Cmd::ToggleUnreadAgent
            | Cmd::StopAllAgents => self.apply_cmd_agent(cmd, ctx),
            Cmd::SetTheme(_)
            | Cmd::SetUiLanguage(_)
            | Cmd::OpenSettings
            | Cmd::OpenConfig
            | Cmd::ReloadConfig
            | Cmd::ZoomIn
            | Cmd::ZoomOut
            | Cmd::ZoomReset
            | Cmd::FileZoomIn
            | Cmd::FileZoomOut
            | Cmd::FileZoomReset
            | Cmd::TextSizeIn
            | Cmd::TextSizeOut
            | Cmd::TextSizeReset
            | Cmd::SendFileToAgent
            | Cmd::RefreshTree
            | Cmd::SetApproval(_)
            | Cmd::TogglePet
            | Cmd::CyclePermissionAll
            | Cmd::SetPetImage
            | Cmd::ResetPetImage
            | Cmd::ResetPetPos
            | Cmd::SetPetVariant(_)
            | Cmd::SetPetScale(_)
            | Cmd::TogglePetFreeRoam
            | Cmd::TogglePetSleep
            | Cmd::TogglePetSounds
            | Cmd::TogglePetBubbles
            | Cmd::TogglePetAutoYes
            | Cmd::ToggleRemote
            | Cmd::OpenSshRemote
            | Cmd::ToggleWordWrap
            | Cmd::ToggleShowWhitespace
            | Cmd::ToggleMinimap
            | Cmd::ToggleShellIntegration
            | Cmd::ToggleBreadcrumbs => self.apply_cmd_settings(cmd, ctx),
            Cmd::ToggleGitBlame => self.apply_cmd_settings(cmd, ctx),
            Cmd::VoiceInput(_)
            | Cmd::VoiceStop
            | Cmd::SetVoiceTarget(_)
            | Cmd::SetVoiceEngine(_)
            | Cmd::SetVoiceLang(_)
            | Cmd::SetVoiceKeyword(_)
            | Cmd::NewPlugin
            | Cmd::InstallPlugin
            | Cmd::RescanPlugins
            | Cmd::ShowPlugins
            | Cmd::RunPlugin(_, _) => self.apply_cmd_voice_plugin(cmd, ctx),
            // ── 第 3 次配線 ──
            Cmd::RestartTutorial => self.tutorial.restart(),
            Cmd::OpenLicense => {
                // 開くたびに読み直す — 別ウィンドウ (別プロセス) で適用された
                // キーや、外部で消されたファイルを画面へ反映するため。
                let (k, st) = license::current_status();
                self.license_key = k;
                self.license_status = st;
                self.license_input.clear();
                self.license_open = true;
            }
            Cmd::OpenApprovals => self.open_approvals_panel(),
            Cmd::OpenMcp => self.open_mcp_panel(),
            Cmd::OpenSkills => self.open_skills_panel(),
            Cmd::OpenApprovalAudit => {
                self.open_approvals_panel();
                self.approvals_audit = true;
                // 控えを捨てて、次の描画後に 1 回だけ読み直させる
                self.approvals_audit_cache = None;
            }
            Cmd::AddCursorAbove
            | Cmd::AddCursorBelow
            | Cmd::SelectAllOccurrences
            | Cmd::SelectNextOccurrence
            | Cmd::ColumnSelectStart
            | Cmd::ColumnSelectFinish
            | Cmd::ClearMultiCursor
            | Cmd::MultiPaste => self.apply_cmd_multi_cursor(cmd, ctx),
            Cmd::ReopenWithEncoding(_) | Cmd::SaveWithEncoding(_) => self.apply_cmd_encoding(cmd),
        }
    }

    /// `apply_cmd` のファイル/タブ操作カテゴリ (Cmd::Save 〜 Cmd::CloseAllTabs)。
    pub(super) fn apply_cmd_file(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::Save => {
                // 保存先を尋ねる場合 (未保存の新規タブ) はダイアログが別スレッドへ
                // 回るので、保存後の追加動作をフラグで預けておく
                if self.save_active_with(false, false, true) {
                    self.persist_session();
                    if let Some(i) = self.editor.active {
                        self.run_on_save_hooks(i, ctx);
                    }
                }
            }
            Cmd::SaveAs => {
                if self.save_active_with(true, false, true) {
                    self.persist_session();
                    if let Some(i) = self.editor.active {
                        self.run_on_save_hooks(i, ctx);
                    }
                }
            }
            Cmd::CloseTab => {
                if let Some(i) = self.editor.active {
                    self.request_close(i);
                }
            }
            // ── VS Code 準拠メニューバー (menu_bar.rs) ──────────────
            Cmd::OpenFileDialog => {
                let dir = self.primary_root().to_path_buf();
                self.ask_dialog(
                    DialogPurpose::OpenFile,
                    DialogSpec::pick_file().directory(dir),
                    ctx,
                );
            }
            Cmd::OpenRecentFolder(p) => {
                self.open_workspace(p.clone(), ctx);
                self.menu_state.touch_folder(&p);
                recent::save(&self.menu_state);
            }
            Cmd::OpenRecentFile(p) => {
                self.open_path(&p);
                self.touch_recent_file(&p);
            }
            Cmd::ClearRecent => {
                self.menu_state.clear_recent();
                recent::save(&self.menu_state);
                self.toast(tr("最近使用した項目をクリアしました"), true);
            }
            Cmd::SaveAll => self.save_all(ctx),
            Cmd::ToggleAutoSave => {
                self.menu_state.auto_save = !self.menu_state.auto_save;
                recent::save(&self.menu_state);
                self.toast(
                    if self.menu_state.auto_save {
                        tr("自動保存: オン (編集は数秒ごとに保存されます)")
                    } else {
                        tr("自動保存: オフ")
                    },
                    true,
                );
            }
            Cmd::RevertFile => self.revert_active(),
            Cmd::TogglePinTab => {
                if let Some(i) = self.editor.active {
                    self.toggle_pin_tab(i);
                }
            }
            Cmd::CloseAllTabs => self.close_all_tabs(),
            _ => {}
        }
    }

    /// `apply_cmd` の編集操作カテゴリ (Cmd::Undo 〜 Cmd::OpenReplace)。
    // アーム本体は apply_cmd からの字句コピー (ガード化すると挙動境界が変わるため抑止)。
    #[allow(clippy::collapsible_match)]
    pub(super) fn apply_cmd_edit(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::Undo => self.undo_active(),
            Cmd::Redo => self.redo_active(),
            Cmd::CutSelection => self.push_editor_event(egui::Event::Cut, true),
            Cmd::CopySelection => self.push_editor_event(egui::Event::Copy, false),
            Cmd::PasteClipboard => match menu_bar::clipboard_text() {
                Some(t) => self.push_editor_event(egui::Event::Paste(t), true),
                None => self.toast(tr("クリップボードにテキストがありません"), false),
            },
            Cmd::SelectAll => self.select_all_active(ctx),
            Cmd::ToggleLineComment => self.editor_op(ctx, EditOp::ToggleComment),
            Cmd::DuplicateLine => self.editor_op(ctx, EditOp::Duplicate),
            Cmd::MoveLineUp => self.editor_op(ctx, EditOp::Move(true)),
            Cmd::MoveLineDown => self.editor_op(ctx, EditOp::Move(false)),
            Cmd::TransformCase(kind) => self.editor_op(ctx, EditOp::Case(kind)),
            Cmd::SortLines(desc) => self.editor_op(ctx, EditOp::Sort(desc)),
            Cmd::DedupeLines => self.editor_op(ctx, EditOp::Dedupe),
            Cmd::FormatJsonSelection => self.editor_op(ctx, EditOp::FormatJson),
            Cmd::OpenReplace => {
                if self.editor.active.is_some() {
                    self.open_find(ctx, true);
                }
            }
            _ => {}
        }
    }

    /// `apply_cmd` の表示/パネル/ナビゲーションカテゴリ (Cmd::GlobalSearch 〜 Cmd::GoToLine)。
    // アーム本体は apply_cmd からの字句コピー (ガード化すると挙動境界が変わるため抑止)。
    #[allow(clippy::collapsible_match)]
    pub(super) fn apply_cmd_view_nav(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            // ── エディタの分割 (VS Code の editor group 相当) ──────
            // 開いているものが無いと分割に意味が無いので、バッファ 0 枚の
            // ときは断る (中身の無いペインを増やさない)。
            Cmd::SplitEditorRight | Cmd::SplitEditorDown => {
                if self.editor.active.is_none() {
                    self.toast(tr("分割するファイルが開かれていません"), false);
                    return;
                }
                let dir = if matches!(cmd, Cmd::SplitEditorRight) {
                    terminal::SplitDir::Horizontal
                } else {
                    terminal::SplitDir::Vertical
                };
                self.panes.split(dir);
                self.sync_panes();
            }
            Cmd::UnsplitEditor => {
                if self.panes.unsplit() {
                    self.sync_panes();
                } else {
                    self.toast(tr("エディタは分割されていません"), false);
                }
            }
            Cmd::FocusNextPane => {
                if self.panes.focus_next() {
                    self.sync_panes();
                }
            }
            Cmd::FocusEditorPane(n) => {
                if self.panes.focus_index(n) {
                    self.sync_panes();
                }
            }
            Cmd::MoveTabToNextPane => {
                if self.editor.active.is_none() {
                    self.toast(tr("移動するタブがありません"), false);
                    return;
                }
                if self.panes.move_active_tab_to_next() {
                    self.sync_panes();
                }
            }
            Cmd::GlobalSearch => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Search;
                self.gsearch.focus = true;
            }
            Cmd::GlobalReplace => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Search;
                self.gsearch.replace_open = true;
                self.gsearch.focus = true;
            }
            Cmd::ToggleSearchCase | Cmd::ToggleSearchWholeWord | Cmd::ToggleSearchRegex => {
                let (flag, on_label, off_label) = match cmd {
                    Cmd::ToggleSearchCase => (
                        &mut self.gsearch.case_sensitive,
                        "検索: 大文字と小文字を区別します",
                        "検索: 大文字と小文字を区別しません",
                    ),
                    Cmd::ToggleSearchWholeWord => (
                        &mut self.gsearch.whole_word,
                        "検索: 単語単位で探します",
                        "検索: 単語単位をやめました",
                    ),
                    _ => (
                        &mut self.gsearch.regex,
                        "検索: 正規表現として探します",
                        "検索: 正規表現をやめました",
                    ),
                };
                *flag = !*flag;
                let msg = if *flag { on_label } else { off_label };
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Search;
                // 条件が変わったので確認待ちの置換件数は捨てる
                self.gsearch.phase = self.gsearch.phase.next(&ReplaceEvent::Cancel);
                self.toast(tr(msg), true);
                self.save_search_prefs(ctx);
                if !self.gsearch.query.trim().is_empty() {
                    self.start_global_search();
                }
            }
            Cmd::ShowSessions => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Sessions;
                // タブを開いた瞬間に最新へ (走査自体はバックグラウンド)
                self.sidebar_sessions.invalidate();
            }
            Cmd::ShowQuota => {
                self.quota.force_refresh();
                self.quota_open = true;
            }
            Cmd::ToggleFailover => self.set_failover_enabled(!self.failover.enabled()),
            Cmd::ToggleTrimTrailingOnSave => {
                self.save_trim_trailing = !self.save_trim_trailing;
                self.save_editor_prefs(ctx);
                self.toast(
                    tr(if self.save_trim_trailing {
                        "保存時に行末の空白を落とします"
                    } else {
                        "保存時に行末の空白を落としません"
                    }),
                    true,
                );
            }
            Cmd::ToggleFinalNewlineOnSave => {
                self.save_final_newline = !self.save_final_newline;
                self.save_editor_prefs(ctx);
                self.toast(
                    tr(if self.save_final_newline {
                        "保存時に最終行へ改行を入れます"
                    } else {
                        "保存時に最終行へ改行を入れません"
                    }),
                    true,
                );
            }
            Cmd::ToggleTrimFinalNewlinesOnSave => {
                self.save_trim_final_newlines = !self.save_trim_final_newlines;
                self.save_editor_prefs(ctx);
                self.toast(
                    tr(if self.save_trim_final_newlines {
                        "保存時に末尾の余分な空行を落とします"
                    } else {
                        "保存時に末尾の余分な空行を落としません"
                    }),
                    true,
                );
            }
            Cmd::ConvertLineEnding(le) => {
                if self.editor.active.is_none() {
                    self.toast(tr("先にファイルを開いてください"), false);
                } else {
                    self.editor_op(ctx, EditOp::NormalizeEol(le));
                    self.le_cache = None;
                    self.toast(
                        trf("改行コードを {le} に揃えました", &[("le", le.label())]),
                        true,
                    );
                }
            }
            Cmd::OpenCommandPalette => self.palette.open_commands(),
            Cmd::OpenFilePalette => self.palette.open_files(),
            Cmd::ShowExplorer => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Files;
                ctx.memory_mut(|m| {
                    if let Some(id) = m.focused() {
                        m.surrender_focus(id);
                    }
                });
                self.tree.focus();
            }
            Cmd::ShowGitHubTab => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::GitHub;
                // メニューからの明示操作なので、ここで初めて GitHub 連携を有効化する。
                // (起動時のセッション復元でタブが出ているだけでは gh は動かさない)
                self.github.active = true;
            }
            Cmd::ToggleProblems => self.problems_open = !self.problems_open,
            Cmd::NextProblem => self.goto_diagnostic(ctx, true),
            Cmd::PrevProblem => self.goto_diagnostic(ctx, false),
            Cmd::ToggleInlineDiagnostics => {
                self.cfg.inline_diagnostics = !self.cfg.inline_diagnostics;
                let msg = if self.cfg.inline_diagnostics {
                    tr("行末の診断メッセージ: ON")
                } else {
                    tr("行末の診断メッセージ: OFF")
                };
                self.toast(msg, true);
            }
            Cmd::ToggleInlayHints => {
                self.cfg.inlay_hints = !self.cfg.inlay_hints;
                let on = self.cfg.inlay_hints;
                if !on {
                    // 消したフレームで組み直しの材料も捨てる (残骸を出さない)
                    self.inlay_cache.clear();
                }
                // 永続化しないのは隣の `ToggleInlineDiagnostics` と同じ判断:
                // パレットの切替は「いまのセッションで一時的に消す/出す」ため。
                // 恒久的に変えるときは設定画面 (editor グループ) から。
                self.toast(
                    tr(if on {
                        "インレイヒント: ON"
                    } else {
                        "インレイヒント: OFF"
                    }),
                    true,
                );
            }
            Cmd::ToggleFullScreen => {
                // 救出 (壊れた全画面から脱出) 中・枠復元の予約中は何も送らない。
                // 遷移の最中に styleMask/zoom を重ねると AppKit が NSException を
                // 投げてプロセスごと落ちる (実測)。救出中は viewport().fullscreen が
                // 既に false + broken_native_fs 学習済みなので、ガードが無いと
                // ⌃⌘F が enter_fake_fullscreen へ直行してしまう。
                if self.fs_rescue_pending || self.fake_fs_restore.is_some() {
                    return;
                }
                let cur = ctx.input(|i| i.viewport().fullscreen.unwrap_or(false));
                if self.fake_fullscreen.is_some() {
                    self.exit_fake_fullscreen(ctx);
                } else {
                    // ネイティブ全画面の出入りは遷移アニメ (~1秒) を伴うので、
                    // 連打は 1.5 秒のクールダウンで無視する (遷移中の再送防止)。
                    if self
                        .fs_toggle_at
                        .is_some_and(|t| t.elapsed().as_millis() < 1500)
                    {
                        return;
                    }
                    self.fs_toggle_at = Some(Instant::now());
                    if cur {
                        ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
                    } else {
                        // このモニタでネイティブ全画面が壊れると学習済みなら最初から疑似で
                        let mon = ctx.input(|i| i.viewport().monitor_size);
                        let known_broken = mon.is_some_and(|m| {
                            self.broken_native_fs
                                .iter()
                                .any(|b| (*b - m).length() < 1.0)
                        });
                        if known_broken {
                            self.enter_fake_fullscreen(ctx);
                        } else {
                            ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(true));
                        }
                    }
                }
            }
            Cmd::NavBack => self.nav_go(-1),
            Cmd::NavForward => self.nav_go(1),
            Cmd::NextTab => self.cycle_tab(1),
            Cmd::PrevTab => self.cycle_tab(-1),
            Cmd::SwitchTab => self.switch_tab(1),
            Cmd::SwitchTabBack => self.switch_tab(-1),
            Cmd::ToggleTabSwitchMru => {
                self.cfg.tab_switch_mru = !self.cfg.tab_switch_mru;
                config::save_state(&self.cfg);
                self.tab_switcher = None;
                self.toast(
                    if self.cfg.tab_switch_mru {
                        tr("タブ切替: 最近使った順 (押している間に候補、離して確定)")
                    } else {
                        tr("タブ切替: 並び順")
                    },
                    true,
                );
            }
            Cmd::TogglePreviewTabs => {
                self.cfg.preview_tabs = !self.cfg.preview_tabs;
                config::save_state(&self.cfg);
                if !self.cfg.preview_tabs {
                    // オフにした瞬間、いま開いているプレビューは確定タブにする
                    // (斜体のまま置き去りにしない)。
                    for id in self.panes.order() {
                        if let Some(b) = self.panes.preview_of(id) {
                            self.panes.promote(b);
                        }
                    }
                }
                self.toast(
                    if self.cfg.preview_tabs {
                        tr("プレビュータブ: オン (1 回クリックで開いたタブは置き換わります)")
                    } else {
                        tr("プレビュータブ: オフ")
                    },
                    true,
                );
            }
            Cmd::GoToDefinition => self.goto_definition(ctx),
            Cmd::GoToBracket => self.goto_bracket(ctx),
            Cmd::GoToLine => {
                if self.editor.active.is_some() {
                    self.goto_open = true;
                    self.goto_input.clear();
                }
            }
            // パレットの `:123` / `@シンボル` から来る使い捨ての座標。
            Cmd::GoToLineAt(line, col) => self.goto_line_col(line, col),
            Cmd::GoToLspPos(line, col) => {
                if let Some(p) = self.active_file_path() {
                    self.jump_to_lsp_pos(&p, line, col);
                }
            }
            _ => {}
        }
    }

    /// `apply_cmd` の実行/IDE 連携/ワークスペースカテゴリ (Cmd::RunActiveFile 〜 Cmd::RemoveFolder)。
    pub(super) fn apply_cmd_run_workspace(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::RunActiveFile => self.run_active_file(ctx),
            Cmd::RunBuildTask => self.run_build_task(ctx),
            Cmd::RunJsonTask(i) => self.run_json_task(i, ctx),
            Cmd::RunSelection => self.run_selection(ctx),
            Cmd::NewTerminal => self.new_terminal(ctx),
            Cmd::ShowShortcuts => self.shortcuts_open = true,
            Cmd::ShowAbout => self.about_open = true,
            Cmd::ShowWhatsNew => self.open_whats_new(),
            Cmd::OpenInIde(key) => {
                let file = self
                    .editor
                    .active
                    .and_then(|i| self.editor.buffers[i].path.clone());
                let root = self.primary_root().to_path_buf();
                let cursor = self.editor.cursor;
                match panels::open_in_ide(&key, file.as_deref(), cursor, &root, false) {
                    Ok(msg) => self.toast(msg, true),
                    Err(msg) => self.toast(msg, false),
                }
            }
            Cmd::OpenFolderInIde(key) => {
                let root = self.primary_root().to_path_buf();
                match panels::open_in_ide(&key, None, (1, 1), &root, true) {
                    Ok(msg) => self.toast(msg, true),
                    Err(msg) => self.toast(msg, false),
                }
            }
            Cmd::NewFile => self.editor.new_untitled(),
            Cmd::NewWindow => self.spawn_new_window(None),
            Cmd::NewWindowFolder => {
                let dir = self.primary_root().to_path_buf();
                self.ask_dialog(
                    DialogPurpose::NewWindowFolder,
                    DialogSpec::pick_folder().directory(dir),
                    ctx,
                );
            }
            Cmd::OpenFolder => {
                let dir = self.primary_root().to_path_buf();
                self.ask_dialog(
                    DialogPurpose::OpenFolder,
                    DialogSpec::pick_folder().directory(dir),
                    ctx,
                );
            }
            Cmd::AddFolder => {
                let dir = self.primary_root().to_path_buf();
                self.ask_dialog(
                    DialogPurpose::AddFolder,
                    DialogSpec::pick_folder().directory(dir),
                    ctx,
                );
            }
            Cmd::AddFolderPath(dir) => {
                if dir.is_dir() {
                    self.add_folder_to_workspace(dir, ctx);
                } else {
                    self.toast(
                        trf(
                            "フォルダがありません: {dir}",
                            &[("dir", dir.display().to_string())],
                        ),
                        false,
                    );
                }
            }
            Cmd::RemoveFolder(dir) => {
                if self.roots.len() <= 1 {
                    // ルートが空になると行き先が無くなるので拒否する
                    self.toast_warn(tr("最後のフォルダは削除できません"));
                } else {
                    let next: Vec<PathBuf> =
                        self.roots.iter().filter(|r| **r != dir).cloned().collect();
                    if next.len() == self.roots.len() {
                        self.toast_warn(tr("そのフォルダはワークスペースにありません"));
                    } else {
                        self.set_roots(next, ctx);
                        self.toast(
                            trf(
                                "📂 {dir} をワークスペースから削除しました",
                                &[("dir", dir.display().to_string())],
                            ),
                            true,
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// 自分自身の実行ファイルを別プロセスとして起動し、新しいウィンドウを開く。
    /// `dir` 指定ありならそのフォルダをワークスペースルートに、無指定なら
    /// ホームディレクトリを開く (ルートは空にできない仕様のため)。
    pub(super) fn spawn_new_window(&mut self, dir: Option<PathBuf>) {
        let exe = match std::env::current_exe() {
            Ok(e) => e,
            Err(e) => {
                self.toast(
                    trf(
                        "実行ファイルの場所を取得できません: {err}",
                        &[("err", e.to_string())],
                    ),
                    false,
                );
                return;
            }
        };
        let mut cmd = std::process::Command::new(exe);
        // 新プロセスは引数のフォルダをルートにする (main.rs の起動引数解釈)。
        // カレントディレクトリも合わせておくと、引数パスが消えていた場合の
        // フォールバック先も同じ場所になる。
        let root = dir.clone().or_else(dirs::home_dir);
        if let Some(d) = &root {
            cmd.arg(d);
            cmd.current_dir(d);
        }
        match cmd.spawn() {
            Ok(_) => match &dir {
                Some(d) => self.toast(
                    trf(
                        "🪟 {dir} を新しいウィンドウで開きました",
                        &[("dir", d.display().to_string())],
                    ),
                    true,
                ),
                None => self.toast(tr("🪟 新しいウィンドウを開きました"), true),
            },
            Err(e) => self.toast(
                trf(
                    "新しいウィンドウを開けません: {err}",
                    &[("err", e.to_string())],
                ),
                false,
            ),
        }
    }

    /// `apply_cmd` のエージェント/ターミナル/Cockpit カテゴリ (Cmd::ToggleTerminal 〜 Cmd::KillAgent)。
    pub(super) fn apply_cmd_agent(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::ToggleTerminal => {
                if self.agents.sessions.is_empty() && !self.agents.panel_open {
                    // 開くものがなければシェルを起動する
                    let shell_idx = self
                        .cfg
                        .agents
                        .iter()
                        .position(|p| p.command.trim().is_empty())
                        .unwrap_or(0);
                    self.launch_preset(shell_idx, ctx);
                } else {
                    self.agents.panel_open = !self.agents.panel_open;
                }
                self.persist_session();
            }
            Cmd::ToggleCockpit => {
                self.cockpit = !self.cockpit;
                if self.cockpit {
                    self.kanban = false;
                    self.deck = false;
                }
            }
            Cmd::ToggleKanban => {
                self.kanban = !self.kanban;
                if self.kanban {
                    // 看板は Cockpit / デッキと同格の中央画面。3 つ同時には出さない。
                    // 下部端末パネルの開閉には触らない — 触ると閉じて開くだけで
                    // パネルが勝手に開いた状態へ変わってしまう。
                    self.cockpit = false;
                    self.deck = false;
                }
            }
            // エージェントデッキは Cockpit と同格の中央画面。3 つ同時には出さない。
            Cmd::ToggleDeck => {
                self.deck = !self.deck;
                if self.deck {
                    self.cockpit = false;
                    self.kanban = false;
                }
            }
            Cmd::OpenAgentPicker => self.agent_picker.open(ctx),
            // フォームは Cockpit の中で描くので、一緒に開く。
            Cmd::NewTask => {
                self.cockpit = true;
                self.kanban = false;
                self.orch.open_task_form();
            }
            // レースのフォームも Cockpit の中で描くので、一緒に開く。
            Cmd::OpenRace => {
                self.cockpit = true;
                self.kanban = false;
                self.race.form_open = true;
            }
            // 🏆 勝者評価。走っているレースが無ければ何も起こさず理由を出す
            // (**提案を出すだけ** — 採用はユーザーが [採用] を押したときだけ)。
            Cmd::EvalRace => {
                if self.race.race.is_none() {
                    self.toast(tr("走っているレースがありません"), false);
                } else {
                    self.cockpit = true;
                    self.kanban = false;
                    self.race.start_eval(&self.cfg.race_eval, ctx);
                }
            }
            Cmd::SendAgentMessage => {
                self.cockpit = true;
                self.kanban = false;
                self.orch.open_msg_form();
            }
            Cmd::ToggleMdPreview => {
                let ok = self
                    .editor
                    .active
                    .map(|i| {
                        let b = &self.editor.buffers[i];
                        markdown::is_markdown(&b.title, &b.lang) || html::is_html(&b.title, &b.lang)
                    })
                    .unwrap_or(false);
                if ok {
                    self.md_preview = !self.md_preview;
                } else {
                    self.toast(tr("Markdown / HTML ファイルではありません"), false);
                }
            }
            Cmd::ToggleSidebar => {
                self.sidebar_open = !self.sidebar_open;
                self.persist_session();
            }
            Cmd::OpenGitPanel => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Git;
                self.persist_session();
            }
            Cmd::GitCommit(all) => self.open_commit_prompt(all),
            Cmd::GitPush => self.run_git_job(GitJob::Push, ctx),
            Cmd::GitPull => self.run_git_job(GitJob::Pull, ctx),
            Cmd::GitHistory => self.open_git_history(ctx),
            Cmd::OpenSearchMultibuffer => self.open_search_multibuffer(),
            Cmd::OpenProblemsMultibuffer => self.open_problems_multibuffer(),
            Cmd::OpenChangesMultibuffer => self.open_changes_multibuffer(),
            // 選択があればそれを検索語にする (VS Code と同じ)
            Cmd::OpenFind => self.open_find(ctx, false),
            Cmd::NewAgent(i) => self.launch_preset(i, ctx),
            Cmd::NewAgentIsolated(i) => self.launch_preset_isolated(i, ctx),
            Cmd::QuickLaunch(slot) => self.launch_quick_slot(slot, false, ctx),
            Cmd::QuickLaunchIsolated(slot) => self.launch_quick_slot(slot, true, ctx),
            Cmd::RenameAgent(i) => self.begin_rename_agent(i),
            Cmd::StopAllAgents => {
                if self.agents.running_count() == 0 {
                    self.toast(tr("稼働中のエージェントはありません"), false);
                } else {
                    // 破壊的操作なので必ず確認を挟む (実行はモーダル側)。
                    self.pending_stop_all = true;
                }
            }
            Cmd::FocusAgent(i) => {
                if i < self.agents.sessions.len() {
                    self.agents.active = i;
                    self.agents.panel_open = true;
                    self.cockpit = false;
                    // 看板タブ表示中はパネルの中身が看板なので、端末ビューへ戻す
                    self.kanban = false;
                    self.term_focus_pending = true;
                    // 明示的なフォーカス = 既読 (「あとで見る」ピンも外す)
                    self.agents.sessions[i].acknowledge();
                }
            }
            Cmd::RestartAgent => {
                let i = self.agents.active;
                if let Err(e) = self.agents.restart(i, ctx) {
                    self.toast(e, false);
                }
            }
            Cmd::KillAgent => {
                let i = self.agents.active;
                self.close_agent(i);
            }
            Cmd::ToggleFollowAgent => self.toggle_follow_agent(),
            Cmd::ResumeFollowAgent => self.resume_follow_agent(),
            Cmd::NextUnreadAgent => self.jump_next_unread(),
            Cmd::DeferUnreadAgent => self.defer_to_next_unread(),
            Cmd::ToggleUnreadAgent => self.toggle_unread_here(),
            _ => {}
        }
    }

    // ── Follow the agent ──────────────────────────────────────────────
    //
    // 「いま何をしているか」を新しいパネルを増やさずに見せる。追従先は 1 体。
    // 状態機械は `follow::Follow` が持ち、ここは UI との橋渡しだけ。

    /// アクティブなエージェントの追従を開始 / 解除する。
    pub(super) fn toggle_follow_agent(&mut self) {
        let Some(s) = self.agents.sessions.get(self.agents.active) else {
            self.toast(tr("エージェントセッションがありません"), false);
            return;
        };
        let (id, title, running) = (s.id, s.title.clone(), s.running());
        if !running && self.follow.target() != Some(id) {
            self.toast(tr("終了したエージェントは追従できません"), false);
            return;
        }
        if self.follow.toggle(id) {
            // 追従は**エディタ**の機能なので、始めた瞬間だけ中央ビューを戻す。
            // ユーザーが自分で押した操作なので「画面が突然変わる」には当たらない。
            self.cockpit = false;
            self.kanban = false;
            self.deck = false;
            self.toast(trf("🎯 {title} を追従します", &[("title", title)]), true);
        } else {
            self.toast(
                trf("🎯 {title} の追従を解除しました", &[("title", title)]),
                true,
            );
        }
    }

    /// ユーザーのスクロールで止まった追従を明示的に再開する。
    pub(super) fn resume_follow_agent(&mut self) {
        if self.follow.resume() {
            self.toast(tr("🎯 追従を再開しました"), true);
        } else if self.follow.is_on() {
            self.toast(tr("追従は止まっていません"), false);
        } else {
            self.toast(tr("追従していません"), false);
        }
    }

    /// **Follow the agent の 1 フレーム。**
    ///
    /// 追従がオフなら最初の 1 行で戻る — git もファイルシステムも触らない
    /// (設計原則 3: アイドル時のコストはゼロ)。走査は
    /// [`follow::Follow::tick`] がスロットリングし、実際の git は別スレッド。
    pub(super) fn follow_tick(&mut self, ctx: &egui::Context) {
        if !self.follow.is_on() {
            return;
        }
        // ① 追える相手 = 走っているセッション。消えたら黙って解除する。
        let alive: Vec<u64> = self
            .agents
            .sessions
            .iter()
            .filter(|s| s.running())
            .map(|s| s.id)
            .collect();
        if self.follow.prune(&alive) {
            self.toast_warn(tr(
                "🎯 追従を解除しました — 対象のエージェントが終了しました",
            ));
            return;
        }
        // ② **ユーザーの操作が常に勝つ。** 自分でスクロールしたら一時停止。
        if self.follow.is_active()
            && ctx.input(|i| i.raw_scroll_delta.y.abs() > 0.5)
            && self.follow.note_user_scroll()
        {
            let key = self.key_hint(BindAction::FollowResume);
            self.toast_warn(trf(
                "🎯 追従を一時停止しました — 再開は {key}",
                &[("key", key)],
            ));
            return;
        }
        // ③ エディタを見ていないフレームは 1 命令も走らせない。
        //    中央ビューを勝手に切り替えたりもしない (画面が突然変わらない)。
        if self.center != CenterView::Editor {
            return;
        }
        let Some(dir) = self
            .follow
            .target()
            .and_then(|id| self.agents.sessions.iter().find(|s| s.id == id))
            .map(|s| s.cwd.clone())
        else {
            return;
        };
        let spot = self.follow.tick(Instant::now(), move || {
            let (tx, rx) = std::sync::mpsc::channel();
            // git は UI スレッドで待たない (worktree.rs の約束)。
            std::thread::spawn(move || {
                let _ = tx.send(follow::probe(&dir));
            });
            rx
        });
        let Some(spot) = spot else { return };
        // 未オープンなら**プレビュータブ**で開く (確定タブを増やさない)。
        self.open_path_preview(&spot.path);
        self.goto_line(spot.line);
    }

    /// **通知は「稼働中 → 待機」の遷移 1 点に絞る。**
    ///
    /// 競合実装 (orca) は「状態が続く間ずっと鳴らす」設計で通知スパムを
    /// 未修正バグとして抱えている。ここは [`notify::WorkGate`] が
    /// **遷移エッジでしか true を返さない**ので、構造的に鳴り続けない。
    ///
    /// 段の観測は見張り ([`supervisor`]) から取る — 画面の文字列からは
    /// 推測しない (設計原則 4)。承認待ち・レート制限・プロセス終了は
    /// 「遷移」ではなく**要対応イベント**なので、それぞれ専用の通知が残る。
    pub(super) fn notify_work_done(&mut self, win_focused: bool) {
        let phases: Vec<(u64, String, Option<notify::WorkPhase>)> = self
            .agents
            .sessions
            .iter()
            .map(|s| {
                (
                    s.id,
                    s.title.clone(),
                    self.supervisor.notify_phase_of(s.id, s.running()),
                )
            })
            .collect();
        let alive: Vec<u64> = phases.iter().map(|(id, _, _)| *id).collect();
        // 消えたセッションの段は忘れる (PID 再利用で誤爆しないため)
        self.work_gate.retain(&alive);
        self.anomaly_gate.retain(&alive);
        for (_, title, _) in phases
            .iter()
            .filter(|(id, _, ph)| self.work_gate.note(*id, *ph))
            .cloned()
            .collect::<Vec<_>>()
        {
            self.toast(
                trf(
                    "✅ {title} が手を止めました — 待機中",
                    &[("title", title.clone())],
                ),
                true,
            );
            if self.cfg.pet_sounds {
                self.sound.play(SoundKind::Complete);
            }
            // OS 通知はこのファイルの既存の作法どおり非アクティブ時だけ
            if !win_focused {
                notify::notify(
                    "Zaivern Code",
                    &trf("✅ {title} が作業を終えました", &[("title", title.clone())]),
                );
            }
            notify::webhook(
                &self.cfg.webhook_url,
                &tr("✅ 作業完了"),
                &trf("{title} が待機に戻りました", &[("title", title)]),
            );
        }
    }

    // ── 未読カーソル ──────────────────────────────────────────────────
    //
    // 「今どれが自分待ちか」へ視線移動ゼロで飛ぶ。3 ビュー (Cockpit / 看板 /
    // デッキ) は排他なので、**いま見ているビューのまま**選択だけを動かす。

    /// ビューを変えずにそのエージェントを選ぶ (未読カーソルの着地点)。
    pub(super) fn focus_agent_in_place(&mut self, i: usize) {
        if i >= self.agents.sessions.len() {
            return;
        }
        self.agents.active = i;
        // エディタを見ているときだけ端末パネルを開く。Cockpit / 看板 /
        // デッキを見ているなら、その中で選択が動くだけで十分見える。
        if self.center == CenterView::Editor {
            self.agents.panel_open = true;
            self.term_focus_pending = true;
        }
        let id = self.agents.sessions[i].id;
        // デッキは自前の選択 (セッション ID) を持つので、そちらも合わせる。
        // これが無いとデッキ表示中だけカーソルが動かない。
        if self.center == CenterView::Deck {
            self.deck_state.select(id);
        }
        self.agents.sessions[i].acknowledge();
        // 見に行った相手は「鳴らし直し」の対象から外す
        self.work_gate.forget(id);
    }

    /// 未読フラグの一覧 (巡回の入力)。
    pub(super) fn unread_flags(&self) -> Vec<bool> {
        self.agents
            .sessions
            .iter()
            .map(|s| s.has_unread())
            .collect()
    }

    /// 次の未読エージェントへ飛ぶ (端で折り返す)。
    pub(super) fn jump_next_unread(&mut self) {
        let flags = self.unread_flags();
        match next_unread(&flags, self.agents.active) {
            Some(i) => {
                let title = self.agents.sessions[i].title.clone();
                self.focus_agent_in_place(i);
                self.toast(trf("◆ {title} へ移動しました", &[("title", title)]), true);
            }
            None => self.toast(tr("未読のエージェントはありません"), false),
        }
    }

    /// いまの相手を未読へ戻してから次の未読へ (後回し宣言)。
    pub(super) fn defer_to_next_unread(&mut self) {
        let cur = self.agents.active;
        let Some(s) = self.agents.sessions.get_mut(cur) else {
            self.toast(tr("エージェントセッションがありません"), false);
            return;
        };
        s.mark_unread();
        let id = s.id;
        // 段を捨てる = 次に待機へ戻ったらもう一度だけ鳴らす
        self.work_gate.forget(id);
        self.jump_next_unread();
    }

    /// いまの相手の未読を反転する。
    pub(super) fn toggle_unread_here(&mut self) {
        let cur = self.agents.active;
        let Some(s) = self.agents.sessions.get_mut(cur) else {
            self.toast(tr("エージェントセッションがありません"), false);
            return;
        };
        let (title, was) = (s.title.clone(), s.has_unread());
        if was {
            s.acknowledge();
        } else {
            s.mark_unread();
            let id = s.id;
            self.work_gate.forget(id);
        }
        let msg = if was {
            trf("✓ {title} を既読にしました", &[("title", title)])
        } else {
            trf("📩 {title} を未読に戻しました", &[("title", title)])
        };
        self.toast(msg, true);
    }

    // ─── ズーム (画面全体 / ファイル単位) ────────────────────────────────
    //
    // VS Code と同じ二階建て:
    //   画面全体 = egui の `zoom_factor` を動かす → UI の全部が拡大縮小する。
    //              値は `Config::ui_zoom` に持ち、state.toml へ覚える。
    //   ファイル = そのタブのフォント倍率だけを動かす → 本文だけが変わる。
    //              値は `Buffer::zoom` に持ち、タブを閉じれば消える。
    // どちらも段は `crate::zoom::STEPS` を共有するので、
    // キー / ホイール / メニュー / パレットのどこから触っても同じ倍率を行き来する。

    /// 画面全体のズームを設定して state.toml へ覚える。
    ///
    /// egui へ渡すのはフレーム先頭の [`apply_ui_zoom`] なので、ここは値の確定だけ。
    /// はしごの端では黙って何も起きない (ブラウザ・VS Code と同じ)。
    pub(super) fn set_ui_zoom(&mut self, z: f32) {
        let z = zoom::clamp(z);
        if (z - self.cfg.ui_zoom).abs() < 1e-4 {
            return;
        }
        self.cfg.ui_zoom = z;
        config::save_state(&self.cfg);
    }

    /// **文字サイズだけ**の倍率を設定して state.toml へ覚える。
    ///
    /// `set_ui_zoom` との違い: あちらは `zoom_factor` を動かすので
    /// 余白・ボタン・パネル幅まで拡大し、画面に入る情報量が減る。
    /// こちらは本文・ボタン文字・エディタ・ターミナルの**文字サイズだけ**を
    /// 掛け直すので、レイアウトはそのままで字だけ読みやすくできる。
    /// 実際の反映は `theme::set_text_scale` が次フレーム先頭で行う
    /// (スタイルの書き換え地点を 1 つに保ち、誤差を積み上げないため)。
    pub(super) fn set_text_scale(&mut self, scale: f32) {
        let scale = zoom::clamp(scale);
        if (scale - self.cfg.text_scale).abs() < 1e-4 {
            return;
        }
        self.cfg.text_scale = scale;
        self.cfg.global_text_scale = scale;
        config::save_state(&self.cfg);
        // 本文とガターの galley はフォントサイズをキーに持っているので、
        // 倍率が変わったら作り直させる (残すと古いサイズのまま描かれる)。
        for b in &mut self.editor.buffers {
            b.cache = None;
            b.gutter = None;
        }
        self.toast(
            trf("🔠 文字サイズ {pct}", &[("pct", zoom::label(scale))]),
            true,
        );
    }

    /// 文字サイズ倍率を掛けたエディタのフォントサイズ。
    pub(super) fn scaled_editor_font(&self) -> f32 {
        (self.cfg.editor_font_size * self.cfg.text_scale).clamp(6.0, 96.0)
    }

    /// 文字サイズ倍率を掛けたターミナルのフォントサイズ。
    pub(super) fn scaled_terminal_font(&self) -> f32 {
        (self.cfg.terminal_font_size * self.cfg.text_scale).clamp(6.0, 96.0)
    }

    /// アクティブなタブだけのズームを段送りする。
    ///
    /// 本文とガターの galley はフォントサイズをキーに持っているので、
    /// ここでキャッシュを捨てる必要はない (次のフレームで自然に作り直される)。
    pub(super) fn step_file_zoom(&mut self, steps: i32) {
        let Some(i) = self.editor.active else {
            self.toast(
                tr("ファイルを開いてから、そのファイルだけのズームを変えてください"),
                false,
            );
            return;
        };
        let b = &mut self.editor.buffers[i];
        let next = zoom::step_by(b.zoom, steps);
        if (next - b.zoom).abs() < 1e-4 {
            return;
        }
        b.zoom = next;
    }

    /// アクティブなタブのズームを解除し、画面全体の倍率だけに戻す。
    pub(super) fn reset_file_zoom(&mut self) {
        let Some(i) = self.editor.active else {
            self.toast(
                tr("ファイルを開いてから、そのファイルだけのズームを変えてください"),
                false,
            );
            return;
        };
        self.editor.buffers[i].zoom = zoom::DEFAULT;
    }

    /// アクティブなタブのズーム倍率 (タブが無ければ等倍)。
    pub(super) fn file_zoom(&self) -> f32 {
        self.editor
            .active
            .map(|i| zoom::clamp(self.editor.buffers[i].zoom))
            .unwrap_or(zoom::DEFAULT)
    }

    /// エディタ本文のフォントサイズ (画面全体のズームは egui が持つので、
    /// ここで掛けるのは **ファイル単位の倍率だけ**)。
    ///
    /// 二重に掛けないこと — `zoom_factor` は pixels_per_point 側を動かすので、
    /// ここでも掛けると倍率が二乗になる。
    pub(super) fn editor_font_pt(&self) -> f32 {
        (self.scaled_editor_font() * self.file_zoom()).clamp(6.0, 96.0)
    }

    /// ⌘ + ホイール / トラックパッドのピンチを、ポインタの位置で振り分ける。
    ///
    /// エディタ本文 (と Markdown プレビュー) の上なら「そのファイルだけ」、
    /// それ以外 (ターミナル・サイドバー・タブ・看板など) なら「画面全体」。
    /// ブラウザで言えば前者が VS Code の `editor.mouseWheelZoom`、
    /// 後者がページズーム。
    ///
    /// **画像ビューアの上では何もしない。** あちらは `zoom_delta` を自分で
    /// 読んで画像を拡大するので、ここでも拾うと同じジェスチャで画像と UI が
    /// 二重に拡大する。持ち主は前フレームに申告された [`ZoomArea`] で決める。
    ///
    /// egui は ⌘+ホイールを `zoom_delta()` (1 フレームあたりの乗算係数) へ
    /// 変換済みで、スクロールからは取り除いてくれる。変化が無いフレームでは
    /// 1.0 が返るだけなので、毎フレーム呼んでもアイドルコストはゼロ。
    pub(super) fn handle_zoom_gesture(&mut self, ctx: &egui::Context) {
        let delta = ctx.input(|i| i.zoom_delta());
        if !delta.is_finite() || (delta - 1.0).abs() < 1e-6 {
            return;
        }
        let pointer = ctx.input(|i| i.pointer.latest_pos());
        // ポインタが乗っている領域の持ち主 (乗っていなければ None = 画面全体)
        let owner = match (pointer, self.zoom_area) {
            (Some(p), Some((r, kind))) if r.contains(p) => Some(kind),
            _ => None,
        };
        if owner == Some(ZoomArea::Image) {
            // 画像ビューアが自分で消費する。貯まりも持ち越さない。
            self.zoom_wheel.reset();
            return;
        }
        let on_file = owner == Some(ZoomArea::File) && self.editor.active.is_some();
        // 対象が変わったら貯まりを捨てる (画面全体の途中経過でファイルが
        // いきなり 1 段飛ぶ、を防ぐ)。
        if on_file != self.zoom_wheel_on_file {
            self.zoom_wheel.reset();
            self.zoom_wheel_on_file = on_file;
        }
        let steps = self.zoom_wheel.feed(delta);
        if steps == 0 {
            return;
        }
        if on_file {
            self.step_file_zoom(steps);
        } else {
            self.set_ui_zoom(zoom::step_by(self.cfg.ui_zoom, steps));
        }
    }

    /// `apply_cmd` のテーマ/設定/ペットカテゴリ (Cmd::SetTheme 〜 Cmd::ToggleRemote)。
    pub(super) fn apply_cmd_settings(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::SetTheme(name) => {
                self.theme = resolve_theme(&name);
                self.cfg.global_theme = name.clone();
                self.cfg.theme = name;
                theme::apply(ctx, &self.theme);
                for b in &mut self.editor.buffers {
                    b.cache = None;
                }
                config::save_state(&self.cfg);
                self.toast(
                    trf(
                        "🎨 {label} を適用しました",
                        &[("label", self.theme.label.clone())],
                    ),
                    true,
                );
            }
            Cmd::SetUiLanguage(id) => self.set_ui_language(&id, ctx),
            Cmd::OpenSettings => self.settings_open = true,
            Cmd::OpenConfig => {
                config::ensure_default();
                self.open_path(&config::config_path());
            }
            Cmd::ReloadConfig => {
                self.cfg = config::load(&self.roots, false);
                self.theme = resolve_theme(&self.cfg.theme);
                theme::apply(ctx, &self.theme);
                self.tree.show_hidden = self.cfg.show_hidden_files;
                self.tree.apply_config(&self.cfg);
                self.tree.invalidate();
                // 索引の上限・除外の扱いも設定に追従させる (作り直しは背景で走る)
                self.rebuild_index();
                self.rebuild_plugins();
                // 監視設定も入れ替える。サンプル間隔が変わるので次回刻みも捨てる。
                self.supervisor.set_config(self.cfg.supervisor.clone());
                self.sup_next_at = None;
                // 自動フェイルオーバーのしきい値も入れ替える (有効/無効も config どおり)。
                self.failover.set_config(self.cfg.failover.clone());
                // 監視役 LLM も選び直されているかもしれないので作り直す。
                self.apply_super_agent();
                self.keys = Keybinds::from_overrides(&self.cfg.keybindings);
                self.feature_keys =
                    crate::keybinds::FeatureBinds::from_overrides(&self.cfg.keybindings);
                // config.toml / state.toml の ui_zoom を画面へ戻す
                // (読み直したのに倍率だけ前のまま、を作らない)
                apply_ui_zoom(ctx, self.cfg.ui_zoom);
                crate::theme::set_text_scale(ctx, self.cfg.text_scale);
                // エディタの見た目 (括弧の色分け / ルーラー / インデント) も
                // 読み直した設定へ揃える。開いているタブのインデントは
                // 取り直す — 設定を変えたのにステータスバーだけ前のまま、を作らない。
                self.bracket_colorization = self.cfg.bracket_colorization;
                self.rulers = normalize_rulers(&self.cfg.rulers);
                self.sync_indent_defaults();
                for i in 0..self.editor.buffers.len() {
                    self.editor.apply_indent_defaults(i);
                }
                for b in &mut self.editor.buffers {
                    b.cache = None;
                    b.gutter = None;
                }
                config::save_state(&self.cfg);
                self.toast(tr("🔄 設定を再読み込みしました"), true);
            }
            Cmd::ZoomIn => self.set_ui_zoom(zoom::step_up(self.cfg.ui_zoom)),
            Cmd::ZoomOut => self.set_ui_zoom(zoom::step_down(self.cfg.ui_zoom)),
            Cmd::ZoomReset => self.set_ui_zoom(zoom::DEFAULT),
            Cmd::FileZoomIn => self.step_file_zoom(1),
            Cmd::FileZoomOut => self.step_file_zoom(-1),
            Cmd::FileZoomReset => self.reset_file_zoom(),
            Cmd::TextSizeIn => self.set_text_scale(zoom::step_up(self.cfg.text_scale)),
            Cmd::TextSizeOut => self.set_text_scale(zoom::step_down(self.cfg.text_scale)),
            Cmd::TextSizeReset => self.set_text_scale(zoom::DEFAULT),
            Cmd::SendFileToAgent => {
                let rel = self.editor.active.and_then(|i| {
                    let b = &self.editor.buffers[i];
                    b.path.as_ref().map(|p| {
                        self.root_for(p)
                            .and_then(|r| p.strip_prefix(r).ok())
                            .unwrap_or(p)
                            .to_string_lossy()
                            .to_string()
                    })
                });
                match rel {
                    Some(r) => self.send_to_agent(format!("@{r} ")),
                    None => self.toast(tr("保存済みのファイルを開いてください"), false),
                }
            }
            Cmd::RefreshTree => {
                self.tree.invalidate();
                self.rebuild_index();
                self.toast(tr("🌲 ツリーを再読み込みしました"), true);
            }
            Cmd::SetApproval(mode) => {
                let mode = match mode.as_str() {
                    "auto" | "agent" => mode,
                    _ => "ask".into(),
                };
                self.cfg.approval_mode = mode.clone();
                self.cfg.global_approval_mode = mode.clone();
                config::save_state(&self.cfg);
                match mode.as_str() {
                    "auto" => self.toast_warn(tr(
                        "⚡ 既定=全自動: 以後起動する Claude/Codex/Antigravity はすべて自動承認 (bypass フラグ付与)",
                    )),
                    "agent" => self.toast(
                        tr("👾 既定=Agent優先: 以後は各プリセットのコマンドどおりに起動します（(全自動) プリセットのみ自動承認）"),
                        true,
                    ),
                    _ => self.toast(
                        tr("🛡 既定=承認: 以後起動する Claude/Codex/Antigravity は操作ごとに許可が必要です"),
                        true,
                    ),
                }
                if self.agents.running_count() > 0 {
                    self.toast(
                        tr("実行中のセッションは各行の 🛡 ボタン（または 🛡 全切替）で切替できます"),
                        true,
                    );
                }
            }
            Cmd::ToggleWordWrap => {
                self.cfg.word_wrap = !self.cfg.word_wrap;
                self.cfg.global_word_wrap = self.cfg.word_wrap;
                // galley は折り返し設定込みでキャッシュしているため作り直す
                for b in &mut self.editor.buffers {
                    b.cache = None;
                    b.gutter = None;
                }
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.word_wrap {
                        tr("↩ 折り返し: オン (長い行をエディタ幅で折り返します)")
                    } else {
                        tr("↩ 折り返し: オフ (横スクロールに戻ります)")
                    },
                    true,
                );
            }
            Cmd::ToggleShowWhitespace => {
                self.cfg.show_whitespace = !self.cfg.show_whitespace;
                self.cfg.global_show_whitespace = self.cfg.show_whitespace;
                for b in &mut self.editor.buffers {
                    b.cache = None;
                }
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.show_whitespace {
                        tr("· 空白文字の表示: オン (スペース=· / タブ=→)")
                    } else {
                        tr("· 空白文字の表示: オフ")
                    },
                    true,
                );
            }
            Cmd::ToggleMinimap => {
                self.cfg.minimap = !self.cfg.minimap;
                self.cfg.global_minimap = self.cfg.minimap;
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.minimap {
                        tr("🗺 ミニマップ: オン (クリック / ドラッグでスクロールできます)")
                    } else {
                        tr("🗺 ミニマップ: オフ")
                    },
                    true,
                );
            }
            Cmd::ToggleShellIntegration => {
                self.cfg.shell_integration = !self.cfg.shell_integration;
                let on = self.cfg.shell_integration;
                // 有効化した時点でシムを書き出す (crate::shellint::set_enabled)。
                crate::shellint::set_enabled(on);
                config::save_state(&self.cfg);
                // 既に OSC を出しているシェル (iTerm2 / kitty / starship 等) では
                // シム側が降りる。**「入れたのに何も変わらない」の理由を先に言う** —
                // 黙って何もしないのが一番たちが悪い。
                let already = on
                    && crate::shellint::already_integrated(
                        &|k| std::env::var(k).ok(),
                        &crate::shellint::default_rc_files(),
                    );
                // **既存の端末には効かない**ことを隠さない。シェルの起動引数を
                // 変える機能なので、次に開いた端末からしか働かない。
                self.toast(
                    match (on, already) {
                        (true, true) => tr(
                            "🐚 シェル統合: オン — ただしお使いのシェル設定は既に OSC 133/633 を出しています。二重発行を避けるためシムは何もしません (受信側はそのまま働きます)",
                        ),
                        (true, false) => tr(
                            "🐚 シェル統合: オン — 次に開くターミナルから、コマンドの境界と終了コードをシェルが直接報告します",
                        ),
                        (false, _) => tr(
                            "🐚 シェル統合: オフ — 次に開くターミナルは従来どおり起動します (受信側は動いたまま)",
                        ),
                    },
                    true,
                );
            }
            Cmd::ToggleBreadcrumbs => {
                self.cfg.breadcrumbs = !self.cfg.breadcrumbs;
                self.cfg.global_breadcrumbs = self.cfg.breadcrumbs;
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.breadcrumbs {
                        tr("🔗 ブレッドクラム: オン (セグメントを押すと移動できます)")
                    } else {
                        tr("🔗 ブレッドクラム: オフ")
                    },
                    true,
                );
            }
            Cmd::ToggleGitBlame => {
                // 表示メニューの 1 項目からは 3 段を順に回す。
                // **どれかを直接選ぶ経路**はパレット (`blame.off` /
                // `blame.current` / `blame.all`) と設定画面にある。
                self.set_blame_mode(self.cfg.git_blame.next());
            }
            Cmd::TogglePet => {
                self.cfg.show_pet = !self.cfg.show_pet;
                self.cfg.global_show_pet = self.cfg.show_pet;
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.show_pet {
                        tr("🐾 ペットを表示しました")
                    } else {
                        tr("🐾 ペットを隠しました（🐾 で再表示）")
                    },
                    true,
                );
            }
            Cmd::CyclePermissionAll => {
                let n = self.agents.cycle_permission_all();
                if n > 0 {
                    self.toast_warn(trf(
                        "🛡 {n} 件のエージェントに権限モード切替を送信しました（各画面の表示を確認してください）",
                        &[("n", n.to_string())],
                    ));
                } else {
                    self.toast(tr("実行中の対応エージェントがありません"), false);
                }
            }
            Cmd::SetPetImage => {
                self.ask_dialog(
                    DialogPurpose::PetImage,
                    DialogSpec::pick_file()
                        .filter(tr("画像"), &["png", "jpg", "jpeg", "gif", "webp"]),
                    ctx,
                );
            }
            Cmd::ResetPetImage => {
                self.pet_tex = None;
                self.cfg.pet_image = None;
                config::save_state(&self.cfg);
                self.toast(tr("↺ ペットを既定の絵に戻しました"), true);
            }
            Cmd::ResetPetPos => {
                self.pet_pos = None;
                self.cfg.pet_x = None;
                self.cfg.pet_y = None;
                config::save_state(&self.cfg);
                self.toast(tr("🐾 ペットの位置を既定(右下)に戻しました"), true);
            }
            Cmd::SetPetVariant(name) => {
                self.cfg.pet_variant = name;
                config::save_state(&self.cfg);
            }
            Cmd::SetPetScale(s) => {
                self.cfg.pet_scale = s;
                config::save_state(&self.cfg);
            }
            Cmd::TogglePetFreeRoam => {
                self.cfg.pet_free_roam = !self.cfg.pet_free_roam;
                config::save_state(&self.cfg);
            }
            Cmd::TogglePetSleep => {
                self.cfg.pet_sleep = !self.cfg.pet_sleep;
                config::save_state(&self.cfg);
            }
            Cmd::TogglePetSounds => {
                self.cfg.pet_sounds = !self.cfg.pet_sounds;
                config::save_state(&self.cfg);
                self.toast(
                    if self.cfg.pet_sounds {
                        tr("🔔 効果音を有効にしました")
                    } else {
                        tr("🔕 効果音を無効にしました")
                    },
                    true,
                );
            }
            Cmd::TogglePetBubbles => {
                self.cfg.pet_bubbles = !self.cfg.pet_bubbles;
                config::save_state(&self.cfg);
            }
            Cmd::TogglePetAutoYes => {
                self.cfg.pet_auto_yes = !self.cfg.pet_auto_yes;
                config::save_state(&self.cfg);
            }
            // 「SSH で繋ぎたい」は用事が決まっている入口なので、
            // トグルではなく必ず開く (閉じるのは ✕ か 📱 ボタン)。
            Cmd::OpenSshRemote => {
                self.remote_open = true;
                self.fw.recheck();
            }
            Cmd::ToggleRemote => {
                self.remote_open = !self.remote_open;
                // 開くたびに調べ直す。1 度きりだと、別の Wi-Fi へ移った後や
                // 規則を外部で消された後も「✅ 許可済み」のまま固まり、
                // 画面と実態がずれる (= 許可したのに繋がらない、に見える)。
                if self.remote_open {
                    self.fw.recheck();
                }
            }
            _ => {}
        }
    }

    /// `apply_cmd` の音声入力/プラグインカテゴリ (Cmd::VoiceInput 〜 Cmd::RunPlugin)。
    pub(super) fn apply_cmd_voice_plugin(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::VoiceInput(target) => {
                // 🎤 のトグル。録音中に押したら止める
                if self.voice.session.is_some() {
                    self.stop_voice();
                } else {
                    self.start_voice(target, ctx);
                }
            }
            Cmd::VoiceStop => self.stop_voice(),
            Cmd::SetVoiceTarget(t) => {
                self.voice.target = t;
                self.voice.last_sent_to = None;
                self.voice.reset_live();
                self.cfg.voice_target = t.name().to_string();
                config::save_state(&self.cfg);
            }
            Cmd::SetVoiceEngine(e) => {
                self.cfg.voice_engine = e;
                config::save_state(&self.cfg);
                if self.cfg.voice_engine == "command" && self.cfg.voice_command.trim().is_empty() {
                    self.toast_warn(tr(
                        "外部エンジンを使うには config.toml の voice_command を設定してください",
                    ));
                } else {
                    self.toast(
                        trf(
                            "🎤 音声認識エンジン: {engine}",
                            &[("engine", self.cfg.voice_engine.clone())],
                        ),
                        true,
                    );
                }
            }
            Cmd::SetVoiceLang(l) => {
                self.cfg.voice_lang = l;
                config::save_state(&self.cfg);
                self.toast(
                    trf(
                        "🎤 認識言語: {lang}",
                        &[("lang", self.cfg.voice_lang.clone())],
                    ),
                    true,
                );
            }
            Cmd::SetVoiceKeyword(k) => {
                self.cfg.voice_keyword = k;
                config::save_state(&self.cfg);
                if self.cfg.voice_keyword.is_empty() {
                    self.toast(tr("🎤 送信は常に手動 Enter になりました"), true);
                } else {
                    self.toast(
                        trf(
                            "🎤 「{keyword}」と話すとそのまま送信します",
                            &[("keyword", self.cfg.voice_keyword.clone())],
                        ),
                        true,
                    );
                }
            }
            Cmd::NewPlugin => {
                if self.new_plugin_name.is_none() {
                    self.new_plugin_name = Some(String::new());
                }
            }
            Cmd::InstallPlugin => {
                self.ask_dialog(
                    DialogPurpose::InstallPlugin,
                    DialogSpec::pick_file().filter(tr("Zaivern プラグイン"), &["zvplug", "zip"]),
                    ctx,
                );
            }
            Cmd::RescanPlugins => {
                self.rebuild_plugins();
                self.toast(
                    trf(
                        "🔌 プラグインを再スキャンしました({n} 件)",
                        &[("n", self.plugins.len().to_string())],
                    ),
                    true,
                );
            }
            Cmd::ShowPlugins => {
                self.sidebar_open = true;
                self.sidebar_tab = SidebarTab::Plugins;
            }
            Cmd::RunPlugin(pi, ci) => {
                self.run_plugin_command(pi, ci, ctx);
            }
            _ => {}
        }
    }

    pub(super) fn run_action(&mut self, a: Action, ctx: &egui::Context) {
        match a {
            Action::OpenFile(p) => {
                // p は絶対パス (file_index が絶対パスを正として持つ)。
                // 同名の相対パスが複数ルートにあっても取り違えない。
                // ツリーと同じく**プレビュー**で開く — 探しているだけで
                // タブが増え続けないようにするため (設定でオフにできる)。
                self.open_path_preview(&p);
            }
            Action::OpenFileAt(p, line, col) => {
                self.open_path(&p);
                self.goto_line_col(line, col);
            }
            Action::Cmd(c) => self.apply_cmd(c, ctx),
        }
    }
}
