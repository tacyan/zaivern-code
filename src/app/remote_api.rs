use super::*;

impl ZaivernApp {
    /// リモートサーバに溜まったリクエストを処理して応答する。毎フレーム呼ぶ。
    pub(super) fn poll_remote(&mut self, ctx: &egui::Context) {
        let reqs: Vec<remote::Request> = match &self.remote {
            Some(r) => r.poll(),
            None => return,
        };
        for req in reqs {
            let json = self.remote_reply(&req.query, ctx);
            req.respond(json);
        }
    }

    /// リモートからの問い合わせ 1 件に応答 JSON を返す。
    pub(super) fn remote_reply(&mut self, q: &remote::Query, ctx: &egui::Context) -> String {
        match q {
            remote::Query::State => self.remote_reply_state(),
            remote::Query::File => self.remote_reply_file(),
            remote::Query::Files => self.remote_reply_files(),
            remote::Query::SetText { text, index, save } => {
                self.remote_reply_set_text(text, *index, *save)
            }
            remote::Query::OpenFile(rel, line) => self.remote_reply_open_file(rel, *line),
            // プラグイン / CLI からのトースト通知
            remote::Query::Notify(message, level) => self.remote_reply_notify(message, level),
            // プラグインパネルの本文を書き換える
            remote::Query::SetPanel {
                plugin,
                panel,
                text,
            } => self.remote_reply_set_panel(plugin, panel, text),
            // ステータスバーへ任意の文字列を出す (空文字で消える)
            remote::Query::SetStatus(text) => self.remote_reply_set_status(text),
            // エージェントの入力欄へ差し込む (submit=true のときだけ Enter)
            remote::Query::Prompt {
                text,
                agent,
                submit,
            } => self.remote_reply_prompt(text, agent, *submit),
            remote::Query::Tab(i) => self.remote_reply_tab(*i),
            remote::Query::Term => self.remote_reply_term(),
            remote::Query::VoiceSend { text, id, submit } => {
                self.remote_reply_voice_send(text, *id, *submit)
            }
            remote::Query::TermInput(payload, raw) => self.remote_reply_term_input(payload, *raw),
            // 待ち一覧 / デッキ / 看板が読む 1 本
            remote::Query::Agents => self.remote_reply_agents(),
            remote::Query::AgentAct { id, act } => self.remote_reply_agent_act(*id, *act),
            // 一括送信 / 一括停止 (スマホの「全員 / 待機 / 選択中」)
            remote::Query::Bulk { text, mode, submit } => {
                self.remote_reply_bulk(text, *mode, *submit)
            }
            remote::Query::BulkStop { mode } => self.remote_reply_bulk_stop(*mode),
            remote::Query::Cmd(name, arg) => self.remote_reply_cmd(name, *arg, ctx),
            // ─── スマホを PC と同じ土俵に載せる読み取り ───
            // git と横断検索は**必ず裏のスレッド**。ここでは控えを見るだけ
            remote::Query::Changes => self.remote_reply_changes(),
            remote::Query::Diff { rel } => self.remote_reply_diff(rel),
            remote::Query::Scrollback {
                agent,
                lines,
                before,
            } => self.remote_reply_scrollback(*agent, *lines, *before),
            remote::Query::Approvals => self.remote_reply_approvals(),
            remote::Query::Approve { id, act } => self.remote_reply_approve(*id, *act),
            remote::Query::Read { rel, from, lines } => self.remote_reply_read(rel, *from, *lines),
            remote::Query::Search { q, max } => self.remote_reply_search(q, *max),
        }
    }

    /// remote_reply: State — タブ・エージェント・カーソル等の全体状態。
    pub(super) fn remote_reply_state(&self) -> String {
        use serde_json::json;
        let ws = roots_label(&self.roots);
        let tabs: Vec<_> = self
            .editor
            .buffers
            .iter()
            .map(|b| json!({"title": b.title, "dirty": b.dirty()}))
            .collect();
        let (file, dirty) = match self.editor.active {
            Some(i) => (
                self.editor.buffers[i].title.clone(),
                self.editor.buffers[i].dirty(),
            ),
            None => (String::new(), false),
        };
        // 一括操作の宛先。判定は PC 側 (`stalled_session_ids` = supervisor) から
        // 取る — 画面の見た目からは推測しない (設計原則 4)。
        let picks = self.bulk_picks();
        let agents: Vec<_> = self
            .agents
            .sessions
            .iter()
            .zip(picks.iter())
            .map(|(s, p)| {
                json!({
                    "id": s.id, "title": s.title, "icon": s.icon,
                    "running": s.running(), "attention": s.attention,
                    // 止まっている (待機中) か。スマホのチップの ⏸ 印に使う
                    "stalled": p.stalled,
                })
            })
            .collect();
        let presets: Vec<_> = self
            .cfg
            .agents
            .iter()
            .map(|p| json!({"name": p.name, "icon": p.icon}))
            .collect();
        // 「待ち」の件数。スマホのビュー切替バッジがこれを出す。
        // **数え方は `/api/agents` と同じ 1 本** (`remote::is_waiting_lane`) —
        // ここで別に数えると「バッジ 3 なのに一覧は 5 件」になる。
        // PTY は読まない (`column_for` は画面末尾を使わない) ので、
        // 一覧を開いていない間の費用はゼロのまま。
        let waiting = self
            .agents
            .sessions
            .iter()
            .filter(|s| {
                remote::is_waiting_lane(kanban::column_for(
                    s.running(),
                    s.attention,
                    s.rate_limited.is_some(),
                    self.supervisor.state_of(s.id),
                ))
            })
            .count();
        json!({
            "ok": true, "workspace": ws, "tabs": tabs,
            "active": self.editor.active, "file": file, "dirty": dirty,
            "cursor": [self.editor.cursor.0, self.editor.cursor.1],
            "agents": agents, "agent_active": self.agents.active,
            "presets": presets, "approval": self.cfg.approval_mode,
            // 「待ち」ビューのバッジ (/api/agents の waiting と同じ数え方)
            "waiting": waiting,
            // 一括操作の宛先数。**数えるのはここ 1 か所だけ** — スマホ側でも
            // 数えると「3 体と出ているのに 5 体へ飛ぶ」がいずれ起きる。
            "bulk": {
                "all": remote::bulk_targets(remote::BulkMode::All, &picks).len(),
                "stalled": remote::bulk_targets(remote::BulkMode::Stalled, &picks).len(),
                "one": remote::bulk_targets(remote::BulkMode::One, &picks).len(),
            },
            // 音声入力ページ (スマホ) が参照する設定
            "voice": {"kw": self.cfg.voice_keyword, "lang": self.cfg.voice_lang},
        })
        .to_string()
    }

    /// remote_reply: File — アクティブバッファの本文。
    pub(super) fn remote_reply_file(&self) -> String {
        use serde_json::json;
        match self.editor.active {
            Some(i) => {
                let b = &self.editor.buffers[i];
                json!({
                    "ok": true, "title": b.title, "text": b.text,
                    "lang": b.lang, "dirty": b.dirty(), "index": i,
                    // UTF-8 以外のときだけ中身が入る (PC 側のステータスバーと同じ)。
                    // スマホからも「このファイルは何で保存されるのか」が見える。
                    "encoding": b.encoding.label(),
                })
                .to_string()
            }
            None => json!({"ok": false}).to_string(),
        }
    }

    /// remote_reply: Files — ワークスペースのファイル一覧。
    pub(super) fn remote_reply_files(&self) -> String {
        use serde_json::json;
        // 全ルートの索引を表示ラベル (曖昧なときだけルート名付き) で返す。
        // OpenFile ではこのラベルを索引で引き直して絶対パスに戻す。
        let files: Vec<&String> = self
            .file_index
            .iter()
            .take(4000)
            .map(|f| &f.label)
            .collect();
        json!({"ok": true, "files": files}).to_string()
    }

    /// remote_reply: SetText — バッファ本文を丸ごと置き換える (+必要なら保存)。
    pub(super) fn remote_reply_set_text(&mut self, text: &str, index: i64, save: bool) -> String {
        use serde_json::json;
        let Some(active) = self.editor.active else {
            return json!({"ok": false, "error": "ファイルが開かれていません"}).to_string();
        };
        // スマホが編集していたタブと PC のアクティブタブが違えば拒否
        // (別ファイルを誤って上書きしない)
        if index >= 0 && index as usize != active {
            return json!({
                "ok": false,
                "error": "PC 側でタブが切り替わっています — 再読込してください",
            })
            .to_string();
        }
        // PR 差分などの読み取り専用タブはスマホ側からも書き換えさせない
        if self.editor.buffers[active].kind.read_only() {
            return json!({
                "ok": false,
                "error": "このタブは読み取り専用です (PR 差分などは編集できません)",
            })
            .to_string();
        }
        let ed = self.edit_step();
        let b = &mut self.editor.buffers[active];
        // スマホ側からの差し替えも PC 側で ⌘Z 1 回で戻せるようにする
        b.apply_edit(text.to_string(), ed);
        if !save {
            return json!({"ok": true, "dirty": b.dirty()}).to_string();
        }
        // 保存も同一リクエストで原子的に行う。rfd ダイアログは開かない
        let Some(path) = b.path.clone() else {
            return json!({
                "ok": false,
                "error": trf(
                    "名前のないファイルは PC 側で保存してください ({key})",
                    &[("key", self.key_hint(BindAction::Save))],
                ),
            })
            .to_string();
        };
        // 元の符号化で表せない文字が入っていれば UTF-8 へ格上げされる。
        // 黙って変えるとスマホ側は何も分からないので、返事にも入れて知らせる。
        let was = b.encoding;
        match b.write_to(&path) {
            Ok(promoted) => {
                b.mark_saved();
                b.disk_mtime = disk_mtime(&path);
                b.conflict_notified = None;
                let enc = b.encoding.label();
                self.tree.invalidate();
                if promoted {
                    self.toast_warn(trf(
                        "💾 保存しました (スマホから): {path}\n\u{3000}{from} では表せない文字があるため UTF-8 で保存しました",
                        &[
                            ("path", path.display().to_string()),
                            ("from", was.label()),
                        ],
                    ));
                } else {
                    self.toast(
                        trf(
                            "💾 保存しました (スマホから): {path}",
                            &[("path", path.display().to_string())],
                        ),
                        true,
                    );
                }
                json!({
                    "ok": true, "dirty": false,
                    "promoted": promoted, "was": was.label(), "encoding": enc,
                })
                .to_string()
            }
            Err(e) => json!({"ok": false, "error": format!("保存に失敗しました: {e}")}).to_string(),
        }
    }

    /// remote_reply: OpenFile — ワークスペース相対パスのファイルを開く。
    pub(super) fn remote_reply_open_file(&mut self, rel: &str, line: Option<usize>) -> String {
        use serde_json::json;
        // `..` を含む要求は入口で拒否する。
        // canonicalize は「存在しないパス」で失敗し、その場合の
        // フォールバック比較は `..` を解決しないまま前方一致してしまうため、
        // 後段のチェックに任せずここで落とす。
        if Path::new(rel)
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return json!({"ok": false, "error": "ワークスペース外は開けません"}).to_string();
        }
        // 索引の表示ラベル → 絶対パス (マルチルートでも一意に定まる)。
        // 索引に無ければ各ルートからの相対パスとして解決を試みる。
        let p = self
            .file_index
            .iter()
            .find(|f| f.label == *rel)
            .map(|f| f.abs.clone())
            .or_else(|| self.roots.iter().map(|r| r.join(rel)).find(|c| c.is_file()))
            .unwrap_or_else(|| self.primary_root().join(rel));

