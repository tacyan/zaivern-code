use super::*;

impl ZaivernApp {
    pub(super) fn save_active(&mut self, as_new: bool) -> bool {
        self.save_active_with(as_new, false, false)
    }

    /// `save_active` に「保存後の追加動作」を添えた版。
    ///
    /// `close_after` / `run_hooks` は、保存先を尋ねるダイアログが**別スレッド**へ
    /// 回されたときのための控え。その場合このフレームでは何も保存していないので
    /// `false` を返し、結果が届いた時点で `apply_dialog_result` が同じ追加動作を行う。
    /// 同期でダイアログを開く macOS では従来どおり呼び出し元が追加動作をやるため、
    /// 二重には走らない (`request_dialog` の説明を参照)。
    pub(super) fn save_active_with(
        &mut self,
        as_new: bool,
        close_after: bool,
        run_hooks: bool,
    ) -> bool {
        let Some(i) = self.editor.active else {
            return false;
        };
        // PR 差分などの非ファイルタブは保存できない。ここで止めないと
        // 「名前を付けて保存」ダイアログが開いて差分がファイルとして書き出される。
        if self.editor.buffers[i].read_only() {
            self.toast(tr("このタブは読み取り専用です (保存できません)"), false);
            return false;
        }
        // 保存時の整形: LSP へ投げて、応答が届いてから本文へ当てて保存する
        // (UI スレッドは待たない)。整形要求が飛行中なら二重に投げない。
        // 「保存して閉じる」やフック付きの保存は先送りしない
        // (この関数の戻り値で追加動作を決めている呼び出し元があるため)。
        if self.format_on_save
            && !as_new
            && !close_after
            && !run_hooks
            && self.lsp_format_buf.is_none()
            && self.editor.buffers[i].path.is_some()
            && self.lsp_format_document(true)
        {
            return false;
        }
        let (need_dialog, cur_path, buffer_id) = {
            let b = &self.editor.buffers[i];
            (as_new || b.path.is_none(), b.path.clone(), b.id)
        };
        let path = if need_dialog {
            let dir = self.primary_root().to_path_buf();
            let purpose = DialogPurpose::SaveAs {
                buffer_id,
                close_after,
                run_hooks,
            };
            match self.request_dialog(purpose, DialogSpec::save_file().directory(dir)) {
                // その場で選ばれた (macOS の同期パス)
                Some(p) => p,
                // キャンセル、または別スレッドで進行中 — どちらも今は何もしない
                None => return false,
            }
        } else {
            cur_path.unwrap()
        };
        self.save_buffer_to(i, path)
    }

    /// 保存直前のクリーンアップ (末尾空白の除去 / 最終行の改行) を本文へかける。
    ///
    /// 本文が 1 バイトも変わらないときは何もしない (`changed == false` なら
    /// undo 積みもカーソル付け替えも丸ごと省ける)。改行コードには触らない —
    /// 揃えるのは明示的な「改行コードを変換」だけで、保存が勝手に全行を
    /// 書き換えて差分を爆発させることがないようにする。
    pub(super) fn apply_save_cleanup(&mut self, i: usize) {
        let opts = editor_ops::SaveCleanup {
            trim_trailing: self.save_trim_trailing,
            trim_final_newlines: self.save_trim_final_newlines,
            final_newline: self.save_final_newline,
            target_ending: None,
        };
        if opts.is_noop() {
            return;
        }
        let before = self.editor.buffers[i].text.clone();
        // 予約済みの選択があればそれを、無ければステータスバーと同じ (行, 桁) を使う
        // (editor.cursor は 1-based で持っている)。
        let sel = self.pending_select.unwrap_or_else(|| {
            let (ln, col) = self.editor.cursor;
            let c = editor_ops::char_index_at(&before, ln.saturating_sub(1), col.saturating_sub(1));
            (c, c)
        });
        let Some((cleaned, sel)) = save_cleanup_edit(&before, sel, &opts) else {
            return;
        };
        self.pending_select = Some(sel);
        // 保存時の掃除も取り消せる 1 段にする (VS Code の formatOnSave と同じ)
        let ed = self.edit_step().to_sel(sel);
        self.editor.buffers[i].apply_edit(cleaned, ed);
    }

