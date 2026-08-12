use super::*;

impl ZaivernApp {
    // ── E: LSP ───────────────────────────────────────────────────

    /// アクティブなバッファに紐づく、起動済み LSP クライアントの鍵とパス。
    pub(super) fn active_lsp_target(&self) -> Option<(LspKey, PathBuf)> {
        let i = self.editor.active?;
        let b = &self.editor.buffers[i];
        let p = b.path.clone()?;
        let key = self.lsp_key_for(&p, &b.lang);
        self.lsp.contains_key(&key).then_some((key, p))
    }

    /// キャレット位置を LSP の Position (UTF-16 桁) にする。
    pub(super) fn caret_lsp_pos(&self) -> Option<lsp::Position> {
        let i = self.editor.active?;
        let t = &self.editor.buffers[i].text;
        let (ln, col) = self.editor.cursor;
        let ci = editor_ops::char_index_at(t, ln.saturating_sub(1), col.saturating_sub(1));
        let byte = editor_ops::char_to_byte(t, ci);
        Some(lsp::byte_index_to_lsp_pos(t, byte))
    }

    /// キャレットの直前にある識別子 (補完の絞り込みキー)。
    pub(super) fn word_before_caret(&self) -> String {
        let Some(i) = self.editor.active else {
            return String::new();
        };
        let t = &self.editor.buffers[i].text;
        let (ln, col) = self.editor.cursor;
        let line = t.split('\n').nth(ln.saturating_sub(1)).unwrap_or("");
        let chars: Vec<char> = line.chars().collect();
        let c = col.saturating_sub(1).min(chars.len());
        let mut s = c;
        while s > 0 && lsp::is_identifier_char(chars[s - 1]) {
            s -= 1;
        }
        chars[s..c].iter().collect()
    }

    /// キャレット位置の識別子まるごと (リネームの初期値)。
    pub(super) fn word_at_caret(&self) -> String {
        let Some(i) = self.editor.active else {
            return String::new();
        };
        let t = &self.editor.buffers[i].text;
        let (ln, col) = self.editor.cursor;
        let line = t.split('\n').nth(ln.saturating_sub(1)).unwrap_or("");
        let chars: Vec<char> = line.chars().collect();
        let c = col.saturating_sub(1).min(chars.len());
        let mut s = c;
        while s > 0 && lsp::is_identifier_char(chars[s - 1]) {
            s -= 1;
        }
        let mut e = c;
        while e < chars.len() && lsp::is_identifier_char(chars[e]) {
            e += 1;
        }
        chars[s..e].iter().collect()
    }

    /// 「参照を検索」。結果はパネルに一覧する。
    pub(super) fn lsp_find_references(&mut self) {
        let (Some((key, path)), Some(pos)) = (self.active_lsp_target(), self.caret_lsp_pos())
        else {
            self.toast_warn(tr("この言語の LSP サーバーが動いていません"));
            return;
        };
        let Some(c) = self.lsp.get(&key) else {
            return;
        };
        if !c.caps().references {
            self.toast_warn(tr("このサーバーは参照の検索に対応していません"));
            return;
        }
        if c.request_references(&path, pos, true).is_sent() {
            self.lsp_refs.clear();
            self.lsp_refs_busy = true;
            self.lsp_refs_open = true;
        }
    }

    /// 「シンボルにジャンプ」。ドキュメントシンボルを quick-open 風に出す。
    pub(super) fn lsp_document_symbols(&mut self) {
        let Some((key, path)) = self.active_lsp_target() else {
            self.toast_warn(tr("この言語の LSP サーバーが動いていません"));
            return;
        };
        let Some(c) = self.lsp.get(&key) else {
            return;
        };
        if !c.caps().document_symbol {
            self.toast_warn(tr("このサーバーはシンボル一覧に対応していません"));
            return;
        }
        if c.request_document_symbols(&path).is_sent() {
            self.lsp_symbols.clear();
            self.lsp_symbols_busy = true;
            self.lsp_symbols_open = true;
            self.lsp_symbols_query.clear();
            self.lsp_symbols_path = Some(path);
            self.lsp_symbols_quiet = false;
            self.rebuild_mention_symbols();
        }
    }

    /// ブレッドクラムのシンボル階層を最新に保つための背景更新。
    ///
    /// **要求経路は `lsp_document_symbols` と同じ** `request_document_symbols` 1 本で、
    /// 違うのは「ピッカーを開かない」「見つからなくても黙る」の 2 点だけ。
    /// 同じ (パス, 本文ハッシュ) へは二重に投げず、失敗時も 700ms は再送しない。
    /// LSP が無い / 非対応なら何もしない (ブレッドクラムのパス部分は消えない)。
    pub(super) fn request_breadcrumb_symbols(&mut self, path: &Path) {
        // シンボルピッカーを開いている間は触らない (ユーザーの一覧を書き換えない)
        if self.lsp_symbols_open || self.lsp_symbols_busy {
            return;
        }
        let hash = self.last_text_hash;
        if hash == 0 {
            return; // まだ本文を 1 度も描いていない
        }
        let now = Instant::now();
        if let Some((p, h, at)) = &self.breadcrumb_symbols_asked {
            if p == path && *h == hash {
                return;
            }
            if now.duration_since(*at) < Duration::from_millis(700) {
                return;
            }
        }
        let Some((key, target)) = self.active_lsp_target() else {
            return;
        };
        if target != path {
            return;
        }
        let Some(c) = self.lsp.get(&key) else {
            return;
        };
        if !c.caps().document_symbol {
            return;
        }
        if c.request_document_symbols(&target).is_sent() {
            // ファイルが変わったときだけ古いシンボルを捨てる。
            // 打鍵のたびに消すと、行がちらついて「突然変わる画面」になる。
            if self.lsp_symbols_path.as_deref() != Some(path) {
                self.lsp_symbols.clear();
                self.lsp_symbols_path = Some(path.to_path_buf());
            }
            self.lsp_symbols_quiet = true;
            self.breadcrumb_symbols_asked = Some((target, hash, now));
        }
    }

    /// 「リネーム」。prepareRename に対応していれば可否を先に尋ねる。
    pub(super) fn lsp_start_rename(&mut self) {
        let (Some((key, path)), Some(pos)) = (self.active_lsp_target(), self.caret_lsp_pos())
        else {
            self.toast_warn(tr("この言語の LSP サーバーが動いていません"));
            return;
        };
        let caps = match self.lsp.get(&key) {
            Some(c) => c.caps(),
            None => return,
        };
        if !caps.rename {
            self.toast_warn(tr("このサーバーはリネームに対応していません"));
            return;
        }
        let name = self.word_at_caret();
        let sent_prepare = caps.prepare_rename
            && self
                .lsp
                .get(&key)
                .map(|c| c.request_prepare_rename(&path, pos).is_sent())
                .unwrap_or(false);
        self.lsp_rename = Some(RenameFlow {
            key,
            path,
            pos,
            preparing: sent_prepare,
            open: !sent_prepare,
            focus: !sent_prepare,
            name,
            applying: false,
        });
    }