        // パストラバーサル防御 (セキュリティ上の要): canonicalize したうえで
        // 「いずれかのルート配下」でなければ開かせない。ルートが増えても
        // 判定は緩めず、各ルートについて同じ前方一致チェックを行う。
        let canon = p.canonicalize().unwrap_or_else(|_| p.clone());
        let inside = self.roots.iter().any(|r| {
            let root = r.canonicalize().unwrap_or_else(|_| r.clone());
            canon.starts_with(&root)
        });
        if !inside {
            return json!({"ok": false, "error": "ワークスペース外は開けません"}).to_string();
        }
        match self.editor.open(&p, self.highlighter) {
            Ok(reloaded) => {
                if reloaded {
                    if let Some(i) = self.editor.active {
                        self.queue_lsp_change(i);
                    }
                }
                self.queue_hook(plugins::HookEvent::FileOpen, Some(p.clone()));
                self.persist_session();
                if let Some(n) = line {
                    self.goto_line(n);
                }
                json!({"ok": true}).to_string()
            }
            Err(e) => json!({"ok": false, "error": e}).to_string(),
        }
    }

    /// remote_reply: Notify — プラグイン / CLI からのトースト通知。
    pub(super) fn remote_reply_notify(&mut self, message: &str, level: &str) -> String {
        use serde_json::json;
        let msg = notify::truncate_chars(message.trim(), 200);
        match level.trim() {
            "warn" => self.toast_warn(format!("🔌 {msg}")),
            "error" => self.toast(format!("🔌 {msg}"), false),
            _ => self.toast(format!("🔌 {msg}"), true),
        }
        json!({"ok": true}).to_string()
    }

    /// remote_reply: SetPanel — プラグインパネルの本文を書き換える。
    pub(super) fn remote_reply_set_panel(
        &mut self,
        plugin: &str,
        panel: &str,
        text: &str,
    ) -> String {
        use serde_json::json;
        // plugin が空なら、その panel id を持つ最初の有効プラグインへ送る
        let target = if plugin.trim().is_empty() {
            self.plugins
                .iter()
                .find(|p| p.active() && p.panels.iter().any(|x| x.id == panel))
                .map(|p| p.name.clone())
        } else {
            Some(plugin.to_string())
        };
        match target {
            Some(name) if self.set_plugin_panel(&name, panel, text.to_string()) => {
                json!({"ok": true, "plugin": name}).to_string()
            }
            _ => json!({
                "ok": false,
                "error": format!("パネルが見つかりません: {panel}"),
            })
            .to_string(),
        }
    }

    /// remote_reply: SetStatus — ステータスバーへ任意の文字列を出す (空文字で消える)。
    pub(super) fn remote_reply_set_status(&mut self, text: &str) -> String {
        use serde_json::json;
        self.plugin_status = text.to_string();
        json!({"ok": true}).to_string()
    }

    /// remote_reply: Prompt — エージェントの入力欄へ差し込む (submit=true のときだけ Enter)。
    pub(super) fn remote_reply_prompt(&mut self, text: &str, agent: &str, submit: bool) -> String {
        use serde_json::json;
        let text = text.trim().to_string();
        if text.is_empty() {
            return json!({"ok": false, "error": "テキストが空です"}).to_string();
        }
        if self.send_agent_prompt(Some(agent), &text, submit) {
            json!({"ok": true, "sent": 1}).to_string()
        } else {
            json!({
                "ok": false,
                "error": "エージェントセッションが見つかりません",
            })
            .to_string()
        }
    }

    /// remote_reply: Tab — タブ切替。
    pub(super) fn remote_reply_tab(&mut self, i: usize) -> String {
        use serde_json::json;
        if i < self.editor.buffers.len() {
            self.editor.active = Some(i);
            self.find.current = None;
            self.find_hits = None;
            json!({"ok": true}).to_string()
        } else {
            json!({"ok": false, "error": "タブがありません"}).to_string()
        }
    }

    /// remote_reply: Term — アクティブなエージェントのターミナル画面テキスト。
    pub(super) fn remote_reply_term(&mut self) -> String {
        use serde_json::json;
        match self.agents.active_session() {
            Some(s) => {
                let text = crate::lockx::lock_ok(&s.parser).screen().contents();
                json!({
                    "ok": true, "title": s.title, "running": s.running(), "text": text,
                })
                .to_string()
            }
            None => json!({"ok": false}).to_string(),
        }
    }

    /// remote_reply: Agents — 待ち一覧 / デッキ / 看板が読む 1 本。
    ///
    /// **レーンも状態ラベルも PC 側の判定をそのまま返す**
    /// (`kanban::column_for` / `kanban::state_label`)。スマホ側で画面文字を
    /// 見て状態を決め直すと、PC と食い違ううえに設計原則 4 に反する。
    /// 直近出力も `Session::screen_tail_lines` (看板カードと同じ関数) で取る。
    ///
    /// PTY を読むのはこの応答を作るときだけ。端末ビューを見ているスマホは
    /// `/api/term` しか叩かないので、一覧を開いていない間の費用はゼロ。
    pub(super) fn remote_reply_agents(&self) -> String {
        use serde_json::json;
        let stalled = self.stalled_session_ids();
        let active = self.agents.active;
        // レーン別の件数。看板の見出しに出す (0 本の見出しはページ側が畳む)
        let mut counts = [0usize; kanban::LANES];
        let agents: Vec<_> = self
            .agents
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let running = s.running();
                let col = kanban::column_for(
                    running,
                    s.attention,
                    s.rate_limited.is_some(),
                    self.supervisor.state_of(s.id),
                );
                counts[col.index()] += 1;
                // 直近の出力は 2 行だけ。行あたりの桁も詰める
                // (スマホの幅で折り返すと 1 行のカードが画面を埋める)
                let tail = s.screen_tail_lines(2, 120);
                json!({
                    "id": s.id, "idx": i, "title": s.title,
                    "icon": if s.icon.is_empty() { "👾" } else { &s.icon },
                    "running": running,
                    "attention": s.attention && running,
                    "stalled": stalled.contains(&s.id),
                    "active": i == active,
                    "unread": s.has_unread(),
                    "lane": col.index(),
                    "state": tr(kanban::state_label(
                        running,
                        s.attention,
                        s.rate_limited.is_some(),
                        self.supervisor.state_of(s.id),
                    )),
                    // 「待ち」一覧に載せるか。判定は remote::is_waiting_lane 1 か所
                    "waiting": remote::is_waiting_lane(col),
                    "uptime": s.uptime(),
                    "preview": tail.join("\n"),
                })
            })
            .collect();
        let lanes: Vec<_> = kanban::COLUMNS
            .iter()
            .map(|c| {
                json!({
                    "i": c.index(), "icon": c.icon(),
                    "title": tr(c.title()), "n": counts[c.index()],
                })
            })
            .collect();
        json!({"ok": true, "agents": agents, "lanes": lanes}).to_string()
    }

    /// remote_reply: AgentAct — 一覧の行から撃つ 1 体宛ての操作。
    ///
    /// 承認は PC 側と**同じ入口** (`press_pet_approve_button`) を通す。
    /// エージェントごとの承認キー (`y` / `1` / Enter…) はカタログが持っている
    /// ので、スマホ側で当て推量の文字を送らない。
    pub(super) fn remote_reply_agent_act(&mut self, id: i64, act: remote::AgentAct) -> String {
        use serde_json::json;
        let fallback = self.cfg.pet_approve_keys.clone();
        let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == id as u64) else {
            return json!({"ok": false, "error": tr("セッションが見つかりません (閉じられた可能性)")})
                .to_string();
        };
        if !s.running() {
            return json!({"ok": false, "error": tr("セッションが停止しています")}).to_string();
        }
        let title = s.title.clone();
        match act {
            remote::AgentAct::Approve => {
                if s.press_pet_approve_button(Some(&fallback)) {
                    self.toast(trf("✅ {title} を承認しました", &[("title", title)]), true);
                    json!({"ok": true}).to_string()
                } else {
                    // 承認キーが分からないまま当て推量を送らない。
                    // 端末ビューの [y] [1] [Enter] で人が選べる
                    json!({"ok": false, "error": tr("このセッションは承認キーが分かりません")})
                        .to_string()
                }
            }
            remote::AgentAct::Stop => {
                // リモートからの手動操作もユーザーの応答扱い
                s.note_user_input();
                s.write_bytes(b"\x1b");
                json!({"ok": true}).to_string()
            }
        }
    }

    /// remote_reply: VoiceSend — 音声入力ページからの送信 (id 負数はブロードキャスト)。
    pub(super) fn remote_reply_voice_send(&mut self, text: &str, id: i64, submit: bool) -> String {
        use serde_json::json;
        let text = text.trim().to_string();
        if text.is_empty() {
            return json!({"ok": false, "error": "テキストが空です"}).to_string();
        }
        // submit=false は入力欄へ挿入するだけ (Enter は送らない)
        let payload = if submit {
            format!("{text}\r")
        } else {
            text.clone()
        };
        let verb = if submit {
            tr("送信")
        } else {
            tr("入力欄へ")
        };
        if id < 0 {
            // 全エージェントへブロードキャスト
            let n = self.agents.running_count();
            if n == 0 {
                return json!({"ok": false, "error": "実行中のセッションがありません"}).to_string();
            }
            for s in self.agents.sessions.iter_mut().filter(|s| s.running()) {
                // リモートからの手動送信もユーザーの応答扱い
                s.note_user_input();
                s.write_bytes(payload.as_bytes());
            }
            self.toast(
                trf(
                    "🎤📣 {n} セッション {verb}: {text}",
                    &[
                        ("n", n.to_string()),
                        ("verb", verb),
                        ("text", text.to_string()),
                    ],
                ),
                true,
            );
            json!({"ok": true, "sent": n}).to_string()
        } else {
            // セッション id 指定 (インデックスではなく id — 閉じてもずれない)
            match self.agents.sessions.iter_mut().find(|s| s.id == id as u64) {
                Some(s) if s.running() => {
                    s.note_user_input();
                    s.write_bytes(payload.as_bytes());
                    let title = s.title.clone();
                    self.toast(format!("🎤 {title} {verb}: {text}"), true);
                    json!({"ok": true, "sent": 1}).to_string()
                }
                Some(_) => json!({"ok": false, "error": "セッションが停止しています"}).to_string(),
                None => json!({
                    "ok": false,
                    "error": "セッションが見つかりません (閉じられた可能性)",
                })
                .to_string(),
            }
        }
    }

    // ══════════════════════════════════════════════════════════════════
    //  一括操作 (スマホの「全員 / 待機 / 選択中」)
    //
    //  宛先の**選び方**は `remote::bulk_targets` (純関数)、**届け方**は
    //  PC 側と同じ入口 (`queue_submit_all` / `queue_submit_stalled` /
    //  `queue_submit`) に合流させる。ここで配達を作り直さない —
    //  作り直すと承認・コスト上限・チェックポイントの見張りが素通りする。
    // ══════════════════════════════════════════════════════════════════

    /// 一括操作の宛先一覧を作る。
    ///
    /// 「止まっている」の判定は [`Self::stalled_session_ids`] (= supervisor) から
    /// 取る。画面の文字列から推測しない (設計原則 4)。
    pub(super) fn bulk_picks(&self) -> Vec<remote::AgentPick> {
        let stalled = self.stalled_session_ids();
        let active = self.agents.active;
        self.agents
            .sessions
            .iter()
            .enumerate()
            .map(|(i, s)| remote::AgentPick {
                id: s.id,
                running: s.running(),
                stalled: stalled.contains(&s.id),
                active: i == active,
            })
            .collect()
    }

    /// 指定した ID 群へ生バイトを書く。実際に届いた数を返す。
    ///
    /// 制御キー (Esc 等) と「入力欄へ入れるだけ」の 1 体宛て送信で使う。
    /// `/api/term` と同じ経路なので、1 体宛ての挙動はこれまでと変わらない。
    fn bulk_write_raw(&mut self, ids: &[u64], bytes: &[u8]) -> usize {
        let mut n = 0;
        for s in self
            .agents
            .sessions
            .iter_mut()
            .filter(|s| s.running() && ids.contains(&s.id))
        {
            // リモートからの手動操作もユーザーの応答扱い (承認エピソードを解決する)
            s.note_user_input();
            s.write_bytes(bytes);
            n += 1;
        }
        n
    }

    /// remote_reply: Bulk — 宛先モードに従って同じ本文を配る。
    pub(super) fn remote_reply_bulk(
        &mut self,
        text: &str,
        mode: remote::BulkMode,
        submit: bool,
    ) -> String {
        use serde_json::json;
        let text = text.trim().to_string();
        if text.is_empty() {
            return json!({"ok": false, "error": tr("テキストが空です")}).to_string();
        }
        let picks = self.bulk_picks();
        let targets = remote::bulk_targets(mode, &picks);
        if targets.is_empty() {
            return json!({"ok": false, "error": tr("送れる宛先がいません")}).to_string();
        }
        // コスト上限で止まっているなら、**理由をそのままスマホへ返す**。
        // 先に見ておくと `queue_submit*` 側の栓は通るので、PC に同じ理由の
        // トーストが二重に出ることも無い。
        if let Some(why) = self.cost_block_reason() {
            return json!({"ok": false, "error": why}).to_string();
        }
        let sent = match (mode, submit) {
            // 1 体宛ては従来どおり生書き (`/api/term` と同じバイト列)。
            // 素のシェルにも効く経路をここで変えない。
            (remote::BulkMode::One, _) => {
                let payload = if submit {
                    format!("{text}\r")
                } else {
                    text.clone()
                };
                self.bulk_write_raw(&targets, payload.as_bytes())
            }
            // 一斉送信は Cockpit のブロードキャストと同じ入口へ合流させる。
            // 確定キーの再送・コスト上限・チェックポイントが全部そのまま効く。
            (remote::BulkMode::All, true) => match self.queue_submit_all(&text) {
                // None = コスト上限。理由は送信側がトーストで説明済み
                None => {
                    return json!({"ok": false, "error": tr("送信できませんでした")}).to_string()
                }
                Some(n) => n,
            },
            (remote::BulkMode::Stalled, true) => match self.queue_submit_stalled(&text) {
                None => {
                    return json!({"ok": false, "error": tr("送信できませんでした")}).to_string()
                }
                Some(n) => n,
            },
            // 「入れるだけ」は Cockpit に対応する入口が無い (一斉送信は必ず確定する)
            // ので、同じ配達機構を submit=false のジョブで通す。
            // コスト上限は**宛先ごとに理由を出さない**よう、ここで一度だけ見る。
            (_, false) => {
                if let Some(why) = self.cost_block_reason() {
                    self.toast(why, false);
                    return json!({"ok": false, "error": tr("送信できませんでした")}).to_string();
                }
                let mut n = 0;
                for id in &targets {
                    let job = submit::Job {
                        submit: false,
                        ..submit::Job::user(*id, text.clone())
                    };
                    if self.queue_submit(job) {
                        n += 1;
                    }
                }
                n
            }
        };
        // 1 体宛ては従来どおりトーストを出さない (PC 側が二重に喋らない)。
        // 一斉送信だけは「何体へ流したか」を PC 側にも残す。
        if mode != remote::BulkMode::One {
            self.toast(
                trf(
                    "📣 {n} セッションへ送信しました",
                    &[("n", sent.to_string())],
                ),
                true,
            );
        }
        json!({"ok": true, "sent": sent, "mode": mode.as_str()}).to_string()
    }

    /// remote_reply: BulkStop — 宛先へ Esc を送っていまの作業を止める。
    ///
    /// セッションを殺す [`Cmd::StopAllAgents`] は PC 側に確認モーダルを開くので、
    /// スマホから撃つと**誰も押せないダイアログが PC に残る**。リモートから
    /// 届くのは「中断」までに留め、破壊的な停止は PC 側の確認を通す。
    pub(super) fn remote_reply_bulk_stop(&mut self, mode: remote::BulkMode) -> String {
        use serde_json::json;
        let picks = self.bulk_picks();
        let targets = remote::bulk_targets(mode, &picks);
        if targets.is_empty() {
            return json!({"ok": false, "error": tr("送れる宛先がいません")}).to_string();
        }
        // Esc 1 バイト。端末キーの [Esc] と同じものを人数分だけ送る
        let n = self.bulk_write_raw(&targets, b"\x1b");
        self.toast(
            trf(
                "⏹ {n} セッションへ停止を送りました",
                &[("n", n.to_string())],
            ),
            true,
        );
        json!({"ok": true, "sent": n, "mode": mode.as_str()}).to_string()
    }

    /// remote_reply: TermInput — アクティブなエージェントへ入力を送る。
    pub(super) fn remote_reply_term_input(&mut self, payload: &str, raw: bool) -> String {
        use serde_json::json;
        match self.agents.active_session() {
            Some(s) if s.running() => {
                // スマホの端末キー/入力欄 = 手入力。承認エピソードを解決する
                s.note_user_input();
                if raw {
                    s.write_bytes(payload.as_bytes());
                } else {
                    s.write_bytes(format!("{payload}\r").as_bytes());
                }
                json!({"ok": true}).to_string()
            }
            _ => json!({"ok": false, "error": "実行中のセッションがありません"}).to_string(),
        }
    }

    /// remote_reply: Cmd — 名前指定コマンドの実行。
    pub(super) fn remote_reply_cmd(&mut self, name: &str, arg: i64, ctx: &egui::Context) -> String {
        use serde_json::json;
        // 無題バッファへの save はブロッキングな rfd ダイアログを
        // PC 側に開いてしまうため、リモートからは拒否する
        if name == "save" {
            let no_path = self
                .editor
                .active
                .map(|i| self.editor.buffers[i].path.is_none())
                .unwrap_or(true);
            if no_path {
                return json!({
                    "ok": false,
                    "error": trf(
                        "名前のないファイルは PC 側で保存してください ({key})",
                        &[("key", self.key_hint(BindAction::Save))],
                    ),
                })
                .to_string();
            }
        }
        let cmd = match name {
            "save" => Some(Cmd::Save),
            "new" => Some(Cmd::NewFile),
            "close_tab" => Some(Cmd::CloseTab),
            "terminal" => Some(Cmd::ToggleTerminal),
            "sidebar" => Some(Cmd::ToggleSidebar),
            "git" => Some(Cmd::OpenGitPanel),
            "cockpit" => Some(Cmd::ToggleCockpit),
            "kanban" => Some(Cmd::ToggleKanban),
            "deck" => Some(Cmd::ToggleDeck),
            "new_task" => Some(Cmd::NewTask),
            "agent_message" => Some(Cmd::SendAgentMessage),
            "zoom_in" => Some(Cmd::ZoomIn),
            "zoom_out" => Some(Cmd::ZoomOut),
            "zoom_reset" => Some(Cmd::ZoomReset),
            "file_zoom_in" => Some(Cmd::FileZoomIn),
            "file_zoom_out" => Some(Cmd::FileZoomOut),
            "file_zoom_reset" => Some(Cmd::FileZoomReset),
            // v0.5.1 までの名前。既存プラグインの run_command を壊さない。
            "font_inc" => Some(Cmd::ZoomIn),
            "font_dec" => Some(Cmd::ZoomOut),
            "tree" => Some(Cmd::RefreshTree),
            "approval_auto" => Some(Cmd::SetApproval("auto".into())),
            "approval_ask" => Some(Cmd::SetApproval("ask".into())),
            "approval_agent" => Some(Cmd::SetApproval("agent".into())),
            "permission_cycle" => Some(Cmd::CyclePermissionAll),
            "agent_launch" => Some(Cmd::NewAgent(arg.max(0) as usize)),
            "agent_focus" => Some(Cmd::FocusAgent(arg.max(0) as usize)),
            "agent_restart" => Some(Cmd::RestartAgent),
            "agent_kill" => Some(Cmd::KillAgent),
            _ => None,
        };
        match cmd {
            Some(c) => {
                self.apply_cmd(c, ctx);
                json!({"ok": true}).to_string()
            }
            None => json!({"ok": false, "error": "unknown cmd"}).to_string(),
        }
    }

    /// スマホリモートの**待ち受け先だけ**を張り替える (LAN ⇄ SSH トンネル)。
    ///
    /// ハンドラ ([`remote::handle_conn`] 以下) は 1 面のまま、変わるのは
    /// トランスポート = bind 先だけ (設計原則 5)。トークンは引き継ぐので、
    /// 既に QR を読んだスマホや CLI の接続情報が無効にならない。
    /// 戻り値は張り直した後のポート (トンネルの転送先に使う)。
    pub(super) fn rebind_remote(
        &mut self,
        ctx: &egui::Context,
        bind: remote::Bind,
    ) -> Result<u16, String> {
        let Some(old) = self.remote.take() else {
            return Err(tr("スマホリモートが起動していません"));
        };
        if old.bind == bind {
            let port = old.port;
            self.remote = Some(old);
            return Ok(port);
        }
        let token = old.token.clone();
        let prefer = old.port;
        let was = old.bind;
        // 先に畳んでポートを解放する (Drop が accept の終了まで待つ)
        drop(old);
        match remote::RemoteServer::rebind(ctx.clone(), bind, token.clone(), prefer) {
            Ok(s) => {
                let port = s.port;
                // CLI (`zai open` など) が見る接続情報も更新する
                let ws = self.primary_root().to_string_lossy().to_string();
                if let Err(e) = cli::write_instance_file(s.port, &s.token, &ws) {
                    eprintln!("インスタンス情報の書き出しに失敗しました: {e}");
                }
                self.remote = Some(s);
                self.remote_err = None;
                self.qr_url.clear(); // URL が変わるので QR を作り直す
                Ok(port)
            }
            Err(e) => {
                // **元の待ち受けへ戻す。** 張り替えは失敗しうる (Tailscale が
                // 落ちた / ポートを横取りされた) のに、失敗したまま None にすると
                // **スマホリモートも `zai` CLI も 🎤 も、再起動するまで全部死ぬ**。
                // 切り替えに失敗しただけで、元の経路まで失う理由は無い。
                let back = remote::RemoteServer::rebind(ctx.clone(), was, token, prefer).ok();
                let restored = back.is_some();
                self.remote = back;
                self.qr_url.clear();
                let msg = if restored {
                    trf(
                        "待ち受けの切り替えに失敗しました: {e} (元の待ち受けに戻しました)",
                        &[("e", e.clone())],
                    )
                } else {
                    trf("待ち受けの切り替えに失敗しました: {e}", &[("e", e.clone())])
                };
                self.remote_err = if restored { None } else { Some(e) };
                Err(msg)
            }
        }
    }

    /// QR コード付きの接続ウィンドウ。📱 ボタンで開閉する。
    pub(super) fn remote_window(&mut self, ctx: &egui::Context) {
        if !self.remote_open {
            return;
        }
        // スマホに読ませる URL。SSH トンネルが繋がっていれば踏み台の URL、
        // そうでなければ LAN の URL。**どちらか 1 つしか出さない**
        // (2 つ並べると、どちらを開けばよいのか分からなくなる)。
        let tstate = self.tunnel.state();
        let url_full = self.remote.as_ref().and_then(|r| {
            tstate
                .phone_url(&r.token)
                .or_else(|| Some(format!("{}?t={}", r.url, r.token)))
        });

        // QR テクスチャは URL が変わったときだけ作り直す
        // (毎フレーム作ると 240×240 のテクスチャを延々とアップロードすることになる)
        let want_qr = url_full.clone().unwrap_or_default();
        if self.qr_url != want_qr {
            self.qr_tex = None;
            self.qr_url = want_qr.clone();
            if !want_qr.is_empty() {
                if let Ok(code) = qrcode::QrCode::new(want_qr.as_bytes()) {
                    let w = code.width();
                    let colors = code.to_colors();
                    let m = 2usize;
                    let size = w + m * 2;
                    let mut pixels = vec![Color32::WHITE; size * size];
                    for y in 0..w {
                        for x in 0..w {
                            if colors[y * w + x] == qrcode::Color::Dark {
                                pixels[(y + m) * size + (x + m)] = Color32::BLACK;
                            }
                        }
                    }
                    let img = egui::ColorImage {
                        size: [size, size],
                        pixels,
                    };
                    self.qr_tex =
                        Some(ctx.load_texture("zv-remote-qr", img, egui::TextureOptions::NEAREST));
                }
            }
        }

        let theme = self.theme.clone();
        let err = self.remote_err.clone();
        let qr_tex = self.qr_tex.clone();
        let mut open = self.remote_open;
        let mut copy = false;
        let mut open_voice = false;
        // 「スマホから届いているのか」— 規則をいくら読んでも分からない事実。
        // これが無いと、真っ白な画面を前に PC 側で何も判断できない。
        // (SSH トンネル経由の接続は loopback から来るので数に入らない。
        //  数えない代わりに、下のトンネル状態行が「どの段にいるか」を出す)
        let reach = self.remote.as_ref().map(|r| r.reach());
        // ── SSH トンネルの UI 用に取り出しておく値 ──
        let tstage = tstate.stage;
        let tattempt = tstate.attempt;
        let tlast = tstate.last_failure;
        let mut tunnel_host = self.tunnel_host.clone();
        let tunnel_err = self.tunnel_err.clone();
        let ssh_missing = tunnel::ssh_path().is_none();
        let mut tunnel_connect = false;
        let mut tunnel_disconnect = false;
        let mut tunnel_copy_l = false;

        // いまどこで待ち受けているか。SSH トンネル時は 127.0.0.1 だけなので、
        // LAN 前提の案内 (ファイアウォール / 同じ Wi-Fi / 到達確認) は全部消す —
        // 出したままだと「許可したのに繋がらない」と誤解させる。
        let bind = self
            .remote
            .as_ref()
            .map(|r| r.bind)
            .unwrap_or(remote::Bind::Lan);
        let lan_mode = bind == remote::Bind::Lan;
        let ts_mode = bind == remote::Bind::Tailscale;
        // Tailscale の検出。**この画面が描かれている間だけ**測る
        // (スレッドもタイマーも持たない — 設計原則 3)。
        let ts = self.ts.get();
        let mut ts_on = false;
        let mut ts_off = false;

        // Windows の受信許可。ここが無いと「QR は読めるのにスマホからだけ
        // 何も起きない」になるので、繋がる前提として真っ先に見せる
        self.fw.ensure_checked();
        // SSH トンネル (127.0.0.1) のときだけ隠す。**Tailscale では隠さない** —
        // 受信は tailnet のインタフェース越しに来るので、Windows の受信許可は
        // 同じように効く。ここを隠すと「QR は読めるのにスマホからだけ何も
        // 起きない」の原因が画面から消える。規則は「この実行ファイル +
        // TCP 8899-8919」でインタフェースを問わないので、そのまま直せる。
        let fw_check = firewall::applicable() && (lan_mode || ts_mode);
        let fw_busy = self.fw.busy();
        let fw_report = self.fw.report().cloned();
        let fw_error = self.fw.error.clone();
        let fw_manual = if fw_check {
            self.fw.manual()
        } else {
            String::new()
        };
        let mut fw_allow = false;
        let mut fw_revoke = false;
        let mut fw_recheck = false;
        let mut fw_copy_cmd = false;
        let mut fw_unblock = false;
        let mut fw_copy_exe = false;
        // 別のファイアウォール製品 (ノートン等) の名前と、そこへ登録する exe パス
        let fw_other = if fw_check {
            self.fw.other_firewall()
        } else {
            String::new()
        };
        let fw_exe = if fw_check {
            self.fw.exe()
        } else {
            String::new()
        };

        egui::Window::new(tr("📱 スマホリモート"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.set_width(340.0);
                // **ここに `ScrollArea` を置かない** (一度入れて撤回した)。
                // 中央に固定した窓の中では、`ScrollArea` が使える高さを
                // 「窓の中の残り」= 実質画面の半分と読むので、`max_height` に
                // いくら大きな値を渡しても効かず、**窓が半分に畳まれる**。
                // 実測 (2026-08-16 / アプリ窓 1920×1050 · ui_zoom 1.0):
                // 上限 954px を渡しても中身は 470px で頭打ちになり、
                // Tailscale と SSH の段がスクロールの下へ隠れた。
                match (&url_full, &err) {
                    (Some(url), _) => {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new(tr(match bind {
                                    remote::Bind::Lan => {
                                        "同じ Wi-Fi のスマホで QR を読み取るだけで接続"
                                    }
                                    remote::Bind::Tailscale => tailscale::HEADLINE,
                                    remote::Bind::Loopback => {
                                        "SSH トンネル経由 — 外出先のスマホで QR を読み取って接続"
                                    }
                                }))
                                .color(theme.text),
                            );
                            // ── Windows: 受信許可の状態と、その場で直せるボタン ──
                            if fw_check {
                                ui.add_space(6.0);
                                match (&fw_report, fw_busy) {
                                    // 確認中 / 適用中
                                    (_, Some(b)) => {
                                        ui.label(
                                            RichText::new(tr(match b {
                                                firewall::Busy::Check => {
                                                    "🛡 Windows の受信許可を確認中…"
                                                }
                                                firewall::Busy::Allow => {
                                                    "🛡 受信を許可しています (管理者の確認に応答してください)…"
                                                }
                                                firewall::Busy::Revoke => {
                                                    "🛡 受信許可を取り消しています…"
                                                }
                                                firewall::Busy::Unblock => {
                                                    "🛡 受信の全ブロックを解除しています (管理者の確認に応答してください)…"
                                                }
                                            }))
                                            .size(11.5)
                                            .color(theme.text_dim),
                                        );
                                    }
                                    // 繋がらない原因がある = スマホからは絶対に繋がらない。
                                    // 「規則が無い」だけでなく「規則はあるのに効いていない」
                                    // (プロファイル不一致 / 受信全ブロック) もここに出す —
                                    // 出さないと「許可済みなのに繋がらない」で手が止まる。
                                    (Some(r), None) if !r.problems().is_empty() => {
                                        let problems = r.problems();
                                        egui::Frame::none()
                                            .fill(theme.panel_alt)
                                            .stroke(egui::Stroke::new(1.0_f32, theme.warn))
                                            .rounding(egui::Rounding::same(6.0))
                                            .inner_margin(egui::Margin::same(8.0))
                                            .show(ui, |ui| {
                                                ui.vertical(|ui| {
                                                    for (i, p) in problems.iter().enumerate() {
                                                        if i > 0 {
                                                            ui.add_space(4.0);
                                                        }
                                                        ui.label(
                                                            RichText::new(tr(p.headline()))
                                                                .size(12.0)
                                                                .strong()
                                                                .color(theme.warn),
                                                        );
                                                        // 別製品は名指しで出す。「別のファイアウォール」
                                                        // とだけ言われても、どこを開けば良いのか分からない。
                                                        if *p == firewall::Problem::OtherFirewall
                                                            && !fw_other.is_empty()
                                                        {
                                                            ui.label(
                                                                RichText::new(&fw_other)
                                                                    .size(11.5)
                                                                    .strong()
                                                                    .color(theme.warn),
                                                            );
                                                        }
                                                        ui.label(
                                                            RichText::new(tr(p.detail()))
                                                                .size(11.0)
                                                                .color(theme.text_dim),
                                                        );
                                                    }
                                                    // 種別と規則のプロファイルを並べて出す。
                                                    // 不一致はこの 2 つを見ないと納得できない。
                                                    let net = r.network_label();
                                                    if !net.is_empty() {
                                                        ui.label(
                                                            RichText::new(trf(
                                                                "※ いまのネットワーク: {net}{rule}",
                                                                &[
                                                                    ("net", net),
                                                                    (
                                                                        "rule",
                                                                        if r.profiles.is_empty() {
                                                                            String::new()
                                                                        } else {
                                                                            trf(
                                                                                " / 規則のプロファイル: {profiles}",
                                                                                &[("profiles", r.profiles.clone())],
                                                                            )
                                                                        },
                                                                    ),
                                                                ],
                                                            ))
                                                            .size(10.5)
                                                            .color(theme.text_dim),
                                                        );
                                                    }
                                                    if r.on_public_network() {
                                                        ui.label(
                                                            RichText::new(tr(
                                                                "※ このネットワークは「パブリック」に分類されています。\n\u{3000}\
                                                                 許可には Public プロファイルも含めます — 公共 Wi-Fi では 📱 を使わないでください。",
                                                            ))
                                                            .size(10.5)
                                                            .color(theme.text_dim),
                                                        );
                                                    }
                                                    ui.horizontal_wrapped(|ui| {
                                                        // 受信全ブロックが立っている間は規則を作っても
                                                        // 無視されるので、解除を先に並べる。
                                                        if problems.contains(&firewall::Problem::StrictInbound)
                                                            && ui
                                                                .button(tr("🛡 受信の全ブロックを解除 (管理者)"))
                                                                .on_hover_text(tr(
                                                                    "使用中のネットワークの「すべての受信接続をブロックする」を外します\n\
                                                                     (他のプロファイルの設定は変えません)。管理者の確認 (UAC) が出ます。",
                                                                ))
                                                                .clicked()
                                                        {
                                                            fw_unblock = true;
                                                        }
                                                        if r.fixable_by_allow()
                                                            && ui
                                                                .button(tr("🛡 受信を許可する (管理者)"))
                                                                .on_hover_text(trf(
                                                                    "この実行ファイルの TCP {from}-{to} だけを、\n\
                                                                     いま繋いでいるネットワークに合わせて許可します。\n\
                                                                     管理者の確認 (UAC) が 1 回出ます。",
                                                                    &[
                                                                        ("from", firewall::PORT_FROM.to_string()),
                                                                        ("to", firewall::PORT_TO.to_string()),
                                                                    ],
                                                                ))
                                                                .clicked()
                                                        {
                                                            fw_allow = true;
                                                        }
                                                        // 別製品への登録は exe パスを求められる。
                                                        // 手で打たせない (打ち間違えると許可されない)。
                                                        if problems
                                                            .contains(&firewall::Problem::OtherFirewall)
                                                            && ui
                                                                .button(tr("📋 exe のパスをコピー"))
                                                                .on_hover_text(&fw_exe)
                                                                .clicked()
                                                        {
                                                            fw_copy_exe = true;
                                                        }
                                                        if ui.button(tr("⟳ 再確認")).clicked() {
                                                            fw_recheck = true;
                                                        }
                                                        if ui
                                                            .button(tr("📋 コマンドをコピー"))
                                                            .on_hover_text(fw_manual.clone())
                                                            .clicked()
                                                        {
                                                            fw_copy_cmd = true;
                                                        }
                                                    });
                                                });
                                            });
                                    }
                                    // 繋がる状態 (許可済み、またはファイアウォールが無効)
                                    (Some(r), None) => {
                                        ui.horizontal(|ui| {
                                            ui.label(
                                                RichText::new(if r.allowed {
                                                    trf(
                                                        "✅ ファイアウォール許可済み ({profiles})",
                                                        &[(
                                                            "profiles",
                                                            if r.profiles.is_empty() {
                                                                "-".to_string()
                                                            } else {
                                                                r.profiles.clone()
                                                            },
                                                        )],
                                                    )
                                                } else {
                                                    // 規則は無いが Windows も受信を検査していない。
                                                    // 「許可済み」と書くと嘘になるので分ける。
                                                    tr("✅ 受信はブロックされていません (ファイアウォールが無効)")
                                                })
                                                .size(11.0)
                                                .color(theme.ok),
                                            );
                                            if r.allowed
                                                && ui
                                                    .small_button(tr("取り消す"))
                                                    .on_hover_text(tr(
                                                        "受信許可の規則を削除します (スマホからは繋がらなくなります)",
                                                    ))
                                                    .clicked()
                                            {
                                                fw_revoke = true;
                                            }
                                            if ui
                                                .small_button(tr("⟳"))
                                                .on_hover_text(tr(
                                                    "受信許可を再確認します (別の Wi-Fi へ移った後はここで確認)",
                                                ))
                                                .clicked()
                                            {
                                                fw_recheck = true;
                                            }
                                        });
                                    }
                                    // まだ結果が無い (起動直後)
                                    (None, None) => {}
                                }
                                if let Some(e) = &fw_error {
                                    ui.label(RichText::new(e).size(10.5).color(theme.err));
                                }
                            }
                            // ── スマホからの接続が実際に届いたか ──
                            // 規則を読んで分かるのは建前だけ。ここが 0 件のままなら
                            // パケットが PC まで来ていない (ファイアウォール / 別セグメント /
                            // ルータのクライアント分離)、1 件でもあれば届いてはいる、と
                            // 切り分けられる。「真っ白で、繋がっているのかも分からない」を
                            // PC 側から潰すための表示。
                            if let Some(re) = reach.as_ref().filter(|_| lan_mode || ts_mode) {
                                ui.add_space(6.0);
                                if re.hits == 0 {
                                    // 届いていない理由は経路ごとに違う。LAN の
                                    // 文面を Tailscale で出すと、直しようのない
                                    // ところ (ファイアウォール) を疑わせてしまう。
                                    ui.label(
                                        RichText::new(tr(if ts_mode {
                                            tailscale::NO_REACH
                                        } else {
                                            "📶 まだスマホからの接続はありません\n\u{3000}\
                                             スマホが真っ白なままなら、通信が PC まで届いていません\n\u{3000}\
                                             (ファイアウォール / スマホが同じ Wi-Fi でない / \
                                             ルータのプライバシーセパレータ)"
                                        }))
                                        .size(11.0)
                                        .color(theme.text_dim),
                                    );
                                } else {
                                    let ago = re
                                        .last_at
                                        .map(|t| t.elapsed().as_secs())
                                        .unwrap_or(0);
                                    ui.label(
                                        RichText::new(trf(
                                            "📶 スマホから接続あり: {ip} ({ago} 秒前 / 計 {n} 回)",
                                            &[
                                                ("ip", re.last_ip.clone().unwrap_or_default()),
                                                ("ago", ago.to_string()),
                                                ("n", re.hits.to_string()),
                                            ],
                                        ))
                                        .size(11.0)
                                        .color(theme.ok),
                                    );
                                }
                            }
                            ui.add_space(8.0);
                            if let Some(tex) = &qr_tex {
                                ui.add(
                                    egui::Image::new(tex)
                                        .fit_to_exact_size(egui::vec2(240.0, 240.0)),
                                );
                            }
                            ui.add_space(8.0);
                            let mut u = url.clone();
                            ui.add(
                                egui::TextEdit::singleline(&mut u)
                                    .desired_width(320.0)
                                    .font(FontId::monospace(12.0)),
                            );
                            if ui.button(tr("📋 URL をコピー")).clicked() {
                                copy = true;
                            }
                            ui.add_space(6.0);
                            ui.label(
                                RichText::new(tr(
                                    "スマホから: ファイルの編集・保存・オープン、\n\
                                     エージェント操作 (Claude の承認・指示も OK)、各種コマンド\n\
                                     🎤 音声入力: スマホは「エージェント」タブのマイクボタン",
                                ))
                                .size(11.5)
                                .color(theme.text_dim),
                            );
                            ui.add_space(6.0);
                            ui.separator();

                            // ── Tailscale VPN ───────────────────────────
                            // 踏み台も同じ Wi-Fi も要らない 3 本目の経路。
                            // **入れていない人には 1 行も出さない** — 押せない
                            // ボタンと直せない警告を並べても場所を食うだけ。
                            // (繋がっていれば必ず検出できるので、使える人には出る)
                            if ts.stage != tailscale::Stage::Missing || ts_mode {
                                ui.horizontal(|ui| {
                                    ui.label(
                                        RichText::new(tr("🔒 Tailscale VPN"))
                                            .strong()
                                            .color(theme.text),
                                    );
                                    ui.label(
                                        RichText::new(tr("(同じ Wi-Fi でなくても繋ぐ)"))
                                            .size(10.5)
                                            .color(theme.text_dim),
                                    );
                                });
                                ui.add_space(3.0);
                                let ts_col = match ts.stage {
                                    tailscale::Stage::Up => theme.ok,
                                    tailscale::Stage::Down => theme.warn,
                                    tailscale::Stage::Missing => theme.err,
                                };
                                // 段は必ず出す (「繋がりません」だけでは、
                                // 入っていないのか止まっているのか分からない)
                                let ts_txt = match ts.ip {
                                    Some(ip) => trf(
                                        "● {stage} ({ip})",
                                        &[
                                            ("stage", tr(ts.stage.label())),
                                            ("ip", ip.to_string()),
                                        ],
                                    ),
                                    None => trf("● {stage}", &[("stage", tr(ts.stage.label()))]),
                                };
                                ui.horizontal_wrapped(|ui| {
                                    ui.label(RichText::new(ts_txt).size(11.5).color(ts_col));
                                });
                                ui.label(
                                    RichText::new(tr(ts.stage.hint()))
                                        .size(10.5)
                                        .color(theme.text_dim),
                                );
                                ui.add_space(3.0);
                                if ts_mode {
                                    if ui
                                        .button(tr("📶 同じ Wi-Fi に戻す"))
                                        .on_hover_text(tr(tailscale::BACK_HINT))
                                        .clicked()
                                    {
                                        ts_off = true;
                                    }
                                    ui.label(
                                        RichText::new(tr(tailscale::ONLY_TAILNET_NOTE))
                                        .size(10.5)
                                        .color(theme.text_dim),
                                    );
                                } else if ui
                                    .add_enabled(
                                        ts.ready(),
                                        egui::Button::new(tr("🔒 Tailscale で待ち受ける")),
                                    )
                                    .on_hover_text(tr(tailscale::SWITCH_HINT))
                                    .clicked()
                                {
                                    ts_on = true;
                                }
                                ui.add_space(6.0);
                                ui.separator();
                            }

                            // ── SSH リモート接続 ────────────────────────
                            // 同じ Wi-Fi にいないスマホは、ユーザーが既に SSH で
                            // 入れるホストを中継すれば届く。スマホ側に SSH
                            // クライアントは要らない (ブラウザだけ)。
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(tr("🔐 SSH リモート接続"))
                                        .strong()
                                        .color(theme.text),
                                );
                                ui.label(
                                    RichText::new(tr("(同じ Wi-Fi でなくても繋ぐ)"))
                                        .size(10.5)
                                        .color(theme.text_dim),
                                );
                            });
                            ui.add_space(3.0);
                            let busy = matches!(
                                tstage,
                                tunnel::Stage::Connecting | tunnel::Stage::Connected
                            );
                            ui.horizontal(|ui| {
                                // ボタン幅を先に確保して、残りを入力欄に渡す
                                // (どの幅でも見切れないこと)
                                let btn_w = 72.0_f32;
                                let field_w =
                                    (ui.available_width() - btn_w - 8.0).max(80.0);
                                ui.add_enabled(
                                    !busy,
                                    egui::TextEdit::singleline(&mut tunnel_host)
                                        .desired_width(field_w)
                                        .hint_text("user@example.com")
                                        .font(FontId::monospace(12.0)),
                                )
                                .on_hover_text(tr(tunnel::HOST_HINT));
                                if ui
                                    .add_sized(
                                        [btn_w, 22.0],
                                        egui::Button::new(if busy {
                                            tr("切断")
                                        } else {
                                            tr("接続")
                                        }),
                                    )
                                    .clicked()
                                {
                                    if busy {
                                        tunnel_disconnect = true;
                                    } else {
                                        tunnel_connect = true;
                                    }
                                }
                            });

                            // ── 状態 1 行: いまどの段か + どこで待ち受けているか ──
                            let stage_txt = match tstage {
                                tunnel::Stage::Connecting if tattempt > 0 => trf(
                                    "接続中 (再試行 {n}/{max})",
                                    &[
                                        ("n", tattempt.to_string()),
                                        ("max", tunnel::MAX_RETRIES.to_string()),
                                    ],
                                ),
                                s => tr(s.label()),
                            };
                            let stage_col = match tstage {
                                tunnel::Stage::Connected => theme.ok,
                                tunnel::Stage::Failed(_) => theme.err,
                                tunnel::Stage::Connecting => theme.warn,
                                tunnel::Stage::Disconnected => theme.text_dim,
                            };
                            ui.add_space(3.0);
                            ui.horizontal_wrapped(|ui| {
                                ui.label(
                                    RichText::new(trf(
                                        "● {stage}",
                                        &[("stage", stage_txt)],
                                    ))
                                    .size(11.5)
                                    .color(stage_col),
                                );
                                ui.label(
                                    RichText::new(trf(
                                        "· 待ち受け: {bind}",
                                        &[("bind", tr(bind.label()))],
                                    ))
                                    .size(11.0)
                                    .color(theme.text_dim),
                                );
                            });
                            // 失敗の理由と、次にすることを 1 行ずつ
                            // (生の ssh の stderr は出さない)
                            let show_fail = match tstage {
                                tunnel::Stage::Failed(f) => Some(f),
                                tunnel::Stage::Connecting if tattempt > 0 => tlast,
                                _ => None,
                            };
                            if let Some(f) = show_fail {
                                ui.label(
                                    RichText::new(tr(f.headline()))
                                        .size(11.0)
                                        .color(theme.err),
                                );
                                ui.label(
                                    RichText::new(tr(f.hint()))
                                        .size(10.5)
                                        .color(theme.text_dim),
                                );
                            }
                            if let Some(e) = &tunnel_err {
                                ui.label(RichText::new(e).size(11.0).color(theme.err));
                            }
                            if ssh_missing && show_fail.is_none() {
                                ui.label(
                                    RichText::new(tr(
                                        "OpenSSH クライアントが見つかりません",
                                    ))
                                    .size(11.0)
                                    .color(theme.warn),
                                );
                                ui.label(
                                    RichText::new(tr(tunnel::install_hint()))
                                        .size(10.5)
                                        .color(theme.text_dim),
                                );
                            }
                            ui.add_space(3.0);
                            if ui
                                .button(tr("📋 ssh -L のコマンドをコピー"))
                                .on_hover_text(tr(tunnel::SSH_L_HINT))
                                .clicked()
                            {
                                tunnel_copy_l = true;
                            }
                            ui.label(
                                RichText::new(tr(tunnel::GATEWAY_NOTE))
                                    .size(10.5)
                                    .color(theme.text_dim),
                            );

                            ui.add_space(6.0);
                            ui.separator();
                            if ui
                                .button(tr("🎤 PC で音声入力する"))
                                .on_hover_text(tr(
                                    "Zaivern 内で音声認識し、話した内容を\n\
                                     エージェントの入力欄へ入れます (送信は自分で Enter)",
                                ))
                                .clicked()
                            {
                                open_voice = true;
                            }
                        });
                    }
                    (None, Some(e)) => {
                        ui.colored_label(
                            theme.err,
                            trf("リモートサーバ起動失敗: {e}", &[("e", e.to_string())]),
                        );
                    }
                    _ => {}
                }
            });

        self.remote_open = open;
        self.tunnel_host = tunnel_host;
        // ── Tailscale の待ち受け切り替え ──
        if ts_on || ts_off {
            // 経路は 1 本に決める。SSH トンネルを張ったまま Tailscale へ
            // 切り替えると、QR に出るのは踏み台の URL のまま (url_full が
            // トンネルを優先する) で、**押しても何も変わらないように見える**。
            if ts_on {
                self.tunnel.disconnect();
            }
            let want = if ts_on {
                remote::Bind::Tailscale
            } else {
                remote::Bind::Lan
            };
            match self.rebind_remote(ctx, want) {
                Ok(_) => {
                    self.tunnel_err = None;
                    self.toast(
                        tr(if ts_on {
                            "Tailscale の IP で待ち受けています"
                        } else {
                            "同じ Wi-Fi から繋げるように戻しました"
                        }),
                        true,
                    );
                }
                Err(e) => self.tunnel_err = Some(e),
            }
            // 切り替えた直後の状態はもう古い (tailnet が落ちていたかもしれない)
            self.ts.invalidate();
        }
        // ── SSH トンネルの操作 ──
        if tunnel_connect {
            match tunnel::parse_target(&self.tunnel_host) {
                Ok(t) => {
                    self.tunnel_err = None;
                    // 入力を正規化して欄へ戻す (前後の空白などを残さない)
                    self.tunnel_host = t.display();
                    // 接続先だけ覚える (鍵・パスフレーズは保存しない)
                    self.cfg.ssh_tunnel_host = self.tunnel_host.clone();
                    config::save_state(&self.cfg);
                    // **先に** 待ち受けを 127.0.0.1 へ絞る。0.0.0.0 のままだと
                    // SSH を迂回して平文で直接叩けてしまう。
                    match self.rebind_remote(ctx, remote::Bind::Loopback) {
                        Ok(port) => self.tunnel.connect(t, port),
                        Err(e) => self.tunnel_err = Some(e),
                    }
                }
                Err(e) => self.tunnel_err = Some(tr(e.msg())),
            }
        }
        if tunnel_disconnect {
            self.tunnel.disconnect();
            self.tunnel_err = self.rebind_remote(ctx, remote::Bind::Lan).err();
        }
        if tunnel_copy_l {
            match tunnel::parse_target(&self.tunnel_host) {
                Ok(t) => {
                    let port = self
                        .remote
                        .as_ref()
                        .map(|r| r.port)
                        .unwrap_or(remote::PORT_FROM);
                    ctx.copy_text(tunnel::ssh_l_command(&t, port, port));
                    self.toast(tr("ssh -L のコマンドをコピーしました"), true);
                }
                Err(e) => self.tunnel_err = Some(tr(e.msg())),
            }
        }
        if open_voice {
            self.apply_cmd(Cmd::VoiceInput(voice::Target::Broadcast), ctx);
        }
        if copy {
            if let Some(u) = url_full {
                ctx.copy_text(u);
            }
            self.toast(tr("URL をコピーしました"), true);
        }
        // ファイアウォール操作 (結果は poll で拾ってトーストにする)
        if fw_allow {
            self.fw.allow();
        }
        if fw_unblock {
            self.fw.unblock();
        }
        if fw_revoke {
            self.fw.revoke();
        }
        if fw_recheck {
            self.fw.recheck();
        }
        if fw_copy_cmd {
            ctx.copy_text(fw_manual);
            self.toast(
                tr("コマンドをコピーしました (管理者の PowerShell で実行してください)"),
                true,
            );
        }
        if fw_copy_exe {
            ctx.copy_text(fw_exe);
            self.toast(
                tr("exe のパスをコピーしました (お使いのファイアウォール製品で受信を許可してください)"),
                true,
            );
        }
    }

    // ══════════════════════════════════════════════════════════════════
    //  変更一覧 / 差分 — PC の `open_changes_multibuffer` と同じ入口
    //
    //  **ここでは git を 1 度も起こさない。** `git status` は単独 0.03 秒でも、
    //  このアプリが同じリポジトリへ同時に撃つと 2.3〜10.2 秒かかる
    //  (CLAUDE.md の実測)。UI スレッドで待つとフレームがそのまま止まるので、
    //  取りに行くのは [`refresh_changes`] が起こす裏のスレッドだけにして、
    //  ここは控えを読む。控えが無い間は `pending` と即答し、
    //  待つのは接続ごとのスレッド (`remote::Query::retries_while_pending`)。
    // ══════════════════════════════════════════════════════════════════

    /// remote_reply: Changes — 未コミットの変更をファイル単位で。
    pub(super) fn remote_reply_changes(&mut self) -> String {
        use serde_json::json;
        let Some(top) = self.git_ops_repo() else {
            return json!({"ok": false, "error": tr("git リポジトリではありません")}).to_string();
        };
        match changes_snapshot(&top) {
            None => json!({
                "ok": false, "pending": true,
                "error": tr("git の読み取り中です"),
            })
            .to_string(),
            Some(Err(e)) => json!({"ok": false, "error": e}).to_string(),
            Some(Ok(snap)) => {
                let files: Vec<_> = snap
                    .files
                    .iter()
                    .map(|f| {
                        json!({
                            "rel": f.rel, "status": f.status,
                            "added": f.added, "removed": f.removed,
                            "binary": f.binary,
                        })
                    })
                    .collect();
                json!({
                    "ok": true, "root": top.display().to_string(), "files": files,
                    "added": snap.added, "removed": snap.removed,
                    "truncated": snap.truncated,
                })
                .to_string()
            }
        }
    }

    /// remote_reply: Diff — 1 ファイルぶんのハンク。
    pub(super) fn remote_reply_diff(&mut self, rel: &str) -> String {
        use serde_json::json;
        let Some(top) = self.git_ops_repo() else {
            return json!({"ok": false, "error": tr("git リポジトリではありません")}).to_string();
        };
        let snap = match changes_snapshot(&top) {
            None => {
                return json!({
                    "ok": false, "pending": true,
                    "error": tr("git の読み取り中です"),
                })
                .to_string()
            }
            Some(Err(e)) => return json!({"ok": false, "error": e}).to_string(),
            Some(Ok(s)) => s,
        };
        let Some(f) = snap.files.iter().find(|f| f.rel == rel) else {
            // 変更が無いファイルを尋ねられただけ。エラーにはしない
            return json!({
                "ok": true, "rel": rel, "binary": false,
                "truncated": false, "hunks": [],
            })
            .to_string();
        };
        let shown = f.hunks.len().min(remote::DIFF_HUNK_CAP);
        let hunks: Vec<_> = f.hunks[..shown]
            .iter()
            .map(|h| {
                let lines: Vec<_> = h
                    .lines
                    .iter()
                    .map(|l| {
                        json!({
                            "k": match l.kind {
                                crate::diff::LineKind::Added => "add",
                                crate::diff::LineKind::Removed => "del",
                                crate::diff::LineKind::Context => "ctx",
                            },
                            "o": l.old_no, "n": l.new_no, "t": l.text,
                        })
                    })
                    .collect();
                // `Hunk` は行数を持たないので、行から数える
                // (ヘッダの数字を信じるより、実際に送る行と必ず一致する)
                let old_lines = h
                    .lines
                    .iter()
                    .filter(|l| l.kind != crate::diff::LineKind::Added)
                    .count();
                let new_lines = h
                    .lines
                    .iter()
                    .filter(|l| l.kind != crate::diff::LineKind::Removed)
                    .count();
                json!({
                    "header": h.header,
                    "old_start": h.old_start, "old_lines": old_lines,
                    "new_start": h.new_start, "new_lines": new_lines,
                    "lines": lines,
                })
            })
            .collect();
        json!({
            "ok": true, "rel": f.rel, "status": f.status, "binary": f.binary,
            "truncated": f.truncated || shown < f.hunks.len(),
            "hunks": hunks,
        })
        .to_string()
    }

    // ══════════════════════════════════════════════════════════════════
    //  端末の履歴 — 色は ANSI ではなく**構造**で渡す
    // ══════════════════════════════════════════════════════════════════

    /// remote_reply: Scrollback — 端末の履歴を色つきで返す。
    pub(super) fn remote_reply_scrollback(
        &mut self,
        agent: i64,
        lines: usize,
        before: Option<usize>,
    ) -> String {
        use serde_json::json;
        let idx = if agent < 0 {
            self.agents.active
        } else {
            agent as usize
        };
        let Some(s) = self.agents.sessions.get(idx) else {
            return json!({"ok": false, "error": tr("セッションがありません")}).to_string();
        };
        let pal = TermPalette::of(&self.theme);
        let (title, running) = (s.title.clone(), s.running());
        let mut p = crate::lockx::lock_ok(&s.parser);
        let (total, from, rows) = scrollback_rows(&mut p, lines, before, &pal);
        drop(p);
        let rows: Vec<_> = rows
            .iter()
            .map(|spans| json!({"spans": remote::spans_json(spans)}))
            .collect();
        json!({
            "ok": true, "agent": idx, "title": title, "running": running,
            "total": total, "from": from, "rows": rows,
            // 要求より少ないのは「そこまでしか履歴が無い」— 黙って切ったのではない
            "truncated": from > 0,
        })
        .to_string()
    }

    // ══════════════════════════════════════════════════════════════════
    //  承認キュー — 決着は PC の承認パネルと**同じ入口** (resolve_approval)
    // ══════════════════════════════════════════════════════════════════

    /// remote_reply: Approvals — 承認待ちの中身 (種別・根拠行・待ち時間)。
    pub(super) fn remote_reply_approvals(&self) -> String {
        use serde_json::json;
        let now = git::unix_now().max(0) as u64;
        let items: Vec<_> = self
            .agents
            .approvals
            .pending()
            .map(|r| {
                let found = self
                    .agents
                    .sessions
                    .iter()
                    .enumerate()
                    .find(|(_, s)| s.id == r.agent_session_id);
                let (agent, agent_index) = match found {
                    Some((i, s)) => (s.title.clone(), i as i64),
                    None => (r.agent_bin.clone(), -1),
                };
                json!({
                    // 文字列で渡す (JS の Number は 2^53 までしか正確でない)
                    "id": r.id.to_string(),
                    "agent": agent, "agent_index": agent_index,
                    // 種別・表示名・アイコンは **approvals.rs の型そのまま**。
                    // ここで語を作り直すと真実の在り処が 2 つになる
                    "kind": r.kind.as_str(),
                    "label": tr(r.kind.label()),
                    "icon": r.kind.icon(),
                    // 自動承認を許す種別か (権限昇格だけは常に false)
                    "auto_ok": r.kind.auto_approvable(),
                    "detail": r.detail, "summary": r.summary,
                    "since": now.saturating_sub(r.created_at),
                    // 「常に許可」にできない要求 (権限昇格など) を先に見せる
                    "never_auto": r.never_auto,
                })
            })
            .collect();
        json!({
            "ok": true, "mode": self.cfg.approval_mode, "items": items,
        })
        .to_string()
    }

    /// remote_reply: Approve — 承認キューの 1 件を決着させる。
    pub(super) fn remote_reply_approve(&mut self, id: u64, act: remote::ApproveAct) -> String {
        use serde_json::json;
        let Some(req) = self.agents.approvals.get(id) else {
            return json!({
                "ok": false,
                "error": tr("その承認はもうありません (PC 側で決着した可能性)"),
            })
            .to_string();
        };
        let summary = req.summary.clone();
        let cmd = match act {
            remote::ApproveAct::Approve => agents::approvals::Command::Approve,
            remote::ApproveAct::Deny => agents::approvals::Command::Deny,
            remote::ApproveAct::Always => agents::approvals::Command::ApproveKindForAgentAlways,
            remote::ApproveAct::AlwaysDeny => agents::approvals::Command::DenyKindForAgentAlways,
        };
        // PC の承認パネルと**同じ入口**。応答の送信・ポリシーの永続化・監査ログを
        // ここで作り直さない (作り直すと見張りが素通りする経路がもう 1 本増える)。
        self.resolve_approval(id, cmd);
        json!({
            "ok": true,
            "msg": trf("🛡 {s}", &[("s", summary)]),
            "pending": false,
        })
        .to_string()
    }

    // ══════════════════════════════════════════════════════════════════
    //  ファイルを開かずに読む / 横断検索
    // ══════════════════════════════════════════════════════════════════

    /// remote_reply: Read — **PC のタブを切り替えずに**ファイルを読む。
    pub(super) fn remote_reply_read(&self, rel: &str, from: usize, lines: usize) -> String {
        use serde_json::json;
        let Some(path) = self.resolve_remote_rel(rel) else {
            return json!({"ok": false, "error": tr("ワークスペース外は読めません")}).to_string();
        };
        match std::fs::metadata(&path) {
            Ok(m) if !m.is_file() => {
                return json!({"ok": false, "error": tr("ファイルではありません")}).to_string()
            }
            Ok(m) if m.len() > file_search::MAX_FILE_BYTES => {
                return json!({
                    "ok": false,
                    "error": trf(
                        "大きすぎて読めません ({n} バイト)",
                        &[("n", m.len().to_string())],
                    ),
                })
                .to_string()
            }
            Ok(_) => {}
            Err(e) => return json!({"ok": false, "error": e.to_string()}).to_string(),
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => return json!({"ok": false, "error": e.to_string()}).to_string(),
        };
        if bytes.contains(&0) {
            return json!({"ok": false, "error": tr("バイナリファイルです")}).to_string();
        }
        let (text, enc) = crate::textenc::decode_bytes(&bytes);
        let all: Vec<&str> = text.lines().collect();
        let start = from.saturating_sub(1).min(all.len());
        let end = (start + lines).min(all.len());
        json!({
            "ok": true, "rel": rel, "from": start + 1, "total": all.len(),
            "lines": &all[start..end],
            "lang": self.highlighter.lang_for(Some(&path), &text),
            "encoding": enc.label(),
            // 続きがあることを黙らない
            "truncated": end < all.len(),
        })
        .to_string()
    }

    /// remote_reply: Search — ワークスペース横断検索。
    ///
    /// **索引 4000 件を UI スレッドで舐めない。** 既存の非同期入口
    /// [`file_search::spawn_with_options`] へ合流させ、結果が届くまでは
    /// `pending` と即答する。
    pub(super) fn remote_reply_search(&self, q: &str, max: usize) -> String {
        use serde_json::json;
        let hits = match self.search_snapshot(q, max) {
            Err(e) => return json!({"ok": false, "error": e}).to_string(),
            Ok(None) => {
                return json!({"ok": false, "pending": true, "error": tr("検索中です")}).to_string()
            }
            Ok(Some(h)) => h,
        };
        // 絶対パス → 表示ラベル。索引を 1 度だけ引き当てる
        let by_abs: std::collections::HashMap<&std::path::Path, &str> = self
            .file_index
            .iter()
            .map(|f| (f.abs.as_path(), f.label.as_str()))
            .collect();
        let out: Vec<_> = hits
            .iter()
            .map(|h| {
                let rel = by_abs
                    .get(h.path.as_path())
                    .map(|s| (*s).to_string())
                    .or_else(|| {
                        self.roots
                            .iter()
                            .find_map(|r| crate::ignore::rel_slash(r, &h.path))
                    })
                    .unwrap_or_else(|| h.path.display().to_string());
                // `Hit.line` は 0 起点。画面に出すのは 1 起点
                json!({"rel": rel, "line": h.line + 1, "text": h.text})
            })
            .collect();
        json!({
            "ok": true, "q": q, "hits": out,
            "truncated": hits.len() >= max,
        })
        .to_string()
    }

    /// ルート相対の綴りを、**必ずいずれかのルート配下にある**実パスへ解く。
    ///
    /// [`remote::safe_rel`] が字句を畳んだ後でも、シンボリックリンクを踏めば
    /// 外へ出られる。`canonicalize` した実体で前方一致を取り直すのはそのため
    /// (`remote_reply_open_file` と同じ守り方)。
    fn resolve_remote_rel(&self, rel: &str) -> Option<PathBuf> {
        let rel = remote::safe_rel(rel)?;
        let cand = self
            .file_index
            .iter()
            .find(|f| f.label == rel || f.rel == rel)
            .map(|f| f.abs.clone())
            .or_else(|| {
                self.roots
                    .iter()
                    .map(|r| r.join(&rel))
                    .find(|c| c.is_file())
            })?;
        let canon = cand.canonicalize().ok()?;
        self.roots
            .iter()
            .any(|r| {
                let root = r.canonicalize().unwrap_or_else(|_| r.clone());
                canon.starts_with(&root)
            })
            .then_some(canon)
    }

    /// 検索結果の控えを返す。`Ok(None)` は「まだ走っている」。
    ///
    /// **この関数はファイルを 1 バイトも読まない** — 読むのは
    /// `spawn_with_options` が起こしたスレッドだけ。
    fn search_snapshot(
        &self,
        q: &str,
        max: usize,
    ) -> Result<Option<Arc<Vec<file_search::Hit>>>, String> {
        let key = format!("{max}\u{1}{q}");
        let cell = SEARCH.get_or_init(Default::default);
        let mut c = crate::lockx::lock_ok(cell);
        if c.key != key {
            *c = SearchCache {
                key: key.clone(),
                ..Default::default()
            };
        }
        // 走らせた検索が終わっていれば取り込む
        if let Some(rx) = &c.rx {
            if let Ok((hits, _scanned)) = rx.try_recv() {
                c.got = Some(Arc::new(hits));
                c.at = Some(Instant::now());
                c.rx = None;
            }
        }
        let fresh = c.at.is_some_and(|t| t.elapsed() < SEARCH_TTL);
        if c.rx.is_none() && (c.got.is_none() || !fresh) {
            let files: Vec<PathBuf> = self.file_index.iter().map(|f| f.abs.clone()).collect();
            let opts = file_search::SearchOptions {
                query: q.to_string(),
                max_results: max,
                root: self.roots.first().cloned(),
                ..Default::default()
            };
            match file_search::spawn_with_options(files, opts) {
                Ok(rx) => c.rx = Some(rx),
                Err(e) => return Err(e.to_string()),
            }
        }
        Ok(c.got.clone())
    }
}

