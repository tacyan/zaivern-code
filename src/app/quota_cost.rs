use super::*;

impl ZaivernApp {
    /// プラン使用量を 1 フレームぶん進める。
    ///
    /// 走行本数を渡してから [`coordinator::QuotaWatch::refresh_if_stale`] を呼ぶ。
    /// 読み取りは TTL 付きでバックグラウンドスレッドへ逃げるので、
    /// UI スレッドはこのフレームでファイルにもネットワークにも触らない。
    pub(super) fn quota_tick(&mut self) {
        // bin 名 → いま走っている本数。素のシェルは枠を食わないので数えない
        let mut by_bin: Vec<(String, usize)> = Vec::new();
        for s in self.agents.sessions.iter().filter(|s| s.running()) {
            let Some(spec) = crate::agents::spec_for_command(&s.command) else {
                continue;
            };
            match by_bin.iter_mut().find(|(b, _)| b == spec.bin) {
                Some((_, n)) => *n += 1,
                None => by_bin.push((spec.bin.to_string(), 1)),
            }
        }
        self.quota.set_running(by_bin);
        self.quota.refresh_if_stale();
        self.cost_limit_tick();
    }

    /// コスト上限の判定を進める。
    ///
    /// **集計か上限の設定が変わったときだけ**計算し直す。推定コストは
    /// エージェント × モデルぶんの掛け算なので、毎フレーム回すと
    /// 「アイドル時のコストはゼロ」(設計原則 3) が崩れる。
    /// 集計は [`coordinator::quota::TOKEN_TTL`] 間隔でしか更新されないので、
    /// 実際に走るのは 2 分に 1 回と、設定を触った直後だけ。
    pub(super) fn cost_limit_tick(&mut self) {
        let limits = self.cfg.cost_limits();
        let stamp = (self.quota.applied(), limits);
        if self.cost_stamp == Some(stamp) {
            return;
        }
        self.cost_stamp = Some(stamp);
        if !limits.any() {
            // 上限が未設定 = 見張らない。表示も通知も残さない。
            self.cost_alert = None;
            self.cost_spent = (0.0, 0.0);
            self.cost_gate.retain(&[]);
            return;
        }
        let prices = &self.cfg.pricing;
        let session = self.quota.cost_session(prices);
        let today = self.quota.cost_today(prices);
        self.cost_spent = (session, today);
        self.cost_alert = limits.worst(session, today);
        // 段が変わった瞬間だけ 1 度鳴らす (毎フレーム鳴らさない)。
        let key = coordinator::quota::budget_edge_key(self.cost_alert.as_ref());
        if !self.cost_gate.changed(Self::COST_GATE_ID, &key) || key.is_empty() {
            return;
        }
        let Some(st) = self.cost_alert.clone() else {
            return;
        };
        let body = self.cost_alert_message(&st);
        let title = match st.state {
            coordinator::quota::BudgetState::Over => tr("💸 コスト上限に達しました"),
            _ => tr("💰 コスト上限に近づいています"),
        };
        // 画面 (トースト) と OS 通知の両方。レイアウトは動かさない。
        match st.state {
            coordinator::quota::BudgetState::Over => self.toast(body.clone(), false),
            _ => self.toast_warn(body.clone()),
        }
        notify::notify(&title, &body);
        if !self.cfg.webhook_url.trim().is_empty() {
            notify::webhook(&self.cfg.webhook_url, &title, &body);
        }
    }

    /// コスト上限の通知は 1 つしか無いので、門番の鍵も 1 つ固定で使う
    /// ([`notify::EdgeGate`] はセッション ID で引く作りなので、
    ///  セッションの ID 空間と衝突しない番号を使う)。
    pub(super) const COST_GATE_ID: u64 = u64::MAX;

    /// 上限の状態を 1 行の日本語にする (通貨記号は設定から)。
    pub(super) fn cost_alert_message(&self, st: &coordinator::quota::BudgetStatus) -> String {
        let amount = st.short_label(&self.cfg.pricing.currency);
        let scope = st.kind.label();
        match st.state {
            coordinator::quota::BudgetState::Over => trf(
                "{scope}の推定コストが上限に達しました ({amount})",
                &[("scope", scope), ("amount", amount)],
            ),
            _ => trf(
                "{scope}の推定コストが上限の {pct}% に達しました ({amount})",
                &[
                    ("scope", scope),
                    ("pct", ((st.fraction() * 100.0).round() as i64).to_string()),
                    ("amount", amount),
                ],
            ),
        }
    }

