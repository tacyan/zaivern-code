use super::*;

const COORDINATOR_DELIVERY_TAG: &str = "coordinator:";

fn coordinator_delivery_tag(session: u64, msg_id: u64) -> String {
    format!("{COORDINATOR_DELIVERY_TAG}{session}:{msg_id}")
}

fn parse_coordinator_delivery_tag(tag: &str) -> Option<(u64, u64)> {
    let mut parts = tag.strip_prefix(COORDINATOR_DELIVERY_TAG)?.split(':');
    let session_text = parts.next()?;
    let msg_id_text = parts.next()?;
    if session_text.is_empty()
        || msg_id_text.is_empty()
        || !session_text.bytes().all(|b| b.is_ascii_digit())
        || !msg_id_text.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let session = session_text.parse().ok()?;
    let msg_id = msg_id_text.parse().ok()?;
    parts.next().is_none().then_some((session, msg_id))
}

impl ZaivernApp {
    // ─── 監視・連携 (supervisor / coordinator / 端末フック) ──────────

    /// スーパーエージェント (指揮官) の指示をユーザーへ届ける。
    ///
    /// 指揮官セッション = `super_agent_session`。指名なし・無効なら何もしない。
    /// 毎フレーム、指揮官の画面から `@対象: 指示` (`@all:` は全員) を拾い、
    /// **ユーザー宛の通知 (📮)** として coordinator バスへ積む。
    ///
    /// **どのセッションの入力欄へも書き込まない**。以前はここから指示の配達と
    /// 状況フィードを各端末へ自動注入していたが、ユーザーが入力中の欄へ勝手に
    /// 文字が流れ込む(しかも折り返しで説明文が指示に誤検出されて連投される)
    /// ため廃止した。指示を実際に流すかは、通知を見たユーザーが決める
    /// (Cockpit の一斉送信や各端末への手入力で)。
    pub(super) fn drive_commander(&mut self) {
        let Some(cmd_id) = self.super_agent_session else {
            return;
        };

        // 指揮官の画面 → 指示。まず画面文字列を取り出してからロックを離す。
        let screen = self
            .agents
            .sessions
            .iter()
            .find(|s| s.id == cmd_id)
            .map(|s| crate::lockx::lock_ok(&s.parser).screen().contents())
            .unwrap_or_default();
        // 上限は古い方から追い出す (全消しすると画面に残っている @指示が
        // もう一度全部通知されてしまう)
        while self.commander_seen.len() > 512 {
            match self.commander_seen_order.pop_front() {
                Some(old) => {
                    self.commander_seen.remove(&old);
                }
                None => {
                    self.commander_seen.clear();
                    break;
                }
            }
        }
        let titles: Vec<String> = self
            .agents
            .sessions
            .iter()
            .filter(|s| s.id != cmd_id)
            .map(|s| s.title.clone())
            .collect();
        for d in commander::parse_directives(&screen, coordinator::INJECT_PREFIX) {
            if !self.commander_seen.insert(d.hash) {
                continue; // 既に通知済み (画面に残っているだけ)
            }
            self.commander_seen_order.push_back(d.hash);
            // 宛先が実在しない @mention の誤爆は従来どおり黙って捨てる。
            let Some(text) = commander_notice(&d, &titles) else {
                continue;
            };
            self.coordinator.enqueue(coordinator::AgentMessage::new(
                coordinator::Endpoint::Session(cmd_id),
                coordinator::Endpoint::User,
                coordinator::MsgKind::Request,
                text,
            ));
        }
    }

    /// 設定 (指揮官の指名) を反映し直す。起動時と設定変更時に必ず通る 1 か所。
    ///
    /// 指揮官は外部プロセスを持たない — 指名されたセッションの端末そのものが
    /// 指揮官なので、ここでやるのは「LLM 診断経路を止めておく」ことと
    /// 「指揮の作業状態のリセット」だけ。実際の対応付けは
    /// `sync_super_agent_session` が毎フレーム行う。
    pub(super) fn apply_super_agent(&mut self) {
        // 指揮方式: LLM 診断 (CLI の spawn) は使わない。`llm_escalation` を
        // 常に false に固定して診断経路を止める (request_diagnosis は入口で no-op、
        // diagnose() は呼ばれない = **CLI を spawn しない / 停止・再起動を提案しない**)。
        // 指揮官の指示はユーザー宛の通知になるだけで、エージェントへは働きかけない。
        self.cfg.supervisor.llm_escalation = false;
        self.supervisor.set_config(self.cfg.supervisor.clone());
        self.sup_last_diag.clear();
        // 指名が変わったので、指揮の作業状態は sync 側で必ずやり直させる。
        self.super_agent_session = None;
        self.commander_seen.clear();
        self.commander_seen_order.clear();
        self.sync_super_agent_session();
    }

    /// 指名 (タイトル / コマンド) から指揮官セッションを引き直す。
    ///
    /// タイトルで指名されていればそのセッション、指名が無ければ同じ CLI の
    /// 最初のセッションを使う。毎フレーム呼ばれるので、途中で指名を変えたり
    /// 再起動で ID が変わったりしてもここで追従する。指揮官が交代した瞬間は
    /// 前任の画面から拾った通知済み記録を必ず捨てる — 持ち越すと前任と同文の
    /// 指示が二重通知扱いで握り潰されたりする。
    pub(super) fn sync_super_agent_session(&mut self) {
        let sa = &self.cfg.super_agent;
        let appointed =
            sa.enabled && (!sa.session_title.trim().is_empty() || !sa.command.trim().is_empty());
        let id = if appointed {
            // 素のシェル (コマンド空) は候補から外す。エコーの @行 を指示と
            // 誤検出しやすい (理由文は commander_reject_reason 参照)。
            let rows: Vec<(u64, bool, String, String)> = self
                .agents
                .sessions
                .iter()
                .filter(|s| commander_reject_reason(&s.command).is_none())
                .map(|s| (s.id, s.running(), s.command.clone(), s.title.clone()))
                .collect();
            pick_commander_session(&rows, &sa.session_title, &sa.command)
        } else {
            None
        };
        if id != self.super_agent_session {
            self.super_agent_session = id;
            self.commander_seen.clear();
            self.commander_seen_order.clear();
        }
    }

