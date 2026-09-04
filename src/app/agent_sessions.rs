use super::*;

impl ZaivernApp {
    /// 自動フェイルオーバーの有効/無効を切り替えて保存し、結果を知らせる。
    /// パレット項目と 📊 プラン使用量ウィンドウのトグルが両方ここを通る。
    /// git blame の表示段階を決める**唯一の入口**。
    ///
    /// パレット (`src/features/blame.rs` の `blame.off` / `blame.current` /
    /// `blame.all`)・表示メニュー (`Cmd::ToggleGitBlame` が次の段へ回す)・
    /// 設定画面 (`git_blame` の 3 択) の 3 経路がここへ集まる。
    pub(crate) fn set_blame_mode(&mut self, mode: config::BlameMode) {
        let changed = self.cfg.git_blame != mode;
        self.cfg.git_blame = mode;
        self.cfg.global_git_blame = mode;
        if changed {
            // 段が変わると取りに行く行域も変わる。古い結果は捨てる
            // (OFF にしたときは裏で走り続けないためでもある)。
            self.blame.clear();
        }
        config::save_state(&self.cfg);
        // 段の名前は `BlameMode::label` が唯一の定義 (画面と設定でずらさない)。
        let name = tr(mode.label());
        self.toast(
            if mode.is_on() {
                trf(
                    "👤 Git blame: {mode} (ガターに 著者 · 相対日時。クリックでそのコミットの差分)",
                    &[("mode", name)],
                )
            } else {
                trf("👤 Git blame: {mode}", &[("mode", name)])
            },
            true,
        );
    }

    pub(super) fn set_failover_enabled(&mut self, on: bool) {
        self.failover.set_enabled(on);
        self.cfg.failover.enabled = on;
        config::save_state(&self.cfg);
        if on {
            self.toast_warn(tr(
                "🔁 自動フェイルオーバーを有効化しました — 上限に当たったら別プロファイルへ切り替えます",
            ));
        } else {
            self.toast(tr("🔁 自動フェイルオーバーを無効化しました"), true);
        }
    }

    /// セッションタブのフォルダ一覧を必要なときだけ作り直す。
    ///
    /// 対象は **いま開いているワークスペースのルートだけ**。MRU も他ブランチの
    /// worktree も混ぜない (VS Code の Claude Code 拡張と同じ切り方: 開いている
    /// フォルダで交わした会話だけが出る)。
    ///
    /// [`session_picker::sidebar_folders`] は `is_dir()` を叩く = ファイルシステムに
    /// 触るので毎フレームは呼ばない。ルートが変わったときだけ作り直す
    /// (走査そのものは SidebarState 側がスレッドへ逃がす)。
    pub(super) fn refresh_session_folders(&mut self) {
        // 変化の判定には **fs を叩かない** 生の値だけを使う。
        let src: Vec<PathBuf> = self.roots.clone();
        if src == self.sess_folders_src {
            return;
        }
        self.sess_folders = session_picker::sidebar_folders(&self.roots);
        self.sess_folders_src = src;
        // 対象フォルダが変わった = 走査結果のキャッシュはもう当たらない
        self.sidebar_sessions.invalidate();
    }

    /// 「💬 セッション」タブ。
    ///
    /// 出すのは **いま開いているフォルダで交わした会話だけ**。ブランチ
    /// (worktree) 別にまとめる表示は持たない — 同じフォルダを開いている限り
    /// 一覧は常に同じ、という一本の規則にする (VS Code の Claude Code 拡張と同じ)。
    pub(super) fn sidebar_sessions_ui(
        &mut self,
        ui: &mut egui::Ui,
        theme: &Theme,
    ) -> session_picker::SidebarAction {
        let folders = self.sess_folders.clone();
        panels::sessions_sidebar_ui(ui, theme, &mut self.sidebar_sessions, &folders)
    }

