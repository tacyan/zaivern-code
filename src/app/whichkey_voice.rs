use super::*;

impl ZaivernApp {
    /// which-key ポップアップ。chord の 1 打鍵目を握っている間だけ、
    /// そこから続く打鍵の一覧を中央ビューの右下に出す。
    ///
    /// 以前はステータスバーへ「⌘K が押されました。待機中…」と 1 行出すだけで、
    /// **次に何を押せるのかは画面のどこにも無かった**。中身は
    /// [`crate::whichkey`] に閉じてあり、ここは材料を渡して結果を捌くだけ。
    pub(super) fn whichkey_ui(&mut self, ctx: &egui::Context) {
        use crate::whichkey;
        // 実データの行 (which-key.nvim の content plugin 相当)。
        // 差分ファイル間移動の prefix を握っている間だけ作る。
        let live = self.whichkey_live_rows();
        let mut st = std::mem::take(&mut self.whichkey);
        let out = whichkey::popup_ui(
            ctx,
            &mut st,
            whichkey::Params {
                pending: self.chord.pending(),
                keys: &self.keys,
                theme: &self.theme,
                live: &live,
                first_delay: whichkey::first_delay(self.cfg.whichkey_delay_ms),
                area: ctx.available_rect(),
                bottom_inset: 8.0,
            },
        );
        self.whichkey = st;
        self.apply_whichkey(out, ctx);
    }

    /// which-key が拾う打鍵をフレームの頭で取る (本文の TextEdit より先)。
    /// 握っていないフレームでは**イベントに一切触らない**。
    pub(super) fn whichkey_keys(&mut self, ctx: &egui::Context) {
        // このフレームの実データ行をここで 1 回だけ確定させる。描画側も同じ
        // ものを見るので、行番号と実体がずれない。
        self.whichkey_live = self.scan_whichkey_live();
        // 変換中の生キーは IME のものであってアプリのものではない。判定は
        // `handle_shortcuts` が直前に入れた値を読む (2 か所で判定しない)。
        if !self.chord.is_waiting()
            || self.chord.ime_active()
            || self.keybind_ui.recording.is_some()
        {
            return;
        }
        if let Some(out) = crate::whichkey::take_keys(ctx, self.whichkey_live.len()) {
            self.apply_whichkey(out, ctx);
        }
    }

    /// which-key の操作を捌く。打鍵経路と描画経路で同じ処理を通す。
    pub(super) fn apply_whichkey(&mut self, out: crate::whichkey::Outcome, ctx: &egui::Context) {
        use crate::whichkey::Outcome;
        match out {
            Outcome::None => {}
            // 1 打鍵戻す。いまの chord は 2 打鍵までなので、戻り切ったら待機ごと捨てる。
            Outcome::Pop => {
                if !self.whichkey.pop() || !self.whichkey.is_active() {
                    self.chord.clear();
                    self.whichkey.clear();
                }
                crate::perf::repaint(ctx, "whichkey");
            }
            // 検索できる全ショートカット一覧へ抜ける (2 つ目の一覧は作らない)
            Outcome::OpenAll => {
                self.chord.clear();
                self.whichkey.clear();
                self.shortcuts_open = true;
            }
            Outcome::Pick(i) => {
                let path = self.whichkey_live.get(i).map(|(p, _, _)| p.clone());
                self.chord.clear();
                self.whichkey.clear();
                if let Some(p) = path {
                    self.apply_cmd(Cmd::OpenRecentFile(p), ctx);
                }
            }
        }
    }

    /// which-key に出す実データの行。**差分ファイル間移動 (`]f` / `[f`) の
    /// prefix を握っている間だけ**、いま変更のあるファイルを並べる。
    ///
    /// `]f` は「次の変更ファイルへ」を目隠しで撃つ打鍵だが、握っているだけで
    /// 行き先が見えて番号で直接飛べる。判定は打鍵をベタ書きせず
    /// **キーバインド表から引く** (再割り当てされても付いてくる)。
    pub(super) fn whichkey_diff_prefix_held(&self) -> bool {
        let Some(sc) = self.chord.pending() else {
            return false;
        };
        [BindAction::DiffNextFile, BindAction::DiffPrevFile]
            .into_iter()
            .any(|a| crate::keybinds::same_stroke(self.keys.binding(a).first(), sc))
    }

