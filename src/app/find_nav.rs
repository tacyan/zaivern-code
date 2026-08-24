use super::*;

impl ZaivernApp {
    /// バッファ内検索のヒット一覧を最新にする。
    ///
    /// **本文か検索条件が変わったときだけ**走査する (鍵は本文ハッシュ + 検索語 +
    /// トグル)。正規表現が不正でも panic せず、理由を [`FindHitCache::error`] に
    /// 持ってヒット 0 件として扱う。
    pub(super) fn refresh_find_hits(&mut self, buf: usize, text_hash: u64) {
        if self.find.query.is_empty() {
            self.find_hits = None;
            return;
        }
        let fresh = self.find_hits.as_ref().is_some_and(|c| {
            c.text_hash == text_hash && c.query == self.find.query && c.opts == self.find.opts
        });
        if fresh {
            return;
        }
        let (matcher, error) = match find_buffer::compile(&self.find.query, self.find.opts) {
            Ok(m) => (m, None),
            // 打鍵の途中は必ず不正な状態を通る (`(` を打った瞬間など)。
            // 空マッチャへ落として理由だけ持ち、UI は赤枠と説明で示す。
            Err(e) => (
                find_buffer::compile("", self.find.opts).expect("空クエリは常にコンパイルできる"),
                Some(e.to_string()),
            ),
        };
        let (hits, truncated) = if error.is_some() {
            (Vec::new(), false)
        } else {
            find_buffer::find_all(&self.editor.buffers[buf].text, &matcher)
        };
        // ミニマップの印は「ヒットのある行」なので重複を潰す (hits は行順)
        let mut mm_lines: Vec<usize> = Vec::new();
        for h in &hits {
            if mm_lines.last() != Some(&h.line) {
                if mm_lines.len() >= crate::minimap::MAX_HITS {
                    break;
                }
                mm_lines.push(h.line);
            }
        }
        self.find_hits = Some(FindHitCache {
            text_hash,
            query: self.find.query.clone(),
            opts: self.find.opts,
            matcher,
            hits: std::sync::Arc::new(hits),
            mm_lines: std::sync::Arc::new(mm_lines),
            truncated,
            error,
        });
    }

    /// いまの現在位置がヒット一覧の何番目か (本文が変わって見失っていれば None)。
    /// ヒットは start 昇順なので二分探索で足りる (毎フレーム線形走査しない)。
    pub(super) fn current_hit_index(&self) -> Option<usize> {
        let (s, e) = self.find.current?;
        let c = self.find_hits.as_ref()?;
        let ix = c.hits.binary_search_by_key(&s, |h| h.start).ok()?;
        (c.hits[ix].end == e).then_some(ix)
    }

    /// 次 (Enter) / 前 (⇧Enter) のヒットへ移動する。末尾↔先頭の折り返しに対応し、
    /// 折り返したことは [`FindState::wrapped`] に残して検索バーで示す。
    pub(super) fn find_step(&mut self, forward: bool) {
        let Some(i) = self.editor.active else {
            return;
        };
        if self.find.query.is_empty() {
            return;
        }
        let text_hash = hash_str(&self.editor.buffers[i].text);
        self.refresh_find_hits(i, text_hash);
        // 現在位置が本文と食い違っていたら起点へ戻す (古い位置へ飛ばない)
        let cur_start = self
            .current_hit_index()
            .map(|ix| self.find_hits.as_ref().expect("直前に確認済み").hits[ix].start);
        let from = match cur_start {
            Some(s) if forward => s + 1,
            Some(s) => s,
            None => self.find.anchor,
        };
        let picked = self
            .find_hits
            .as_ref()
            .filter(|c| c.error.is_none())
            .and_then(|c| find_buffer::step(&c.hits, from, forward).map(|(ix, w)| (c.hits[ix], w)));
        let Some((hit, wrapped)) = picked else {
            // 0 件 / 正規表現エラーはバー側 (赤枠と件数) で示す。
            // 打鍵ごとにトーストを出すと通知が埋まるので鳴らさない。
            self.find.current = None;
            self.find.wrapped = None;
            return;
        };
        self.find.current = Some(hit.range());
        self.find.wrapped = wrapped.then_some(forward);
        let text = &self.editor.buffers[i].text;
        let cs = find_buffer::byte_to_char(text, hit.start);
        let ce = find_buffer::byte_to_char(text, hit.end);
        self.pending_select = Some((cs, ce));
        // **フォーカスは動かさない。** 検索は打鍵ごとに走る (インクリメンタル検索)
        // ので、ここで本文へフォーカスを移すと 1 文字打った次のフレームには
        // 本文が入力先になり、2 文字目以降が**ファイルへ打ち込まれる**。
        // VS Code も検索中はフォーカスを検索欄に置いたままヒットだけを動かす。
        self.pending_select_focus = false;
        // **見えている一致のために画面を動かさない** (VS Code の reveal と同じ)。
        // 打鍵ごとに走るので、毎回寄せると 1 文字ごとに本文が飛び跳ねる。
        // 判断は `find_buffer::reveal_scroll` (純関数 + 表テスト) に閉じてある。
        if let Some(y) = find_buffer::reveal_scroll(
            hit.line,
            self.last_row_h,
            self.last_scroll_y,
            self.last_view_h,
        ) {
            self.pending_scroll = Some(y);
        }
    }