    /// 差分ビュー (PR 差分・レース差分) の「エージェントに送る」で組み立てられた
    /// レビュープロンプトを入力欄へ流し込む。
    ///
    /// **送信はしない** — 入れるだけで、送るかどうかはユーザーが決める
    /// (プロジェクト方針: 入力欄への自動書き込みで Enter は撃たない)。
    pub(super) fn take_review_prompt(&mut self, prompt: String) {
        // 差分を見ていたエージェント宛ての下書きへ入れる (居なければ全員宛て)。
        // 宛先ごとに下書きが分かれるので、レビュー文が全エージェントへ飛ばない。
        let target = match self.agents.active_session() {
            Some(s) => crate::agent_input::ComposerTarget::Agent(s.id),
            None => crate::agent_input::ComposerTarget::Broadcast,
        };
        self.agent_input_buf.set_target(target);
        if !self.agent_input_buf.append_prompt_for(target, &prompt) {
            return;
        }
        // 入力欄が見えていないと「押したのに何も起きない」ので開く
        self.cockpit = true;
        self.kanban = false;
        self.agents.panel_open = true;
        self.toast(
            tr("レビューコメントを入力欄へ入れました (内容を確かめてから送信してください)"),
            true,
        );
    }

    /// セッションサイドバー (「💬 セッション」タブ) で押されたものを実行する。
    ///
    /// 判断そのものは純関数 [`session_sidebar_effect`] 側にあり、ここは
    /// 「決まったこと」を通常の起動経路へ流すだけ。
    pub(super) fn apply_session_sidebar(
        &mut self,
        action: session_picker::SidebarAction,
        ctx: &egui::Context,
    ) {
        match session_sidebar_effect(&action, &self.cfg.agents) {
            SessionSidebarEffect::Nothing(msg) => {
                if let Some(m) = msg {
                    self.toast(m, false);
                }
            }
            SessionSidebarEffect::Launch {
                preset,
                command,
                cwd,
            } => {
                self.launch_preset_with(preset, command, &cwd, ctx);
            }
            SessionSidebarEffect::Reveal(dir) => {
                open_external(&dir.display().to_string());
            }
            SessionSidebarEffect::RemoveRoot(dir) => {
                // 「フォルダをワークスペースから削除」と同じ口を通す
                // (最後の 1 つは削除できない、という判断もそちらに揃う)
                self.apply_cmd(Cmd::RemoveFolder(dir), ctx);
            }
        }
    }

    /// プリセット `i` を **専用の git worktree** で起動する。
    ///
    /// 「並列エージェントは衝突を後で発見させない」の一番強い形 —
    /// そもそも同じ作業ツリーを共有させない。作業フォルダが git リポジトリで
    /// ないときは worktree を作れないので、理由を出して何もしない
    /// (メニュー側でも同じ判定で選択肢を無効化している)。
    pub(super) fn launch_preset_isolated(&mut self, i: usize, ctx: &egui::Context) {
        let Some(p) = self.cfg.agents.get(i).cloned() else {
            return;
        };
        let root = self.agent_cwd();
        if !worktree::looks_like_git_repo(&root) {
            self.toast(
                tr("git リポジトリではないので worktree 隔離は使えません（worktree は git の機能です）"),
                false,
            );
            return;
        }
        let wt = match worktree::create_agent_worktree(&root, &p.name) {
            Ok(wt) => wt,
            Err(e) => {
                self.toast(e, false);
                return;
            }
        };
        let before = self.agents.sessions.len();
        self.launch_preset_with(i, p.command.clone(), &wt.dir, ctx);
        match self.agents.sessions.len() > before {
            true => {
                if let Some(id) = self.agents.sessions.last().map(|s| s.id) {
                    self.agent_worktrees.insert(id, wt.clone());
                }
                self.toast(
                    trf(
                        "🌿 {name} を隔離 worktree ({branch}) で起動しました",
                        &[("name", p.name.clone()), ("branch", wt.branch.clone())],
                    ),
                    true,
                );
                self.persist_session();
            }
            // 起動できなかったなら、作ったばかりの worktree を残さず畳む
            // (中身は空なので force で消して構わない)。
            false => {
                let _ = worktree::remove_agent_worktree(&wt, true);
            }
        }
    }

    // ═══════════════════════════════════════════════════════════════════
    //  起動バー (⌃1〜⌃9) — 打鍵 1 つでプリセットを起動する
    //
    //  **番号は固定**。スロット → プリセットの対応を決めるのは
    //  `config::quick_launch_slots` という純粋関数だけで、その入力は
    //  「プリセット一覧」と「ユーザーが保存した並び」しか無い。
    //  使用頻度・未読・通知はどこからも入らない (cmux が HN で批判された
    //  「通知順で並べ替えたら ⌘1-9 の割当が動き続ける」を構造的に禁じる)。
    // ═══════════════════════════════════════════════════════════════════