    /// **エージェント同士の伝言を拾って、相手のタブへ届ける。**
    ///
    /// Team Run の中では前から動いていた仕組みを、普通に並べているタブへも
    /// 広げたもの。読み取りも配達も**同じ部品**を使う (第 2 の経路を作らない):
    /// 取り出しは `result_parser`、配達は `submit`。
    ///
    /// **同じ塊を二度配らない。** 画面は同じ伝言を何度も映すので、
    /// submit の実配送 ACK が成功したものだけを配り済みとして覚える。
    /// キュー拒否時は配り済みにせず、30 秒スロットごとに再試行する。
    ///
    /// 配った伝言と断った理由の**両方**がここを通る。片方だけ通すと、
    /// 断りばかり出る画面で覚え書きが際限なく伸びる。
    fn talk_once(&mut self, key: u64) -> bool {
        if !self.talk_seen.insert(key) {
            return false;
        }
        self.talk_order.push_back(key);
        while self.talk_order.len() > crate::app::TALK_SEEN_CAP {
            if let Some(old) = self.talk_order.pop_front() {
                self.talk_seen.remove(&old);
            }
        }
        true
    }

    /// 配送中の一時札を外す。古い並びも同時に外さないと、
    /// 同じキーを再試行した後に古い順番が新しい札を消してしまう。
    fn talk_forget(&mut self, key: u64) {
        self.talk_seen.remove(&key);
        self.talk_order.retain(|queued| *queued != key);
    }

    fn deliver_agent_talk(&mut self) {
        use crate::agent_talk::{deliveries, Peer};
        let peers: Vec<Peer> = self
            .agents
            .sessions
            .iter()
            .filter(|s| s.running())
            .map(|s| Peer {
                id: s.id,
                name: s.title.clone(),
            })
            .collect();
        if peers.len() < 2 {
            return; // 相手が居なければ何もしない
        }
        // **画面の読み取りを先に済ませてから配る。** 読みながら配ると
        // `self` を同時に借りることになる (借用検査で落ちる)。
        let mut read: Vec<(
            u64,
            Vec<crate::agent_talk::Delivery>,
            Vec<crate::agent_talk::TalkReject>,
        )> = Vec::new();
        for s in self.agents.sessions.iter() {
            if !s.running() {
                continue;
            }
            let screen = s
                .screen_tail_lines(crate::app::TALK_SCAN_ROWS, crate::app::TALK_SCAN_COLS)
                .join("\n");
            if !screen.contains(crate::features::team::imp::result_parser::MSG_OPEN) {
                continue;
            }
            let (deliveries, rejected) =
                deliveries(&screen, s.id, &peers, s.last_prompt.as_deref());
            read.push((s.id, deliveries, rejected));
        }
        let retry_slot = crate::agent_talk::retry_slot(std::time::SystemTime::now());
        let mut jobs: Vec<crate::agent_talk::Delivery> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for (from, out, bad) in read {
            for d in out {
                let delivered_key = d.delivered_key();
                if !self.talk_seen.contains(&delivered_key)
                    && !self.talk_seen.contains(&d.in_flight_key())
                    && self.talk_once(d.attempt_key(retry_slot))
                {
                    jobs.push(d);
                }
            }
            for e in bad {
                if self.talk_once(crate::agent_talk::rejection_key(from, &e)) {
                    refused.push(e.detail());
                }
            }
        }
        for d in jobs {
            let in_flight_key = d.in_flight_key();
            let failure_notice_key = d.queue_failure_notice_key();
            let delivery_tag = d.delivery_tag();
            let mut job = crate::submit::Job::user(d.to, d.text);
            job.wait_idle = true; // 相手の作業を割らない
            job.tag = Some(delivery_tag);
            if self.queue_submit(job) {
                // ここではまだ「配達中」。submit::Act::Done の outcome だけが
                // note_submit_delivery で配り済みを立てる。
                self.talk_once(in_flight_key);
            } else if self.talk_once(failure_notice_key) {
                self.toast_warn(format!(
                    "🗣 session:{} への伝言を配達待ちに積めませんでした。{} 秒後に再試行します",
                    d.to,
                    crate::agent_talk::QUEUE_RETRY_BACKOFF.as_secs()
                ));
            }
        }
        for why in refused {
            self.toast(why, false);
        }
    }

    /// **いま選んでいるエージェントへ、伝言の作法を教える。**
    ///
    /// 通常タブには Team のような指示文が無いので、教えなければ
    /// エージェントは一生この仕組みを使わない。送るのは人が押したときだけ。
    pub(crate) fn teach_agent_talk(&mut self) {
        use crate::agent_talk::Peer;
        let peers: Vec<Peer> = self
            .agents
            .sessions
            .iter()
            .filter(|s| s.running())
            .map(|s| Peer {
                id: s.id,
                name: s.title.clone(),
            })
            .collect();
        let Some(me) = self.agents.sessions.get(self.agents.active).map(|s| s.id) else {
            self.toast(tr("エージェントが選ばれていません"), false);
            return;
        };
        if peers.len() < 2 {
            self.toast(
                tr("伝言の相手が居ません (エージェントを 2 つ以上開いてください)"),
                false,
            );
            return;
        }
        let text = crate::agent_talk::how_to(&peers, me);
        let mut job = crate::submit::Job::user(me, text);
        job.wait_idle = true;
        if self.queue_submit(job) {
            self.toast(tr("🗣 伝言の使い方を送りました"), true);
        }
    }

