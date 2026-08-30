use super::*;

impl ZaivernApp {
    /// `roots` は必ず 1 件以上 (呼び出し側で `file_tree::normalize_roots` 済み)。
    /// `open_files` はコマンドライン引数で渡されたファイル (起動後に開く)。
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        roots: Vec<PathBuf>,
        open_files: Vec<PathBuf>,
    ) -> Self {
        install_fonts(&cc.egui_ctx);
        // 🏛 Team の状態は `thread_local!` に居るので**アプリより長生きする**。
        // 前のアプリが `on_exit` を通らずに消えていると、生き残った Runtime を
        // この新しいアプリがそのまま拾ってしまう (もう自分のものではない
        // セッションへ結び付いた Run を操作できる)。暗黙の引き継ぎはここで
        // 断つ — 保存してから手放すので、続きは復元の案内から入り直せる。
        crate::features::team::imp::panel::begin_app_context();
        // 空で渡されても決して空のままにしない (roots[0] が常に存在する不変条件)
        let roots = if roots.is_empty() {
            vec![std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))]
        } else {
            roots
        };
        let cfg = config::load(&roots, true);
        // シェル統合の注入は設定 1 つで決まる (既定 off)。ここで反映しないと
        // 「設定に書いたのに次の起動で効かない」になる。
        crate::shellint::set_enabled(cfg.shell_integration);
        // 画面全体のズームは **テーマ適用より先** に入れる。theme::apply は
        // その時点の pixels_per_point でフォントサイズを物理ピクセルへ丸めるので、
        // 後から倍率を変えると最初のフレームだけ丸めがズレた絵になる。
        apply_ui_zoom(&cc.egui_ctx, cfg.ui_zoom);
        crate::theme::set_text_scale(&cc.egui_ctx, cfg.text_scale);
        let theme = resolve_theme(&cfg.theme);
        theme::apply(&cc.egui_ctx, &theme);

        cc.egui_ctx
            .send_viewport_cmd(egui::ViewportCommand::Title(workspace_title(&roots)));

        let (plugin_tx, plugin_rx) = mpsc::channel();
        let (gh_tx, gh_rx) = mpsc::channel();
        // 外部 IDE の検出は IDE ごとにシェルを起動するので UI スレッドでは回さない。
        // 結果は ide::cached() に載るため、受信側はここで捨ててよい
        // (送信が Err になるだけで、検出結果そのものは失われない)。
        {
            let (ide_tx, _ide_rx) = mpsc::channel();
            ide::detect_async(ide_tx, cc.egui_ctx.clone());
        }
        // デッキの副題 (ブランチ) を裏で解決するための口。
        let (deck_branch_tx, deck_branch_rx) = mpsc::channel();
        let primary_root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));
        // ライセンスは `~/.zaivern/license.key` を 1 回読んで署名検証するだけ。
        // ネットワークは叩かない (通信ゼロの約束を破らない)。未ライセンスでも
        // アプリは完全に動くので、失敗しても起動を止めない。
        let license_boot = license::current_status();
        // Hot Exit の退避先はワークスペース (ルート集合) ごと。
        // `roots` はこの後で構造体へ移るので、先に取っておく。
        let hotexit = session::HotExitStore::new(
            session::hotexit_dir_for(&roots),
            cfg.hot_exit_max_kb.saturating_mul(1024),
        );
        let mut app = Self {
            tree: FileTree::new(roots.clone(), cfg.show_hidden_files),
            gitinfo: git::GitSet::new(roots.clone()),
            blame: git::Blame::default(),
            commit_diff_cache: HashMap::new(),
            checkpoints: checkpoint::Checkpoints::new(
                roots.first().cloned().unwrap_or_else(|| PathBuf::from(".")),
            ),
            checkpoint_pending: None,
            local_history: local_history::LocalHistory::new(
                roots.first().cloned().unwrap_or_else(|| PathBuf::from(".")),
                &cfg,
            ),
            tab_drag: None,
            git_panel: git_panel::GitPanel::new(
                roots.first().cloned().unwrap_or_else(|| PathBuf::from(".")),
            ),
            review: git_panel::ReviewPanel::new(
                roots.first().cloned().unwrap_or_else(|| PathBuf::from(".")),
            ),
            git_ops: GitOps::default(),
            git_sub_review: false,
            compare_left: None,
            compare_view: None,
            fold_view: None,
            sticky_cache: None,
            guide_cache: None,
            lsp_completion: lsp::CompletionState::new(),
            lsp_completion_buf: None,
            lsp_hover: lsp::HoverState::new(),
            lsp_hover_flight: None,
            hover_doc_pos: None,
            caret_screen: None,
            lsp_hover_pos: None,
            lsp_refs: Vec::new(),
            lsp_refs_open: false,
            lsp_refs_busy: false,
            lsp_symbols: Vec::new(),
            lsp_symbols_open: false,
            lsp_symbols_busy: false,
            lsp_symbols_query: String::new(),
            lsp_symbols_path: None,
            lsp_symbols_quiet: false,
            lsp_rename: None,
            lsp_format_buf: None,
            lsp_actions: Vec::new(),
            lsp_actions_open: false,
            lsp_actions_busy: false,
            lsp_actions_sel: 0,
            lsp_actions_key: None,
            lsp_actions_anchor: None,
            lsp_signature: None,
            lsp_highlight: lsp::HighlightState::new(),
            lsp_highlight_spans: Vec::new(),
            lsp_highlight_buf: None,
            lsp_highlight_on: cfg.lsp_highlight_occurrences,
            editor_sel_chars: None,
            format_on_save: cfg.format_on_save,
            bracket_colorization: cfg.bracket_colorization,
            rulers: normalize_rulers(&cfg.rulers),
            ext_check_at: None,
            // ここでは `egui::Context` を持てない (`new` はフレームの外)。
            // 最初の `watch_tick` で起こす。
            fswatch: None,
            keys: Keybinds::from_overrides(&cfg.keybindings),
            feature_keys: crate::keybinds::FeatureBinds::from_overrides(&cfg.keybindings),
            theme,
            // 起動引数で指定されたフォルダ (`zai .` / `zai <dir>`) を作業フォルダの
            // 初期値にする。セッション復元でルートが増えても、ユーザーが
            // 「ここで開いた」フォルダでエージェントが起動するようにするため。
            agent_root: roots.first().cloned(),
            roots,
            editor: Editor::new(),
            panes: editor_split::EditorPanes::new(),
            cur_pane: 1,
            tab_switcher: None,
            tab_scrolled: HashMap::new(),
            tasks_cache: TasksCache::default(),
            agents: AgentManager::new(),
            splits: HashMap::new(),
            split_rect: HashMap::new(),
            palette: {
                // MRU (state.toml) を復元。実体 (Cmd) は保存していないので、
                // パレットを最初に開いたフレームで組み込み表から引き直す。
                let mut p = Palette::new();
                let saved: Vec<(String, String, u32)> = cfg
                    .palette_recent
                    .iter()
                    .map(|r| (r.label.clone(), r.icon.clone(), r.uses))
                    .collect();
                p.restore_recent(&saved);
                p
            },
            palette_worktrees: None,
            outbox: Vec::new(),
            race: race::RacePanel::new(),
            agent_worktrees: HashMap::new(),
            manual_titles: std::collections::HashSet::new(),
            rename_agent: None,
            turns: crate::agents::naming::TurnWatcher::default(),
            namer: crate::agents::naming::Namer::default(),
            named_for: HashMap::new(),
            conflicts: worktree::ConflictWatch::new(),
            conflict_detail: false,
            conflict_radar: conflict::ConflictRadar::new(),
            radar_open: false,
            radar_pair: None,
            pending_stop_all: false,
            pending_worktree: None,
            highlighter: crate::highlight::shared(),
            hl_ready: HashMap::new(),
            hl_windowed: HashMap::new(),
            cockpit: false,
            cockpit_followed: None,
            center: CenterView::Editor,
            marks: marks::MarksState::new(&primary_root),
            kanban: false,
            kanban_state: kanban::KanbanState::default(),
            fleet: crate::fleet::FleetStore::default(),
            remote_fleet_reads: Vec::new(),
            deck: false,
            deck_state: deck::DeckState::default(),
            changes: false,
            changes_state: crate::changes_view::ChangesState::default(),
            deck_branches: HashMap::new(),
            deck_branch_pending: HashSet::new(),
            deck_branch_tx,
            deck_branch_rx,
            md_preview: false,
            md_images: markdown::ImageCache::default(),
            md_pre_cache: None,
            img_zoom: HashMap::new(),
            checker_tex: None,
            sidebar_open: true,
            sidebar_tab: SidebarTab::Files,
            sidebar_sessions: session_picker::SidebarState::default(),
            sess_folders: Vec::new(),
            sess_folders_src: Vec::new(),
            branch_nav: git::BranchNav::new(primary_root.clone()),
            file_index: Vec::new(),
            index_at: None,
            index_rx: None,
            index_progress: Arc::new(AtomicUsize::new(0)),
            index_truncated: false,
            index_gen: 0,
            custom_themes: Vec::new(),
            find: FindState {
                open: false,
                query: String::new(),
                focus: false,
                current: None,
                anchor: 0,
                wrapped: None,
                replace_open: false,
                replace: String::new(),
                opts: find_buffer::FindOptions::default(),
            },
            menu_state: recent::load(),
            autosave_at: None,
            lease_armed_for: None,
            lease_armed_notified: false,
            goto_open: false,
            goto_input: String::new(),
            problems_open: false,
            problems_filter: ProblemsFilter::default(),
            problems_collapsed: HashSet::new(),
            shortcuts_open: false,
            keybind_ui: KeybindUi::default(),
            settings_open: false,
            settings_ui: SettingsUi::default(),
            hooks_log: String::new(),
            hotexit,
            hotexit_due: None,
            hotexit_fingerprint: 0,
            hotexit_conflicts: Vec::new(),
            hotexit_warned: HashSet::new(),
            chord: crate::keybinds::ChordState::default(),
            whichkey: crate::whichkey::WhichKey::default(),
            whichkey_live: Vec::new(),
            about_open: false,
            whats_new: Vec::new(),
            // 起動時に 1 回だけローカルのキーを読んで検証する (通信なし)。
            license_open: false,
            license_input: String::new(),
            license_key: license_boot.0,
            license_status: license_boot.1,
            fake_fullscreen: None,
            broken_native_fs: Vec::new(),
            fs_rescue_pending: false,
            fs_rescue_from: None,
            fs_rescue_at: None,
            fs_last_rect: None,
            fs_rect_moved_at: None,
            fake_fs_restore: None,
            fs_toggle_at: None,
            fs_broken_since: None,
            gsearch: GlobalSearchState::new(),
            nav_history: Vec::new(),
            nav_index: 0,
            pending_editor_events: Vec::new(),
            awaiting_definition: None,
            toasts: Vec::new(),
            pending_close: None,
            pending_delete: None,
            pending_transfer: None,
            file_history: FileHistory::default(),
            pending_select: None,
            pending_select_focus: true,
            pending_scroll: None,
            undo_clock: Instant::now(),
            last_row_h: 18.0,
            last_view_h: 620.0,
            zoom_area: None,
            zoom_area_next: None,
            zoom_wheel: zoom::WheelAccum::default(),
            zoom_wheel_on_file: false,
            last_scroll_y: 0.0,
            last_text_hash: 0,
            find_hits: None,
            multibuffer_cursor: HashMap::new(),
            breadcrumb_symbols_asked: None,
            remote: None,
            remote_err: None,
            remote_open: false,
            fw: firewall::FirewallUi::default(),
            voice: VoiceState::default(),
            qr_tex: None,
            qr_url: String::new(),
            tunnel: tunnel::Tunnel::new(cc.egui_ctx.clone()),
            ts: crate::tailscale::Probe::default(),
            https: crate::tailscale::Https::default(),
            https_err: None,
            tunnel_host: cfg.ssh_tunnel_host.clone(),
            tunnel_err: None,
            agent_input_buf: crate::agent_input::AgentInputBuffer::new(),
            mention: mention::Mention::default(),
            mention_rels: Vec::new(),
            mention_syms: Vec::new(),
            quota: coordinator::QuotaWatch::new(),
            quota_open: false,
            token_detail: false,
            cost_alert: None,
            cost_stamp: None,
            cost_spent: (0.0, 0.0),
            cost_gate: notify::EdgeGate::default(),
            failover: failover::Failover::new(cfg.failover.clone()),
            save_trim_trailing: cfg.trim_trailing_whitespace,
            save_trim_final_newlines: cfg.trim_final_newlines,
            save_final_newline: cfg.insert_final_newline,
            prefs_loaded: false,
            le_cache: None,
            pet_pos: match (cfg.pet_x, cfg.pet_y) {
                (Some(x), Some(y)) => Some(egui::pos2(x, y)),
                _ => None,
            },
            pet_tex: None,
            pet_rt: pet::PetRuntime::default(),
            sound: sound::SoundPlayer::default(),
            pet_happy_until: None,
            pet_error_until: None,
            pet_bubble_dismissed: HashSet::new(),
            pet_bubble_answered: HashMap::new(),
            pet_attention_notified: HashMap::new(),
            follow: follow::Follow::default(),
            work_gate: notify::WorkGate::default(),
            anomaly_gate: notify::EdgeGate::default(),
            plugins: Vec::new(),
            plugin_langs: HashMap::new(),
            plugin_keys: Vec::new(),
            plugin_tx,
            plugin_rx,
            github: panels::GithubPanel::default(),
            gh_tx,
            gh_rx,
            new_plugin_name: None,
            agent_picker: AgentPicker::default(),
            snippets_by_lang: HashMap::new(),
            lsp: HashMap::new(),
            lsp_opened: HashSet::new(),
            lsp_pending: HashMap::new(),
            lsp_which_missing: HashMap::new(),
            diag_counts: (0, 0),
            // 初期キーは番兵 (u64::MAX): 最初の refresh で必ず作り直す
            diag_cache: diagview::DiagCache::default(),
            inlay_cache: diagview::InlayCache::default(),
            bracket_hl: None,
            diag_hover: None,
            plugin_panels: HashMap::new(),
            plugin_status: String::new(),
            hook_last_run: HashMap::new(),
            panel_last_run: HashMap::new(),
            startup_hooks_done: false,
            frame_guard: FrameGuard::default(),
            dialogs: DialogJobs::default(),
            hook_git_branch: None,
            pending_hooks: Vec::new(),
            plugins_tab_was_open: false,
            supervisor: supervisor::Supervisor::new(cfg.supervisor.clone()),
            coordinator: coordinator::Coordinator::new(),
            pending_intervention: Vec::new(),
            pending_stop: Vec::new(),
            stopping: Vec::new(),
            orch: orchestration::OrchState::default(),
            known_sessions: HashSet::new(),
            sup_last_state: HashMap::new(),
            sup_next_at: None,
            typed_voice: HashMap::new(),
            typed_sup: HashMap::new(),
            report_colors: HashMap::new(),
            super_agent_session: None,
            sup_last_diag: HashMap::new(),
            commander_seen: HashSet::new(),
            commander_seen_order: std::collections::VecDeque::new(),
            term_focus_pending: false,
            tutorial: tutorial::Tutorial::new(),
            tutorial_autostarted: false,
            approvals_view: false,
            approvals_audit: false,
            approvals_audit_cache: None,
            approvals_expanded: HashSet::new(),
            acp: acp::AcpManager::default(),
            mcp_view: false,
            mcp: mcp::McpPanel::default(),
            skills_view: false,
            skills: skills::SkillsPanel::default(),
            spec_view: false,
            spec: spec::SpecPanel::default(),
            multi_sel: None,
            multi_sticky_col: None,
            column_anchor: None,
            enc_picker: None,
            enc_filter: String::new(),
            cfg,
        };
        // 設定で指名されている指揮官を反映する (居なければ起動を待つだけ)。
        app.apply_super_agent();
        // ユーザー指定のペット画像をロード
        if let Some(path) = app.cfg.pet_image.clone() {
            app.pet_tex = load_pet_texture(&cc.egui_ctx, Path::new(&path));
        }
        app.rebuild_plugins();
        // スマホリモートサーバを起動 (LAN 内からブラウザで操作可能に)。
        // 既定は LAN。SSH トンネルを張るときだけ 127.0.0.1 へ絞る。
        match remote::RemoteServer::start(cc.egui_ctx.clone(), remote::Bind::Lan) {
            Ok(s) => {
                // CLI (`zai open` など) が接続先を見つけられるよう接続情報を書き出す
                let ws = app.primary_root().to_string_lossy().to_string();
                if let Err(e) = cli::write_instance_file(s.port, &s.token, &ws) {
                    eprintln!("インスタンス情報の書き出しに失敗しました: {e}");
                }
                app.remote = Some(s);
            }
            Err(e) => app.remote_err = Some(e),
        }
        app.tree.apply_config(&app.cfg);
        app.rebuild_index();
        app.restore_session(&cc.egui_ctx);
        // コマンドラインで渡されたファイルはセッション復元の後に開く
        // (最後に開いたものがアクティブになる)
        for f in open_files {
            app.open_path(&f);
        }
        // 更新後の初回起動でだけ「この版の新機能」を出す。
        // **セッション復元の後**に置く — 復元中に開くと、復元でレイアウトが
        // 変わる最中に窓が出て「画面が突然変わる」ように見える。
        app.whats_new_on_start();
        app
    }

    /// primary ルート (= `roots[0]`)。単一ルート時代の `self.workspace` 相当。
    /// ダイアログの初期ディレクトリ・エージェントの cwd 等、
    /// 「1 つ選ぶしかない」場面で使う。
    pub(super) fn primary_root(&self) -> &Path {
        self.roots
            .first()
            .map(|p| p.as_path())
            .unwrap_or(Path::new("."))
    }

    /// これから起動するエージェント / ターミナル / ビルドタスクの作業フォルダ。
    /// 直近に開いた・選んだフォルダ (`agent_root`) を優先し、無ければ primary ルート。
    pub(super) fn agent_cwd(&self) -> PathBuf {
        agent_cwd_from(&self.roots, self.agent_root.as_deref())
    }

    /// `path` を含むルートを作業フォルダとして覚える (次回以降の起動先になる)。
    /// どのルートにも属さないパス (`~/.zaivern/config.toml` など) では何もしない
    /// — 設定ファイルを開いただけでエージェントの起動先が飛ぶのを避ける。
    pub(super) fn track_agent_root(&mut self, path: &Path) {
        let canon = pathx::canonical(path);
        if let Some(root) = self.root_for(&canon) {
            self.agent_root = Some(root.to_path_buf());
        }
    }

    /// トースト等の表示用: 所属ルートからの相対パス。
    /// 複数ルートのときはどのフォルダの話か分かるようルート名を前置する。
    /// どのルートにも属さなければ絶対パスのまま。
    pub(super) fn rel_label(&self, p: &Path) -> String {
        match self.root_for(p).and_then(|r| {
            p.strip_prefix(r)
                .ok()
                .map(|rel| (root_name(r), rel.display().to_string()))
        }) {
            Some((name, rel)) if self.roots.len() > 1 => format!("{name}/{rel}"),
            Some((_, rel)) => rel,
            None => p.display().to_string(),
        }
    }

    /// `p` を含むルート (最長一致)。どのルートにも属さなければ None。
    pub(super) fn root_for(&self, p: &Path) -> Option<&Path> {
        file_tree::root_for(&self.roots, p)
    }

    /// ルート一覧を差し替え、ツリー / git / 索引 / タイトルを追随させる。
    /// 現在のセッションを先に保存してから切り替える。
    /// `roots` が空になる呼び出しは無視する (不変条件を守る)。
    pub(super) fn set_roots(&mut self, roots: Vec<PathBuf>, ctx: &egui::Context) {
        if roots.is_empty() {
            return;
        }
        self.persist_session();
        self.apply_roots(roots, ctx);
    }

    /// `set_roots` からセッション保存を除いた部分 (セッション復元中に使う)。
    pub(super) fn apply_roots(&mut self, roots: Vec<PathBuf>, ctx: &egui::Context) {
        if roots.is_empty() {
            return;
        }
        self.roots = roots;
        self.tree.set_roots(self.roots.clone());
        self.gitinfo.set_roots(self.roots.clone());
        // Git パネルは単一 repo 表示。GitSet と同じ「primary ルート」を見せることで、
        // サイドバーとステータスバーのブランチ表示が食い違わないようにする。
        self.git_panel
            .set_workspace(self.primary_root().to_path_buf());
        self.review.set_workspace(self.primary_root().to_path_buf());
        // ブックマークもワークスペース単位。前のぶんは書き出してから読み替える。
        let ws = self.primary_root().to_path_buf();
        self.marks.set_workspace(&ws);
        // ブランチピッカーも新しいリポジトリへ。旧 repo の一覧が残っていると、
        // そこに無いブランチへの切り替えを発行できてしまう。
        self.branch_nav.set_repo(self.primary_root().to_path_buf());
        // チェックポイントも新しいリポジトリへ。旧 repo の sha を握ったままだと
        // 別リポジトリへ復元を撃ててしまう。
        self.checkpoints.set_repo(self.primary_root().to_path_buf());
        // state.toml の UI 選択 (テーマ等) は維持したいので with_state = true
        self.cfg = config::load(&self.roots, true);
        // ローカルヒストリも新しいワークスペースへ (別プロジェクトの履歴へ
        // 復元を撃てないよう、裏のスレッドごと捨てて張り直す)。
        let lh_root = self.primary_root().to_path_buf();
        self.local_history.set_workspace(lh_root, &self.cfg);
        crate::shellint::set_enabled(self.cfg.shell_integration);
        self.tree.show_hidden = self.cfg.show_hidden_files;
        self.tree.apply_config(&self.cfg);
        self.rebuild_index();
        // CLI (`zai open <file>`) が見る接続情報も新しいワークスペースへ更新する。
        // 起動時に書いたままだと、フォルダを開き直した後の相対パス解決が
        // 前のワークスペース基準になってしまう。
        if let Some(s) = &self.remote {
            let ws = self.primary_root().to_string_lossy().to_string();
            if let Err(e) = cli::write_instance_file(s.port, &s.token, &ws) {
                eprintln!("インスタンス情報の更新に失敗しました: {e}");
            }
        }
        ctx.send_viewport_cmd(egui::ViewportCommand::Title(workspace_title(&self.roots)));
    }

    // ─── プラグイン (コマンド / スニペット / テーマ) ─────────────────

    /// インストール済みプラグインを再スキャンし、スニペット辞書・テーマ一覧・
    /// コマンドキーバインドを作り直す。
    pub(super) fn rebuild_plugins(&mut self) {
        use plugins::PluginList;

        // 同梱の標準プラグインを ~/.zaivern/plugins へ展開してからスキャンする
        // (バンドル版が新しいときだけ上書きするので、ユーザーの編集は残る)
        let mut existed: HashSet<String> = HashSet::new();
        if let Some(root) = plugins::plugins_root() {
            // 「初回インストール」の検知用に、展開前からあったディレクトリ名を控える
            if let Ok(rd) = std::fs::read_dir(&root) {
                existed = rd
                    .flatten()
                    .filter_map(|e| e.file_name().to_str().map(str::to_string))
                    .collect();
            }
            plugins::seed_bundled(&root);
        }
        self.plugins = plugins::scan_installed();
        // `default_enabled = false` のプラグイン (english-mode 等、入れただけで
        // 挙動が変わるもの) は初回展開時だけ無効で始める。一度でもユーザーが
        // 触った後は cfg 側の記録に従う (更新シードで勝手に無効へ戻さない)。
        let mut newly_disabled = false;
        for p in self.plugins.iter() {
            if !p.default_enabled
                && !existed.contains(&p.name)
                && !self.cfg.plugins.disabled.iter().any(|d| d == &p.name)
            {
                self.cfg.plugins.disabled.push(p.name.clone());
                if !self.cfg.global_plugins.disabled.contains(&p.name) {
                    self.cfg.global_plugins.disabled.push(p.name.clone());
                }
                newly_disabled = true;
            }
        }
        if newly_disabled {
            if let Err(e) = config::save_plugins_section(&self.cfg) {
                self.toast(trf("設定の保存に失敗: {e}", &[("e", e.to_string())]), false);
            }
        }
        // 無効化リストと保存済み設定値を反映する。無効なプラグインは
        // 以降の登録 (コマンド/キーバインド/テーマ/スニペット) から一切外れる
        self.plugins.apply_disabled(&self.cfg.plugins.disabled);
        self.plugins.apply_all_settings(&self.cfg.plugins.settings);

        // UI 言語 (旧形式): 有効な言語プラグインの TOML 辞書を i18n へ入れる。
        // 無ければ None。複数あれば名前順の先頭 (scan_installed が名前順)。
        // **Language Pack (locales/*.json) が訳を持つ文字列はそちらが勝つ** ので、
        // これは「同梱に無い訳を足す」後方互換の層として残っている。
        // 読めない辞書は黙って捨てず、理由をトーストで見せる。
        let mut dict: Option<HashMap<String, String>> = None;
        let mut lang_err: Option<String> = None;
        for p in self.plugins.iter().filter(|p| p.active()) {
            let Some(lang) = &p.language else { continue };
            let Some(path) = &lang.dict else { continue };
            match i18n::load_dict(path) {
                Ok(d) => dict = Some(d),
                Err(e) => lang_err = Some(format!("{}: {e}", p.name)),
            }
            break;
        }
        i18n::set_dict(dict);
        if let Some(e) = lang_err {
            self.toast(trf("⚠ UI 言語辞書を読めません — {e}", &[("e", e)]), false);
        }

        // Language Pack。プラグインが `locales` を増やしていることがあるので、
        // プラグインを組み直したあとで必ず引き直す。
        // **有効な言語プラグインがあれば、それが選択そのもの** (プラグイン画面の
        // 有効/無効と 🌐 の選択が食い違わないよう、`ui_language` へ写して
        // 真実の在り処を 1 つに保つ)。
        self.adopt_language_plugin_choice();
        self.apply_ui_language();

        // 閉じたプラグインのパネル内容は残さない
        self.plugin_panels.retain(|(pl, id), _| {
            self.plugins
                .iter()
                .any(|p| p.active() && &p.name == pl && p.panels.iter().any(|x| &x.id == id))
        });

        // 構文ハイライト定義 (`[[syntax]]`) を集めて Highlighter へ入れる。
        // syntect が知らない言語 (TypeScript / Kotlin / Zig …) はここで足される。
        // 読めない定義は黙って捨てず、理由をトーストで見せる (作者がすぐ気づけるように)。
        let mut packs = crate::grammar::GrammarSet::default();
        let mut syn_errs: Vec<String> = Vec::new();
        let mut by_plugin: HashMap<String, Vec<String>> = HashMap::new();
        for p in self.plugins.iter().filter(|p| p.active()) {
            let mut mine = crate::grammar::GrammarSet::default();
            for path in &p.syntax_files {
                mine.merge(crate::grammar::GrammarSet::load_path(path, &mut syn_errs));
            }
            if !mine.is_empty() {
                let mut names: Vec<String> = mine.names().iter().map(|s| s.to_string()).collect();
                names.sort_by_key(|s| s.to_lowercase());
                by_plugin.insert(p.name.clone(), names);
            }
            packs.merge(mine);
        }
        self.plugin_langs = by_plugin;
        let had_langs = self.highlighter.extra_lang_count();
        let now_langs = packs.grammars.len();
        self.highlighter.set_grammars(packs);
        if had_langs != now_langs {
            // 認識できる言語が変わったので、開いているタブを判定し直す
            // (プラグインを有効にした瞬間に、開いたままの .ts が色づく)。
            for b in self.editor.buffers.iter_mut() {
                if let Some(path) = b.path.clone() {
                    b.lang = self.highlighter.lang_for(Some(&path), &b.text);
                }
            }
        }
        if let Some(e) = syn_errs.first() {
            self.toast(
                trf("⚠ 構文定義を読めません — {e}", &[("e", e.clone())]),
                false,
            );
        }

        // スニペットを言語IDごとに集約
        let mut by_lang: HashMap<String, Vec<Snippet>> = HashMap::new();
        for p in self.plugins.iter().filter(|p| p.active()) {
            for (lang, path) in &p.snippet_files {
                let snips = snippets::parse_file(path, lang);
                if !snips.is_empty() {
                    by_lang.entry(lang.clone()).or_default().extend(snips);
                }
            }
        }
        self.snippets_by_lang = by_lang;

        // テーマ一覧 = ~/.zaivern/themes + プラグイン同梱テーマ(パスで重複排除)
        let mut themes = theme_json::discover_user_themes();
        let mut seen: HashSet<String> = themes.iter().map(|(_, p)| p.clone()).collect();
        for p in self.plugins.iter().filter(|p| p.active()) {
            for (label, path) in &p.themes {
                let ps = path.to_string_lossy().to_string();
                if seen.insert(ps.clone()) {
                    themes.push((label.clone(), ps));
                }
            }
        }
        themes.sort_by_key(|a| a.0.to_lowercase());
        self.custom_themes = themes;

        // コマンドの keybind をパースしてキャッシュ (不正な文字列は無視)
        self.plugin_keys.clear();
        for (pi, p) in self.plugins.iter().enumerate() {
            if !p.active() {
                continue;
            }
            for (ci, c) in p.commands.iter().enumerate() {
                if let Some(sc) = c.keybind.as_deref().and_then(parse_shortcut) {
                    self.plugin_keys.push((sc, pi, ci));
                }
            }
        }
    }

    /// アクティブバッファでいま選択されているテキスト (無選択なら None)。
    /// 同梱している言語プラグインの名前と言語 ID (`("korean-mode", "ko")` …)。
    ///
    /// **インストール済みのものだけ**を見る。名前を決め打ちしないので、
    /// あとから言語プラグインが増えても、ここを触らずに追随する。
    pub(super) fn language_plugins(&self) -> Vec<(String, String)> {
        self.plugins
            .iter()
            .filter_map(|p| p.language.as_ref().map(|l| (p.name.clone(), l.id.clone())))
            .collect()
    }

    /// プラグイン画面での有効/無効を `cfg.ui_language` へ写す。
    ///
    /// * 有効な言語プラグインがある → その言語を選んだことにする
    /// * 1 つも無く、いま選んでいる言語の**プラグインが入っている** (= 人が
    ///   無効にした) → 「自動」へ戻す
    ///
    /// 選んでいる言語にプラグインが存在しないとき (`~/.zaivern/locales/fr.json`
    /// のようなコミュニティ言語) は**触らない**。プラグインが無いことを
    /// 「無効にした」と読み違えると、選んだ言語が毎回勝手に戻ってしまう。
    fn adopt_language_plugin_choice(&mut self) {
        let installed = self.language_plugins();
        let active: Option<String> = self
            .plugins
            .iter()
            .filter(|p| p.active())
            .find_map(|p| p.language.as_ref().map(|l| l.id.clone()));
        // 手で config.toml を書くなどして**複数の言語パックが有効**になっている
        // ことがある。先頭以外を無効へ倒して「同時に 1 つだけ」を回復する
        // (放っておくと、画面では 2 つ有効なのに効いているのは片方、になる)。
        if let Some(first) = active.clone() {
            let mut fixed = false;
            let mut seen = false;
            for (name, lang) in &installed {
                if !self.cfg.plugins.is_enabled(name) {
                    continue;
                }
                if *lang == first && !seen {
                    seen = true;
                    continue;
                }
                self.cfg.plugins.set_enabled(name, false);
                self.cfg.global_plugins.set_enabled(name, false);
                fixed = true;
            }
            if fixed {
                let _ = config::save_plugins_section(&self.cfg);
                crate::plugins::PluginList::apply_disabled(
                    self.plugins.as_mut_slice(),
                    &self.cfg.plugins.disabled,
                );
            }
        }
        // **有効なものが無いときは何もしない。**
        // 「無効にしたら自動へ戻す」は plugin パネルの切り替え側
        // (`set_ui_language(auto)`) が既にやっている。ここでも同じ判断をすると、
        // `zai lang set ko` のように**プラグインを使わずに言語を選んだ**設定を
        // 起動のたびに `auto` へ書き戻してしまう (CLI で選べなくなる)。
        let Some(want) = active else { return };
        if self.cfg.ui_language == want {
            return;
        }
        self.cfg.ui_language = want.clone();
        let mut v = std::collections::BTreeMap::new();
        v.insert(
            "ui_language".to_string(),
            config::SettingValue::Text(want).to_toml(),
        );
        if let Err(e) = config::save_settings(&v) {
            self.toast(trf("設定の保存に失敗: {e}", &[("e", e)]), false);
        }
    }

    /// `~/.zaivern/locales` を作って開く (翻訳を始める人の入口)。
    pub(crate) fn open_locales_dir(&mut self) {
        let dir = config::zaivern_dir().join("locales");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast(
                trf("置き場を作れません: {e}", &[("e", e.to_string())]),
                false,
            );
            return;
        }
        self.open_path(&dir);
        self.toast(
            trf(
                "🌐 {path} を開きました — ここに <言語ID>.json を置くと 🌐 の一覧に並びます",
                &[("path", dir.display().to_string())],
            ),
            true,
        );
    }

    /// いまの言語の**翻訳の雛形**を `~/.zaivern/locales/<id>.json` へ書き出して開く。
    ///
    /// 中身は「同梱 `en` の全キー」に、その言語の既訳が入っていれば入った状態。
    /// **上書きはしない** — 既にあるファイルを消してしまうと、途中まで訳した
    /// ものが黙って消える。既にあるときはそれをそのまま開く。
    pub(crate) fn export_locale_template(&mut self) {
        let id = i18n::current();
        let dir = config::zaivern_dir().join("locales");
        if let Err(e) = std::fs::create_dir_all(&dir) {
            self.toast(
                trf("置き場を作れません: {e}", &[("e", e.to_string())]),
                false,
            );
            return;
        }
        let path = dir.join(format!("{id}.json"));
        if path.is_file() {
            self.open_path(&path);
            self.toast(
                trf(
                    "🌐 すでにあります: {path} (上書きしていません)",
                    &[("path", path.display().to_string())],
                ),
                true,
            );
            return;
        }
        let mut errs = Vec::new();
        let map = locale::resolved(&id, &self.plugin_locale_dirs(), &mut errs);
        // BTreeMap にしてから書く — **ID 順に並んでいないと差分が読めない**
        let sorted: std::collections::BTreeMap<&String, &String> = map.iter().collect();
        let body = match serde_json::to_string_pretty(&sorted) {
            Ok(b) => b,
            Err(e) => {
                self.toast(trf("書き出しに失敗: {e}", &[("e", e.to_string())]), false);
                return;
            }
        };
        match std::fs::write(&path, body + "\n") {
            Ok(()) => {
                self.open_path(&path);
                self.toast(
                    trf(
                        "🌐 雛形を書き出しました: {path} ({n} 件)",
                        &[
                            ("path", path.display().to_string()),
                            ("n", map.len().to_string()),
                        ],
                    ),
                    true,
                );
            }
            Err(e) => self.toast(trf("書き出しに失敗: {e}", &[("e", e.to_string())]), false),
        }
    }

    /// `tr()` が訳を引けなかった文字列を書き出す (訳漏れ探し)。
    ///
    /// 集めるのは `ZAIVERN_I18N_TRACE=1` で起動しているあいだだけ。
    /// **既定で集めない**のは、要らない人に費用を払わせないため (設計原則 3)。
    pub(crate) fn dump_missing_translations(&mut self) {
        let keys = i18n::missing_keys();
        if keys.is_empty() {
            self.toast(
                tr("訳漏れはまだ 0 件です (集めるには ZAIVERN_I18N_TRACE=1 を付けて起動)"),
                false,
            );
            return;
        }
        let dir = config::zaivern_dir().join("locales");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("missing-{}.json", i18n::current()));
        let body = serde_json::to_string_pretty(&keys).unwrap_or_default();
        match std::fs::write(&path, body + "\n") {
            Ok(()) => {
                self.open_path(&path);
                self.toast(
                    trf(
                        "🌐 訳が無い文字列を {n} 件書き出しました: {path}",
                        &[
                            ("n", keys.len().to_string()),
                            ("path", path.display().to_string()),
                        ],
                    ),
                    true,
                );
            }
            Err(e) => self.toast(trf("書き出しに失敗: {e}", &[("e", e.to_string())]), false),
        }
    }

    /// プラグインが供給する Language Pack ディレクトリ (`[language] locales`)。
    ///
    /// 有効なプラグインのぶんだけ。無効化した瞬間に言語一覧からも消える。
    pub(super) fn plugin_locale_dirs(&self) -> Vec<PathBuf> {
        self.plugins
            .iter()
            .filter(|p| p.active())
            .filter_map(|p| p.language.as_ref().and_then(|l| l.locales.clone()))
            .collect()
    }

    /// 選べる言語の一覧 (同梱 + `~/.zaivern/locales` + プラグイン)。
    pub(super) fn available_locales(&self) -> Vec<locale::Info> {
        locale::available(&self.plugin_locale_dirs())
    }

    /// `cfg.ui_language` を UI へ反映する。**再起動は要らない** —
    /// 次のフレームから全ラベルが新しい言語で描き直される。
    pub(super) fn apply_ui_language(&mut self) {
        let extra = self.plugin_locale_dirs();
        let known: Vec<String> = locale::available(&extra)
            .into_iter()
            .map(|i| i.id)
            .collect();
        let id = locale::resolve(
            &self.cfg.ui_language,
            locale::detected().as_deref(),
            &known,
            locale::SOURCE_LANG,
        );
        for e in i18n::set_locale(&id, &extra) {
            self.toast(trf("⚠ 言語ファイルを読めません — {e}", &[("e", e)]), false);
        }
    }

    /// 言語を選び直して保存する (🌐 ピッカー / コマンドパレットの入口)。
    ///
    /// `id` は `"auto"` か言語 ID。`config.toml` へも書き戻すので次の起動でも残る。
    pub(super) fn set_ui_language(&mut self, id: &str, ctx: &egui::Context) {
        self.cfg.ui_language = id.to_string();

        // 言語プラグインの有効/無効を「選んだ 1 つだけ有効」へ揃える。
        // ここを揃えないと、🌐 で韓国語にしたのにプラグイン画面では
        // english-mode が「有効」のまま、という**画面ごとに違う真実**ができる。
        let mut changed = false;
        for (name, lang) in self.language_plugins() {
            let want = lang == id;
            if self.cfg.plugins.is_enabled(&name) != want {
                self.cfg.plugins.set_enabled(&name, want);
                self.cfg.global_plugins.set_enabled(&name, want);
                changed = true;
            }
        }
        if changed {
            if let Err(e) = config::save_plugins_section(&self.cfg) {
                self.toast(trf("設定の保存に失敗: {e}", &[("e", e)]), false);
            }
        }

        let mut v = std::collections::BTreeMap::new();
        v.insert(
            "ui_language".to_string(),
            config::SettingValue::Text(id.to_string()).to_toml(),
        );
        let saved = config::save_settings(&v);
        // 有効/無効を書き換えたので登録内容を作り直す。ここで
        // `apply_ui_language` まで走るので、次のフレームから全ラベルが変わる。
        self.rebuild_plugins();
        match saved {
            Err(e) => self.toast(trf("設定の保存に失敗: {e}", &[("e", e)]), false),
            Ok(()) => {
                let name = locale::display_name(&i18n::current());
                self.toast(
                    trf("🌐 表示言語を {name} にしました", &[("name", name)]),
                    true,
                );
            }
        }
        crate::perf::repaint(ctx, "set_ui_language");
    }

    pub(super) fn active_selection(&self, ctx: &egui::Context) -> Option<String> {
        let b = self.editor.active.map(|i| &self.editor.buffers[i])?;
        let ed_id = buf_edit_id(self.cur_pane, b.id);
        let r = egui::TextEdit::load_state(ctx, ed_id)?
            .cursor
            .char_range()?;
        let (s, e) = (
            r.primary.index.min(r.secondary.index),
            r.primary.index.max(r.secondary.index),
        );
        if s == e {
            return None;
        }
        Some(b.text.chars().skip(s).take(e - s).collect())
    }

    /// プラグインプロセスへ渡す環境変数一式を組み立てる (仕様 3章)。
    /// ワークスペース・カーソル位置・ブランチ名など、その時点のアプリの状態を集めて
    /// `plugins::command_env` に渡す。設定値 (`ZV_CFG_*`) は向こうで足される。
    pub(super) fn plugin_envs(
        &mut self,
        plugin_name: &str,
        file: Option<&Path>,
        lang: &str,
        selection: &str,
        event: Option<plugins::HookEvent>,
    ) -> Vec<(String, String)> {
        let branch = self.git_branch().unwrap_or_default();
        let agent = self
            .agents
            .sessions
            .get(self.agents.active)
            .map(|s| s.preset_name.clone())
            .unwrap_or_default();
        // マルチルート対応後は「代表ルート」をワークスペースとして渡す。
        let workspace = self.primary_root().to_path_buf();
        let (line, column) = self.editor.cursor;
        let Some(p) = self.plugins.iter().find(|p| p.name == plugin_name) else {
            return Vec::new();
        };
        plugins::command_env(
            p,
            &plugins::EnvContext {
                file,
                lang,
                workspace: &workspace,
                selection,
                line,
                column,
                agent: &agent,
                event,
                git_branch: &branch,
            },
        )
    }

    /// プラグインコマンドを実行する。stdin へ渡す入力(選択範囲/ファイル)を集めて
    /// ワーカースレッドへ投げ、結果は plugin_rx 経由で process_plugin_results が適用する。
    pub(super) fn run_plugin_command(&mut self, pi: usize, ci: usize, ctx: &egui::Context) {
        // 位置は「その場で名前と安定IDへ」変換し、実行はIDで引き直す。
        // こうしておくと再スキャンで番号がずれても別のコマンドを撃たない。
        let Some((plugin_name, cmd_id)) = self
            .plugins
            .get(pi)
            .and_then(|p| p.commands.get(ci).map(|c| (p.name.clone(), c.id.clone())))
        else {
            return;
        };
        self.run_plugin_command_by_id(&plugin_name, &cmd_id, ctx);
    }

    /// プラグイン名 + コマンドの安定IDでコマンドを実行する。
    pub(super) fn run_plugin_command_by_id(
        &mut self,
        plugin: &str,
        cmd_id: &str,
        ctx: &egui::Context,
    ) {
        use plugins::PluginList;
        let Some((plugin, command)) = self.plugins.find_command(plugin, cmd_id) else {
            self.toast(
                trf(
                    "🔌 コマンドが見つかりません: {plugin}/{cmd_id}",
                    &[
                        ("plugin", plugin.to_string()),
                        ("cmd_id", cmd_id.to_string()),
                    ],
                ),
                false,
            );
            return;
        };
        if !plugin.active() {
            let name = plugin.name.clone();
            self.toast(
                trf("🔌 {name} は無効になっています", &[("name", name)]),
                false,
            );
            return;
        }
        let plugin_name = plugin.name.clone();
        let command = command.clone();

        let active = self.editor.active.map(|i| &self.editor.buffers[i]);
        let lang_id = active
            .map(|b| snippets::lang_id_for(&b.lang).to_string())
            .unwrap_or_default();
        if !command.lang_matches(&lang_id) {
            self.toast(
                trf(
                    "「{title}」は {langs} 用のコマンドです",
                    &[
                        ("title", command.title.clone()),
                        ("langs", format!("{:?}", command.langs)),
                    ],
                ),
                false,
            );
            return;
        }

        // 入力の収集 (selection は TextEdit の選択 char 範囲)
        let (stdin_text, buffer_id, replace_range) = match command.input {
            plugins::CmdInput::None => (String::new(), active.map(|b| b.id), None),
            plugins::CmdInput::File => match active {
                Some(b) => (b.text.clone(), Some(b.id), None),
                None => {
                    self.toast(tr("実行にはファイルを開いてください"), false);
                    return;
                }
            },
            plugins::CmdInput::Selection => {
                let Some(b) = active else {
                    self.toast(tr("実行にはファイルを開いてください"), false);
                    return;
                };
                let ed_id = buf_edit_id(self.cur_pane, b.id);
                let range = egui::TextEdit::load_state(ctx, ed_id)
                    .and_then(|st| st.cursor.char_range())
                    .map(|r| (r.primary.index, r.secondary.index))
                    .unwrap_or((0, 0));
                let (s, e) = (range.0.min(range.1), range.0.max(range.1));
                if s == e {
                    self.toast(tr("選択範囲がありません"), false);
                    return;
                }
                let sel: String = b.text.chars().skip(s).take(e - s).collect();
                (sel, Some(b.id), Some((s, e)))
            }
        };

        let file = active.and_then(|b| b.path.clone());
        // ZV_SELECTION は入力モードによらず「いま選択中のテキスト」を渡す
        let selection = match command.input {
            plugins::CmdInput::Selection => stdin_text.clone(),
            _ => self.active_selection(ctx).unwrap_or_default(),
        };
        let envs = self.plugin_envs(&plugin_name, file.as_deref(), &lang_id, &selection, None);
        let title = command.title.clone();
        plugins::run_async(
            plugins::RunRequest {
                plugin: plugin_name,
                command,
                stdin_text,
                envs,
                workdir: self.primary_root().to_path_buf(),
                buffer_id,
                replace_range,
                resave: false,
            },
            self.plugin_tx.clone(),
            ctx.clone(),
        );
        self.toast(trf("🔌 {title} を実行中…", &[("title", title)]), true);
    }

    /// ワーカースレッドから届いた gh の結果を GitHub パネルへ反映する。
    /// (plugin_rx と同じ流儀 — try_recv でだけ受け、UI スレッドは待たない)
    pub(super) fn process_gh_results(&mut self) {
        while let Ok(out) = self.gh_rx.try_recv() {
            match panels::apply_gh_outcome(&mut self.github, out) {
                panels::GhEffect::None => {}
                panels::GhEffect::Toast(msg, ok) => self.toast(msg, ok),
                panels::GhEffect::OpenDiff {
                    number,
                    title,
                    text,
                } => {
                    let id = self.editor.open_virtual(
                        title,
                        text,
                        crate::editor::BufferKind::PrDiff { number },
                    );
                    // 同じタブを使い回すので、古いパース結果は捨てる
                    self.github.drop_diff_cache(id);
                }
            }
        }
    }

    /// GitHub パネルが積んだ gh リクエストを、必ず別スレッドで実行する。
    pub(super) fn dispatch_gh(&mut self, reqs: Vec<github::GhRequest>, ctx: &egui::Context) {
        for req in reqs {
            github::run_async(req, self.gh_tx.clone(), ctx.clone());
        }
    }

    /// ワーカースレッドから届いたプラグインコマンドの結果をエディタへ適用する。
    pub(super) fn process_plugin_results(&mut self, ctx: &egui::Context) {
        while let Ok(r) = self.plugin_rx.try_recv() {
            if !r.ok {
                let msg = r.stderr.trim();
                let msg = if msg.is_empty() {
                    tr("失敗しました (出力なし)")
                } else {
                    msg.to_string()
                };
                self.toast(
                    format!(
                        "🔌 {} ({}): {}",
                        r.title,
                        r.plugin,
                        notify::truncate_chars(&msg, 200)
                    ),
                    false,
                );
                continue;
            }
            match r.sink {
                plugins::CmdSink::Silent => {}
                plugins::CmdSink::Notify => {
                    let msg = if r.stdout.trim().is_empty() {
                        tr("完了しました")
                    } else {
                        notify::truncate_chars(r.stdout.trim(), 200)
                    };
                    self.toast(format!("🔌 {}: {msg}", r.title), true);
                    notify::notify(&format!("Zaivern — {}", r.title), &msg);
                }
                plugins::CmdSink::NewTab => {
                    self.editor.new_untitled();
                    let ed = self.edit_step();
                    if let Some(i) = self.editor.active {
                        let b = &mut self.editor.buffers[i];
                        b.title = r.title.clone();
                        b.apply_edit(r.stdout.clone(), ed);
                    }
                    self.toast(
                        trf("🔌 {title} → 新規タブ", &[("title", r.title.clone())]),
                        true,
                    );
                }
                plugins::CmdSink::Insert => {
                    let Some(i) = self
                        .editor
                        .buffers
                        .iter()
                        .position(|b| Some(b.id) == r.buffer_id)
                    else {
                        self.toast(
                            trf(
                                "🔌 {title}: 反映先のタブが閉じられています",
                                &[("title", r.title.clone())],
                            ),
                            false,
                        );
                        continue;
                    };
                    let ed_id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
                    let cur = egui::TextEdit::load_state(ctx, ed_id)
                        .and_then(|st| st.cursor.char_range())
                        .map(|c| c.primary.index)
                        .unwrap_or_else(|| self.editor.buffers[i].text.chars().count());
                    let ed = self.edit_step();
                    let b = &mut self.editor.buffers[i];
                    let cur = cur.min(b.text.chars().count());
                    let byte = editor_ops::char_to_byte(&b.text, cur);
                    b.text.insert_str(byte, &r.stdout);
                    b.history
                        .record(byte, cur, String::new(), r.stdout.clone(), ed);
                    b.invalidate_render_cache();
                    let end = cur + r.stdout.chars().count();
                    self.pending_select = Some((end, end));
                    self.toast(
                        trf("🔌 {title} を挿入しました", &[("title", r.title.clone())]),
                        true,
                    );
                }
                plugins::CmdSink::Replace => {
                    let Some(i) = self
                        .editor
                        .buffers
                        .iter()
                        .position(|b| Some(b.id) == r.buffer_id)
                    else {
                        self.toast(
                            trf(
                                "🔌 {title}: 反映先のタブが閉じられています",
                                &[("title", r.title.clone())],
                            ),
                            false,
                        );
                        continue;
                    };
                    let ed = self.edit_step();
                    let b = &mut self.editor.buffers[i];
                    match r.replace_range {
                        // 選択範囲の置換: 実行中に編集されていたら黙って上書きしない
                        Some((s, e)) => {
                            let cur_sel: String = b.text.chars().skip(s).take(e - s).collect();
                            if cur_sel != r.original {
                                self.toast(
                                    trf(
                                        "🔌 {title}: 実行中に編集されたため適用を中止しました",
                                        &[("title", r.title.clone())],
                                    ),
                                    false,
                                );
                                continue;
                            }
                            let start = editor_ops::char_to_byte(&b.text, s);
                            let end = editor_ops::char_to_byte(&b.text, e);
                            let removed = b.text[start..end].to_string();
                            b.text.replace_range(start..end, &r.stdout);
                            b.history.record(start, s, removed, r.stdout.clone(), ed);
                            b.invalidate_render_cache();
                            let np = s + r.stdout.chars().count();
                            self.pending_select = Some((np, np));
                            self.toast(
                                trf("🔌 {title} を適用しました", &[("title", r.title.clone())]),
                                true,
                            );
                        }
                        // ファイル全体の置換 (整形など)
                        None => {
                            if b.text != r.original {
                                self.toast(
                                    trf(
                                        "🔌 {title}: 実行中に編集されたため適用を中止しました",
                                        &[("title", r.title.clone())],
                                    ),
                                    false,
                                );
                                continue;
                            }
                            if b.text == r.stdout {
                                if r.resave {
                                    continue; // 保存時フックで変更なし → 静かに終了
                                }
                                self.toast(
                                    trf(
                                        "🔌 {title}: 変更はありません",
                                        &[("title", r.title.clone())],
                                    ),
                                    true,
                                );
                                continue;
                            }
                            b.apply_edit(r.stdout.clone(), ed);
                            // 保存時フック由来なら整形結果をそのままファイルへ書き戻す
                            if r.resave {
                                if let Some(path) = b.path.clone() {
                                    match b.write_to(&path) {
                                        Ok(_) => {
                                            b.mark_saved();
                                            b.disk_mtime = disk_mtime(&path);
                                            b.conflict_notified = None;
                                            self.toast(
                                                trf(
                                                    "🔌 {title} → 整形して保存しました",
                                                    &[("title", r.title.clone())],
                                                ),
                                                true,
                                            );
                                        }
                                        Err(e) => self.toast(
                                            trf(
                                                "🔌 {title}: 再保存に失敗: {e}",
                                                &[("title", r.title.clone()), ("e", e.to_string())],
                                            ),
                                            false,
                                        ),
                                    }
                                }
                            } else {
                                self.toast(
                                    trf("🔌 {title} を適用しました", &[("title", r.title.clone())]),
                                    true,
                                );
                            }
                        }
                    }
                }
                // エージェントの入力欄へ差し込むだけ。送信は必ず人の操作で行う
                plugins::CmdSink::AgentPrompt => {
                    let text = r.stdout.trim_end_matches('\n').to_string();
                    if text.is_empty() {
                        continue;
                    }
                    self.send_agent_prompt(None, &text, false);
                }
                // 指定パネルの本文を差し替える
                plugins::CmdSink::Panel => {
                    let Some(panel) = r.panel.clone() else {
                        self.toast(
                            trf(
                                "🔌 {title}: 出力先パネルが未指定です",
                                &[("title", r.title.clone())],
                            ),
                            false,
                        );
                        continue;
                    };
                    self.set_plugin_panel(&r.plugin, &panel, r.stdout.clone());
                }
                // stdout の JSON Lines をアクションとして実行する
                plugins::CmdSink::Actions => {
                    let plugin = r.plugin.clone();
                    let actions = r.actions.clone();
                    self.run_plugin_actions(&plugin, actions, ctx);
                }
            }
        }
    }

    /// プラグインパネルの本文を書き込む。存在しないパネルなら false。
    pub(super) fn set_plugin_panel(&mut self, plugin: &str, panel: &str, text: String) -> bool {
        let exists = self
            .plugins
            .iter()
            .any(|p| p.active() && p.name == plugin && p.panels.iter().any(|x| x.id == panel));
        if !exists {
            self.toast(
                trf(
                    "🔌 パネルが見つかりません: {plugin}/{panel}",
                    &[("plugin", plugin.to_string()), ("panel", panel.to_string())],
                ),
                false,
            );
            return false;
        }
        self.plugin_panels
            .insert((plugin.to_string(), panel.to_string()), text);
        self.panel_last_run
            .insert((plugin.to_string(), panel.to_string()), Instant::now());
        true
    }

    /// エージェントの入力欄へテキストを差し込む。`submit` が true のときだけ Enter を送る。
    /// `agent` が None ならアクティブなセッション、Some なら名前 (プリセット名/タイトル) で探す。
    pub(super) fn send_agent_prompt(
        &mut self,
        agent: Option<&str>,
        text: &str,
        submit: bool,
    ) -> bool {
        // スラッシュコマンド（/goal, /loop, /help 等）の高速パース・プロンプト展開
        let parsed_cmd = crate::agent_input::SlashCommandEngine::parse(text);
        let expanded_text = crate::agent_input::SlashCommandEngine::expand_command(&parsed_cmd);

        let payload = if submit {
            format!("{expanded_text}\r")
        } else {
            expanded_text.clone()
        };
        let idx = match agent.map(str::trim).filter(|a| !a.is_empty()) {
            Some(name) => self
                .agents
                .sessions
                .iter()
                .position(|s| s.running() && (s.preset_name == name || s.title == name)),
            None => self
                .agents
                .sessions
                .get(self.agents.active)
                .filter(|s| s.running())
                .map(|_| self.agents.active),
        };
        let Some(i) = idx else {
            self.toast(tr("エージェントセッションが見つかりません"), false);
            return false;
        };
        let title = {
            let s = &mut self.agents.sessions[i];
            // 明示的な送り込みはユーザーの応答扱い (承認エピソードを解決する)
            s.note_user_input();
            s.write_bytes(payload.as_bytes());
            s.title.clone()
        };
        self.agents.panel_open = true;
        let verb = if submit {
            tr("送信")
        } else {
            tr("入力欄へ")
        };
        self.toast(
            format!(
                "🔌 {title} {verb}: {}",
                notify::truncate_chars(&expanded_text, 60)
            ),
            true,
        );
        true
    }

    /// プラグインが返したアクション (仕様 2章) を順に実行する。
    pub(super) fn run_plugin_actions(
        &mut self,
        plugin: &str,
        actions: Vec<plugins::PluginAction>,
        ctx: &egui::Context,
    ) {
        use plugins::PluginAction as A;
        for a in actions {
            match a {
                A::OpenFile { path, line } => {
                    let p = if Path::new(&path).is_absolute() {
                        PathBuf::from(&path)
                    } else {
                        self.primary_root().join(&path)
                    };
                    self.open_path(&p);
                    if let Some(n) = line {
                        self.goto_line(n as usize);
                    }
                }
                A::Notify { message, level } => {
                    let msg = notify::truncate_chars(message.trim(), 200);
                    match level {
                        plugins::NotifyLevel::Info => self.toast(format!("🔌 {msg}"), true),
                        plugins::NotifyLevel::Warn => self.toast_warn(format!("🔌 {msg}")),
                        plugins::NotifyLevel::Error => self.toast(format!("🔌 {msg}"), false),
                    }
                    if !matches!(level, plugins::NotifyLevel::Info) {
                        notify::notify("Zaivern Code", &msg);
                    }
                }
                A::InsertText { text } => self.insert_at_cursor(&text, ctx),
                A::ReplaceBuffer { text } => match self.editor.active {
                    Some(i) => {
                        let ed = self.edit_step();
                        self.editor.buffers[i].apply_edit(text, ed);
                        self.toast(tr("🔌 バッファを置き換えました"), true);
                    }
                    None => self.toast(tr("🔌 置き換え先のタブがありません"), false),
                },
                A::NewTab { title, text } => {
                    self.editor.new_untitled();
                    let ed = self.edit_step();
                    if let Some(i) = self.editor.active {
                        let b = &mut self.editor.buffers[i];
                        b.title = title;
                        b.apply_edit(text, ed);
                    }
                }
                A::AgentPrompt {
                    agent,
                    text,
                    submit,
                } => {
                    self.send_agent_prompt(agent.as_deref(), &text, submit);
                }
                A::RunTerminal { command, cwd } => {
                    self.run_in_terminal(&command, cwd.as_deref(), ctx)
                }
                A::OpenUrl { url } => {
                    open_external(&url);
                    self.toast(trf("🔗 {url} を開きました", &[("url", url)]), true);
                }
                A::SetPanel { panel, text } => {
                    self.set_plugin_panel(plugin, &panel, text);
                }
                A::SetStatus { text } => self.plugin_status = text,
                A::RefreshFiles => {
                    self.tree.invalidate();
                    self.rebuild_index();
                }
                A::SetSetting { key, value } => {
                    self.cfg.plugins.set_setting(plugin, &key, &value);
                    self.cfg.global_plugins.set_setting(plugin, &key, &value);
                    if let Err(e) = config::save_plugins_section(&self.cfg) {
                        self.toast(trf("設定の保存に失敗: {e}", &[("e", e.to_string())]), false);
                    }
                    // 実行中のプラグインへも即座に反映する
                    if let Some(p) = self.plugins.iter_mut().find(|p| p.name == plugin) {
                        if let Some(vals) = self.cfg.plugins.settings.get(plugin) {
                            p.apply_settings(vals);
                        }
                    }
                }
            }
        }
    }

    /// 1 始まりの行番号へカーソルを移動し、その行が見えるようスクロールする。
    pub(super) fn goto_line(&mut self, line: usize) {
        let Some(i) = self.editor.active else {
            return;
        };
        let line = line.max(1);
        let text = &self.editor.buffers[i].text;
        let char_pos = text
            .split_inclusive('\n')
            .take(line - 1)
            .map(|l| l.chars().count())
            .sum::<usize>()
            .min(text.chars().count());
        self.pending_select = Some((char_pos, char_pos));
        self.pending_scroll =
            Some(((line - 1) as f32 * self.last_row_h - self.last_view_h * 0.4).max(0.0));
    }

    /// カーソル位置へテキストを差し込む。
    pub(super) fn insert_at_cursor(&mut self, text: &str, ctx: &egui::Context) {
        let Some(i) = self.editor.active else {
            self.toast(tr("🔌 挿入先のタブがありません"), false);
            return;
        };
        let ed_id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
        let cur = egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|c| c.primary.index)
            .unwrap_or_else(|| self.editor.buffers[i].text.chars().count());
        let ed = self.edit_step();
        let b = &mut self.editor.buffers[i];
        let cur = cur.min(b.text.chars().count());
        let byte = editor_ops::char_to_byte(&b.text, cur);
        b.text.insert_str(byte, text);
        b.history
            .record(byte, cur, String::new(), text.to_string(), ed);
        b.invalidate_render_cache();
        let end = cur + text.chars().count();
        self.pending_select = Some((end, end));
    }

    /// フックの起動を予約する。実際の起動は update から `fire_hooks` で行う
    /// (egui の Context が要るため)。
    pub(super) fn queue_hook(&mut self, event: plugins::HookEvent, file: Option<PathBuf>) {
        self.pending_hooks.push((event, file));
    }

    /// 指定イベントのフックを一斉に起動する (仕様 1章 `[[hook]]`)。
    /// 実行は既存の非同期機構に載せるので UI スレッドは止まらない。
    pub(super) fn fire_hooks(
        &mut self,
        event: plugins::HookEvent,
        file: Option<PathBuf>,
        ctx: &egui::Context,
    ) {
        use plugins::PluginList;
        let targets: Vec<(String, plugins::PluginCommand)> = self
            .plugins
            .active_hooks(event)
            .into_iter()
            .map(|(p, h)| (p.name.clone(), h.as_command(&p.name)))
            .collect();
        if targets.is_empty() {
            return;
        }
        let lang = file
            .as_deref()
            .and_then(|p| p.extension())
            .map(|e| snippets::lang_id_for(&e.to_string_lossy()).to_string())
            .unwrap_or_default();
        for (plugin_name, command) in targets {
            let envs = self.plugin_envs(&plugin_name, file.as_deref(), &lang, "", Some(event));
            plugins::run_async(
                plugins::RunRequest {
                    plugin: plugin_name,
                    command,
                    stdin_text: String::new(),
                    envs,
                    workdir: self.primary_root().to_path_buf(),
                    buffer_id: None,
                    replace_range: None,
                    resave: false,
                },
                self.plugin_tx.clone(),
                ctx.clone(),
            );
        }
    }

    /// 時間で動くもの — interval フックと interval 更新のパネル — を回す。
    /// 毎フレーム呼ばれるので、まだ間隔に達していないものは何もしない。
    pub(super) fn tick_plugin_timers(&mut self, ctx: &egui::Context) {
        use plugins::PluginList;

        // interval フック: プラグイン毎に前回実行からの経過を見る
        let due: Vec<(String, plugins::PluginCommand)> = self
            .plugins
            .active_hooks(plugins::HookEvent::Interval)
            .into_iter()
            .filter(|(p, h)| {
                let key = (p.name.clone(), h.event.as_str().to_string());
                match self.hook_last_run.get(&key) {
                    Some(at) => at.elapsed().as_secs() >= h.interval_secs.max(5),
                    None => true,
                }
            })
            .map(|(p, h)| (p.name.clone(), h.as_command(&p.name)))
            .collect();
        for (plugin_name, command) in due {
            self.hook_last_run.insert(
                (
                    plugin_name.clone(),
                    plugins::HookEvent::Interval.as_str().to_string(),
                ),
                Instant::now(),
            );
            let envs = self.plugin_envs(
                &plugin_name,
                None,
                "",
                "",
                Some(plugins::HookEvent::Interval),
            );
            plugins::run_async(
                plugins::RunRequest {
                    plugin: plugin_name,
                    command,
                    stdin_text: String::new(),
                    envs,
                    workdir: self.primary_root().to_path_buf(),
                    buffer_id: None,
                    replace_range: None,
                    resave: false,
                },
                self.plugin_tx.clone(),
                ctx.clone(),
            );
        }

        // パネルはプラグインタブが見えているときだけ更新する
        let tab_open = self.sidebar_open && self.sidebar_tab == SidebarTab::Plugins;
        let just_opened = tab_open && !self.plugins_tab_was_open;
        self.plugins_tab_was_open = tab_open;
        if !tab_open {
            return;
        }
        // refresh = on_open のパネルは、タブを開いた瞬間に取り直す
        if just_opened {
            let on_open: Vec<(String, String)> = self
                .plugins
                .active_panels()
                .into_iter()
                .filter(|(_, pa)| {
                    pa.refresh == plugins::PanelRefresh::OnOpen && !pa.run.trim().is_empty()
                })
                .map(|(p, pa)| (p.name.clone(), pa.id.clone()))
                .collect();
            for (plugin_name, panel_id) in on_open {
                self.refresh_panel(&plugin_name, &panel_id, ctx);
            }
        }
        let panels: Vec<(String, String)> = self
            .plugins
            .active_panels()
            .into_iter()
            .filter(|(_, pa)| {
                pa.refresh == plugins::PanelRefresh::Interval && !pa.run.trim().is_empty()
            })
            .filter(|(p, pa)| {
                let key = (p.name.clone(), pa.id.clone());
                match self.panel_last_run.get(&key) {
                    Some(at) => at.elapsed().as_secs() >= pa.interval_secs.max(5),
                    None => true,
                }
            })
            .map(|(p, pa)| (p.name.clone(), pa.id.clone()))
            .collect();
        for (plugin_name, panel_id) in panels {
            self.refresh_panel(&plugin_name, &panel_id, ctx);
        }
    }

    /// パネルの `run` を実行して本文を取り直す。`run` が空のパネルは
    /// アクション (`set_panel`) 経由でしか更新されないので何もしない。
    pub(super) fn refresh_panel(&mut self, plugin_name: &str, panel_id: &str, ctx: &egui::Context) {
        use plugins::PluginList;
        let Some((_, panel)) = self.plugins.find_panel(plugin_name, panel_id) else {
            return;
        };
        if panel.run.trim().is_empty() {
            return;
        }
        let command = panel.as_command();
        self.panel_last_run.insert(
            (plugin_name.to_string(), panel_id.to_string()),
            Instant::now(),
        );
        let envs = self.plugin_envs(plugin_name, None, "", "", None);
        plugins::run_async(
            plugins::RunRequest {
                plugin: plugin_name.to_string(),
                command,
                stdin_text: String::new(),
                envs,
                workdir: self.primary_root().to_path_buf(),
                buffer_id: None,
                replace_range: None,
                resave: false,
            },
            self.plugin_tx.clone(),
            ctx.clone(),
        );
    }

    /// ターミナル (エージェントパネル) で任意のコマンドを走らせる。
    pub(super) fn run_in_terminal(
        &mut self,
        command: &str,
        cwd: Option<&str>,
        ctx: &egui::Context,
    ) {
        let preset = config::AgentPreset {
            name: notify::truncate_chars(command, 24),
            icon: "🔌".to_string(),
            command: command.to_string(),
            cwd: cwd.map(|s| s.to_string()),
            env: HashMap::new(),
        };
        // self.agents を可変で借りる前に作業フォルダを取り出しておく
        let root = self.agent_cwd();
        match self
            .agents
            .launch(&preset, &root, crate::agents::Approval::Agent, ctx)
        {
            Ok(()) => self.toast(
                trf(
                    "▶ {command} を実行しています",
                    &[("command", command.to_string())],
                ),
                true,
            ),
            Err(e) => self.toast(trf("実行に失敗: {e}", &[("e", e.to_string())]), false),
        }
    }

    /// 「➕ 新規プラグイン」の名前入力ダイアログ。
    /// 作成後は plugin.toml をエディタで開き、すぐ編集を始められるようにする。
    pub(super) fn new_plugin_ui(&mut self, ctx: &egui::Context) {
        let Some(mut name) = self.new_plugin_name.clone() else {
            return;
        };
        let theme = self.theme.clone();
        let mut open = true;
        let mut create = false;
        let mut cancel = false;
        egui::Window::new(tr("➕ 新規プラグイン"))
            .collapsible(false)
            .resizable(false)
            .open(&mut open)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, -80.0))
            .show(ctx, |ui| {
                ui.label(tr("プラグイン名 (小文字英数と - _ のみ):"));
                let re = ui.text_edit_singleline(&mut name);
                if re.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    create = true;
                }
                let ok = plugins::valid_name(&name.trim().to_lowercase());
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(ok, egui::Button::new(tr("作成"))).clicked() {
                        create = true;
                    }
                    if ui.button(tr("キャンセル")).clicked() {
                        cancel = true;
                    }
                    if !name.trim().is_empty() && !ok {
                        ui.label(RichText::new(tr("名前が不正です")).color(theme.warn));
                    }
                });
                ui.label(
                    RichText::new(tr(
                        "~/.zaivern/plugins/<名前>/ にコマンド・テーマ・スニペットの\nテンプレート一式を生成し、plugin.toml を開きます",
                    ))
                    .size(10.5)
                    .color(theme.text_dim),
                );
            });
        if create && plugins::valid_name(&name.trim().to_lowercase()) {
            match plugins::create_template(name.trim()) {
                Ok(dir) => {
                    self.rebuild_plugins();
                    self.open_path(&dir.join("plugin.toml"));
                    self.toast(
                        trf(
                            "➕ 作成しました: {dir}",
                            &[("dir", dir.display().to_string())],
                        ),
                        true,
                    );
                    self.new_plugin_name = None;
                }
                Err(e) => {
                    self.toast(trf("作成失敗: {e}", &[("e", e.to_string())]), false);
                    self.new_plugin_name = Some(name);
                }
            }
        } else if cancel || !open {
            self.new_plugin_name = None;
        } else {
            self.new_plugin_name = Some(name);
        }
    }
}
