use super::*;

impl eframe::App for ZaivernApp {
    fn update(&mut self, ctx: &egui::Context, frame: &mut eframe::Frame) {
        // フレーム内の panic をアプリごと道連れにしない。winit/eframe は
        // update 中の panic を捕まえるとイベントループごと終了するため、
        // 利用者には「作業中にいきなり落ちた」に見える (実行中のエージェント
        // も全部道連れ)。ここで捕捉して「1 フレーム捨てる」に格下げする。
        // 詳細は panic フック (main.rs) が ~/.zaivern/panic.log へ残す。
        //
        // ただし「捨てて続ける」だけでは足りない。**たまに成功する** panic
        // (panic → ok → panic → ok …) は、以前の「完走したらカウンタを 0」
        // では永久に検知できず、半分だけ組み立てた画面を延々と描き続けた
        // (= 画面が崩れて操作できない、しかも落ちない)。
        // いまは時間窓で頻度を見て、収まらなければ犯人の部分ビューを隔離し、
        // それでも駄目なら最後の手段として従来どおり落とす (`FramePanicPolicy`)。
        let _ = take_drawing_subview(); // 前フレームの印が残っていたら捨てる
                                        // フレーム時間の計測。ZAIVERN_PERF=1 のときだけ Instant を取る
                                        // (無効時はここも [`perf::frame_end`] も即 return する)。
        let perf_frame = crate::perf::frame_start();
        let ok = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.update_impl(ctx, frame);
        }));
        crate::perf::frame_end(perf_frame);
        let now_ms = self.frame_guard.now_ms();
        match ok {
            Ok(()) => {
                let _ = take_drawing_subview();
                self.frame_guard.observe(FrameOutcome::Ok, None, now_ms);
            }
            Err(payload) => {
                let msg = panic_message(&*payload);
                // 犯人はまず「描いている途中の印」から。取れなければ
                // panic メッセージに混ざった位置情報から推測する。
                let culprit = take_drawing_subview().or_else(|| subview_from_panic_message(&msg));
                let action = self
                    .frame_guard
                    .observe(FrameOutcome::Panic, culprit.clone(), now_ms);
                eprintln!(
                    "zaivern: frame panicked ({action:?}, culprit={culprit:?}) — {msg} \
                     (details: ~/.zaivern/panic.log)"
                );
                match action {
                    FrameGuardAction::Abort => std::panic::resume_unwind(payload),
                    FrameGuardAction::Quarantine => {
                        let where_ = culprit
                            .as_ref()
                            .map(|c| c.label())
                            .unwrap_or_else(|| tr("不明な場所"));
                        let banner = if culprit.is_some() {
                            trf(
                                "⚠ {where} で内部エラーが繰り返し起きています。\
                                 壊れた部分の描画を止めました (詳細: ~/.zaivern/panic.log)",
                                &[("where", where_)],
                            )
                        } else {
                            tr("⚠ 内部エラーが繰り返し起きています。原因の場所を特定できませんでした (詳細: ~/.zaivern/panic.log)")
                        };
                        self.toast_warn(banner.clone());
                        self.frame_guard.banner = Some(banner);
                    }
                    FrameGuardAction::Continue => {
                        self.toast_warn(tr(
                            "⚠ 内部エラーが起きました。継続します (詳細: ~/.zaivern/panic.log)",
                        ));
                    }
                }
                crate::perf::repaint(ctx, "update");
            }
        }
    }

    /// 終了時の後始末: CLI 向けの接続情報ファイルと、走らせたままのエージェント。
    ///
    /// セッションをそのまま drop させると ConPTY を閉じる待ちで終了処理が止まり、
    /// ウィンドウが閉じないまま残る。エージェントを落として PTY は OS に任せる
    /// ([`crate::terminal::abandon`] の説明を参照)。
    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // 計測が有効なら、この起動ぶんのフレーム時間を 1 回だけ書き出す。
        // 「起動 → 触る → 閉じる」で版ごとのレポートが 1 行ずつ溜まるので、
        // 版間比較にボタン操作が要らない (無効なら何もしない)。
        crate::perf::dump();
        // 終わる前にセッションを保存する — エージェントタブの記録はここでしか
        // 確実に残せない (走らせたまま閉じても、次回開けば会話を再開できる)。
        self.persist_session();
        cli::remove_instance_file();
        // SSH トンネルは必ず畳む。置き去りにすると踏み台の公開ポートを掴んだ
        // ssh が残り、次に繋ぐとき「ポート使用中」で失敗する。
        self.tunnel.disconnect();
        // 握っているファイル所有を返す。返し損ねても TTL で回収されるが、
        // 返せば次の担当がすぐ入れる (待たせる時間がそのまま損害になる)。
        crate::lease::release_all();
        for s in std::mem::take(&mut self.agents.sessions) {
            crate::terminal::abandon(s);
        }
    }
}

