use super::*;

impl ZaivernApp {
    /// primary ルートが属する repo のブランチ名 (git.rs 側で TTL キャッシュ)。
    pub(super) fn git_branch(&mut self) -> Option<String> {
        self.gitinfo.branch()
    }

    pub(super) fn open_path(&mut self, path: &Path) {
        match self.editor.open(path, self.highlighter) {
            Ok(reloaded) => {
                // Cockpit 表示中でも左の編集ペインに開いた内容が見えるため、
                // ビューは切り替えない
                if reloaded {
                    if let Some(i) = self.editor.active {
                        let title = self.editor.buffers[i].title.clone();
                        self.toast(
                            trf(
                                "↻ {title} を再読み込みしました(外部で変更)",
                                &[("title", title)],
                            ),
                            true,
                        );
                        self.queue_lsp_change(i);
                    }
                }
                self.queue_hook(plugins::HookEvent::FileOpen, Some(path.to_path_buf()));
                // 開いたファイルのルートを作業フォルダにする。マルチルートで
                // 別フォルダのファイルへ移ったら、次のエージェントもそちらで起動する。
                self.track_agent_root(path);
                // メニューバーの「最近使用した項目」へ記録
                self.touch_recent_file(path);
                self.persist_session()
            }
            Err(e) => self.toast(e, false),
        }
    }

    /// **プレビュー**としてファイルを開く (ツリー / パレットの 1 回クリック)。
    ///
    /// VS Code の斜体タブと同じ約束:
    /// * 直前のプレビュータブは**置き換わる** — 眺めるだけでタブが増え続けない。
    /// * 未保存の編集があるタブは置き換えず、確定タブへ昇格させてから残す。
    /// * 同じファイルをもう一度開いたら確定タブへ昇格する
    ///   (ツリーの 2 回目のクリック = ダブルクリック相当)。
    ///
    /// `preview_tabs = false` なら素通しで [`Self::open_path`] と同じ。
    pub(super) fn open_path_preview(&mut self, path: &Path) {
        if !self.cfg.preview_tabs {
            self.open_path(path);
            return;
        }
        let pane = self.panes.focus_id();
        let prev = self.panes.preview_of(pane);
        let already = self
            .editor
            .buffers
            .iter()
            .find(|b| b.path.as_deref() == Some(path))
            .map(|b| b.id);
        if let Some(id) = already {
            // 2 回目 = 確定タブへ昇格 (以後は他のプレビューに潰されない)
            if prev == Some(id) {
                self.panes.promote(id);
            }
            self.open_path(path);
            return;
        }
        // 使い捨て枠を空ける。編集中のタブは捨てずに確定タブへ格上げする。
        if let Some(old) = prev {
            let dirty = self
                .editor
                .buffers
                .iter()
                .find(|b| b.id == old)
                .map(|b| b.dirty())
                .unwrap_or(false);
            if dirty {
                self.panes.promote(old);
            } else if self.panes.open_count(old) > 1 {
                self.panes.close_tab(pane, old);
            } else if let Some(i) = self.editor.buffers.iter().position(|b| b.id == old) {
                self.editor.close(i);
            }
        }
        self.open_path(path);
        // 開けていたら、そのタブを新しいプレビュー枠にする。
        if let Some(id) = self
            .editor
            .buffers
            .iter()
            .find(|b| b.path.as_deref() == Some(path))
            .map(|b| b.id)
        {
            self.sync_panes();
            let pane = self.panes.focus_id();
            self.panes.set_preview(pane, Some(id));
        }
    }

    /// **見張りスレッドの 1 フレームぶん。**
    ///
    /// 見張りが「変わった」と言っているなら、`check_external_changes` の
    /// 1 秒ゲートを開けてから通す。開けないと、直前に別の理由でチェックが
    /// 走っていた場合にゲートへ捨てられ、**次のフレームを誰も予約しない**ので
    /// 取り込みがそこで止まる (= 外部変更が画面へ出ない)。
    pub(super) fn watch_tick(&mut self, ctx: &egui::Context) {
        let w = self
            .fswatch
            .get_or_insert_with(|| crate::fswatch::FsWatch::new(ctx));
        if w.take_news() {
            self.ext_check_at = None;
        }
    }