    /// 「ドキュメントを整形」。`save_after` が true なら結果の適用後に保存する。
    ///
    /// **選択があれば `textDocument/rangeFormatting` へ振り分ける**
    /// (VS Code の「選択範囲のフォーマット」と同じ)。到達経路は ⇧⌥F と
    /// パレットの 1 本のままで、何を整形するかは選択の有無だけで決まる。
    /// 保存時整形は選択に関係なく常に全体 — 一部だけ整形して保存すると、
    /// 保存のたびに結果が変わって驚くため。
    /// 範囲整形に非対応のサーバーでは黙って全体整形へ落ちる。
    /// 送れたら true (サーバーが無い / 非対応なら false)。
    pub(super) fn lsp_format_document(&mut self, save_after: bool) -> bool {
        let Some(i) = self.editor.active else {
            return false;
        };
        let Some((key, path)) = self.active_lsp_target() else {
            return false;
        };
        // 保存時クリーンアップの設定と食い違わせない
        // (サーバー側で削って、こちらでも削って、で二重にならないようにする)
        let ind = self.editor.buffers[i].indent;
        let opts = lsp::FormatOptions {
            tab_size: ind.width as u32,
            insert_spaces: !ind.tabs,
            trim_trailing_whitespace: self.save_trim_trailing,
            insert_final_newline: self.save_final_newline,
            trim_final_newlines: self.save_trim_final_newlines,
        };
        let sel = if save_after {
            None
        } else {
            self.editor_sel_chars
                .map(|(s, e)| lsp::char_span_to_range(&self.editor.buffers[i].text, s, e))
        };
        let buf_id = self.editor.buffers[i].id;
        let Some(c) = self.lsp.get(&key) else {
            return false;
        };
        let caps = c.caps();
        let sent = match sel {
            Some(range) if caps.range_formatting => {
                c.request_range_formatting(&path, &range, &opts).is_sent()
            }
            _ if caps.formatting => c.request_formatting(&path, &opts).is_sent(),
            _ => false,
        };
        if sent {
            self.lsp_format_buf = Some((buf_id, save_after));
        }
        sent
    }

    /// 「クイックフィックス」。キャレット行 (選択があればその範囲) の
    /// コードアクションを要求する。
    ///
    /// サーバーが codeAction 非対応なら**黙って何もしない**のではなく 1 行で
    /// 伝える (定義ジャンプと同じ流儀)。押しても無反応、が一番わかりにくい。
    pub(super) fn lsp_code_actions(&mut self) {
        let (Some((key, path)), Some(caret)) = (self.active_lsp_target(), self.caret_lsp_pos())
        else {
            self.toast_warn(tr("この言語の LSP サーバーが動いていません"));
            return;
        };
        let supported = self
            .lsp
            .get(&key)
            .map(|c| c.caps().code_action)
            .unwrap_or(false);
        if !supported {
            self.toast_warn(tr("この言語の LSP は クイックフィックスに対応していません"));
            return;
        }
        let Some(i) = self.editor.active else {
            return;
        };
        let sel = self.editor_sel_chars.map(|(s, e)| {
            let r = lsp::char_span_to_range(&self.editor.buffers[i].text, s, e);
            (r.start, r.end)
        });
        let range = lsp::action_range(&self.editor.buffers[i].text, sel, caret);
        let sent = match self.lsp.get(&key) {
            Some(c) => {
                // その範囲に重なる診断だけを context に載せる
                // (サーバーはこれを見て「その問題の修正」を絞り込む)
                let diags = c.diagnostics(&path);
                let picked = diags
                    .as_deref()
                    .map(|v| lsp::diagnostics_in_range(v, &range))
                    .unwrap_or_default();
                c.request_code_actions(&path, &range, &picked).is_sent()
            }
            None => false,
        };
        if sent {
            self.lsp_actions.clear();
            self.lsp_actions_sel = 0;
            self.lsp_actions_busy = true;
            self.lsp_actions_open = true;
            self.lsp_actions_key = Some(key);
            // 要求した時点のキャレット直下で固定する。追従させると候補を
            // 選んでいる間にポップアップが飛び回る。
            self.lsp_actions_anchor = self.caret_screen;
        }
    }

    /// 選んだクイックフィックスを 1 件適用する。
    ///
    /// WorkspaceEdit を持つものは既存の適用経路 (`apply_workspace_edit`) を
    /// そのまま通す (未保存で残す方針も共通)。command 形式は
    /// `workspace/executeCommand` をサーバーへ送る — 本クライアントは
    /// `workspace/applyEdit` を受けないので、サーバー内で完結するコマンド専用。
    pub(super) fn apply_code_action(&mut self, n: usize) {
        let Some(a) = self.lsp_actions.get(n).cloned() else {
            return;
        };
        self.lsp_actions_open = false;
        self.lsp_actions.clear();
        if !lsp::action_is_actionable(&a) {
            self.toast_warn(tr(
                "このアクションはサーバー側での解決が必要で、まだ適用できません",
            ));
            return;
        }
        if !a.edit.is_empty() {
            self.apply_workspace_edit(a.edit.clone());
        }
        let Some(cmd) = a.command.as_ref() else {
            return;
        };
        let key = self.lsp_actions_key.clone();
        let st = match key.as_ref().and_then(|k| self.lsp.get(k)) {
            Some(c) => c.execute_command(cmd),
            None => lsp::RequestStatus::Dead,
        };
        let title = lsp::one_line_label(&a.title, lsp::ACTION_TITLE_MAX);
        if st.is_sent() {
            self.toast(
                trf("🛠 {title} をサーバーへ依頼しました", &[("title", title)]),
                true,
            );
        } else {
            self.toast_warn(tr("このアクションをサーバーへ送れませんでした"));
        }
    }

    /// 「引数ヒント」の手動要求 (⇧⌘Space / パレット)。
    /// 自動トリガ ('(' や ',' の直後) は `lsp_completion_tick` 側で撃つ。
    pub(super) fn lsp_signature_help(&mut self) {
        let (Some((key, path)), Some(pos)) = (self.active_lsp_target(), self.caret_lsp_pos())
        else {
            self.toast_warn(tr("この言語の LSP サーバーが動いていません"));
            return;
        };
        let supported = self
            .lsp
            .get(&key)
            .map(|c| c.caps().signature_help)
            .unwrap_or(false);
        if !supported {
            self.toast_warn(tr("この言語の LSP は 引数ヒントに対応していません"));
            return;
        }
        if let Some(c) = self.lsp.get(&key) {
            c.request_signature_help(&path, pos);
        }
    }

