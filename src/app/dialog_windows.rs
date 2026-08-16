use super::*;

impl ZaivernApp {
    // ── メニュー関連の小窓 (行移動 / 問題 / ショートカット / バージョン情報) ──

    pub(super) fn menu_windows_ui(&mut self, ctx: &egui::Context) {
        self.goto_line_window(ctx);
        self.marks_windows(ctx);
        self.git_commit_window(ctx);
        self.git_history_window(ctx);
        self.problems_window(ctx);
        self.compare_window(ctx);
        self.shortcuts_window(ctx);
        self.settings_window(ctx);
        self.hotexit_conflict_window(ctx);
        self.about_window(ctx);
        self.whats_new_window(ctx);
        self.license_window(ctx);
    }

    pub(super) fn goto_line_window(&mut self, ctx: &egui::Context) {
        if !self.goto_open {
            return;
        }
        let mut go: Option<(usize, usize)> = None;
        let mut close = false;
        egui::Window::new(tr("行/列へ移動"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, [0.0, 90.0])
            .show(ctx, |ui| {
                let resp = ui.add(
                    egui::TextEdit::singleline(&mut self.goto_input)
                        .desired_width(220.0)
                        .hint_text(tr("行[:列] — 例 42:5")),
                );
                resp.request_focus();
                if ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    // パレットの `:` モードと同じ扱い — 日本語入力中は
                    // `４２：５` のように全角で入るので半角へ畳んでから読む。
                    go = editor_ops::parse_goto(&crate::palette::fold_goto(&self.goto_input));
                    close = true;
                }
                if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                    close = true;
                }
            });
        if let Some((line, col)) = go {
            self.goto_line_col(line, col);
        }
        if close {
            self.goto_open = false;
        }
    }

    /// 0 起点の (行, 桁) へ飛ぶ。⌃G の小窓とパレットの `:123` の共通経路。
    ///
    /// 行数を超える値・0 行目・巨大な値は `editor_ops::char_index_at` が
    /// 末尾へ丸める (パニックしない)。負の値は `parse_goto` が弾く。
    /// 端末リンクから開く。0 起点の `(行, 桁)`。
    ///
    /// 端末が指す桁は **文字数**なので、LSP の UTF-16 桁を扱う
    /// [`Self::jump_to_lsp_pos`] ではなく [`Self::goto_line_col`] を通す。
    pub(super) fn open_path_at(&mut self, path: &Path, line: usize, col: usize) {
        if !path.is_file() {
            self.toast(
                trf("{p} が見つかりません", &[("p", path.display().to_string())]),
                false,
            );
            return;
        }
        if self.active_file_path().as_deref() != Some(path) {
            self.open_path(path);
        }
        self.goto_line_col(line, col);
    }

    pub(super) fn goto_line_col(&mut self, line: usize, col: usize) {
        let Some(i) = self.editor.active else { return };
        let ch = editor_ops::char_index_at(&self.editor.buffers[i].text, line, col);
        if let Some(p) = self.active_file_path() {
            self.nav_push(p, ch);
        }
        self.jump_to_char(ch, 0);
    }

    // ─── マルチバッファ (複数ファイルの抜粋を 1 本の面へ) ────────────
    //
    // 「散らばった注目点をファイルを開いて回らずに一望する」ための面。
    // 種 (`multibuffer::Seed`) を作るところだけが出所ごとに違い、
    // 組み立て・表示・移動は 1 本に集約してある。

    /// マルチバッファへ載せるファイルの上限。これを超えるものは丸ごと落とす
    /// (索引に巨大ファイルを引き込むと、開いた瞬間にメモリが跳ねる)。
    pub(super) const MULTIBUFFER_MAX_FILE_BYTES: u64 = 4 * 1024 * 1024;

    /// 種からマルチバッファを組み立ててタブで開く (全出所の共通経路)。
    ///
    /// 本文は **エディタで開いていればその未保存の本文**、無ければディスクから
    /// 取る。画面に出ているものと索引が食い違わないようにするため。
    pub(super) fn open_multibuffer(
        &mut self,
        source: crate::multibuffer::Source,
        subtitle: &str,
        seeds: &[crate::multibuffer::Seed],
    ) {
        use crate::multibuffer as mbuf;
        let mut mb = {
            let editor = &self.editor;
            // 同じファイルに何十件も種があるので、読み込みは 1 回だけ
            let mut cache: HashMap<PathBuf, Option<Vec<String>>> = HashMap::new();
            mbuf::build(
                source,
                subtitle,
                seeds,
                None,
                mbuf::BuildOpts::for_source(source),
                |path| {
                    cache
                        .entry(path.to_path_buf())
                        .or_insert_with(|| {
                            if let Some(b) = editor.buffers.iter().find(|b| {
                                b.kind == crate::editor::BufferKind::File
                                    && b.path.as_deref() == Some(path)
                            }) {
                                return Some(mbuf::split_lines(&b.text));
                            }
                            let meta = std::fs::metadata(path).ok()?;
                            if !meta.is_file() || meta.len() > Self::MULTIBUFFER_MAX_FILE_BYTES {
                                return None;
                            }
                            let bytes = std::fs::read(path).ok()?;
                            // CP932 等も開ける経路をそのまま使う (UTF-8 決め打ちにしない)
                            let (text, _) = crate::textenc::decode_bytes(&bytes);
                            Some(mbuf::split_lines(&text))
                        })
                        .clone()
                },
            )
        };
        // 表示名は複数ルートを考慮した既存規則へ揃える (`multibuffer::label_for`
        // は単一ルートしか知らないので、ここで上書きする)
        for e in &mut mb.excerpts {
            e.label = self.rel_label(&e.path);
        }
        let empty = mb.is_empty();
        // 開いた直後のカーソルは**最初の一致**。0 (先頭の見出し) にすると
        // 1 回目の「次へ」が 2 件目へ飛んで 1 件目を飛ばす。
        let cursor = mbuf::first_focus(&mbuf::rows(&mb), &mb);
        let id = self.editor.open_multibuffer(mb);
        self.multibuffer_cursor.insert(id, cursor);
        // 中央ビューはエディタでなければ見えない (Cockpit / 看板 / デッキが前面だと
        // タブを開いても何も起きなかったように見える)
        self.cockpit = false;
        self.kanban = false;
        self.deck = false;
        if empty {
            self.toast_warn(tr("表示するものがありませんでした"));
        }
        self.persist_session();
    }

    /// ワークスペース検索の全ヒットをマルチバッファで開く。
    pub(super) fn open_search_multibuffer(&mut self) {
        use crate::multibuffer as mbuf;
        let seeds: Vec<mbuf::Seed> = self
            .gsearch
            .results
            .iter()
            .map(|h| mbuf::Seed {
                path: h.path.clone(),
                // Hit.line は 0 起点、Seed.line は 1 起点
                line: h.line + 1,
                note: String::new(),
                severity: 0,
                // col / len は**元の行**基準なので、そのまま本文へ当てられる
                mark: (h.len > 0).then_some((h.col, h.col + h.len)),
            })
            .collect();
        let subtitle = self.gsearch.query.clone();
        self.open_multibuffer(mbuf::Source::Search, &subtitle, &seeds);
    }

    /// ワークスペース全体の診断をマルチバッファで開く。
    pub(super) fn open_problems_multibuffer(&mut self) {
        use crate::multibuffer as mbuf;
        let mut items = self.collect_problems();
        // 重い順 → ファイル順 → 行順。読む順序が毎回同じになる
        items.sort_by(|a, b| {
            a.severity
                .cmp(&b.severity)
                .then_with(|| a.path.cmp(&b.path))
                .then_with(|| a.line.cmp(&b.line))
        });
        let seeds: Vec<mbuf::Seed> = items
            .iter()
            .map(|it| mbuf::Seed {
                path: it.path.clone(),
                // ProblemItem.line は 0 起点 (LSP)、Seed.line は 1 起点
                line: it.line + 1,
                note: it.message.clone(),
                severity: it.severity,
                mark: None,
            })
            .collect();
        self.open_multibuffer(mbuf::Source::Problems, "", &seeds);
    }

    /// 作業ツリーの変更 (未コミット) をマルチバッファで開く。
    ///
    /// `git diff HEAD` を **1 回だけ**走らせて全ファイルぶんの変更行を取る
    /// (ファイルごとに git を起動すると、変更が多いときに固まる)。
    /// (可視性は `pub(crate)` — 機能レジストリ `src/features/changes.rs` から
    /// **メソッド越しに**呼ぶため。`ZaivernApp` のフィールドは公開しない。)
    pub(crate) fn open_changes_multibuffer(&mut self) {
        use crate::multibuffer as mbuf;
        let Some(top) = self.git_ops_repo() else {
            self.toast_warn(tr("git リポジトリではありません"));
            return;
        };
        let out = match git::working_tree_diff(&top) {
            Ok(out) => out,
            Err(e) => {
                self.toast(e, false);
                return;
            }
        };
        let mut seeds: Vec<mbuf::Seed> = Vec::new();
        for f in crate::diff::parse_unified(&out) {
            if f.is_binary || f.new_path.is_empty() || f.new_path == "/dev/null" {
                continue;
            }
            let path = top.join(&f.new_path);
            for h in &f.hunks {
                let added: Vec<usize> = h
                    .lines
                    .iter()
                    .filter(|l| l.kind == crate::diff::LineKind::Added)
                    .filter_map(|l| l.new_no)
                    .collect();
                if added.is_empty() {
                    // 削除だけのハンク。消えた行は本文に無いので、
                    // **消えた場所** (新しい側の行番号) を注目点にする
                    seeds.push(mbuf::Seed {
                        path: path.clone(),
                        line: h.new_start.max(1),
                        note: tr("ここで削除"),
                        severity: 0,
                        mark: None,
                    });
                    continue;
                }
                for l in added {
                    seeds.push(mbuf::Seed::plain(path.clone(), l));
                }
            }
        }
        let subtitle = self.rel_label(&top);
        self.open_multibuffer(mbuf::Source::Changes, &subtitle, &seeds);
    }

    /// LSP の documentSymbol を `@` ピッカーが読める形へ落とす。
    ///
    /// **`@` ピッカーは LSP が出した範囲を「正確 (=)」として扱う。**
    /// LSP が動いていない言語は `mention::scan_symbols` の近似 (≈) で補う。
    pub(super) fn rebuild_mention_symbols(&mut self) {
        self.mention_syms.clear();
        let Some(path) = self.lsp_symbols_path.clone() else {
            return;
        };
        let Some(root) = self.roots.first() else {
            return;
        };
        let Some(rel) = crate::ignore::rel_slash(root, &path) else {
            return;
        };
        // documentSymbol は入れ子を持つ。子も含めて平らにし、
        // それぞれの `range` (本体全体) をそのまま添付範囲に使う。
        let mut ranges: Vec<(String, usize, usize, usize, u8)> = Vec::new();
        collect_symbol_ranges(&self.lsp_symbols, &mut ranges);
        for (name, start, end, end_col, kind) in ranges {
            self.mention_syms.push(mention::SymbolHit {
                name,
                kind_label: symbol_kind_label(kind).to_string(),
                rel: rel.clone(),
                start_line: start,
                end_line: end,
                end_col: Some(end_col),
                exact: true,
            });
        }
    }

    /// `@` の確定で「App しか持っていない本文」を求められたら返す。
    pub(super) fn serve_mention_need(&mut self, need: mention::Need) {
        match need {
            mention::Need::Terminal { token, id } => {
                let text = self
                    .agents
                    .sessions
                    .iter()
                    .find(|s| s.id == id)
                    .map(|s| {
                        s.screen_tail_lines(mention::TERM_TAIL_ROWS, mention::TERM_TAIL_COLS)
                            .join("\n")
                    })
                    .unwrap_or_else(|| tr("その端末はもうありません"));
                self.mention.provide(&token, text, mention::Keep::Tail);
            }
            mention::Need::Problems { token } => {
                let body = self
                    .collect_problems()
                    .iter()
                    .map(|p| {
                        format!(
                            "{}:{}:{}: [{}] {}",
                            p.path.display(),
                            p.line + 1,
                            p.col + 1,
                            severity_word(p.severity),
                            p.message
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.mention.provide(&token, body, mention::Keep::Head);
            }
        }
    }

    /// 送信直前に `@` 添付を本文へ展開する。宛先が 1 体なら、その CLI の
    /// ファイル参照記法 (`agents.rs` のカタログ) に従って重複を省く。
    pub(super) fn expand_mentions(
        &self,
        text: &str,
        to: Option<u64>,
        ledger: ComposerTarget,
    ) -> String {
        let cmd = to
            .and_then(|id| self.agents.sessions.iter().find(|s| s.id == id))
            .map(|s| s.command.clone());
        self.mention.expand_for(text, cmd.as_deref(), ledger)
    }

    /// ワークスペース全体の診断を集める。
    ///
    /// **開いていないファイルも対象**。LSP サーバーはプロジェクト全体の
    /// `publishDiagnostics` を送ってくるので、[`lsp::LspClient::all_diagnostics`]
    /// から丸ごと拾う (以前は開いているバッファだけを回していた)。
    pub(super) fn collect_problems(&self) -> Vec<ProblemItem> {
        let open: HashSet<PathBuf> = self
            .editor
            .buffers
            .iter()
            .filter_map(|b| b.path.clone())
            .collect();
        let mut out: Vec<ProblemItem> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for client in self.lsp.values() {
            // can_fix = その LSP がクイックフィックスに対応しているか
            // (対応していない言語で「押しても何も起きないボタン」を並べない)。
            let can_fix = client.caps().code_action;
            for (path, diags) in client.all_diagnostics() {
                // 同じファイルを複数サーバーが見ている場合の二重計上を防ぐ
                if !seen.insert(path.clone()) {
                    continue;
                }
                let title = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_else(|| path.display().to_string());
                for d in diags.iter() {
                    out.push(ProblemItem {
                        path: path.clone(),
                        title: title.clone(),
                        line: d.line,
                        col: d.col,
                        severity: d.severity,
                        message: d.message.clone(),
                        can_fix,
                        open: open.contains(&path),
                    });
                }
            }
        }
        out
    }

    pub(super) fn problems_window(&mut self, ctx: &egui::Context) {
        if !self.problems_open {
            return;
        }
        let theme = self.theme.clone();
        let all = self.collect_problems();
        let counts = problem_counts(&all);
        let shown = filter_problems(&all, &self.problems_filter);
        let rows = group_problems(shown, &self.problems_collapsed);

        let mut open = self.problems_open;
        let mut filter = self.problems_filter.clone();
        let mut toggle_group: Option<PathBuf> = None;
        let mut jump: Option<(PathBuf, usize, usize)> = None;
        // 借用を握ったまま &mut self を呼べないので、押されたら記録だけする
        let mut open_multi = false;
        // 「この診断を直す」= その位置へ飛んでからクイックフィックスを要求する
        let mut fix: Option<(PathBuf, usize, usize)> = None;
        let collapsed = self.problems_collapsed.clone();
        egui::Window::new(tr("⚠ 問題"))
            .open(&mut open)
            .default_size([620.0, 340.0])
            .show(ctx, |ui| {
                // ── 絞り込み ──
                // severity のトグル (SelectableLabel なので同じラベルが並んでも
                // ID は衝突しない) と、ファイル名・メッセージ両方に効く検索。
                ui.horizontal_wrapped(|ui| {
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, icon) in PROBLEM_SEV_ICONS.iter().enumerate() {
                        let label = format!("{icon} {}", counts[i]);
                        let r = ui.selectable_label(filter.sev[i], label);
                        if r.on_hover_text(tr(PROBLEM_SEV_NAMES[i])).clicked() {
                            filter.sev[i] = !filter.sev[i];
                        }
                    }
                    ui.add_space(space::SM);
                    // 0 件のときは押しても空の面が出るだけなので出さない
                    if !all.is_empty()
                        && ui
                            .small_button(tr("⿴ まとめて開く"))
                            .on_hover_text(tr(
                                "全ての問題を前後の文脈つきで 1 枚の面に並べます (マルチバッファ)",
                            ))
                            .clicked()
                    {
                        open_multi = true;
                    }
                    // 幅は「残り」を素直に使う。どの窓幅でも見切れない。
                    let w = (ui.available_width() - 4.0).max(80.0);
                    ui.add_sized(
                        [w, 22.0],
                        egui::TextEdit::singleline(&mut filter.text)
                            .id_salt("zv-problems-filter")
                            .hint_text(tr("ファイル名 / メッセージで絞り込み")),
                    );
                });
                ui.add_space(space::XS);

                if rows.is_empty() {
                    let msg = if all.is_empty() {
                        tr("問題は検出されていません")
                    } else {
                        tr("絞り込みに一致する問題はありません")
                    };
                    let sub = if all.is_empty() {
                        tr("LSP が有効なファイルが対象です")
                    } else {
                        trf(
                            "{n} 件の問題を絞り込みで隠しています",
                            &[("n", all.len().to_string())],
                        )
                    };
                    problems_empty_card(ui, &theme, &msg, &sub);
                    return;
                }

                // 1000 件でも破綻しないよう、行高を固定して見えている分だけ描く。
                let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
                egui::ScrollArea::vertical()
                    .id_salt("zv-problems")
                    .auto_shrink(false)
                    .show_rows(ui, row_h, rows.len(), |ui, range| {
                        for row in &rows[range] {
                            match row {
                                ProblemRow::Header {
                                    path,
                                    title,
                                    count,
                                    worst,
                                } => {
                                    let open_group = !collapsed.contains(path);
                                    let label = format!(
                                        "{} {title}  ({count})",
                                        if open_group { "▾" } else { "▸" }
                                    );
                                    let full = path.display().to_string();
                                    let r = ui.add(
                                        egui::Button::new(
                                            RichText::new(label)
                                                .size(12.5)
                                                .color(diagview::severity_color(&theme, *worst)),
                                        )
                                        .frame(false)
                                        .wrap_mode(egui::TextWrapMode::Truncate),
                                    );
                                    if r.on_hover_text(full).clicked() {
                                        toggle_group = Some(path.clone());
                                    }
                                }
                                ProblemRow::Item(it) => {
                                    let icon =
                                        PROBLEM_SEV_ICONS[(it.severity.clamp(1, 4) - 1) as usize];
                                    let label = format!(
                                        "    {icon} {}:{}  {}",
                                        it.line + 1,
                                        it.col + 1,
                                        it.message.lines().next().unwrap_or("")
                                    );
                                    // 1 行 = [💡] + 診断本文。💡 は固定幅なので、残りは
                                    // 必ず available_width に収まる (見切れは Truncate 側で吸収)。
                                    ui.horizontal(|ui| {
                                        if it.can_fix
                                            && it.open
                                            && ui
                                                .add(egui::Button::new("💡").frame(false))
                                                .on_hover_text(tr("クイックフィックス"))
                                                .clicked()
                                        {
                                            fix = Some((it.path.clone(), it.line, it.col));
                                        }
                                        let color =
                                            if it.open { theme.text } else { theme.text_dim };
                                        let r = ui.add(
                                            egui::Button::new(
                                                RichText::new(label).size(12.5).color(color),
                                            )
                                            .frame(false)
                                            .wrap_mode(egui::TextWrapMode::Truncate),
                                        );
                                        if r.on_hover_text(&it.message).clicked() {
                                            jump = Some((it.path.clone(), it.line, it.col));
                                        }
                                    });
                                }
                            }
                        }
                    });
            });
        self.problems_open = open;
        self.problems_filter = filter;
        if let Some(p) = toggle_group {
            if !self.problems_collapsed.remove(&p) {
                self.problems_collapsed.insert(p);
            }
        }
        if open_multi {
            self.open_problems_multibuffer();
        }
        if let Some((path, line, col)) = jump {
            // 行だけでなく**桁**まで飛ぶ (LSP の col は UTF-16 単位)
            self.jump_to_lsp_pos(&path, line, col);
        }
        if let Some((path, line, col)) = fix {
            // 先に診断の位置へ飛ぶ (codeAction はキャレット位置で範囲を決めるため)
            self.jump_to_lsp_pos(&path, line, col);
            self.lsp_code_actions();
        }
    }

    // ── Hot Exit — 未保存の本文の退避と復元 ────────────────────────

    /// 未保存バッファの安い指紋。**本文をハッシュしない** —
    /// 「前フレームから何か動いたか」だけを見るための値で、実際に何を
    /// 書くかは [`Self::hotexit_flush`] が厳密に決める。
    ///
    /// 未保存かどうかは `Buffer::dirty()` ではなく `history.at_saved_point()`
    /// で見る。`dirty()` は保存点から外れているときに全文をハッシュするので、
    /// 毎フレーム呼ぶと巨大ファイルでフレームを落とす。
    pub(super) fn hotexit_fingerprint(&self) -> u64 {
        let mut h: u64 = 0;
        for b in &self.editor.buffers {
            if b.kind.read_only() || b.history.at_saved_point() {
                continue;
            }
            h = combine_hash(h, b.id);
            h = combine_hash(h, b.text.len() as u64);
            h = combine_hash(h, b.history.revision());
            h = combine_hash(h, b.saved_hash);
        }
        h
    }

    /// 退避を間引いて書き出す。**変化があったときだけ**締切を立て、
    /// 締切のぶんだけ再描画を予約する (常時再描画も常時 I/O も起こさない)。
    pub(super) fn hotexit_tick(&mut self, ctx: &egui::Context) {
        if !self.cfg.hot_exit {
            return;
        }
        let fp = self.hotexit_fingerprint();
        if fp != self.hotexit_fingerprint {
            self.hotexit_fingerprint = fp;
            if self.hotexit_due.is_none() {
                let wait = Duration::from_millis(self.cfg.hot_exit_interval_ms);
                self.hotexit_due = Some(Instant::now() + wait);
                // 間隔ぶん先の 1 フレームだけ予約する。編集が止まれば
                // 予約も止まるので、アイドル時のコストはゼロのまま。
                crate::perf::repaint_after(ctx, wait, "hotexit_tick");
            }
        }
        let Some(due) = self.hotexit_due else {
            return;
        };
        if Instant::now() < due {
            return;
        }
        self.hotexit_due = None;
        self.hotexit_flush();
    }

    /// いまの未保存バッファをそのまま退避へ反映する (間引きを飛ばす)。
    ///
    /// 保存・タブを閉じた直後に呼ぶ。ここを通らないと、保存済みの退避が
    /// 次の間隔まで残り続ける。
    pub(super) fn hotexit_flush(&mut self) {
        if !self.cfg.hot_exit {
            return;
        }
        let report = {
            let snaps: Vec<session::HotExitSnapshot> = self
                .editor
                .buffers
                .iter()
                // 読み取り専用タブ (画像 / PDF / 16 進) は本文を持たない
                .filter(|b| !b.kind.read_only() && b.dirty())
                .map(|b| session::HotExitSnapshot {
                    id: b.id,
                    path: b.path.as_deref(),
                    title: b.title.as_str(),
                    text: b.text.as_str(),
                    saved_hash: b.saved_hash,
                })
                .collect();
            self.hotexit.sync(&snaps)
        };
        // 黙って落とさない。ただし退避は数秒ごとに走るので、同じバッファに
        // ついて伝えるのは 1 回だけ (トーストを埋め尽くさない)。
        let fresh: Vec<String> = report
            .skipped
            .iter()
            .filter(|t| !self.hotexit_warned.contains(*t))
            .cloned()
            .collect();
        // もう上限を超えていないものは、次に超えたときまた伝える
        self.hotexit_warned
            .retain(|t| report.skipped.iter().any(|s| s == t));
        if !fresh.is_empty() {
            let names = fresh.join(", ");
            let kb = self.cfg.hot_exit_max_kb.to_string();
            self.hotexit_warned.extend(fresh);
            self.toast_warn(trf(
                "⚠ {names} は {kb} KiB を超えるため退避していません — 保存してください",
                &[("names", names), ("kb", kb)],
            ));
        }
    }

    /// 退避しておいた未保存の本文を戻す (起動時に 1 回だけ)。
    ///
    /// ディスク側が外から変わっていたバッファは**黙って戻さず**、
    /// [`Self::hotexit_conflicts`] へ積んで選ばせる。
    pub(super) fn hotexit_restore(&mut self) {
        if !self.cfg.hot_exit {
            return;
        }
        let saved = session::load_hotexit(self.hotexit.dir());
        if saved.is_empty() {
            return;
        }
        let mut restored = 0usize;
        for r in saved {
            let Some(path) = r.path.clone() else {
                // 名前のないバッファ (untitled) も戻す
                self.editor.new_untitled();
                let Some(b) = self.editor.buffers.last_mut() else {
                    continue;
                };
                b.title = r.title.clone();
                // 履歴は戻さない (本文だけ)。空の新規タブからの差分にする
                b.reset_text(r.text);
                b.saved_hash = hash_str("");
                restored += 1;
                continue;
            };
            if r.disk.needs_choice() {
                self.hotexit_conflicts.push(HotExitConflict {
                    path,
                    title: r.title,
                    text: r.text,
                    disk_text: r.disk_text,
                    state: r.disk,
                });
                continue;
            }
            let i = match self
                .editor
                .buffers
                .iter()
                .position(|b| b.path == Some(path.clone()))
            {
                Some(i) => i,
                None => {
                    if self.editor.open(&path, self.highlighter).is_err() {
                        // 保存前に名前だけ付いていたバッファ (ディスクに実体が無い)
                        self.editor.new_untitled();
                        let last = self.editor.buffers.len() - 1;
                        let b = &mut self.editor.buffers[last];
                        b.path = Some(path.clone());
                        b.title = r.title.clone();
                        last
                    } else {
                        match self
                            .editor
                            .buffers
                            .iter()
                            .position(|b| b.path == Some(path.clone()))
                        {
                            Some(i) => i,
                            None => continue,
                        }
                    }
                }
            };
            let b = &mut self.editor.buffers[i];
            // ディスクの内容として読み込んだ時点のハッシュを覚えておき、
            // 本文を差し替えたあとで戻す = 「未保存」の印がちゃんと残る。
            let base = b.saved_hash;
            b.reset_text(r.text);
            b.saved_hash = base;
            self.queue_lsp_change(i);
            restored += 1;
        }
        if restored > 0 {
            self.toast(
                trf(
                    "↩ 未保存だった {n} 件の本文を復元しました",
                    &[("n", restored.to_string())],
                ),
                true,
            );
        }
        // 競合していない分は退避を最新化する (戻した本文はまだ未保存のまま)
        self.hotexit_fingerprint = self.hotexit_fingerprint();
        self.hotexit_flush();
    }

    /// 復元した本文とディスクが食い違っていたときに選ばせる小窓。
    ///
    /// **どちらかを勝手に採らない。** 退避を採ればディスクは上書きされず
    /// 未保存のまま残り、ディスクを採れば退避だけが捨てられる。
    pub(super) fn hotexit_conflict_window(&mut self, ctx: &egui::Context) {
        if self.hotexit_conflicts.is_empty() {
            return;
        }
        let theme = self.theme.clone();
        let mut keep: Option<usize> = None;
        let mut drop_one: Option<usize> = None;
        let mut diff: Option<usize> = None;
        let mut keep_all = false;
        let mut drop_all = false;
        let mut open = true;
        egui::Window::new(tr("⚠ 未保存の復元とディスクが食い違っています"))
            .open(&mut open)
            .collapsible(false)
            .default_size([560.0, 320.0])
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(
                    RichText::new(tr(
                        "退避しておいた未保存の本文と、いまのディスクの内容が違います。どちらを開くか選んでください (ディスクは書き換えません)。",
                    ))
                    .size(11.5)
                    .color(theme.text_dim),
                );
                ui.separator();
                egui::ScrollArea::vertical()
                    .id_salt("zv-hotexit-conflicts")
                    .max_height(220.0)
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        for (i, c) in self.hotexit_conflicts.iter().enumerate() {
                            let avail = ui.available_width();
                            ui.horizontal_wrapped(|ui| {
                                ui.set_max_width(avail);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(&c.title).size(12.0).strong(),
                                    )
                                    .truncate(),
                                )
                                .on_hover_text(c.path.to_string_lossy());
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(tr(c.state.reason()))
                                            .size(11.0)
                                            .color(theme.warn),
                                    )
                                    .truncate(),
                                );
                            });
                            ui.horizontal_wrapped(|ui| {
                                ui.set_max_width(avail);
                                if ui
                                    .button(tr("復元した本文を開く"))
                                    .on_hover_text(tr(
                                        "未保存のまま開きます。保存するまでディスクは変わりません",
                                    ))
                                    .clicked()
                                {
                                    keep = Some(i);
                                }
                                if ui
                                    .button(tr("ディスクを開く"))
                                    .on_hover_text(tr("退避しておいた本文は捨てます"))
                                    .clicked()
                                {
                                    drop_one = Some(i);
                                }
                                if c.disk_text.is_some()
                                    && ui
                                        .button(tr("差分を見る"))
                                        .on_hover_text(tr("ディスクと復元した本文を並べます"))
                                        .clicked()
                                {
                                    diff = Some(i);
                                }
                            });
                            ui.separator();
                        }
                    });
                ui.horizontal(|ui| {
                    if ui.button(tr("すべて復元した本文を開く")).clicked() {
                        keep_all = true;
                    }
                    if ui.button(tr("すべてディスクを開く")).clicked() {
                        drop_all = true;
                    }
                });
            });
        // 「×」で閉じたら、まだ選んでいない分は安全側 (本文を失わない側) へ倒す
        if !open {
            keep_all = true;
        }
        if let Some(i) = diff {
            let c = &self.hotexit_conflicts[i];
            let (path, text) = (c.path.clone(), c.text.clone());
            let disk = c.disk_text.clone().unwrap_or_default();
            self.open_hotexit_diff(&path, &disk, &text);
            return;
        }
        let take: Vec<(HotExitConflict, bool)> = if keep_all {
            std::mem::take(&mut self.hotexit_conflicts)
                .into_iter()
                .map(|c| (c, true))
                .collect()
        } else if drop_all {
            std::mem::take(&mut self.hotexit_conflicts)
                .into_iter()
                .map(|c| (c, false))
                .collect()
        } else if let Some(i) = keep.or(drop_one) {
            let use_saved = keep.is_some();
            vec![(self.hotexit_conflicts.remove(i), use_saved)]
        } else {
            Vec::new()
        };
        for (c, use_saved) in take {
            self.apply_hotexit_choice(c, use_saved);
        }
    }

    /// 競合 1 件の決着をつける。`use_saved` なら退避の本文で開き (未保存)、
    /// そうでなければディスクをそのまま開く (退避は捨てる)。
    pub(super) fn apply_hotexit_choice(&mut self, c: HotExitConflict, use_saved: bool) {
        let opened = self.editor.open(&c.path, self.highlighter).is_ok();
        if !use_saved {
            if !opened {
                self.toast_warn(trf(
                    "⚠ {title} を開けませんでした",
                    &[("title", c.title.clone())],
                ));
            }
            self.hotexit_flush();
            return;
        }
        let i = match self
            .editor
            .buffers
            .iter()
            .position(|b| b.path == Some(c.path.clone()))
        {
            Some(i) => i,
            None => {
                // ディスクから消えていた: 名前を持った未保存タブとして復活させる
                self.editor.new_untitled();
                let last = self.editor.buffers.len() - 1;
                let b = &mut self.editor.buffers[last];
                b.path = Some(c.path.clone());
                b.title = c.title.clone();
                last
            }
        };
        let b = &mut self.editor.buffers[i];
        // ディスクから読んだ時点のハッシュへ戻す = 未保存印が必ず残る。
        // ディスクを書き換えるのは、この後ユーザーが保存したときだけ。
        let base = b.saved_hash;
        b.reset_text(c.text);
        b.saved_hash = base;
        self.queue_lsp_change(i);
        self.hotexit_flush();
    }

    /// 退避した本文とディスクの差分を読み取り専用タブで見せる。
    ///
    /// 描画は既存の unified diff ビューをそのまま使う
    /// (`BufferKind::CommitDiff` = 1 本しか出ない読み取り専用の差分タブ)。
    /// 新しいタブ種別を足すより、既にある差分の見せ方に揃える方が良い。
    pub(super) fn open_hotexit_diff(&mut self, path: &Path, disk: &str, saved: &str) {
        let title = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string_lossy().into_owned());
        let mut text = format!(
            "--- {}\n+++ {}\n",
            tr("ディスク"),
            tr("復元した未保存の本文")
        );
        text.push_str(&unified_lines(disk, saved, HOTEXIT_DIFF_MAX_CELLS));
        self.editor.open_virtual(
            trf("差分: {title}", &[("title", title)]),
            text,
            crate::editor::BufferKind::CommitDiff,
        );
    }

    /// 設定画面 (VS Code の「設定 (UI)」相当)。
    ///
    /// 一覧・あいまい検索・`@modified`・変更マーカー・既定へ戻す、そして
    /// GUI で表現しきれない設定のための「config.toml を直接編集」。
    /// 値の書き戻しは [`config::save_settings`] が**その行だけ**を差し替える。
    pub(super) fn settings_window(&mut self, ctx: &egui::Context) {
        if !self.settings_open {
            return;
        }
        let theme = self.theme.clone();
        let mut open = self.settings_open;
        let mut query = std::mem::take(&mut self.settings_ui.query);
        let mut only_modified = self.settings_ui.only_modified;
        let rows = config::settings_rows(&self.cfg, &query, only_modified);
        // このフレームで確定した変更 (キー → 新しい値)
        let mut changed: Vec<(&'static str, config::SettingValue)> = Vec::new();
        let mut reset_all = false;
        let mut open_toml = false;

        egui::Window::new(tr("⚙ 設定"))
            .open(&mut open)
            .default_size([680.0, 520.0])
            .show(ctx, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(tr("検索")).size(11.5).color(theme.text_dim));
                    ui.add(
                        egui::TextEdit::singleline(&mut query)
                            .id_salt("zv-settings-query")
                            .hint_text(tr("設定名 / 説明で絞り込み (@modified も使えます)"))
                            .desired_width(220.0),
                    );
                    ui.checkbox(&mut only_modified, tr("変更したものだけ"));
                    if ui
                        .button(tr("config.toml"))
                        .on_hover_text(tr("GUI に無い設定はテキストで直接編集します"))
                        .clicked()
                    {
                        open_toml = true;
                    }
                    if ui
                        .button(tr("すべて既定へ"))
                        .on_hover_text(tr("この一覧の設定を全部、出荷時の値へ戻します"))
                        .clicked()
                    {
                        reset_all = true;
                    }
                });
                ui.separator();
                if rows.is_empty() {
                    // 空状態は利用可能領域の中央に 1 枚だけ (下に取り残さない)
                    let msg = if only_modified || query.contains("@modified") {
                        tr("既定から変えた設定はありません")
                    } else {
                        tr("一致する設定がありません")
                    };
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        |ui| {
                            ui.label(RichText::new(msg).color(theme.text_dim));
                        },
                    );
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("zv-settings-rows")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let avail = ui.available_width();
                        let cols = config::settings_columns(avail);
                        let row_h = ui.spacing().interact_size.y;
                        // 絞り込み中は一致度の順に並ぶので、グループ見出しは
                        // 出さない (同じ見出しが何度も割り込んで読みにくくなる)。
                        let show_groups = query
                            .split_whitespace()
                            .all(|t| t.eq_ignore_ascii_case("@modified"));
                        let mut group = "";
                        for d in &rows {
                            if show_groups && d.group != group {
                                group = d.group;
                                ui.add_space(4.0);
                                ui.label(
                                    RichText::new(tr(group))
                                        .size(11.0)
                                        .strong()
                                        .color(theme.accent),
                                );
                            }
                            let modified = config::is_setting_modified(&self.cfg, d.key);
                            let Some(cur) = config::setting_value(&self.cfg, d.key) else {
                                continue;
                            };
                            ui.horizontal(|ui| {
                                ui.set_max_width(cols.total_w().min(avail));
                                ui.spacing_mut().item_spacing.x = config::SETTINGS_COL_GAP;
                                // 変更マーカー (VS Code の左端の色バー)
                                settings_col(ui, cols.marker_w, row_h, |ui| {
                                    if modified {
                                        let r = ui.max_rect();
                                        ui.painter().rect_filled(r, 1.0, theme.accent);
                                    }
                                });
                                settings_col(ui, cols.label_w, row_h, |ui| {
                                    let doc = config::setting_doc(d.key);
                                    let hover = if doc.is_empty() {
                                        d.key.to_string()
                                    } else {
                                        format!("{}\n\n{}", d.key, tr(&doc))
                                    };
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(tr(d.label)).size(11.5).color(theme.text),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(hover);
                                });
                                settings_col(ui, cols.value_w, row_h, |ui| {
                                    if let Some(v) = self.setting_widget(ui, d, &cur) {
                                        changed.push((d.key, v));
                                    }
                                });
                                if cols.reset_w > 0.0 {
                                    settings_col(ui, cols.reset_w, row_h, |ui| {
                                        let label = if cols.icon_only {
                                            "↺".to_string()
                                        } else {
                                            tr("既定へ戻す")
                                        };
                                        let btn =
                                            egui::Button::new(RichText::new(label).size(11.0));
                                        if ui
                                            .add_enabled(modified, btn)
                                            .on_hover_text(tr("この設定だけを出荷時の値へ戻します"))
                                            .clicked()
                                        {
                                            if let Some(def) = config::setting_default(d.key) {
                                                changed.push((d.key, def));
                                            }
                                        }
                                    });
                                }
                            });
                            // 説明は 1 行だけ添える (畳んだ幅でもホバーで全文が出る)
                            let doc = config::setting_doc(d.key);
                            if !doc.is_empty() {
                                let one = doc.lines().next().unwrap_or_default().to_string();
                                ui.horizontal(|ui| {
                                    ui.set_max_width(avail);
                                    ui.add_space(cols.marker_w + config::SETTINGS_COL_GAP);
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(tr(&one))
                                                .size(10.5)
                                                .color(theme.text_dim),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(tr(&doc));
                                });
                            }
                        }
                    });
                // ── 状態ラダー 2 段目: ベンダー提供フックの設置 ──────────
                // ユーザーの設定ファイルを書き換えるので、同意はここでだけ取る
                // (押されたときにしか書かない・初回はバックアップを残す)。
                ui.separator();
                let root = self.primary_root().to_path_buf();
                supervisor::hooks::ui(ui, &root, &mut self.hooks_log);
            });

        self.settings_open = open;
        self.settings_ui.query = query;
        self.settings_ui.only_modified = only_modified;
        if reset_all {
            // **機能レジストリ由来の設定も一括リセットの対象にする。**
            // `setting_defs()` だけを回すと、`[features]` 側の設定が
            // 「全部を既定へ戻す」を押しても取り残される。
            for d in config::all_setting_defs() {
                if let Some(def) = config::setting_default(d.key) {
                    changed.push((d.key, def));
                }
            }
            self.settings_ui.drafts.clear();
        }
        if !changed.is_empty() {
            self.apply_settings(changed, ctx);
        }
        if open_toml {
            config::ensure_default();
            self.open_path(&config::config_path());
            self.settings_open = false;
        }
    }

    /// 設定 1 行の値ウィジェット。確定した新しい値だけを返す。
    pub(super) fn setting_widget(
        &mut self,
        ui: &mut egui::Ui,
        d: &config::SettingDef,
        cur: &config::SettingValue,
    ) -> Option<config::SettingValue> {
        use config::{SettingKind as K, SettingValue as V};
        let w = ui.available_width();
        match (d.kind, cur) {
            (K::Bool, V::Bool(b)) => {
                let mut on = *b;
                // Checkbox は自動採番の ID なので push_id は要らない
                if ui.checkbox(&mut on, "").changed() {
                    return Some(V::Bool(on));
                }
            }
            (K::Int { min, max }, V::Int(i)) => {
                let mut v = *i;
                if ui
                    .add_sized(
                        [w, ui.spacing().interact_size.y],
                        egui::DragValue::new(&mut v).range(min..=max),
                    )
                    .changed()
                {
                    return Some(V::Int(v.clamp(min, max)));
                }
            }
            (K::Float { min, max }, V::Float(f)) => {
                let mut v = *f;
                if ui
                    .add_sized(
                        [w, ui.spacing().interact_size.y],
                        egui::DragValue::new(&mut v).speed(0.1).range(min..=max),
                    )
                    .changed()
                {
                    return Some(V::Float(v.clamp(min, max)));
                }
            }
            (K::Choice(opts), V::Text(s)) => {
                let mut picked: Option<&str> = None;
                // 可変長リストの中の ComboBox は ID を明示する
                // (`make_persistent_id` を通るので自動採番されない)
                egui::ComboBox::from_id_salt(("zv-set-combo", d.key))
                    .selected_text(s.as_str())
                    .width(w)
                    .show_ui(ui, |ui| {
                        for o in opts {
                            if ui.selectable_label(s == o, *o).clicked() {
                                picked = Some(o);
                            }
                        }
                    });
                if let Some(o) = picked {
                    return Some(V::Text(o.to_string()));
                }
            }
            (K::Text, V::Text(s)) => {
                // 1 文字ごとに config.toml を書かないよう、確定するまで下書きに置く
                let draft = self
                    .settings_ui
                    .drafts
                    .entry(d.key.to_string())
                    .or_insert_with(|| s.clone());
                let r = ui.add(
                    egui::TextEdit::singleline(draft)
                        .id_salt(("zv-set-text", d.key))
                        .desired_width(w),
                );
                let typed = draft.clone();
                if r.lost_focus() {
                    // 欄を離れた (Enter / 別の欄へ移った) ところで確定する
                    self.settings_ui.drafts.remove(d.key);
                    if typed != *s {
                        return Some(V::Text(typed));
                    }
                } else if !r.has_focus() {
                    // 触っていない間は設定の値をそのまま映す。
                    // 下書きを持ち越すと、他の経路 (🎨 メニュー等) で
                    // 変わった値が画面に反映されない。
                    self.settings_ui.drafts.remove(d.key);
                }
            }
            _ => {}
        }
        None
    }

    /// 変更を `Config` へ入れ、config.toml へ書き戻し、画面へ反映する。
    pub(super) fn apply_settings(
        &mut self,
        changed: Vec<(&'static str, config::SettingValue)>,
        ctx: &egui::Context,
    ) {
        let mut write: std::collections::BTreeMap<String, String> = Default::default();
        let mut touched = false;
        for (key, v) in changed {
            if !config::set_setting_value(&mut self.cfg, key, &v) {
                continue;
            }
            write.insert(key.to_string(), v.to_toml());
            touched = true;
        }
        if !touched {
            return;
        }
        if let Err(e) = config::save_settings(&write) {
            self.toast_warn(e);
        }
        self.apply_config_to_ui(ctx);
    }

    /// いま通知音を鳴らす設定か。**真実源は `Config`** (`notify::sound()` の
    /// 旗は設定から一方通行で写した派生値なので、書き戻す側はこちらを読む)。
    pub(crate) fn notify_sound_enabled(&self) -> bool {
        self.cfg
            .feature_bool(crate::features::notifications::KEY_SOUND)
    }

    /// 通知音のオン/オフ。**設定画面 (⚙) と同じ書き戻し経路を通る** —
    /// `config.toml` への保存と `apply_runtime_flags` まで含む。
    /// ペットメニュー・パレット・設定画面のどこから変えても状態が 1 つに保たれる。
    pub(crate) fn set_notify_sound(&mut self, on: bool, ctx: &egui::Context) {
        self.apply_settings(
            vec![(
                crate::features::notifications::KEY_SOUND,
                config::SettingValue::Bool(on),
            )],
            ctx,
        );
        self.toast(
            if on {
                tr("🔔 通知音を有効にしました")
            } else {
                tr("🔕 通知音を無効にしました")
            },
            true,
        );
    }

    /// 設定値を「いま見えているもの」へ効かせる。
    /// 設定画面から変えた直後と、config.toml を読み直した直後に通る。
    pub(super) fn apply_config_to_ui(&mut self, ctx: &egui::Context) {
        self.theme = resolve_theme(&self.cfg.theme);
        theme::apply(ctx, &self.theme);
        apply_ui_zoom(ctx, self.cfg.ui_zoom);
        self.tree.show_hidden = self.cfg.show_hidden_files;
        self.tree.apply_config(&self.cfg);
        self.tree.invalidate();
        self.format_on_save = self.cfg.format_on_save;
        self.bracket_colorization = self.cfg.bracket_colorization;
        self.lsp_highlight_on = self.cfg.lsp_highlight_occurrences;
        self.rulers = normalize_rulers(&self.cfg.rulers);
        self.hotexit
            .set_max_bytes(self.cfg.hot_exit_max_kb.saturating_mul(1024));
        // 画面に出ている値が変わるので 1 フレームだけ描き直す
        crate::perf::repaint(ctx, "apply_config_to_ui");
    }

    /// キーバインド編集 UI (VS Code の ⌘K ⌘S 相当)。
    ///
    /// 読み取り専用の一覧ではなく **その場で再割り当てできる表**。
    /// 変更は `[keybindings]` 区画だけを書き換えて config.toml へ残すので、
    /// 手書きの設定とコメントは 1 行も消えない。
    pub(super) fn shortcuts_window(&mut self, ctx: &egui::Context) {
        if !self.shortcuts_open {
            return;
        }
        let theme = self.theme.clone();
        let mut open = self.shortcuts_open;
        let rows = keybind_rows(&self.keys, &self.keybind_ui.query);
        // 注記の列を出すかは表全体で 1 回だけ決める (行ごとに変えると列がぶれる)
        let notes: Vec<Option<String>> =
            rows.iter().map(|a| conflict_note(&self.keys, *a)).collect();
        let has_note = notes.iter().any(|n| n.is_some());
        let recording = self.keybind_ui.recording.as_ref().map(|r| r.action);
        let record_hint = self
            .keybind_ui
            .recording
            .as_ref()
            .and_then(|r| r.preview())
            .map(crate::keybinds::format_binding);
        let mut query = std::mem::take(&mut self.keybind_ui.query);
        let mut start_record: Option<BindAction> = None;
        let mut reset_one: Option<BindAction> = None;
        let mut reset_all = false;

        egui::Window::new(tr("⌨ キーボード ショートカット"))
            .open(&mut open)
            .default_size([620.0, 460.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(tr("検索")).size(11.5).color(theme.text_dim));
                    ui.add(
                        egui::TextEdit::singleline(&mut query)
                            .hint_text(tr("アクション名 / 打鍵で絞り込み"))
                            .desired_width(200.0),
                    );
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .button(tr("すべて既定へ戻す"))
                            .on_hover_text(tr("全アクションの割り当てを出荷時へ戻します"))
                            .clicked()
                        {
                            reset_all = true;
                        }
                    });
                });
                ui.label(
                    RichText::new(tr(
                        "行の「記録」を押すと次の打鍵を取り込みます。2 打鍵 (chord) は続けて押してください。中止は Esc。",
                    ))
                    .size(11.0)
                    .color(theme.text_dim),
                );
                ui.separator();
                if rows.is_empty() {
                    // 空状態は利用可能領域の中央に 1 枚だけ出す
                    ui.allocate_ui_with_layout(
                        ui.available_size(),
                        egui::Layout::centered_and_justified(egui::Direction::TopDown),
                        |ui| {
                            ui.label(
                                RichText::new(tr("一致するアクションがありません"))
                                    .color(theme.text_dim),
                            );
                        },
                    );
                    return;
                }
                egui::ScrollArea::vertical()
                    .id_salt("zv-keybind-rows")
                    .auto_shrink(false)
                    .show(ui, |ui| {
                        let avail = ui.available_width();
                        let cols = crate::keybinds::keybind_columns(avail, has_note);
                        let row_h = ui.spacing().interact_size.y;
                        for (a, note) in rows.iter().zip(notes.iter()) {
                            let a = *a;
                            let is_rec = recording == Some(a);
                            ui.horizontal(|ui| {
                                // 行の実幅は列の合計で決める (可用幅を超えない)
                                ui.set_max_width(cols.total_w().min(avail));
                                ui.spacing_mut().item_spacing.x = crate::keybinds::KEYBIND_COL_GAP;
                                let label = tr(crate::keybinds::action_label(a));
                                keybind_col(ui, cols.label_w, row_h, |ui| {
                                    ui.add(
                                        egui::Label::new(RichText::new(&label).size(12.0))
                                            .truncate(),
                                    )
                                    .on_hover_text(format!(
                                        "{label}  ({})",
                                        crate::keybinds::config_name(a)
                                    ));
                                });
                                let keys_txt = if is_rec {
                                    record_hint
                                        .clone()
                                        .map(|k| format!("{k} …"))
                                        .unwrap_or_else(|| tr("打鍵を待っています…"))
                                } else {
                                    self.keys.label(a)
                                };
                                let keys_col = if is_rec { theme.warn } else { theme.text };
                                keybind_col(ui, cols.keys_w, row_h, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&keys_txt)
                                                .monospace()
                                                .size(12.0)
                                                .color(keys_col),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(&keys_txt);
                                });
                                if cols.note_w > 0.0 {
                                    let txt = note.clone().unwrap_or_default();
                                    keybind_col(ui, cols.note_w, row_h, |ui| {
                                        if !txt.is_empty() {
                                            ui.add(
                                                egui::Label::new(
                                                    RichText::new(&txt)
                                                        .size(11.0)
                                                        .color(theme.warn),
                                                )
                                                .truncate(),
                                            )
                                            .on_hover_text(&txt);
                                        }
                                    });
                                }
                                if cols.buttons_w > 0.0 {
                                    keybind_col(ui, cols.buttons_w, row_h, |ui| {
                                        let rec_label = if cols.icon_only {
                                            "⌨".to_string()
                                        } else {
                                            tr("記録")
                                        };
                                        if ui
                                            .button(RichText::new(rec_label).size(11.5))
                                            .on_hover_text(tr("次に押した打鍵を割り当てます"))
                                            .clicked()
                                        {
                                            start_record = Some(a);
                                        }
                                        let is_def = self.keys.is_default(a);
                                        let def_label = if cols.icon_only {
                                            "↺".to_string()
                                        } else {
                                            tr("既定")
                                        };
                                        if ui
                                            .add_enabled(
                                                !is_def,
                                                egui::Button::new(
                                                    RichText::new(def_label).size(11.5),
                                                ),
                                            )
                                            .on_hover_text(tr("この行を既定へ戻します"))
                                            .clicked()
                                        {
                                            reset_one = Some(a);
                                        }
                                    });
                                }
                            });
                            // 注記の列を畳んだ幅でも、警告そのものは失わない
                            if cols.note_w <= 0.0 {
                                if let Some(txt) = note {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(txt).size(10.5).color(theme.warn),
                                        )
                                        .truncate(),
                                    )
                                    .on_hover_text(txt);
                                }
                            }
                        }
                        ui.add_space(6.0);
                        ui.label(
                            RichText::new(tr("以下は入力欄に組み込みで、変更できません"))
                                .size(11.0)
                                .color(theme.text_dim),
                        );
                        for (label, keys) in builtin_shortcuts() {
                            ui.horizontal(|ui| {
                                ui.set_max_width(cols.total_w().min(avail));
                                keybind_col(ui, cols.label_w, row_h, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&label)
                                                .size(11.5)
                                                .color(theme.text_dim),
                                        )
                                        .truncate(),
                                    );
                                });
                                keybind_col(ui, cols.keys_w, row_h, |ui| {
                                    ui.add(
                                        egui::Label::new(
                                            RichText::new(&keys)
                                                .monospace()
                                                .size(11.5)
                                                .color(theme.text_dim),
                                        )
                                        .truncate(),
                                    );
                                });
                            });
                        }
                    });
            });
        self.shortcuts_open = open;
        self.keybind_ui.query = query;
        if let Some(a) = start_record {
            self.keybind_ui.recording = Some(crate::keybinds::Recorder::new(a));
            crate::perf::repaint(ctx, "shortcuts_window");
        }
        if reset_all {
            self.keys.reset_all();
            self.persist_keybindings();
        } else if let Some(a) = reset_one {
            self.keys.reset(a);
            self.persist_keybindings();
        }
    }

    /// 記録モードの打鍵を取り込む。記録中なら `true` (呼び出し側は即戻る)。
    ///
    /// **`handle_shortcuts` の先頭から呼ぶこと。** 通常の消費やエディタより
    /// 先に打鍵を取らないと、記録しようとした ⌘S でファイルが保存される。
    /// 中止は Esc、1 打鍵目から [`crate::keybinds::CHORD_TIMEOUT`] だけ
    /// 2 打鍵目を待ち、来なければ単打として確定する。
    pub(super) fn keybind_record_tick(&mut self, ctx: &egui::Context) -> bool {
        let Some(mut rec) = self.keybind_ui.recording.take() else {
            return false;
        };
        let now = ctx.input(|i| i.time);
        let esc = ctx.input_mut(|i| {
            crate::keybinds::consume_shortcut_compat(
                i,
                egui::KeyboardShortcut::new(egui::Modifiers::NONE, egui::Key::Escape),
            )
        });
        if esc {
            // 中止。記録は捨てて通常の消費へ戻す (このフレームは休む)
            crate::perf::repaint(ctx, "keybind_record_tick");
            return true;
        }
        let stroke = ctx.input_mut(crate::keybinds::record_stroke);
        let done = match stroke {
            Some(sc) => rec.push(sc, now),
            None => rec.tick(now),
        };
        match done {
            Some(b) => {
                let a = rec.action;
                self.keys.set(a, b);
                self.persist_keybindings();
                crate::perf::repaint(ctx, "keybind_record_tick");
            }
            None => {
                // 2 打鍵目の締切まで画面を回す (待っている間だけ)
                let left = rec.remaining(now).min(0.1);
                crate::perf::repaint_after(
                    ctx,
                    std::time::Duration::from_secs_f64(left),
                    "keybind_record_tick",
                );
                self.keybind_ui.recording = Some(rec);
            }
        }
        true
    }

    /// キーバインドの変更を config.toml の `[keybindings]` 区画へ書き戻す。
    /// **既定と同じ行は書かない** ので、既定を変えたときに古い値へ固定されない。
    pub(super) fn persist_keybindings(&mut self) {
        // **機能側の再割り当ても一緒に保存する。** `self.keys.overrides()` だけを
        // 書くと、保存のたびに機能の打鍵設定が消える。
        let ov = crate::keybinds::merged_overrides(&self.keys, &self.feature_keys);
        self.cfg.keybindings = ov.clone();
        if let Err(e) = config::save_keybindings(&ov) {
            self.toast(e, false);
        }
    }

    /// **What's New** を開く (ヘルプメニュー / パレットから)。
    ///
    /// 手動で開いたときは版に関係なく**最新の 1 件**を出す。
    /// 何も無い (変更履歴が読めない) ときは黙って何もしない —
    /// 空のウィンドウを出す方がよほど分かりにくい。
    pub(super) fn open_whats_new(&mut self) {
        let all = crate::whats_new::releases();
        self.whats_new = all.into_iter().take(1).collect();
        if self.whats_new.is_empty() {
            self.toast(tr("変更履歴が読み込めませんでした"), false);
        }
    }

    /// **更新後の初回起動で 1 度だけ開く。**
    ///
    /// 初回インストールでは出さない (`unseen` が空を返す)。開いた時点で
    /// 「見た版」を今の版へ進め、state.toml へ書く — 出しっぱなしにすると
    /// 起動のたびに同じものが出る。
    pub(super) fn whats_new_on_start(&mut self) {
        let cur = crate::whats_new::current_version();
        let shown = crate::whats_new::unseen(
            &crate::whats_new::releases(),
            &self.cfg.last_seen_version,
            cur,
        );
        // 見た印は「出す物が無かった」場合も含めて必ず進める
        // (次の更新まで毎回同じ計算をしないため)。
        if self.cfg.last_seen_version != cur {
            self.cfg.last_seen_version = cur.to_string();
            config::save_state(&self.cfg);
        }
        self.whats_new = shown;
    }

    /// **What's New** のウィンドウ。中身が空なら 1 ピクセルも描かない。
    pub(super) fn whats_new_window(&mut self, ctx: &egui::Context) {
        if self.whats_new.is_empty() {
            return;
        }
        let theme = self.theme.clone();
        let mut open = true;
        // 閉じるボタンは別の旗で受ける。`Window::open` が `&mut open` を
        // 借りたままなので、クロージャの中から同じ変数を触れない。
        let mut close = false;
        egui::Window::new(tr("この版の新機能"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                // 幅を決め打ちすると狭い窓で見切れる。可用幅に収める。
                let w = ui.available_width().min(560.0).max(280.0);
                ui.set_width(w);
                egui::ScrollArea::vertical()
                    .id_salt("whats_new_body")
                    .max_height(420.0)
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        for r in &self.whats_new {
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new(format!("v{}", r.version))
                                        .size(16.0)
                                        .strong()
                                        .color(theme.accent),
                                );
                                if !r.date.is_empty() {
                                    ui.label(
                                        RichText::new(r.date.clone())
                                            .size(11.5)
                                            .color(theme.text_dim),
                                    );
                                }
                            });
                            ui.add_space(4.0);
                            for item in &r.items {
                                ui.horizontal_top(|ui| {
                                    ui.label(RichText::new("・").color(theme.text_dim));
                                    // 長い項目は折り返す (右端で見切れない)。
                                    ui.add(egui::Label::new(RichText::new(item).size(12.5)).wrap());
                                });
                            }
                            ui.add_space(8.0);
                        }
                    });
                ui.separator();
                ui.vertical_centered(|ui| {
                    if ui.button(tr("閉じる")).clicked() {
                        close = true;
                    }
                });
            });
        if !open || close {
            self.whats_new.clear();
        }
    }

    pub(super) fn about_window(&mut self, ctx: &egui::Context) {
        if !self.about_open {
            return;
        }
        let theme = self.theme.clone();
        let mut open = self.about_open;
        egui::Window::new(tr("Zaivern Code について"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("⚡").size(42.0).color(theme.accent));
                    ui.label(RichText::new("Zaivern Code").size(20.0).strong());
                    ui.label(
                        RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                            .color(theme.text_dim),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        RichText::new(tr(
                            "Rust製 AI-Native エディタ — Zed の速度 × Cmux の並列エージェント × AGI Cockpit の操縦席",
                        ))
                        .size(12.0)
                        .color(theme.text_dim),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        RichText::new(format!("egui 0.29 / rustc {}", rustc_version()))
                            .size(11.0)
                            .color(theme.text_dim),
                    );
                });
            });
        self.about_open = open;
    }

    /// ライセンス (Pro) の状態表示とキーの適用。**通信は一切しない**。
    ///
    /// - キー全体は画面に出さない ([`license::mask_key`] で伏せる)
    /// - オフライン検証なので**失効はできない**ことを画面に明記する
    /// - 未ライセンスでも全機能が使えることを正直に書く (機能は奪っていない)
    pub(super) fn license_window(&mut self, ctx: &egui::Context) {
        if !self.license_open {
            return;
        }
        let theme = self.theme.clone();
        let mut open = self.license_open;
        let mut apply = false;
        let mut remove = false;
        let status = self.license_status.clone();
        let saved = self.license_key.clone();
        let configured = license::pubkey_configured(&license::EMBEDDED_PUBKEY);

        egui::Window::new(tr("🔑 ライセンス"))
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .default_width(430.0)
            .anchor(Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                // 幅は必ず可用領域に収める (狭い画面でも行が見切れない)
                let w = ui.available_width().min(430.0);
                ui.set_max_width(w);

                // ── 現在の状態 ──────────────────────────────────
                let (icon, head, color) = license_status_head(&status, &theme);
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(icon).size(16.0).color(color));
                    ui.label(RichText::new(head).size(14.0).strong().color(color));
                });

                match &status {
                    license::LicenseStatus::Valid {
                        tier,
                        sub,
                        exp,
                        seats,
                    } => {
                        let exp_text = match exp {
                            Some(e) => license::format_unix_date(*e),
                            None => tr("無期限"),
                        };
                        ui.label(
                            RichText::new(trf(
                                "等級 {tier} ・ 購入者 {sub} ・ 期限 {exp} ・ {seats} 席",
                                &[
                                    ("tier", tier.clone()),
                                    ("sub", sub.clone()),
                                    ("exp", exp_text),
                                    ("seats", seats.to_string()),
                                ],
                            ))
                            .size(11.5)
                            .color(theme.text_dim),
                        );
                    }
                    license::LicenseStatus::Expired { exp } => {
                        ui.label(
                            RichText::new(trf(
                                "{exp} に期限が切れています。新しいキーを貼り付けてください",
                                &[("exp", license::format_unix_date(*exp))],
                            ))
                            .size(11.5)
                            .color(theme.text_dim),
                        );
                    }
                    license::LicenseStatus::Malformed(why) => {
                        ui.label(RichText::new(tr(why)).size(11.5).color(theme.text_dim));
                    }
                    license::LicenseStatus::BadSignature => {
                        ui.label(
                            RichText::new(tr(
                                "署名が一致しません。写し間違いか、別の製品のキーの可能性があります",
                            ))
                            .size(11.5)
                            .color(theme.text_dim),
                        );
                    }
                    license::LicenseStatus::Unlicensed => {
                        ui.label(
                            RichText::new(tr(
                                "Zaivern Code は無料で全機能使えます。Pro ライセンスは開発支援者向けです",
                            ))
                            .size(11.5)
                            .color(theme.text_dim),
                        );
                    }
                }

                if let Some(k) = &saved {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(trf(
                            "保存済みのキー: {k}",
                            &[("k", license::mask_key(k))],
                        ))
                        .size(11.0)
                        .color(theme.text_dim),
                    );
                }

                if !configured {
                    ui.add_space(2.0);
                    ui.label(
                        RichText::new(tr(
                            "⚠ この配布版には検証用の公開鍵が入っていないため、どのキーも有効になりません",
                        ))
                        .size(11.0)
                        .color(theme.warn),
                    );
                }

                ui.add_space(6.0);
                ui.separator();
                ui.add_space(4.0);

                // ── キーの貼り付け ──────────────────────────────
                ui.label(RichText::new(tr("ライセンスキーを貼り付け:")).size(12.0));
                let te = ui.add(
                    egui::TextEdit::multiline(&mut self.license_input)
                        .desired_rows(3)
                        .desired_width(w)
                        .hint_text("ZVL1.…")
                        .font(egui::TextStyle::Monospace),
                );
                // 改行はキーの一部にならないので、Enter は「適用」と解釈する
                if te.has_focus() && ctx.input(|i| i.key_pressed(egui::Key::Enter)) {
                    apply = true;
                }

                ui.add_space(4.0);
                ui.horizontal_wrapped(|ui| {
                    let can_apply = !self.license_input.trim().is_empty();
                    if ui
                        .add_enabled(can_apply, egui::Button::new(tr("適用")))
                        .clicked()
                    {
                        apply = true;
                    }
                    if saved.is_some() && ui.button(tr("保存済みのキーを削除")).clicked() {
                        remove = true;
                    }
                });

                ui.add_space(6.0);
                ui.label(
                    RichText::new(trf(
                        "検証は完全にこの端末の中だけで行われます (通信ゼロ)。キーは {p} に保存されます",
                        &[("p", license::license_path().display().to_string())],
                    ))
                    .size(10.5)
                    .color(theme.text_dim),
                );
                ui.label(
                    RichText::new(tr(
                        "オフライン検証のため、発行済みキーの失効 (revoke) はできません。期限付きキーで運用しています",
                    ))
                    .size(10.5)
                    .color(theme.text_dim),
                );
            });

        self.license_open = open;

        if apply {
            let input = self.license_input.clone();
            match license::apply_key(&input) {
                Ok((k, st)) => {
                    self.license_key = k;
                    self.license_status = st;
                    self.license_input.clear();
                    let ok = license::is_pro(&self.license_status);
                    let (_, head, _) = license_status_head(&self.license_status, &theme);
                    self.toast(head, ok);
                }
                Err(e) => self.toast(trf("ライセンスの保存に失敗: {e}", &[("e", e)]), false),
            }
        } else if remove {
            match license::remove_key() {
                Ok(()) => {
                    self.license_key = None;
                    self.license_status = license::LicenseStatus::Unlicensed;
                    self.toast(tr("🔑 ライセンスキーを削除しました"), true);
                }
                Err(e) => self.toast(trf("ライセンスの削除に失敗: {e}", &[("e", e)]), false),
            }
        }
    }
}