    /// 検索バーを開く。選択があればそれを検索語にする (VS Code と同じ)。
    ///
    /// 入れない場合が 2 つある:
    /// * **行をまたぐ選択** — 走査は行単位なので必ず 0 件になる。
    /// * **選択がいまのヒットそのもの** — 直前の検索が付けた選択なので、
    ///   入れると正規表現がヒット文字列に置き換わって消える。
    pub(super) fn open_find(&mut self, ctx: &egui::Context, with_replace: bool) {
        let sel = self.active_cursor_bytes(ctx);
        if let Some((s, e)) = sel {
            let from_hit = e > s && self.find.current == Some((s, e));
            let text = self
                .editor
                .active
                .map(|i| self.editor.buffers[i].text.as_str());
            if let Some(picked) = text.filter(|_| e > s && !from_hit).map(|t| &t[s..e]) {
                if !picked.contains('\n') {
                    // 正規表現モードでは選択を**そのまま探す**意図なのでエスケープする
                    // (VS Code と同じ)。
                    self.find.query = if self.find.opts.regex {
                        regex::escape(picked)
                    } else {
                        picked.to_string()
                    };
                    self.find.current = None;
                    self.find.wrapped = None;
                }
            }
        }
        // 起点はいまのカーソル (選択があれば手前側)。近い方のヒットから回る。
        self.find.anchor = sel.map_or(0, |(s, _)| s);
        self.find.open = true;
        self.find.focus = true;
        if with_replace {
            self.find.replace_open = true;
        }
    }

    /// エディタ本文へフォーカスを返す (検索バーを閉じたときなど)。
    ///
    /// キャレットを動かす用事が無いときの経路。位置も動かすなら
    /// `pending_select` + `pending_select_focus = true` を使う
    /// (フォーカス要求は本文の `TextEdit` を描くときに出る)。
    pub(super) fn focus_editor_body(&self, ctx: &egui::Context) {
        let Some(i) = self.editor.active else {
            return;
        };
        let id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
        ctx.memory_mut(|m| m.request_focus(id));
    }

    /// アクティブバッファのカーソル位置をバイト範囲で返す。
    /// 選択が無ければ `(c, c)`。バッファが無い / 状態が無いときは `None`。
    pub(super) fn active_cursor_bytes(&self, ctx: &egui::Context) -> Option<(usize, usize)> {
        let b = self.editor.active.map(|i| &self.editor.buffers[i])?;
        let ed_id = buf_edit_id(self.cur_pane, b.id);
        let r = egui::TextEdit::load_state(ctx, ed_id)?
            .cursor
            .char_range()?;
        let (s, e) = (
            r.primary.index.min(r.secondary.index),
            r.primary.index.max(r.secondary.index),
        );
        Some((
            editor_ops::char_to_byte(&b.text, s),
            editor_ops::char_to_byte(&b.text, e),
        ))
    }

    /// 指定フォルダをワークスペースとして開き直す (フォルダを開く / worktree を開く)。
    /// マルチルート化後は `set_roots` に一本化してある。ツリー / GitSet / Git パネル /
    /// 索引 / タイトルはすべてその中で追随するので、ここで個別に触らない。
    /// 「開き直す」なので既存のルートは置き換わる (トーストで結果を明示する)。
    pub(super) fn open_workspace(&mut self, dir: PathBuf, ctx: &egui::Context) {
        let roots = file_tree::normalize_roots(vec![dir.clone()]);
        if roots.is_empty() {
            self.toast_warn(trf(
                "📂 {dir} を開けませんでした",
                &[("dir", dir.display().to_string())],
            ));
            return;
        }
        self.set_roots(roots, ctx);
        // 開き直したフォルダを作業フォルダにする (以降のエージェントはここで起動する)。
        // 正規化後のルートを使う: ダイアログや引数のパスは `..` やシンボリックリンクを
        // 含みうるので、ルートと同じ表記に揃えないと後の前方一致が外れる。
        self.agent_root = self.roots.first().cloned();
        self.restore_session(ctx);
        // メニューバーの「最近使用した項目」へ記録
        self.menu_state.touch_folder(&dir);
        recent::save(&self.menu_state);
        self.toast(
            trf(
                "📂 {dir} を開きました",
                &[("dir", dir.display().to_string())],
            ),
            true,
        );
    }