    /// documentHighlight の結果を本文の char スパンへ焼き直す。
    /// **応答が来たときとバッファが変わったときだけ**呼ぶ (毎フレームは走査しない)。
    pub(super) fn refresh_highlight_spans(&mut self) {
        let Some(i) = self.editor.active else {
            self.lsp_highlight_spans.clear();
            self.lsp_highlight_buf = None;
            return;
        };
        let (id, spans) = {
            let b = &self.editor.buffers[i];
            (
                b.id,
                lsp::highlight_char_spans(&b.text, self.lsp_highlight.shown()),
            )
        };
        self.lsp_highlight_spans = spans;
        self.lsp_highlight_buf = Some(id);
    }

    /// 同一シンボルのハイライトを全部捨てる (本文編集・タブ切替・機能 OFF)。
    pub(super) fn clear_highlight_spans(&mut self) {
        self.lsp_highlight.clear();
        self.lsp_highlight_spans.clear();
        self.lsp_highlight_buf = None;
    }

    /// 補完候補を確定して本文へ差し込む (`additionalTextEdits` も一緒に当てる)。
    /// 実際に本文が変わったら true。
    pub(super) fn accept_completion(&mut self) -> bool {
        let Some(i) = self.editor.active else {
            return false;
        };
        let Some(pos) = self.caret_lsp_pos() else {
            return false;
        };
        // サーバーが textEdit を寄こさなかったときの既定の置換範囲 =
        // 「キャレット直前の識別子」〜キャレット
        let word_u16 = self.word_before_caret().encode_utf16().count();
        let fallback = lsp::Range::new(
            lsp::Position::new(pos.line, pos.character.saturating_sub(word_u16)),
            pos,
        );
        let Some(edits) = self.lsp_completion.accept(fallback) else {
            return false;
        };
        self.lsp_completion.dismiss();
        if edits.is_empty() {
            return false;
        }
        let before = self.editor.buffers[i].text.clone();
        let after = lsp::apply_text_edits(&before, &edits);
        if after == before {
            return false;
        }
        // キャレットは「キャレット行の編集」の直後へ。手前に当たった
        // additionalTextEdits (import 追加など) のぶんだけずらす。
        let main = edits
            .iter()
            .find(|e| e.range.start.line == pos.line)
            .cloned();
        if let Some(m) = main {
            let mut shift: isize = 0;
            for e in &edits {
                let earlier = (e.range.start.line, e.range.start.character)
                    < (m.range.start.line, m.range.start.character);
                if earlier {
                    let (bs, be) = lsp::range_to_byte_span(&before, &e.range);
                    shift += e.new_text.chars().count() as isize
                        - before[bs..be].chars().count() as isize;
                }
            }
            let bs = lsp::lsp_pos_to_byte_index(&before, m.range.start);
            let base = before[..bs].chars().count() as isize + shift;
            let ci = (base.max(0) as usize) + m.new_text.chars().count();
            self.pending_select = Some((ci, ci));
        }
        // 補完の確定は additionalTextEdits も含めて**まとめて 1 段**
        let ed = self.edit_step();
        self.editor.buffers[i].apply_edit(after, ed);
        self.fold_view = None;
        self.queue_lsp_change(i);
        true
    }

    /// rename が返した WorkspaceEdit を適用する。
    ///
    /// 開いていないファイルもタブとして開いてから書き換え、**未保存のまま**
    /// 残す。勝手にディスクへ書かないのはこのエディタの一貫した方針
    /// (取り消しはタブを閉じるだけで済む)。ファイルの作成 / 削除 / 改名を
    /// 含む編集は適用せずに知らせる。
    pub(super) fn apply_workspace_edit(&mut self, plan: lsp::WorkspaceEditPlan) {
        if plan.is_empty() {
            self.toast_warn(tr("変更はありませんでした"));
            return;
        }
        let total = plan.edit_count();
        let mut files = 0usize;
        let mut failed: Vec<String> = Vec::new();
        for fe in &plan.files {
            let idx = match self
                .editor
                .buffers
                .iter()
                .position(|b| b.path.as_deref() == Some(fe.path.as_path()))
            {
                Some(i) => Some(i),
                None => match self.editor.open(&fe.path, self.highlighter) {
                    Ok(_) => self.editor.active,
                    Err(e) => {
                        failed.push(e);
                        None
                    }
                },
            };
            let Some(i) = idx else { continue };
            let ed = self.edit_step();
            let b = &mut self.editor.buffers[i];
            let next = lsp::apply_file_edits(&b.text, fe);
            // ファイル 1 本ぶんの rename は 1 段 (⌘Z 1 回で丸ごと戻る)
            if b.apply_edit(next, ed) {
                files += 1;
            }
            self.queue_lsp_change(i);
        }
        self.fold_view = None;
        for e in failed {
            self.toast_warn(e);
        }
        if plan.has_resource_ops {
            self.toast_warn(tr(
                "ファイルの作成 / 削除 / 改名を含む変更は適用していません",
            ));
        }
        self.toast(
            trf(
                "✏ {files} ファイル / {edits} 箇所を書き換えました (未保存)",
                &[("files", files.to_string()), ("edits", total.to_string())],
            ),
            true,
        );
    }