// ══════════════════════════════════════════════════════════════════════
//  控え (git / 検索) — **UI スレッドの外**で作る
//
//  `ZaivernApp` にフィールドを増やさない (共有ファイルを 1 バイトも触らない)
//  ため、モジュール内の static に置く。鍵はリポジトリの絶対パス / 検索語なので、
//  別のワークスペースの結果が混ざることはない。
// ══════════════════════════════════════════════════════════════════════

/// 変更 1 ファイルぶん。`git` の出力から**裏のスレッドで**組み立てる。
struct ChangeFile {
    rel: String,
    /// `"M"|"A"|"D"|"R"|"?"`
    status: &'static str,
    added: usize,
    removed: usize,
    binary: bool,
    /// 上限で切ったか (追跡外の巨大ファイルなど)。
    truncated: bool,
    hunks: Vec<crate::diff::Hunk>,
}

/// 作業ツリー全体の控え。
struct ChangesSnapshot {
    files: Vec<ChangeFile>,
    added: usize,
    removed: usize,
    truncated: bool,
}

#[derive(Default)]
struct ChangesCache {
    repo: PathBuf,
    got: Option<Result<Arc<ChangesSnapshot>, String>>,
    at: Option<Instant>,
    cost: Option<Duration>,
    inflight: bool,
}