    /// GitHub Issue の「⚡ 着手」ワンフロー:
    /// worktree 作成 → ワークスペースへ追加 → エージェント起動 → 着手指示を入力欄へ。
    ///
    /// 置き場とブランチ名は worktrees プラグインと同じ規約
    /// (リポジトリの隣に `<repo>-wt-<slug>`、ブランチ `wt/<slug>`)。
    /// 指示文の投入はセッションが安全 (Idle) になるまで待つ — 起動直後に書くと
    /// フォルダ信頼確認などの起動時プロンプトへ流れ込んで誤答になるため。
    pub(super) fn start_issue_flow(
        &mut self,
        root: &Path,
        issue: &github::Issue,
        preset_idx: usize,
        ctx: &egui::Context,
    ) {
        let Some(preset) = self.cfg.agents.get(preset_idx).cloned() else {
            return;
        };
        let git = |args: &[&str]| -> Result<String, String> {
            let out = crate::procx::hidden_command("git")
                .arg("-C")
                .arg(root)
                .args(args)
                .output()
                .map_err(|e| e.to_string())?;
            if out.status.success() {
                Ok(crate::textenc::decode_output(&out.stdout)
                    .trim()
                    .to_string())
            } else {
                Err(crate::textenc::decode_output(&out.stderr)
                    .trim()
                    .to_string())
            }
        };
        let repo = match git(&["rev-parse", "--show-toplevel"]) {
            Ok(p) => PathBuf::from(p),
            Err(e) => {
                self.toast(format!("git リポジトリではありません: {e}"), false);
                return;
            }
        };
        let name = repo
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| "repo".into());
        let slug = format!("issue-{}", issue.number);
        let branch = format!("wt/{slug}");
        let dir = repo
            .parent()
            .unwrap_or(&repo)
            .join(format!("{name}-wt-{slug}"));

        if dir.is_dir() {
            // 既にある = 前回の着手の続き。作り直さずそのまま使う。
            self.toast(
                trf(
                    "🌿 既存の worktree を再利用します: {dir}",
                    &[("dir", dir.display().to_string())],
                ),
                true,
            );
        } else {
            // ブランチが残っていたら -b 無しで拾い、無ければ新規に切る
            let fresh = git(&["worktree", "add", "-b", &branch, &dir.to_string_lossy()]);
            if let Err(e1) = fresh {
                if let Err(e2) = git(&["worktree", "add", &dir.to_string_lossy(), &branch]) {
                    self.toast(
                        trf(
                            "worktree を作成できません: {e1} / {e2}",
                            &[("e1", e1), ("e2", e2)],
                        ),
                        false,
                    );
                    return;
                }
            }
            self.toast(
                trf(
                    "🌿 worktree を作成しました: {dir} (ブランチ {branch})",
                    &[
                        ("dir", dir.display().to_string()),
                        ("branch", branch.clone()),
                    ],
                ),
                true,
            );
        }

        self.add_folder_to_workspace(dir.clone(), ctx);

