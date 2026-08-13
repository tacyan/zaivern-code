use super::*;

impl ZaivernApp {
    /// 「エージェントを追加」ピッカーを描き、選ばれたものを cfg.agents へ足す。
    ///
    /// ピッカー側は操作を返すだけにして、設定への反映はここで行う
    /// (パネルのクロージャの中から self を可変で触らないための定石)。
    pub(super) fn agent_picker_ui(&mut self, ctx: &egui::Context) {
        // 検出結果の取り込みはウィンドウの開閉に関係なく毎フレーム行う。
        self.agent_picker.poll();
        let theme = self.theme.clone();
        let action = agent_picker::ui(&mut self.agent_picker, ctx, &theme, &self.cfg.agents);
        match action {
            Some(agent_picker::PickerAction::Reprobe) => {
                self.agent_picker.probe(ctx);
                self.toast(tr("⟳ PATH を調べています…"), true);
            }
            Some(agent_picker::PickerAction::Add { preset, spec }) => {
                let installed = self.agent_picker.is_installed(spec.bin);
                let name = preset.name.clone();
                match config::append_agent_preset(&preset) {
                    Ok(()) => {
                        // 先に config.toml へ書けたものだけをメモリへ足す。
                        // (書けていないのに一覧へ出すと、再起動で消えて混乱する)
                        self.cfg.agents.push(preset);
                        if installed {
                            self.toast(trf("➕ {name} を追加しました", &[("name", name)]), true);
                        } else {
                            // 起動しても必ず失敗するので、黙って足したように見せない。
                            self.toast_warn(trf(
                                "➕ {name} を追加しましたが、{bin} は未インストールです → {install}",
                                &[
                                    ("name", name),
                                    ("bin", spec.bin.to_string()),
                                    ("install", spec.install.to_string()),
                                ],
                            ));
                        }
                    }
                    Err(e) => self.toast(
                        trf("設定に書けませんでした: {e}", &[("e", e.to_string())]),
                        false,
                    ),
                }
            }
            None => {}
        }
    }

    /// 保存直後に on_save フック (整形など) を持つプラグインコマンドを起動する。
    pub(super) fn run_on_save_hooks(&mut self, buf_index: usize, ctx: &egui::Context) {
        let b = &self.editor.buffers[buf_index];
        let lang_id = snippets::lang_id_for(&b.lang).to_string();
        let Some(path) = b.path.clone() else {
            return;
        };
        let (text, buffer_id) = (b.text.clone(), b.id);
        // file_save フックも同じ契機で走らせる (整形の on_save とは別系統)
        self.fire_hooks(plugins::HookEvent::FileSave, Some(path.clone()), ctx);
        let mut launched: Vec<(String, plugins::PluginCommand)> = Vec::new();
        for p in self.plugins.iter().filter(|p| p.active()) {
            for c in &p.commands {
                if c.on_save && c.lang_matches(&lang_id) {
                    launched.push((p.name.clone(), c.clone()));
                }
            }
        }
        for (plugin_name, command) in launched {
            let envs = self.plugin_envs(&plugin_name, Some(&path), &lang_id, "", None);
            plugins::run_async(
                plugins::RunRequest {
                    plugin: plugin_name,
                    command,
                    stdin_text: text.clone(),
                    envs,
                    workdir: self.primary_root().to_path_buf(),
                    buffer_id: Some(buffer_id),
                    replace_range: None,
                    resave: true,
                },
                self.plugin_tx.clone(),
                ctx.clone(),
            );
        }
    }

    // ─── LSP (言語サーバー) ─────────────────────────────────────────

    /// バッファを開いた/表示したときに、その言語のサーバーを必要なら起動し did_open する。
    ///
    /// `buf_idx` は did_open に送る本文を持つバッファの添字。本文は初回の did_open で
    /// しか使わないので、呼び出し側で毎フレーム clone せず、必要になった所で借りる。
    pub(super) fn ensure_lsp(
        &mut self,
        ctx: &egui::Context,
        path: &Path,
        lang: &str,
        buf_idx: usize,
    ) {
        let lang_id = snippets::lang_id_for(lang).to_string();
        let Some(server_cmd) = lsp_server_for(&lang_id) else {
            return;
        };
        // マルチルート: そのファイルが属するルート毎にサーバーを起動する。
        // ルート外のファイルは primary ルートのサーバーに預ける。
        let root = self
            .root_for(path)
            .unwrap_or_else(|| self.primary_root())
            .to_path_buf();
        let key: LspKey = (lang_id.clone(), root.clone());
        if !self.lsp.contains_key(&key) {
            let bin = server_cmd.split_whitespace().next().unwrap_or("");
            // ここは毎フレーム通るので、「見つからなかった」結果を短時間だけ覚えて
            // PATH の走査を繰り返さないようにする。
            let now = Instant::now();
            if which_result_is_fresh(
                self.lsp_which_missing.get(bin).copied(),
                now,
                WHICH_MISS_TTL,
            ) {
                return; // 直近で確認済み。未インストールのまま
            }
            if !which(bin) {
                self.lsp_which_missing.insert(bin.to_string(), now);
                return; // サーバー未インストールなら静かに無効
            }
            self.lsp_which_missing.remove(bin);
            match lsp::LspClient::spawn(server_cmd, &root, ctx.clone()) {
                Ok(client) => {
                    self.lsp.insert(key.clone(), client);
                }
                Err(_) => return,
            }
        }
        if !self.lsp_opened.contains(path) {
            if let Some(client) = self.lsp.get(&key) {
                // initialize 完了前に通知を送ってはならない (LSP 仕様)。
                // ここは毎フレーム通るので、ready になった次のフレームで did_open される
                if !client.is_ready() {
                    return;
                }
                // 本文はこの一回だけ必要。self.lsp / self.editor はどちらも不変借用なので両立する
                let text = self
                    .editor
                    .buffers
                    .get(buf_idx)
                    .map(|b| b.text.as_str())
                    .unwrap_or("");
                client.did_open(path, &lang_id, text);
            }
            // クライアントの有無に関わらず登録するのは元の insert と同じ挙動
            self.lsp_opened.insert(path.to_path_buf());
        }
    }