static CHANGES: std::sync::OnceLock<std::sync::Mutex<ChangesCache>> = std::sync::OnceLock::new();

/// 控えを取り直すまでの最短間隔。実際の間隔は直近の所要時間から
/// [`git::scan_interval`] が決める (遅いリポジトリで git が常時走るのを防ぐ)。
const CHANGES_BASE: Duration = Duration::from_secs(3);

#[derive(Default)]
struct SearchCache {
    key: String,
    rx: Option<std::sync::mpsc::Receiver<(Vec<file_search::Hit>, usize)>>,
    got: Option<Arc<Vec<file_search::Hit>>>,
    at: Option<Instant>,
}

static SEARCH: std::sync::OnceLock<std::sync::Mutex<SearchCache>> = std::sync::OnceLock::new();

/// 同じ検索語を撃ち直すまでの猶予。スマホは画面を見ている間ポーリングするので、
/// これが無いと**同じ検索が延々と走り続ける**。
const SEARCH_TTL: Duration = Duration::from_secs(15);

/// 作業ツリーの差分を控えから返す。`None` は「まだ用意できていない」。
///
/// **この関数は git を 1 度も起こさない。** 起こすのは spawn した先だけで、
/// 呼び出し側 (UI スレッド) は必ず即座に戻る。
fn changes_snapshot(repo: &Path) -> Option<Result<Arc<ChangesSnapshot>, String>> {
    let cell = CHANGES.get_or_init(Default::default);
    let mut c = crate::lockx::lock_ok(cell);
    if c.repo != repo {
        *c = ChangesCache {
            repo: repo.to_path_buf(),
            ..Default::default()
        };
    }
    let interval = git::scan_interval(CHANGES_BASE, c.cost);
    let stale = c.at.map(|t| t.elapsed() >= interval).unwrap_or(true);
    if stale && !c.inflight {
        c.inflight = true;
        let repo = repo.to_path_buf();
        std::thread::spawn(move || {
            let t0 = Instant::now();
            let got = scan_changes(&repo).map(Arc::new);
            let cost = t0.elapsed();
            let cell = CHANGES.get_or_init(Default::default);
            let mut c = crate::lockx::lock_ok(cell);
            // 走っている間にワークスペースが変わっていたら、この結果は捨てる
            if c.repo == repo {
                c.got = Some(got);
                c.at = Some(Instant::now());
                c.cost = Some(cost);
                c.inflight = false;
            }
        });
    }
    c.got.clone()
}