    /// LSP の応答を毎フレーム回収する。
    ///
    /// `sweep_timeouts` は**必ず毎フレーム**呼ぶ。返らないリクエストの席を
    /// 空けないと、以降の要求が「飛行中」のまま詰まって黙って効かなくなる。
    pub(super) fn poll_lsp(&mut self) {
        let mut completion: Option<lsp::CompletionList> = None;
        let mut hover: Option<lsp::HoverInfo> = None;
        let mut refs: Option<Vec<lsp::ReferenceGroup>> = None;
        let mut syms: Option<Vec<lsp::SymbolNode>> = None;
        let mut prepare: Option<Option<lsp::Range>> = None;
        let mut plan: Option<lsp::WorkspaceEditPlan> = None;
        let mut fmt: Option<Vec<lsp::TextEdit>> = None;
        let mut actions: Option<Vec<lsp::CodeAction>> = None;
        let mut signature: Option<lsp::SignatureHelp> = None;
        let mut highlights: Option<Vec<lsp::DocumentHighlight>> = None;
        for c in self.lsp.values() {
            c.sweep_timeouts(lsp::REQUEST_TIMEOUT);
            if let Some(v) = c.poll_completion() {
                completion = Some(v);
            }
            if let Some(v) = c.poll_hover() {
                hover = Some(v);
            }
            if let Some(v) = c.poll_references() {
                refs = Some(v);
            }
            if let Some(v) = c.poll_document_symbols() {
                syms = Some(v);
            }
            if let Some(v) = c.poll_prepare_rename() {
                prepare = Some(v);
            }
            if let Some(v) = c.poll_rename() {
                plan = Some(v);
            }
            if let Some(v) = c.poll_formatting() {
                fmt = Some(v);
            }
            if let Some(v) = c.poll_code_actions() {
                actions = Some(v);
            }
            if let Some(v) = c.poll_signature_help() {
                signature = Some(v);
            }
            if let Some(v) = c.poll_document_highlight() {
                highlights = Some(v);
            }
        }

        if let (Some(list), Some(id)) = (completion, self.lsp_completion.in_flight()) {
            self.lsp_completion.apply_response(id, list);
            if self.lsp_completion.is_open() {
                let w = self.word_before_caret();
                self.lsp_completion.set_filter(&w);
            }
        }
        if let Some(info) = hover {
            // HoverState は要求 ID で新旧を見分ける。飛行中でなければ捨てる。
            if let Some(id) = self.lsp_hover_flight {
                self.lsp_hover.apply_response(id, info);
            }
        }
        if let Some(groups) = refs {
            self.lsp_refs = groups;
            self.lsp_refs_busy = false;
            if self.lsp_refs.is_empty() {
                self.toast_warn(tr("参照は見つかりませんでした"));
                self.lsp_refs_open = false;
            }
        }
        if let Some(nodes) = syms {
            self.lsp_symbols = nodes;
            self.lsp_symbols_busy = false;
            if self.lsp_symbols.is_empty() {
                // ブレッドクラムの背景更新では黙る (ユーザーが頼んでいない)
                if !self.lsp_symbols_quiet {
                    self.toast_warn(tr("シンボルは見つかりませんでした"));
                }
                self.lsp_symbols_open = false;
            }
            self.lsp_symbols_quiet = false;
        }
        if let Some(range) = prepare {
            if let Some(f) = self.lsp_rename.as_mut() {
                f.preparing = false;
                match range {
                    Some(_) => {
                        f.open = true;
                        f.focus = true;
                    }
                    None => {
                        self.lsp_rename = None;
                        self.toast_warn(tr("ここではリネームできません"));
                    }
                }
            }
        }
        if let Some(p) = plan {
            if self.lsp_rename.take().is_some() {
                self.apply_workspace_edit(p);
            }
        }
        if let Some(edits) = fmt {
            self.apply_format_result(edits);
        }
        if let Some(list) = actions {
            self.lsp_actions = lsp::rank_code_actions(list);
            self.lsp_actions_busy = false;
            self.lsp_actions_sel = 0;
            if self.lsp_actions.is_empty() {
                // 空のポップアップを開いたままにしない (空白は作らない)
                self.lsp_actions_open = false;
                self.toast_warn(tr("ここで使えるクイックフィックスはありません"));
            }
        }
        if let Some(help) = signature {
            // 中身が無い応答でポップアップを出さない (空の枠だけが残る)
            self.lsp_signature = (!help.signatures.is_empty()).then_some(help);
        }
        if let Some(hl) = highlights {
            if let Some(id) = self.lsp_highlight.in_flight() {
                if self.lsp_highlight.apply_response(id, hl) {
                    self.refresh_highlight_spans();
                }
            }
        }
    }

    /// 整形の結果を本文へ当てる (必要なら続けて保存する)。
    pub(super) fn apply_format_result(&mut self, edits: Vec<lsp::TextEdit>) {
        let Some((buf_id, save_after)) = self.lsp_format_buf.take() else {
            return;
        };
        let Some(i) = self.editor.buffers.iter().position(|b| b.id == buf_id) else {
            return;
        };
        if !edits.is_empty() {
            let before = self.editor.buffers[i].text.clone();
            let after = lsp::apply_text_edits(&before, &edits);
            // 整形で本文の長さが変わるので、キャレットは必ず付け替える。
            // 付け替えないと「保存したらカーソルが別の行へ飛んだ」になる
            // (行末の空白が削れる保存時クリーンアップと同じ事故)。
            let sel = self.pending_select.unwrap_or_else(|| {
                let (ln, col) = self.editor.cursor;
                let c =
                    editor_ops::char_index_at(&before, ln.saturating_sub(1), col.saturating_sub(1));
                (c, c)
            });
            let moved = (
                editor_ops::adjust_char_index_after_cleanup(&before, &after, sel.0),
                editor_ops::adjust_char_index_after_cleanup(&before, &after, sel.1),
            );
            // 整形はファイル全体でも**必ず 1 段**
            let ed = self.edit_step().with_sel_before(sel).to_sel(moved);
            if self.editor.buffers[i].apply_edit(after, ed) {
                self.pending_select = Some(moved);
                self.fold_view = None;
                self.queue_lsp_change(i);
            }
        }
        if save_after {
            if let Some(p) = self.editor.buffers[i].path.clone() {
                self.save_buffer_to(i, p);
            }
        } else if edits.is_empty() {
            self.toast(tr("整形の必要はありませんでした"), true);
        } else {
            self.toast(tr("✏ 整形しました"), true);
        }
    }

    // ── E: LSP の UI (ポップアップ / 一覧 / リネーム入力) ─────────