    /// いまのスロット割り当て (添字 0 が ⌃1)。中身はプリセットの添字。
    pub(super) fn quick_slots(&self) -> Vec<usize> {
        crate::config::quick_launch_slots(&self.cfg.agents, self.cfg.quick_launch.as_deref())
    }

    /// スロット番号 (1〜9) のプリセットを起動する。空きスロットは何もしない。
    pub(super) fn launch_quick_slot(&mut self, slot: usize, isolated: bool, ctx: &egui::Context) {
        let Some(i) = slot
            .checked_sub(1)
            .and_then(|ix| self.quick_slots().get(ix).copied())
        else {
            // 割り当てが無い番号は**黙って無視**する (トーストで叱らない —
            // 起動バーを見れば何番が空きかは分かる)。
            return;
        };
        match isolated {
            true => self.launch_preset_isolated(i, ctx),
            false => self.launch_preset(i, ctx),
        }
    }

    /// 起動バーの並びを保存する (state.toml)。**渡された順のまま**書く。
    pub(super) fn save_quick_slots(&mut self, slots: &[usize]) {
        self.cfg.quick_launch = Some(crate::config::quick_launch_names(&self.cfg.agents, slots));
        crate::config::save_state(&self.cfg);
    }

    /// スロットの並べ替え / 取り外し / 追加。`Cmd` を増やさずに済むよう、
    /// 起動バーの右クリックメニューから直接呼ぶ。
    pub(super) fn edit_quick_slots(&mut self, edit: QuickBarEdit) {
        let mut slots = self.quick_slots();
        match edit {
            QuickBarEdit::MoveLeft(ix) => {
                if ix > 0 && ix < slots.len() {
                    slots.swap(ix - 1, ix);
                }
            }
            QuickBarEdit::MoveRight(ix) => {
                if ix + 1 < slots.len() {
                    slots.swap(ix, ix + 1);
                }
            }
            QuickBarEdit::Remove(ix) => {
                if ix < slots.len() {
                    slots.remove(ix);
                }
            }
            QuickBarEdit::Add(preset) => {
                if !slots.contains(&preset) && slots.len() < crate::config::QUICK_LAUNCH_SLOTS {
                    slots.push(preset);
                }
            }
            QuickBarEdit::Reset => {
                self.cfg.quick_launch = None;
                crate::config::save_state(&self.cfg);
                return;
            }
        }
        self.save_quick_slots(&slots);
    }

    /// エージェントタブの名前を手で付け直す入口 (自動命名より常に優先される)。
    pub(super) fn begin_rename_agent(&mut self, i: usize) {
        let Some(s) = self.agents.sessions.get(i) else {
            return;
        };
        self.rename_agent = Some((s.id, s.title.clone()));
    }

    pub(super) fn launch_preset(&mut self, i: usize, ctx: &egui::Context) {
        let cmd = self.cfg.agents.get(i).map(|p| p.command.clone());
        let Some(cmd) = cmd else { return };
        let cwd = self.agent_cwd();
        self.launch_preset_with(i, cmd, &cwd, ctx);
    }

    /// プリセット `i` を「コマンドと作業ディレクトリだけ差し替えて」起動する。
    ///
    /// 過去セッションの再開 (`--resume <id>` 付き) と、フォルダを指定した新規会話が
    /// これを通る。承認モードの判定・トーストは通常の起動とまったく同じ
    /// (再開だけ全自動判定が違う、といったズレを作らないため)。
    pub(super) fn launch_preset_with(
        &mut self,
        i: usize,
        command: String,
        cwd: &Path,
        ctx: &egui::Context,
    ) {
        // 既定は**いまの設定**の承認モード。
        let approval = crate::agents::Approval::from_mode(&self.cfg.approval_mode);
        self.launch_preset_as(i, command, cwd, approval, ctx);
    }