    /// コスト上限で新規の送信を止めるべきなら、その理由を返す。
    ///
    /// **`stop` を選んでいて、かつ上限に達しているときだけ。** 既定の
    /// `notify` では常に `None` = 何も止めない。判定そのものは
    /// [`coordinator::quota::CostLimits::blocks`] が持つ (規則を二重に書かない)。
    pub(super) fn cost_block_reason(&self) -> Option<String> {
        let (session, today) = self.cost_spent;
        let blocked = self.cfg.cost_limits().blocks(session, today)?;
        Some(trf(
            "⛔ {reason} — 設定の「上限に達したときの動作」を notify に戻すか、上限を上げると送れます",
            &[("reason", self.cost_alert_message(&blocked))],
        ))
    }

    /// このセッションが「レート制限だ」と言える根拠のうち、いちばん上の段。
    ///
    /// 設計原則 4 の降り方をそのまま実装している:
    /// 状態ファイル (ベンダーの実測値) → 画面スクレイプ (裏取り済みのときだけ)。
    /// 構造化プロトコル / ベンダーフックの段は、それを出す CLI が現れたらここへ足す。
    pub(super) fn failover_signal(&self, sid: u64, line: &str) -> Option<failover::Signal> {
        let Some(s) = self.agents.sessions.iter().find(|x| x.id == sid) else {
            return None;
        };
        let bin = crate::agents::spec_for_command(&s.command).map(|sp| sp.bin);
        let mut have: Vec<failover::Signal> = Vec::new();
        // 3 段目: ベンダーがローカルへ書いた使用率。実測なので最優先で採る。
        if let Some(bin) = bin {
            let exhausted = self.quota.snapshots().iter().any(|q| {
                q.agent == bin
                    && q.source == coordinator::quota::SourceKind::Vendor
                    && q.used_fraction.map(|u| u >= 0.99).unwrap_or(false)
            });
            if exhausted {
                have.push(failover::Signal::StateFile);
            }
        }
        // 4 段目: 画面。単語列一致 + 連続一致 + 出力が進んでいないことを裏取りする。
        if failover::confirm_screen(
            line,
            s.rate_limit_hits(),
            s.output_advanced(),
            self.cfg.failover.min_screen_hits,
        ) {
            have.push(failover::Signal::Screen);
        }
        failover::classify_signal(&have)
    }

    /// レート制限を検知したセッションを、別プロファイル / 別 CLI へ引き継ぐ。
    ///
    /// **現行セッションには一切触らない** (kill もしない)。新しいプリセットで
    /// 別セッションを立ち上げ、覚えているプロンプトを既存の遅延配達
    /// (`outbox` — 相手が落ち着いてから入れる) に載せるだけ。
    /// 切り替えたら true。
    pub(super) fn failover_on_rate_limit(
        &mut self,
        title: &str,
        line: &str,
        ctx: &egui::Context,
    ) -> bool {
        if !self.failover.enabled() {
            return false;
        }
        let Some((sid, preset_name, bin, env, carry)) = self
            .agents
            .sessions
            .iter()
            .find(|s| s.title == title)
            .and_then(|s| {
                let spec = crate::agents::spec_for_command(&s.command)?;
                Some((
                    s.id,
                    s.preset_name.clone(),
                    spec.bin.to_string(),
                    s.env.clone(),
                    s.last_prompt.clone(),
                ))
            })
        else {
            return false;
        };
        let Some(signal) = self.failover_signal(sid, line) else {
            // 根拠が足りない (画面に一瞬出ただけ等)。勝手に切り替えない。
            return false;
        };
        let now = Instant::now();
        self.failover.note_detected(sid, signal, line, now);

        let current = failover::FailingSession {
            session_id: sid,
            preset: preset_name.clone(),
            bin: bin.clone(),
            account: failover::account_key(&bin, &env),
            signal,
            switches: self.failover.switches_for(sid),
            tried: self.failover.tried_for(sid).to_vec(),
        };
        let candidates = failover::candidates_from_presets(
            &self.cfg.agents,
            self.failover.cooldowns(),
            self.failover.attempt_counts(),
            now,
        );
        let plan = match self.failover.plan(&current, &candidates, now) {
            Ok(p) => p,
            Err(why) => {
                self.toast_warn(trf(
                    "🔁 自動フェイルオーバー: 切り替えませんでした — {why}",
                    &[("why", why.label())],
                ));
                return false;
            }
        };
        // 枯れた枠は寝かせる (次の候補選定から外れ、時間が経てば復活する)。
        self.failover.note_failed(&current.account, now);

        match self.failover_launch(&plan, &env, carry, ctx) {
            Ok(new_id) => {
                self.failover
                    .note_switched(sid, &preset_name, &plan, new_id, signal, now);
                let msg = self
                    .failover
                    .records()
                    .last()
                    .map(|r| r.line())
                    .unwrap_or_default();
                self.toast_warn(format!("🔁 {msg}"));
                notify::notify("Zaivern Code", &msg);
                notify::webhook(&self.cfg.webhook_url, &tr("🔁 自動フェイルオーバー"), &msg);
                true
            }
            Err(e) => {
                // 起動に失敗した枠も寝かせる (同じ相手へ即座に殺到しない)。
                self.failover.note_failed(&plan.account, now);
                self.toast(
                    trf(
                        "🔁 {to} の起動に失敗しました: {e}",
                        &[("to", plan.preset.clone()), ("e", e)],
                    ),
                    false,
                );
                false
            }
        }
    }