    /// `path` / 言語から LSP サーバーのキーを作る (起動はしない)。
    pub(super) fn lsp_key_for(&self, path: &Path, lang: &str) -> LspKey {
        let root = self
            .root_for(path)
            .unwrap_or_else(|| self.primary_root())
            .to_path_buf();
        (snippets::lang_id_for(lang).to_string(), root)
    }

    /// デバウンスした did_change を実際に送る(update から毎フレーム呼ぶ)。
    pub(super) fn flush_lsp_changes(&mut self) {
        if self.lsp_pending.is_empty() {
            return;
        }
        let ready: Vec<PathBuf> = self
            .lsp_pending
            .iter()
            .filter(|(_, (_, at, _))| (at.elapsed().as_millis() as u64) >= LSP_DEBOUNCE_MS)
            .map(|(p, _)| p.clone())
            .collect();
        for p in ready {
            // did_open 前 (initialize 未完了を含む) のドキュメントには送らない。
            // pending に残しておき、ensure_lsp が did_open した後のフレームで送る
            if !self.lsp_opened.contains(&p) {
                continue;
            }
            if let Some((text, _, key)) = self.lsp_pending.remove(&p) {
                if let Some(client) = self.lsp.get(&key) {
                    client.did_change(&p, &text);
                }
            }
        }
    }

    /// アクティブバッファの診断を `self.diag_cache` へ反映する。
    ///
    /// キャッシュは **範囲付き** で持つ (行→severity だけだと本文に波線を
    /// 引けない)。組み直しの判定と中身は [`diagview::DiagCache`] 側にあり、
    /// 診断も本文も変わっていないフレームでは何も確保しない。
    pub(super) fn refresh_active_diagnostics(&mut self, text_hash: u64) {
        let diags = (|| {
            let i = self.editor.active?;
            let path = self.editor.buffers[i].path.as_ref()?;
            let key = self.lsp_key_for(path, &self.editor.buffers[i].lang);
            self.lsp.get(&key)?.diagnostics(path)
        })()
        .unwrap_or_default();
        let text = self
            .editor
            .active
            .map(|i| self.editor.buffers[i].text.as_str())
            .unwrap_or("");
        self.diag_cache.refresh(diags, text, text_hash);
    }

    /// アクティブバッファのインレイヒントを要求し、`self.inlay_cache` へ写す。
    ///
    /// * 要求は **版が変わって、まだ同じ版の要求が飛んでいないとき**だけ 1 回。
    ///   毎フレームは撃たない (設計原則 3)。
    /// * 応答は受信スレッドが LSP 側のキャッシュへ入れて再描画を起こすので、
    ///   ここは覗くだけ。**UI スレッドは一切待たない**。
    /// * 応答待ちの間は前の版のヒントを消す — 打鍵でずれた型注釈を出し続けない。
    pub(super) fn refresh_inlay_hints(&mut self, text_hash: u64) {
        if !self.cfg.inlay_hints {
            self.inlay_cache.clear();
            return;
        }
        let (Some((key, path)), Some(i)) = (self.active_lsp_target(), self.editor.active) else {
            self.inlay_cache.clear();
            return;
        };
        let Some(client) = self.lsp.get(&key) else {
            self.inlay_cache.clear();
            return;
        };
        let Some(hints) = client.inlay_hints(&path) else {
            if !client.inlay_in_flight(&path) {
                client.request_inlay_hints(&path, &self.editor.buffers[i].text);
            }
            self.inlay_cache.clear();
            return;
        };
        // 借用は分離したフィールド同士 (inlay_cache / editor) なので複製は要らない
        self.inlay_cache
            .refresh(&hints, &self.editor.buffers[i].text, text_hash);
    }

    /// 次 / 前の診断へ飛ぶ (VS Code の F8 / ⇧F8)。端では巻き戻る。
    pub(super) fn goto_diagnostic(&mut self, ctx: &egui::Context, forward: bool) {
        let Some(i) = self.editor.active else { return };
        let Some(path) = self.editor.buffers[i].path.clone() else {
            self.toast(tr("このタブには診断がありません"), false);
            return;
        };
        let diags = self.diag_cache.items.clone();
        if diags.is_empty() {
            self.toast(tr("診断はありません"), false);
            return;
        }
        let cur_char = self.active_cursor_char(ctx);
        let (line, col) = lsp::char_index_to_lsp_pos(&self.editor.buffers[i].text, cur_char);
        let Some(n) = diagview::step_diag(&diags, lsp::Position::new(line, col), forward) else {
            return;
        };
        let d = diags[n].clone();
        self.jump_to_lsp_pos(&path, d.line, d.col);
        // 飛んだ先が何なのかを 1 行で見せる (行末表示を切っていても分かるように)。
        // トーストの種別は severity に合わせる (error だけ赤くする)。
        let kind = match d.severity {
            1 => 2u8,
            2 => 1,
            _ => 0,
        };
        self.push_toast(diagview::labeled_message(&d), kind);
    }