    /// **承認モードを明示して**起こす。
    ///
    /// Team の Run は「この Run にだけ効く締め具合」を持つ (既存のグローバル
    /// 設定は書き換えない)。その値をここへ渡す — 設定を書き換えてから
    /// 起こす形にすると、Run を 1 本作る操作で Zaivern 全体の承認モードが
    /// 変わってしまう。
    pub(super) fn launch_preset_as(
        &mut self,
        i: usize,
        command: String,
        cwd: &Path,
        approval: crate::agents::Approval,
        ctx: &egui::Context,
    ) {
        use crate::agents::{
            apply_approval, command_is_bypass, env_enables_auto, merged_env, spec_for_command,
            Approval,
        };
        let Some(mut p) = self.cfg.agents.get(i).cloned() else {
            return;
        };
        // 再開・フォルダ指定はコマンドと cwd だけ差し替える (名前・アイコン・env は据え置き)
        p.command = command;
        p.cwd = Some(cwd.display().to_string());
        // 実際に起動されるコマンドで bypass かどうかを判定する
        // (Agent優先モードではプリセットのフラグがそのまま効く)
        //
        // goose / aider は全自動フラグを持たず環境変数でしか自動承認できないため、
        // コマンド文字列だけを見る command_is_bypass では取りこぼす。
        // 環境変数側の判定も OR で足さないと、全自動で動いているのに
        // 🛡(承認あり) と表示してしまう。
        let launched = apply_approval(&p.command, approval);
        let env = merged_env(&p.command, approval, &p.env);
        let is_bypass = command_is_bypass(&launched) || env_enables_auto(&p.command, &env);
        // カタログにあるエージェントかどうかで判定する。
        // 先頭トークンの直接比較だと /usr/local/bin/claude のような
        // 絶対パス指定で一致に失敗する(spec_for_command は末尾要素で照合する)。
        let is_agent_cli = spec_for_command(&p.command).is_some();
        let via = if approval == Approval::Agent {
            tr("（Agent欄の指定どおり）")
        } else {
            tr("（既定モード）")
        };
        match self.agents.launch(&p, cwd, approval, ctx) {
            Ok(()) => {
                if is_agent_cli && is_bypass {
                    self.toast_warn(trf(
                        "⚡ {name} を全自動モードで起動しました{via}",
                        &[("name", p.name.clone()), ("via", via)],
                    ));
                } else if is_agent_cli {
                    self.toast(
                        trf(
                            "🛡 {name} を承認モードで起動しました{via}",
                            &[("name", p.name.clone()), ("via", via)],
                        ),
                        true,
                    );
                } else {
                    self.toast(
                        trf(
                            "{icon} {name} を起動しました",
                            &[("icon", p.icon.clone()), ("name", p.name.clone())],
                        ),
                        true,
                    );
                }
            }
            Err(e) => self.toast(e, false),
        }
    }

    /// エージェントを閉じる共通口 (✕ ボタン・看板・パレット・リモートすべてここ)。
    ///
    /// 後始末そのものは [`crate::terminal::reap`] が別スレッドで持っていくので、
    /// ここは UI 側の後片付けだけを見る。閉じた端末が持っていたキーボード
    /// フォーカスは egui ごと消えてしまうため、残ったセッションへ渡し直す
    /// (渡さないと入力の行き先が無くなり、「閉じたら操作を受け付けなくなった」
    /// ように見える)。
    pub(super) fn close_agent(&mut self, i: usize) {
        let _ = self.close_agent_tracked(i);
    }

