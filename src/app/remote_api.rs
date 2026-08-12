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
            remote::Query::Cmd(name, arg) => self.remote_reply_cmd(name, *arg, ctx),
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
        let agents: Vec<_> = self
            .agents
            .sessions
            .iter()
            .map(|s| {
                json!({
                    "id": s.id, "title": s.title, "icon": s.icon,
                    "running": s.running(), "attention": s.attention,
                })
            })
            .collect();
        let presets: Vec<_> = self
            .cfg
            .agents
            .iter()
            .map(|p| json!({"name": p.name, "icon": p.icon}))
            .collect();
        json!({
            "ok": true, "workspace": ws, "tabs": tabs,
            "active": self.editor.active, "file": file, "dirty": dirty,
            "cursor": [self.editor.cursor.0, self.editor.cursor.1],
            "agents": agents, "agent_active": self.agents.active,
            "presets": presets, "approval": self.cfg.approval_mode,
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
        // 先に畳んでポートを解放する (Drop が accept の終了まで待つ)
        drop(old);
        match remote::RemoteServer::rebind(ctx.clone(), bind, token, prefer) {
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
                let msg = trf("待ち受けの切り替えに失敗しました: {e}", &[("e", e.clone())]);
                self.remote_err = Some(e);
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

        // Windows の受信許可。ここが無いと「QR は読めるのにスマホからだけ
        // 何も起きない」になるので、繋がる前提として真っ先に見せる
        self.fw.ensure_checked();
        let fw_check = firewall::applicable() && lan_mode;
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
                match (&url_full, &err) {
                    (Some(url), _) => {
                        ui.vertical_centered(|ui| {
                            ui.label(
                                RichText::new(tr(if lan_mode {
                                    "同じ Wi-Fi のスマホで QR を読み取るだけで接続"
                                } else {
                                    "SSH トンネル経由 — 外出先のスマホで QR を読み取って接続"
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
                            if let Some(re) = reach.as_ref().filter(|_| lan_mode) {
                                ui.add_space(6.0);
                                if re.hits == 0 {
                                    ui.label(
                                        RichText::new(tr(
                                            "📶 まだスマホからの接続はありません\n\u{3000}\
                                             スマホが真っ白なままなら、通信が PC まで届いていません\n\u{3000}\
                                             (ファイアウォール / スマホが同じ Wi-Fi でない / \
                                             ルータのプライバシーセパレータ)",
                                        ))
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
}