/// 未コミットの変更を集める。**必ず裏のスレッドから呼ぶこと**。
///
/// PC の `open_changes_multibuffer` と同じ 2 本
/// (`git::working_tree_diff` → `crate::diff::parse_unified`) を通る。
/// スマホ側で数え直さないのは、真実の在り処を 1 つに保つため。
/// 追跡外のファイルは diff に出てこないので `git status` から別に足す。
fn scan_changes(repo: &Path) -> Result<ChangesSnapshot, String> {
    let out = git::working_tree_diff(repo)?;
    let mut files: Vec<ChangeFile> = Vec::new();
    let mut truncated = false;
    for f in crate::diff::parse_unified(&out) {
        if files.len() >= remote::CHANGES_CAP {
            truncated = true;
            break;
        }
        let status = remote::change_status(&f.old_path, &f.new_path, f.is_rename);
        let rel = if status == "D" {
            f.old_path.clone()
        } else {
            f.new_path.clone()
        };
        if rel.is_empty() || rel == "/dev/null" {
            continue;
        }
        files.push(ChangeFile {
            rel,
            status,
            added: f.additions,
            removed: f.deletions,
            binary: f.is_binary,
            truncated: false,
            hunks: f.hunks,
        });
    }
    // ── 追跡外 (`?`)。git は diff の対象にしないので status から拾う ──
    let args: Vec<String> = ["status", "--porcelain=v1", "-z", "--untracked-files=all"]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if let Ok(porcelain) = git::run_git_at(repo, &args) {
        for rel in remote::untracked_paths_z(&porcelain) {
            if files.len() >= remote::CHANGES_CAP {
                truncated = true;
                break;
            }
            let (added, binary, cut, hunks) = untracked_body(&repo.join(&rel));
            files.push(ChangeFile {
                rel,
                status: "?",
                added,
                removed: 0,
                binary,
                truncated: cut,
                hunks,
            });
        }
    }
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    let added = files.iter().map(|f| f.added).sum();
    let removed = files.iter().map(|f| f.removed).sum();
    Ok(ChangesSnapshot {
        files,
        added,
        removed,
        truncated,
    })
}