    /// 補完のトリガ検出 → デバウンス → 要求、とポップアップのキー操作。
    ///
    /// **本文の TextEdit より先に**呼ぶこと。Enter / Tab / Esc / 矢印は
    /// ポップアップが開いている間だけ横取りする (閉じているときは素通し)。
    pub(super) fn lsp_completion_tick(&mut self, ctx: &egui::Context) {
        let Some(i) = self.editor.active else {
            self.lsp_completion.dismiss();
            self.lsp_hover.dismiss();
            self.lsp_signature = None;
            self.clear_highlight_spans();
            return;
        };
        let buf_id = self.editor.buffers[i].id;
        let ed_id = buf_edit_id(self.cur_pane, buf_id);
        let focused = ctx.memory(|m| m.has_focus(ed_id));

        // 開いているタブが変わったら候補は捨てる (別ファイルの候補を出さない)
        if self.lsp_completion_buf != Some(buf_id) {
            self.lsp_completion.dismiss();
            self.lsp_hover.dismiss();
            self.lsp_signature = None;
            self.lsp_actions_open = false;
            self.lsp_actions.clear();
            self.clear_highlight_spans();
            self.lsp_completion_buf = Some(buf_id);
        }

        // ポップアップのキー操作 (開いているときだけ横取りする)
        if self.lsp_completion.is_open() {
            let (esc, up, down, accept) = ctx.input_mut(|inp| {
                (
                    inp.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                    inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                    inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                    inp.consume_key(egui::Modifiers::NONE, egui::Key::Enter)
                        || inp.consume_key(egui::Modifiers::NONE, egui::Key::Tab),
                )
            });
            if esc {
                self.lsp_completion.dismiss();
            } else if up {
                self.lsp_completion.select_prev();
            } else if down {
                self.lsp_completion.select_next();
            } else if accept {
                self.accept_completion();
            }
        }

        if !focused {
            if self.lsp_completion.is_open() {
                self.lsp_completion.dismiss();
            }
            // 引数ヒントはキャレット基準のオーバーレイなので、本文から
            // フォーカスが外れたら残さない (どこの引数か分からなくなる)。
            self.lsp_signature = None;
            return;
        }

        let Some((key, path)) = self.active_lsp_target() else {
            return;
        };
        let caps = match self.lsp.get(&key) {
            Some(c) => c.caps(),
            None => return,
        };
        let now = Instant::now();

        // 打鍵の検出 (Text イベント = 実際に本文へ入った文字)。
        // 補完とシグネチャの両方が見るので、能力ゲートの外で 1 回だけ集める。
        let (typed, backspace) = ctx.input(|inp| {
            let mut typed: Vec<char> = Vec::new();
            for e in &inp.events {
                if let egui::Event::Text(s) = e {
                    typed.extend(s.chars());
                }
            }
            (typed, inp.key_pressed(egui::Key::Backspace))
        });

        if caps.completion {
            for ch in &typed {
                self.lsp_completion.on_typed(*ch, &caps, now);
            }
            if backspace {
                self.lsp_completion.on_backspace(now);
            }
            // 明示的に呼び出す。既定は ⌘I (VS Code mac の第 2 割り当て)。
            // ⌃Space は macOS が「前の入力ソース」に予約していてアプリまで
            // 届かないため既定から外したが、届く環境では拾えるよう残してある。
            let invoke = ctx.input_mut(|inp| {
                crate::keybinds::consume_shortcut_compat(
                    inp,
                    self.keys.get(BindAction::LspCompletion),
                ) || crate::keybinds::consume_shortcut_compat(
                    inp,
                    egui::KeyboardShortcut::new(egui::Modifiers::CTRL, egui::Key::Space),
                )
            });
            if invoke {
                let w = self.word_before_caret();
                self.lsp_completion.invoke(&w, now);
            }
            if self.lsp_completion.is_open() {
                let w = self.word_before_caret();
                self.lsp_completion.set_filter(&w);
            }
            if let Some(trigger) = self
                .lsp_completion
                .due_request(now, lsp::COMPLETION_DEBOUNCE)
            {
                if let Some(pos) = self.caret_lsp_pos() {
                    let ch = match trigger {
                        lsp::CompletionTrigger::TriggerChar(c) => Some(c),
                        lsp::CompletionTrigger::Invoked => None,
                    };
                    if let Some(c) = self.lsp.get(&key) {
                        let st = c.request_completion_at(&path, pos, ch);
                        self.lsp_completion.mark_sent(st, pos);
                    }
                }
            }
        }

        // ホバー: マウスが本文の上で静止したら要求する
        if caps.hover {
            if let Some(pos) = self.hover_doc_pos.take() {
                self.lsp_hover.on_move(pos, now);
            }
            if let Some(pos) = self.lsp_hover.due_request(now, lsp::HOVER_DEBOUNCE) {
                if let Some(c) = self.lsp.get(&key) {
                    let st = c.request_hover_at(&path, pos);
                    self.lsp_hover_flight = st.id();
                    self.lsp_hover.mark_sent(st);
                }
            }
        }

        // 引数ヒント: '(' や ',' を打った直後に出し、')' と Esc で閉じる。
        // **既存レイアウトは押しのけない** — キャレット近傍のオーバーレイだけ。
        if caps.signature_help {
            let mut want = false;
            for ch in &typed {
                if *ch == '(' || *ch == ',' || caps.signature_trigger_chars.contains(ch) {
                    want = true;
                }
                if *ch == ')' {
                    self.lsp_signature = None;
                    want = false;
                }
            }
            // Esc で閉じる。補完ポップアップが開いているときは上で吸われている
            // ので、ここへ来るのは「シグネチャだけが出ている」ときだけ。
            if self.lsp_signature.is_some()
                && ctx.input_mut(|inp| inp.consume_key(egui::Modifiers::NONE, egui::Key::Escape))
            {
                self.lsp_signature = None;
            }
            if want {
                let sent = match (self.caret_lsp_pos(), self.lsp.get(&key)) {
                    (Some(pos), Some(c)) => c.request_signature_help(&path, pos).is_sent(),
                    _ => false,
                };
                if !sent {
                    self.lsp_signature = None;
                }
            }
        } else if self.lsp_signature.is_some() {
            self.lsp_signature = None;
        }

        // 同一シンボルのハイライト: キャレットが止まってから 1 回だけ要求する。
        // 毎フレームは撃たない (設計原則 3: アイドル時のコストはゼロ)。
        if caps.document_highlight && self.lsp_highlight_on {
            if let Some(pos) = self.caret_lsp_pos() {
                self.lsp_highlight.on_move(pos, now);
            }
            if let Some(pos) = self.lsp_highlight.due_request(now, lsp::HIGHLIGHT_DEBOUNCE) {
                let st = self
                    .lsp
                    .get(&key)
                    .map(|c| c.request_document_highlight(&path, pos));
                if let Some(st) = st {
                    self.lsp_highlight.mark_sent(st);
                }
            }
            // デバウンス満了の瞬間に 1 回だけ起こす。予定が無ければ何も予約
            // しないので、放っておけば再描画は止まる (常時アニメーションにしない)。
            if let Some(after) = self.lsp_highlight.due_in(now, lsp::HIGHLIGHT_DEBOUNCE) {
                crate::perf::repaint_after(ctx, after, "lsp_completion_tick");
            }
        } else if !self.lsp_highlight_spans.is_empty() {
            self.clear_highlight_spans();
        }
    }