    /// 実データ行の実体を作る (フレームに 1 回。`git` は 1 回も起動しない)。
    pub(super) fn scan_whichkey_live(&self) -> Vec<(PathBuf, String, crate::git::FileStatus)> {
        if !self.whichkey_diff_prefix_held() {
            return Vec::new();
        }
        self.gitinfo.changed_paths(crate::whichkey::MAX_LIVE_ROWS)
    }

    /// 実体を画面の行へ写す。`self.whichkey_live` と 1 対 1 で並ぶ。
    pub(super) fn whichkey_live_rows(&self) -> Vec<crate::whichkey::LiveRow> {
        self.whichkey_live
            .iter()
            .map(|(_, rel, st)| {
                let (_, mark, _) = crate::file_tree::git_status_style(*st, &self.theme);
                crate::whichkey::LiveRow {
                    desc: format!("{mark}  {rel}"),
                    detail: rel.clone(),
                }
            })
            .collect()
    }

    pub(super) fn toasts_ui(&mut self, ctx: &egui::Context) {
        self.toasts.retain(|t| t.at.elapsed().as_secs_f32() < 4.2);
        if self.toasts.is_empty() {
            return;
        }
        let theme = self.theme.clone();
        egui::Area::new(egui::Id::new("zv-toasts"))
            .order(egui::Order::Foreground)
            .anchor(Align2::RIGHT_BOTTOM, egui::vec2(-14.0, -76.0))
            .show(ctx, |ui| {
                for t in &self.toasts {
                    let color = match t.kind {
                        0 => theme.ok,
                        1 => theme.warn,
                        _ => theme.err,
                    };
                    egui::Frame::none()
                        .fill(theme.panel)
                        .stroke(egui::Stroke::new(1.0_f32, color.gamma_multiply(0.7)))
                        .rounding(egui::Rounding::same(8.0))
                        .inner_margin(egui::Margin::symmetric(12.0, 8.0))
                        .show(ui, |ui| {
                            ui.label(RichText::new(&t.msg).color(theme.text));
                        });
                }
            });
        crate::perf::repaint_after(ctx, std::time::Duration::from_millis(300), "toasts_ui");
    }

    // ─── スマホリモート ─────────────────────────────────────────────

    // ─── 音声入力 (Zaivern 内で完結) ────────────────────────────────

    /// 音声入力を開始する。⏹ を押すまで録音し続ける。
    pub(super) fn start_voice(&mut self, target: voice::Target, ctx: &egui::Context) {
        if self.voice.session.is_some() {
            return;
        }
        if self.agents.running_count() == 0 {
            self.toast_warn(tr(
                "音声入力の宛先がありません — 先にエージェントを起動してください",
            ));
            return;
        }
        // ブラウザ経路は子プロセスを持たない — /voice をブラウザで開いて、
        // 認識結果は Web Speech API から /api/voice 経由で戻ってくる。
        if voice::resolve_engine(
            &self.cfg.voice_engine,
            &self.cfg.voice_lang,
            &self.cfg.voice_command,
        ) == "browser"
        {
            self.open_voice_page();
            return;
        }
        match voice::start(
            &self.cfg.voice_engine,
            &self.cfg.voice_lang,
            &self.cfg.voice_command,
            ctx,
        ) {
            Ok(s) => {
                self.voice = VoiceState {
                    session: Some(s),
                    target,
                    ..Default::default()
                };
                if self.cfg.pet_sounds {
                    self.sound.play(SoundKind::Confirm);
                }
            }
            Err(e) => {
                self.voice = VoiceState::default();
                self.toast(format!("🎤 {e}"), false);
            }
        }
    }