        // worktree を作業ディレクトリにしてエージェントを起動
        let mut p = preset;
        p.cwd = Some(dir.to_string_lossy().into_owned());
        let approval = crate::agents::Approval::from_mode(&self.cfg.approval_mode);
        if let Err(e) = self.agents.launch(&p, &dir, approval, ctx) {
            self.toast(e, false);
            return;
        }
        let sid = match self.agents.sessions.last() {
            Some(s) => s.id,
            None => return,
        };
        let prompt = format!(
            "GitHub Issue #{n}「{title}」に着手してください。詳細は `gh issue view {n}` で確認できます。\
             この作業ツリー (ブランチ {branch}) はこの issue 専用です。実装が終わったらテストを通してコミットしてください。",
            n = issue.number,
            title = issue.title,
        );
        // 入力欄へ入れるだけ (確定はしない) — 着手前に人が内容を確かめられるように。
        self.queue_submit(submit::Job::deferred(sid, prompt, false));
        self.toast(
            tr("⚡ エージェントを起動しました — 準備ができ次第、着手指示を入力欄へ入れます"),
            true,
        );
    }

    /// 🏁 プロンプトレース開始ワンフロー:
    /// racer ごとの worktree 作成 → エージェント起動 → プロンプトの配達予約。
    /// (Issue 着手フローと同じ部品 — worktree / launch / outbox — の組み合わせ。
    /// worktree の作成・検証は race.rs 側。)
    pub(super) fn start_race_flow(
        &mut self,
        prompt: &str,
        preset_indices: &[usize],
        ctx: &egui::Context,
    ) {
        let root = self
            .agent_root
            .clone()
            .unwrap_or_else(|| self.primary_root().to_path_buf());
        let pairs: Vec<(String, String)> = preset_indices
            .iter()
            .filter_map(|&i| self.cfg.agents.get(i))
            .map(|p| (p.icon.clone(), p.name.clone()))
            .collect();
        if pairs.len() != preset_indices.len() {
            self.toast(
                tr("選択したプリセットが見つかりません (設定が変わりました)"),
                false,
            );
            return;
        }
        let mut race = match race::start_race(&root, prompt, &pairs) {
            Ok(r) => r,
            Err(e) => {
                self.toast(e, false);
                return;
            }
        };
        let approval = crate::agents::Approval::from_mode(&self.cfg.approval_mode);
        for (slot, &pi) in preset_indices.iter().enumerate() {
            let Some(preset) = self.cfg.agents.get(pi).cloned() else {
                continue;
            };
            let racer = &mut race.racers[slot];
            // worktree を作業ディレクトリにして起動する (Issue 着手フローと同じ)
            let mut p = preset;
            p.cwd = Some(racer.dir.to_string_lossy().into_owned());
            let dir = racer.dir.clone();
            match self.agents.launch(&p, &dir, approval, ctx) {
                Ok(()) => {
                    racer.session_id = self.agents.sessions.last().map(|s| s.id);
                    racer.status = race::RacerStatus::Running;
                    // 起動直後のプロンプトへ流れ込まないよう、Idle を待つ配達機構へ積む
                    if let Some(sid) = racer.session_id {
                        self.queue_submit(submit::Job::deferred(
                            sid,
                            race::build_race_prompt(prompt, &racer.branch),
                            true,
                        ));
                    }
                }
                Err(e) => {
                    racer.status = race::RacerStatus::Error(e.clone());
                    self.toast(e, false);
                }
            }
        }
        let n = race.racers.len();
        self.race.begin(race);
        self.git_panel.invalidate();
        self.review.invalidate();
        self.toast(
            trf(
                "🏁 レース開始 — {n} 体が並走中 (準備でき次第プロンプトを配達します)",
                &[("n", n.to_string())],
            ),
            true,
        );
    }

    /// レースダッシュボードの操作 (race.rs の RaceAction) を反映する。
    pub(super) fn apply_race_actions(&mut self, acts: Vec<race::RaceAction>, ctx: &egui::Context) {
        for act in acts {
            match act {
                race::RaceAction::Start {
                    prompt,
                    preset_indices,
                } => {
                    self.start_race_flow(&prompt, &preset_indices, ctx);
                }
                race::RaceAction::Focus(idx) => {
                    let pos = self
                        .race
                        .session_of(idx)
                        .and_then(|sid| self.agents.sessions.iter().position(|s| s.id == sid));
                    if let Some(pos) = pos {
                        self.agents.active = pos;
                        self.agents.panel_open = true;
                        self.cockpit = false;
                    } else {
                        self.toast(tr("この racer のセッションは閉じられています"), false);
                    }
                }
                race::RaceAction::OpenDiff(idx) => match self.race.full_diff(idx) {
                    Ok((title, text)) => {
                        let id = self.editor.open_virtual(
                            title,
                            text,
                            crate::editor::BufferKind::RaceDiff { slot: idx },
                        );
                        // 同じタブを使い回すので、古いパース結果は捨てる
                        self.race.drop_diff_cache(id);
                        self.cockpit = false;
                    }
                    Err(e) => self.toast(e, false),
                },
                race::RaceAction::Adopt(idx) => match self.race.adopt(idx) {
                    Ok(msg) => {
                        self.toast(msg, true);
                        self.git_panel.invalidate();
                        self.review.invalidate();
                    }
                    Err(e) => self.toast(e, false),
                },
                race::RaceAction::Discard { idx, force } => {
                    // 走行中/終了済みのセッションが残っていれば先にタブごと閉じる
                    // (Windows では生きたシェルが worktree を掴んで削除を妨げるため)
                    if let Some(pos) = self
                        .race
                        .session_of(idx)
                        .and_then(|sid| self.agents.sessions.iter().position(|s| s.id == sid))
                    {
                        self.close_agent(pos);
                    }
                    match self.race.discard(idx, force) {
                        Ok(msg) => {
                            self.toast(msg, true);
                            self.git_panel.invalidate();
                            self.review.invalidate();
                        }
                        Err(e) => self.toast(e, false),
                    }
                }
                // 🏆 勝者評価 — **提案を出すだけ**。ここで採用 (マージ) はしない。
                // 除外パターンと上限は設定 (`[race_eval]`) から配る。
                race::RaceAction::Evaluate => {
                    self.race.start_eval(&self.cfg.race_eval, ctx);
                }
                race::RaceAction::Close => self.race.close(),
            }
        }
    }

    /// フォルダをワークスペースへ追加する (AddFolder ダイアログと
    /// `#` パレットの worktree 追加が共有する本体)。
    ///
    /// 追加したフォルダは以降のエージェント起動先になる (Issue 着手フローで作った
    /// worktree で、そのままエージェントを動かせるようにするため)。
    pub(super) fn add_folder_to_workspace(&mut self, dir: PathBuf, ctx: &egui::Context) {
        let before = self.roots.len();
        let mut next = self.roots.clone();
        next.push(dir.clone());
        let next = file_tree::normalize_roots(next);
        // normalize_roots が畳んだ = 既存ルート配下だった
        if next.len() == before && next.iter().any(|r| dir.starts_with(r)) {
            self.toast_warn(trf(
                "{dir} は既にワークスペースに含まれています",
                &[("dir", dir.display().to_string())],
            ));
        } else {
            self.set_roots(next, ctx);
            self.toast(
                trf(
                    "📚 {dir} をワークスペースに追加しました",
                    &[("dir", dir.display().to_string())],
                ),
                true,
            );
        }
        // 既に含まれていた場合も、そのフォルダを選んだ意思は同じなので追随させる
        self.track_agent_root(&dir);
    }

    // ═══════════════════════════════════════════════════════════════
    // 第 2 次配線: レビュー / 折りたたみ / ブックマーク / テーブル / LSP
    // ═══════════════════════════════════════════════════════════════

    /// パレット・キーバインドから来る「エディタまわりの追加機能」をさばく。
    pub(super) fn apply_cmd_editor_extras(&mut self, cmd: Cmd, ctx: &egui::Context) {
        match cmd {
            Cmd::OpenReview => self.open_review_panel(),
            Cmd::SetReviewBase(ref kind) => {
                let base = match kind.as_str() {
                    "staged" => git_panel::ReviewBase::Staged,
                    "unstaged" => git_panel::ReviewBase::Unstaged,
                    _ => git_panel::ReviewBase::Head,
                };
                let label = base.label();
                self.review.set_base(base);
                self.open_review_panel();
                self.toast(trf("レビューの比較: {b}", &[("b", label)]), true);
            }
            Cmd::SetReviewMode(ref kind) => {
                let mode = match kind.as_str() {
                    "focus" => git_panel::ReviewMode::Focus,
                    "queue" => git_panel::ReviewMode::Queue,
                    _ => git_panel::ReviewMode::Files,
                };
                let label = mode.label();
                self.review.set_mode(mode);
                self.open_review_panel();
                self.toast(trf("レビュー: {m}", &[("m", label)]), true);
            }
            Cmd::CompareWithSaved => self.compare_with_saved(),
            Cmd::SelectForCompare => self.select_for_compare(),
            Cmd::CompareWithSelected => self.compare_with_selected(),
            Cmd::ToggleFold | Cmd::FoldAll | Cmd::UnfoldAll | Cmd::FoldLevel(_) => {
                self.apply_fold_cmd(&cmd)
            }
            Cmd::ToggleBookmark | Cmd::NextBookmark | Cmd::PrevBookmark | Cmd::ClearBookmarks => {
                self.apply_bookmark_cmd(&cmd)
            }
            Cmd::MarkToggleMnemonic
            | Cmd::MarksPanel
            | Cmd::MarkJump
            | Cmd::MarkJumpDigit(_)
            | Cmd::MarksClearAll => self.apply_mark_cmd(&cmd),
            Cmd::ReopenClosedTab => self.reopen_closed_tab(),
            Cmd::ToggleTableView => self.toggle_table_view(),
            Cmd::LspCompletion => {
                // パレット経由だとフォーカスが本文から外れている。
                // キャレットを置き直して本文へ戻してから候補を出す
                // (pending_select の経路が request_focus までやる)。
                if let Some(i) = self.editor.active {
                    let (ln, col) = self.editor.cursor;
                    let c = editor_ops::char_index_at(
                        &self.editor.buffers[i].text,
                        ln.saturating_sub(1),
                        col.saturating_sub(1),
                    );
                    self.pending_select = Some((c, c));
                }
                let w = self.word_before_caret();
                self.lsp_completion.invoke(&w, Instant::now());
            }
            Cmd::LspReferences => self.lsp_find_references(),
            Cmd::LspSymbols => self.lsp_document_symbols(),
            Cmd::LspRename => self.lsp_start_rename(),
            Cmd::LspFormat => {
                if !self.lsp_format_document(false) {
                    self.toast_warn(tr("整形できるサーバーが動いていません"));
                }
            }
            Cmd::LspCodeAction => self.lsp_code_actions(),
            Cmd::LspSignatureHelp => self.lsp_signature_help(),
            Cmd::ToggleLspHighlight => {
                self.lsp_highlight_on = !self.lsp_highlight_on;
                if !self.lsp_highlight_on {
                    self.clear_highlight_spans();
                }
                let msg = if self.lsp_highlight_on {
                    tr("同一シンボルのハイライト: ON")
                } else {
                    tr("同一シンボルのハイライト: OFF")
                };
                self.toast(msg, true);
            }
            Cmd::ToggleFormatOnSave => {
                self.format_on_save = !self.format_on_save;
                self.save_editor_prefs(ctx);
                let msg = if self.format_on_save {
                    tr("保存時に整形する: ON")
                } else {
                    tr("保存時に整形する: OFF")
                };
                self.toast(msg, true);
            }
            _ => {}
        }
    }

    // ── 任意 2 テキストの比較 (VS Code の Compare With) ──────────

    /// 開いているタブの本文を**ディスク上の保存済み**と比べる。
    ///
    /// `diff.rs` は Git 基準専用だったので、任意の 2 テキストで駆動できる
    /// [`crate::diff::diff_texts`] を使う。ハンクの形は git 由来のものと
    /// 同じなので、既存の描画・折りたたみ・語単位ハイライトがそのまま効く。
    pub(super) fn compare_with_saved(&mut self) {
        let Some(i) = self.editor.active else {
            self.toast_warn(tr("比較するタブがありません"));
            return;
        };
        let Some(path) = self.editor.buffers[i].path.clone() else {
            self.toast_warn(tr("保存されていないタブは比較できません"));
            return;
        };
        let disk = match crate::diff::read_compare_file(&path, crate::diff::COMPARE_MAX_BYTES) {
            Ok(t) => t,
            Err(e) => {
                self.toast_warn(e);
                return;
            }
        };
        let name = path.display().to_string();
        if disk.truncated {
            self.toast_warn(tr("保存済みが大きいため、途中までを比較しています"));
        }
        let f = if disk.binary {
            crate::diff::binary_diff(&name, &name)
        } else {
            crate::diff::diff_texts(
                &trf("{p} (保存済み)", &[("p", name.clone())]),
                &trf("{p} (編集中)", &[("p", name.clone())]),
                &disk.text,
                &self.editor.buffers[i].text,
            )
        };
        self.show_compare(tr("保存済みと比較"), f);
    }

    /// このファイルを「比較の左側」として覚える。
    pub(super) fn select_for_compare(&mut self) {
        let Some(p) = self
            .editor
            .active
            .and_then(|i| self.editor.buffers[i].path.clone())
        else {
            self.toast_warn(tr("保存されたファイルを開いてから実行してください"));
            return;
        };
        let name = p.display().to_string();
        self.compare_left = Some(p);
        self.toast(trf("比較の左側: {p}", &[("p", name)]), true);
    }

    /// 覚えた左側と、いま開いているファイルを比べる。
    pub(super) fn compare_with_selected(&mut self) {
        let Some(left) = self.compare_left.clone() else {
            self.toast_warn(tr("先に「比較の左側として選ぶ」を実行してください"));
            return;
        };
        let Some(right) = self
            .editor
            .active
            .and_then(|i| self.editor.buffers[i].path.clone())
        else {
            self.toast_warn(tr("保存されたファイルを開いてから実行してください"));
            return;
        };
        match crate::diff::compare_files(&left, &right, crate::diff::COMPARE_MAX_BYTES) {
            Ok(f) => self.show_compare(tr("2 つのファイルを比較"), f),
            Err(e) => self.toast_warn(e),
        }
    }

    /// 比較結果を出す。**レイアウトを押しのけない別ウィンドウ**に置く
    /// (「画面が突然変わらない」— 大きな領域を勝手に開かない)。
    pub(super) fn show_compare(&mut self, title: String, file: crate::diff::FileDiff) {
        let empty = file.hunks.is_empty() && !file.is_binary;
        self.compare_view = Some(CompareView {
            title,
            file,
            comments: crate::diff::DiffCommentStore::default(),
        });
        if empty {
            self.toast(tr("差分はありません"), true);
        }
    }

    /// 比較ウィンドウ。閉じるまで出しっぱなし (中身は既存の diff レンダラ)。
    pub(super) fn compare_window(&mut self, ctx: &egui::Context) {
        let Some(view) = self.compare_view.as_mut() else {
            return;
        };
        let theme = self.theme.clone();
        let mut open = true;
        egui::Window::new(view.title.clone())
            .open(&mut open)
            .collapsible(true)
            .resizable(true)
            .default_size(egui::vec2(760.0, 520.0))
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(view.file.display_path())
                        .color(theme.text_dim)
                        .monospace()
                        .small(),
                );
                if view.file.hunks.is_empty() && !view.file.is_binary {
                    ui.label(
                        RichText::new(tr("差分はありません"))
                            .color(theme.text_dim)
                            .small(),
                    );
                    return;
                }
                egui::ScrollArea::both()
                    .id_salt("zv-compare-view")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let _ = crate::diff::diff_ui_with_hunk_actions(
                            ui,
                            &theme,
                            std::slice::from_ref(&view.file),
                            &mut view.comments,
                            None,
                        );
                    });
            });
        if !open {
            self.compare_view = None;
        }
    }

    // ── A: PR 風のローカル変更レビュー ───────────────────────────

    /// 「変更をレビュー」を開く (サイドバー → Git タブ → レビューサブタブ)。
    pub(super) fn open_review_panel(&mut self) {
        self.sidebar_open = true;
        self.sidebar_tab = SidebarTab::Git;
        self.git_sub_review = true;
        self.review.invalidate();
        self.persist_session();
    }

    // ── B/C 共通のキャレット操作 ─────────────────────────────────

    /// キャレット行 (0 始まり)。`Editor::cursor` は 1 始まりで持っている。
    pub(super) fn caret_line0(&self) -> usize {
        self.editor.cursor.0.saturating_sub(1)
    }

    /// 指定行 (0 始まり) を隠している折りたたみを開く。ジャンプの前に呼ぶ。
    pub(super) fn reveal_line(&mut self, i: usize, line0: usize) {
        let b = &mut self.editor.buffers[i];
        if !b.folds.is_line_hidden(line0) {
            return;
        }
        let starts: Vec<usize> = b
            .folds
            .ranges()
            .iter()
            .filter(|r| r.hides(line0))
            .map(|r| r.start_line)
            .collect();
        for s in starts {
            b.folds.unfold(s);
        }
        b.gutter = None;
        self.fold_view = None;
    }

    // ── B: 折りたたみ ────────────────────────────────────────────

    /// 折りたたみ系のコマンド。行番号は 0 始まりで扱う。
    pub(super) fn apply_fold_cmd(&mut self, cmd: &Cmd) {
        let Some(i) = self.editor.active else {
            return;
        };
        let line = self.caret_line0();
        let b = &mut self.editor.buffers[i];
        b.refresh_folds();
        let mut warn: Option<String> = None;
        match cmd {
            Cmd::ToggleFold => {
                if !b.folds.toggle_fold(line) {
                    warn = Some(tr("この行には折りたためる範囲がありません"));
                }
            }
            Cmd::FoldAll => b.folds.fold_all(),
            Cmd::UnfoldAll => b.folds.unfold_all(),
            Cmd::FoldLevel(n) => b.folds.fold_level(*n),
            _ => {}
        }
        b.gutter = None;
        self.fold_view = None;
        if let Some(w) = warn {
            self.toast_warn(w);
        }
    }

    // ── C: ブックマーク / 閉じたタブ ─────────────────────────────

    /// ブックマーク系のコマンド。
    pub(super) fn apply_bookmark_cmd(&mut self, cmd: &Cmd) {
        let Some(i) = self.editor.active else {
            return;
        };
        let line = self.caret_line0();
        let target = bookmark_cmd_target(cmd, &mut self.editor.buffers[i].bookmarks, line);
        self.editor.buffers[i].gutter = None;
        match target {
            Some(l) => {
                self.reveal_line(i, l);
                self.goto_line(l + 1);
            }
            None if matches!(cmd, Cmd::NextBookmark | Cmd::PrevBookmark) => {
                self.toast_warn(tr("このファイルにはブックマークがありません"))
            }
            None => {}
        }
    }

    /// ニーモニック付きブックマーク (`crate::marks`) のコマンド。
    ///
    /// **描画も判定も `marks` 側の純粋関数に置く** — ここは「いまのファイルと
    /// 行を渡して、返ってきた要求を実行する」だけに保つ (app.rs は 42k 行あり、
    /// 10 本のブランチが直列にマージされるので差分を局所化する)。
    pub(super) fn apply_mark_cmd(&mut self, cmd: &Cmd) {
        match cmd {
            Cmd::MarksPanel => self.marks.panel_open = !self.marks.panel_open,
            Cmd::MarkJump => self.marks.jump_open = !self.marks.jump_open,
            Cmd::MarksClearAll => {
                self.marks.clear_all();
                self.toast(tr("ブックマークをすべて消しました"), true);
            }
            Cmd::MarkJumpDigit(d) => match self.marks.goto_digit(*d) {
                Some(a) => self.run_mark_action(a),
                None => self.toast_warn(tr("そのニーモニックのブックマークがありません")),
            },
            Cmd::MarkToggleMnemonic => {
                let Some(i) = self.editor.active else {
                    self.toast_warn(tr("ファイルを開いてから使ってください"));
                    return;
                };
                let Some(path) = self.editor.buffers[i].path.clone() else {
                    self.toast_warn(tr("保存されていないファイルには付けられません"));
                    return;
                };
                let line = self.caret_line0();
                let text = self.editor.buffers[i].text.clone();
                let sel = self.editor_selection_text();
                self.marks.begin_toggle(&path, line, &text, sel);
            }
            _ => {}
        }
    }

    /// 選択範囲の文字列 (無ければ `None`)。ブックマークの説明の種になる。
    pub(super) fn editor_selection_text(&self) -> Option<String> {
        let i = self.editor.active?;
        let (a, b) = self.editor_sel_chars?;
        let (lo, hi) = (a.min(b), a.max(b));
        if lo >= hi {
            return None;
        }
        let t = &self.editor.buffers[i].text;
        Some(t.chars().skip(lo).take(hi - lo).collect())
    }

    /// `marks` から返ってきた要求を実行する。
    pub(super) fn run_mark_action(&mut self, a: marks::MarkAction) {
        match a {
            marks::MarkAction::Goto(path, line0) => {
                if self.active_file_path().as_deref() != Some(path.as_path()) {
                    self.open_path(&path);
                }
                if let Some(i) = self.editor.active {
                    self.reveal_line(i, line0);
                }
                self.goto_line(line0 + 1);
            }
            marks::MarkAction::Toast(msg, ok) => self.toast(msg, ok),
        }
    }

    /// ブックマークの小窓 (一覧 / ニーモニック選択 / ジャンプ) を描く。
    pub(super) fn marks_windows(&mut self, ctx: &egui::Context) {
        let hints = marks::Hints {
            toggle: self.key_hint(BindAction::MarkToggleMnemonic),
            panel: self.key_hint(BindAction::MarksPanel),
        };
        let theme = self.theme.clone();
        let root = self.primary_root().to_path_buf();
        let acts = marks::windows_ui(ctx, &mut self.marks, &theme, &hints, &root);
        for a in acts {
            self.run_mark_action(a);
        }
    }

    /// 直前に閉じたタブを開き直す (VS Code: ⇧⌘T)。
    pub(super) fn reopen_closed_tab(&mut self) {
        let Some(t) = self.editor.closed_tabs.pop_closed() else {
            self.toast_warn(tr("開き直せるタブがありません"));
            return;
        };
        if !t.path.exists() {
            self.toast_warn(trf(
                "{path} はもう存在しません",
                &[("path", t.path.display().to_string())],
            ));
            return;
        }
        self.open_path(&t.path);
        // 閉じた時点のキャレット位置 (1 始まり) とスクロールへ戻す
        self.goto_line(t.cursor.0);
        if t.scroll > 0.0 {
            self.pending_scroll = Some(t.scroll);
        }
    }

    // ── D: CSV / TSV のテーブル表示 ──────────────────────────────

    /// 表形式ファイルのグリッド表示 ⇄ 素のテキスト表示を切り替える。
    pub(super) fn toggle_table_view(&mut self) {
        let Some(i) = self.editor.active else {
            return;
        };
        let is_table = self.editor.buffers[i]
            .path
            .as_deref()
            .map(crate::editor::is_table_path)
            .unwrap_or(false);
        let showing = self.editor.buffers[i].table.is_some();
        match table_toggle_decision(is_table, showing) {
            TableToggle::Drop => self.editor.buffers[i].drop_table(),
            TableToggle::Build => {
                self.editor.buffers[i].build_table();
            }
            TableToggle::NotTable => self.toast_warn(tr("このファイルは CSV / TSV ではありません")),
        }
    }
}