    /// 保存済みのエディタ分割レイアウトを復元する。
    ///
    /// リーフはパスで指しているので、**開き直せなかったファイルは黙って落ちる**。
    /// 残るペインが 1 枚以下なら分割を復元しない (空ペインを残さない)。
    /// 壊れた記録・未知のバージョンでも `EditorPanesRec` 側が空を返すので、
    /// ここは常に「復元できたか / できなかったか」だけを見る。
    pub(super) fn restore_editor_split(&mut self, line: &str) {
        if line.is_empty() {
            return;
        }
        let rec = editor_split::EditorPanesRec::from_line(line);
        let restored = rec.to_panes(&mut |p| {
            let path = Path::new(p);
            self.editor
                .buffers
                .iter()
                .find(|b| b.path.as_deref() == Some(path))
                .map(|b| b.id)
        });
        let Some(panes) = restored else { return };
        self.panes = panes;
        self.cur_pane = self.panes.focus_id();
        // `sync_panes` がフォーカス中ペインへ `editor.active` を押し込まないよう、
        // 先にこちらから合わせておく (合っていないとタブが 1 枚増える)。
        if let Some(b) = self.panes.active_buf() {
            self.editor.active = self.editor.buffers.iter().position(|x| x.id == b);
        }
    }

    /// 保存済みのピン留めを戻す。
    ///
    /// 記録はファイルの**絶対パス**なので、開き直せなかったファイルは黙って
    /// 落ちる (バッファ ID は再起動で必ず変わる)。同じファイルを複数ペインで
    /// 開いていたら、その全部でピン留めし直す。
    pub(super) fn restore_pinned_tabs(&mut self, files: &[String]) {
        if files.is_empty() {
            return;
        }
        for f in files {
            let path = Path::new(f);
            let Some(buf) = self
                .editor
                .buffers
                .iter()
                .find(|b| b.path.as_deref() == Some(path))
                .map(|b| b.id)
            else {
                continue;
            };
            for id in self.panes.order() {
                self.panes.set_pinned(id, buf, true);
            }
        }
        // ピン留めを左端へ寄せた並びを `editor.buffers` へ写し戻す。
        self.sync_panes();
    }