    /// **見張りへ「いま何を、どの姿だと思っているか」を置き直す。**
    ///
    /// 指紋が変わらないフレームでは 1 バイトも確保しない
    /// (パスと mtime を舐めて畳むだけ)。置き直しを怠ると、見張りは
    /// 古い姿と食い違ったままになり 1 秒ごとに起こし続ける。
    pub(super) fn publish_watch_targets(&mut self) {
        use crate::fswatch;
        let Some(w) = self.fswatch.as_mut() else {
            return;
        };
        if !w.active() {
            return;
        }
        // ① 指紋 (確保なし)
        let mut sig = fswatch::Sig::new();
        for b in &self.editor.buffers {
            if let Some(p) = b.path.as_deref() {
                sig.file(p, b.disk_mtime, b.conflict_notified);
            }
        }
        for (d, m) in self.tree.watch_dirs() {
            sig.dir(d, m);
        }
        // ② 変わったときだけ組み立てる
        let editor = &self.editor;
        let tree = &self.tree;
        w.publish(sig.finish(), || {
            let mut v: Vec<fswatch::Target> = editor
                .buffers
                .iter()
                .filter_map(|b| {
                    b.path
                        .clone()
                        .map(|p| fswatch::Target::file(p, b.disk_mtime, b.conflict_notified))
                })
                .collect();
            v.extend(
                tree.watch_dirs()
                    .map(|(d, m)| fswatch::Target::dir(d.to_path_buf(), m)),
            );
            v
        });
    }

    /// 開いているタブのファイルが外部(エージェント等)で書き換えられていないか
    /// 約1秒ごとに確認する。未保存の編集が無いバッファはディスクの内容へ自動で
    /// 読み直し、編集と競合したバッファは上書きせず一度だけ警告する。
    /// あわせてファイルツリーも外部でのファイル追加・削除を検知して自動更新する。
    pub(super) fn check_external_changes(&mut self) {
        let fresh = self
            .ext_check_at
            .map(|t| (t.elapsed().as_millis() as u64) < EXT_CHECK_MS)
            .unwrap_or(false);
        if fresh {
            return;
        }
        self.ext_check_at = Some(Instant::now());
        self.tree.refresh_if_changed();
        for ev in self.editor.check_external() {
            match ev {
                ExternalEvent::Reloaded { index, title } => {
                    self.toast(
                        trf(
                            "↻ {title} を再読み込みしました(外部で変更)",
                            &[("title", title)],
                        ),
                        true,
                    );
                    self.queue_lsp_change(index);
                }
                ExternalEvent::Conflict { title } => {
                    self.toast_warn(trf(
                        "⚠ {title} が外部で変更されました — 未保存の編集があるため読み直していません({key} で上書き)",
                        &[("title", title), ("key", self.key_hint(BindAction::Save))],
                    ));
                }
            }
        }
    }

    /// リロード後のテキストを LSP へ(デバウンス付きで)通知する
    pub(super) fn queue_lsp_change(&mut self, i: usize) {
        // 本文が変わった時点で、ハイライトの char 添字は指す場所を失う。
        // ずれた位置を塗り続けるくらいなら消す (次のデバウンス満了で入れ直る)。
        if !self.lsp_highlight_spans.is_empty() {
            self.lsp_highlight_spans.clear();
            self.lsp_highlight_buf = None;
        }
        let Some(b) = self.editor.buffers.get(i) else {
            return;
        };
        let Some(p) = b.path.clone() else {
            return;
        };
        let key = self.lsp_key_for(&p, &b.lang);
        if self.lsp.contains_key(&key) {
            self.lsp_pending
                .insert(p, (b.text.clone(), Instant::now(), key));
        }
    }

    /// ステータスバーで選ばれたインデントの切替を反映する。
    ///
    /// 「表示だけ」は本文に触らない = 取り消し履歴も汚さない。
    /// 「変換する」は [`EditOp::ConvertIndent`] 1 回で済ませるので、
    /// ⌘Z 一回で元に戻る。
    pub(super) fn apply_indent_action(&mut self, action: IndentAction, ctx: &egui::Context) {
        let Some(i) = self.editor.active else {
            return;
        };
        match action {
            IndentAction::Display(st) => {
                self.editor.buffers[i].indent = st;
                self.toast(
                    trf(
                        "インデントの表示: {kind} {n}",
                        &[
                            ("kind", tr(if st.tabs { "タブ" } else { "スペース" })),
                            ("n", st.width.to_string()),
                        ],
                    ),
                    true,
                );
            }
            IndentAction::Convert(st) => {
                if self.editor.buffers[i].indent == st {
                    return;
                }
                self.editor_op(ctx, EditOp::ConvertIndent(st));
                self.toast(
                    trf(
                        "インデントを変換しました: {kind} {n}",
                        &[
                            ("kind", tr(if st.tabs { "タブ" } else { "スペース" })),
                            ("n", st.width.to_string()),
                        ],
                    ),
                    true,
                );
            }
            IndentAction::Detect => {
                // 推定は設定に関わらず必ず走らせる (明示的に頼まれた操作なので)
                let saved = self.editor.indent_defaults;
                self.editor.indent_defaults.0 = true;
                self.editor.apply_indent_defaults(i);
                self.editor.indent_defaults = saved;
                let st = self.editor.buffers[i].indent;
                self.toast(
                    trf(
                        "中身から推定しました: {kind} {n}",
                        &[
                            ("kind", tr(if st.tabs { "タブ" } else { "スペース" })),
                            ("n", st.width.to_string()),
                        ],
                    ),
                    true,
                );
            }
        }
    }

