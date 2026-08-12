use super::*;

impl ZaivernApp {
    pub(super) fn code_editor_ui(&mut self, ui: &mut egui::Ui) {
        let Some(active) = self.editor.active else {
            return;
        };
        // 専用ビューアのタブ (画像 / 16 進 / メディア / 書庫) が紛れ込んでも
        // TextEdit を出さない (二重の防御。通常はタブ描画の分岐で先に振られる)
        if self.preview_view_ui(ui, active) {
            return;
        }
        // ⌘+ホイール / ピンチを「このファイルだけ」へ振り分けるための矩形。
        // 次のフレームの `handle_zoom_gestures` が読む。
        self.zoom_area_next = Some((ui.max_rect(), ZoomArea::File));
        let theme_text = self.theme.text;
        let theme_dim = self.theme.text_dim;
        let syntect_theme = self.theme.syntect_theme.clone();
        // ジャンプモードのグルー。**`self` が可変借用される前に**必要な値を写す。
        let jump_tab_w = self.cfg.tab_size;
        let mut jump_to: Option<crate::jump::Pos> = None;
        // 端末と同じ理由で行高を物理ピクセルの整数へ揃える (theme::snap_len)。
        // 行高が小数だと N 行目の y が丸められる位置が行ごとに変わり、
        // ガター番号・本文・スティッキーヘッダの縦位置が 1px ずつずれる。
        let ppp = ui.ctx().pixels_per_point();
        // 画面全体のズームは pixels_per_point 側に乗っているので、ここで掛けるのは
        // このタブだけの倍率 (`editor_font_pt`)。両方掛けると倍率が二乗になる。
        let font = FontId::monospace(crate::theme::snap_font_size(self.editor_font_pt(), ppp));
        let row_h = ui
            .fonts(|f| crate::theme::snap_len(f.row_height(&font), ppp))
            .max(1.0 / ppp);
        self.last_row_h = row_h;
        // ミニマップ: 帯を出すかは**純関数** minimap_visible が決める
        // (狭い画面では設定が ON でも自動的に隠れる)。
        let mm_on = crate::minimap::minimap_visible(ui.available_width(), self.cfg.minimap);
        let mm_w = if mm_on {
            crate::minimap::strip_width(ppp)
        } else {
            0.0
        };
        // galley をフレーム跨ぎでキャッシュするためのフォント世代キー。
        // egui は pixels_per_point 変更時とフォントアトラス逼迫時(fill_ratio > 0.8)に
        // FontsImpl ごと作り直し、そのとき全グリフの UV が変わる。古い galley を
        // 使い回すと描画が壊れるため、作り直しを検知できる値をキーに混ぜておく。
        // (アトラスは作り直しで初期サイズに戻るのでサイズ変化で検知できる)
        let font_gen = {
            let sz = ui.fonts(|f| f.font_image_size());
            (sz[0] as u64).rotate_left(23)
                ^ (sz[1] as u64).rotate_left(47)
                ^ (ui.ctx().pixels_per_point().to_bits() as u64).rotate_left(41)
        };
        let view_h = self.last_view_h;
        // ガターは本文と別背景にして「打ち込める範囲」との境界を見せる
        let theme_panel = self.theme.panel;
        let theme_border = self.theme.border;
        let theme_accent = self.theme.accent;
        // ブックマークのニーモニックはアクセント色の四角の上に載せるので、
        // 文字色は本文背景 (= アクセントの反対側) を使う。
        let theme_bg = self.theme.bg;
        // 現在行ハイライト用 (テキストの上に重ねるのでごく薄く)
        let cur_line_hl = self.theme.text.gamma_multiply(0.07);
        // 折り返しと空白可視化 (コマンド/メニューで切替、config に永続化)
        let word_wrap = self.cfg.word_wrap;
        let show_ws = self.cfg.show_whitespace;
        // 空白グリフの色: テーマの薄文字をさらに落とす (テーマ準拠・固定色なし)
        let ws_color = theme_dim.gamma_multiply(0.55);

        let mut pending_select = self.pending_select.take();
        let pending_scroll = self.pending_scroll.take();

        // Git 行マーク(バッファの可変借用前に取得)
        let theme_ok = self.theme.ok;
        let theme_warn = self.theme.warn;
        let theme_err = self.theme.err;
        self.gitinfo.refresh_if_stale();
        let abs = self.editor.buffers[active].path.clone();
        let text_hash = hash_str(&self.editor.buffers[active].text);
        // find バーもこのハッシュを使い回す (再計算しない)
        self.last_text_hash = text_hash;
        // バッファ内検索のヒットは**ここで 1 回だけ**走査する。
        // 検索バーの「3 / 27」・ミニマップの印・本文のハイライトが同じ結果を見るので、
        // 同じ本文を 3 回走査することはない (本文か検索条件が変わったときだけ走る)。
        let find_on = self.find.open && !self.find.query.is_empty();
        if find_on {
            self.refresh_find_hits(active, text_hash);
        }
        // Arc 共有: キャッシュヒット時は参照カウント増加のみで Vec は複製されない
        let find_hits: std::sync::Arc<Vec<find_buffer::BufHit>> = match (find_on, &self.find_hits) {
            (true, Some(c)) => c.hits.clone(),
            _ => std::sync::Arc::new(Vec::new()),
        };
        let mm_search: std::sync::Arc<Vec<usize>> = match (find_on && mm_on, &self.find_hits) {
            (true, Some(c)) => c.mm_lines.clone(),
            _ => std::sync::Arc::new(Vec::new()),
        };
        // ハイライトの色はテーマから作る (直書きしない)
        let find_hit_bg = find_buffer::hit_bg(&self.theme);
        let find_cur_bg = find_buffer::current_hit_bg(&self.theme);
        let find_current = self.find.current;
        // 本文 galley のキャッシュ鍵に混ぜる検索条件。本文そのものは鍵に入っている
        // ので、ヒットの中身は (検索語, トグル, 現在位置) から一意に決まる。
        let find_key: u64 = {
            let opts = self.find.opts;
            let flags = (opts.case_sensitive as u64)
                | ((opts.whole_word as u64) << 1)
                | ((opts.regex as u64) << 2)
                | ((find_on as u64) << 3);
            let cur = find_current.map_or(u64::MAX, |(s, e)| (s as u64).rotate_left(17) ^ e as u64);
            combine_hash(combine_hash(hash_str(&self.find.query), flags), cur)
        };
        // Arc 共有: キャッシュヒット時は参照カウント増加のみで Vec は複製されない
        let marks = match &abs {
            Some(p) => self.gitinfo.line_marks(p, text_hash),
            None => git::empty_line_marks(),
        };

        // LSP: この言語のサーバーを必要なら起動し did_open、診断を取得
        let path_clone = self.editor.buffers[active].path.clone();
        let lang_clone = self.editor.buffers[active].lang.clone();
        if let Some(p) = path_clone.clone() {
            let ctx = ui.ctx().clone();
            self.ensure_lsp(&ctx, &p, &lang_clone, active);
        }
        self.refresh_active_diagnostics(text_hash);
        self.refresh_inlay_hints(text_hash);
        // ブックマークの行追従。**毎フレームやるのはハッシュ比較だけ**で、
        // 実際の差分は 100ms のデバウンス後にバックグラウンドスレッドが取る
        // (編集経路で本文を走査すると、このリポジトリが git で踏んだ
        //  「UI スレッドが数秒返ってこない」を再演することになる)。
        if let Some(p) = abs.clone() {
            let mut m = std::mem::take(&mut self.marks);
            m.tick(ui.ctx(), &p, text_hash, &self.editor.buffers[active].text);
            self.marks = m;
        }
        self.diag_counts = (self.diag_cache.errors, self.diag_cache.warnings);
        // diag_cache の借用は、可変借用が要る第 2 次配線の準備が済んでから取る
        // (この束縛を上げると self を可変に触れなくなる)。

        // スニペット Tab 展開: エディタにフォーカスがあり、選択が空で、
        // カーソル直前の単語が prefix に一致するときだけ Tab を横取りする
        // (一致しなければ Tab はそのまま TextEdit のタブ挿入に流す)。
        let ed_id_early = buf_edit_id(self.cur_pane, self.editor.buffers[active].id);
        let has_focus = ui.memory(|m| m.has_focus(ed_id_early));

        // Ctrl+A 全選択: egui 標準の TextEdit は mac では Cmd+A のみ対応で、
        // Ctrl+A は本文に届かず何も起きない (ブロードキャスト等の入力欄は
        // 個別対応済みなので、エディタだけ効かない不一致になっていた)。
        // 選択するのは TextEdit の中身 = 実際に文字を打ち込める本文だけ。
        // 行番号ガターは別描画なので選択には含まれない。
        if has_focus && ui.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, egui::Key::A)) {
            let len = self.editor.buffers[active].text.chars().count();
            pending_select = Some((0, len));
        }
        // 折りたたみ中は TextEdit に「表示テキスト」を編集させるので、その
        // キャレット添字は原文の添字と一致しない。**原文の添字を前提にする
        // 補助機能** (スニペット展開・自動ペア) はこのフレームだけ止める。
        // 打ち込み自体と、⌃A / 検索ジャンプのような `pending_select` 経由の
        // 移動は写像しているので影響しない。
        let folds_closed = !self.editor.buffers[active].folds.folded().is_empty();
        let expand = if has_focus && !folds_closed {
            let lang_id = snippets::lang_id_for(&lang_clone);
            match self.snippets_by_lang.get(lang_id) {
                Some(snips) if !snips.is_empty() => {
                    let cursor = egui::TextEdit::load_state(ui.ctx(), ed_id_early)
                        .and_then(|st| st.cursor.char_range())
                        .filter(|r| r.primary.index == r.secondary.index)
                        .map(|r| r.primary.index);
                    match cursor {
                        Some(cursor_char) => {
                            let filename = path_clone
                                .as_ref()
                                .and_then(|p| p.file_name())
                                .map(|n| n.to_string_lossy().to_string())
                                .unwrap_or_default();
                            // 全文 clone はしない: カーソル直前の単語だけを窓として
                            // try_expand_at へ渡し、展開時に前後を張り合わせる。
                            // prefix 単語は ASCII 英数字と _ のみなので、バイト後退で
                            // 安全に単語頭を求められる (UTF-8 継続バイトは非 ASCII)。
                            let text = &self.editor.buffers[active].text;
                            // char index → byte index (try_expand_at 同様、末尾へクランプ)
                            let mut cursor_chars = 0usize;
                            let mut cursor_byte = text.len();
                            for (b, _) in text.char_indices() {
                                if cursor_chars == cursor_char {
                                    cursor_byte = b;
                                    break;
                                }
                                cursor_chars += 1;
                            }
                            let cursor_chars = cursor_chars.min(cursor_char);
                            let bytes = text.as_bytes();
                            let mut word_start = cursor_byte;
                            while word_start > 0 {
                                let c = bytes[word_start - 1];
                                if c.is_ascii_alphanumeric() || c == b'_' {
                                    word_start -= 1;
                                } else {
                                    break;
                                }
                            }
                            // ASCII のみの単語なのでバイト数 = char 数
                            let word_len = cursor_byte - word_start;
                            if word_len == 0 {
                                // 直前が単語でなければ従来どおり展開なし
                                None
                            } else {
                                snippets::try_expand_at(
                                    &text[word_start..cursor_byte],
                                    word_len,
                                    snips,
                                    &filename,
                                )
                                .map(|(ins, rel)| {
                                    let mut nt =
                                        String::with_capacity(text.len() - word_len + ins.len());
                                    nt.push_str(&text[..word_start]);
                                    nt.push_str(&ins);
                                    nt.push_str(&text[cursor_byte..]);
                                    // 窓は単語そのものなので窓内カーソル rel を
                                    // 単語頭の絶対 char 位置に足せば従来と一致する
                                    (nt, cursor_chars - word_len + rel)
                                })
                            }
                        }
                        None => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        if let Some((nt, ncur)) = expand {
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Tab)) {
                // スニペット展開は 1 段 (⌘Z 1 回で打った略語へ戻る)
                let ed = self.edit_step().to_sel((ncur, ncur));
                self.editor.buffers[active].apply_edit(nt, ed);
                pending_select = Some((ncur, ncur));
            }
        }

        // ── 複数キャレット: 打鍵を全キャレットへ配る ──────────────────
        //
        // `TextEdit` を描く**前**にイベントを抜き取るのが要。egui は主キャレット
        // 1 本にしか打鍵を適用しないので、ここで横取りしないと ⌘D で 5 箇所
        // 選んでも文字は 1 箇所にしか入らない。
        // 本文の差し替えは 1 回だけ = 取り消しも 1 段 (`MultiPaste` と同じ約束)。
        //
        // ここで消費した打鍵は下の自動ペア処理にも届かない (イベントごと抜くため)
        // ので、複数キャレット中は自動ペアが二重に走ることはない。
        let buf_id_active = self.editor.buffers[active].id;
        // クリック**前**のキャレット。Alt+クリックの 1 本目の種にする
        // (クリック後は egui が主キャレットを動かしてしまい取れない)。
        let prev_caret: Option<(usize, usize)> = egui::TextEdit::load_state(ui.ctx(), ed_id_early)
            .and_then(|st| st.cursor.char_range())
            .map(|r| {
                let (a, b) = (r.primary.index, r.secondary.index);
                (a.min(b), a.max(b))
            });
        let multi_live =
            matches!(&self.multi_sel, Some((id, s)) if *id == buf_id_active && s.len() > 1);
        if multi_live && has_focus {
            // Escape で解除。エディタにフォーカスがあるときだけ奪うので、
            // パレット / 検索 / 全画面解除 (どれもフォーカス無しが条件) とは
            // 取り合いにならない。
            if ui.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape)) {
                self.multi_sel = None;
                self.multi_sticky_col = None;
                self.column_anchor = None;
            } else if !folds_closed && !self.editor.buffers[active].read_only() {
                let ops = ui.input_mut(|i| take_multi_keys(&mut i.events));
                if !ops.is_empty() {
                    let sel = match &self.multi_sel {
                        Some((_, s)) => s.clone(),
                        None => editor_ops::MultiSel::default(),
                    };
                    let (new_text, next) =
                        apply_multi_keys(&self.editor.buffers[active].text, &sel, &ops);
                    let (cs, ce) = byte_range_to_char_range(&new_text, &next.to_single_selection());
                    // 打鍵は連続して来るので `typed` で積む — 続けて打った文字は
                    // 1 段に併合され、⌘Z 一回で「打った塊」が戻る。プログラム的
                    // 編集にすると 1 文字 = 1 段になり、単キャレットと粒度がずれる。
                    // 取り消し後は編集前の主キャレット位置へ戻す。
                    let ed = self
                        .edit_typed()
                        .with_sel_before(prev_caret.unwrap_or((cs, ce)))
                        .to_sel((cs, ce));
                    // 本文の書き込みはこの 1 か所だけ (取り消しが 1 段で戻る条件)
                    self.editor.buffers[active].apply_edit(new_text, ed);
                    self.fold_view = None;
                    self.queue_lsp_change(active);
                    pending_select = Some((cs, ce));
                    self.multi_sel = Some((buf_id_active, next));
                }
            }
        }

        // 括弧・引用符の自動ペア (VS Code の autoClosingBrackets 相当):
        // 開き括弧で自動閉じ/選択囲み、閉じ括弧でスキップ、
        // 空ペアの間での Backspace は両方まとめて削除。
        // IME 変換中の文字は Event::Ime で届くため、ここには掛からない。
        if has_focus && !folds_closed && !self.editor.buffers[active].read_only() {
            let sel = egui::TextEdit::load_state(ui.ctx(), ed_id_early)
                .and_then(|st| st.cursor.char_range())
                .map(|r| {
                    let (a, b) = (r.primary.index, r.secondary.index);
                    (a.min(b), a.max(b))
                });
            if let Some((sel_min, sel_max)) = sel {
                let typed: Option<char> = ui.input(|i| {
                    i.events.iter().find_map(|e| match e {
                        egui::Event::Text(t) => {
                            let mut cs = t.chars();
                            match (cs.next(), cs.next()) {
                                (Some(c), None) if "([{\"'`)]}".contains(c) => Some(c),
                                _ => None,
                            }
                        }
                        _ => None,
                    })
                });
                let edit = typed.and_then(|c| {
                    editor_ops::pair_on_type(&self.editor.buffers[active].text, sel_min, sel_max, c)
                });
                if let Some(edit) = edit {
                    // 元の 1 文字 Text イベントは消費して TextEdit へ渡さない
                    let c = typed.unwrap();
                    ui.input_mut(|i| {
                        let mut removed = false;
                        i.events.retain(|e| {
                            if removed {
                                return true;
                            }
                            match e {
                                egui::Event::Text(t) if t.chars().eq(std::iter::once(c)) => {
                                    removed = true;
                                    false
                                }
                                _ => true,
                            }
                        });
                    });
                    match edit {
                        editor_ops::PairEdit::Insert { text: nt, cursor } => {
                            // 自動で足した閉じ括弧は打鍵と同じ段に混ぜる
                            let ed = self.edit_typed();
                            self.editor.buffers[active].apply_edit(nt, ed);
                            pending_select = Some((cursor, cursor));
                        }
                        editor_ops::PairEdit::Surround { text: nt, select } => {
                            // 選択を括弧で囲むのはプログラム的編集 (1 段)
                            let ed = self
                                .edit_step()
                                .with_sel_before((sel_min, sel_max))
                                .to_sel(select);
                            self.editor.buffers[active].apply_edit(nt, ed);
                            pending_select = Some(select);
                        }
                        editor_ops::PairEdit::SkipOver { cursor } => {
                            pending_select = Some((cursor, cursor));
                        }
                    }
                } else if sel_min == sel_max && ui.input(|i| i.key_pressed(egui::Key::Backspace)) {
                    if let Some((nt, cur)) =
                        editor_ops::pair_on_backspace(&self.editor.buffers[active].text, sel_min)
                    {
                        if ui.input_mut(|i| {
                            i.consume_key(egui::Modifiers::NONE, egui::Key::Backspace)
                        }) {
                            let ed = self.edit_typed();
                            self.editor.buffers[active].apply_edit(nt, ed);
                            pending_select = Some((cur, cur));
                        }
                    }
                }
            }
        }

        // ── 第 2 次配線: 表 / 巨大ファイル / 折りたたみ / ガイド ────────
        // 表形式ファイルはグリッドで描いて終わり (本文 TextEdit は出さない)
        if self.editor.buffers[active].table.is_some() {
            self.table_view_ui(ui, active);
            return;
        }
        // 巨大ファイルの帯 (読み取り専用 / 強調表示オフの理由を必ず見せる)
        let banner_bytes = self.editor.buffers[active].large_file_banner();
        if let Some(bytes) = banner_bytes {
            self.large_file_banner_ui(ui, bytes);
        }
        // 構造解析 (折りたたみ / ガイド / スティッキー) は本文全走査なので、
        // 強調表示を切っている巨大ファイルでは丸ごと止める。
        let structure_on = self.editor.buffers[active].highlight_enabled();
        let read_only = self.editor.buffers[active].read_only();
        if structure_on {
            self.editor.buffers[active].refresh_folds();
        }
        // ジャンプ先が畳んだ中にあるなら、先にその折りたたみを開く
        let has_folds = structure_on && !self.editor.buffers[active].folds.folded().is_empty();
        if has_folds {
            if let Some((sel, _)) = pending_select {
                let line0 = self.editor.buffers[active]
                    .text
                    .chars()
                    .take(sel)
                    .filter(|c| *c == '\n')
                    .count();
                self.reveal_line(active, line0);
            }
        }
        let hidden_spans = if structure_on {
            self.editor.buffers[active].folds.hidden_spans()
        } else {
            Vec::new()
        };
        // ガターに出す印: 折りたたみ (行 → 畳んでいるか) とブックマーク
        let fold_marks: HashMap<usize, bool> = if structure_on {
            let f = &self.editor.buffers[active].folds;
            f.ranges()
                .iter()
                .map(|r| (r.start_line, f.is_folded(r.start_line)))
                .collect()
        } else {
            HashMap::new()
        };
        let bookmark_lines: HashSet<usize> = self.editor.buffers[active].bookmarks.iter().collect();
        // ニーモニック付きブックマーク (crate::marks) の印。行 → 表示文字。
        // **ガターは 1 系統しか持たない** — 既存の ◆ と同じ列に重ねて描く。
        let mark_glyphs: HashMap<usize, char> = match &abs {
            Some(p) => self.marks.store().glyphs(p),
            None => HashMap::new(),
        };

        // インデントガイド (鍵にキャレット行を含める = 強調ガイドが行依存のため)
        let tab_w = crate::highlight::DEFAULT_TAB_WIDTH;
        let caret_line0 = self.editor.cursor.0.saturating_sub(1);
        let mut guide_cache = self.guide_cache.take();
        if structure_on {
            let gkey = [tab_w as u64, caret_line0 as u64]
                .into_iter()
                .fold(text_hash, combine_hash);
            if guide_cache.as_ref().map(|(k, ..)| *k) != Some(gkey) {
                let t = &self.editor.buffers[active].text;
                guide_cache = Some((
                    gkey,
                    crate::highlight::indent_guides(t, tab_w),
                    crate::highlight::active_guide(t, tab_w, caret_line0),
                ));
            }
        } else {
            guide_cache = None;
        }

        // 折りたたみ表示テキスト (キャッシュ。無効なら作り直す)
        let fold_key = hidden_spans.iter().fold(text_hash, |acc, (s0, e0)| {
            combine_hash(combine_hash(acc, *s0 as u64), *e0 as u64)
        });
        let buf_id_now = self.editor.buffers[active].id;
        let mut fv = self
            .fold_view
            .take()
            .filter(|v| v.buf == buf_id_now && v.key == fold_key);
        if hidden_spans.is_empty() {
            fv = None;
        } else if fv.is_none() {
            let (t, lines, cut) = build_fold_view(&self.editor.buffers[active].text, &hidden_spans);
            fv = Some(FoldView {
                buf: buf_id_now,
                key: fold_key,
                prev: t.clone(),
                text: t,
                lines,
                cut,
            });
        }
        // 予約済みの選択は原文の添字なので、表示テキストの添字へ写す
        if let (Some(v), Some((sa0, sb0))) = (fv.as_ref(), pending_select) {
            pending_select = Some((
                fold_source_to_display(&v.cut, sa0),
                fold_source_to_display(&v.cut, sb0),
            ));
        }
        let (mut disp_text, disp_prev, disp_lines, disp_cut) = match fv {
            Some(v) => (Some(v.text), v.prev, v.lines, v.cut),
            None => (None, String::new(), Vec::new(), Vec::new()),
        };

        // スティッキーヘッダ (前フレームのスクロール量から最上部の可視行を推定)
        let mut sticky_cache = self.sticky_cache.take();
        let top_disp_line = if row_h > 0.0 {
            (self.last_scroll_y / row_h).floor().max(0.0) as usize
        } else {
            0
        };
        let top_src_line = if disp_lines.is_empty() {
            top_disp_line
        } else {
            disp_lines
                .get(top_disp_line)
                .copied()
                .unwrap_or(top_disp_line)
        };
        if structure_on {
            let skey = [top_src_line as u64, hash_str(&lang_clone)]
                .into_iter()
                .fold(text_hash, combine_hash);
            if sticky_cache.as_ref().map(|(k, _)| *k) != Some(skey) {
                sticky_cache = Some((
                    skey,
                    crate::highlight::sticky_headers(
                        &self.editor.buffers[active].text,
                        &lang_clone,
                        top_src_line,
                        STICKY_MAX_ROWS,
                    ),
                ));
            }
        } else {
            sticky_cache = None;
        }

        // ── 巨大ファイルの構文ハイライト (可視域だけ塗る) ──────────────
        //
        // `MAX_HIGHLIGHT_BYTES` (400KB) を超える文書は、素の `layout_job` だと
        // **色を丸ごと捨てて**白一色で返る。`layout_job_visible` は可視域だけを
        // 正しい文脈で塗るので、2MB のファイルでも 20,000 行目に色が付く。
        //
        // 正しい文脈まで解析のフロンティアを進めるのは `advance_to_visible` の
        // 仕事で、こちらは `LayoutJob` を作らない = galley を組み直さないので
        // **毎フレーム呼んでよい**。追い付き済みなら本文に 1 バイトも触らずに
        // 返るので、スクロールが止まっているあいだの費用はゼロ (設計原則 3)。
        let hl_rows = if row_h > 0.0 {
            (view_h / row_h).ceil() as usize + 2
        } else {
            1
        };
        // **生のスクロール行番号は galley キーへ混ぜない。** `snap_window` が
        // 512 行の倍数へ丸めた値だけを混ぜる (1 行動くたびに組み直すと
        // 実測 495ms/回 が毎フレーム乗る)。
        let hl_win = crate::highlight::snap_window(top_disp_line, hl_rows);
        // 直前のフレームで可視域の塗り分けが効いたか (galley キーへ可視域を
        // 混ぜてよいかの判定。詳細は `galley_window_key`)。
        //
        // **まだ分からないうちは「効く」側に倒す。** 外した場合に払うのは
        // 小さい文書の galley を 1 回組み直す費用だけだが、逆に倒して外すと
        // 巨大ファイルの組み直し (実測 495ms) を開くたびに余計に 1 回払う。
        let hl_windowed_prev = self.hl_windowed.get(&ed_id_early).copied().unwrap_or(true);
        // 追い付きが完了したフレームで 1 回だけ galley を捨てる印。
        let hl_drop_galley = {
            let hl_text: &str = match disp_text.as_deref() {
                Some(d) => d,
                None => &self.editor.buffers[active].text,
            };
            let adv = self.highlighter.advance_to_visible(
                hl_text,
                &lang_clone,
                &syntect_theme,
                theme_text,
                hl_win,
            );
            // 追い付きは (本文, 可視域, バッファ) ごとに決まる。どれかが動いたら
            // 「まだ追い付いていない」から数え直す。
            let key = [
                hl_win.start as u64,
                hl_win.end as u64,
                disp_lines.len() as u64,
            ]
            .into_iter()
            .fold(
                crate::editor::combine_hash(text_hash, self.editor.buffers[active].id),
                crate::editor::combine_hash,
            );
            // 捨てるのは **`false` → `true` に変わったその 1 回だけ**。
            // 「記録が無い」は変化ではない (タブを切り替えただけで組み直す
            // ことになり、巨大ファイルでは 495ms を無駄に払う)。
            let was_waiting = self.hl_ready.get(&ed_id_early) == Some(&(key, false));
            // 可視域で塗り分けている文書だけが塗り直しの対象
            // (小さい文書は全文を塗ってあるので捨てる意味が無い)。
            let drop_galley = hl_windowed_prev && adv.ready && was_waiting;
            // 閉じたタブぶんが残り続けないよう、増えすぎたら丸ごと捨てる
            // (作り直しは 1 フレームで済む値なので LRU を持つ意味が無い)。
            if self.hl_ready.len() > HL_STATE_CAP {
                self.hl_ready.clear();
            }
            self.hl_ready.insert(ed_id_early, (key, adv.ready));
            if !adv.ready {
                // **追い付くまでだけ**再描画を要求する。追い付いたら止めるので
                // アイドルでは 1 フレームも要求しない。
                crate::perf::repaint(ui.ctx(), "highlight-window");
            }
            drop_galley
        };

        // 同一シンボルの薄いハイライト (LSP documentHighlight) を塗る位置。
        // 応答が来た時点で計算済みの char スパンを写すだけで、ここでは本文を
        // 走査しない。折りたたみ中は表示テキストと char 添字がずれるので塗らない。
        let folding = disp_text.is_some();
        let occ_color = self.theme.accent.gamma_multiply(0.16);
        let occ_spans: Vec<(usize, usize)> =
            if folding || self.lsp_highlight_buf != Some(self.editor.buffers[active].id) {
                Vec::new()
            } else {
                self.lsp_highlight_spans.clone()
            };
        // 複数キャレットの描画データ (char 範囲)。`TextEdit` は主キャレットしか
        // 描かないので、残りの縦線と選択の背景をここのデータで重ね塗りする。
        // 折りたたみ中は表示テキストと char 添字がずれるので塗らない
        // (occ_spans と同じ理由)。色はテーマから取る (固定色を書かない)。
        let multi_spans: Vec<(usize, usize)> = match &self.multi_sel {
            Some((mid, s)) if !folding && *mid == self.editor.buffers[active].id && s.len() > 1 => {
                s.to_char_ranges(&self.editor.buffers[active].text)
                    .into_iter()
                    .take(MULTI_PAINT_MAX)
                    .map(|r| (r.start, r.end))
                    .collect()
            }
            _ => Vec::new(),
        };
        let multi_caret_color = self.theme.accent;
        let multi_sel_color = self.theme.accent.gamma_multiply(0.28);
        // キャレットの線幅は物理ピクセルへ揃える (端末セルと同じ理由。
        // 小数のままだと桁によって 1px/2px に揺れて汚い)。
        let multi_caret_w = crate::theme::snap_len(1.5, ppp).max(1.0 / ppp);

        // ── Git blame (既定 OFF) ────────────────────────────────────
        // **可視ブロックだけ**を非同期で取る。キーは (パス, 保存時ハッシュ,
        // ブロック範囲) なので、同じ場所を見ている限り git は 1 度も起きない。
        // 打鍵中に取り直さないのは意図的 — `git blame` はディスク上の内容を
        // 見るので、保存するまで結果は変わらない。
        let char_w = ui.fonts(|f| f.glyph_width(&font, '0'));
        let line_count = self.editor.buffers[active].text.split('\n').count();
        let blame_mode = self.cfg.git_blame;
        // 折りたたみ中でも blame は**原文の行**に紐づく。表示行を原文行へ写す。
        let caret_src_line = if disp_lines.is_empty() {
            caret_line0
        } else {
            disp_lines.get(caret_line0).copied().unwrap_or(caret_line0)
        };
        // `current` で描くのはこの 1 行だけ (GitLens の既定と同じ)。
        let blame_only_line = match blame_mode {
            config::BlameMode::Current => Some(caret_src_line),
            _ => None,
        };
        let blame: Option<(git::BlameKey, git::BlameMap)> = if blame_mode.is_on()
            && self.editor.buffers[active].kind == crate::editor::BufferKind::File
        {
            // **取りに行く行域もモードで変える。** 全行ぶん取ってから 1 行だけ
            // 描くのでは重さが `all` と変わらず、3 段にした意味が無い。
            let (bs, be) = match blame_mode {
                config::BlameMode::Current => blame_current_range(caret_src_line, line_count),
                _ => {
                    let rows = if row_h > 0.0 {
                        (view_h / row_h).ceil() as usize + 2
                    } else {
                        1
                    };
                    git::blame_block(top_src_line + 1, top_src_line + 1 + rows, line_count)
                }
            };
            let rev = self.editor.buffers[active].saved_hash;
            match abs.clone() {
                Some(p) => match self.gitinfo.locate(&p) {
                    // git リポジトリでなければ静かに何もしない (ジョブも起こさない)
                    None => None,
                    Some((top, rel)) => {
                        let key = git::BlameKey {
                            path: p,
                            rev,
                            start: bs,
                            end: be,
                        };
                        self.blame
                            .request(&top, &rel, key.clone())
                            .map(|m| (key, m))
                    }
                },
                None => None,
            }
        } else {
            // OFF の間はキャッシュもワーカーも持たない (既に空なら無視される)
            self.blame.clear();
            None
        };
        // blame 欄の計画。**いま読み込んでいるブロック全体**から決めるので、
        // ブロック内をスクロールしても列幅は動かない (画面が突然変わらない)。
        // 著者が 1 人しか居なければ著者名は出さない — 全行に同じ文字列が並ぶ
        // だけで情報量がゼロだから。狭い窓では従来どおり 0 (= 列ごと消す)。
        // `blame_now` は計画を立てた時刻。行ラベルにも**この値**を使う。
        let (blame_plan, blame_now) = match blame.as_ref() {
            Some((key, map)) => {
                let max = git::blame_gutter_cols(ui.available_width(), char_w);
                self.blame.column_plan(
                    key,
                    map,
                    git::unix_now(),
                    git::BlameFit {
                        max_cols: max,
                        single_line: blame_only_line.is_some(),
                    },
                )
            }
            None => (git::BlameColumnPlan::HIDDEN, 0),
        };
        let blame_cols = blame_plan.cols;

        // 虹色括弧 (VS Code の editor.bracketPairColorization)。
        // 色は**テーマから**採る (直書きしない)。強調表示を切っている
        // 巨大ファイルでは走らせない — 本文全走査になるため。
        //
        // 下の「対応括弧の強調」(`diagview::bracket_hl`) とは**層が違うので共存する**:
        //   * こちらは galley の**文字色**を差し替える (下地)
        //   * あちらはキャレットの隣の 1 組へ半透明の矩形と枠を**上から**重ねる
        // どちらか一方を消す必要は無い。深さの色は矩形越しに透けて見える。
        let bracket_on = self.bracket_colorization && structure_on;
        let bracket_cols = self.theme.bracket_colors();
        let bracket_err = self.theme.err;
        // 括弧の色はテーマ由来なので、テーマ名もキャッシュキーに混ぜる
        // (syntect テーマ名は複数のテーマで共有されていて区別にならない)。
        let theme_name_hash = hash_str(&self.theme.name);

        // ── 対応括弧の強調 ──
        // 相手を探すのは `editor_ops::matching_bracket` (括弧ジャンプと同じ
        // 関数) だけ。ここは結果を (char 添字, 相手がいるか) へ写して持つ。
        // **キャレットか本文が変わったときだけ**走査する = アイドルは 0 コスト。
        // 折りたたみ中は表示テキストと char 添字がずれるので出さない。
        let bracket_spans: Vec<(usize, bool)> = if folding {
            self.bracket_hl = None;
            Vec::new()
        } else {
            let caret = egui::TextEdit::load_state(ui.ctx(), ed_id_early)
                .and_then(|st| st.cursor.char_range())
                .filter(|r| r.primary.index == r.secondary.index)
                .map(|r| r.primary.index);
            match caret {
                Some(c) => {
                    let fresh = self
                        .bracket_hl
                        .as_ref()
                        .is_some_and(|(h, cc, _)| *h == text_hash && *cc == c);
                    if !fresh {
                        let v = match diagview::bracket_hl(&self.editor.buffers[active].text, c) {
                            Some(h) => {
                                let mut v = vec![(h.at, h.other.is_some())];
                                if let Some(o) = h.other {
                                    v.push((o, true));
                                }
                                v
                            }
                            None => Vec::new(),
                        };
                        self.bracket_hl = Some((text_hash, c, v));
                    }
                    self.bracket_hl
                        .as_ref()
                        .map(|(_, _, v)| v.clone())
                        .unwrap_or_default()
                }
                None => {
                    self.bracket_hl = None;
                    Vec::new()
                }
            }
        };
        // 対応するペアはアクセント、相手のいない括弧はエラー色 (色はテーマ由来)
        let bracket_fill = [
            theme_err.gamma_multiply(0.30),
            theme_accent.gamma_multiply(0.30),
        ];
        let bracket_edge = [
            theme_err.gamma_multiply(0.85),
            theme_accent.gamma_multiply(0.85),
        ];
        // severity → 色 (1..=4 の順)。`diagview::severity_color` を通すので
        // ここにも app.rs にも色のベタ書きは無い。
        let diag_colors: [Color32; 4] = [
            diagview::severity_color(&self.theme, 1),
            diagview::severity_color(&self.theme, 2),
            diagview::severity_color(&self.theme, 3),
            diagview::severity_color(&self.theme, 4),
        ];
        // 行末の診断メッセージ (Error Lens 相当)。既定オン・カーソル行だけ。
        let inline_diag_on = self.cfg.inline_diagnostics && !folding;
        // インレイヒント (型・引数名)。既定オフ・可視行すべて。
        // 折りたたみ中は char 添字がずれるので出さない (波線と同じ判断)。
        let inlay_views = if self.cfg.inlay_hints && !folding {
            std::sync::Arc::clone(&self.inlay_cache.views)
        } else {
            std::sync::Arc::new(Vec::new())
        };
        // 種別 → 色 (0/1 = 型ほか, 2 = 引数名)。テーマ経由でしか取らない。
        let inlay_colors: [Color32; 2] = [
            diagview::inlay_color(&self.theme, lsp::INLAY_KIND_TYPE),
            diagview::inlay_color(&self.theme, lsp::INLAY_KIND_PARAMETER),
        ];
        // 波線を引く範囲と、行末メッセージに使う診断そのもの。
        // 折りたたみ中は char 添字がずれるので波線は出さない (ガターの印は残る)。
        let empty_spans: Vec<diagview::DiagSpan> = Vec::new();
        let diag_spans: &[diagview::DiagSpan] = if folding {
            &empty_spans
        } else {
            &self.diag_cache.spans
        };
        let diag_items = self.diag_cache.items.clone();
        let diag_by_line = &self.diag_cache.by_line;
        let hl = self.highlighter;
        // 取り消し履歴へ渡すしきい値と上限は設定から (直書きしない)
        let hist_edit = self.edit_typed();
        let buf = &mut self.editor.buffers[active];
        let Buffer {
            id,
            text,
            lang,
            cache,
            gutter,
            minimap,
            history,
            ..
        } = buf;

        // 行番号ガター: git マークで行ごとに色分けした LayoutJob をキャッシュ
        let mut marks_hash: u64 = marks.len() as u64;
        for (l, m) in marks.iter() {
            marks_hash = marks_hash
                .wrapping_mul(31)
                .wrapping_add(((*l as u64) << 1) | matches!(m, git::LineMark::Added) as u64);
        }
        let mut diag_hash: u64 = diag_by_line.len() as u64;
        for (l, sev) in diag_by_line {
            diag_hash = diag_hash
                .wrapping_mul(37)
                .wrapping_add((*l as u64) << 3 | *sev as u64);
        }
        // ガターの galley 化は ScrollArea 描画の後に行う (折り返し ON では本文
        // galley の視覚行の並びが要るため)。幅は桁数から先に確定できる
        // (等幅フォントなので数字幅 × 桁数で galley と同じ幅になる)。
        let gutter_digits = line_count.to_string().len().max(3);
        // 右端に「折りたたみ ▸/▾」と「ブックマーク ◆」の 2 桁を確保する
        let marker_w = if structure_on {
            FOLD_MARKER_W * 2.0
        } else {
            0.0
        };
        // blame 欄は行番号の**左**に置く (行番号は本文の隣に残す)。
        // x の配置は `git::gutter_layout` が唯一の定義 (テーブルテストで固定)。
        let gl = git::gutter_layout(blame_cols, char_w, gutter_digits, marker_w, ppp);
        let blame_w = gl.blame_w;
        let gutter_w = gl.width;

        let ed_id = buf_edit_id(self.cur_pane, *id);
        // 折り返し OFF: highlight::layout_job が wrap.max_width = INFINITY を設定
        // する (横スクロールのため折り返さない) ので galley は wrap 幅に依存しない。
        // 折り返し ON: TextEdit から渡る利用可能幅で折り返す。キャッシュキーに
        // 折り返し幅と空白可視化の有無も混ぜてあるので、条件が変わらない限り
        // フレーム跨ぎで使い回せる。
        // 追い付きが終わったフレームは、暫定色で組んだ galley を 1 回だけ捨てる。
        if hl_drop_galley {
            *cache = None;
        }
        // 可視域で塗り分けが効いたかを layouter から持ち帰る器
        // (次のフレームの `galley_window_key` の入力になる)。
        let hl_windowed_now = std::cell::Cell::new(hl_windowed_prev);
        // galley キーへ混ぜる可視域。**効くと分かってからしか混ぜない**。
        let (win_k0, win_k1) = galley_window_key(hl_windowed_prev, hl_win);
        let mut layouter = |ui: &egui::Ui, t: &str, wrap_w: f32| {
            let max_w = crate::editor::wrap_max_width(word_wrap, wrap_w);
            let key = [
                hash_str(lang.as_str()),
                hash_str(&syntect_theme),
                theme_name_hash,
                font.size.to_bits() as u64,
                font_gen,
                (word_wrap as u64) | ((show_ws as u64) << 1) | ((bracket_on as u64) << 2),
                max_w.to_bits() as u64,
                find_key,
                win_k0 as u64,
                win_k1 as u64,
            ]
            .into_iter()
            .fold(hash_str(t), combine_hash);
            match cache {
                // ヒット時は Arc の参照カウント増加のみ。
                // LayoutJob のコピーも egui 側の job ハッシュ計算も起きない。
                Some((k, g)) if *k == key => g.clone(),
                _ => {
                    // 巨大ファイルはここで**可視域だけ**が塗り分けられる。
                    // `scanned_lines > 0` = 窓が効いた (小さい文書は 0)。
                    let v = hl.layout_job_visible(
                        t,
                        lang,
                        &syntect_theme,
                        font.clone(),
                        theme_text,
                        hl_win,
                    );
                    hl_windowed_now.set(v.scanned_lines > 0);
                    let mut j = v.job;
                    // 虹色括弧も**空白可視化の前**に当てる (下と同じ理由)。
                    // 括弧だけを深さの色へ塗り替える (本文は 1 バイトも動かない)。
                    if bracket_on {
                        let hits = crate::highlight::bracket_pairs(t, lang);
                        j = crate::highlight::colorize_brackets(
                            j,
                            &hits,
                            &bracket_cols,
                            bracket_err,
                        );
                    }
                    // 検索ヒットの背景は**空白可視化の前**に差す。
                    // whitespace_layout_job は ' ' → '·' でバイト長を変えるので、
                    // 後から当てるとバイト範囲がズレる (書式は引き継がれる)。
                    if !find_hits.is_empty() {
                        j = find_buffer::apply_hits(
                            j,
                            &find_hits,
                            find_current,
                            find_hit_bg,
                            find_cur_bg,
                        );
                    }
                    if show_ws {
                        // スペース→「·」/ タブ→「→」(char 数は変えない)
                        j = crate::editor::whitespace_layout_job(j, ws_color);
                    }
                    j.wrap.max_width = max_w;
                    let g = ui.fonts(|f| f.layout_job(j));
                    *cache = Some((key, g.clone()));
                    g
                }
            }
        };

        // 折り返し ON では横スクロールは不要 (幅は常に表示域に収まる)
        let mut sa = if word_wrap {
            egui::ScrollArea::vertical()
        } else {
            egui::ScrollArea::both()
        }
        .id_salt(("editor-scroll", *id))
        .auto_shrink(false);
        if let Some(y) = pending_scroll {
            sa = sa.vertical_scroll_offset(y);
        }

        // VS Code の scrollBeyondLastLine: 最終行を越えてスクロールできる余白
        let past_end = (view_h - row_h * 3.0).max(0.0);

        let body_ui = |ui: &mut egui::Ui| {
            if let Some((s, e)) = pending_select {
                let mut st = egui::TextEdit::load_state(ui.ctx(), ed_id).unwrap_or_default();
                st.cursor.set_char_range(Some(egui::text::CCursorRange::two(
                    egui::text::CCursor::new(s),
                    egui::text::CCursor::new(e),
                )));
                st.store(ui.ctx(), ed_id);
                ui.ctx().memory_mut(|m| m.request_focus(ed_id));
            }

            let mut cursor_out: Option<(usize, usize)> = None;
            let mut changed_flag = false;
            let mut text_top: Option<f32> = None;
            let mut text_left: Option<f32> = None;
            let mut caret_at: Option<egui::Pos2> = None;
            let mut hover_hit: Option<(usize, egui::Pos2)> = None;
            let mut sel_out: Option<(usize, usize)> = None;
            let mut multi_ptr: Option<MultiPointer> = None;
            // 編集対象: 折りたたみ中は表示テキスト、読み取り専用なら
            // is_mutable() == false の包み (選択とコピーは残る)
            let mut target = match (read_only, disp_text.as_mut()) {
                (false, Some(d)) => EditTarget::Rw(d),
                (false, None) => EditTarget::Rec {
                    text: &mut *text,
                    hist: &mut *history,
                    ed: hist_edit,
                },
                (true, Some(d)) => EditTarget::Ro(&*d),
                (true, None) => EditTarget::Ro(&*text),
            };
            ui.horizontal_top(|ui| {
                // ガターぶんの余白だけ空けて本文を置く
                // (ガター自体はスクロール確定後に上から固定描画する)
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.add_space(gutter_w);

                let output = egui::TextEdit::multiline(&mut target)
                    .id(ed_id)
                    .font(font.clone())
                    .code_editor()
                    .frame(false)
                    .desired_width(f32::INFINITY)
                    .margin(egui::Margin::ZERO)
                    .layouter(&mut layouter)
                    .show(ui);
                changed_flag = output.response.changed();
                // ガイドツアーの「エディタ本文」はこの矩形
                tutorial::anchor(ui.ctx(), AnchorId::EditorBody, output.response.rect);
                // 本文が実際に描かれた y 原点。ScrollArea はホイールの
                // オフセットを配置後に適用するため、state.offset ではなく
                // これを使わないとガターが 1 フレームずれて「泳ぐ」
                text_top = Some(output.response.rect.top());
                text_left = Some(output.response.rect.left());
                // 補完ポップアップの基準 = キャレット行の左下
                if let Some(cr) = output.cursor_range {
                    let r = output.galley.pos_from_cursor(&cr.primary);
                    caret_at = Some(egui::pos2(
                        output.galley_pos.x + r.min.x,
                        output.galley_pos.y + r.max.y,
                    ));
                }
                // ホバー: マウス下の文字位置 (表示テキストの char 添字)
                if let Some(p) = ui.ctx().pointer_hover_pos() {
                    if output.response.rect.contains(p) {
                        let c = output.galley.cursor_from_pos(p - output.galley_pos);
                        hover_hit = Some((c.ccursor.index, p));
                    }
                }

                // 現在行ハイライト (VS Code 相当)。選択中は出さない。
                // テキスト描画の後に重ねるため、文字を邪魔しない極薄の帯にする。
                if output.response.has_focus() {
                    if let Some(cr) = output.cursor_range {
                        if cr.primary.ccursor.index == cr.secondary.ccursor.index {
                            let row = output.galley.pos_from_cursor(&cr.primary);
                            let row_rect = egui::Rect::from_min_max(
                                egui::pos2(
                                    output.response.rect.left(),
                                    output.galley_pos.y + row.min.y,
                                ),
                                egui::pos2(
                                    output.response.rect.right(),
                                    output.galley_pos.y + row.max.y,
                                ),
                            );
                            ui.painter().rect_filled(row_rect, 0.0, cur_line_hl);
                        }
                    }
                }

                // カーソル下シンボルと同じものの薄いハイライト。
                // 現在行ハイライトと同じく**文字の上に重ねる**ので、字を潰さない
                // 極薄の塗りにする。視覚行をまたぐ範囲は矩形が一意に決まらない
                // ので塗らない (識別子は 1 行に収まるので実害がない)。
                for (s, e) in &occ_spans {
                    let c0 = output.galley.from_ccursor(egui::text::CCursor::new(*s));
                    let c1 = output.galley.from_ccursor(egui::text::CCursor::new(*e));
                    let r0 = output.galley.pos_from_cursor(&c0);
                    let r1 = output.galley.pos_from_cursor(&c1);
                    if (r0.min.y - r1.min.y).abs() > 0.5 || r1.min.x <= r0.min.x {
                        continue;
                    }
                    let rect = egui::Rect::from_min_max(
                        egui::pos2(
                            output.galley_pos.x + r0.min.x,
                            output.galley_pos.y + r0.min.y,
                        ),
                        egui::pos2(
                            output.galley_pos.x + r1.min.x,
                            output.galley_pos.y + r0.max.y,
                        ),
                    );
                    ui.painter().rect_filled(rect, 2.0, occ_color);
                }

                // 対応括弧の強調。カーソルに隣接する括弧と相手の**両方**を塗る。
                // 相手がいない括弧はエラー色 (色は theme 由来・ベタ書きなし)。
                for (idx, matched) in &bracket_spans {
                    let c0 = output.galley.from_ccursor(egui::text::CCursor::new(*idx));
                    let c1 = output
                        .galley
                        .from_ccursor(egui::text::CCursor::new(idx + 1));
                    let r0 = output.galley.pos_from_cursor(&c0);
                    let r1 = output.galley.pos_from_cursor(&c1);
                    // 視覚行をまたぐと矩形が一意に決まらないので塗らない
                    if (r0.min.y - r1.min.y).abs() > 0.5 || r1.min.x <= r0.min.x {
                        continue;
                    }
                    let k = usize::from(*matched);
                    let rect = egui::Rect::from_min_max(
                        egui::pos2(
                            crate::theme::snap_len(output.galley_pos.x + r0.min.x, ppp),
                            crate::theme::snap_len(output.galley_pos.y + r0.min.y, ppp),
                        ),
                        egui::pos2(
                            crate::theme::snap_len(output.galley_pos.x + r1.min.x, ppp),
                            crate::theme::snap_len(output.galley_pos.y + r0.max.y, ppp),
                        ),
                    );
                    ui.painter().rect_filled(rect, 2.0, bracket_fill[k]);
                    ui.painter().rect_stroke(
                        rect,
                        2.0,
                        egui::Stroke::new(1.0_f32, bracket_edge[k]),
                    );
                }

                // ── 複数キャレット: 追加キャレットの縦線と選択範囲の背景 ──
                //
                // egui は主キャレットしか描かないので、残りをここで塗る。
                // 主キャレットぶんは egui が既に描いている (点滅する) ので
                // 二重塗りを避けて飛ばす。追加キャレットは**点滅させない** —
                // 点滅は毎フレームの再描画要求で、設計原則 3 に反する。
                if !multi_spans.is_empty() {
                    let primary = output.cursor_range.map(|cr| {
                        let (a, b) = (cr.primary.ccursor.index, cr.secondary.ccursor.index);
                        (a.min(b), a.max(b))
                    });
                    let gp = output.galley_pos.to_vec2();
                    let rows = &output.galley.rows;
                    let last_row = rows.len().saturating_sub(1);
                    let focused = output.response.has_focus();
                    // 改行が選ばれていることを示す幅 (VS Code と同じ見せ方)
                    let nl_w = char_w * 0.5;
                    for (s, e) in &multi_spans {
                        if primary == Some((*s, *e)) {
                            continue;
                        }
                        let c0 = output.galley.from_ccursor(egui::text::CCursor::new(*s));
                        let c1 = output.galley.from_ccursor(egui::text::CCursor::new(*e));
                        let q0 = output.galley.pos_from_cursor(&c0);
                        let q1 = output.galley.pos_from_cursor(&c1);
                        let r0 = c0.rcursor.row.min(last_row);
                        let r1 = c1.rcursor.row.min(last_row).max(r0);
                        if s != e && !rows.is_empty() {
                            // 跨いだ行の矩形だけを取り出す (巨大ファイルでも O(選択行数))
                            let span: Vec<egui::Rect> =
                                rows[r0..=r1].iter().map(|r| r.rect).collect();
                            for rect in selection_row_rects(&span, q0.min.x, q1.min.x, nl_w) {
                                ui.painter()
                                    .rect_filled(rect.translate(gp), 2.0, multi_sel_color);
                            }
                        }
                        // キャレットは範囲の終端 (= タイプで伸びる側)。
                        // フォーカスが無いときは出さない (egui の主キャレットと同じ)。
                        if focused {
                            ui.painter().vline(
                                gp.x + q1.min.x,
                                egui::Rangef::new(gp.y + q1.min.y, gp.y + q1.max.y),
                                egui::Stroke::new(multi_caret_w, multi_caret_color),
                            );
                        }
                    }
                }

                // 表示行 → 「インレイヒントを描き終えた x」。行末の診断メッセージが
                // 同じ行に出るとき、そこから書き始めて重なりを避けるために使う。
                // ヒントが 1 件も無ければ確保しない (空の HashMap は割り当てゼロ)。
                let mut inlay_row_end: HashMap<usize, f32> = HashMap::new();

                // 診断の波線。深刻度の低い順に並んでいるので、重なった場所は
                // 後に塗る error が上に残る。可視域の外は座標だけ作って捨てる
                // のも惜しいので、行ごとに clip_rect で先に落とす。
                if !diag_spans.is_empty() {
                    let clip = ui.clip_rect();
                    let last_row = output.galley.rows.len().saturating_sub(1);
                    for sp in diag_spans {
                        let color = diag_colors[(sp.severity.clamp(1, 4) - 1) as usize];
                        let c0 = output
                            .galley
                            .from_ccursor(egui::text::CCursor::new(sp.start));
                        let c1 = output.galley.from_ccursor(egui::text::CCursor::new(sp.end));
                        let row0 = c0.rcursor.row.min(last_row);
                        let end_row = c1.rcursor.row.min(last_row);
                        // 1 件の診断が抱える視覚行には上限を置く (巨大な範囲を
                        // 返してくるサーバーでフレームを潰さないため)
                        let row1 = end_row.min(row0 + diagview::SQUIGGLE_MAX_ROWS);
                        let x0 = output.galley.pos_from_cursor(&c0).min.x;
                        let x1 = output.galley.pos_from_cursor(&c1).min.x;
                        for row in row0..=row1 {
                            let Some(r) = output.galley.rows.get(row) else {
                                break;
                            };
                            let y = output.galley_pos.y + r.rect.max.y - diagview::SQUIGGLE_AMP;
                            if y < clip.top() - row_h || y > clip.bottom() + row_h {
                                continue; // 画面外の行は描かない
                            }
                            let a = if row == row0 { x0 } else { r.rect.min.x };
                            // 終端の x を使うのは**終端の行**だけ。上限で打ち切った
                            // 行に他行の x を持ち込むと、関係ない場所へ線が伸びる。
                            let b = if row == end_row { x1 } else { r.rect.max.x };
                            // 範囲が次の行頭で終わる場合、その行には引くものが無い
                            if row == end_row && row > row0 && b <= r.rect.min.x + 0.5 {
                                continue;
                            }
                            // 空行や範囲の継ぎ目で幅 0 になったら 1 文字ぶんだけ見せる
                            let b = if b <= a { a + char_w } else { b };
                            // 可用領域 (= スクロール窓) からはみ出さないよう先に詰める
                            let ax = (output.galley_pos.x + a).max(clip.left());
                            let bx = (output.galley_pos.x + b).min(clip.right());
                            let pts = diagview::squiggle_points(
                                ax,
                                bx,
                                y,
                                diagview::SQUIGGLE_AMP,
                                diagview::SQUIGGLE_WAVE,
                                ppp,
                            );
                            if pts.len() >= 2 {
                                ui.painter()
                                    .add(egui::Shape::line(pts, egui::Stroke::new(1.0_f32, color)));
                            }
                        }
                    }
                }

                // ── インレイヒント (型・引数名) ───────────────────────
                //
                // **本文の galley には一切触らない。** ヒントを本文へ混ぜたり
                // レイアウタで足したりすると galley の char 添字が原文とずれ、
                // キャレット・選択・クリック位置が全部壊れる。かといって挿入
                // 位置へ重ね描きすると右隣のコードを覆う。そこで
                // **行末のマージンへまとめて出し、挿入位置には短い縦の目印**
                // だけを打つ (どのヒントが行のどこに属すかは目印の x で読める)。
                // 判断そのものは diagview::inlay_line_text 側に書いてある。
                if !inlay_views.is_empty() && char_w > 0.0 {
                    let clip = ui.clip_rect();
                    let last_row = output.galley.rows.len().saturating_sub(1);
                    let mut done_line = usize::MAX;
                    for v in inlay_views.iter() {
                        if v.line == done_line {
                            continue; // 行あたり 1 回 (行末へまとめて出すため)
                        }
                        done_line = v.line;
                        let anchor = output.galley.from_ccursor(egui::text::CCursor::new(v.at));
                        let row = anchor.rcursor.row.min(last_row);
                        let Some(r) = output.galley.rows.get(row) else {
                            continue;
                        };
                        let y = output.galley_pos.y + r.rect.center().y;
                        if y < clip.top() - row_h || y > clip.bottom() + row_h {
                            continue; // 画面外の行は組み立てすらしない
                        }
                        // 行末 + 2 桁ぶんから書き始め、残り幅に収まる文字数で畳む
                        let x = output.galley_pos.x + r.rect.max.x + char_w * 2.0;
                        let max_chars = (((clip.right() - x) / char_w).floor()).max(0.0) as usize;
                        let Some(text) = diagview::inlay_line_text(&inlay_views, v.line, max_chars)
                        else {
                            continue;
                        };
                        // 行末にまとめた 1 行の色は先頭のヒントの種別で決める
                        // (1 行の中で色を混ぜると、まとまりが読み取れなくなる)。
                        let color = inlay_colors[(v.kind == lsp::INLAY_KIND_PARAMETER) as usize];
                        let painted = ui.painter().text(
                            egui::pos2(
                                crate::theme::snap_len(x, ppp),
                                crate::theme::snap_len(y, ppp),
                            ),
                            Align2::LEFT_CENTER,
                            text,
                            font.clone(),
                            color.gamma_multiply(0.75),
                        );
                        // 同じ行に行末診断も出るときは、その先へ押し出す
                        // (2 つの文章が重なって読めなくなるのを防ぐ)
                        inlay_row_end.insert(row, painted.max.x + char_w * 2.0);
                        // 挿入位置の目印: 行の下端に短い縦線。キャレットと
                        // 見間違えないよう行高の 1/4 だけにする。
                        for (at, kind) in diagview::inlay_marks(&inlay_views, v.line) {
                            let c = output.galley.from_ccursor(egui::text::CCursor::new(at));
                            if c.rcursor.row.min(last_row) != row {
                                continue;
                            }
                            // 目印だけは 1 件ずつの種別で塗る (型と引数名を見分ける)
                            let mc = inlay_colors[(kind == lsp::INLAY_KIND_PARAMETER) as usize];
                            let q = output.galley.pos_from_cursor(&c);
                            let mx = crate::theme::snap_len(output.galley_pos.x + q.min.x, ppp);
                            if mx < clip.left() || mx > clip.right() {
                                continue;
                            }
                            let bottom = output.galley_pos.y + r.rect.max.y;
                            ui.painter().vline(
                                mx,
                                egui::Rangef::new(bottom - r.rect.height() * 0.25, bottom),
                                egui::Stroke::new(1.0_f32, mc.gamma_multiply(0.55)),
                            );
                        }
                    }
                }

                // ── Alt+クリック / Alt+ドラッグ ────────────────────────
                //
                // `Modifiers::alt` は macOS では ⌥、Windows/Linux では Alt に
                // 写る (egui-winit が正規化する) ので OS 分岐は要らない。
                // 折りたたみ中は char 添字が原文とずれるので受け付けない。
                if !folding {
                    let alt = ui.input(|i| i.modifiers.alt);
                    let to_char = |p: egui::Pos2| {
                        output
                            .galley
                            .cursor_from_pos(p - output.galley_pos)
                            .ccursor
                            .index
                    };
                    multi_ptr = if alt && output.response.drag_started() {
                        // egui はしきい値ぶん動いてからドラッグと判定するので、
                        // 始点は「押した点」を使う (数ピクセルずれた桁を掴まない)
                        ui.input(|i| i.pointer.press_origin())
                            .or_else(|| output.response.interact_pointer_pos())
                            .map(|p| MultiPointer::DragStart(to_char(p)))
                    } else if alt && output.response.dragged() {
                        output
                            .response
                            .interact_pointer_pos()
                            .map(|p| MultiPointer::Drag(to_char(p)))
                    } else if alt && output.response.clicked() {
                        output
                            .response
                            .interact_pointer_pos()
                            .map(|p| MultiPointer::Click(to_char(p)))
                    } else if output.response.drag_stopped() {
                        Some(MultiPointer::DragEnd)
                    } else if output.response.clicked() || output.response.drag_started() {
                        // Alt 無しのポインタ操作は複数キャレットを解除する
                        Some(MultiPointer::Clear)
                    } else {
                        None
                    };
                }

                if let Some(cr) = output.cursor_range {
                    // 選択範囲 (char 添字)。折りたたみ中は表示テキストの添字に
                    // なってしまうので、LSP へ渡せる形ではないため None のまま。
                    let (sa, sb) = (cr.primary.ccursor.index, cr.secondary.ccursor.index);
                    if sa != sb && !folding {
                        sel_out = Some((sa.min(sb), sa.max(sb)));
                    }
                    let idx = cr.primary.ccursor.index;
                    let mut line = 1usize;
                    let mut col = 1usize;
                    for ch in egui::TextBuffer::as_str(&target).chars().take(idx) {
                        if ch == '\n' {
                            line += 1;
                            col = 1;
                        } else {
                            col += 1;
                        }
                    }
                    // 折りたたみ中は「表示行」を数えているので原文行へ写す
                    let line = match disp_lines.get(line - 1) {
                        Some(src) => src + 1,
                        None => line,
                    };
                    cursor_out = Some((line, col));

                    // 行末の診断メッセージ (VS Code の Error Lens 相当)。
                    // **キャレット行だけ** — 全行に出すと本文の右側が文章で
                    // 埋まる。設定 `inline_diagnostics` で消せる。
                    if inline_diag_on && !diag_items.is_empty() && char_w > 0.0 {
                        let row = output
                            .galley
                            .from_ccursor(egui::text::CCursor::new(idx))
                            .rcursor
                            .row;
                        if let Some(r) = output.galley.rows.get(row) {
                            let clip = ui.clip_rect();
                            // インレイヒントが同じ行に出ているならその先から書く
                            let x = inlay_row_end
                                .get(&row)
                                .copied()
                                .unwrap_or(output.galley_pos.x + r.rect.max.x + char_w * 2.0);
                            // 残り幅に収まる文字数までしか出さない (行が見切れない)
                            let max_chars =
                                (((clip.right() - x) / char_w).floor()).max(0.0) as usize;
                            if let Some((msg, sev)) =
                                diagview::inline_message(&diag_items, line - 1, max_chars)
                            {
                                let color = diag_colors[(sev.clamp(1, 4) - 1) as usize];
                                ui.painter().text(
                                    egui::pos2(
                                        crate::theme::snap_len(x, ppp),
                                        crate::theme::snap_len(
                                            output.galley_pos.y + r.rect.center().y,
                                            ppp,
                                        ),
                                    ),
                                    Align2::LEFT_CENTER,
                                    msg,
                                    font.clone(),
                                    color.gamma_multiply(0.75),
                                );
                            }
                        }
                    }
                }

                // Enter 直後の自動インデント
                if output.response.changed() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    if let Some(cr) = output.cursor_range {
                        let cursor = cr.primary.ccursor.index;
                        let indented = editor_ops::auto_indent_after_newline(
                            egui::TextBuffer::as_str(&target),
                            cursor,
                        );
                        if let Some((new_text, new_cursor)) = indented {
                            // cache はキーが text ハッシュなので書き換えだけで無効化される
                            target.set(new_text);
                            let mut st =
                                egui::TextEdit::load_state(ui.ctx(), ed_id).unwrap_or_default();
                            st.cursor.set_char_range(Some(egui::text::CCursorRange::one(
                                egui::text::CCursor::new(new_cursor),
                            )));
                            st.store(ui.ctx(), ed_id);
                        }
                    }
                }

                // ── ジャンプモード (2 打鍵で画面上の任意の語へ飛ぶ) ────────
                // **待機中は 1 行も集めない** (`wants_view` が false)。
                // 本文は `desired_width(INFINITY)` で折り返さないので、
                // 視覚行と論理行が 1 対 1 に対応する = 行の切り出しが単純。
                if crate::jump::wants_view() {
                    let origin = output.galley_pos;
                    let clip = ui.clip_rect();
                    let src = egui::TextBuffer::as_str(&target);
                    let first = (((clip.top() - origin.y) / row_h).floor()).max(0.0) as usize;
                    let take = (clip.height() / row_h).ceil() as usize + 2;
                    let rows: Vec<crate::jump::Row> = src
                        .lines()
                        .enumerate()
                        .skip(first)
                        .take(take)
                        .map(|(line, text)| crate::jump::Row {
                            line,
                            text: text.to_string(),
                        })
                        .collect();
                    // 折り返しが無いので rcursor の (row, column) が
                    // そのまま (論理行, 行内の文字位置) になる。
                    let caret = output
                        .cursor_range
                        .map(|cr| crate::jump::Pos {
                            line: cr.primary.rcursor.row,
                            ch: cr.primary.rcursor.column,
                        })
                        .unwrap_or_default();
                    jump_to = crate::jump::exchange(
                        ui.ctx(),
                        crate::jump::View {
                            rows,
                            caret,
                            tab_width: jump_tab_w,
                            // ラベルは可視行の先頭を基準に置く。
                            origin: egui::pos2(origin.x, origin.y + first as f32 * row_h),
                            cell: egui::vec2(char_w, row_h),
                            clip,
                        },
                    );
                }
            });
            // 最終行より先までスクロールできる余白 (VS Code の scrollBeyondLastLine)
            if past_end > 0.0 {
                ui.add_space(past_end);
            }
            (
                cursor_out,
                changed_flag,
                text_top,
                text_left,
                caret_at,
                hover_hit,
                sel_out,
                multi_ptr,
            )
        };

        // ミニマップを出すときは、その帯のぶんだけ**先に**本文の幅を取り置く。
        // (`ui.set_max_width` はタブ行で確定済みの min_rect に引っ張られて効かない)
        // 帯を置ける全体領域 (本文 + 帯)。帯の右端は必ずここの右端に合わせる
        // — ScrollArea の inner_rect はスクロールバーぶん内側に来ることがあり、
        // それを基準にすると帯と画面端の間に死んだ余白ができる。
        let mut mm_area: Option<egui::Rect> = None;
        let inner = if mm_on {
            let avail = ui.available_rect_before_wrap();
            mm_area = Some(avail);
            ui.allocate_ui_with_layout(
                egui::vec2((avail.width() - mm_w).max(0.0), avail.height()),
                egui::Layout::top_down(egui::Align::Min),
                |ui| sa.show(ui, body_ui),
            )
            .inner
        } else {
            sa.show(ui, body_ui)
        };

        // layouter は組み直したフレームにだけ動かす。値だけ取り出しておく
        // (Cell はこの先の `&mut self` と両立しない)。
        let hl_windowed_val = hl_windowed_now.get();

        let (cursor_out, changed, text_top, text_left, caret_at, hover_hit, sel_out, multi_ptr) =
            inner.inner;

        // 行番号ガター: git マークで行ごとに色分けした galley をキャッシュ。
        // 折り返し OFF は論理行と 1:1。ON は本文 galley の視覚行に合わせ、
        // 折り返しの継続行には空行を挟む (行番号は行頭の視覚行にだけ出す)。
        // 本文 galley は直前の sa.show 内の layouter で必ず作られている。
        // キーには LayoutJob の内容(行数/マーク/診断/フォントサイズ/テーマ)に加え
        // ラスタライズ側の font_gen と、折り返し ON では本文キー (= 折り返しの
        // 並びが変わったら作り直すため) も含める。
        let body_key = cache.as_ref().map(|(k, _)| *k).unwrap_or(0);
        // 表示行 → 原文行。折りたたみが無ければ恒等。
        let disp_count = if disp_lines.is_empty() {
            line_count
        } else {
            disp_lines.len()
        };
        let src_of_disp = |d: usize| -> usize {
            if disp_lines.is_empty() {
                d
            } else {
                disp_lines.get(d).copied().unwrap_or(d)
            }
        };
        // 折りたたみ中は表示行と原文行がずれる。印 (検索 / 診断 / ブックマーク) は
        // 原文行なので、その間はミニマップに印を出さない (ずれた場所に出すより無い方がよい)。
        let folds_active = !disp_lines.is_empty();

        // ── ミニマップの行データ: **本文 galley のキーが変わったときだけ**組み直す ──
        //
        // ここが唯一の再構築点。キーが同じフレーム (= テキストもテーマも
        // フォントも折り返しも変わっていないフレーム) では Vec に触りもしない。
        // 設計原則 3「アイドル時のコストはゼロ」。
        if mm_on && minimap.as_ref().map(|(k, _)| *k) != Some(body_key) {
            let rows = match cache.as_ref() {
                Some((_, g)) => crate::minimap::build_rows(
                    &g.job,
                    // 巨大ファイルでハイライトを切っているときは単色
                    if structure_on { None } else { Some(theme_dim) },
                    crate::minimap::MAX_ROWS,
                ),
                None => crate::minimap::MinimapRows::default(),
            };
            *minimap = Some((body_key, rows));
        }

        let gutter_key = [
            marks_hash,
            diag_hash,
            font.size.to_bits() as u64,
            font_gen,
            hash_str(&syntect_theme),
            word_wrap as u64,
            if word_wrap { body_key } else { 0 },
            fold_key,
        ]
        .into_iter()
        .fold(line_count as u64, combine_hash);
        if gutter.as_ref().map(|(k, _)| *k) != Some(gutter_key) {
            let width = gutter_digits;
            let mark_map: HashMap<usize, git::LineMark> = marks.iter().cloned().collect();
            // 診断色(エラー/警告)を git マークより優先する
            let color_of = |n: usize| match diag_by_line.get(&n) {
                Some(1) => theme_err,
                Some(2) => theme_warn,
                _ => match mark_map.get(&n) {
                    Some(git::LineMark::Added) => theme_ok,
                    Some(git::LineMark::Modified) => theme_warn,
                    None => theme_dim,
                },
            };
            let mut job = egui::text::LayoutJob::default();
            job.wrap.max_width = f32::INFINITY;
            let append = |job: &mut egui::text::LayoutJob, s: &str, color| {
                job.append(
                    s,
                    0.0,
                    egui::TextFormat {
                        font_id: font.clone(),
                        color,
                        ..Default::default()
                    },
                );
            };
            if word_wrap {
                if let Some((_, g)) = cache.as_ref() {
                    let rows = &g.rows;
                    let mut line = 0usize;
                    let mut at_line_start = true;
                    for (ri, row) in rows.iter().enumerate() {
                        let src = src_of_disp(line);
                        let num = if at_line_start {
                            format!("{:>width$}", src + 1)
                        } else {
                            String::new() // 折り返しの継続行は空欄
                        };
                        let s = if ri + 1 < rows.len() {
                            format!("{num}\n")
                        } else {
                            num
                        };
                        append(&mut job, &s, color_of(src));
                        if row.ends_with_newline {
                            line += 1;
                            at_line_start = true;
                        } else {
                            at_line_start = false;
                        }
                    }
                }
            } else {
                for n in 0..disp_count {
                    let src = src_of_disp(n);
                    let s = if n + 1 < disp_count {
                        format!("{:>width$}\n", src + 1)
                    } else {
                        format!("{:>width$}", src + 1)
                    };
                    append(&mut job, &s, color_of(src));
                }
            }
            *gutter = Some((gutter_key, ui.fonts(|f| f.layout_job(job))));
        }
        // Arc の参照カウント増加だけ。LayoutJob のコピーも再レイアウトも起きない。
        let gutter_galley = match gutter.as_ref() {
            Some((_, g)) => g.clone(),
            None => ui.fonts(|f| f.layout_job(Default::default())),
        };

        // ガターを固定描画: 垂直スクロールには追従し、水平スクロールでは動かない
        let vis = inner.inner_rect;
        self.last_view_h = vis.height();
        self.last_scroll_y = inner.state.offset.y;
        let painter = ui.painter_at(vis);
        let gutter_edge = vis.left() + gutter_w - 10.0;
        // ガターは本文と別の背景色 + 境界線で塗り分け、文字を打ち込める
        // 範囲 (境界線の右側) がひと目で分かるようにする
        painter.rect_filled(
            egui::Rect::from_min_max(vis.min, egui::pos2(gutter_edge, vis.bottom())),
            0.0,
            theme_panel,
        );
        painter.galley(
            egui::pos2(
                vis.left() + gl.num_left,
                text_top.unwrap_or(vis.top() - inner.state.offset.y),
            ),
            gutter_galley,
            theme_dim,
        );
        painter.vline(
            gutter_edge,
            vis.y_range(),
            egui::Stroke::new(1.0_f32, theme_border),
        );
        // フォーカスリング: エディタに入力フォーカスがあるときだけ、
        // キー入力が入る本文エリアをアクセント色の枠で囲って明示する
        if ui.memory(|m| m.has_focus(ed_id)) {
            let text_area = egui::Rect::from_min_max(egui::pos2(gutter_edge, vis.top()), vis.max);
            painter.rect_stroke(
                text_area.shrink(1.0),
                0.0,
                egui::Stroke::new(1.5_f32, theme_accent.gamma_multiply(0.65)),
            );
        }

        // ═══ 第 2 次配線: ガターの印 / インデントガイド / スティッキー ═══
        //
        // 本文 galley の**視覚行**を辿って「表示行の先頭行」だけを拾う。
        // 折り返し ON でも OFF でも同じ経路で正しい y が出る。
        let top_y = text_top.unwrap_or(vis.top() - inner.state.offset.y);
        let mut row_lines: Vec<(usize, f32, f32)> = Vec::new();
        if structure_on || blame_cols > 0 {
            if let Some((_, g)) = cache.as_ref() {
                let nl: Vec<bool> = g.rows.iter().map(|r| r.ends_with_newline).collect();
                for (ri, dl) in row_line_starts(&nl) {
                    let row = &g.rows[ri];
                    let y0 = top_y + row.rect.top();
                    let y1 = top_y + row.rect.bottom();
                    // 画面外の行は描かない (巨大ファイルでも O(可視行))
                    if y1 >= vis.top() && y0 <= vis.bottom() {
                        row_lines.push((src_of_disp(dl), y0, y1));
                    }
                }
            }
        }

        // インデントガイド: 桁位置に縦線を引き、キャレットのブロックだけ強調する
        if let (Some((_, guides, active_g)), Some(tl)) = (guide_cache.as_ref(), text_left) {
            let char_w = ui.fonts(|f| f.glyph_width(&font, '0'));
            let dim = theme_border;
            let hot = theme_accent;
            for (src, y0, y1) in &row_lines {
                let Some((_, cols)) = guides.get(*src) else {
                    continue;
                };
                for c in cols {
                    let x = tl + *c as f32 * char_w + 0.5;
                    if x < gutter_edge {
                        continue;
                    }
                    if x > vis.right() {
                        break;
                    }
                    let on = active_g
                        .map(|g| g.column == *c && *src >= g.start_line && *src <= g.end_line)
                        .unwrap_or(false);
                    painter.vline(
                        x,
                        egui::Rangef::new(*y0, *y1),
                        egui::Stroke::new(1.0_f32, if on { hot } else { dim }),
                    );
                }
            }
        }

        // 縦のルーラー (VS Code の editor.rulers): 指定した桁に縦線を引く。
        // 桁は等幅の**桁数**で数える (東アジア文字幅ではない)。
        // 設定が空なら 1 ピクセルも出さない。
        if let (false, Some(tl)) = (self.rulers.is_empty(), text_left) {
            let char_w = ui.fonts(|f| f.glyph_width(&font, '0'));
            let clip = egui::Rangef::new(gutter_edge, vis.right());
            let ppp = ui.ctx().pixels_per_point();
            for x in ruler_x_positions(&self.rulers, tl, char_w, clip, ppp) {
                painter.vline(
                    x,
                    vis.y_range(),
                    egui::Stroke::new(1.0_f32, self.theme.ruler_color()),
                );
            }
        }

        // Git blame: ガターの blame 欄へ薄く出す。
        // 描くのは**可視行だけ** (row_lines が画面外を除いてある)。
        // ラベルは列の**右端**へ寄せる — 左寄せ + 固定幅列だと、ラベルが
        // 短いほど行番号との隙間が広がり、1 文字へ縮退した瞬間に画面の
        // 左端へ取り残される (実際に「離れすぎて見づらい」と報告された)。
        let mut blame_open: Option<String> = None;
        if let (Some((_, map)), false) = (blame.as_ref(), blame_plan.is_hidden()) {
            let now = blame_now;
            let col = egui::Rect::from_min_max(
                egui::pos2(vis.left(), vis.top()),
                egui::pos2(vis.left() + blame_w, vis.bottom()),
            );
            let resp = ui.interact(col, ed_id.with("blame-gutter"), egui::Sense::click());
            let pointer = resp.hover_pos().or_else(|| resp.interact_pointer_pos());
            let mut detail: Option<String> = None;
            let blame_color = theme_dim.gamma_multiply(0.8);
            for (src, y0, y1) in &row_lines {
                // `current` はカーソル行だけ。全行ガターは横幅を食って邪魔になる
                // という評価が競合 (GitLens) で最も多かったので中間の段を置く。
                if blame_only_line.is_some_and(|l| l != *src) {
                    continue;
                }
                let Some(bl) = map.get(src) else {
                    continue;
                };
                // 何を書くかは計画が決める (著者が 1 人なら相対日時だけ)。
                // 幅が足りなければ縮退し、書くことが無ければ描かない。
                if let Some(label) =
                    git::blame_row_label(&blame_plan, &bl.author, bl.uncommitted, bl.time, now)
                {
                    painter.text(
                        egui::pos2(vis.left() + gl.blame_right, *y0),
                        Align2::RIGHT_TOP,
                        &label,
                        font.clone(),
                        blame_color,
                    );
                }
                // ホバー: 完全なコミットメッセージ・SHA・日時
                let row = egui::Rect::from_min_max(
                    egui::pos2(vis.left(), *y0),
                    egui::pos2(vis.left() + blame_w, *y1),
                );
                let Some(p) = pointer.filter(|p| row.contains(*p)) else {
                    continue;
                };
                let _ = p;
                detail = Some(if bl.uncommitted {
                    trf(
                        "{line} 行目: まだコミットされていません",
                        &[("line", (src + 1).to_string())],
                    )
                } else {
                    format!(
                        "{}\n{}\n{} · {}\n{}",
                        bl.summary,
                        bl.sha,
                        bl.author,
                        crate::git::relative_time(bl.time, now),
                        tr("クリックでこのコミットの差分を開きます"),
                    )
                });
                if resp.clicked() && !bl.uncommitted {
                    blame_open = Some(bl.sha.clone());
                }
            }
            if let Some(d) = detail {
                resp.on_hover_text(d);
            }
        }

        // ガターの印: 折りたたみ ▸ / ▾ とブックマーク ◆
        let fold_x = gutter_edge - FOLD_MARKER_W;
        let mark_x = gutter_edge - FOLD_MARKER_W * 2.0;
        let mut toggle_line: Option<usize> = None;
        let mut mark_toggle_line: Option<usize> = None;
        if structure_on {
            let gutter_resp = ui.interact(
                egui::Rect::from_min_max(
                    egui::pos2(mark_x, vis.top()),
                    egui::pos2(gutter_edge, vis.bottom()),
                ),
                ed_id.with("fold-gutter"),
                egui::Sense::click(),
            );
            let hit = gutter_resp.interact_pointer_pos();
            // 印の列 (折りたたみ列より手前) のホバーだけ案内を出す。
            // **打鍵表記はキーマップから作る** — ベタ書きしない。
            if gutter_resp
                .hover_pos()
                .map(|p| p.x < fold_x)
                .unwrap_or(false)
            {
                let hint = crate::keybinds::key_hint(ui.ctx(), BindAction::MarkToggleMnemonic);
                let _ = gutter_resp.on_hover_text(crate::marks::gutter_tooltip(&hint));
            }
            for (src, y0, _) in &row_lines {
                match mark_glyphs.get(src) {
                    // ニーモニック付きはアイコン + 文字 (0.75 倍・中央)
                    Some(g) => crate::marks::paint_gutter_glyph(
                        &painter,
                        egui::pos2(mark_x, *y0),
                        row_h,
                        *g,
                        theme_accent,
                        theme_bg,
                    ),
                    None if bookmark_lines.contains(src) => {
                        painter.text(
                            egui::pos2(mark_x, *y0),
                            Align2::LEFT_TOP,
                            "◆",
                            font.clone(),
                            theme_accent,
                        );
                    }
                    None => {}
                }
                if let Some(folded) = fold_marks.get(src) {
                    painter.text(
                        egui::pos2(fold_x, *y0),
                        Align2::LEFT_TOP,
                        if *folded { "▸" } else { "▾" },
                        font.clone(),
                        theme_dim,
                    );
                }
            }
            if let Some(p) = hit {
                toggle_line = fold_click_line(&row_lines, &fold_marks, fold_x, p);
                // 折りたたみの列より手前 (印の列) はブックマークの付け外し
                let rows: Vec<(usize, f32)> = row_lines.iter().map(|(l, y, _)| (*l, *y)).collect();
                mark_toggle_line = crate::marks::gutter_click_line(&rows, row_h, mark_x, fold_x, p);
            }
        }

        // スティッキーヘッダ: 上端に「いま居る文脈」を貼り付ける
        let sticky: Vec<(usize, String)> = sticky_cache
            .as_ref()
            .map(|(_, v)| v.clone())
            .unwrap_or_default();
        let mut sticky_jump: Option<usize> = None;
        if !sticky.is_empty() {
            let h = row_h * sticky.len() as f32;
            let r = egui::Rect::from_min_max(
                egui::pos2(gutter_edge, vis.top()),
                egui::pos2(vis.right(), vis.top() + h),
            );
            painter.rect_filled(r, 0.0, theme_panel);
            painter.hline(
                r.x_range(),
                r.bottom(),
                egui::Stroke::new(1.0_f32, theme_border),
            );
            for (n, (line, body)) in sticky.iter().enumerate() {
                let y = vis.top() + row_h * n as f32;
                painter.text(
                    egui::pos2(gutter_edge + 8.0, y),
                    Align2::LEFT_TOP,
                    body,
                    font.clone(),
                    theme_text,
                );
                let row_r = egui::Rect::from_min_max(
                    egui::pos2(gutter_edge, y),
                    egui::pos2(vis.right(), y + row_h),
                );
                if ui
                    .interact(row_r, ed_id.with(("sticky", n)), egui::Sense::click())
                    .clicked()
                {
                    sticky_jump = Some(*line);
                }
            }
        }

        // ═══ ミニマップ (右端の細い帯) ═════════════════════════════════
        //
        // ここでやるのは「キャッシュ済みの矩形列を描く」「ビューポート枠を重ねる」
        // 「クリック / ドラッグを受ける」だけ。再計算も再描画要求も出さない。
        let mut mm_scroll: Option<f32> = None;
        if let (true, Some(area)) = (mm_on, mm_area) {
            // 縦は本文の可視範囲、横は「本文 + 帯」の全体領域の右端まで
            let full = egui::Rect::from_min_max(
                vis.min,
                egui::pos2(area.right().max(vis.right() + mm_w), vis.bottom()),
            );
            let geom = crate::minimap::geometry(full, disp_count, ppp);
            // クリックとドラッグでスクロール (これが無いミニマップは飾り)
            let resp = ui.interact(
                geom.strip,
                ed_id.with("minimap"),
                egui::Sense::click_and_drag(),
            );
            if resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if resp.clicked() || resp.dragged() {
                if let Some(p) = resp.interact_pointer_pos() {
                    mm_scroll = Some(geom.scroll_for_y(p.y, row_h, vis.height()));
                }
            }
            let none_hits: Vec<usize> = Vec::new();
            let none_diag: HashMap<usize, u8> = HashMap::new();
            let none_marks: HashSet<usize> = HashSet::new();
            let marks = crate::minimap::Marks {
                search: if folds_active { &none_hits } else { &mm_search },
                diags: if folds_active {
                    &none_diag
                } else {
                    diag_by_line
                },
                bookmarks: if folds_active {
                    &none_marks
                } else {
                    &bookmark_lines
                },
            };
            let colors = crate::minimap::Colors {
                bg: theme_panel,
                border: theme_border,
                viewport: theme_text.gamma_multiply(0.10),
                accent: theme_accent,
                err: theme_err,
                warn: theme_warn,
            };
            let first_line = if row_h > 0.0 {
                inner.state.offset.y / row_h
            } else {
                0.0
            };
            let on_screen = if row_h > 0.0 {
                (vis.height() / row_h).max(1.0)
            } else {
                1.0
            };
            let default_rows = crate::minimap::MinimapRows::default();
            let rows = self.editor.buffers[active]
                .minimap
                .as_ref()
                .map(|(_, r)| r)
                .unwrap_or(&default_rows);
            crate::minimap::paint(
                &ui.painter().with_clip_rect(geom.strip),
                &geom,
                rows,
                &marks,
                &colors,
                first_line,
                on_screen,
            );
        }
        // ミニマップを出さないときは、スクロールバー幅の帯へ**印だけ**を出す。
        // ミニマップは 64px を本文から奪うので既定 off で、印 (検索ヒット /
        // 診断 / ブックマーク) が誰にも見えていなかった。
        //
        // 当たり判定は **`Sense::click()` だけ**にする。`ScrollArea` は egui 側で
        // `outer_scroll_bar_rect` へ `Sense::click_and_drag()` を**先に**置いて
        // いる (egui-0.29.1 の scroll_area.rs:1083) ので、`click_and_drag` を
        // 後から重ねるとドラッグの当たりまで奪い、**つまみのドラッグが
        // 「クリック位置へ飛ぶ」に変わる**。egui の hit_test は click と drag を
        // **別々に**選ぶ (hit_test.rs:122-123) ので、click だけならつまみの
        // ドラッグは egui のまま残る。さらにつまみの上のクリックは
        // 何もしない — 掴み直しただけで表示が飛ばないようにする。
        // 描くのは印だけで、ビューポート枠は描かない (egui のつまみと二重になる)。
        // 印は egui がスクロールバーを描いた**後**に重ねるので、つまみに
        // 隠れず必ず見える。
        //
        // スクロールバーが出ていないフレーム (中身が収まっている) と、
        // 印が 1 つも無いフレームは 1 ピクセルも触らない — 本文の右端に
        // 意味の無い帯を出さないため (「空白は作らない」)。
        let sb_visible = inner.content_size.y > inner.inner_rect.height() + 0.5;
        if !mm_on && sb_visible {
            let sb_w = crate::minimap::scrollbar_width(ppp);
            let band = egui::Rect::from_min_max(
                egui::pos2((vis.right() - sb_w).max(vis.left()), vis.top()),
                egui::pos2(vis.right(), vis.bottom()),
            );
            let sb = crate::minimap::scrollbar_geometry(band, disp_count, ppp);
            let none_hits: Vec<usize> = Vec::new();
            let none_diag: HashMap<usize, u8> = HashMap::new();
            let none_marks: HashSet<usize> = HashSet::new();
            let marks = crate::minimap::Marks {
                search: if folds_active { &none_hits } else { &mm_search },
                diags: if folds_active {
                    &none_diag
                } else {
                    diag_by_line
                },
                bookmarks: if folds_active {
                    &none_marks
                } else {
                    &bookmark_lines
                },
            };
            let first_line = if row_h > 0.0 {
                inner.state.offset.y / row_h
            } else {
                0.0
            };
            let on_screen = if row_h > 0.0 {
                (vis.height() / row_h).max(1.0)
            } else {
                1.0
            };
            let deco = crate::minimap::scrollbar_marks(&sb, &marks, first_line, on_screen, None);
            if !deco.marks.is_empty() {
                let p = ui.painter().with_clip_rect(sb.band);
                for m in &deco.marks {
                    let c = match m.kind {
                        crate::minimap::ScrollKind::Error => theme_err,
                        crate::minimap::ScrollKind::Warn => theme_warn,
                        crate::minimap::ScrollKind::Cursor => theme_text,
                        _ => theme_accent,
                    };
                    p.rect_filled(m.rect, 0.0, c.gamma_multiply(m.weight));
                }
                // 印を押したらそこへ飛ぶ (押せない印は飾り)。
                let resp =
                    ui.interact(sb.band, ed_id.with("scrollbar_marks"), egui::Sense::click());
                if let Some(pos) = resp
                    .clicked()
                    .then(|| resp.interact_pointer_pos())
                    .flatten()
                    .filter(|pos| !deco.viewport.contains(*pos))
                {
                    mm_scroll = Some(sb.scroll_for_y(pos.y, row_h, vis.height()));
                }
            }
        }
        // 可視域の塗り分けが効いたかを次のフレームへ渡す
        // (galley キーへ可視域を混ぜてよいかの判定に使う)。
        if self.hl_windowed.len() > HL_STATE_CAP {
            self.hl_windowed.clear();
        }
        self.hl_windowed.insert(ed_id_early, hl_windowed_val);
        if let Some(y) = mm_scroll {
            self.pending_scroll = Some(y);
        }

        if let Some(c) = cursor_out {
            self.editor.cursor = c;
        }
        // 選択範囲は「選択があれば範囲整形 / 範囲のコードアクション」の分岐に使う
        self.editor_sel_chars = sel_out;

        // 折りたたみ表示への編集を原文へ差し戻す。
        // 表示テキストは原文から隠す行を抜いたものなので、共通接頭辞 /
        // 接尾辞の外側を原文の対応区間へ写して置き換えるだけで足りる。
        // ホバー位置の写像に使うので、FoldView へ戻す前に控えておく
        let hover_cut = disp_cut.clone();
        let disp_cut_len = disp_cut.len();
        let mut spliced = false;
        if let Some(d) = disp_text {
            if d != disp_prev {
                let src = self.editor.buffers[active].text.clone();
                let next = splice_fold_edit(&src, &disp_cut, &disp_prev, &d);
                let (at, delta) = fold_edit_shift(&src, &next, &disp_cut, &disp_prev, &d);
                let ed = self.edit_typed();
                let b = &mut self.editor.buffers[active];
                if delta != 0 {
                    b.folds.shift_lines(at, delta);
                    b.bookmarks.shift_lines(at, delta);
                }
                // 折りたたみ表示への打鍵も原文側の履歴へ 1 段として積む
                b.apply_edit(next, ed);
                let spliced_text = b.text.clone();
                // 増減が分かっている編集は差分を待たずに追従させる
                // (`marks` の経路 1。デバウンス後の一括更新より 1 テンポ速い)
                if delta != 0 {
                    if let Some(p) = path_clone.clone() {
                        self.marks.note_edit(&p, at, delta, &spliced_text);
                    }
                }
                // 表示テキストは次フレームで作り直す
                self.fold_view = None;
                spliced = true;
            } else {
                self.fold_view = Some(FoldView {
                    buf: buf_id_now,
                    key: fold_key,
                    text: d,
                    prev: disp_prev,
                    lines: disp_lines,
                    cut: disp_cut,
                });
            }
        }
        self.guide_cache = guide_cache;
        self.sticky_cache = sticky_cache;

        // ガターのクリックで折りたたみを開閉、スティッキーのクリックでジャンプ
        if let Some(l) = toggle_line {
            let b = &mut self.editor.buffers[active];
            b.folds.toggle_fold(l);
            b.gutter = None;
            self.fold_view = None;
        }
        if let Some(l) = sticky_jump {
            self.goto_line(l + 1);
        }
        // blame のガターをクリック → そのコミットの差分を既存の差分ビューで開く
        if let Some(sha) = blame_open {
            self.open_commit_diff(&sha);
        }

        // 補完 / ホバーの基準位置を控える (ポップアップは中央パネルの後に描く)
        self.caret_screen = caret_at;
        match hover_hit {
            Some((idx, p)) => {
                let src_idx = if disp_cut_len == 0 {
                    idx
                } else {
                    fold_display_to_source(&hover_cut, idx)
                };
                // 波線の上なら診断を出す。**LSP ホバーとは排他** —
                // 同じ場所に説明を 2 枚重ねない (要求そのものを送らない)。
                let hit = diagview::diag_at(&self.diag_cache.spans, src_idx)
                    .and_then(|n| self.diag_cache.get(n))
                    .map(|d| (diagview::labeled_message(d), d.severity));
                match hit {
                    Some((msg, sev)) => {
                        self.diag_hover = Some((msg, sev, p));
                        self.hover_doc_pos = None;
                        self.lsp_hover_pos = None;
                        self.lsp_hover.dismiss();
                    }
                    None => {
                        self.diag_hover = None;
                        self.lsp_hover_pos = Some(p);
                        let t = &self.editor.buffers[active].text;
                        let byte = editor_ops::char_to_byte(t, src_idx.min(t.chars().count()));
                        self.hover_doc_pos = Some(lsp::byte_index_to_lsp_pos(t, byte));
                    }
                }
            }
            None => {
                self.diag_hover = None;
                self.lsp_hover_pos = None;
                self.lsp_hover.dismiss();
            }
        }

        // ジャンプが確定していたらキャレットを運ぶ。描画中はバッファを
        // 可変借用しているので、ここまで持ち越して当てる。
        if let Some(p) = jump_to {
            let text = &self.editor.buffers[active].text;
            // (行, 行内の文字位置) → 本文全体の文字位置。
            let head: usize = text
                .split_inclusive('\n')
                .take(p.line)
                .map(|l| l.chars().count())
                .sum();
            let at = (head + p.ch).min(text.chars().count());
            self.pending_select = Some((at, at));
            // 画面外へ飛んだときだけ追う (見えている語へのジャンプで
            // 画面が動くと、UI 原則「画面が突然変わらない」に反する)。
            let top = self.last_scroll_y / self.last_row_h.max(1.0);
            let rows = (self.last_view_h / self.last_row_h.max(1.0)).max(1.0);
            if (p.line as f32) < top || (p.line as f32) > top + rows - 1.0 {
                self.pending_scroll =
                    Some(((p.line as f32) * self.last_row_h - self.last_view_h * 0.4).max(0.0));
            }
        }

        // 複数キャレットのポインタ操作を反映する (描画中はバッファを可変借用
        // しているので `self` を触れない。拾った操作をここで当てる)。
        if let Some(ev) = multi_ptr {
            self.apply_multi_pointer(active, ev, tab_w, prev_caret);
        }

        // 単一キャレットの編集 (= `TextEdit` 自身の打鍵や整形の差し込み) が
        // 入ったら複数キャレットは捨てる。バイト位置がずれた集合を持ち越すと
        // 本文を壊す。複数キャレット経由の編集は `TextEdit` を通らないので
        // `changed` は立たず、ここでは消えない。
        if (changed || spliced) && self.multi_sel.is_some() {
            self.multi_sel = None;
            self.multi_sticky_col = None;
        }

        // LSP: テキストが変わったらデバウンスして did_change を予約
        if changed || spliced {
            if let (Some(p), lang) = (path_clone.clone(), lang_clone.clone()) {
                let key = self.lsp_key_for(&p, &lang);
                if self.lsp.contains_key(&key) {
                    let text = self.editor.buffers[active].text.clone();
                    self.lsp_pending.insert(p, (text, Instant::now(), key));
                }
            }
        }

        // ガターの印をクリックしていたらここで付け外しする。
        // 描画中は `self` の別フィールドを不変借用しているので、
        // トーストを出せるのはこの位置まで来てから (複数キャレットと同じ流儀)。
        if let (Some(l), Some(p)) = (mark_toggle_line, path_clone) {
            let text = self.editor.buffers[active].text.clone();
            let out = self.marks.quick_toggle(&p, l, &text);
            self.toast(crate::marks::toggle_message(&out), true);
        }
    }
}