    /// セッションの増減を supervisor / coordinator へ反映する。
    ///
    /// 起動・削除・再起動 (再起動は ID が変わる) をここ 1 か所で拾うので、
    /// 個々の操作箇所へ手を入れずに済む。
    pub(super) fn reconcile_sessions(&mut self) {
        let live: HashSet<u64> = self.agents.sessions.iter().map(|s| s.id).collect();

        let gone: Vec<u64> = self.known_sessions.difference(&live).copied().collect();
        // コンポーザの宛先別下書きも一緒に掃く。消えたエージェント宛ての
        // 書きかけを残すと、次に同じ ID が振られたときに他人の下書きが出る。
        if !gone.is_empty() {
            let ids: Vec<u64> = live.iter().copied().collect();
            self.agent_input_buf.retain_agents(&ids);
        }
        // 隔離集合も掃く。残すと ID が再利用されたとき、新しいタイルが
        // いきなり隔離状態 (= 黒いまま) で現れる。
        self.frame_guard.forget_sessions(&live);
        for id in gone {
            self.supervisor.forget(id);
            self.coordinator.unregister_session(id);
            self.orch.forget(id);
            self.known_sessions.remove(&id);
            self.sup_last_state.remove(&id);
            self.typed_sup.remove(&id);
            self.typed_voice.remove(&id);
            self.report_colors.remove(&id);
            self.sup_last_diag.remove(&id);
            // 消えたセッションについての確認ダイアログはもう意味がない。
            self.pending_intervention.retain(|p| p.session_id != id);
        }

        let fresh: Vec<u64> = live.difference(&self.known_sessions).copied().collect();
        for id in fresh {
            self.coordinator.register_session(id);
            self.known_sessions.insert(id);
        }

        // 監視役自身のセッションが増減したかもしれないので追従する。
        self.sync_super_agent_session();
    }

    /// 端末との細かい連携: フォーカス通知・クリップボード・色問い合わせへの応答。
    pub(super) fn terminal_hooks(&mut self, ctx: &egui::Context, win_focused: bool) {
        let (fg, bg) = (self.theme.term_fg, self.theme.term_bg);

        // テーマ色を伝えていない (= 起動直後 / テーマ変更後) セッションを先に洗い出す。
        let stale: Vec<u64> = self
            .agents
            .sessions
            .iter()
            .filter(|s| self.report_colors.get(&s.id) != Some(&(fg, bg)))
            .map(|s| s.id)
            .collect();

        let mut clip: Option<String> = None;
        for s in self.agents.sessions.iter_mut() {
            // 端末アプリ (vim / helix 等) へフォーカスの出入りを伝える。内部で重複排除される。
            s.set_focus(win_focused);
            // OSC 52 のヤンクを拾う。これで Neovim / Helix のコピーがシステムへ乗る。
            if let Some(t) = s.take_clipboard() {
                clip = Some(t);
            }
        }

        for id in stale {
            if let Some(s) = self.agents.sessions.iter().find(|s| s.id == id) {
                s.set_report_colors(fg, bg);
            }
            self.report_colors.insert(id, (fg, bg));
        }

        if let Some(t) = clip {
            ctx.copy_text(t);
        }
    }

    /// 毎フレーム: エージェントを見張り、返ってきた介入を実行する。
    pub(super) fn supervise(&mut self, ctx: &egui::Context, win_focused: bool) {
        // 「ユーザーが手で打った」フラグは端末側で 1 回しか取れず、音声入力とも
        // 取り合いになる。ここで一度だけ読み取って、双方の持ち越し袋へ配る。
        for s in self.agents.sessions.iter_mut() {
            if s.take_user_typed() {
                self.typed_voice.insert(s.id, true);
                self.typed_sup.insert(s.id, true);
            }
        }

        // **エージェント同士の伝言を配る。** 見張りが切られていても回す —
        // 伝言は見張りの機能ではなく、エージェント同士の通信路なので。
        self.deliver_agent_talk();

        if !self.cfg.supervisor.enabled {
            return;
        }

        // LLM から返ってきた診断を先に回収する。間引き待ちで取りこぼさないよう
        // サンプリング刻みの前に置く。**推奨はここでも同じ経路へ流すだけ**で、
        // 確認ゲートを飛ばす近道は作らない。
        let approval = crate::agents::Approval::from_mode(&self.cfg.approval_mode);
        for d in self.supervisor.poll_diagnoses() {
            self.toast(
                trf("💡 AI 診断: {summary}", &[("summary", d.summary.clone())]),
                false,
            );
            if let Some(it) = self.supervisor.intent_from_diagnosis(&d, approval) {
                self.accept_intent(it, ctx, win_focused);
            }
        }

        // supervisor 側も内部で間引くが、無駄に画面テキストを取り出さないよう
        // こちらでも同じ間隔 (+ 余裕 50ms) で刻む。UI スレッドは止めない。
        let now = Instant::now();
        if self.sup_next_at.is_some_and(|t| now < t) {
            return;
        }
        self.sup_next_at = Some(
            now + Duration::from_millis(self.cfg.supervisor.sample_interval_ms.saturating_add(50)),
        );

        let mut typed = std::mem::take(&mut self.typed_sup);
        let snaps: Vec<supervisor::SessionSnapshot> = self
            .agents
            .sessions
            .iter()
            .map(|s| {
                supervisor::SessionSnapshot::from_session(s, typed.remove(&s.id).unwrap_or(false))
            })
            .collect();

        for it in self.supervisor.tick(&snaps, approval) {
            // 見張りは「検知」と「ユーザーへの通知」だけに徹する。
            // 停止 (Restart/Halt)・一時停止・促しメッセージ (Nudge)・自動応答
            // (AutoAnswer) を **エージェントへ直接投げない**。エージェントへ実際に
            // 働きかけるのは、スーパーエージェントの指揮 (drive_commander) のみ。
            use supervisor::Intervention as I;
            if matches!(it.action, I::AutoAnswer | I::Nudge | I::Restart | I::Halt) {
                continue;
            }
            // llm_escalation は常に false なので、この相談は入口で no-op (spawn しない)。
            let (sid, anomaly) = (it.session_id, it.anomaly);
            if self.sup_last_diag.get(&sid) != Some(&anomaly) {
                self.sup_last_diag.insert(sid, anomaly);
                self.supervisor.request_diagnosis(sid, anomaly, ctx);
            }
            self.accept_intent(it, ctx, win_focused);
        }

        self.bridge_states();
    }