/// 追跡外のファイルを「全部が追加された 1 ハンク」として読む。
///
/// 返り値は `(行数, バイナリか, 切ったか, ハンク)`。読めない / 大きすぎる /
/// バイナリなら**中身は付けない** (0 行と言い切らず `binary` / `truncated` で伝える)。
fn untracked_body(path: &Path) -> (usize, bool, bool, Vec<crate::diff::Hunk>) {
    let Ok(meta) = std::fs::metadata(path) else {
        return (0, false, true, Vec::new());
    };
    if !meta.is_file() {
        return (0, false, false, Vec::new());
    }
    if meta.len() > remote::UNTRACKED_READ_CAP {
        return (0, false, true, Vec::new());
    }
    let Ok(bytes) = std::fs::read(path) else {
        return (0, false, true, Vec::new());
    };
    if bytes.contains(&0) {
        return (0, true, false, Vec::new());
    }
    let (text, _enc) = crate::textenc::decode_bytes(&bytes);
    let lines: Vec<crate::diff::DiffLine> = text
        .lines()
        .enumerate()
        .map(|(i, l)| crate::diff::DiffLine {
            kind: crate::diff::LineKind::Added,
            old_no: None,
            new_no: Some(i + 1),
            text: l.to_string(),
            no_newline: false,
            crlf: false,
        })
        .collect();
    let n = lines.len();
    if n == 0 {
        return (0, false, false, Vec::new());
    }
    (
        n,
        false,
        false,
        vec![crate::diff::Hunk {
            header: format!("@@ -0,0 +1,{n} @@"),
            old_start: 0,
            new_start: 1,
            lines,
        }],
    )
}