    /// 切替先プリセットでセッションを起動し、覚えているプロンプトを引き継ぐ。
    /// 成功したら新しいセッション ID。
    pub(super) fn failover_launch(
        &mut self,
        plan: &failover::FailoverPlan,
        from_env: &HashMap<String, String>,
        carry: Option<String>,
        ctx: &egui::Context,
    ) -> Result<u64, String> {
        let Some(mut preset) = self
            .cfg
            .agents
            .iter()
            .find(|p| p.name == plan.preset)
            .cloned()
        else {
            return Err(tr("プリセットが見つかりません"));
        };
        // 会話の保存先が同じなら、既存の再開の仕組みをそのまま使う
        // (別 CLI / 別設定ディレクトリでは過去の会話が無いので付けない)。
        if let Some(spec) = crate::agents::spec_for_command(&preset.command) {
            if failover::can_resume(&plan.bin, from_env, spec.bin, &preset.env) {
                preset.command = crate::agents::apply_resume(&preset.command, spec);
            }
        }
        let approval = crate::agents::Approval::from_mode(&self.cfg.approval_mode);
        let cwd = self.agent_cwd();
        self.agents.launch(&preset, &cwd, approval, ctx)?;
        let new_id = self
            .agents
            .sessions
            .last()
            .map(|s| s.id)
            .ok_or_else(|| tr("起動したセッションが見つかりません"))?;
        if let Some(text) = carry.filter(|t| !t.trim().is_empty()) {
            // 既存の遅延配達に載せる: 相手が落ち着く (Idle) のを待ってから入れる。
            // 確定まで送る (引き継ぎは人の確認を挟まずそのまま続きをやらせる)。
            self.queue_submit(submit::Job::deferred(new_id, text.trim(), true));
        }
        Ok(new_id)
    }