    /// 介入の意図を受け取り、**確認ゲートを通してから**実行する唯一の入口。
    ///
    /// 決定論的な見張りからの提案も、LLM の助言由来の提案もここへ集める。
    /// 経路を 1 本にしておかないと、片方だけ確認を飛ばす抜け道がいつか生える。
    pub(super) fn accept_intent(
        &mut self,
        it: supervisor::InterventionIntent,
        ctx: &egui::Context,
        win_focused: bool,
    ) {
        match route_intent(&it) {
            IntentRoute::Confirm => {
                // 同じセッション・同じ操作の確認が二重に積まれないようにする。
                let dup = self
                    .pending_intervention
                    .iter()
                    .any(|p| p.session_id == it.session_id && p.action == it.action);
                if !dup {
                    self.toast_warn(it.toast_line());
                    self.pending_intervention.push(it);
                }
            }
            IntentRoute::Run => self.run_intervention(it, ctx, win_focused),
        }
    }

    /// スーパーバイザーの見立ての変化を coordinator へ伝える。
    /// 状態が変わった瞬間だけ通すので、毎フレーム叩き続けることにはならない。
    pub(super) fn bridge_states(&mut self) {
        let now = Instant::now();
        let seen: Vec<(u64, Option<supervisor::SessionState>)> = self
            .agents
            .sessions
            .iter()
            .map(|s| (s.id, self.supervisor.state_of(s.id)))
            .collect();
        for (id, st) in seen {
            let Some(st) = st else { continue };
            if self.sup_last_state.get(&id) == Some(&st) {
                continue;
            }
            self.sup_last_state.insert(id, st);
            match st {
                supervisor::SessionState::Stalled => self.coordinator.note_stalled(id, now),
                supervisor::SessionState::Crashed | supervisor::SessionState::Done => {
                    self.coordinator.note_exited(id, now)
                }
                _ => {}
            }
        }
    }

    /// 毎フレーム: 配達待ちのメッセージを流し、ユーザー宛は必ず UI へ出す。
    pub(super) fn coordinate(&mut self, win_focused: bool) {
        // 0) 指揮官の `@対象:` 指示をユーザー宛の通知として coordinator へ積む
        //    (セッションへの自動注入はしない。下の take_user_messages で UI へ出る)。
        self.drive_commander();

        // 1) いまのセッション状態表。曖昧なら Unknown (= 配達しない)。
        let states: Vec<(coordinator::SessionId, coordinator::SessionState)> = self
            .agents
            .sessions
            .iter()
            .map(|s| {
                (
                    s.id,
                    coordinator_state(
                        s.running(),
                        s.attention,
                        s.rate_limited.is_some(),
                        self.supervisor.state_of(s.id),
                    ),
                )
            })
            .collect();

        // 2) 注入して安全なセッションへ 1 通ずつ予約する。
        // PTY へは直書きしない。本文→確定キー→入力欄検証の共通経路へ流し、
        // 成功 ACK が返るまで coordinator の受信箱から消さない。
        for d in self.coordinator.take_deliverable(&states) {
            let mut job = crate::submit::Job::deferred(d.session, d.text, true);
            job.tag = Some(coordinator_delivery_tag(d.session, d.msg_id));
            if !self.queue_submit(job) {
                self.coordinator
                    .defer_delivery(d.session, d.msg_id, Instant::now());
            }
        }

        // 3) ユーザー宛は握り潰さない。抑制もエスカレーションも必ず見える形にする。
        for m in self.coordinator.take_user_messages() {
            let line = format!("📮 {} — {}", tr(m.kind.label()), m.body);
            self.toast_warn(line.clone());
            if !win_focused {
                notify::notify("Zaivern Code", &line);
            }
        }

        // 4) 前任セッションの停止提案を承認モードのゲートに通す。
        self.gate_stop_proposals();
        // 5) 実際にプロセスが消えたものだけ「停止確認済み」にする。
        self.confirm_stopped_sessions();
        // 6) 発信マーカーの取り込みと、停止確認済みタスクの引き継ぎ。
        self.orch_tick();
    }