    /// ブラウザの音声入力ページ (`/voice`) を開く。
    ///
    /// `http://127.0.0.1:PORT` は W3C の Secure Contexts 上「信頼できるオリジン」
    /// なので、TLS 無しでも Web Speech API が動く。マイクはブラウザ側なので
    /// Zaivern 内に録音プロセスは立たない (⏹ も出ない — 閉じれば止まる)。
    pub(super) fn open_voice_page(&mut self) {
        let Some(r) = self.remote.as_ref() else {
            self.toast(
                tr(
                    "🎤 ブラウザの音声入力ページを開けません — スマホリモートが起動していません\
                     (config.toml の voice_command に外部コマンドを設定する手もあります)",
                ),
                false,
            );
            return;
        };
        let url = format!("http://127.0.0.1:{}/voice?t={}", r.port, r.token);
        // Edge の webkitSpeechRecognition は不安定なので Chrome があればそちらを使う。
        // どちらで開いたかは必ず伝える (黙って既定ブラウザに投げない)。
        let browser = match chrome_path() {
            Some(p) => {
                let _ = std::process::Command::new(p).arg(&url).spawn();
                "Chrome".to_string()
            }
            None => {
                open_external(&url);
                tr("既定のブラウザ")
            }
        };
        self.toast(
            trf(
                "🎤 {browser} で音声入力ページを開きました — これから先はそちらのマイクが 🎤 です\
                 (認識テキストは入力欄に入るだけ。送信は自分で Enter)",
                &[("browser", browser)],
            ),
            true,
        );
    }

    /// 録音を止める。認識プロセスは最後の確定テキストを返してから終了するので、
    /// ここでは kill せず `stopping_at` を立てて確定を待つ。
    pub(super) fn stop_voice(&mut self) {
        if let Some(s) = self.voice.session.as_mut() {
            s.stop();
            if self.voice.stopping_at.is_none() {
                self.voice.stopping_at = Some(Instant::now());
            }
        }
    }

    /// 音声入力の主処理。毎フレーム呼ぶ。
    pub(super) fn poll_voice(&mut self, ctx: &egui::Context) {
        let events = match self.voice.session.as_ref() {
            Some(s) => s.poll(),
            None => return,
        };
        let mut ended = false;
        for ev in events {
            match ev {
                voice::Event::Ready => {
                    self.voice.ready = true;
                }
                // 途中経過も確定も同じ経路で入力欄へ流す。違いは、確定した分は
                // もう書き換えないので追跡をやめる (= 次のひとことが後ろへ続く) 点だけ。
                voice::Event::Partial(t) => {
                    self.voice.partial = t.clone();
                    self.apply_voice_text(&t, false);
                }
                voice::Event::Final(t) => {
                    self.voice.partial.clear();
                    self.apply_voice_text(&t, true);
                }
                voice::Event::Error(e) => {
                    self.toast(format!("🎤 {e}"), false);
                    ended = true;
                }
                voice::Event::Warning(e) => {
                    // stderr のノイズ等。見せるだけで録音は続ける
                    self.toast(format!("🎤 {e}"), false);
                }
                voice::Event::Ended => ended = true,
            }
        }

        // 停止要求から一定時間たっても終わらないプロセスは打ち切る
        let timed_out = self
            .voice
            .stopping_at
            .is_some_and(|at| at.elapsed() > Duration::from_secs(5));
        if ended || timed_out {
            if let Some(mut s) = self.voice.session.take() {
                s.kill();
            }
            self.voice = VoiceState::default();
        } else {
            // 録音中は HUD を動かし続ける
            crate::perf::repaint_after(ctx, Duration::from_millis(120), "poll_voice");
        }
    }