    /// バッファ `i` を `path` へ書き出して後始末する。成功したら true。
    pub(super) fn save_buffer_to(&mut self, i: usize, path: PathBuf) -> bool {
        self.apply_save_cleanup(i);
        let text = self.editor.buffers[i].text.clone();
        // 読み込んだときの文字コードで書き戻す (CP932 のファイルを勝手に UTF-8 に
        // しない)。元の符号化で表せない文字が入っていたときだけ UTF-8 へ格上げし、
        // 黙って変えたことにならないよう知らせる。
        let was = self.editor.buffers[i].encoding;
        match self.editor.buffers[i].write_to(&path) {
            Ok(promoted) => {
                let lang = self.highlighter.lang_for(Some(&path), &text);
                let b = &mut self.editor.buffers[i];
                b.title = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "???".into());
                b.path = Some(path.clone());
                // 保存時点を履歴へ刻む (ここまで取り消したら未保存印が消える)
                b.mark_saved();
                b.lang = lang;
                b.cache = None;
                b.disk_mtime = disk_mtime(&path);
                b.conflict_notified = None;
                self.tree.invalidate();
                // 「保存」という**コマンド名**でローカルヒストリへ刻む。
                // 取り込みは遅延して裏で走る (連続保存で歩き回らない)。
                self.local_history.note(&tr("保存"));
                // 保存した本文の退避はもう要らない (ゴミを残さない)
                self.hotexit_flush();
                // ブックマークの追悼パスは 2 秒のデバウンスで走るので、
                // 保存 (= ここまでを確定する操作) の直前に 1 回流しておく。
                self.marks.flush_memorial(&path, &text);
                if promoted {
                    self.toast_warn(trf(
                        "💾 保存しました: {path}\n\u{3000}{from} では表せない文字があるため UTF-8 で保存しました",
                        &[
                            ("path", path.display().to_string()),
                            ("from", was.label()),
                        ],
                    ));
                } else {
                    self.toast(
                        trf(
                            "💾 保存しました: {path}",
                            &[("path", path.display().to_string())],
                        ),
                        true,
                    );
                }
                true
            }
            Err(e) => {
                self.toast(
                    trf("保存に失敗しました: {e}", &[("e", e.to_string())]),
                    false,
                );
                false
            }
        }
    }

    // ─── 符号化を指定して開き直す / 保存する (crate::textenc) ────────

    /// パレットから来た符号化コマンドを処理する。
    pub(super) fn apply_cmd_encoding(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::ReopenWithEncoding(None) => self.open_encoding_picker(false),
            Cmd::SaveWithEncoding(None) => self.open_encoding_picker(true),
            Cmd::ReopenWithEncoding(Some(id)) => self.reopen_with_encoding(&id),
            Cmd::SaveWithEncoding(Some(id)) => self.save_with_encoding(&id),
            _ => {}
        }
    }

    /// 符号化ピッカーを開く (`for_save` が真なら保存用)。
    pub(super) fn open_encoding_picker(&mut self, for_save: bool) {
        if self.editor.active.is_none() {
            self.toast(tr("先にファイルを開いてください"), false);
            return;
        }
        self.enc_picker = Some(for_save);
        self.enc_filter.clear();
    }

    /// **指定した符号化で開き直す**。自動判定は使わない。
    ///
    /// 化けた箇所があれば件数を添えて警告する — 黙って壊れた本文を見せない。
    pub(super) fn reopen_with_encoding(&mut self, id: &str) {
        let Some(i) = self.editor.active else {
            self.toast(tr("先にファイルを開いてください"), false);
            return;
        };
        let Some(enc) = crate::textenc::encoding_by_name(id) else {
            self.toast(
                trf(
                    "この環境では使えない符号化です: {id}",
                    &[("id", id.to_string())],
                ),
                false,
            );
            return;
        };
        let Some(path) = self.editor.buffers[i].path.clone() else {
            self.toast(tr("保存されていないタブは開き直せません"), false);
            return;
        };
        // 未保存の変更があるときは黙って捨てない (開き直し = ディスクの再読込)
        if self.editor.buffers[i].dirty() {
            self.toast(
                tr("未保存の変更があります — 保存するか元に戻してから開き直してください"),
                false,
            );
            return;
        }
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                self.toast(trf("読み込めません: {e}", &[("e", e.to_string())]), false);
                return;
            }
        };
        let rep = crate::textenc::reopen_with_report(&bytes, enc);
        let lossy = rep.lossy();
        let n = rep.replacements;
        let label = rep.format.label();
        {
            let b = &mut self.editor.buffers[i];
            // 別の符号化で**開き直した** = 取り消しで前の解釈へは戻さない
            b.reset_text(rep.text);
            b.encoding = rep.format.encoding;
            b.disk_mtime = disk_mtime(&path);
            b.conflict_notified = None;
        }
        self.multi_sel = None;
        self.column_anchor = None;
        self.le_cache = None;
        self.queue_lsp_change(i);
        if lossy {
            self.toast_warn(trf(
                "⚠ {label} で開き直しましたが {n} 箇所が化けています — 別の符号化を試してください",
                &[("label", label), ("n", n.to_string())],
            ));
        } else {
            self.toast(trf("{label} で開き直しました", &[("label", label)]), true);
        }
    }

    /// **指定した符号化で保存する**。表せない文字があれば保存せずに知らせ、
    /// その文字までキャレットを飛ばす (どこを直せばいいか分かる形で断る)。
    pub(super) fn save_with_encoding(&mut self, id: &str) {
        let Some(i) = self.editor.active else {
            self.toast(tr("先にファイルを開いてください"), false);
            return;
        };
        let Some(enc) = crate::textenc::encoding_by_name(id) else {
            self.toast(
                trf(
                    "この環境では使えない符号化です: {id}",
                    &[("id", id.to_string())],
                ),
                false,
            );
            return;
        };
        let Some(path) = self.editor.buffers[i].path.clone() else {
            self.toast(tr("先に名前を付けて保存してください"), false);
            return;
        };
        self.apply_save_cleanup(i);
        let text = self.editor.buffers[i].text.clone();
        let ending = crate::textenc::detect_line_ending(&text);
        match crate::textenc::save_with(&text, enc, ending) {
            Ok(bytes) => match std::fs::write(&path, &bytes) {
                Ok(()) => {
                    let b = &mut self.editor.buffers[i];
                    b.encoding = enc;
                    b.mark_saved();
                    b.disk_mtime = disk_mtime(&path);
                    b.conflict_notified = None;
                    self.tree.invalidate();
                    self.le_cache = None;
                    self.toast(
                        trf(
                            "💾 {enc} で保存しました: {path}",
                            &[("enc", enc.name()), ("path", path.display().to_string())],
                        ),
                        true,
                    );
                }
                Err(e) => self.toast(
                    trf("保存に失敗しました: {e}", &[("e", e.to_string())]),
                    false,
                ),
            },
            Err(issue) => {
                // 保存できない文字へキャレットを移す。char 添字なのでそのまま渡せる。
                if let Some(ix) = issue.char_index() {
                    self.pending_select = Some((ix, ix + 1));
                }
                self.toast_warn(issue.message());
            }
        }
    }

    /// 符号化ピッカーの小窓。**実測で使えるものだけ**を並べる。
    pub(super) fn encoding_picker_ui(&mut self, ctx: &egui::Context) {
        let Some(for_save) = self.enc_picker else {
            return;
        };
        let theme = self.theme.clone();
        let mut open = true;
        let mut chosen: Option<String> = None;
        let title = if for_save {
            tr("エンコーディングを指定して保存")
        } else {
            tr("エンコーディングを指定して開き直す")
        };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, egui::vec2(0.0, -40.0))
            .show(ctx, |ui| {
                ui.set_width(380.0);
                ui.label(
                    RichText::new(tr(
                        "この一覧は「この PC で本当に往復できる」符号化だけです (実測)",
                    ))
                    .size(11.5)
                    .color(theme.text_dim),
                );
                ui.add(
                    egui::TextEdit::singleline(&mut self.enc_filter)
                        .desired_width(f32::INFINITY)
                        .hint_text(tr("絞り込み (utf, sjis, cp932 …)")),
                );
                ui.separator();
                let q = self.enc_filter.trim().to_lowercase();
                egui::ScrollArea::vertical()
                    .id_salt("zv-enc-picker")
                    .max_height(320.0)
                    .show(ui, |ui| {
                        for info in crate::textenc::supported_encodings() {
                            if !q.is_empty()
                                && !info.id.to_lowercase().contains(&q)
                                && !info.label.to_lowercase().contains(&q)
                                && !info.aliases.iter().any(|a| a.contains(&q))
                            {
                                continue;
                            }
                            // 日本語を往復できる符号化の目印。絵文字の国旗は
                            // 同梱フォントに字が無い (豆腐) ので仮名を使う。
                            let mark = if info.japanese { "あ" } else { "　" };
                            if ui
                                .add_sized(
                                    [ui.available_width(), 24.0],
                                    egui::Button::new(format!("{mark} {}", info.label)),
                                )
                                .clicked()
                            {
                                chosen = Some(info.id.clone());
                            }
                        }
                    });
            });
        if let Some(id) = chosen {
            self.enc_picker = None;
            if for_save {
                self.save_with_encoding(&id);
            } else {
                self.reopen_with_encoding(&id);
            }
        } else if !open {
            self.enc_picker = None;
        }
    }

    /// ネイティブのファイルダイアログを要求する。
    ///
    /// 返り値が `Some(path)` になるのは **macOS だけ** (その場で開いて待つ)。
    /// Windows / Linux では常に `None` を返し、結果は後続フレームの
    /// `poll_dialogs` → `apply_dialog_result` へ届く。
    ///
    /// なぜ OS で分けるか:
    /// * **Windows / Linux**: UI スレッドで同期ダイアログを開くと、winit の
    ///   イベントループの内側で OS のモーダルループが回る。エージェント (PTY) の
    ///   リーダースレッドが撃つ `request_repaint` が再入で届き、ウィンドウが
    ///   再描画されないまま固まる (画面が崩れて操作不能)。スレッドへ逃がす。
    /// * **macOS**: AppKit のパネル (NSOpenPanel/NSSavePanel) はメインスレッド
    ///   専用で、別スレッドから開くと落ちる。そもそもこの固まり方をしないので、
    ///   従来どおり同期で開く。
    pub(super) fn request_dialog(
        &mut self,
        purpose: DialogPurpose,
        spec: DialogSpec,
    ) -> Option<PathBuf> {
        if cfg!(target_os = "macos") {
            return run_file_dialog(&spec);
        }
        let Some(tx) = self.dialogs.begin(purpose.kind()) else {
            // 同じ用途のダイアログがもう開いている — 二重には開かない
            return None;
        };
        std::thread::spawn(move || {
            let path = run_file_dialog(&spec);
            // 受け手が消えていても (ウィンドウを閉じた等) 落ちないよう握り潰す
            let _ = tx.send(DialogOutcome { purpose, path });
        });
        None
    }

    /// ダイアログを要求し、その場で結果が返った場合 (macOS) はすぐ適用する。
    pub(super) fn ask_dialog(
        &mut self,
        purpose: DialogPurpose,
        spec: DialogSpec,
        ctx: &egui::Context,
    ) {
        if let Some(p) = self.request_dialog(purpose.clone(), spec) {
            self.apply_dialog_result(purpose, p, ctx);
        }
    }

    /// 別スレッドのダイアログから返ってきた結果を取り込む。
    pub(super) fn poll_dialogs(&mut self, ctx: &egui::Context) {
        while let Some(out) = self.dialogs.poll() {
            // キャンセル (path=None) は従来どおり「何もしない」
            if let Some(p) = out.path {
                self.apply_dialog_result(out.purpose, p, ctx);
            }
        }
        if self.dialogs.busy() {
            // 待っている間は少し速く回して、選ばれた瞬間に反応できるようにする
            crate::perf::repaint_after(ctx, Duration::from_millis(50), "poll_dialogs");
        }
    }

    /// ダイアログでパスが選ばれたときの処理。
    /// 同期で開く macOS も、スレッドから返る Windows/Linux も同じここを通る。
    pub(super) fn apply_dialog_result(
        &mut self,
        purpose: DialogPurpose,
        path: PathBuf,
        ctx: &egui::Context,
    ) {
        match purpose {
            DialogPurpose::OpenFile => {
                self.open_path(&path);
                self.touch_recent_file(&path);
            }
            DialogPurpose::NewWindowFolder => self.spawn_new_window(Some(path)),
            DialogPurpose::OpenFolder => self.open_workspace(path, ctx),
            DialogPurpose::AddFolder => self.add_folder_to_workspace(path, ctx),
            DialogPurpose::PetImage => match load_pet_texture(ctx, &path) {
                Some(tex) => {
                    self.pet_tex = Some(tex);
                    self.cfg.pet_image = Some(path.to_string_lossy().to_string());
                    self.cfg.show_pet = true;
                    self.cfg.global_show_pet = true;
                    config::save_state(&self.cfg);
                    self.toast(tr("🖼 ペット画像を変更しました"), true);
                }
                None => self.toast(tr("画像を読み込めませんでした"), false),
            },
            DialogPurpose::InstallPlugin => match plugins::install(&path) {
                Ok(p) => {
                    let msg = trf(
                        "📦 {name} v{version} をインストールしました(コマンド{commands} / テーマ{themes} / スニペット{snippets})",
                        &[
                            ("name", p.name.clone()),
                            ("version", p.version.clone()),
                            ("commands", p.commands.len().to_string()),
                            ("themes", p.themes.len().to_string()),
                            ("snippets", p.snippet_files.len().to_string()),
                        ],
                    );
                    self.rebuild_plugins();
                    self.sidebar_open = true;
                    self.sidebar_tab = SidebarTab::Plugins;
                    self.toast(msg, true);
                }
                Err(e) => self.toast(trf("インストール失敗: {e}", &[("e", e.to_string())]), false),
            },
            DialogPurpose::SaveAs {
                buffer_id,
                close_after,
                run_hooks,
            } => {
                // ダイアログを開いている間にタブが閉じられている可能性がある
                let Some(i) = self.editor.buffers.iter().position(|b| b.id == buffer_id) else {
                    self.toast(tr("保存先のタブが見つかりません (閉じられました)"), false);
                    return;
                };
                if self.save_buffer_to(i, path) {
                    self.persist_session();
                    if run_hooks {
                        self.run_on_save_hooks(i, ctx);
                    }
                    if close_after {
                        self.editor.close(i);
                        self.persist_session();
                    }
                }
            }
        }
    }

    pub(super) fn request_close(&mut self, i: usize) {
        if self
            .editor
            .buffers
            .get(i)
            .map(|b| b.dirty())
            .unwrap_or(false)
        {
            self.pending_close = Some(i);
        } else {
            self.editor.close(i);
            self.persist_session();
        }
    }
}