// ══════════════════════════════════════════════════════════════════════
//  端末のセル → span
// ══════════════════════════════════════════════════════════════════════

/// スマホへ色を渡すのに要るテーマの抜き出し。
///
/// egui の型を span へ持ち込まないための境界でもある
/// (畳み込み自体は `remote::fold_spans` = 純関数)。
struct TermPalette {
    ansi: [[u8; 3]; 16],
    fg: [u8; 3],
    bg: [u8; 3],
}

impl TermPalette {
    fn of(t: &theme::Theme) -> Self {
        let c = |x: egui::Color32| [x.r(), x.g(), x.b()];
        let mut ansi = [[0u8; 3]; 16];
        for (i, slot) in ansi.iter_mut().enumerate() {
            *slot = c(t.ansi[i]);
        }
        Self {
            ansi,
            fg: c(t.term_fg),
            bg: c(t.term_bg),
        }
    }

    /// 既定色は `None` (= 契約どおり省略する)。
    fn hex(&self, c: vt100::Color) -> Option<String> {
        match c {
            vt100::Color::Default => None,
            vt100::Color::Idx(i) => Some(remote::ansi_hex(i, &self.ansi)),
            vt100::Color::Rgb(r, g, b) => Some(remote::hex_rgb(r, g, b)),
        }
    }
}

/// 画面 1 行ぶんのセルを span へ畳む。
fn row_spans(sc: &vt100::Screen, r: u16, cols: u16, pal: &TermPalette) -> Vec<remote::Span> {
    let mut cells: Vec<(String, remote::CellStyle)> = Vec::with_capacity(cols as usize);
    for col in 0..cols {
        let Some(cell) = sc.cell(r, col) else { break };
        // 全角の 2 桁目は 1 つ目のセルに畳まれている。足すと桁が倍になる
        if cell.is_wide_continuation() {
            continue;
        }
        let (mut fg, mut bg) = (pal.hex(cell.fgcolor()), pal.hex(cell.bgcolor()));
        if cell.inverse() {
            // 反転は「既定色どうしの入れ替え」でも見た目が変わる。
            // None のまま入れ替えると**反転が消える**ので、既定色を実体化してから入れ替える
            let f = fg.unwrap_or_else(|| remote::hex_rgb(pal.fg[0], pal.fg[1], pal.fg[2]));
            let b = bg.unwrap_or_else(|| remote::hex_rgb(pal.bg[0], pal.bg[1], pal.bg[2]));
            fg = Some(b);
            bg = Some(f);
        }
        cells.push((
            cell.contents(),
            remote::CellStyle {
                fg,
                bg,
                bold: cell.bold(),
                italic: cell.italic(),
                underline: cell.underline(),
            },
        ));
    }
    remote::fold_spans(&cells)
}