    /// クイックフィックスのポップアップのキー操作。
    ///
    /// **本文の TextEdit より先に**呼ぶこと (Enter / Esc / 矢印を先に横取りする)。
    /// 開いていないときは何も消費しないので、通常の編集は素通しする。
    pub(super) fn lsp_actions_tick(&mut self, ctx: &egui::Context) {
        if !self.lsp_actions_open {
            return;
        }
        let (esc, up, down, accept) = ctx.input_mut(|inp| {
            (
                inp.consume_key(egui::Modifiers::NONE, egui::Key::Escape),
                inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowUp),
                inp.consume_key(egui::Modifiers::NONE, egui::Key::ArrowDown),
                inp.consume_key(egui::Modifiers::NONE, egui::Key::Enter),
            )
        });
        if esc {
            self.lsp_actions_open = false;
            self.lsp_actions.clear();
            return;
        }
        let n = self.lsp_actions.len();
        if n == 0 {
            return;
        }
        if up {
            self.lsp_actions_sel = (self.lsp_actions_sel + n - 1) % n;
        }
        if down {
            self.lsp_actions_sel = (self.lsp_actions_sel + 1) % n;
        }
        if accept {
            self.apply_code_action(self.lsp_actions_sel);
        }
    }

    /// 補完ポップアップ。キャレットの下に候補一覧を出す。
    pub(super) fn lsp_completion_popup(&mut self, ctx: &egui::Context) {
        if !self.lsp_completion.is_open() || self.lsp_completion.is_empty() {
            return;
        }
        let Some(anchor) = self.caret_screen else {
            return;
        };
        let theme = self.theme.clone();
        let sel = self.lsp_completion.selected_index();
        let items: Vec<(String, String, String, bool, bool)> = self
            .lsp_completion
            .visible()
            .iter()
            .take(MAX_COMPLETION_ROWS)
            .map(|it| {
                (
                    completion_kind_label(it.kind).to_string(),
                    it.label.clone(),
                    it.detail.clone(),
                    it.deprecated,
                    !it.additional_text_edits.is_empty(),
                )
            })
            .collect();
        let mut clicked: Option<usize> = None;
        egui::Area::new(egui::Id::new("zv-lsp-completion"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor)
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(4.0))
                    .show(ui, |ui| {
                        ui.set_max_width(COMPLETION_POPUP_W);
                        egui::ScrollArea::vertical()
                            .max_height(COMPLETION_POPUP_H)
                            .show(ui, |ui| {
                                for (n, (kind, label, detail, deprecated, extra)) in
                                    items.iter().enumerate()
                                {
                                    let on = n == sel;
                                    let mut job = egui::text::LayoutJob::default();
                                    let dim = egui::TextFormat {
                                        color: theme.text_dim,
                                        ..Default::default()
                                    };
                                    let strong = egui::TextFormat {
                                        color: if *deprecated {
                                            theme.text_dim
                                        } else {
                                            theme.text
                                        },
                                        ..Default::default()
                                    };
                                    job.append(&format!("{kind:<5} "), 0.0, dim.clone());
                                    job.append(label, 0.0, strong);
                                    if *extra {
                                        job.append(" +", 0.0, dim.clone());
                                    }
                                    if !detail.is_empty() {
                                        job.append(&format!("  {detail}"), 0.0, dim);
                                    }
                                    if ui.selectable_label(on, job).clicked() {
                                        clicked = Some(n);
                                    }
                                }
                            });
                    });
            });
        if let Some(n) = clicked {
            while self.lsp_completion.selected_index() < n {
                self.lsp_completion.select_next();
            }
            while self.lsp_completion.selected_index() > n {
                self.lsp_completion.select_prev();
            }
            self.accept_completion();
        }
    }

    /// ホバーポップアップ。本文は markdown なので既存のレンダラで描く。
    /// 診断のホバー。波線の上にマウスがあるフレームだけ出す。
    ///
    /// [`Self::lsp_hover_popup`] とは**排他** — 本文描画側で診断が当たった
    /// フレームは LSP ホバーを dismiss しているので、2 枚重なることはない。
    pub(super) fn diag_hover_popup(&mut self, ctx: &egui::Context) {
        // take: 本文を描いていないフレーム (別ビューへ切り替えた等) で
        // ツールチップが取り残されないように、1 フレームで使い切る。
        // ホバーが続いていれば `code_editor_ui` が毎フレーム入れ直す。
        let Some((msg, sev, at)) = self.diag_hover.take() else {
            return;
        };
        let color = diagview::severity_color(&self.theme, sev);
        let (panel, text) = (self.theme.panel, self.theme.text);
        egui::Area::new(egui::Id::new("zv-diag-hover"))
            .order(egui::Order::Tooltip)
            .fixed_pos(at + egui::vec2(0.0, HOVER_OFFSET_Y))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(panel)
                    .stroke(egui::Stroke::new(1.0_f32, color))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| {
                        // 幅は LSP ホバーと同じ上限。長文は折り返して収める。
                        ui.set_max_width(HOVER_POPUP_W);
                        ui.horizontal_top(|ui| {
                            ui.label(RichText::new("\u{25cf}").color(color));
                            ui.label(RichText::new(msg).color(text));
                        });
                    });
            });
    }

    pub(super) fn lsp_hover_popup(&mut self, ctx: &egui::Context) {
        let Some(info) = self.lsp_hover.shown() else {
            return;
        };
        let Some(at) = self.lsp_hover_pos else {
            return;
        };
        let body = info.contents.clone();
        let theme = self.theme.clone();
        let base = self.scaled_editor_font();
        let mut images = std::mem::take(&mut self.md_images);
        let hl = self.highlighter;
        egui::Area::new(egui::Id::new("zv-lsp-hover"))
            .order(egui::Order::Tooltip)
            .fixed_pos(at + egui::vec2(0.0, HOVER_OFFSET_Y))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(8.0))
                    .show(ui, |ui| {
                        ui.set_max_width(HOVER_POPUP_W);
                        egui::ScrollArea::vertical()
                            .max_height(HOVER_POPUP_H)
                            .show(ui, |ui| {
                                let mut rctx = markdown::RenderCtx {
                                    dir: None,
                                    images: &mut images,
                                };
                                markdown::render(ui, &theme, hl, base, &body, &mut rctx);
                            });
                    });
            });
        self.md_images = images;
    }

    /// クイックフィックスのポップアップ。キャレット直下に小さく出す。
    ///
    /// 中央ビューは奪わず [`egui::Area`] で重ねるだけ (画面が突然変わらない)。
    /// タイトルは `lsp::one_line_label` で先に 1 行へ畳んであるので、
    /// 幅がいくつでも行が見切れない。全文はホバーで出す。
    pub(super) fn lsp_code_actions_popup(&mut self, ctx: &egui::Context) {
        if !self.lsp_actions_open {
            return;
        }
        if self.lsp_actions.is_empty() && !self.lsp_actions_busy {
            // 中身も見込みも無いなら枠ごと消す (空白は作らない)
            self.lsp_actions_open = false;
            return;
        }
        let theme = self.theme.clone();
        let anchor = self
            .lsp_actions_anchor
            .or(self.caret_screen)
            .unwrap_or_else(|| ctx.screen_rect().center());
        let busy = self.lsp_actions_busy;
        let sel = self.lsp_actions_sel;
        // (1 行に畳んだ表示, 全文, 押せるか)
        let rows: Vec<(String, String, bool)> = self
            .lsp_actions
            .iter()
            .map(|a| {
                (
                    lsp::one_line_label(&a.title, lsp::ACTION_TITLE_MAX),
                    a.title.clone(),
                    lsp::action_is_actionable(a),
                )
            })
            .collect();
        let mut clicked: Option<usize> = None;
        let area = egui::Area::new(egui::Id::new("zv-lsp-code-actions"))
            .order(egui::Order::Foreground)
            .fixed_pos(anchor + egui::vec2(0.0, HOVER_OFFSET_Y))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(6.0))
                    .show(ui, |ui| {
                        ui.set_max_width(ACTION_POPUP_W);
                        ui.label(
                            RichText::new(tr("💡 クイックフィックス"))
                                .size(11.0)
                                .color(theme.text_dim),
                        );
                        if rows.is_empty() && busy {
                            ui.label(
                                RichText::new(tr("候補を問い合わせています…"))
                                    .color(theme.text_dim),
                            );
                            return;
                        }
                        egui::ScrollArea::vertical()
                            .id_salt("zv-code-actions")
                            .max_height(ACTION_POPUP_H)
                            .show(ui, |ui| {
                                for (n, (short, full, ok)) in rows.iter().enumerate() {
                                    let txt = RichText::new(short).color(if *ok {
                                        theme.text
                                    } else {
                                        theme.text_dim
                                    });
                                    let mut r = ui.selectable_label(n == sel, txt);
                                    if short != full {
                                        r = r.on_hover_text(full);
                                    }
                                    if r.clicked() {
                                        clicked = Some(n);
                                    }
                                }
                            });
                    });
            });
        if let Some(n) = clicked {
            self.apply_code_action(n);
            return;
        }
        // ポップアップの外を押したら閉じる。閉じないと `lsp_actions_tick` が
        // Enter / Esc / 矢印を横取りし続けて、本文が打てなくなる。
        let rect = area.response.rect;
        let outside = ctx.input(|i| {
            i.pointer.any_pressed()
                && i.pointer
                    .interact_pos()
                    .map(|p| !rect.contains(p))
                    .unwrap_or(false)
        });
        if outside {
            self.lsp_actions_open = false;
            self.lsp_actions.clear();
        }
    }

    /// 引数ヒント (シグネチャ) のポップアップ。
    ///
    /// キャレット近傍のオーバーレイに 1 行だけ出す。補完ポップアップが開いて
    /// いる間は出さない — 同じ場所に 2 枚重ねると、どちらも読めなくなる。
    pub(super) fn lsp_signature_popup(&mut self, ctx: &egui::Context) {
        if self.lsp_completion.is_open() {
            return;
        }
        let Some(help) = self.lsp_signature.as_ref() else {
            return;
        };
        let Some(d) = lsp::signature_display(help, SIGNATURE_DOC_MAX) else {
            return;
        };
        let Some(at) = self.caret_screen else {
            return;
        };
        let theme = self.theme.clone();
        egui::Area::new(egui::Id::new("zv-lsp-signature"))
            .order(egui::Order::Tooltip)
            .fixed_pos(at + egui::vec2(0.0, HOVER_OFFSET_Y))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(1.0_f32, theme.border))
                    .rounding(egui::Rounding::same(4.0))
                    .inner_margin(egui::Margin::same(6.0))
                    .show(ui, |ui| {
                        ui.set_max_width(SIGNATURE_POPUP_W);
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing.x = 4.0;
                            if d.total > 1 {
                                ui.label(
                                    RichText::new(format!("{}/{}", d.index, d.total))
                                        .size(11.0)
                                        .color(theme.text_dim),
                                );
                            }
                            ui.label(RichText::new(&d.label).color(theme.text));
                            if !d.active_param.is_empty() {
                                ui.label(
                                    RichText::new(&d.active_param).strong().color(theme.accent),
                                );
                            }
                        });
                        if !d.doc.is_empty() {
                            ui.label(RichText::new(&d.doc).size(11.5).color(theme.text_dim));
                        }
                    });
            });
    }

    /// 「参照を検索」の結果一覧。行をクリックするとそこへ飛ぶ。
    pub(super) fn lsp_refs_window(&mut self, ctx: &egui::Context) {
        if !self.lsp_refs_open {
            return;
        }
        let theme = self.theme.clone();
        let root = self.primary_root().to_path_buf();
        let busy = self.lsp_refs_busy;
        let groups: Vec<(PathBuf, Vec<lsp::Range>)> = self
            .lsp_refs
            .iter()
            .map(|g| (g.path.clone(), g.locations.clone()))
            .collect();
        let total: usize = groups.iter().map(|(_, l)| l.len()).sum();
        let mut open = true;
        let mut jump: Option<(PathBuf, usize, usize)> = None;
        egui::Window::new(tr("参照を検索"))
            .open(&mut open)
            .default_width(REF_WINDOW_W)
            .show(ctx, |ui| {
                if busy {
                    ui.label(RichText::new(tr("検索中…")).color(theme.text_dim).small());
                    return;
                }
                ui.label(
                    RichText::new(trf("{n} 件", &[("n", total.to_string())]))
                        .color(theme.text_dim)
                        .small(),
                );
                egui::ScrollArea::vertical()
                    .max_height(REF_WINDOW_H)
                    .show(ui, |ui| {
                        for (path, locs) in &groups {
                            let rel = path.strip_prefix(&root).unwrap_or(path);
                            ui.label(
                                RichText::new(rel.display().to_string())
                                    .color(theme.accent)
                                    .small(),
                            );
                            for r in locs {
                                let label = trf(
                                    "  {line} 行 {col} 桁",
                                    &[
                                        ("line", (r.start.line + 1).to_string()),
                                        ("col", (r.start.character + 1).to_string()),
                                    ],
                                );
                                if ui
                                    .selectable_label(false, RichText::new(label).small())
                                    .clicked()
                                {
                                    jump = Some((path.clone(), r.start.line, r.start.character));
                                }
                            }
                        }
                    });
            });
        self.lsp_refs_open = open;
        if let Some((p, l, c)) = jump {
            self.jump_to_lsp_pos(&p, l, c);
        }
    }

    /// 「シンボルにジャンプ」。quick-open 風の絞り込み一覧。
    pub(super) fn lsp_symbols_window(&mut self, ctx: &egui::Context) {
        if !self.lsp_symbols_open {
            return;
        }
        let theme = self.theme.clone();
        let busy = self.lsp_symbols_busy;
        let path = self.lsp_symbols_path.clone();
        let mut flat: Vec<(usize, String, u8, lsp::Position)> = Vec::new();
        flatten_symbols(&self.lsp_symbols, 0, &mut flat);
        let mut query = std::mem::take(&mut self.lsp_symbols_query);
        let mut open = true;
        let mut jump: Option<lsp::Position> = None;
        egui::Window::new(tr("シンボルにジャンプ"))
            .open(&mut open)
            .default_width(REF_WINDOW_W)
            .show(ctx, |ui| {
                if busy {
                    ui.label(
                        RichText::new(tr("読み込み中…"))
                            .color(theme.text_dim)
                            .small(),
                    );
                    return;
                }
                ui.text_edit_singleline(&mut query);
                let pq = fuzzy::PreparedQuery::new(query.trim());
                let mut rows: Vec<(i32, usize)> = flat
                    .iter()
                    .enumerate()
                    .filter_map(|(n, (_, name, _, _))| pq.score(name).map(|s| (s, n)))
                    .collect();
                rows.sort_by(|a, b| b.0.cmp(&a.0));
                egui::ScrollArea::vertical()
                    .max_height(REF_WINDOW_H)
                    .show(ui, |ui| {
                        for (_, n) in rows.iter().take(MAX_SYMBOL_ROWS) {
                            let (depth, name, kind, pos) = &flat[*n];
                            let label = format!(
                                "{}{:<7}{}",
                                "  ".repeat(*depth),
                                symbol_kind_label(*kind),
                                name
                            );
                            if ui
                                .selectable_label(false, RichText::new(label).small())
                                .clicked()
                            {
                                jump = Some(*pos);
                            }
                        }
                    });
            });
        self.lsp_symbols_query = query;
        self.lsp_symbols_open = open;
        if let (Some(p), Some(pos)) = (path, jump) {
            self.jump_to_lsp_pos(&p, pos.line, pos.character);
            self.lsp_symbols_open = false;
        }
    }

    /// リネームの名前入力。Enter で確定、Esc で取り消し。
    pub(super) fn lsp_rename_window(&mut self, ctx: &egui::Context) {
        let Some(f) = self.lsp_rename.as_ref() else {
            return;
        };
        if f.preparing {
            return;
        }
        if !f.open {
            return;
        }
        let theme = self.theme.clone();
        let applying = f.applying;
        let mut name = f.name.clone();
        let focus = f.focus;
        let mut cancel = false;
        let mut submit = false;
        egui::Window::new(tr("リネーム"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_TOP, egui::vec2(0.0, RENAME_WINDOW_Y))
            .show(ctx, |ui| {
                if applying {
                    ui.label(RichText::new(tr("適用中…")).color(theme.text_dim).small());
                    return;
                }
                let r = ui.text_edit_singleline(&mut name);
                if focus {
                    r.request_focus();
                }
                if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                    submit = true;
                }
                ui.horizontal(|ui| {
                    if ui.button(tr("変更")).clicked() {
                        submit = true;
                    }
                    if ui.button(tr("取り消し")).clicked() {
                        cancel = true;
                    }
                });
            });
        if ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            cancel = true;
        }
        if cancel {
            self.lsp_rename = None;
            return;
        }
        let Some(f) = self.lsp_rename.as_mut() else {
            return;
        };
        f.name = name;
        f.focus = false;
        if !submit || f.applying {
            return;
        }
        let (key, path, pos, new_name) = (f.key.clone(), f.path.clone(), f.pos, f.name.clone());
        if new_name.trim().is_empty() {
            self.toast_warn(tr("新しい名前を入力してください"));
            return;
        }
        let sent = self
            .lsp
            .get(&key)
            .map(|c| c.request_rename(&path, pos, new_name.trim()).is_sent())
            .unwrap_or(false);
        if sent {
            if let Some(f) = self.lsp_rename.as_mut() {
                f.applying = true;
            }
        } else {
            self.lsp_rename = None;
            self.toast_warn(tr("リネームを要求できませんでした"));
        }
    }

    // ── D: テーブル表示 / 巨大ファイルの帯 ───────────────────────

    /// CSV / TSV を表として描く。ヘッダ行は上に固定する。
    ///
    /// `editor::parse_table` が返すのは**ラグド (行ごとに列数が違う)** な表なので、
    /// 足りないセルは空欄で埋めて列を揃える。打ち切られていたらその旨を出す。
    pub(super) fn table_view_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let theme = self.theme.clone();
        let mut close = false;
        let Some(t) = self.editor.buffers[i].table.as_ref() else {
            return;
        };
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(trf(
                    "📊 {rows} 行 × {cols} 列",
                    &[
                        ("rows", t.rows.len().to_string()),
                        ("cols", t.columns.to_string()),
                    ],
                ))
                .color(theme.text_dim)
                .small(),
            );
            if t.truncated {
                ui.label(
                    RichText::new(trf(
                        "⚠ 先頭 {n} 行だけ読み込んでいます",
                        &[("n", crate::editor::TABLE_MAX_ROWS.to_string())],
                    ))
                    .color(theme.warn)
                    .small(),
                );
            }
            if ui
                .button(RichText::new(tr("テキストとして表示")).small())
                .clicked()
            {
                close = true;
            }
        });
        ui.separator();
        let cols = t.columns.max(1);
        let cell = |row: &[String], n: usize| -> String {
            let v = row.get(n).map(String::as_str).unwrap_or("");
            if v.chars().count() > TABLE_CELL_CHARS {
                let head: String = v.chars().take(TABLE_CELL_CHARS).collect();
                format!("{head}…")
            } else {
                v.to_string()
            }
        };
        egui::ScrollArea::both()
            .id_salt(("zv-table", self.editor.buffers[i].id))
            .auto_shrink(false)
            .show(ui, |ui| {
                egui::Grid::new("zv-table-grid")
                    .striped(true)
                    .show(ui, |ui| {
                        for n in 0..cols {
                            ui.label(
                                RichText::new(cell(&t.headers, n))
                                    .monospace()
                                    .strong()
                                    .color(theme.accent),
                            );
                        }
                        ui.end_row();
                        for row in &t.rows {
                            for n in 0..cols {
                                ui.label(RichText::new(cell(row, n)).monospace().color(theme.text));
                            }
                            ui.end_row();
                        }
                    });
            });
        if close {
            self.editor.buffers[i].drop_table();
        }
    }

    /// 巨大ファイルで効いている制限を本文の上に帯で出す。
    ///
    /// 文言は `trf` でここで組み立てる (editor.rs は意図的に旗だけ返す)。
    pub(super) fn large_file_banner_ui(&mut self, ui: &mut egui::Ui, bytes: u64) {
        let theme = self.theme.clone();
        let mb = bytes as f64 / (1024.0 * 1024.0);
        let Some(i) = self.editor.active else {
            return;
        };
        let ro = self.editor.buffers[i].read_only();
        let hl_off = !self.editor.buffers[i].highlight_enabled();
        let why = large_file_reasons(ro, hl_off);
        egui::Frame::none()
            .fill(theme.panel_alt)
            .stroke(egui::Stroke::new(1.0_f32, theme.warn))
            .inner_margin(egui::Margin::symmetric(8.0, 4.0))
            .show(ui, |ui| {
                ui.label(
                    RichText::new(trf(
                        "⚠ 大きなファイル ({size} MB): {why}",
                        &[("size", format!("{mb:.1}")), ("why", why.join(" / "))],
                    ))
                    .color(theme.warn)
                    .small(),
                );
            });
    }
}