    /// 切替後の段 (④再開 → ⑤検証 → 完了 / 打ち切り) を 1 フレームぶん進める。
    ///
    /// 「新しい側が本当に動いているか」を、画面ではなく**セッションの生死と
    /// レート制限フラグ**で見る (画面の文字列を読み直して推測しない)。
    pub(super) fn failover_tick(&mut self) {
        if self.failover.in_flight().is_empty() {
            return;
        }
        let now = Instant::now();
        for sid in self.failover.in_flight() {
            let Some(stage) = self.failover.stage_of(sid).cloned() else {
                continue;
            };
            match stage {
                failover::Stage::Resuming { session, .. } => {
                    // 引き継ぎ待ちが捌けたら検証へ。
                    if !self.outbox.iter().any(|p| p.job.session == session) {
                        self.failover.note_resumed(sid, now);
                    }
                }
                failover::Stage::Verifying { session, .. } => {
                    let alive = self
                        .agents
                        .sessions
                        .iter()
                        .find(|s| s.id == session)
                        .filter(|s| s.running());
                    match alive {
                        // 切替先まで上限に当たった: 枠を寝かせて打ち切る
                        // (ここから連鎖させると人が見ていない間に暴走する)。
                        Some(s) if s.rate_limited.is_some() => {
                            let account = crate::agents::spec_for_command(&s.command)
                                .map(|sp| failover::account_key(sp.bin, &s.env));
                            if let Some(a) = account {
                                self.failover.note_failed(&a, now);
                            }
                            self.failover.note_gave_up(
                                sid,
                                failover::Refusal::TargetAlsoLimited,
                                now,
                            );
                        }
                        Some(_) if self.failover.verify_elapsed(sid, now) => {
                            self.failover.note_verified(sid, now);
                        }
                        Some(_) => {}
                        // 立ち上がった直後に落ちた = 切替先が使えない。
                        None => {
                            self.failover
                                .note_gave_up(sid, failover::Refusal::TargetFailed, now)
                        }
                    }
                }
                _ => {}
            }
        }
    }