    /// Team RunのClose用。通常のUI後始末に加え、プロセスとPTYが実際に
    ///畳まれたことを呼び出し側が確認できる札を返す。
    pub(super) fn close_agent_tracked(&mut self, i: usize) -> Option<crate::terminal::ReapHandle> {
        // セッション ID は再利用され得るので、フェイルオーバーの段も一緒に忘れる
        // (残すと別セッションの状態として読まれてしまう)。
        let mut freed: Option<worktree::AgentWorktree> = None;
        // エージェント別履歴の締め (終了時刻 + 最初の指示の要約)。
        // `agents.remove` に渡すと Session ごと別スレッドへ流れて読めなくなるので、
        // **消す前に**書く。書けなくても閉じる操作は続ける。
        if let Some(s) = self.agents.sessions.get(i) {
            if let Some(bin) = crate::agents::spec_for_command(&s.command).map(|sp| sp.bin) {
                let _ = crate::history::finish(
                    bin,
                    &s.cwd,
                    s.id,
                    crate::history::now_unix(),
                    s.last_prompt.as_deref().unwrap_or_default(),
                );
            }
        }
        if let Some(id) = self.agents.sessions.get(i).map(|s| s.id) {
            self.failover.forget_session(id);
            // 自動命名の状態も一緒に忘れる。セッション ID は再利用され得るので、
            // 残すと別のセッションの「命名済み」として読まれてしまう。
            self.turns.forget(id);
            self.namer.forget(id);
            self.named_for.remove(&id);
            self.manual_titles.remove(&id);
            if self
                .rename_agent
                .as_ref()
                .is_some_and(|(rid, _)| *rid == id)
            {
                self.rename_agent = None;
            }
            freed = self.agent_worktrees.remove(&id);
        }
        let reaping = self.agents.remove_tracked(i);
        // 隔離 worktree を持っていたなら、**残すか消すかをユーザーに選ばせる**。
        // 黙って消すと未コミットの成果ごと消える (git 自身の拒否も回避してしまう)。
        if let Some(wt) = freed {
            let dirty = worktree::worktree_is_dirty(&wt.dir);
            self.pending_worktree = Some((wt, dirty));
        }
        // 閉じたセッションが分割ペインだったら、木からも外して畳む
        // (残り 1 枚になったタイルは分割なしの描画へ戻る)。
        self.normalize_splits();
        if !self.agents.sessions.is_empty() {
            self.term_focus_pending = true;
        }
        reaping
    }

    pub(super) fn send_to_agent(&mut self, text: String) {
        let Some(id) = self.agents.active_session().map(|s| s.id) else {
            self.toast(
                tr("エージェントセッションがありません（👾 Agent＋ から起動）"),
                false,
            );
            return;
        };
        if !self.queue_submit(submit::Job::user(id, text)) {
            // 積めなかった理由 (コスト上限など) は queue_submit が説明済み
            return;
        }
        self.agents.panel_open = true;
        self.toast(tr("アクティブなエージェントに送信しました"), true);
    }

    // ══════════════════════════════════════════════════════════════════
    //  エージェントへの指示 — **送信経路はここ 1 本だけ**
    //
    //  以前は送信地点ごとに `format!("{text}\r")` を組み立てて PTY へ
    //  1 回で書いていた (Cockpit 指名 / 一斉 / かんばん / リモート /
    //  失敗切替の引き継ぎ / Issue 着手 で 6 箇所)。Ink 系 TUI
    //  (Claude Code / Codex / Gemini) は本文と CR が同じ write で届くと
    //  まとめてペーストと判定し、**CR を改行として飲んで実行しない**。
    //  「送ったのにエージェントが入力欄で待機している」がこれ。素のシェルは
    //  1 回でも動くので「エージェントによって効いたり効かなかったり」に見えた。
    //  手順 (本文 → 待つ → 確定キー → 効いたか確認) は `submit.rs` が持つ。
    // ══════════════════════════════════════════════════════════════════

    /// 指示 1 通を配達待ちへ積む。空文字と宛先不明は黙って捨てる。
    ///
    /// **コスト上限で止まっているときはここで弾く。** 送信経路が 1 本なので、
    /// 見張りもここ 1 か所で済む。**黙って捨てない** — なぜ送れないかを
    /// トーストで説明する。
    /// 戻りは「配達待ちへ積めたか」。呼び出し側は積めなかったときに
    /// 「送信しました」と嘘をつかないこと。
    pub(super) fn queue_submit(&mut self, mut job: submit::Job) -> bool {
        if job.text.trim().is_empty() {
            return false;
        }
        if let Some(why) = self.cost_block_reason() {
            self.toast(why, false);
            return false;
        }
        let Some(s) = self.agents.sessions.iter().find(|s| s.id == job.session) else {
            return false;
        };
        job.title = s.title.clone();
        // 送る**直前**の作業ツリーを 1 枚残す。承認キューは「通す前」しか
        // 守れないので、通した後に暴走した変更を戻せる唯一の足場がこれ。
        // 実際の取得は ctx を持つ `submit_tick` が仕込む。
        if self.checkpoint_pending.is_none() {
            self.checkpoint_pending =
                Some((job.title.clone(), checkpoint::one_line(&job.text, 160)));
        }
        self.outbox.push(submit::Pending::new(job, Instant::now()));
        true
    }