    /// 現在のタブ構成などをワークスペース単位で保存する。
    pub(super) fn persist_session(&self) {
        let data = session::SessionData {
            open_files: self
                .editor
                .buffers
                .iter()
                .filter_map(|b| b.path.as_ref().map(|p| p.to_string_lossy().to_string()))
                .collect(),
            active: self.editor.active,
            sidebar_open: self.sidebar_open,
            panel_open: self.agents.panel_open,
            sidebar_tab: self.sidebar_tab.as_key().to_string(),
            // ルート一覧そのものも保存する (再起動で全フォルダを復元するため)
            roots: self
                .roots
                .iter()
                .map(|p| p.to_string_lossy().to_string())
                .collect(),
            // エディタ分割。リーフはバッファ ID ではなく**絶対パス**で指す
            // (ID は再起動で必ず変わる)。分割していなければ空文字。
            editor_split: self
                .panes
                .to_rec(&mut |b| {
                    self.editor
                        .buffers
                        .iter()
                        .find(|x| x.id == b)
                        .and_then(|x| x.path.as_ref())
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .to_line(),
            // ピン留めは分割の有無に関わらず残す (分割していないと
            // `editor_split` は空になるので、こちらが唯一の記録になる)。
            pinned_files: self
                .panes
                .pinned_bufs()
                .into_iter()
                .filter_map(|b| {
                    self.editor
                        .buffers
                        .iter()
                        .find(|x| x.id == b)
                        .and_then(|x| x.path.as_ref())
                        .map(|p| p.to_string_lossy().into_owned())
                })
                .collect(),
            // 変更レビューの「レビュー済み」の印。**印が消えるとレビューは
            // 有限でなくなる** (毎回ゼロから読み直しになる) ので必ず往復させる。
            reviewed_files: self.review.reviewed_paths(),
            // 走らせているエージェントタブの記録 (チャット履歴のフォルダ別保存)。
            // 終了済みは復元しても意味が無いので残さない。
            agents: self
                .agents
                .sessions
                .iter()
                .filter(|s| !s.exited.load(std::sync::atomic::Ordering::SeqCst))
                .map(|s| session::AgentSessionRec {
                    preset_name: s.preset_name.clone(),
                    title: s.title.clone(),
                    icon: s.icon.clone(),
                    command: s.command.clone(),
                    cwd: s.cwd.to_string_lossy().into_owned(),
                    log_file: s
                        .log_path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    // 分割しているタイルだけが中身を持つ (他は空文字)。
                    split: self.split_line_for(s.id),
                    // worktree 隔離で起動した 1 体だけが中身を持つ (他は空文字)。
                    // これが往復しないと、再起動で自分の worktree に戻れない。
                    worktree_repo: self
                        .agent_worktrees
                        .get(&s.id)
                        .map(|w| w.repo.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    worktree_branch: self
                        .agent_worktrees
                        .get(&s.id)
                        .map(|w| w.branch.clone())
                        .unwrap_or_default(),
                })
                .collect(),
        };
        session::save(&self.roots, &data);
        // マルチルート時は primary ルート単独のキーでも保存しておく。
        // これで `zai <primaryフォルダ>` だけで起動しても、
        // 保存されている roots からワークスペース全体を復元できる。
        if self.roots.len() > 1 {
            session::save(&self.roots[..1], &data);
        }
    }

    /// 保存済みセッション(開いていたタブ等)を復元する。
    /// セッションに記録されたルート一覧の方が広ければ、フォルダ構成ごと復元する。
    pub(super) fn restore_session(&mut self, ctx: &egui::Context) {
        self.restore_session_data(ctx);
        // Hot Exit: 未保存だった本文を戻す。
        //
        // **セッションファイルが無くても必ず走らせる** — 退避は数秒ごと、
        // セッションはタブ構成が変わったときに書くので、「退避だけが
        // 残っている」落ち方が現実に起こる。タブを開き直した**後**に
        // 呼ぶこと (同じファイルのタブが二重にならない)。
        self.hotexit_restore();
    }

    /// 保存済みセッション本体 (タブ・分割・エージェント) の復元。
    /// セッションファイルが無ければ何もしない。
    pub(super) fn restore_session_data(&mut self, ctx: &egui::Context) {
        // ついでに古いターミナルログを掃除する (新しい 40 本だけ残す)
        session::prune_term_logs(self.primary_root(), 40);
        // エージェント別の会話履歴も同じ方針で頭打ちにする。
        // 起動のたびに 1 行積むので、放っておくと単調増加する
        // (一覧に出るのは `session_picker::MAX_RESULTS` 件までなので、
        //  それより十分多い分だけ残せばよい)。
        for spec in crate::agents::AGENT_CATALOG {
            let _ = crate::history::prune(spec.bin, self.primary_root(), HISTORY_KEEP);
        }
        let Some(sess) = session::load(&self.roots) else {
            return;
        };
        // 記録されていたルート一覧が現在のルートをすべて含み、かつより広いなら
        // マルチルートワークスペースとして開き直す。
        let saved =
            file_tree::normalize_roots(sess.roots.iter().map(PathBuf::from).collect::<Vec<_>>());
        if let Some(wider) = restored_roots(&self.roots, saved) {
            self.apply_roots(wider, ctx);
        }
        let base = self.editor.buffers.len();
        for f in &sess.open_files {
            let _ = self.editor.open(Path::new(f), self.highlighter);
        }
        if let Some(a) = sess.active {
            let idx = base + a;
            if idx < self.editor.buffers.len() {
                self.editor.active = Some(idx);
            }
        }
        self.restore_editor_split(&sess.editor_split);
        self.restore_pinned_tabs(&sess.pinned_files);
        self.sidebar_open = sess.sidebar_open;
        self.sidebar_tab = SidebarTab::from_key(&sess.sidebar_tab);
        // レビュー済みの印はセッションを跨いで残す (VS Code の Mark as viewed)。
        self.review.set_reviewed_paths(&sess.reviewed_files);
        self.agents.panel_open = sess.panel_open;
        // 前回走らせていたエージェントタブの復元 (チャット履歴の再開)。
        // restore_agents = false なら何もしない (タブも作らない)。
        if self.cfg.restore_agents && !sess.agents.is_empty() {
            self.restore_agent_sessions(&sess.agents, ctx);
        }
        // Hot Exit: 未保存だった本文を戻す。タブを開き直した**後**に
        // 走らせること (同じファイルのタブが二重にならない)。
        self.hotexit_restore();
    }

    /// 保存済みエージェント記録からターミナルタブを復元する。
    ///
    /// 1. 前回の生ログ末尾を新しい vt100 パーサへ再生し、旧スクロールバックを
    ///    見える状態にする (区切りバナー付き — terminal::RESTORE_BANNER)
    /// 2. 実際にログが残っている場合だけ、対応 CLI (claude / codex) へ
    ///    再開指定を足して会話を継続する
    /// 3. ログは前回と同じファイルへ追記する (再起動をまたいで 1 本の履歴になる)
    pub(super) fn restore_agent_sessions(
        &mut self,
        recs: &[session::AgentSessionRec],
        ctx: &egui::Context,
    ) {
        use crate::agents::{apply_resume, merged_env, spec_for_command, Approval};
        let approval = Approval::from_mode(&self.cfg.approval_mode);
        let mut restored = 0usize;
        for rec in recs {
            // 素のシェルには再開する会話が無いので復元しない
            if rec.command.trim().is_empty() {
                continue;
            }
            let log_path = (!rec.log_file.is_empty()).then(|| PathBuf::from(&rec.log_file));
            // 既に同じログへ書いている生きたタブがあれば二重復元しない
            // (エージェントを残したまま同じフォルダを開き直した場合など)
            if let Some(lp) = &log_path {
                if self
                    .agents
                    .sessions
                    .iter()
                    .any(|s| s.log_path.as_deref() == Some(lp.as_path()))
                {
                    continue;
                }
            }
            // ログが実在するときだけ再開指定を足す — ログが消えたフォルダで
            // `--continue` しても再開する会話が無く、CLI 側の挙動が不定になる
            let replay = log_path
                .as_ref()
                .map(|p| session::read_term_log_tail(p, session::REPLAY_TAIL_CAP))
                .unwrap_or_default();
            let command = match spec_for_command(&rec.command) {
                Some(spec) if !replay.is_empty() => apply_resume(&rec.command, spec),
                _ => rec.command.clone(),
            };
            // env はセッションファイルへ保存しない (シークレットになり得る) ので、
            // プリセット名で現在の設定から引き直す。プリセットが消えていたら
            // 自動承認まわりの既定 env だけで起動する。
            let env = match self.cfg.agents.iter().find(|p| p.name == rec.preset_name) {
                Some(p) => merged_env(&p.command, approval, &p.env),
                None => merged_env(&rec.command, approval, &HashMap::new()),
            };
            let cwd = PathBuf::from(&rec.cwd);
            let cwd = if cwd.is_dir() { cwd } else { self.agent_cwd() };
            let spec = crate::terminal::SpawnSpec {
                title: rec.title.clone(),
                preset_name: rec.preset_name.clone(),
                icon: rec.icon.clone(),
                command,
                cwd,
                env,
                log_path,
            };
            match self.agents.launch_restored(spec, &replay, ctx) {
                Ok(()) => {
                    restored += 1;
                    // 隔離エージェントは**前回と同じ worktree** へ戻す。
                    // cwd は上で worktree のフォルダを指しているので、
                    // ここでは「どのリポジトリ / どのブランチか」を繋ぎ直す。
                    if let Some(id) = self.agents.sessions.last().map(|s| s.id) {
                        if let Some(wt) = restored_worktree(rec) {
                            self.agent_worktrees.insert(id, wt);
                        }
                    }
                }
                Err(e) => self.toast(e, false),
            }
        }
        if restored > 0 {
            self.toast(
                trf(
                    "🔄 前回のエージェント {n} 本を再開しました",
                    &[("n", restored.to_string())],
                ),
                true,
            );
        }
        // 端末分割の復元は**全部起こし終えてから**。リーフはログのパスで
        // 引くので、復元されなかったペイン (素のシェル等) は黙って落ちる。
        let lines: Vec<String> = recs
            .iter()
            .map(|r| r.split.clone())
            .filter(|l| !l.is_empty())
            .collect();
        if !lines.is_empty() {
            self.restore_splits(&lines);
        }
    }

    // ─── 取り消し履歴 (バッファ側の `editor::History` を駆動する) ─────

    /// 単調時計の現在値 (ms)。連続入力の併合判定に使う。
    pub(super) fn undo_now_ms(&self) -> u64 {
        self.undo_clock.elapsed().as_millis() as u64
    }

    /// プログラム的編集 1 回ぶんの記録情報 (整形・置換・行移動など)。
    /// しきい値と上限は**必ず設定から**取る。
    pub(super) fn edit_step(&self) -> editor::Edit {
        editor::Edit::programmatic(self.undo_now_ms(), self.cfg.history_limits())
    }

    /// 打鍵らしさを差分から決める記録情報 (`TextEdit` 経由・折りたたみの差し戻し)。
    pub(super) fn edit_typed(&self) -> editor::Edit {
        editor::Edit::typed(self.undo_now_ms(), self.cfg.history_limits())
    }

    /// 取り消し / やり直しの後始末 (選択復元・折りたたみ表示の作り直し・LSP 通知)。
    pub(super) fn after_undo_redo(&mut self, i: usize, sel: (usize, usize)) {
        self.pending_select = Some(sel);
        self.fold_view = None;
        self.queue_lsp_change(i);
    }

    /// アクティブなタブを 1 段取り消す。
    pub(super) fn undo_active(&mut self) {
        let Some(i) = self.editor.active else { return };
        if self.editor.buffers[i].read_only() {
            self.toast(tr("このタブは読み取り専用です"), false);
            return;
        }
        match self.editor.buffers[i].undo() {
            Some(sel) => self.after_undo_redo(i, sel),
            None => {
                // 上限で古い段を捨てていたなら、黙って「戻せない」で終わらせない
                let msg = if self.editor.buffers[i].history.dropped() > 0 {
                    tr("これ以上取り消せません (古い履歴は上限で破棄しました)")
                } else {
                    tr("これ以上取り消せません")
                };
                self.toast(msg, false);
            }
        }
    }

    /// アクティブなタブを 1 段やり直す。
    pub(super) fn redo_active(&mut self) {
        let Some(i) = self.editor.active else { return };
        if self.editor.buffers[i].read_only() {
            self.toast(tr("このタブは読み取り専用です"), false);
            return;
        }
        match self.editor.buffers[i].redo() {
            Some(sel) => self.after_undo_redo(i, sel),
            None => self.toast(tr("やり直せる操作がありません"), false),
        }
    }

    /// 本文エディタ (中央の `TextEdit`) にフォーカスがあるか。
    ///
    /// ⌘Z を横取りしてよいのはこのときだけ。検索欄やエージェント入力の
    /// `TextEdit` では、そちらの取り消しをそのまま使わせる。
    pub(super) fn editor_body_focused(&self, ctx: &egui::Context) -> bool {
        let Some(i) = self.editor.active else {
            return false;
        };
        let Some(b) = self.editor.buffers.get(i) else {
            return false;
        };
        let id = buf_edit_id(self.cur_pane, b.id);
        ctx.memory(|m| m.has_focus(id))
    }

    /// アクティブバッファへ editor_ops の編集操作を適用する。
    pub(super) fn editor_op(&mut self, ctx: &egui::Context, op: EditOp) {
        let Some(i) = self.editor.active else {
            return;
        };
        let ed_id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
        let range = egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| (r.primary.index, r.secondary.index))
            .unwrap_or((0, 0));
        let (start, end) = (range.0.min(range.1), range.0.max(range.1));

        let prefix = editor_ops::comment_prefix_for(&self.editor.buffers[i].lang);
        if matches!(op, EditOp::ToggleComment) && prefix.is_none() {
            let lang = self.editor.buffers[i].lang.clone();
            self.toast(
                trf("{lang} の行コメント記法が不明です", &[("lang", lang)]),
                false,
            );
            return;
        }

        let (now_ms, limits) = (self.undo_now_ms(), self.cfg.history_limits());
        // JSON 整形だけは**失敗しうる**(壊れた JSON を黙って書き換えない)。
        // 下の `buf` の借用を跨いで toast を出せないので、結果を先に作っておく。
        let json = matches!(op, EditOp::FormatJson).then(|| {
            let unit = self.editor.buffers[i].indent.unit();
            let text = &self.editor.buffers[i].text;
            // 選択が無ければ本文全体を対象にする (VS Code の Format Document 相当)
            let (s, e) = if start == end {
                (0, text.chars().count())
            } else {
                (start, end)
            };
            let sb = editor_ops::char_to_byte(text, s);
            let eb = editor_ops::char_to_byte(text, e);
            editor_ops::format_json(&text[sb..eb], &unit).map(|out| {
                let mut t = String::with_capacity(text.len());
                t.push_str(&text[..sb]);
                t.push_str(&out);
                t.push_str(&text[eb..]);
                (t, (s, s + out.chars().count()))
            })
        });
        if let Some(Err(msg)) = &json {
            let msg = msg.clone();
            self.toast(trf("JSON として読めません: {e}", &[("e", msg)]), false);
            return;
        }
        let json_ok = json.and_then(|r| r.ok());

        let buf = &mut self.editor.buffers[i];
        let (new_text, new_sel) = match op {
            EditOp::ToggleComment => {
                let (t, s, e) = editor_ops::toggle_comment(&buf.text, start, end, prefix.unwrap());
                (t, (s, e))
            }
            EditOp::Duplicate => {
                let (t, c) = editor_ops::duplicate_line(&buf.text, end);
                (t, (c, c))
            }
            EditOp::Move(up) => {
                let (t, c) = editor_ops::move_line(&buf.text, end, up);
                (t, (c, c))
            }
            EditOp::NormalizeEol(target) => {
                let t = crate::textenc::normalize_to(&buf.text, target);
                // CRLF ⇄ LF で本文の長さが変わるので、選択範囲は必ず付け替える
                let s = editor_ops::adjust_char_index_after_cleanup(&buf.text, &t, start);
                let e = editor_ops::adjust_char_index_after_cleanup(&buf.text, &t, end);
                (t, (s, e))
            }
            EditOp::Case(kind) => {
                // 変換で文字数が変わることがある (ß → SS) ので、
                // 選択の終わりは**変換後の長さ**から取り直す。
                let sb = editor_ops::char_to_byte(&buf.text, start);
                let eb = editor_ops::char_to_byte(&buf.text, end);
                let out = editor_ops::transform_case(&buf.text[sb..eb], kind);
                let mut t = String::with_capacity(buf.text.len());
                t.push_str(&buf.text[..sb]);
                t.push_str(&out);
                t.push_str(&buf.text[eb..]);
                let e = start + out.chars().count();
                (t, (start, e))
            }
            EditOp::Sort(desc) => {
                let (t, s, e) = editor_ops::sort_lines(&buf.text, start, end, desc);
                (t, (s, e))
            }
            EditOp::Dedupe => {
                let (t, s, e) = editor_ops::dedupe_lines(&buf.text, start, end);
                (t, (s, e))
            }
            EditOp::ConvertIndent(to) => {
                let from = buf.indent;
                let t = editor_ops::convert_indentation(&buf.text, from, to);
                // 行頭の空白が増減するので、選択範囲は必ず付け替える
                let s = editor_ops::adjust_char_index_after_cleanup(&buf.text, &t, start);
                let e = editor_ops::adjust_char_index_after_cleanup(&buf.text, &t, end);
                buf.indent = to;
                (t, (s, e))
            }
            // 整形は上で済ませてある (失敗なら既に return している)
            EditOp::FormatJson => json_ok.unwrap_or_else(|| (buf.text.clone(), (start, end))),
        };
        // プログラム的編集なので**必ず 1 段**。取り消しで編集前の選択へ戻す。
        let ed = editor::Edit::programmatic(now_ms, limits)
            .with_sel_before((start, end))
            .to_sel(new_sel);
        buf.apply_edit(new_text, ed);
        self.pending_select = Some(new_sel);
    }

    // ─── 複数キャレット (editor_ops::MultiSel) ──────────────────────

    /// アクティブバッファの `(index, id, egui の選択範囲 (char))` を取る。
    pub(super) fn active_buffer_selection(
        &self,
        ctx: &egui::Context,
    ) -> Option<(usize, u64, usize, usize)> {
        let i = self.editor.active?;
        let id = self.editor.buffers.get(i)?.id;
        let ed_id = buf_edit_id(self.cur_pane, id);
        let (a, b) = egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| (r.primary.index, r.secondary.index))
            .unwrap_or((0, 0));
        Some((i, id, a.min(b), a.max(b)))
    }

    /// いまの複数キャレット集合。無ければ egui の単一選択を種にして作る。
    ///
    /// タブが切り替わっていたら (バッファ ID が違う) 前の集合は**捨てる** —
    /// 別のファイルのバイト位置をそのまま当てると本文を壊すため。
    pub(super) fn multi_seed(
        &mut self,
        buf_id: u64,
        text: &str,
        start: usize,
        end: usize,
    ) -> editor_ops::MultiSel {
        match &self.multi_sel {
            Some((id, sel)) if *id == buf_id && !sel.is_empty() => sel.clone(),
            _ => editor_ops::MultiSel::from_char_ranges(text, [start..end]),
        }
    }

    /// 複数キャレットの結果を反映する (キャレットの見た目は 1 本だけ)。
    ///
    /// `to_first` が真なら先頭のキャレットへ寄せる (上へ伸ばすコマンド用)。
    pub(super) fn commit_multi(&mut self, buf_id: u64, sel: editor_ops::MultiSel, to_first: bool) {
        let Some(i) = self.editor.active else { return };
        let text = self.editor.buffers[i].text.clone();
        let n = sel.len();
        let r = if to_first {
            sel.carets().first().cloned().unwrap_or(0..0)
        } else {
            sel.to_single_selection()
        };
        // バイト範囲 → char 範囲 (egui のキャレットは char 添字)
        self.pending_select = Some(byte_range_to_char_range(&text, &r));
        self.multi_sel = Some((buf_id, sel));
        if n > 1 {
            self.toast(
                trf(
                    "✏ キャレット {n} 本 (編集コマンドは全箇所へ 1 回の取り消しで効きます)",
                    &[("n", n.to_string())],
                ),
                true,
            );
        }
    }

    /// Alt 付きポインタ操作を複数キャレットへ反映する。
    ///
    /// `prev_caret` はクリック**前**の egui キャレット (char 範囲)。クリックの
    /// 時点で egui は主キャレットを動かしてしまうので、1 本目の種はこれを使う。
    pub(super) fn apply_multi_pointer(
        &mut self,
        buf_index: usize,
        ev: MultiPointer,
        tab_width: usize,
        prev_caret: Option<(usize, usize)>,
    ) {
        let Some(b) = self.editor.buffers.get(buf_index) else {
            return;
        };
        let buf_id = b.id;
        let text = b.text.clone();
        match ev {
            MultiPointer::Clear => {
                self.multi_sel = None;
                self.multi_sticky_col = None;
            }
            MultiPointer::DragEnd => {
                self.column_anchor = None;
            }
            MultiPointer::DragStart(idx) => {
                let (line, col) = char_index_to_line_col(&text, idx, tab_width);
                self.column_anchor = Some((buf_id, line, col));
                self.multi_sticky_col = None;
            }
            MultiPointer::Drag(idx) => {
                let Some((aid, al, ac)) = self.column_anchor else {
                    return;
                };
                if aid != buf_id {
                    return;
                }
                let (hl, hc) = char_index_to_line_col(&text, idx, tab_width);
                let sel = editor_ops::column_selection(&text, al, ac, hl, hc, tab_width);
                if !sel.is_empty() {
                    // egui 自身もドラッグで「行をまたぐ 1 本の選択」を作ってしまう。
                    // 主キャレットを矩形の最後の行へ寄せて、画面に出るのが
                    // 矩形だけになるようにする (寄せないと 2 種類の選択が重なる)。
                    self.pending_select =
                        Some(byte_range_to_char_range(&text, &sel.to_single_selection()));
                    self.multi_sel = Some((buf_id, sel));
                }
            }
            MultiPointer::Click(idx) => {
                let byte = editor_ops::char_to_byte(&text, idx);
                let mut ranges: Vec<std::ops::Range<usize>> = match &self.multi_sel {
                    Some((id, s)) if *id == buf_id => s.carets().to_vec(),
                    _ => Vec::new(),
                };
                if ranges.is_empty() {
                    if let Some((a, z)) = prev_caret {
                        ranges.push(
                            editor_ops::char_to_byte(&text, a)..editor_ops::char_to_byte(&text, z),
                        );
                    }
                }
                match ranges.iter().position(|r| r.start == byte && r.end == byte) {
                    // 同じ位置をもう一度 Alt+クリック → 取り除く (VS Code と同じ)。
                    // 最後の 1 本は残す (0 本にすると打鍵の行き先が消える)。
                    Some(p) if ranges.len() > 1 => {
                        ranges.remove(p);
                    }
                    Some(_) => {}
                    None => ranges.push(byte..byte),
                }
                let sel = editor_ops::MultiSel::in_text(&text, ranges);
                self.multi_sel = (!sel.is_empty()).then_some((buf_id, sel));
                self.multi_sticky_col = None;
            }
        }
    }

    /// パレットから来た複数キャレット系コマンドを処理する。
    pub(super) fn apply_cmd_multi_cursor(&mut self, cmd: Cmd, ctx: &egui::Context) {
        let tw = crate::highlight::DEFAULT_TAB_WIDTH;
        let Some((i, buf_id, start, end)) = self.active_buffer_selection(ctx) else {
            self.toast(tr("先にファイルを開いてください"), false);
            return;
        };
        let text = self.editor.buffers[i].text.clone();

        match cmd {
            Cmd::ClearMultiCursor => {
                self.multi_sel = None;
                self.multi_sticky_col = None;
                self.column_anchor = None;
                self.toast(tr("✏ 複数キャレットを解除しました"), true);
            }
            Cmd::AddCursorAbove | Cmd::AddCursorBelow => {
                let up = matches!(cmd, Cmd::AddCursorAbove);
                let seed = self.multi_seed(buf_id, &text, start, end);
                // sticky column: 押し始めの桁を覚えて、短い行を跨いでも戻る
                if self.multi_sticky_col.is_none() {
                    let anchor = if up {
                        seed.carets().first().map(|r| r.start)
                    } else {
                        seed.carets().last().map(|r| r.end)
                    };
                    self.multi_sticky_col =
                        anchor.map(|b| editor_ops::visual_column_of(&text, b, tw));
                }
                let col = self.multi_sticky_col;
                let next = if up {
                    editor_ops::add_cursor_above_at(&text, &seed, tw, col)
                } else {
                    editor_ops::add_cursor_below_at(&text, &seed, tw, col)
                };
                if next.len() == seed.len() {
                    self.toast(tr("これ以上キャレットを増やせません"), false);
                    return;
                }
                self.commit_multi(buf_id, next, up);
            }
            Cmd::SelectAllOccurrences | Cmd::SelectNextOccurrence => {
                let needle: String = text
                    .chars()
                    .skip(start)
                    .take(end.saturating_sub(start))
                    .collect();
                if needle.trim().is_empty() {
                    self.toast(tr("先に語を選択してください (選択が空です)"), false);
                    return;
                }
                // 一致規則は「ファイル間で検索」と同じ設定をそのまま使う
                let opts = editor_ops::MatchOpts {
                    case_sensitive: self.gsearch.case_sensitive,
                    whole_word: self.gsearch.whole_word,
                    regex: false,
                };
                let next = if matches!(cmd, Cmd::SelectAllOccurrences) {
                    editor_ops::select_all_occurrences(&text, &needle, opts)
                } else {
                    let seed = self.multi_seed(buf_id, &text, start, end);
                    editor_ops::select_next_occurrence(&text, &seed, &needle, opts)
                };
                if next.is_empty() {
                    self.toast(tr("見つかりませんでした"), false);
                    return;
                }
                self.multi_sticky_col = None;
                self.commit_multi(buf_id, next, false);
            }
            Cmd::MultiPaste => {
                let Some(ins) = menu_bar::clipboard_text() else {
                    self.toast(tr("クリップボードにテキストがありません"), false);
                    return;
                };
                let seed = self.multi_seed(buf_id, &text, start, end);
                if seed.is_empty() {
                    self.toast(tr("キャレットがありません"), false);
                    return;
                }
                // **1 回だけ**本文を差し替える = egui の取り消しも 1 段。
                let (new_text, sel, n) = multi_batch_insert(&text, &seed, &ins);
                let ed = self.edit_step().with_sel_before((start, end)).to_sel({
                    let r = sel.to_single_selection_chars(&new_text);
                    (r.start, r.end)
                });
                self.editor.buffers[i].apply_edit(new_text, ed);
                self.fold_view = None;
                self.queue_lsp_change(i);
                self.commit_multi(buf_id, sel, false);
                self.toast(
                    trf(
                        "✏ {n} 箇所へ貼り付けました ({undo} 一回で戻ります)",
                        &[
                            ("n", n.to_string()),
                            // 取り消しは自前の履歴なので割り当ては設定で変えられる。
                            // 表記は必ず現在のバインドから作る
                            // (ベタ書きの ⌘Z は Windows/Linux でも再割り当てでも嘘になる)。
                            ("undo", self.key_hint(BindAction::Undo)),
                        ],
                    ),
                    true,
                );
            }
            Cmd::ColumnSelectStart => {
                let (line, col) = char_index_to_line_col(&text, start, tw);
                self.column_anchor = Some((buf_id, line, col));
                self.toast(
                    trf(
                        "◇ 矩形選択の始点: {line} 行 {col} 桁 — もう一方の角へ移動して「矩形選択の確定」",
                        &[
                            ("line", (line + 1).to_string()),
                            ("col", (col + 1).to_string()),
                        ],
                    ),
                    true,
                );
            }
            Cmd::ColumnSelectFinish => {
                let Some((aid, al, ac)) = self.column_anchor else {
                    self.toast(tr("先に「矩形選択の開始」を実行してください"), false);
                    return;
                };
                if aid != buf_id {
                    self.column_anchor = None;
                    self.toast(
                        tr("矩形選択の始点は別のタブのものでした — やり直してください"),
                        false,
                    );
                    return;
                }
                let (hl, hc) = char_index_to_line_col(&text, end, tw);
                let sel = editor_ops::column_selection(&text, al, ac, hl, hc, tw);
                self.column_anchor = None;
                self.multi_sticky_col = None;
                if sel.is_empty() {
                    self.toast(tr("矩形が空です"), false);
                    return;
                }
                self.commit_multi(buf_id, sel, false);
            }
            _ => {}
        }
    }

    pub(super) fn toast(&mut self, msg: impl Into<String>, ok: bool) {
        self.push_toast(msg, if ok { 0 } else { 2 });
    }

    pub(super) fn toast_warn(&mut self, msg: impl Into<String>) {
        self.push_toast(msg, 1);
    }

    pub(super) fn push_toast(&mut self, msg: impl Into<String>, kind: u8) {
        self.toasts.push(Toast {
            msg: msg.into(),
            kind,
            at: Instant::now(),
        });
        if self.toasts.len() > 5 {
            self.toasts.remove(0);
        }
    }

    /// ファイル索引の作り直しを**バックグラウンドで**始める。
    ///
    /// 設計原則 2: 隠れている処理は欠落ありで良いが、決して UI をブロックしない。
    /// 走査中も ⌘P は開けて、そのとき出来ている分だけが出る (進捗を添える)。
    pub(super) fn rebuild_index(&mut self) {
        let roots = self.roots.clone();
        let opts = IndexOptions::from_config(&self.cfg);
        self.index_gen = self.index_gen.wrapping_add(1);
        let gen = self.index_gen;
        let progress = Arc::new(AtomicUsize::new(0));
        self.index_progress = progress.clone();
        let (tx, rx) = mpsc::channel();
        let job = opts.clone();
        let spawned = std::thread::Builder::new()
            .name("zv-file-index".into())
            .spawn(move || {
                let out = build_file_index_with(&roots, &job, Some(&progress));
                let _ = tx.send((gen, out));
            })
            .is_ok();
        if spawned {
            self.index_rx = Some(rx);
        } else {
            // スレッドが立てられない環境では従来どおり同期で作る
            // (遅くはなるが「索引が無い」よりはよい)。
            let out = build_file_index_with(&self.roots, &opts, None);
            self.apply_index(out);
        }
    }

    /// バックグラウンド索引の完了を取り込む (毎フレーム呼んでよい)。
    /// 走査中は控えめな再描画を予約して進捗が止まって見えないようにする。
    pub(super) fn poll_index(&mut self, ctx: &egui::Context) {
        let Some(rx) = self.index_rx.as_ref() else {
            return;
        };
        match rx.try_recv() {
            Ok((gen, out)) => {
                self.index_rx = None;
                // 古い世代 (ルートが変わった等) の結果は捨てる
                if gen == self.index_gen {
                    self.apply_index(out);
                }
            }
            Err(std::sync::mpsc::TryRecvError::Empty) => {
                crate::perf::repaint_after(
                    ctx,
                    std::time::Duration::from_millis(200),
                    "poll_index",
                );
            }
            Err(std::sync::mpsc::TryRecvError::Disconnected) => self.index_rx = None,
        }
    }

    pub(super) fn apply_index(&mut self, out: IndexOutcome) {
        self.index_truncated = out.truncated;
        self.file_index = out.files;
        self.index_at = Some(Instant::now());
        // `@` ピッカーは**主ルート配下だけ**を扱う (rel は所属ルート基準なので、
        // 別ルートのものを混ぜると root.join(rel) が別のファイルを指す)。
        self.mention_rels = match self.roots.first() {
            Some(root) => self
                .file_index
                .iter()
                .filter(|f| f.abs.starts_with(root))
                .map(|f| f.rel.clone())
                .collect(),
            None => Vec::new(),
        };
    }

    /// ⌘P のパレットに出す「索引の状態」。作成中/打ち切りのときだけ Some。
    pub(super) fn index_note(&self) -> Option<String> {
        if self.index_rx.is_some() {
            let n = self.index_progress.load(Ordering::Relaxed);
            return Some(trf(
                "索引を作成中… {n} 件走査 (今ある分から探せます)",
                &[("n", n.to_string())],
            ));
        }
        if self.index_truncated {
            return Some(trf(
                "{n} 件で打ち切りました (設定 index_max_files で変更できます)",
                &[("n", self.file_index.len().to_string())],
            ));
        }
        None
    }
}