    /// 共通 submit キューの結果を、目印ごとの所有者へ一度だけ返す。
    pub(crate) fn note_submit_delivery(&mut self, outcomes: Vec<(String, bool)>) {
        let now = Instant::now();
        let mut team = Vec::new();
        let mut coordinator_delivered = Vec::new();
        for (tag, delivered) in outcomes {
            if let Some((session, msg_id)) = parse_coordinator_delivery_tag(&tag) {
                if self
                    .coordinator
                    .finish_delivery(session, msg_id, delivered, now)
                {
                    coordinator_delivered.push(session);
                }
            } else if tag.starts_with(COORDINATOR_DELIVERY_TAG) {
                // coordinator 名前空間の壊れたタグを Team 側へ流すと、
                // 無関係な Run の ACK として誤解釈され得る。配送は進めず人へ見せる。
                self.toast_warn(format!("内部配送タグが壊れています: {tag}"));
            } else if let Some(identity) = crate::agent_talk::parse_delivery_tag(&tag) {
                self.talk_forget(identity.in_flight_key());
                if delivered {
                    // **ここが agent-talk の配り済み確定点。**
                    // queue 受理ではなく、確定キーが効いたと検証できた後だけ立てる。
                    self.talk_once(identity.delivered_key());
                } else if self.talk_once(identity.outcome_failure_notice_key()) {
                    self.toast_warn(format!(
                        "🗣 session:{} への伝言を実配送できませんでした。次の再試行スロットでやり直します",
                        identity.to
                    ));
                }
            } else if crate::agent_talk::is_delivery_tag_namespace(&tag) {
                // agent-talk 名前空間の壊れたタグも Team に渡さない。
                self.toast_warn(format!("内部伝言タグが壊れています: {tag}"));
            } else {
                team.push((tag, delivered));
            }
        }
        if !coordinator_delivered.is_empty() {
            orchestration::note_delivered(&mut self.coordinator, &coordinator_delivered, now);
        }
        if !team.is_empty() {
            self.team_note_delivery(team);
        }
    }

    /// 停止提案 → [`coordinator::gate_for`] → 自動承認なら即実行 / 要確認なら待ち行列へ。
    pub(super) fn gate_stop_proposals(&mut self) {
        let mode = orchestration::permission_mode(&self.cfg.approval_mode);
        let task_ids: Vec<coordinator::TaskId> = self
            .coordinator
            .tasks()
            .iter()
            .filter(|t| {
                matches!(
                    t.state,
                    coordinator::TaskState::Stalled | coordinator::TaskState::Failed
                )
            })
            .map(|t| t.id)
            .collect();

        for tid in task_ids {
            if self.stopping.iter().any(|(t, _)| *t == tid) {
                continue;
            }
            let queued = self.pending_stop.iter().any(|p| {
                let coordinator::Proposal::StopSession { task, .. } = p;
                *task == tid
            });
            if queued {
                continue;
            }
            let Some(p) = self.coordinator.propose_stop(tid) else {
                continue;
            };
            match coordinator::gate_for(mode) {
                coordinator::ProposalGate::AutoApproved => self.execute_stop(p),
                coordinator::ProposalGate::NeedsUserConfirm => self.pending_stop.push(p),
            }
        }
    }