impl ZaivernApp {
    /// 1 フレーム分の実処理 ([`eframe::App::update`] の本体)。
    /// 呼び出し側のパニックガードが囲うので、ここからの panic は
    /// アプリ終了ではなく「フレームのスキップ」になる。
    pub(super) fn update_impl(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // 壊れたネイティブ全画面の検知と疑似フルスクリーンへの救出 (macOS)
        self.fullscreen_guard(ctx);
        // 内部エラーの警告バナーは**最初に**描く。フレームの途中で panic しても
        // ここまでは描き終わっているので、崩れた画面でも理由が読める。
        self.frame_error_banner_ui(ctx);
        // 定期フレームの予約は**フレームの最後**で [`idle_repaint_ms`] が決める。
        //
        // かつてはここで無条件に 4fps を予約していた。理由は「スマホリモートと
        // CLI (`zai notify` など) の要求は UI スレッドがフレームを回した時にしか
        // 処理できず、背面や非表示だと request_repaint 1 回では取りこぼす」から
        // だったが、その心配は [`crate::remote`] 側で解決済み — HTTP を受けた
        // スレッドが応答を受け取るまで 150ms 間隔で `request_repaint` を撃ち
        // 続ける (`remote.rs` の待機ループ)。一方向の指示も積んだ直後に 1 回
        // 起こす。よって「誰も何もしていないのに 4fps で回す」理由は無い。
        //
        // 永続 UI 設定 (検索オプション・保存時クリーンアップ) は最初のフレームで読む
        self.load_prefs_once(ctx);
        // バックグラウンドで作っている索引の完了を取り込む (UI は止めない)
        self.poll_index(ctx);
        // 編集中のファイルの所有を台帳と揃える (無効なら即 return する)。
        self.sync_lease_ownership();

        // 画面全体のズームは毎フレームここで egui へ揃える。値が変わって
        // いなければ何も起きない (再描画も要求されない)。
        apply_ui_zoom(ctx, self.cfg.ui_zoom);
        // ⌘+ホイール / ピンチはショートカットより先に見る。ここで拾わないと
        // ScrollArea が同じイベントをスクロールとして食ってしまう。
        self.handle_zoom_gesture(ctx);

        // **このフレームで描く中央ビューをここで 1 つに畳む。**
        // 以降の描画判断はすべて `self.center` を見る。描画中にフラグが
        // 変わっても今フレームの絵は変わらないので、2 つのビューが重なって
        // 描かれることがない (切り替えは次のフレームから効く)。
        self.center = center_view(self.cockpit, self.kanban, self.deck);

        // 音声入力が先。押している間だけ録音するキーは他所へ渡さない
        // (ターミナルが PTY へ転送してしまうため)
        self.poll_voice(ctx);

        self.handle_shortcuts(ctx);
        // which-key が拾う打鍵 (⌫ / ? / 番号) は **パネルを描く前に**取る。
        // ポップアップ自身は最後に描くが、そのころには本文の TextEdit が
        // Backspace を食べ終わっている (フォーカスがあれば必ず取る)。
        self.whichkey_keys(ctx);
        // ⌃Tab の切替は**修飾キーを離したフレーム**で確定する。
        // ショートカット処理の直後に見る (押した同じフレームで確定させない)。
        self.tab_switcher_tick(ctx);
        // ⌘+ホイール / トラックパッドのピンチ。egui はどちらも zoom_delta へ
        // 集約するので、ここで 1 か所だけ拾う (画像タブは自前で消費する)。
        self.handle_zoom_gesture(ctx);
        // Keybinds を持てない描画側 (ターミナルの右クリックメニュー等) へ
        // 打鍵表記を配る。ベタ書きを増やさないための唯一の経路。
        crate::keybinds::publish_key_hints(
            ctx,
            &self.keys,
            &[BindAction::Find, BindAction::MarkToggleMnemonic],
        );

        // メニューバー経由のエディタ操作 (元に戻す/貼り付け等) を、パネル描画前に
        // フォーカス復帰 + イベント注入で TextEdit へ届ける
        self.flush_editor_events(ctx);
        // 定義ジャンプ (F12) の LSP 応答と、ファイル横断検索の結果を取り込む
        self.poll_definition_result();
        // LSP: 応答の回収 (sweep_timeouts は毎フレーム) と補完のデバウンス。
        // どちらもチャネルを覗くだけで、UI スレッドでは I/O をしない。
        self.poll_lsp();
        // 補完ポップアップのキー (Enter/Tab/Esc/矢印) は本文 TextEdit より先に
        // 消費する必要があるので、パネル描画前のここで処理する。
        self.lsp_completion_tick(ctx);
        self.lsp_actions_tick(ctx);
        self.poll_global_search();
        // Git blame: ワーカーの結果を取り込む。ジョブが無ければ Option の
        // 検査 1 回で戻り、再描画も要求しない (= アイドル時のコストはゼロ)。
        self.blame.poll();
        if self.blame.busy() {
            crate::perf::repaint_after(ctx, std::time::Duration::from_millis(80), "update_impl");
        }
        // セッションタブに出すフォルダ一覧 (is_dir を叩くので変化時だけ作り直す)
        self.refresh_session_folders();
        // 差分ビューの「エージェントに送る」を拾って入力欄へ流し込む
        if let Some(p) = crate::diff::take_pending_review_prompt(ctx) {
            self.take_review_prompt(p);
        }
        // 差分ビューのツールバーで表示モードが切り替えられていたら控えて永続化する
        // (ctx が実行時の持ち主・config が既定値の持ち主、という 1 方向の関係)。
        {
            let m = crate::diff::diff_mode(ctx);
            if m != crate::diff::DiffMode::from_config_str(&self.cfg.diff_view) {
                self.cfg.diff_view = m.config_str().into();
                config::save_state(&self.cfg);
            }
        }
        // 差分ビューからの一言 (「変更はありません」など)
        if let Some(msg) = crate::diff::take_pending_notice(ctx) {
            self.toast(msg, true);
        }
        // 別スレッドで開いているファイルダイアログの結果を取り込む
        self.poll_dialogs(ctx);
        // 自動保存 (メニュー: ファイル > 自動保存)
        self.autosave_tick();
        // メニュー関連の小窓 (行/列へ移動・問題・ショートカット一覧・バージョン情報)
        self.menu_windows_ui(ctx);
        // ⌃Tab の候補一覧 (押している間だけ・画面中央の 1 枚)
        self.tab_switcher_ui(ctx);

        // スマホリモートからのリクエストを処理する
        self.poll_remote(ctx);

        // ファイアウォール操作 (別スレッド) の結果を取り込む。
        // UAC の確認中に 📱 ウィンドウを閉じられても結果は拾えるよう、
        // ウィンドウの描画とは切り離してここで回す
        if self.fw.poll() {
            if let Some(msg) = self.fw.done.take() {
                self.toast(tr(&msg), true);
            }
            if let Some(err) = self.fw.error.clone() {
                self.toast_warn(format!("🛡 {err}"));
            }
        }

        // プラグインコマンドの実行結果をエディタへ反映する
        self.process_plugin_results(ctx);

        // gh (GitHub CLI) の実行結果を GitHub パネルへ反映する
        self.process_gh_results();

        // 「エージェントを追加」ピッカー (PATH 検出の結果取り込みも兼ねる)
        self.agent_picker_ui(ctx);

        // エージェント名のリネーム窓 (開いていなければ 1px も描かない)
        self.rename_agent_ui(ctx);
        // セッションの自動命名 (既定オフ。結果の回収 → ターン境界 → 依頼)
        self.auto_name_tick(ctx);

        // フック: 起動時 (初回フレームの後に一度だけ)
        if !self.startup_hooks_done {
            self.startup_hooks_done = true;
            self.hook_git_branch = self.git_branch();
            self.fire_hooks(plugins::HookEvent::Startup, None, ctx);
        }
        // フック: ブランチが切り替わったら git_change
        let branch = self.git_branch();
        if branch != self.hook_git_branch {
            self.hook_git_branch = branch;
            self.fire_hooks(plugins::HookEvent::GitChange, None, ctx);
        }
        // 予約されたフック (ファイル操作・エージェントの状態変化) を消化する
        for (event, file) in std::mem::take(&mut self.pending_hooks) {
            self.fire_hooks(event, file, ctx);
        }
        // interval フックと interval 更新のパネルを回す
        self.tick_plugin_timers(ctx);

        // 外部(エージェント等)によるファイル書き換えを検知して自動リロードする
        self.check_external_changes();

        // Hot Exit: 未保存の本文を間引いて退避する。
        // 編集が無いフレームでは指紋を取るだけ (I/O も再描画も起こさない)。
        self.hotexit_tick(ctx);

        // LSP: デバウンスした変更を送信し、閉じたドキュメントを did_close する
        self.flush_lsp_changes();
        if !self.lsp_opened.is_empty() {
            let open_paths: HashSet<PathBuf> = self
                .editor
                .buffers
                .iter()
                .filter_map(|b| b.path.clone())
                .collect();
            let closed: Vec<PathBuf> = self.lsp_opened.difference(&open_paths).cloned().collect();
            for p in closed {
                for client in self.lsp.values() {
                    client.did_close(&p);
                }
                self.lsp_opened.remove(&p);
                self.lsp_pending.remove(&p);
            }
        }

        // エージェントの状態変化を通知する(非フォーカス時は OS 通知も)
        let win_focused = ctx.input(|i| i.viewport().focused.unwrap_or(true));
        // 自動YESモード (`pet_auto_yes`) が OFF の間は自動応答せず、ユーザーの承認を待つ。
        // 既定は OFF — ユーザーが明示的にオンにしない限り勝手に YES は送らない
        let allow_auto_yes = self.cfg.pet_auto_yes;
        for ev in self.agents.poll_events(allow_auto_yes) {
            match ev {
                SessionEvent::NeedsApproval(title) => {
                    // 同じセッションへのトースト+効果音は10秒に1回まで
                    // (プロンプトが画面に残ると再検出で連発するため)
                    let throttled = self
                        .pet_attention_notified
                        .get(&title)
                        .is_some_and(|at| at.elapsed().as_secs() < 10);
                    if !throttled {
                        self.pet_attention_notified
                            .insert(title.clone(), Instant::now());
                        self.toast_warn(trf(
                            "🔔 {title} が承認待ちです — パネルで確認してください",
                            &[("title", title.clone())],
                        ));
                        if self.cfg.pet_sounds {
                            self.sound.play(SoundKind::Confirm);
                        }
                    }
                    // OS 通知も同じスロットリングに入れる。素通りさせると
                    // プロンプト再検出のたびに通知センターが同文で埋まり、
                    // 通知プロセスの起動も連発する
                    if !throttled && !win_focused {
                        notify::notify(
                            "Zaivern Code",
                            &trf("🔔 {title} が承認待ちです", &[("title", title.clone())]),
                        );
                    }
                    if !throttled {
                        notify::webhook(
                            &self.cfg.webhook_url,
                            &tr("🔔 承認待ち"),
                            &trf("{title} が承認を待っています", &[("title", title.clone())]),
                        );
                        self.queue_hook(plugins::HookEvent::AgentAttention, None);
                    }
                }
                SessionEvent::AutoApproved(title, desc) => {
                    self.toast(
                        trf(
                            "⚡ {title}: {desc} を自動送信しました",
                            &[("title", title.clone()), ("desc", tr(desc))],
                        ),
                        true,
                    );
                }
                SessionEvent::Exited(title, code) => {
                    if code == 0 {
                        self.toast(
                            trf("✅ {title} が終了しました", &[("title", title.clone())]),
                            true,
                        );
                        // ペットが少しのあいだ喜ぶ + 完了音
                        self.pet_happy_until = Some(Instant::now() + Duration::from_secs(4));
                        if self.cfg.pet_sounds {
                            self.sound.play(SoundKind::Complete);
                        }
                    } else {
                        self.toast(
                            trf(
                                "❌ {title} が終了しました (code {code})",
                                &[("title", title.clone()), ("code", code.to_string())],
                            ),
                            false,
                        );
                        // ペットが少しのあいだ落ち込む + エラー音
                        self.pet_error_until = Some(Instant::now() + Duration::from_secs(6));
                        if self.cfg.pet_sounds {
                            self.sound.play(SoundKind::Error);
                        }
                    }
                    if !win_focused {
                        let mark = if code == 0 { "✅" } else { "❌" };
                        notify::notify(
                            "Zaivern Code",
                            &trf(
                                "{mark} {title} が終了しました",
                                &[("mark", mark.to_string()), ("title", title.clone())],
                            ),
                        );
                    }
                    let mark = if code == 0 { "✅" } else { "❌" };
                    notify::webhook(
                        &self.cfg.webhook_url,
                        &trf("{mark} エージェント終了", &[("mark", mark.to_string())]),
                        &trf(
                            "{title} が終了しました (code {code})",
                            &[("title", title.clone()), ("code", code.to_string())],
                        ),
                    );
                    self.queue_hook(plugins::HookEvent::AgentFinish, None);
                }
                SessionEvent::RateLimited(title, line) => {
                    self.toast_warn(trf(
                        "⏳ {title} がレート制限/使用上限に達しました",
                        &[("title", title.clone())],
                    ));
                    // 自動フェイルオーバー (既定は無効)。有効なときだけ、
                    // 別プロファイル / 別 CLI へ引き継ぐ。現行セッションは残す。
                    self.failover_on_rate_limit(&title, &line, ctx);
                    if self.cfg.pet_sounds {
                        self.sound.play(SoundKind::Confirm);
                    }
                    if !win_focused {
                        notify::notify(
                            "Zaivern Code",
                            &trf(
                                "⏳ {title} がレート制限/使用上限に達しました",
                                &[("title", title.clone())],
                            ),
                        );
                    }
                    notify::webhook(
                        &self.cfg.webhook_url,
                        &tr("⏳ レート制限"),
                        &format!("{title}: {line}"),
                    );
                }
            }
        }

        // ── エージェントへの指示の配達 (唯一の出口) ──
        self.submit_tick(ctx);

        // 表示中のアクティブセッションを既読にする。未読 (◆) は
        // 「見ていない間に意味的な出力が変わった」セッションだけに残る。
        // 看板タブ表示中はパネルに端末が見えていないので既読にしない。
        if (self.agents.panel_open && !self.kanban && !self.deck) || self.cockpit || self.deck {
            let active = self.agents.active;
            if let Some(s) = self.agents.sessions.get_mut(active) {
                s.mark_read();
            }
        }

        // ペットバブル関連の記録を毎フレーム掃除する(ペット非表示中も行い、
        // セッションの増減で無関係なセッションの記録が残らないようにする)
        {
            let sessions = &self.agents.sessions;
            // 承認待ちでなくなったセッションの却下記録は外す(次のプロンプトで再表示)
            self.pet_bubble_dismissed.retain(|&id| {
                sessions
                    .iter()
                    .any(|s| s.id == id && s.attention && s.running())
            });
            // 応答済み記録は3秒経過またはセッション消滅で外す
            self.pet_bubble_answered.retain(|&id, at| {
                at.elapsed().as_secs() < 3 && sessions.iter().any(|s| s.id == id)
            });
            // 通知スロットルの古い記録も掃除する
            self.pet_attention_notified
                .retain(|_, at| at.elapsed().as_secs() < 10);
        }

        // ── Follow the agent ──────────────────────────────────
        // 追従がオフなら最初の 1 行で戻る (git もファイルシステムも触らない)。
        self.follow_tick(ctx);

        // ── 監視・連携 ────────────────────────────────────────
        // セッションの増減を先に反映してから、見張り → 配達の順で回す。
        self.reconcile_sessions();
        self.terminal_hooks(ctx, win_focused);
        self.supervise(ctx, win_focused);
        // 通知は「働いていたものが手を止めた瞬間」の 1 点だけ。
        // 見張りが段を更新した直後に見る (同じフレームの判定を使う)。
        self.notify_work_done(win_focused);
        self.coordinate(win_focused);
        self.quota_tick();
        self.failover_tick();
        self.git_ops_poll();

        self.top_bar(ctx);
        self.status_bar(ctx);
        // LSP の小窓 (参照一覧・シンボル一覧・リネーム入力)。
        // ポップアップ (補完 / ホバー) は本文の上に重ねたいので中央パネルの後。
        self.lsp_refs_window(ctx);
        self.lsp_symbols_window(ctx);
        self.lsp_rename_window(ctx);
        // 大きな領域は「いま描いている場所」の印を付けて描く。フレームが
        // panic したときに犯人を特定して、そこだけ隔離できるようにするため。
        self.guarded_view(Subview::Panel("sidebar"), ctx, |me| me.sidebar(ctx));
        self.guarded_view(Subview::Panel("terminal"), ctx, |me| me.terminal_panel(ctx));

        let theme_bg = self.theme.bg;
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(theme_bg))
            .show(ctx, |ui| {
                // 看板・デッキ・Cockpit はどれも「全エージェントを一望する」画面
                // なので、中央パネル全面に描く (下部端末パネルは畳む)。看板を
                // 下部パネルの中で描いていた頃は既定 300px しか使えず、画面の
                // 1/3 にレーンが押し込まれていた。
                // **ここで見るのは `self.center` だけ。** 生のフラグを見ると、
                // 描画中に押された「看板」でフラグが変わり、同じフレームに
                // 2 つのビューが描かれて重なる (実際に起きた不具合)。
                if self.center == CenterView::Deck {
                    let ctx = ui.ctx().clone();
                    self.guarded_ui(Subview::Panel("deck"), ui, |me, ui| me.deck_ui(ui, &ctx));
                } else if self.center == CenterView::Kanban {
                    let ctx = ui.ctx().clone();
                    self.guarded_ui(Subview::Panel("kanban"), ui, |me, ui| {
                        me.kanban_ui(ui, &ctx)
                    });
                } else if self.center == CenterView::Cockpit {
                    let ctx = ui.ctx().clone();
                    // ファイルを開いていれば左に編集ペインを並べて出す。
                    // Cockpit との切り替え無しでファイルが見えるようにするため。
                    if self.editor.buffers.is_empty() {
                        self.guarded_ui(Subview::Panel("cockpit"), ui, |me, ui| {
                            me.cockpit_ui(ui, &ctx)
                        });
                    } else {
                        let avail = ui.available_width();
                        egui::SidePanel::left("cockpit-editor-split")
                            .frame(egui::Frame::none().fill(theme_bg))
                            .resizable(true)
                            .default_width((avail * 0.42).max(280.0))
                            .min_width(220.0)
                            .max_width(avail * 0.75)
                            .show_inside(ui, |ui| {
                                self.guarded_ui(Subview::Panel("editor"), ui, |me, ui| {
                                    me.editor_area(ui)
                                })
                            });
                        egui::CentralPanel::default()
                            .frame(egui::Frame::none().fill(theme_bg))
                            .show_inside(ui, |ui| {
                                self.guarded_ui(Subview::Panel("cockpit"), ui, |me, ui| {
                                    me.cockpit_ui(ui, &ctx)
                                })
                            });
                    }
                } else {
                    self.guarded_ui(Subview::Panel("editor"), ui, |me, ui| me.editor_area(ui));
                }
            });