    /// `config.toml` のインデント設定を [`editor::Editor`] へ流し込む。
    ///
    /// タブを開く経路は 6 か所あるので、設定を引き回さずに Editor 側へ
    /// 1 度だけ置く (漏れたタブだけ既定値になる、という壊れ方を避ける)。
    pub(super) fn sync_indent_defaults(&mut self) {
        self.editor.indent_defaults = (
            self.cfg.detect_indentation,
            editor_ops::IndentStyle::new(!self.cfg.insert_spaces, self.cfg.tab_size),
        );
    }

    // ── 永続する UI 設定 (egui memory) ──
    //
    // 保存時のクリーンアップ (末尾空白 / 末尾の空行 / 最終行の改行 / 整形) は
    // **config.toml が持ち主**で、ここにはセッション中の切替だけを覚える。
    // 検索の Aa / Ab| / .* は画面の状態なので egui memory だけで完結する。

    pub(super) fn search_prefs_id() -> egui::Id {
        egui::Id::new("zv-search-prefs")
    }

    pub(super) fn editor_prefs_id() -> egui::Id {
        egui::Id::new("zv-editor-prefs")
    }

    /// 起動後の最初のフレームで永続設定を読む (ctx はここでしか手に入らない)。
    pub(super) fn load_prefs_once(&mut self, ctx: &egui::Context) {
        if self.prefs_loaded {
            return;
        }
        self.prefs_loaded = true;
        // インデントの既定を Editor へ (以後開くタブはこれを使う)。
        // 起動時に復元されたタブは推定前なので、ここで取り直す。
        self.sync_indent_defaults();
        for i in 0..self.editor.buffers.len() {
            self.editor.apply_indent_defaults(i);
        }
        // (大文字小文字, 単語単位, 正規表現)
        let (c, w, r) = ctx
            .data_mut(|d| *d.get_persisted_mut_or(Self::search_prefs_id(), (false, false, false)));
        self.gsearch.case_sensitive = c;
        self.gsearch.whole_word = w;
        self.gsearch.regex = r;
        // (末尾空白を除去, 末尾の空行を落とす, 最終行に改行, 保存時に整形)
        // **種は config.toml** — 設定に書いた既定で始まり、セッション中の
        // 切替 (パレット / メニュー) だけをここへ覚える。
        let seed = (
            self.cfg.trim_trailing_whitespace,
            self.cfg.trim_final_newlines,
            self.cfg.insert_final_newline,
            self.cfg.format_on_save,
        );
        let (t, tf, n, f) =
            ctx.data_mut(|d| *d.get_persisted_mut_or(Self::editor_prefs_id(), seed));
        self.save_trim_trailing = t;
        self.save_trim_final_newlines = tf;
        self.save_final_newline = n;
        self.format_on_save = f;
        // 差分ビューの表示モードは config が持ち主。ここで 1 回だけ ctx へ種を蒔く
        // (以降はビューのトグル / パレット / F7 が ctx 側を書き換え、
        //  update の頭で config へ書き戻す)。
        crate::diff::set_diff_mode(
            ctx,
            crate::diff::DiffMode::from_config_str(&self.cfg.diff_view),
        );
    }

    pub(super) fn save_search_prefs(&self, ctx: &egui::Context) {
        let v = (
            self.gsearch.case_sensitive,
            self.gsearch.whole_word,
            self.gsearch.regex,
        );
        ctx.data_mut(|d| d.insert_persisted(Self::search_prefs_id(), v));
    }

    pub(super) fn save_editor_prefs(&self, ctx: &egui::Context) {
        let v = (
            self.save_trim_trailing,
            self.save_trim_final_newlines,
            self.save_final_newline,
            self.format_on_save,
        );
        ctx.data_mut(|d| d.insert_persisted(Self::editor_prefs_id(), v));
    }
}