    /// 停止を実行する。**自動承認済み / ユーザー確認済みのものだけ**ここへ来る。
    pub(super) fn execute_stop(&mut self, p: coordinator::Proposal) {
        let coordinator::Proposal::StopSession {
            session,
            task,
            reason,
        } = p;
        if let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == session) {
            s.kill();
        }
        self.toast_warn(format!("🛑 {reason}"));
        // プロセスが本当に消えるまで confirm_stopped は呼ばない。
        self.stopping.push((task, session));
    }

    /// 停止待ちのうち、プロセスが消えたものだけ確定させる。
    pub(super) fn confirm_stopped_sessions(&mut self) {
        if self.stopping.is_empty() {
            return;
        }
        let now = Instant::now();
        let done: Vec<(coordinator::TaskId, u64)> = self
            .stopping
            .iter()
            .filter(|(_, sid)| {
                !self
                    .agents
                    .sessions
                    .iter()
                    .any(|s| s.id == *sid && s.running())
            })
            .copied()
            .collect();
        for (tid, _) in done {
            self.coordinator.confirm_stopped(tid, now);
            self.stopping.retain(|(t, _)| *t != tid);
        }
    }

    // ─── 調停レイヤ (orchestration) への橋渡し ──────────────────────
    //
    // 判断も描画も `orchestration` 側に置いてある。ここにあるのは
    // 「いまのセッションを写す」「返ってきた副作用を実行する」だけ。

    /// 生きているセッションを `orchestration` 用の要約へ写す。
    pub(super) fn orch_rows(&self) -> Vec<orchestration::SessionRow> {
        self.agents
            .sessions
            .iter()
            .map(|s| orchestration::SessionRow {
                id: s.id,
                title: s.title.clone(),
                running: s.running(),
                state: coordinator_state(
                    s.running(),
                    s.attention,
                    s.rate_limited.is_some(),
                    self.supervisor.state_of(s.id),
                ),
            })
            .collect()
    }

    /// `orchestration` が返した副作用 (トースト・PTY への書き込み) を実行する。
    pub(super) fn orch_effects(&mut self, eff: orchestration::Effects) {
        for (sid, text) in eff.writes {
            if let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == sid) {
                s.note_prompt(&text);
                s.write_bytes(text.as_bytes());
            }
        }
        for (line, ok) in eff.toasts {
            self.toast(line, ok);
        }
    }

    /// UI から出てきた要求をまとめて実行する。
    pub(super) fn orch_apply(&mut self, acts: Vec<orchestration::OrchAction>) {
        if acts.is_empty() {
            return;
        }
        let now = Instant::now();
        let rows = self.orch_rows();
        for a in acts {
            let eff = orchestration::apply_action(&mut self.coordinator, &rows, a, now);
            self.orch_effects(eff);
        }
    }

    /// 毎フレーム: 発信マーカーの取り込みと、停止確認済みタスクの引き継ぎ。
    ///
    /// 配達完了の記録は submit の成功 ACK を受けた
    /// [`ZaivernApp::note_submit_delivery`] だけが行う。
    pub(super) fn orch_tick(&mut self) {
        let now = Instant::now();

        // 画面の走査は間引く。UI スレッドを塞がないため。
        if orchestration::scan_due(&mut self.orch, now) {
            let rows = self.orch_rows();
            let screens: Vec<(u64, String)> = self
                .agents
                .sessions
                .iter()
                .filter(|s| s.running())
                .map(|s| {
                    let text = crate::lockx::lock_ok(&s.parser).screen().contents();
                    (s.id, text)
                })
                .collect();
            for (id, screen) in screens {
                let eff = orchestration::scan_outbound(
                    &mut self.orch,
                    &mut self.coordinator,
                    id,
                    &screen,
                    &rows,
                    now,
                );
                self.orch_effects(eff);
            }
        }

        // 前任の停止が確認できたタスクだけを次の担当へ渡す。
        let rows = self.orch_rows();
        let eff =
            orchestration::redispatch_ready(&mut self.orch, &mut self.coordinator, &rows, now);
        self.orch_effects(eff);
    }

    /// 介入を実際に実行する。確認が要るものは、呼び出し側で確認済みであること。
    pub(super) fn run_intervention(
        &mut self,
        it: supervisor::InterventionIntent,
        ctx: &egui::Context,
        win_focused: bool,
    ) {
        use supervisor::Intervention as I;
        let idx = self
            .agents
            .sessions
            .iter()
            .position(|s| s.id == it.session_id);

        match it.action {
            // 記録だけ。UI には出さない。
            I::Observe => {}
            I::Notify => {
                let line = it.toast_line();
                self.toast_warn(line.clone());
                // **同じ異常が続く間は OS 通知を繰り返さない。**
                // 見張りは cooldown (既定 120 秒) ごとに同じ診断を上げ直すので、
                // 素通しすると通知センターが同文で埋まる。トーストは
                // アプリ内なので残し、外へ出るものだけを遷移エッジへ絞る。
                let fresh = self.anomaly_gate.changed(it.session_id, &line);
                if !win_focused && fresh {
                    notify::notify("Zaivern Code", &line);
                }
            }
            I::AutoAnswer => {
                let (Some(i), Some(payload)) = (idx, it.payload.clone()) else {
                    return;
                };
                if let Some(s) = self.agents.sessions.get_mut(i) {
                    s.write_bytes(payload.as_bytes());
                    s.resolve_attention();
                }
                self.toast(
                    trf(
                        "🛡 {title} へ自動応答しました",
                        &[("title", it.session_title.clone())],
                    ),
                    true,
                );
            }
            I::Nudge => {
                let (Some(i), Some(payload)) = (idx, it.payload.clone()) else {
                    return;
                };
                let sent = self
                    .agents
                    .sessions
                    .get_mut(i)
                    .is_some_and(|s| s.send_text(&format!("{payload}\r")));
                if sent {
                    self.toast_warn(trf(
                        "🛡 {title} を促しました: {payload}",
                        &[("title", it.session_title.clone()), ("payload", payload)],
                    ));
                }
            }
            I::Restart => {
                let Some(i) = idx else { return };
                // 再起動すると ID が変わる。古い ID の記録はここで捨て、
                // 新しい ID は次フレームの reconcile_sessions が拾う。
                self.supervisor.forget(it.session_id);
                self.coordinator.note_exited(it.session_id, Instant::now());
                self.sup_last_state.remove(&it.session_id);
                match self.agents.restart(i, ctx) {
                    Ok(()) => self.toast_warn(trf(
                        "🛡 {title} を再起動しました",
                        &[("title", it.session_title.clone())],
                    )),
                    Err(e) => self.toast(e, false),
                }
            }
            I::Halt => {
                let Some(i) = idx else { return };
                if let Some(s) = self.agents.sessions.get_mut(i) {
                    s.kill();
                }
                self.coordinator.note_exited(it.session_id, Instant::now());
                self.toast_warn(trf(
                    "🛡 {title} を停止しました",
                    &[("title", it.session_title.clone())],
                ));
            }
        }
    }

    /// 介入の確認モーダル。`needs_confirmation` の介入は、ここで「実行」を
    /// 押されない限り絶対に実行されない。
    pub(super) fn intervention_confirm_ui(&mut self, ctx: &egui::Context, win_focused: bool) {
        if self.pending_intervention.is_empty() {
            return;
        }
        let it = self.pending_intervention[0].clone();
        let warn = self.theme.warn;
        let rest = self.pending_intervention.len() - 1;
        let mut decided: Option<bool> = None;

        egui::Window::new(tr("エージェント監視からの提案"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(it.confirm_body());
                if rest > 0 {
                    ui.label(
                        RichText::new(trf(
                            "ほかに {rest} 件の提案があります",
                            &[("rest", rest.to_string())],
                        ))
                        .small()
                        .color(warn),
                    );
                }
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    let label = trf("▶ {action} を実行", &[("action", tr(it.action.label()))]);
                    if ui.button(RichText::new(label).color(warn)).clicked() {
                        decided = Some(true);
                    }
                    if ui.button(tr("何もしない")).clicked() {
                        decided = Some(false);
                    }
                });
            });

        match decided {
            Some(true) => {
                self.pending_intervention.remove(0);
                self.run_intervention(it, ctx, win_focused);
            }
            Some(false) => {
                self.pending_intervention.remove(0);
            }
            None => {}
        }
    }

    /// 前任セッションを止める提案の確認モーダル。
    /// 全エージェント一括停止の確認モーダル (破壊的操作なので必ず通す)。
    pub(super) fn stop_all_confirm_ui(&mut self, ctx: &egui::Context) {
        if !self.pending_stop_all {
            return;
        }
        let running = self.agents.running_count();
        if running == 0 {
            self.pending_stop_all = false;
            return;
        }
        let warn = self.theme.warn;
        let mut decided: Option<bool> = None;
        egui::Window::new(tr("全エージェントの停止"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(trf(
                    "稼働中の {n} 体をすべて停止します。作業中の内容は失われる可能性があります。",
                    &[("n", running.to_string())],
                ));
                ui.label(
                    RichText::new(tr(
                        "タブは残るので、あとから ⟳ で同じ場所から起動し直せます。",
                    ))
                    .small(),
                );
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new(tr("🛑 全部停止する")).color(warn))
                        .clicked()
                    {
                        decided = Some(true);
                    }
                    if ui.button(tr("キャンセル")).clicked() {
                        decided = Some(false);
                    }
                });
            });
        match decided {
            Some(true) => {
                self.pending_stop_all = false;
                let n = self.agents.stop_all();
                self.toast(
                    trf("🛑 {n} 体を停止しました", &[("n", n.to_string())]),
                    true,
                );
            }
            Some(false) => self.pending_stop_all = false,
            None => {}
        }
    }

    /// 閉じたエージェントに割り当てられていた worktree をどうするかの確認モーダル。
    ///
    /// **既定は「残す」側**。未コミットの変更があるときは何が失われるかを本文に
    /// 明示し、削除ボタンだけを警告色にする (`git worktree remove --force` は
    /// ここを通ったときにしか撃たれない)。
    pub(super) fn worktree_confirm_ui(&mut self, ctx: &egui::Context) {
        let Some((wt, dirty)) = self.pending_worktree.clone() else {
            return;
        };
        let warn = self.theme.warn;
        let body = worktree::removal_prompt(&wt.branch, &wt.dir, dirty);
        let mut decided: Option<bool> = None;
        egui::Window::new(tr("worktree の後始末"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_max_width(560.0);
                ui.label(body);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(tr("🌿 残す"))
                        .on_hover_text(tr(
                            "worktree とブランチをそのまま残します（あとで自分でマージ・削除できます）",
                        ))
                        .clicked()
                    {
                        decided = Some(false);
                    }
                    let del = if dirty {
                        tr("🗑 変更ごと削除する")
                    } else {
                        tr("🗑 削除する")
                    };
                    if ui
                        .button(RichText::new(del).color(warn))
                        .on_hover_text(tr("worktree のフォルダとブランチを削除します"))
                        .clicked()
                    {
                        decided = Some(true);
                    }
                });
            });
        match decided {
            Some(true) => {
                self.pending_worktree = None;
                match worktree::remove_agent_worktree(&wt, dirty) {
                    Ok(()) => self.toast(
                        trf(
                            "🗑 worktree {branch} を削除しました",
                            &[("branch", wt.branch.clone())],
                        ),
                        true,
                    ),
                    Err(e) => self.toast(e, false),
                }
                self.persist_session();
            }
            Some(false) => {
                self.pending_worktree = None;
                self.toast(
                    trf(
                        "🌿 worktree {branch} を残しました",
                        &[("branch", wt.branch.clone())],
                    ),
                    true,
                );
                self.persist_session();
            }
            None => {}
        }
    }

    pub(super) fn stop_confirm_ui(&mut self, ctx: &egui::Context) {
        if self.pending_stop.is_empty() {
            return;
        }
        let p = self.pending_stop[0].clone();
        let coordinator::Proposal::StopSession { ref reason, .. } = p;
        let body = reason.clone();
        let warn = self.theme.warn;
        let mut decided: Option<bool> = None;

        egui::Window::new(tr("セッション停止の確認"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(&body);
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if ui
                        .button(RichText::new(tr("🛑 停止する")).color(warn))
                        .clicked()
                    {
                        decided = Some(true);
                    }
                    if ui.button(tr("キャンセル")).clicked() {
                        decided = Some(false);
                    }
                });
            });

        match decided {
            Some(true) => {
                self.pending_stop.remove(0);
                self.execute_stop(p);
            }
            Some(false) => {
                self.pending_stop.remove(0);
            }
            None => {}
        }
    }

    /// 音声側が読む「ユーザーが手で打った」フラグ。
    /// 端末側の生フラグは監視 (supervise) も読むので、
    /// 取りこぼさないよう持ち越し袋と OR して返す。
    pub(super) fn take_typed_voice(&mut self, id: u64) -> bool {
        let live = self
            .agents
            .sessions
            .iter_mut()
            .find(|s| s.id == id)
            .is_some_and(|s| s.take_user_typed());
        let carried = self.typed_voice.remove(&id).unwrap_or(false);
        live || carried
    }

    // ── フレームガード: 部分ビューの隔離と警告バナー ──────────────────

    /// 部分ビューを「いま描いている場所」の印付きで描く。
    ///
    /// 隔離中は**必ず代わりの説明を描く**。かつては何も描かずに `return`
    /// していたが、パネルごと消えると egui はその矩形に何も塗らないので、
    /// 直前のフレームの内容かウィンドウ背景がそのまま残る = 「黒い空間」に
    /// なる (Windows の Cockpit で「ファイルを開く / ウィンドウを閉じると
    /// 元の場所が黒く残る」と報告された不具合の直接原因)。
    /// しかも何が起きたのか一切分からない。
    pub(super) fn guarded_view(
        &mut self,
        sv: Subview,
        ctx: &egui::Context,
        draw: impl FnOnce(&mut Self),
    ) {
        if self.frame_guard.is_quarantined(&sv) {
            self.quarantined_panel_ui(&sv, ctx);
            return;
        }
        draw_subview(sv, || draw(self));
    }

    /// `guarded_view` の `Ui` 版。隔離中は代わりに説明を出す
    /// (領域が確保されているので、真っ黒な穴を残さないため)。
    pub(super) fn guarded_ui(
        &mut self,
        sv: Subview,
        ui: &mut egui::Ui,
        draw: impl FnOnce(&mut Self, &mut egui::Ui),
    ) {
        if self.frame_guard.is_quarantined(&sv) {
            let (err, dim) = (self.theme.err, self.theme.text_dim);
            if Self::quarantine_placeholder_ui(ui, &sv.label(), err, dim) {
                self.frame_guard.unquarantine(&sv);
            }
            return;
        }
        draw_subview(sv, || draw(self, ui));
    }

    /// 隔離されたパネルの代わりを、**同じ場所・同じ大きさ**で描く。
    ///
    /// パネルの id は生きているときと同じものを使う — 変えるとリサイズ幅の
    /// 記憶が失われ、隔離が解けた瞬間に既定幅へ戻ってしまう。
    pub(super) fn quarantined_panel_ui(&mut self, sv: &Subview, ctx: &egui::Context) {
        let (err, dim) = (self.theme.err, self.theme.text_dim);
        let label = sv.label();
        let mut retry = false;
        match sv {
            Subview::Panel("sidebar") => {
                let open = self.sidebar_open;
                egui::SidePanel::left("zv-side")
                    .resizable(true)
                    .default_width(255.0)
                    .width_range(180.0..=440.0)
                    .show_animated(ctx, open, |ui| {
                        retry = Self::quarantine_placeholder_ui(ui, &label, err, dim);
                    });
            }
            Subview::Panel("terminal") => {
                let open = self.agents.panel_open && !self.cockpit;
                egui::TopBottomPanel::bottom("zv-terminal")
                    .resizable(true)
                    .default_height(300.0)
                    .min_height(140.0)
                    .show_animated(ctx, open, |ui| {
                        // ボトムパネルは中身が高さを埋め切らないとリサイズバーが
                        // 毎フレームずり落ちる (生きているときと同じ手当て)。
                        ui.set_min_height(ui.available_height());
                        retry = Self::quarantine_placeholder_ui(ui, &label, err, dim);
                    });
            }
            // 中央に敷かれる領域 (エディタ / Cockpit) は guarded_ui 側が
            // その場の Ui へ描く。ここへ来るのは想定外の Subview だけなので、
            // 画面を黒くしないよう中央パネルで受ける。
            _ => {
                egui::CentralPanel::default().show(ctx, |ui| {
                    retry = Self::quarantine_placeholder_ui(ui, &label, err, dim);
                });
            }
        }
        if retry {
            self.frame_guard.unquarantine(sv);
        }
    }

    /// 隔離した領域の代わりに出す説明。「再試行」が押されたら `true`。
    ///
    /// 復帰手段をここに置くのが要点 — バナーは 1 度閉じると二度と出ないので、
    /// バナーにしか再試行が無いとそのタイルは永久に死んだままになる。
    pub(super) fn quarantine_placeholder_ui(
        ui: &mut egui::Ui,
        label: &str,
        err: Color32,
        dim: Color32,
    ) -> bool {
        let mut retry = false;
        ui.vertical_centered(|ui| {
            ui.add_space(24.0);
            ui.label(RichText::new("⚠").size(28.0).color(err));
            ui.label(
                RichText::new(trf(
                    "{where} の表示を停止しました",
                    &[("where", label.to_string())],
                ))
                .color(err),
            );
            ui.label(
                RichText::new(tr("内部エラーが繰り返し起きたため描画から外しています。\n\
                     詳細は ~/.zaivern/panic.log を見てください。"))
                .size(11.5)
                .color(dim),
            );
            ui.add_space(8.0);
            if ui
                .button(tr("⟳ ここだけ再試行"))
                .on_hover_text(tr("この領域の隔離だけを解いて描画をやり直します"))
                .clicked()
            {
                retry = true;
            }
        });
        retry
    }

    /// 内部エラーの警告バナー (画面最上部)。隔離が起きたときだけ出る。
    pub(super) fn frame_error_banner_ui(&mut self, ctx: &egui::Context) {
        let Some(msg) = self.frame_guard.banner.clone() else {
            return;
        };
        let (bg, fg) = (self.theme.panel_alt, self.theme.err);
        let mut retry = false;
        let mut dismiss = false;
        egui::TopBottomPanel::top("zv-frame-error")
            .frame(
                egui::Frame::none()
                    .fill(bg)
                    .inner_margin(egui::Margin::symmetric(10.0, 6.0)),
            )
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(msg).color(fg).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(tr("閉じる")).clicked() {
                            dismiss = true;
                        }
                        if ui
                            .small_button(tr("再試行"))
                            .on_hover_text(tr("隔離を解いて描画をやり直します"))
                            .clicked()
                        {
                            retry = true;
                        }
                    });
                });
            });
        if retry {
            self.frame_guard.reset();
        } else if dismiss {
            // 隔離はそのまま (解くと崩れが戻るため)。表示だけ引っ込める
            self.frame_guard.banner = None;
        }
    }
}

#[cfg(test)]
mod coordinator_delivery_tag_tests {
    use super::{
        coordinator_delivery_tag, parse_coordinator_delivery_tag, COORDINATOR_DELIVERY_TAG,
    };

    #[test]
    fn coordinator_tag_round_trips_exact_ids() {
        let tag = coordinator_delivery_tag(u64::MAX - 1, u64::MAX);
        assert_eq!(
            parse_coordinator_delivery_tag(&tag),
            Some((u64::MAX - 1, u64::MAX))
        );
    }

    #[test]
    fn coordinator_tag_rejects_malformed_or_ambiguous_values() {
        for tag in [
            "coordinator:",
            "coordinator:1",
            "coordinator::2",
            "coordinator:1:",
            "coordinator: 1:2",
            "coordinator:+1:2",
            "coordinator:1:2:3",
            "coordinator:1:18446744073709551616",
            "team:1:2",
        ] {
            assert_eq!(parse_coordinator_delivery_tag(tag), None, "{tag}");
        }
        assert!("coordinator:broken".starts_with(COORDINATOR_DELIVERY_TAG));
    }
}