    /// 使用量の詳細ウィンドウ (ステータスバーのクリック / パレットから開く)。
    pub(super) fn quota_window_ui(&mut self, ctx: &egui::Context) {
        if !self.quota_open {
            return;
        }
        let theme = self.theme.clone();
        let now = std::time::SystemTime::now();
        let accounts = self.quota.accounts(now);
        let advice = self.quota.advice(now);
        // フェイルオーバーの表示材料はクロージャの外で作る (self の二重借用を避ける)。
        let fo_enabled = self.failover.enabled();
        let fo_max = self.failover.config().max_switches;
        let fo_stage = self.failover.active().map(|(_, s)| (s.step(), s.label()));
        let mono = Instant::now();
        let fo_recent: Vec<String> = self
            .failover
            .records()
            .iter()
            .rev()
            .take(3)
            .map(|r| {
                trf(
                    "{ago} 前: {line}",
                    &[("ago", fmt_ago(r.ago(mono))), ("line", r.line())],
                )
            })
            .collect();
        let fo_ladder = self.failover_ladder_text();
        let fo_next = self.failover_preview(mono);
        let mut toggle_failover = false;
        // トークン消費の明細 (エージェント → モデル)。self の二重借用を避けて先に作る。
        let token_rows = self.token_rows();
        // フレーム時間の計測。ZAIVERN_PERF=1 のときだけ Some。
        let perf_line = crate::perf::status_line();
        let mut perf_dump = false;
        let mut perf_reset = false;
        let mut open = self.quota_open;
        egui::Window::new(tr("📊 プラン使用量"))
            .open(&mut open)
            .resizable(true)
            .default_width(460.0)
            .show(ctx, |ui| {
                // ── 🔁 自動フェイルオーバー (既定は無効) ──────────────
                let mut on = fo_enabled;
                ui.horizontal_wrapped(|ui| {
                    if ui
                        .checkbox(&mut on, tr("🔁 レート制限で自動的に切り替える"))
                        .on_hover_text(format!(
                            "{}\n\n{}",
                            tr(
                                "上限に当たったら、同じ CLI の別プロファイル → 別 CLI の順で\n\
                                切替先を選び、新しいセッションを立てて続きを渡します。\n\
                                いま動いているセッションは残したまま (終了させません)。"
                            ),
                            fo_ladder,
                        ))
                        .changed()
                    {
                        toggle_failover = true;
                    }
                    ui.label(
                        RichText::new(trf(
                            "1 セッションあたり最大 {n} 回",
                            &[("n", fo_max.to_string())],
                        ))
                        .size(11.5)
                        .color(theme.text_dim),
                    );
                });
                // 「今どの段にいるか」を必ず出す (設計原則 4)。
                if let Some((step, label)) = &fo_stage {
                    ui.label(
                        RichText::new(format!("　{}/5  {label}", step))
                            .size(11.5)
                            .color(theme.warn),
                    );
                } else if on {
                    ui.label(
                        RichText::new(match &fo_next {
                            Some(to) => trf("　待機中 — 次の切替先: {to}", &[("to", to.clone())]),
                            None => tr("　待機中 — 使える切替先はいまありません"),
                        })
                        .size(11.5)
                        .color(theme.text_dim),
                    );
                }
                for line in &fo_recent {
                    ui.label(
                        RichText::new(format!("　• {line}"))
                            .size(11.5)
                            .color(theme.text_dim),
                    );
                }
                ui.separator();

                // ── 🪙 トークン消費と推定コスト ──────────────────────
                // 消費がゼロなら見出しごと出さない (空のセクションを作らない)。
                if !token_rows.is_empty() {
                    ui.label(
                        RichText::new(tr("🪙 トークン消費 (直近 24 時間)"))
                            .strong()
                            .size(12.5),
                    );
                    for row in &token_rows {
                        ui.horizontal_wrapped(|ui| {
                            ui.set_max_width(ui.available_width());
                            ui.label(RichText::new(&row.head).size(11.5).color(if row.top {
                                theme.warn
                            } else {
                                theme.text
                            }));
                        });
                        for sub in &row.subs {
                            ui.label(
                                RichText::new(format!("　　{sub}"))
                                    .size(11.0)
                                    .color(theme.text_dim),
                            );
                        }
                    }
                    ui.label(
                        RichText::new(tr(
                            "金額は推定です (単価は設定の [pricing] から。通信はしません)",
                        ))
                        .size(11.0)
                        .color(theme.text_dim),
                    );
                    ui.separator();
                }

                // ── ⏱ フレーム時間の計測 (ZAIVERN_PERF=1 のときだけ) ──
                if let Some(line) = &perf_line {
                    ui.label(RichText::new(tr("⏱ フレーム時間")).strong().size(12.5));
                    ui.label(RichText::new(line).size(11.5).color(theme.text_dim));
                    ui.horizontal_wrapped(|ui| {
                        if ui
                            .button(tr("レポートを出力"))
                            .on_hover_text(tr("ヒストグラムを 1 行 1 レコードで書き出します \
                                 (既定は stderr。ZAIVERN_PERF_OUT にパスを指定すると追記)"))
                            .clicked()
                        {
                            perf_dump = true;
                        }
                        if ui
                            .button(tr("計測をやり直す"))
                            .on_hover_text(tr("集計を捨てて、ここから計り直します"))
                            .clicked()
                        {
                            perf_reset = true;
                        }
                    });
                    ui.separator();
                }

                if accounts.is_empty() {
                    ui.label(
                        RichText::new(tr(
                            "使用量を報告する CLI が見つかりません (対応 CLI を起動すると出ます)",
                        ))
                        .color(theme.text_dim),
                    );
                    return;
                }
                for u in &accounts {
                    let sev = advice
                        .iter()
                        .find(|(a, _)| *a == u.account)
                        .map(|(_, a)| a.severity())
                        .unwrap_or(0);
                    ui.horizontal(|ui| {
                        ui.label(quota_severity_icon(sev));
                        ui.label(RichText::new(u.account.clone()).strong());
                        ui.label(RichText::new(quota_usage_label(u)).color(if sev >= 2 {
                            theme.err
                        } else {
                            theme.text
                        }));
                    });
                    ui.label(
                        RichText::new(trf(
                            "　{agents} / {n} 本並列 / {proj}",
                            &[
                                ("agents", u.agents.join(", ")),
                                ("n", u.running_agents.to_string()),
                                ("proj", quota_projection_label(u.projection)),
                            ],
                        ))
                        .size(11.5)
                        .color(theme.text_dim),
                    );
                    if let Some((_, a)) = advice.iter().find(|(a, _)| *a == u.account) {
                        let msg = a.message();
                        if !msg.is_empty() {
                            ui.label(RichText::new(format!("　⚠ {msg}")).size(11.5).color(
                                if a.severity() >= 2 {
                                    theme.err
                                } else {
                                    theme.warn
                                },
                            ));
                        }
                    }
                    ui.separator();
                }
            });
        self.quota_open = open;
        if toggle_failover {
            self.set_failover_enabled(!fo_enabled);
        }
        if perf_dump {
            let n = crate::perf::dump();
            self.toast(
                trf("性能レポートを {n} 行出力しました", &[("n", n.to_string())]),
                true,
            );
        }
        if perf_reset {
            crate::perf::reset();
            self.toast(tr("性能の計測をやり直します"), true);
        }
    }