    /// 起動中の全セッションへ同じ指示を積む (Cockpit の一斉送信)。宛先数を返す。
    /// **止まっているセッションだけ**へ同じ本文を積む。届けた数を返す。
    ///
    /// 対象は「生きていて、かつ [`supervisor::SessionState::is_stuck`]」。
    /// 状態が取れないセッション (起動直後で supervisor がまだ何も見ていない) は
    /// **対象にしない** — 「分からないもの」を止まっている扱いにすると、
    /// 立ち上がったばかりのエージェントへ横から指示が刺さる。
    /// **`None` = コスト上限で止まっている** (理由はトーストで説明済み)。
    /// 宛先ごとに 1 回ずつ理由を出すとうるさいので、ここで一度だけ弾く。
    pub(super) fn queue_submit_stalled(&mut self, text: &str) -> Option<usize> {
        if let Some(why) = self.cost_block_reason() {
            self.toast(why, false);
            return None;
        }
        let ids: Vec<u64> = self.stalled_session_ids();
        for id in &ids {
            self.queue_submit(submit::Job::user(*id, text));
        }
        Some(ids.len())
    }

    /// 止まっているセッションの ID (起動順)。チップの件数表示と送信で共有する。
    pub(super) fn stalled_session_ids(&self) -> Vec<u64> {
        self.agents
            .sessions
            .iter()
            .filter(|s| s.running())
            .filter(|s| {
                self.supervisor
                    .state_of(s.id)
                    .is_some_and(|st| st.is_stuck())
            })
            .map(|s| s.id)
            .collect()
    }

    /// **`None` = コスト上限で止まっている** (理由はトーストで説明済み)。
    pub(super) fn queue_submit_all(&mut self, text: &str) -> Option<usize> {
        if let Some(why) = self.cost_block_reason() {
            self.toast(why, false);
            return None;
        }
        let ids: Vec<u64> = self
            .agents
            .sessions
            .iter()
            .filter(|s| s.running())
            .map(|s| s.id)
            .collect();
        for id in &ids {
            self.queue_submit(submit::Job::user(*id, text));
        }
        Some(ids.len())
    }