    /// 認識テキストを対象セッションの入力欄へ流し込む。
    ///
    /// 確定を待たずに、話している途中 (`is_final == false`) の文字もそのまま
    /// 入力欄へ書き込む。喋りが進んで変換が変わると前に書いた文字列は書き換わるので、
    /// **前回書いた分と食い違うところだけ Backspace で消してから続きを送る**。
    /// これで入力欄が二重になったり、消し残しが出たりしない。
    ///
    /// **Enter は送らない**。ユーザーが内容を見て自分で Enter を押すまで
    /// エージェントへは送信されない。設定した合図キーワードを話したときだけ、
    /// キーワードを取り除いたうえで Enter まで送る。合図の判定は確定したときだけ
    /// 行う (途中経過で誤爆させない)。
    pub(super) fn apply_voice_text(&mut self, text: &str, is_final: bool) {
        let text = text.trim();
        if text.is_empty() {
            return;
        }
        let kw = self.cfg.voice_keyword.trim().to_string();
        let (body, submit) = match is_final && !kw.is_empty() {
            false => (text.to_string(), false),
            true => match strip_trailing_keyword(text, &kw) {
                Some(rest) => (rest, true),
                None => (text.to_string(), false),
            },
        };
        let body = body.trim().to_string();
        if body.is_empty() && !submit {
            return;
        }

        // 宛先が変わったら、前の入力欄に書いた文字はそのまま残して書き出しからやり直す
        // (別のセッションへ Backspace を送り込んでしまわないように)。
        let dest = self.resolve_voice_target();
        let key = dest.unwrap_or(u64::MAX);
        if self.voice.last_sent_to.is_some_and(|k| k != key) {
            self.voice.live.clear();
            self.voice.last_char = None;
        }

        // 録音中に人が手で打った (Enter で送った・自分で消した) なら、覚えている
        // 書き込み内容はもう当てにならない。Backspace を送り込まず書き出しから
        // やり直す — Enter で入力欄が空になったあとも、そのまま話し続けられる。
        let typed = match dest {
            Some(id) => self.take_typed_voice(id),
            None => {
                let ids: Vec<u64> = self.agents.sessions.iter().map(|s| s.id).collect();
                // 全セッションの typed フラグを消費する必要があるので
                // `any` (短絡評価) には書き換えないこと。
                let mut any_typed = false;
                for id in ids {
                    any_typed |= self.take_typed_voice(id);
                }
                any_typed
            }
        };
        if typed {
            self.voice.reset_live();
        }

        let edit = self.voice.plan(&body, key);
        // 同じ途中経過がもう一度届いただけなら端末へ何も送らない。
        // ただし確定と送信は、送るバイトが無くても追跡の締めが要るので通す。
        if edit.is_noop() && !submit && !is_final {
            return;
        }
        let out = edit.bytes(submit);

        let sent = match dest {
            Some(id) => match self.agents.sessions.iter_mut().find(|s| s.id == id) {
                Some(s) if s.running() => {
                    // 音声もユーザーの応答扱い。ただし user_typed を立てると
                    // 音声側が自分の書き込みを「手入力」と誤認して live 追跡を
                    // 捨ててしまうため、承認エピソードの解決だけを行う。
                    s.resolve_attention();
                    s.write_bytes(&out);
                    Some(s.title.clone())
                }
                _ => None,
            },
            None if self.voice.target == voice::Target::Broadcast => {
                let n = self.agents.running_count();
                if n == 0 {
                    None
                } else {
                    // ブロードキャストは Enter 込みの broadcast() を使わず、
                    // 書き込みのみ / 送信ありを自分で選ぶ
                    for s in self.agents.sessions.iter_mut().filter(|s| s.running()) {
                        s.resolve_attention();
                        s.write_bytes(&out);
                    }
                    Some(trf("{n} セッション", &[("n", n.to_string())]))
                }
            }
            None => None,
        };

        let Some(where_) = sent else {
            self.toast_warn(tr("音声入力の宛先セッションが見つかりません"));
            return;
        };
        self.voice.commit(edit, is_final, submit, key);
        if submit {
            self.toast(
                trf(
                    "🎤▶ {where} へ送信: {body}",
                    &[("where", where_), ("body", body.to_string())],
                ),
                true,
            );
        }
    }

    /// いま文字を届けるべきセッション id。ブロードキャストなら None。
    /// `Active` は毎回引き直すので、録音中にタブを切り替えれば宛先も移る。
    pub(super) fn resolve_voice_target(&self) -> Option<u64> {
        match self.voice.target {
            voice::Target::Broadcast => None,
            voice::Target::Active => self.agents.sessions.get(self.agents.active).map(|s| s.id),
            voice::Target::Session(id) => Some(id),
        }
    }