    /// 使用量ウィンドウへ出すトークン消費の明細。
    ///
    /// 消費がゼロなら空 (見出しごと消すため)。**集計値だけ**を作る —
    /// プロンプト本文は元データから読んでいないので混ざりようがない。
    pub(super) fn token_rows(&self) -> Vec<TokenRow> {
        use coordinator::quota;
        let prices = &self.cfg.pricing;
        let cur = &prices.currency;
        let mut out = Vec::new();
        for (i, a) in self.quota.tokens().iter().enumerate() {
            let est = quota::estimate_cost(a, prices);
            let mut head = trf(
                "{label}: {tok} トークン / {n} 回",
                &[
                    ("label", a.label.clone()),
                    ("tok", quota::short_tokens(a.total.total())),
                    ("n", a.turns.to_string()),
                ],
            );
            if prices.enabled {
                head.push_str(&format!(" · {}", est.label(cur)));
            }
            if a.truncated {
                head.push_str(&tr(" (読み切れず・実際はこれ以上)"));
            }
            let mut subs = vec![trf(
                "入力 {i} / 出力 {o} / キャッシュ書 {cw} / キャッシュ読 {cr}",
                &[
                    ("i", quota::short_tokens(a.total.input)),
                    ("o", quota::short_tokens(a.total.output)),
                    ("cw", quota::short_tokens(a.total.cache_write)),
                    ("cr", quota::short_tokens(a.total.cache_read)),
                ],
            )];
            for (model, u) in &a.by_model {
                let name = if model.is_empty() {
                    tr("(モデル不明)")
                } else {
                    model.clone()
                };
                subs.push(format!("{name}: {}", quota::short_tokens(u.total())));
            }
            out.push(TokenRow {
                // 先頭 (= 最も消費しているエージェント) を目立たせる。
                // 「どれが高いか」が一目で分かるのがこの表示の目的。
                top: i == 0 && self.quota.tokens().len() > 1,
                head,
                subs,
            });
        }
        out
    }

    /// 「どの段の情報で判断できる状態か」を段ごとに並べた説明文。
    ///
    /// 設計原則 4 のはしごをそのまま見せる。最下段 (画面スクレイプ) しか無いときに
    /// 「上の段が使えないから推定でやっている」とユーザーが分かるようにするため。
    pub(super) fn failover_ladder_text(&self) -> String {
        let vendor_ok = self
            .quota
            .snapshots()
            .iter()
            .any(|q| q.source == coordinator::quota::SourceKind::Vendor);
        let mut lines = vec![tr("検知の根拠 (上ほど確実):")];
        for s in failover::LADDER {
            let available = match s {
                // これらを出す CLI はまだ無い (対応が出たらここを繋ぐ)。
                failover::Signal::Protocol | failover::Signal::VendorHook => false,
                failover::Signal::StateFile => vendor_ok,
                // 画面は常に読めるが、単独では裏取りを通さないと使わない。
                failover::Signal::Screen => true,
            };
            let mark = if available { "✓" } else { "−" };
            let note = if s.is_estimate() {
                tr(" ※裏取りが通ったときだけ使う")
            } else {
                String::new()
            };
            lines.push(format!("  {mark} {}{note}", s.label()));
        }
        lines.join("\n")
    }

    /// いまアクティブなエージェントが上限に当たったら、どこへ移るかの下見。
    /// 切替はまだ何も起きていない (純関数 [`failover::pick_failover`] を引くだけ)。
    pub(super) fn failover_preview(&self, now: Instant) -> Option<String> {
        let s = self
            .agents
            .sessions
            .get(self.agents.active)
            .filter(|s| s.running())?;
        let spec = crate::agents::spec_for_command(&s.command)?;
        let current = failover::FailingSession {
            session_id: s.id,
            preset: s.preset_name.clone(),
            bin: spec.bin.to_string(),
            account: failover::account_key(spec.bin, &s.env),
            signal: failover::Signal::Screen,
            switches: self.failover.switches_for(s.id),
            tried: self.failover.tried_for(s.id).to_vec(),
        };
        let candidates = failover::candidates_from_presets(
            &self.cfg.agents,
            self.failover.cooldowns(),
            self.failover.attempt_counts(),
            now,
        );
        failover::pick_failover(&current, &candidates, now).map(|p| p.preset)
    }
}