    /// 配達待ちを 1 フレームぶん進める。
    ///
    /// **アイドル時のコストはゼロ** — 待ちが空なら即 return し、再描画も
    /// 要求しない。待ちがある間だけ次の期限へ `request_repaint_after` する
    /// (常時再描画にはしない)。
    pub(super) fn submit_tick(&mut self, ctx: &egui::Context) {
        if self.outbox.is_empty() {
            return;
        }
        // 配達の**直前**に作業ツリーを 1 枚残す。`queue_submit` は
        // `egui::Context` を持たないので、予約の消化はここで行う
        // (この関数は配達を進める唯一の経路なので、取り漏らしが起きない)。
        if let Some((agent, note)) = self.checkpoint_pending.take() {
            self.checkpoints.capture_before_submit(&agent, &note, ctx);
            // ローカルヒストリにもターン境界の 1 枚を残す。git 側と違い
            // `.gitignore` の外や未追跡も含めてファイルシステムから撮るので、
            // エージェントの shell が書いた変更もここに入る。
            self.local_history.snapshot(&agent);
        }
        let now = Instant::now();
        let mut next: Option<Duration> = None;
        let mut delivered: Vec<String> = Vec::new();
        let mut gave_up: Vec<String> = Vec::new();
        // **終わり方を目印つきで拾う** (`(目印, 本当に届いたか)`)。
        // 積めたことと届いたことは別の時刻に決まるので、頼んだ側へは
        // ここでしか本当のことを返せない。
        let mut outcomes: Vec<(String, bool)> = Vec::new();
        let mut queue = std::mem::take(&mut self.outbox);
        let sup = &self.supervisor;
        let agents = &mut self.agents;
        queue.retain_mut(|p| {
            let sid = p.job.session;
            let idle = matches!(sup.state_of(sid), Some(supervisor::SessionState::Idle));
            let Some(s) = agents.sessions.iter_mut().find(|s| s.id == sid) else {
                // セッションが消えた。**黙って捨てない** — 頼んだ側は
                // 「届いた」と思ったまま待ち続けることになる。
                if let Some(t) = p.job.tag.clone() {
                    outcomes.push((t, false));
                }
                return false;
            };
            let bracketed = s.running() && s.bracketed_paste();
            let peek = submit::Peek {
                // **起動直後は書かない。** 待つ長さはカタログが持つので、
                // ここに CLI ごとの分岐は作らない。
                // 起動時プロンプトに答えた直後も同じだけ待つ (答えた 71ms 後に
                // 貼った指示が丸ごと消えた実測がある)。判定は `submit::input_ready`
                // 1 か所。
                input_ready: s
                    .agent_bin()
                    .map(|b| {
                        submit::input_ready(
                            s.age(),
                            s.since_startup_reply(),
                            std::time::Duration::from_millis(crate::agents::input_ready_ms(b)),
                        )
                    })
                    .unwrap_or(true),
                running: s.running(),
                idle,
                attention: s.attention,
                bracketed,
                // 入力欄の読み取りは [`peek_input_at`] が決める段だけ。
                input: peek_input_at(p.job.stage, p.job.submit)
                    .then(|| s.input_text())
                    .flatten(),
            };
            let mut soon = |d: Duration| {
                next = Some(next.map_or(d, |n: Duration| n.min(d)));
            };
            match p.act(&peek, now) {
                submit::Act::Done => {
                    if let Some(t) = p.job.tag.clone() {
                        outcomes.push((t, true));
                    }
                    false
                }
                // **相手が消えた = 届いていない。** 本文を書いた後でも、
                // 確定キーが効いたことは確かめられていない。
                submit::Act::Gone => {
                    if let Some(t) = p.job.tag.clone() {
                        outcomes.push((t, false));
                    }
                    false
                }
                submit::Act::GaveUp => {
                    gave_up.push(s.title.clone());
                    if let Some(t) = p.job.tag.clone() {
                        outcomes.push((t, false));
                    }
                    false
                }
                submit::Act::Wait(d) => {
                    soon(d);
                    true
                }
                submit::Act::WriteBody => {
                    // 手入力と同じ扱い (承認エピソードのラッチを立てる)。
                    s.note_user_input();
                    // 失敗切替で別プロファイルへ引き継ぐ材料として覚えておく。
                    s.note_prompt(&p.job.text);
                    s.write_bytes(&submit::body_bytes(&p.job.text, peek.bracketed));
                    s.set_scroll(0);
                    if p.job.wait_idle {
                        delivered.push(s.title.clone());
                    }
                    p.advance(submit::Stage::Commit, now);
                    soon(submit::COMMIT_DELAY);
                    true
                }
                submit::Act::WriteCommit => {
                    // 確定キーもカタログから引く (CLI ごとに違いうる)。
                    let keys = s
                        .agent_bin()
                        .map(crate::agents::commit_keys)
                        .unwrap_or(submit::COMMIT);
                    s.write_bytes(keys);
                    p.job.tries = p.job.tries.saturating_add(1);
                    p.advance(submit::Stage::Verify, now);
                    soon(submit::VERIFY_DELAY);
                    true
                }
            }
        });
        self.outbox = queue;
        // **配達の結末を頼んだ側へ 1 回だけ返す。** 送信経路は増やさない
        // (ここは結果を伝えるだけで、PTY へは 1 バイトも書かない)。
        if !outcomes.is_empty() {
            self.note_submit_delivery(outcomes);
        }
        for title in delivered {
            self.toast(
                trf("📋 {title} へ指示を配達しました", &[("title", title)]),
                true,
            );
        }
        for title in gave_up {
            self.toast_warn(trf(
                "指示文を配達できませんでした ({title}): セッションが落ち着きません",
                &[("title", title)],
            ));
        }
        if let Some(d) = next {
            crate::perf::repaint_after(ctx, d, "submit_tick");
        }
    }
}