    /// 録音中に画面上部へ出すパネル。認識中の文字・届け先の切替・⏹ 停止を持つ。
    pub(super) fn voice_hud(&mut self, ctx: &egui::Context) {
        if self.voice.session.is_none() {
            return;
        }
        let theme = self.theme.clone();
        let stopping = self.voice.stopping_at.is_some();
        let head = if stopping {
            tr("🎤 最後のひとことを待っています…")
        } else if self.voice.ready {
            let dots = (self.voice.partial.len() % 3) + 1;
            trf("🔴 録音中{dots}", &[("dots", "・".repeat(dots))])
        } else {
            tr("🎤 マイクを準備しています…")
        };
        let target_label = self.voice_target_label();
        let mut stop = false;
        let mut set_target: Option<voice::Target> = None;

        egui::Area::new(egui::Id::new("zv-voice-hud"))
            .anchor(Align2::CENTER_TOP, egui::vec2(0.0, 56.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(
                        1.5_f32,
                        if stopping { theme.accent } else { theme.err },
                    ))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::symmetric(16.0, 10.0))
                    .show(ui, |ui| {
                        ui.set_max_width(600.0);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(head).strong().color(theme.text));
                            ui.label(
                                RichText::new(format!("→ {target_label}")).color(theme.text_dim),
                            );
                            ui.with_layout(
                                egui::Layout::right_to_left(egui::Align::Center),
                                |ui| {
                                    if ui
                                        .button(RichText::new(tr("⏹ 停止")).strong())
                                        .on_hover_text(tr("録音をやめます"))
                                        .clicked()
                                    {
                                        stop = true;
                                    }
                                },
                            );
                        });
                        // 録音したまま届け先を切り替えられる
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(tr("届け先:")).size(11.0).color(theme.text_dim));
                            for (t, label) in [
                                (voice::Target::Active, "🎯 アクティブなエージェント"),
                                (voice::Target::Broadcast, "📣 全エージェント"),
                            ] {
                                let sel = self.voice.target == t;
                                if ui.selectable_label(sel, RichText::new(tr(label)).size(11.5)).clicked()
                                    && !sel
                                {
                                    set_target = Some(t);
                                }
                            }
                        });
                        if !self.voice.partial.is_empty() {
                            ui.label(RichText::new(&self.voice.partial).color(theme.accent));
                        }
                        ui.label(
                            RichText::new(tr(
                                "話しながらリアルタイムで入力欄へ書き込まれます。送信は自分で Enter を押したときだけ。\n\
                                 Enter で空になっても録音は続いているので、そのまま話し続けられます",
                            ))
                            .size(11.0)
                            .color(theme.text_dim),
                        );
                    });
            });

        if let Some(t) = set_target {
            self.voice.target = t;
            // 宛先が変わったら、前の入力欄の追跡を捨てて書き出しからやり直す
            self.voice.last_sent_to = None;
            self.voice.reset_live();
            if t != voice::Target::Session(0) {
                self.cfg.voice_target = t.name().to_string();
                config::save_state(&self.cfg);
            }
        }
        if stop {
            self.stop_voice();
        }
    }

    /// 届け先の表示名。
    pub(super) fn voice_target_label(&self) -> String {
        match self.voice.target {
            voice::Target::Broadcast => trf(
                "📣 全エージェント ({n})",
                &[("n", self.agents.running_count().to_string())],
            ),
            voice::Target::Active | voice::Target::Session(_) => {
                match self.resolve_voice_target() {
                    Some(id) => self
                        .agents
                        .sessions
                        .iter()
                        .find(|s| s.id == id)
                        .map(|s| format!("{} {}", s.icon, s.title))
                        .unwrap_or_else(|| tr("(見つかりません)")),
                    None => tr("(エージェントがいません)"),
                }
            }
        }
    }
}