        // ── 端末のリンククリック ──
        // `terminal::draw` の呼び出し口は多数あるので、戻り値ではなく egui の
        // 一時データ経由で受け取る (ドロップの印と同じ約束)。
        if let Some((path, line, col)) = terminal::take_open_request(ctx) {
            self.open_path_at(&path, line, col);
        }

        // ── OS からのファイルドロップ ──
        // ターミナル (terminal::draw) が受けた分は印が立つので、残りをエディタ側で
        // 処理する: ファイル → タブで開く / フォルダ → ワークスペースに追加。
        let dropped: Vec<egui::DroppedFile> = ctx.input(|i| i.raw.dropped_files.clone());
        if !dropped.is_empty() {
            let consumed = ctx
                .data_mut(|d| d.remove_temp::<bool>(egui::Id::new("zv-drop-consumed")))
                .unwrap_or(false);
            if !consumed {
                for f in dropped {
                    let Some(p) = f.path else { continue };
                    if p.is_dir() {
                        self.add_folder_to_workspace(p, ctx);
                    } else {
                        self.open_path(&p);
                    }
                }
            }
        }
        // ドラッグ中は行き先のヒントを出す
        if ctx.input(|i| !i.raw.hovered_files.is_empty()) {
            egui::Area::new(egui::Id::new("zv-drop-hint"))
                .order(egui::Order::Foreground)
                .anchor(Align2::CENTER_BOTTOM, egui::vec2(0.0, -48.0))
                .show(ctx, |ui| {
                    egui::Frame::none()
                        .fill(self.theme.panel)
                        .stroke(egui::Stroke::new(1.0_f32, self.theme.accent))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(14.0, 8.0))
                        .show(ui, |ui| {
                            ui.label(
                                RichText::new(tr(
                                    "ターミナルへドロップ = @パスを入力欄へ挿入 ・ それ以外 = エディタで開く (フォルダは追加)",
                                ))
                                .color(self.theme.text),
                            );
                        });
                });
        }

        // 本文の上に重ねる LSP のポップアップ (中央パネルより後に描く)
        self.lsp_completion_popup(ctx);
        self.lsp_signature_popup(ctx);
        self.lsp_code_actions_popup(ctx);
        self.diag_hover_popup(ctx);
        self.lsp_hover_popup(ctx);

        self.palette_ui(ctx);
        self.encoding_picker_ui(ctx);
        self.new_plugin_ui(ctx);
        self.close_confirm_ui(ctx);
        self.delete_confirm_ui(ctx);
        self.transfer_confirm_ui(ctx);
        self.intervention_confirm_ui(ctx, win_focused);
        self.stop_confirm_ui(ctx);
        self.stop_all_confirm_ui(ctx);
        self.worktree_confirm_ui(ctx);
        self.checkpoint_ui(ctx);
        self.local_history_ui(ctx);
        self.remote_window(ctx);
        self.voice_hud(ctx);
        // feature.rs のレジストリに登録された機能のオーバーレイ。
        // **ここが唯一の描画差し込み口**で、機能が増えてもこの 1 行は
        // 変わらない (並列開発の衝突対策)。トーストより手前に置いて、
        // 通知が機能のオーバーレイに隠れないようにしておく。
        crate::feature::draw_all(self, ctx);
        self.toasts_ui(ctx);
        self.whichkey_ui(ctx);

        // デスクトップペット 🐾
        if self.cfg.show_pet {
            let now = Instant::now();
            let attention = self
                .agents
                .sessions
                .iter()
                .filter(|s| s.attention && s.running())
                .count();
            let input = pet::PetInput {
                working: self.agents.running_count(),
                attention,
                recent_success: self.pet_happy_until.is_some_and(|t| now < t),
                recent_error: self.pet_error_until.is_some_and(|t| now < t),
                variant: pet::PetVariant::from_name(&self.cfg.pet_variant),
                scale: self.cfg.pet_scale,
                free_roam: self.cfg.pet_free_roam,
                sleep_enabled: self.cfg.pet_sleep,
            };
            let r = pet::draw(
                ctx,
                &self.theme,
                &input,
                &mut self.pet_pos,
                self.pet_tex.as_ref(),
                &mut self.pet_rt,
            );
            // ペットの矩形は egui が Area の実測として持っている。
            // 大きさを app.rs 側で決め打ちしない (pet.rs の寸法と二重管理にしない)。
            if let Some(rect) = ctx.memory(|m| m.area_rect(egui::Id::new("zv-pet"))) {
                tutorial::anchor(ctx, AnchorId::Pet, rect);
            }
            if r.drag_released {
                // ドラッグ後の位置を保存する
                if let Some(p) = self.pet_pos {
                    self.cfg.pet_x = Some(p.x);
                    self.cfg.pet_y = Some(p.y);
                    config::save_state(&self.cfg);
                }
            }
            // ダブルクリックのご機嫌ホップに合わせて効果音を鳴らす
            if r.double_clicked && self.cfg.pet_sounds {
                self.sound.play(SoundKind::Confirm);
            }
            // クリック(ドラッグでない)のときだけアクション
            if r.clicked && !r.dragged {
                if let Some(i) = self
                    .agents
                    .sessions
                    .iter()
                    .position(|s| s.attention && s.running())
                {
                    self.apply_cmd(Cmd::FocusAgent(i), ctx);
                } else {
                    self.cockpit = !self.cockpit;
                }
            }

            // 承認待ちの吹き出し(ペットより後に描いて頭上に重ねる)
            if self.cfg.pet_bubbles {
                // 自動YES (`pet_auto_yes`) がオンの時、ペットからの承認が保留のまま 30秒以上経過していたら
                // 「✔ 承認」ボタンを押したことと全く同じ動作を自動実行する。
                if self.cfg.pet_auto_yes {
                    let auto_approve_indices: Vec<usize> = self
                        .agents
                        .sessions
                        .iter()
                        .enumerate()
                        .filter_map(|(i, s)| {
                            if s.attention
                                && s.running()
                                && !self.pet_bubble_dismissed.contains(&s.id)
                                && !self.pet_bubble_answered.contains_key(&s.id)
                            {
                                if let Some(since) = s.attention_since {
                                    if since.elapsed().as_secs() >= 30 {
                                        return Some(i);
                                    }
                                }
                            }
                            None
                        })
                        .collect();

                    for i in auto_approve_indices {
                        let fallback = self.cfg.pet_approve_keys.clone();
                        let sent = self.agents.sessions.get_mut(i).map(|s| {
                            let ok = s.press_pet_approve_button(Some(&fallback));
                            (ok, s.title.clone(), s.id)
                        });
                        if let Some((true, title, id)) = sent {
                            self.pet_bubble_answered.insert(id, Instant::now());
                            self.toast(
                                trf(
                                    "⚡ (自動YES: 30秒保留経過) ✔ 承認を自動送信: {title}",
                                    &[("title", title)],
                                ),
                                true,
                            );
                        }
                    }
                }

                let items: Vec<pet_bubble::BubbleItem> = self
                    .agents
                    .sessions
                    .iter()
                    .enumerate()
                    .filter(|(_, s)| {
                        // 却下済み・応答直後(3秒以内)のセッションは出さない
                        s.attention
                            && s.running()
                            && !self.pet_bubble_dismissed.contains(&s.id)
                            && !self.pet_bubble_answered.contains_key(&s.id)
                    })
                    .map(|(i, s)| pet_bubble::BubbleItem {
                        session_idx: i,
                        key: s.id,
                        icon: if s.icon.is_empty() {
                            "👾".into()
                        } else {
                            s.icon.clone()
                        },
                        title: s.title.clone(),
                    })
                    .collect();
                for act in pet_bubble::draw(ctx, &self.theme, &items, r.bubble_anchor) {
                    match act {
                        pet_bubble::BubbleAction::Approve(i) => {
                            let fallback = self.cfg.pet_approve_keys.clone();
                            let sent = self.agents.sessions.get_mut(i).map(|s| {
                                // 画面のプロンプトに合った承認キーを優先する
                                // (Bypass 警告は Enter だと「No, exit」になるため)
                                let ok = s.press_pet_approve_button(Some(&fallback));
                                (ok, s.title.clone(), s.id)
                            });
                            if let Some((true, title, id)) = sent {
                                self.pet_bubble_answered.insert(id, Instant::now());
                                self.toast(trf("✔ 承認を送信: {title}", &[("title", title)]), true);
                            }
                        }
                        pet_bubble::BubbleAction::Deny(i) => {
                            let keys = self.cfg.pet_deny_keys.clone();
                            let sent = self.agents.sessions.get_mut(i).map(|s| {
                                let ok = s.send_text(&keys);
                                if ok {
                                    s.resolve_attention();
                                }
                                (ok, s.title.clone(), s.id)
                            });
                            if let Some((true, title, id)) = sent {
                                self.pet_bubble_answered.insert(id, Instant::now());
                                self.toast(trf("✖ 拒否を送信: {title}", &[("title", title)]), true);
                            }
                        }
                        pet_bubble::BubbleAction::Focus(i) => {
                            self.apply_cmd(Cmd::FocusAgent(i), ctx);
                        }
                        pet_bubble::BubbleAction::Dismiss(i) => {
                            // index を安定 id に変換して記録する(index は次フレームでずれ得る)
                            if let Some(s) = self.agents.sessions.get(i) {
                                self.pet_bubble_dismissed.insert(s.id);
                            }
                        }
                    }
                }
            }
        }

        // ── 初回起動ガイドツアー ──
        // **すべての UI を描き終わったあと**に 1 回だけ。アンカーの申告は
        // 上のパネル描画で済んでいるので、ここで初めて全部が揃う。
        //
        // アイドル CPU との関係: `Tutorial::overlay` は**表示中だけ** 30fps を
        // 自前で予約する。その予約は直後の `schedule_idle_repaint` から
        // `IdleSignals::animating` として見えるので、二重予約にはならない
        // (`idle_repaint_ms` は animating のとき `None` を返す)。
        // 非表示のときは 1 本も予約しないので、完全アイドルの 0fps は保たれる。
        self.tutorial_tick(ctx);

        // 今フレームのズームジェスチャの持ち主を確定させる。描かなかった
        // フレームでは None になり、看板や画像タブに切り替えたあとも
        // 古い矩形で「ファイル単位ズーム」に流れることがない。
        self.zoom_area = self.zoom_area_next.take();

        // フレームの最後に、次の定期フレームだけをまとめて予約する。
        // ここより上で誰かが予約していれば egui は最短を採るので、
        // このポリシーは「他に誰も予約していないときの下限」を決める役になる。
        self.schedule_idle_repaint(ctx);
    }

    /// ガイドツアーの 1 フレーム: 初回だけ自動開始し、オーバーレイを描き、
    /// 返ってきた依頼を自分の状態で実行する。
    pub(super) fn tutorial_tick(&mut self, ctx: &egui::Context) {
        // 自動開始は**一度だけ**。ここで撃つのは Context が要るため
        // (`ZaivernApp::new` の時点ではフレームが始まっていない)。
        if !self.tutorial_autostarted {
            self.tutorial_autostarted = true;
            if self.tutorial.autostart() {
                // 開始フレームは必ず 1 枚描く (アイドルからでも立ち上がる)
                crate::perf::repaint(ctx, "tutorial_tick");
            }
        }
        let theme = self.theme.clone();
        if let Some(act) = self.tutorial.overlay(ctx, &theme, &self.keys) {
            self.apply_tutorial_action(act, ctx);
        }
    }

    /// ACP パネルを開閉する (`crate::acp::FEATURE` から呼ばれる)。
    ///
    /// **オーバーレイ**なので中央ビューを奪わない (起動しただけで画面が激変しない)。
    pub fn toggle_acp_panel(&mut self) {
        self.acp.open = !self.acp.open;
        if self.acp.open {
            self.toast(
                tr("🛰 ACP: 構造化プロトコルでエージェントを駆動します"),
                true,
            );
        }
    }

    /// ACP (構造化プロトコル) の 1 フレーム。
    ///
    /// 受信の畳み込みとオーバーレイの描画をここ 1 か所へ集める。接続が
    /// 0 本のときは**何も起きない** (設計原則 3: アイドルのコストはゼロ)。
    pub fn acp_tick(&mut self, ctx: &egui::Context) {
        if self.acp.is_empty() && !self.acp.open {
            return;
        }
        // 未保存のエディタバッファを公開する。`fs/read_text_file` が
        // ディスクではなく「いま画面に見えている内容」を返せる = エディタを
        // 持つクライアントだけの強み。署名が変わらなければ本文は読まない。
        self.acp.sync_unsaved(
            self.editor
                .buffers
                .iter()
                .filter(|b| b.dirty())
                .filter_map(|b| {
                    b.path
                        .as_deref()
                        .map(|p| (p, b.history.revision(), b.text.as_str()))
                }),
        );
        let theme = self.theme.clone();
        let cwd = self.agent_cwd();
        let toasts = self.acp.frame(
            ctx,
            &theme,
            &mut self.agents.approvals,
            &self.roots.clone(),
            &cwd,
        );
        for (msg, ok) in toasts {
            if ok {
                self.toast(msg, true);
            } else {
                self.toast_warn(msg);
            }
        }
    }

    /// ツアーからの「これを開いておいて」を実行する。
    ///
    /// 実行できなくても構わない設計 (アンカーが現れなければツアー側が
    /// 数秒で自動的に次へ送る) なので、ここでは失敗を握り潰さず**必ず試す**。
    pub(super) fn apply_tutorial_action(
        &mut self,
        act: tutorial::TutorialAction,
        ctx: &egui::Context,
    ) {
        use tutorial::TutorialAction as TA;
        match act {
            TA::OpenSidebar(t) => {
                self.sidebar_open = true;
                self.sidebar_tab = sidebar_tab_for(t);
            }
            TA::ShowTerminalPanel => {
                self.agents.panel_open = true;
                self.cockpit = false;
                self.kanban = false;
                self.approvals_view = false;
                self.mcp_view = false;
                self.skills_view = false;
                self.spec_view = false;
            }
            TA::ShowCockpit => {
                self.cockpit = true;
                self.kanban = false;
            }
            TA::ShowKanban => {
                self.kanban = true;
                self.cockpit = false;
                self.deck = false;
                self.approvals_view = false;
                self.mcp_view = false;
                self.skills_view = false;
                self.spec_view = false;
            }
            TA::ShowDeck => {
                self.deck = true;
                self.cockpit = false;
                self.kanban = false;
                self.agents.panel_open = true;
                self.approvals_view = false;
                self.mcp_view = false;
                self.skills_view = false;
                self.spec_view = false;
            }
            TA::OpenPalette => self.palette.open_commands(),
            TA::OpenRaceForm => {
                self.cockpit = true;
                self.kanban = false;
                self.race.form_open = true;
            }
            TA::ShowRemoteQr => {
                // 切替ではなく「開く」。既に開いていたら閉じてしまうと、
                // 説明しようとした画面が消えて手順が空振りする。
                if !self.remote_open {
                    self.apply_cmd(Cmd::ToggleRemote, ctx);
                }
            }
        }
        // 依頼を実行した結果を次のフレームで見せる (アイドルでも 1 枚回す)
        crate::perf::repaint(ctx, "apply_tutorial_action");
    }

    /// いまの状態から [`idle_repaint_ms`] の材料を組み立てて予約する。
    ///
    /// 「別スレッドが結果を届けたら起こす」経路 (PTY リーダ / LSP / git /
    /// リモート HTTP / プラグイン / gh / 音声 / 見張り) は各所で
    /// `request_repaint` を撃っているので、ここでは数えない。
    /// 逆に**自前で起こさない**待ち (OS ファイルダイアログ・ファイアウォール
    /// 操作・横断検索・定義ジャンプ) は `awaiting` として拾う。
    pub(super) fn schedule_idle_repaint(&self, ctx: &egui::Context) {
        use plugins::PluginList;
        let (focused, minimized, had_input) = ctx.input(|i| {
            let v = i.viewport();
            (
                v.focused.unwrap_or(true),
                v.minimized.unwrap_or(false),
                !i.events.is_empty() || i.pointer.is_moving(),
            )
        });
        let awaiting = self.dialogs.busy()
            || self.fw.busy().is_some()
            || self.gsearch.rx.is_some()
            || self.awaiting_definition.is_some();
        // 外部での書き換えを見張る対象: 開いているフォルダか、
        // ディスク上のファイルに紐付いたタブ (`check_external_changes` の対象)
        let watching_files =
            !self.roots.is_empty() || self.editor.buffers.iter().any(|b| b.path.is_some());
        let timers_due = self.menu_state.auto_save
            || !self
                .plugins
                .active_hooks(plugins::HookEvent::Interval)
                .is_empty();
        let signals = IdleSignals {
            had_input,
            animating: ctx.has_requested_repaint(),
            awaiting,
            agents_running: self.agents.running_count() > 0,
            watching_files,
            timers_due,
            focused,
            visible: !minimized,
        };
        if let Some(ms) = idle_repaint_ms(signals) {
            crate::perf::repaint_after(
                ctx,
                std::time::Duration::from_millis(ms),
                "schedule_idle_repaint",
            );
        }
        // 「実入力が無いのに描いたフレーム」を数える。
        // `ZAIVERN_PERF=1` のときだけ働き、レポートは `perf::dump` で 1 回だけ出す
        // (1 フレームごとに文字列を吐くと計測自体が観測対象を歪めるため)。
        //
        // **エージェントが走っているフレームはアイドルではない。** これを入れずに
        // 入力の有無だけで数えると、エージェントの出力で正当に再描画している
        // フレームまで「アイドルなのに描いた」に混ざり、設計原則 3 の数字が
        // 実際より悪く出る (= 直す必要のないものを追いかける)。
        crate::perf::note_idle(!signals.had_input && !signals.agents_running);
    }
}