/// 履歴を絶対行 (0 = 最古) で切り出す。返り値は `(全行数, 先頭の絶対行, 行)`。
///
/// vt100 は「いま見えている 1 画面」しか読めないので、`set_scrollback` で
/// 窓をずらしながら 1 画面ずつ読む (`terminal::all_terminal_lines` と同じ作法)。
/// **呼び出し前の戻り量は必ず元へ戻す** — 戻さないと PC 側の表示が飛ぶ。
fn scrollback_rows(
    p: &mut vt100::Parser,
    want: usize,
    before: Option<usize>,
    pal: &TermPalette,
) -> (usize, usize, Vec<Vec<remote::Span>>) {
    let saved = p.screen().scrollback();
    p.set_scrollback(usize::MAX);
    let top = p.screen().scrollback();
    let (rows, cols) = p.screen().size();
    if rows == 0 {
        p.set_scrollback(saved);
        return (0, 0, Vec::new());
    }
    let rows_u = rows as usize;
    let total = top + rows_u;
    let end = before.unwrap_or(total).min(total);
    let start = end.saturating_sub(want);
    let mut out: Vec<Vec<remote::Span>> = Vec::with_capacity(end - start);
    let mut abs = start;
    while abs < end {
        p.set_scrollback(top.saturating_sub(abs));
        // 効いた戻り量で窓の位置を読み直す (vt100 は履歴より深い指定を切り詰める)
        let win_first = top - p.screen().scrollback();
        if abs < win_first {
            break; // 進めない (履歴が縮んだ)。黙って回り続けない
        }
        let mut r = (abs - win_first) as u16;
        while (r as usize) < rows_u && abs < end {
            out.push(row_spans(p.screen(), r, cols, pal));
            r += 1;
            abs += 1;
        }
    }
    p.set_scrollback(saved);
    (total, start, out)
}

/// スマホの一括操作が **PC 側の入口へ合流しているか**をソースで固定する。
///
/// 配達 (確定キーの再送・コスト上限・チェックポイント) を remote 側で
/// 作り直すと、見張りが素通りしたまま「送れているように見える」経路が
/// もう 1 本増える。合流していることは実行時には見えないので、ここで押さえる。
#[cfg(test)]
mod bulk_wiring_tests {
    /// 関数 1 本ぶんの本文を切り出す (実装はテストより前に来る前提)。
    fn body_of(sig: &str) -> String {
        let src = crate::app::SRC.replace("\r\n", "\n");
        let after = src
            .split(sig)
            .nth(1)
            .unwrap_or_else(|| panic!("{sig} が無い"))
            .to_string();
        let end = crate::app::method_end(&after);
        after[..end.min(after.len())].to_string()
    }

    #[test]
    fn 一括送信は既存の入口へ合流する() {
        let body = body_of("pub(super) fn remote_reply_bulk(");
        for entry in [
            "self.queue_submit_all(&text)",
            "self.queue_submit_stalled(&text)",
            "self.queue_submit(job)",
        ] {
            assert!(
                body.contains(entry),
                "一括送信が {entry} を通っていない (配達を作り直している疑い)"
            );
        }
        // 宛先の選び方は純関数 1 本だけ (ここで数え直さない)
        assert!(
            body.contains("remote::bulk_targets(mode, &picks)"),
            "宛先の選び方を remote::bulk_targets 以外で決めている"
        );
    }

    #[test]
    fn 承認は既存の承認キーを使う() {
        let body = body_of("pub(super) fn remote_reply_agent_act(");
        assert!(
            body.contains("s.press_pet_approve_button(Some(&fallback))"),
            "承認が PC 側の入口を通っていない (当て推量の文字を送っている疑い)"
        );
    }

    #[test]
    fn 一覧の状態は看板の判定をそのまま出す() {
        let body = body_of("pub(super) fn remote_reply_agents(");
        for entry in [
            "kanban::column_for(",
            "kanban::state_label(",
            "remote::is_waiting_lane(",
        ] {
            assert!(
                body.contains(entry),
                "一覧が {entry} を使っていない (状態を作り直している疑い)"
            );
        }
        // 画面文字の部分一致で状態を決めていないこと
        assert!(
            !body.contains(".contains(\""),
            "画面テキストの部分一致で状態を決めている"
        );
    }
}

/// スマホの読み取り API が **PC 側の入口へ合流しているか**と、
/// **git を UI スレッドで走らせていないか**をソースで固定する。
///
/// どちらも実行時には見えない (画面は「動いているように」見える) ので、
/// ここで押さえる。合流を外すと真実の在り処が 2 つになり、
/// UI スレッドで git を撃つとフレームが数秒止まる (CLAUDE.md の実測)。
#[cfg(test)]
mod mobile_api_wiring_tests {
    fn src() -> String {
        crate::app::SRC.replace("\r\n", "\n")
    }

    fn body_of(sig: &str) -> String {
        let s = src();
        let after = s
            .split(sig)
            .nth(1)
            .unwrap_or_else(|| panic!("{sig} が無い"))
            .to_string();
        let end = crate::app::method_end(&after);
        after[..end.min(after.len())].to_string()
    }

    #[test]
    fn 変更一覧はpcと同じ入口へ合流する() {
        let s = src();
        // PC の open_changes_multibuffer と同じ 2 本を通る (数え直さない)
        assert!(
            s.contains("git::working_tree_diff(repo)"),
            "変更一覧が git::working_tree_diff を通っていない"
        );
        assert!(
            s.contains("crate::diff::parse_unified(&out)"),
            "変更一覧が diff::parse_unified を通っていない"
        );
        // 状態の 1 文字も純関数 1 本だけ (スマホ側でも remote 側でも作り直さない)
        assert!(
            s.contains("remote::change_status("),
            "状態の判定を remote::change_status 以外で決めている"
        );
    }

    /// **git を UI スレッドで待たない** (CLAUDE.md の鉄則)。
    ///
    /// `remote_reply` は毎フレーム呼ばれる。ここで `git` を起こすと
    /// 1 回 2.3〜10.2 秒かかることがあり、そのままフレームが止まる。
    #[test]
    fn 描画スレッドの応答からgitを起こしていない() {
        for sig in [
            "pub(super) fn remote_reply_changes(",
            "pub(super) fn remote_reply_diff(",
        ] {
            let body = body_of(sig);
            for ng in ["working_tree_diff", "run_git_at", "Command::new"] {
                assert!(
                    !body.contains(ng),
                    "{sig} が {ng} を直接呼んでいる (UI スレッドで git が走る)"
                );
            }
            assert!(
                body.contains("changes_snapshot("),
                "{sig} が控え (changes_snapshot) を読んでいない"
            );
        }
        // 控えを取り直すのは必ず別スレッド
        let snap = body_of("fn changes_snapshot(");
        assert!(
            snap.contains("std::thread::spawn"),
            "控えの取り直しが別スレッドになっていない"
        );
    }

    /// 横断検索も同じ理由で UI スレッドから外す
    /// (索引 4000 件 × 最大 1.5MB を舐める)。
    #[test]
    fn 検索は既存の非同期入口へ合流する() {
        let body = body_of("fn search_snapshot(");
        assert!(
            body.contains("file_search::spawn_with_options("),
            "検索が file_search の非同期入口を通っていない"
        );
        assert!(
            !body.contains("search_with_options(&"),
            "同期検索を UI スレッドで撃っている"
        );
    }

    /// 承認は `approvals.rs` の型と入口をそのまま使う。
    /// ここで種別や応答キーを作り直すと、ポリシー・監査ログが素通りする。
    #[test]
    fn 承認は既存のキューへ合流する() {
        let list = body_of("pub(super) fn remote_reply_approvals(");
        for entry in ["r.kind.as_str()", "r.kind.label()", "r.detail"] {
            assert!(
                list.contains(entry),
                "承認一覧が {entry} を使っていない (種別を作り直している疑い)"
            );
        }
        let act = body_of("pub(super) fn remote_reply_approve(");
        assert!(
            act.contains("self.resolve_approval(id, cmd)"),
            "承認の決着が PC の承認パネルと同じ入口を通っていない"
        );
        for cmd in [
            "agents::approvals::Command::Approve",
            "agents::approvals::Command::Deny",
            "agents::approvals::Command::ApproveKindForAgentAlways",
            "agents::approvals::Command::DenyKindForAgentAlways",
        ] {
            assert!(act.contains(cmd), "{cmd} へ写していない");
        }
        // 承認キーを当て推量で送っていない (approvals.rs が持っている)
        assert!(
            !act.contains("write_bytes"),
            "承認が生バイトを直接送っている"
        );
    }

    /// スマホから届くパスは**必ず**畳んでから使う。
    #[test]
    fn リモートのパスは必ず正規化を通る() {
        let body = body_of("fn resolve_remote_rel(");
        assert!(
            body.contains("remote::safe_rel(rel)"),
            "パスが remote::safe_rel を通っていない"
        );
        assert!(
            body.contains("canonicalize"),
            "実体の前方一致を見ていない (リンクで外へ出られる)"
        );
    }
}

/// 実際に git リポジトリを作って、変更一覧が**中身を返す**ことまで見る。
///
/// 単体テストが全部緑でも、実物を回さないと分からない回帰がある
/// (CLAUDE.md)。ここは `scan_changes` を直に呼ぶので UI もサーバも要らない。
#[cfg(test)]
mod changes_scan_tests {
    use std::path::Path;
    use std::process::Command;

    /// `git` が使えないマシンでは検査そのものを降りる (嘘の緑を出さない)。
    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .args(["-c", "user.email=t@example.com", "-c", "user.name=t"])
            .args(["-C"])
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn 変更一覧は追跡中と追跡外の両方を返す() {
        let dir = crate::test_util::unique_temp_dir("zv-remote", "changes");
        if !git(&dir, &["init", "-q"]) {
            eprintln!("[skip] git が使えないため検査を降りる");
            return;
        }
        std::fs::write(dir.join("kept.txt"), "one\ntwo\nthree\n").expect("write");
        assert!(git(&dir, &["add", "."]), "add");
        assert!(git(&dir, &["commit", "-qm", "init"]), "commit");
        // 追跡中を 1 行足して 1 行消す / 追跡外を 1 つ置く
        std::fs::write(dir.join("kept.txt"), "one\ntwo\nfour\n").expect("write");
        std::fs::write(dir.join("fresh.txt"), "a\nb\n").expect("write");

        let snap = super::scan_changes(&dir).expect("scan");
        let by = |rel: &str| snap.files.iter().find(|f| f.rel == rel);

        let kept = by("kept.txt").expect("追跡中の変更が出ていない");
        assert_eq!(kept.status, "M");
        assert_eq!((kept.added, kept.removed), (1, 1));
        assert!(!kept.hunks.is_empty(), "ハンクが空 (差分が取れていない)");

        let fresh = by("fresh.txt").expect("追跡外が出ていない");
        assert_eq!(fresh.status, "?", "追跡外が ? になっていない");
        assert_eq!(fresh.added, 2, "追跡外の行数を数えていない");
        assert!(!fresh.truncated);
        // 追跡外も「全部追加された 1 ハンク」として読める
        assert_eq!(fresh.hunks.len(), 1);
        assert_eq!(fresh.hunks[0].lines.len(), 2);

        assert_eq!(snap.added, 3, "合計の追加行が合わない");
        assert_eq!(snap.removed, 1);
        assert!(!snap.truncated);

        // **後始末は書かない。** `unique_temp_dir` が古いものを掃く
        // (`test_util::sweep_stale_dirs`)。ここで `remove_dir_all` を書くと、
        // 「復元できない削除は delete_permanently / replace_dest の中だけ」を
        // 守る番人 (`file_tree::tests::破壊的なファイル操作は確認を経ずに呼ばれない`)
        // が **app の非テスト部分と区別できずに落ちる** — `app/*.rs` は
        // `SRC_IMPL` へ丸ごと入るので、テストの中の削除も同じ検査に載る。
    }
}