/// **その段で入力欄を読むか** (純関数)。
///
/// [`submit::Peek::input`] は「自分が書いた本文がまだ入力欄に残っているか」
/// だけを見る材料で、読むには端末のパーサをロックして画面を走査する。
/// だから**要る段だけ**で読む。
///
/// * [`submit::Stage::Ready`] — まだ 1 バイトも書いていないので、
///   残っているかを問う意味が無い
/// * [`submit::Stage::Commit`] — **読む** (`job.submit` のときだけ)。
///   `submit` が偽なら [`submit::decide`] はその場で `Done` を返すので、
///   読んでも捨てるだけ
/// * [`submit::Stage::Verify`] — 読む (撃った確定キーが効いたかを見る)
///
/// **`Commit` を読んでいなかったことが、実機の「配達したことになっている
/// のに 1 文字も届いていない」の片割れである。** `submit.rs` の `Commit` は
/// 「本文が入力欄に見えなければ書き直す」判定を持っているのに、`input` が
/// 常に `None` で渡っていた。`Peek::input_seen(None)` は**真**を返す設計
/// (読めない相手で書き直しを繰り返さないため) なので、書き直しの枝は
/// **実機で一度も通っていなかった** — 起動中の CLI が本文を捨てても
/// そのまま確定キーへ進み、`Verify` は「入力欄に本文が残っていない」を見て
/// **届いたと判断する**。1 バイトも送っていないのに配達完了になる。
///
/// アイドル時のコストは増えない: `submit_tick` は待ちが空なら最初の行で
/// 帰るので、**読むのは配達中の 1 通につき 1 回**だけ。読む回数は
/// `Verify` と同じ刻み ([`submit::POLL`]) で、増えるのは配達 1 通あたり
/// 高々 `COMMIT_IDLE_WAIT / POLL` 回。
pub(super) fn peek_input_at(stage: submit::Stage, submit: bool) -> bool {
    match stage {
        submit::Stage::Ready => false,
        submit::Stage::Commit => submit,
        submit::Stage::Verify => true,
    }
}

/// **配達の 2 つの穴の番人。**
///
/// どちらも「台帳に 1 行も残らないまま担当が `running` で放置される」
/// という同じ形で出る (実機: 6 体中 2 体・28 分)。
#[cfg(test)]
mod delivery_peek_tests {
    use super::peek_input_at;
    use crate::submit::Stage;

    /// **`Commit` 段でも入力欄を読む。**
    ///
    /// ここが偽に戻ると、`submit.rs` の「本文が入力欄に見えなければ
    /// 書き直す」判定 (`peek.input_seen`) は `input: None` を受け取り、
    /// **必ず真**を返して発火しなくなる (`input_seen` は読めない相手で
    /// 書き直しを繰り返さないよう `None` を真とする)。
    #[test]
    fn commit_段でも入力欄を読む() {
        // (段, 確定キーまで送るか) → 読むか
        let table = [
            (Stage::Ready, true, false),
            (Stage::Ready, false, false),
            (Stage::Commit, true, true),
            (Stage::Commit, false, false),
            (Stage::Verify, true, true),
            (Stage::Verify, false, true),
        ];
        for (stage, submit, want) in table {
            assert_eq!(
                peek_input_at(stage, submit),
                want,
                "{stage:?} / submit={submit} の読み取り判断が違う"
            );
        }
    }

    /// **判断を `submit_tick` が実際に通していること。**
    ///
    /// 純関数だけ直しても、呼び出し側が `matches!` を書き戻せば実機は
    /// 元のままになる (「作ったのに繋いでいない」)。
    #[test]
    fn submit_tick_は入力欄の読み取りをこの判断へ委ねている() {
        let src = include_str!("agent_sessions.rs").replace("\r\n", "\n");
        let at = src
            .find("pub(super) fn submit_tick")
            .expect("submit_tick が無い");
        let body = &src[at..];
        let end = body.find("\n    }\n").unwrap_or(body.len());
        let body = &body[..end];
        assert!(
            body.contains("input: peek_input_at(p.job.stage, p.job.submit)"),
            "入力欄の読み取りを段で決めていない (Commit で読まなくなる):\n{body}"
        );
        // **アイドル時のコストはゼロのまま。** 待ちが空なら読む前に帰る。
        assert!(
            body.contains("if self.outbox.is_empty() {"),
            "待ちが空でも画面を舐めている:\n{body}"
        );
    }
}
