use super::*;

impl ZaivernApp {
    // ─── UI: editor ─────────────────────────────────────────────────

    /// エディタの中央ビュー。**分割しているかどうかで経路が 1 本に決まる**
    /// (bool を 2 つ持つと 2 つの経路が同時に描かれる事故が起きるため)。
    pub(super) fn editor_area(&mut self, ui: &mut egui::Ui) {
        self.sync_panes();
        // 所有が取れていないファイルを編集しているなら、**保存でつまずく前に**
        // 出す。分割の有無より前に描くので、どちらの経路でも必ず見える。
        self.lease_banner_ui(ui);
        if self.panes.is_split() {
            self.editor_split_ui(ui);
            return;
        }
        // 分割していない間は**今までと同じ 1 本の経路**を通す。
        // 分割機能を足しただけで画面が変わらないようにするための分岐。
        self.cur_pane = self.panes.focus_id();
        if !self.editor.buffers.is_empty() {
            let idx: Vec<usize> = (0..self.editor.buffers.len()).collect();
            let hit = self.editor_tab_strip(ui, &idx, self.editor.active, true);
            self.apply_tab_hit(self.cur_pane, hit);
        }
        if self.find.open && self.editor.active.is_some() {
            self.find_bar(ui);
        }
        self.editor_body_ui(ui);
    }

    /// `editor.buffers` とペインのタブを突き合わせ、`editor.active` を
    /// フォーカス中ペインのアクティブタブへ合わせ直す。
    ///
    /// これが「バッファは 1 つ・ビューは複数」の接ぎ目。ペインは
    /// バッファ ID しか持たないので、`Vec` の添字がずれても壊れない。
    pub(super) fn sync_panes(&mut self) {
        let ids: Vec<u64> = self.editor.buffers.iter().map(|b| b.id).collect();
        let active_id = self
            .editor
            .active
            .and_then(|i| self.editor.buffers.get(i))
            .map(|b| b.id);
        let left = self.panes.sync(&ids, active_id);
        self.editor.active = left.and_then(|b| self.editor.buffers.iter().position(|x| x.id == b));
        self.cur_pane = self.panes.focus_id();
        self.mirror_pane_order();
        // **編集した瞬間に確定タブへ昇格する** (VS Code と同じ)。
        // 編集の入口は複数あるので、入口ごとにフックせず結果 (dirty) を見る。
        // プレビュー枠が空なら比較 0 回で抜けるので、常時のコストは無い。
        for id in self.panes.order() {
            let Some(b) = self.panes.preview_of(id) else {
                continue;
            };
            if self.editor.buffers.iter().any(|x| x.id == b && x.dirty()) {
                self.panes.promote(b);
            }
        }
    }

    /// 分割していないときだけ、ペインのタブ順を `editor.buffers` へ写し戻す。
    ///
    /// ピン留めは [`editor_split::EditorPane::normalize`] がタブ列の先頭へ
    /// 寄せる。単一ペインでは「バッファ列 = 画面の並び」が前提
    /// (`reorder_tab` は `editor.buffers` の添字で動く) なので、写し戻さないと
    /// **ドラッグの落とし先が 1 つずれる**。並びが同じなら何もしない
    /// (= 通常フレームのコストは比較 1 回)。
    pub(super) fn mirror_pane_order(&mut self) {
        if self.panes.is_split() {
            return;
        }
        let Some(order) = self.panes.pane(self.cur_pane).map(|p| p.tabs.clone()) else {
            return;
        };
        let cur: Vec<u64> = self.editor.buffers.iter().map(|b| b.id).collect();
        if cur == order {
            return;
        }
        let active_id = self
            .editor
            .active
            .and_then(|i| self.editor.buffers.get(i))
            .map(|b| b.id);
        let mut rest = std::mem::take(&mut self.editor.buffers);
        let mut out = Vec::with_capacity(rest.len());
        for id in &order {
            if let Some(k) = rest.iter().position(|b| b.id == *id) {
                out.push(rest.remove(k));
            }
        }
        out.extend(rest);
        self.editor.buffers = out;
        self.editor.active =
            active_id.and_then(|id| self.editor.buffers.iter().position(|b| b.id == id));
        // 検索のヒット位置は本文に紐づくので、並びが変わったら捨てる。
        self.find.current = None;
        self.find.wrapped = None;
    }

    /// タブ列のクリック結果 `(activate, close)` を適用する。
    /// どちらも **`editor.buffers` の添字**。
    pub(super) fn apply_tab_hit(
        &mut self,
        pane: editor_split::PaneId,
        hit: (Option<usize>, Option<usize>),
    ) {
        let (activate, close) = hit;
        if let Some(i) = activate {
            self.panes.set_focus(pane);
            if let (Some(buf), Some(p)) = (
                self.editor.buffers.get(i).map(|b| b.id),
                self.panes.pane_mut(pane),
            ) {
                if let Some(at) = p.tabs.iter().position(|x| *x == buf) {
                    p.active = at;
                    // 押した = 使った。MRU (⌃Tab の順) の先頭へ。
                    p.touch(buf);
                }
            }
            self.editor.active = Some(i);
            self.find.current = None;
            self.find_hits = None;
        }
        if let Some(i) = close {
            let buf = self.editor.buffers.get(i).map(|b| b.id);
            match buf {
                // 同じファイルを別のペインでも開いているなら、**このペインの
                // タブを外すだけ**でバッファは生かす (VS Code と同じ)。
                Some(b) if self.panes.open_count(b) > 1 => {
                    self.panes.close_tab(pane, b);
                    self.sync_panes();
                }
                _ => self.request_close(i),
            }
        }
    }

    /// タブ列を 1 本描く。`idx` は `editor.buffers` の添字の並び、
    /// `active` はそのうちアクティブなもの。戻り値は `(押された, 閉じられた)`。
    ///
    /// **どの幅でも見切れない** — 幅が足りなければ題名を省略し、それでも
    /// 足りなければアイコンだけへ縮退する。判断は純関数
    /// [`editor_split::tab_strip`] が持ち、ここは結果を置くだけ。
    pub(super) fn editor_tab_strip(
        &mut self,
        ui: &mut egui::Ui,
        idx: &[usize],
        active: Option<usize>,
        anchor: bool,
    ) -> (Option<usize>, Option<usize>) {
        let theme = self.theme.clone();
        let mut close_req: Option<usize> = None;
        let mut activate: Option<usize> = None;
        // ドラッグ並べ替えの作業領域。`tab_rects` は (バッファ添字, 矩形) を
        // **このタブ列に並んでいる順**で持つ。分割中はペインごとにタブの
        // 部分集合が並ぶので、バッファ添字そのままでは順序にならない。
        let mut tab_rects: Vec<(usize, egui::Rect)> = Vec::new();
        let mut reorder: Option<(usize, usize)> = None;
        let mut drag_from = self.tab_drag;
        if idx.is_empty() {
            return (None, None);
        }
        // このタブ列のピン留め / プレビューを引く。ピン留めは
        // `EditorPane::normalize` が先頭へ寄せているので「先頭から N 枚」で足りる。
        let pane_id = self.cur_pane;
        let pinned_ids: Vec<u64> = self
            .panes
            .pane(pane_id)
            .map(|p| p.pinned.clone())
            .unwrap_or_default();
        let preview_id = self.panes.preview_of(pane_id);
        let pinned_n = self
            .panes
            .pane(pane_id)
            .map(|p| p.pinned_count())
            .unwrap_or(0)
            .min(idx.len());
        let font = egui::TextStyle::Body.resolve(ui.style());
        // 幅の基準は**ピン留めしていないタブ**の題名だけ (ピン留めは固定幅)。
        let longest = idx
            .iter()
            .skip(pinned_n)
            .filter_map(|i| self.editor.buffers.get(*i))
            .map(|b| {
                let name = format!("{} {}", file_tree::icon_for(&b.title), b.title);
                ui.fonts(|f| f.layout_no_wrap(name, font.clone(), theme.text).size().x)
            })
            .fold(0.0f32, f32::max);
        let strip = editor_split::tab_strip_pinned(
            ui.available_width() - 12.0,
            idx.len(),
            pinned_n,
            longest,
        );
        let text_w = (strip.tab_w - editor_split::TAB_CHROME_W).max(0.0);
        let pin_text_w = (strip.pin_w - 20.0).max(1.0);
        // アクティブタブへの自動スクロールは**変わったフレームだけ**。
        // 毎フレーム要求すると横スクロールを手で動かせなくなる。
        let active_id = active
            .and_then(|i| self.editor.buffers.get(i))
            .map(|b| b.id);
        let want_follow =
            active_id.is_some() && self.tab_scrolled.get(&pane_id) != active_id.as_ref();
        // 右クリックメニュー / ダブルクリックの要求
        // (`&mut self` が要るので描画後に適用する)
        let mut pin_req: Option<usize> = None;
        let mut promote_req: Option<usize> = None;
        let tabs = egui::Frame::none()
            .fill(theme.panel_alt)
            .inner_margin(egui::Margin {
                left: 6.0,
                right: 6.0,
                top: 6.0,
                bottom: 0.0,
            })
            .show(ui, |ui| {
                egui::ScrollArea::horizontal()
                    .id_salt("editor-tabs")
                    .auto_shrink([false, true])
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            // **各タブの矩形は純関数が決める** — 描画はその
                            // 結果に従うだけ。追従スクロールもこの「計画」から
                            // 決めるので、そのタブが初めて現れたフレームで
                            // 確実に画面内へ入る (実測を 1 フレーム待たない)。
                            let total = editor_split::tab_total_w(strip, idx.len(), pinned_n);
                            let planned = editor_split::tab_rects(
                                egui::Rect::from_min_size(
                                    ui.cursor().min,
                                    egui::vec2(total, ui.available_height().max(1.0)),
                                ),
                                strip,
                                idx.len(),
                                pinned_n,
                            );
                            // 収まらない (= 横スクロールへ逃がす) ときだけ、
                            // 合計幅を中身の幅として先に宣言する。こうしないと
                            // ScrollArea が 1 フレーム遅れた幅でスクロール範囲を
                            // 決め、末尾のタブへ届かない。
                            if strip.scroll {
                                ui.set_min_width(total);
                            }
                            // アクティブタブが画面外なら追従する
                            // (掴んでいる間は動かさない — 落とし先が逃げるため)。
                            if want_follow && !ui.ctx().input(|i| i.pointer.any_down()) {
                                if let Some(r) = active
                                    .and_then(|a| idx.iter().position(|i| *i == a))
                                    .and_then(|p| planned.get(p))
                                {
                                    ui.scroll_to_rect(*r, None);
                                }
                            }
                            for i in idx {
                                let i = *i;
                                let Some(b) = self.editor.buffers.get(i) else {
                                    continue;
                                };
                                let active = Some(i) == active;
                                let fill = if active {
                                    theme.bg
                                } else {
                                    Color32::TRANSPARENT
                                };
                                // タブは Frame 全体を当たり判定にする。Label に
                                // Sense を付けるとテキストの矩形しか反応せず、
                                // inner_margin の余白を押しても切り替わらない
                                // (押せるのは見た目のタブの 3 割ほどしかない)。
                                // × は Sense を持たせず座標で判定する — 後から
                                // 呼ぶ interact に当たり判定を奪われて閉じるボタンが
                                // 無反応になるのを避けるため。
                                let icon = file_tree::icon_for(&b.title);
                                let full = if b.dirty() {
                                    format!("{icon} {} ●", b.title)
                                } else {
                                    format!("{icon} {}", b.title)
                                };
                                let color = if active { theme.text } else { theme.text_dim };
                                let mode = strip.mode;
                                // ピン留め = 左端に寄る固定幅の短いタブ。
                                // プレビュー = 斜体の使い捨てタブ。
                                let is_pinned = pinned_ids.contains(&b.id);
                                let is_preview = preview_id == Some(b.id);
                                let dirty = b.dirty();
                                let fr = egui::Frame::none()
                                    .fill(fill)
                                    .rounding(egui::Rounding {
                                        nw: 7.0,
                                        ne: 7.0,
                                        sw: 0.0,
                                        se: 0.0,
                                    })
                                    .inner_margin(egui::Margin::symmetric(10.0, 6.0))
                                    .show(ui, |ui| {
                                        ui.spacing_mut().item_spacing.x = 6.0;
                                        // 斜体は「まだ確定していない」の合図 (VS Code と同じ)
                                        let styled = |t: RichText| {
                                            if is_preview {
                                                t.italics()
                                            } else {
                                                t
                                            }
                                        };
                                        let icon_only =
                                            mode == editor_split::TabLabelMode::IconOnly;
                                        let lab = if icon_only {
                                            let c = if dirty { theme.accent } else { color };
                                            ui.add(
                                                egui::Label::new(styled(
                                                    RichText::new(icon).color(c),
                                                ))
                                                .selectable(false),
                                            )
                                        } else if is_pinned {
                                            // ピン留めは幅を縮めて題名を省略する
                                            ui.set_max_width(pin_text_w);
                                            ui.add(
                                                egui::Label::new(styled(
                                                    RichText::new(&full).color(color),
                                                ))
                                                .selectable(false)
                                                .truncate(),
                                            )
                                        } else if mode == editor_split::TabLabelMode::Truncated {
                                            ui.set_max_width(text_w.max(1.0));
                                            ui.add(
                                                egui::Label::new(styled(
                                                    RichText::new(&full).color(color),
                                                ))
                                                .selectable(false)
                                                .truncate(),
                                            )
                                        } else {
                                            ui.add(
                                                egui::Label::new(styled(
                                                    RichText::new(&full).color(color),
                                                ))
                                                .selectable(false),
                                            )
                                        };
                                        if icon_only
                                            || is_pinned
                                            || mode != editor_split::TabLabelMode::Full
                                        {
                                            lab.on_hover_text(&full);
                                        }
                                        // **ピン留めタブには閉じるボタンを出さない** —
                                        // 誤って閉じないことがピン留めの目的なので、
                                        // 「×」を置いたら意味が矛盾する。
                                        if is_pinned {
                                            return egui::Rect::NOTHING;
                                        }
                                        ui.add(
                                            egui::Label::new(
                                                RichText::new("×").color(theme.text_dim),
                                            )
                                            .selectable(false),
                                        )
                                        .rect
                                    });
                                let x_rect = fr.inner;
                                tab_rects.push((i, fr.response.rect));
                                // click_and_drag: ドラッグ中は clicked() が
                                // 立たないので、並べ替えと切り替えは競合しない
                                let tab = ui.interact(
                                    fr.response.rect,
                                    ui.id().with(("editor-tab", i)),
                                    egui::Sense::click_and_drag(),
                                );
                                // 右クリックメニュー。`&mut self` が要る操作は
                                // ここでは呼べないので、要求だけ控えて後で適用する。
                                tab.context_menu(|ui| {
                                    let label = if is_pinned {
                                        tr("ピン留めを解除")
                                    } else {
                                        tr("タブをピン留め")
                                    };
                                    if ui.button(label).clicked() {
                                        pin_req = Some(i);
                                        ui.close_menu();
                                    }
                                    if ui.button(tr("タブを閉じる")).clicked() {
                                        close_req = Some(i);
                                        ui.close_menu();
                                    }
                                });
                                if tab.hovered() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                                }
                                // ダブルクリック = プレビュータブを確定させる
                                if tab.double_clicked() {
                                    promote_req = Some(i);
                                }
                                if tab.drag_started() {
                                    drag_from = Some(i);
                                }
                                if tab.dragged() {
                                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
                                }
                                if tab.clicked() {
                                    if tab
                                        .interact_pointer_pos()
                                        .is_some_and(|p| x_rect.expand(4.0).contains(p))
                                    {
                                        close_req = Some(i);
                                    } else {
                                        activate = Some(i);
                                    }
                                }
                            }

                            // ── ドラッグ中: 落とし先を線で示し、離したら確定 ──
                            //
                            // 位置の計算は純関数 (`reorder_target` / `reorder_marker_x`)
                            // に閉じていて、ここは描画と確定だけを担う。
                            // 添字は「このタブ列に並んでいる順」なので、分割中で
                            // タブが部分集合でも `tab_rects` の順序がそのまま真実になる。
                            let pointer = ui.ctx().pointer_latest_pos();
                            let down = ui.ctx().input(|i| i.pointer.any_down());
                            if let Some(from) = drag_from {
                                let pos_in_strip = tab_rects.iter().position(|(b, _)| *b == from);
                                let rects: Vec<egui::Rect> =
                                    tab_rects.iter().map(|(_, r)| *r).collect();
                                // 落とし先は**ピン境界でクランプ**する。ピン留めは
                                // 左の区画から出られず、通常タブは入れない
                                // (= 「ピン留めは常に左端」を掴んでも壊さない)。
                                let to = pos_in_strip
                                    .and_then(|f| {
                                        pointer.and_then(|p| reorder_target(&rects, p.x, f))
                                    })
                                    .map(|t| {
                                        editor_split::clamp_reorder(
                                            rects.len(),
                                            pinned_n,
                                            pos_in_strip.unwrap_or(0),
                                            t,
                                        )
                                    });
                                let row = pos_in_strip.and_then(|f| rects.get(f).copied());
                                if down {
                                    if let (Some(f), Some(r)) = (pos_in_strip, row) {
                                        if let Some(x) =
                                            to.and_then(|t| reorder_marker_x(&rects, f, t))
                                        {
                                            ui.painter().vline(
                                                x,
                                                egui::Rangef::new(r.top(), r.bottom()),
                                                egui::Stroke::new(2.5_f32, theme.accent),
                                            );
                                        }
                                    }
                                    // 端に寄せたらタブ列を自動スクロールする
                                    // (掴んだまま ScrollArea の外へは行けないため)
                                    if let Some(p) = pointer {
                                        let vis = ui.clip_rect();
                                        const EDGE: f32 = 28.0;
                                        const STEP: f32 = 12.0;
                                        // scroll_with_delta の +x は中身を右へ動かす
                                        // = オフセットが減る = 左のタブが見える
                                        if p.x < vis.left() + EDGE {
                                            ui.scroll_with_delta(egui::vec2(STEP, 0.0));
                                            crate::perf::repaint(ui.ctx(), "tab_drag_scroll");
                                        } else if p.x > vis.right() - EDGE {
                                            ui.scroll_with_delta(egui::vec2(-STEP, 0.0));
                                            crate::perf::repaint(ui.ctx(), "tab_drag_scroll");
                                        }
                                    }
                                } else {
                                    // 離した = 確定。ただしタブ列から縦に大きく
                                    // 外れた位置で離したら取り消す (誤操作対策)。
                                    // ポインタが取れないとき (窓の外) も同じ。
                                    let inside = match (pointer, row) {
                                        (Some(p), Some(r)) => {
                                            p.y >= r.top() - r.height()
                                                && p.y <= r.bottom() + r.height()
                                        }
                                        _ => false,
                                    };
                                    // 落とし先は「タブ列の中での位置」なので、
                                    // バッファ添字へ翻訳してから確定する。
                                    if let Some(t) = to.filter(|_| inside) {
                                        if let Some((buf_to, _)) = tab_rects.get(t) {
                                            reorder = Some((from, *buf_to));
                                        }
                                    }
                                    drag_from = None;
                                }
                            }
                        });
                    });
            });
        if anchor {
            tutorial::anchor(ui.ctx(), AnchorId::EditorTabs, tabs.response.rect);
        }
        self.tab_drag = drag_from;
        if want_follow {
            match active_id {
                Some(b) => {
                    self.tab_scrolled.insert(pane_id, b);
                }
                None => {
                    self.tab_scrolled.remove(&pane_id);
                }
            }
        }
        if let Some((from, to)) = reorder {
            // 掴んで動かした = そのタブはもう使い捨てではない (VS Code と同じ)
            if let Some(b) = self.editor.buffers.get(from).map(|b| b.id) {
                self.panes.promote(b);
            }
            self.reorder_tab(from, to);
        }
        if let Some(b) = promote_req
            .and_then(|i| self.editor.buffers.get(i))
            .map(|b| b.id)
        {
            self.panes.promote(b);
        }
        if let Some(i) = pin_req {
            self.toggle_pin_tab(i);
        }
        (activate, close_req)
    }

    /// `editor.buffers` の添字で指したタブのピン留めを切り替える。
    ///
    /// ピン留めしたタブは [`editor_split::EditorPane::normalize`] が左端へ寄せ、
    /// 単一ペインでは [`Self::mirror_pane_order`] がその並びを
    /// `editor.buffers` へ写し戻すので、画面とバッファ列は常に一致する。
    pub(super) fn toggle_pin_tab(&mut self, i: usize) {
        let Some(buf) = self.editor.buffers.get(i).map(|b| b.id) else {
            return;
        };
        let pane = self.panes.focus_id();
        let on = self.panes.toggle_pinned(pane, buf);
        // ピン留め = 確定タブ (使い捨てのままピン留めはできない)
        if on {
            self.panes.promote(buf);
        }
        self.sync_panes();
        self.toast(
            if on {
                tr("📌 タブをピン留めしました")
            } else {
                tr("ピン留めを解除しました")
            },
            true,
        );
        self.persist_session();
    }

    /// エディタ本文 (`editor.active` が指すバッファ) を種類ごとに振り分けて描く。
    pub(super) fn editor_body_ui(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();
        if self.editor.active.is_none() {
            self.welcome_ui(ui);
            return;
        }
        // PR 差分タブはファイルではないので、専用の読み取り専用ビューを出して終わる
        // (TextEdit を一切出さないので、編集も保存も原理的に起きない)
        if let Some(i) = self.editor.active {
            // 画像 / 16 進 / メディア / 書庫も専用ビューア
            // (TextEdit を通らない読み取り専用表示)
            if self.preview_view_ui(ui, i) {
                return;
            }
            if let crate::editor::BufferKind::PrDiff { number } = self.editor.buffers[i].kind {
                let b = &self.editor.buffers[i];
                let view = ui.scope(|ui| {
                    panels::pr_diff_ui(ui, &theme, number, b.id, &b.text, &mut self.github)
                });
                tutorial::anchor(ui.ctx(), AnchorId::DiffView, view.response.rect);
                return;
            }
            // コミット差分タブ (blame のガターから開く) も読み取り専用の専用ビュー
            if matches!(
                self.editor.buffers[i].kind,
                crate::editor::BufferKind::CommitDiff | crate::editor::BufferKind::CheckpointDiff
            ) {
                let b = &self.editor.buffers[i];
                let (id, title, text) = (b.id, b.title.clone(), b.text.clone());
                let cache = &mut self.commit_diff_cache;
                let view =
                    ui.scope(|ui| panels::commit_diff_ui(ui, &theme, id, &title, &text, cache));
                tutorial::anchor(ui.ctx(), AnchorId::DiffView, view.response.rect);
                return;
            }
            // レース差分タブも同じく読み取り専用の専用ビュー (race.rs)
            if let crate::editor::BufferKind::RaceDiff { slot } = self.editor.buffers[i].kind {
                let b = &self.editor.buffers[i];
                let view = ui.scope(|ui| {
                    race::race_diff_ui(ui, &theme, slot, b.id, &b.text, &mut self.race)
                });
                tutorial::anchor(ui.ctx(), AnchorId::DiffView, view.response.rect);
                return;
            }
        }
        // パンくず (ワークスペース › フォルダ › ファイル › シンボル)。
        // 出すものが無ければ何も描かない = 高さも取らない。
        self.breadcrumb_bar(ui);
        // Markdown / HTML ファイルは 編集/プレビュー の切替バーを出す
        // (Cockpit の編集ペインに出ているときも同様に切り替えられる)
        let (is_md, is_html) = self
            .editor
            .active
            .map(|i| {
                let b = &self.editor.buffers[i];
                (
                    markdown::is_markdown(&b.title, &b.lang),
                    html::is_html(&b.title, &b.lang),
                )
            })
            .unwrap_or((false, false));
        if is_md || is_html {
            self.md_toggle_bar(ui, is_html);
            if self.md_preview {
                self.markdown_preview_ui(ui, is_html);
                return;
            }
        }
        self.code_editor_ui(ui);
    }

    /// 分割中のエディタを描く。
    ///
    /// ペインの矩形と仕切りは [`editor_split::EditorPanes`]
    /// (= 端末と同じ分割木) が唯一の真実源 — 幾何をここで作り直さない。
    pub(super) fn editor_split_ui(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();
        // 検索バーは分割の上に 1 本だけ。対象はフォーカス中のバッファなので、
        // ペインごとに出すと同じものが何本も並ぶ。
        if self.find.open && self.editor.active.is_some() {
            self.find_bar(ui);
        }
        let area = ui.available_rect_before_wrap();
        if area.width() < 2.0 || area.height() < 2.0 {
            return;
        }
        ui.allocate_rect(area, egui::Sense::hover());
        let gutter = editor_split::GUTTER;
        let rects = self.panes.rects(area, gutter);

        // クリックしたペインへフォーカスを移す。イベントは**消費しない**ので、
        // 同じクリックは本文側 (キャレット移動・選択) にもそのまま届く。
        let press = ui.input(|i| {
            if i.pointer.button_pressed(egui::PointerButton::Primary) {
                i.pointer.interact_pos()
            } else {
                None
            }
        });
        if let Some(pos) = press {
            if let Some((id, _)) = rects.iter().find(|(_, r)| r.contains(pos)) {
                self.panes.set_focus(*id);
            }
        }
        let focus = self.panes.focus_id();
        let strip_h = ui.text_style_height(&egui::TextStyle::Body) + 18.0;
        let mut hits: Vec<(editor_split::PaneId, (Option<usize>, Option<usize>))> = Vec::new();

        for (pid, r) in &rects {
            let (pid, r) = (*pid, *r);
            // このペインのタブを **バッファ ID → 添字** へ解決する。
            let tabs: Vec<u64> = self
                .panes
                .pane(pid)
                .map(|p| p.tabs.clone())
                .unwrap_or_default();
            let idx: Vec<usize> = tabs
                .iter()
                .filter_map(|b| self.editor.buffers.iter().position(|x| x.id == *b))
                .collect();
            let active_idx = self
                .panes
                .pane(pid)
                .and_then(|p| p.active_buf())
                .and_then(|b| self.editor.buffers.iter().position(|x| x.id == b));
            let lay = editor_split::pane_layout(r, idx.len(), strip_h);
            let is_focus = pid == focus;

            // ペインごとのビュー状態を持ち込む (スクロール・カーソル)。
            if let Some(p) = self.panes.pane(pid) {
                let (sc, cur) = (p.scroll, p.cursor);
                self.last_scroll_y = sc;
                self.editor.cursor = cur;
            }
            self.cur_pane = pid;
            self.editor.active = active_idx;

            let mut pane_ui = ui.new_child(
                egui::UiBuilder::new()
                    .max_rect(r)
                    .layout(egui::Layout::top_down(egui::Align::Min)),
            );
            // **必ず push_id** — ペインごとに ScrollArea / TextEdit の
            // 永続 ID を分ける。分けないと同じファイルを 2 ペインで開いた
            // ときにスクロールとキャレットが混ざる。
            pane_ui.push_id(pid, |ui| {
                let mut tab_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(lay.tabs)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                tab_ui.set_clip_rect(lay.tabs);
                let hit = self.editor_tab_strip(&mut tab_ui, &idx, active_idx, false);
                if hit.0.is_some() || hit.1.is_some() {
                    hits.push((pid, hit));
                }
                let mut body_ui = ui.new_child(
                    egui::UiBuilder::new()
                        .max_rect(lay.body)
                        .layout(egui::Layout::top_down(egui::Align::Min)),
                );
                body_ui.set_clip_rect(lay.body);
                self.editor_body_ui(&mut body_ui);
            });

            // 描き終わったビュー状態を書き戻す。
            let (sc, cur) = (self.last_scroll_y, self.editor.cursor);
            if let Some(p) = self.panes.pane_mut(pid) {
                p.scroll = sc;
                p.cursor = cur;
            }
            // フォーカスの合図は細い輪だけ。太枠にすると本文が狭く見える。
            if is_focus {
                ui.painter().rect_stroke(
                    r.shrink(1.0),
                    4.0,
                    egui::Stroke::new(terminal::FOCUS_RING, theme.accent),
                );
            }
        }

        // ── 仕切り (ドラッグでリサイズ / ダブルクリックで均等化) ──
        for g in self.panes.gutters(area, gutter) {
            let hit = match g.dir {
                terminal::SplitDir::Horizontal => g.rect.expand2(egui::vec2(2.0, 0.0)),
                terminal::SplitDir::Vertical => g.rect.expand2(egui::vec2(0.0, 2.0)),
            };
            let id = ui.id().with(("zv-editor-gutter", g.path.as_slice()));
            let resp = ui.interact(hit, id, egui::Sense::click_and_drag());
            let hot = resp.hovered() || resp.dragged();
            if hot {
                ui.ctx().set_cursor_icon(match g.dir {
                    terminal::SplitDir::Horizontal => egui::CursorIcon::ResizeHorizontal,
                    terminal::SplitDir::Vertical => egui::CursorIcon::ResizeVertical,
                });
            }
            if resp.double_clicked() {
                self.panes.equalize_at(&g.path);
            } else if resp.dragged() {
                let d = resp.drag_delta();
                let (delta, span) = match g.dir {
                    terminal::SplitDir::Horizontal => (d.x, g.span.width()),
                    terminal::SplitDir::Vertical => (d.y, g.span.height()),
                };
                if delta != 0.0 {
                    self.panes.drag_gutter(&g.path, delta, span, gutter);
                }
            }
            let col = if hot { theme.accent } else { theme.border };
            let bar = match g.dir {
                terminal::SplitDir::Horizontal => {
                    g.rect.shrink2(egui::vec2(g.rect.width() * 0.3, 2.0))
                }
                terminal::SplitDir::Vertical => {
                    g.rect.shrink2(egui::vec2(2.0, g.rect.height() * 0.3))
                }
            };
            ui.painter().rect_filled(bar, 1.0, col);
        }

        for (pid, hit) in hits {
            self.apply_tab_hit(pid, hit);
        }
        // フォーカス中ペインの状態を「現在のもの」として残す
        // (ステータスバーのカーソル表示・スクロール指示の宛先になる)。
        let f = self.panes.focus_id();
        self.cur_pane = f;
        if let Some(p) = self.panes.pane(f) {
            let (sc, cur, buf) = (p.scroll, p.cursor, p.active_buf());
            self.last_scroll_y = sc;
            self.editor.cursor = cur;
            self.editor.active =
                buf.and_then(|b| self.editor.buffers.iter().position(|x| x.id == b));
        }
    }

    /// エディタ上部のブレッドクラム (`ワークスペース › フォルダ › ファイル › シンボル`)。
    ///
    /// * **パス部分は LSP 不要**。ルートからの相対パスを分解するだけなので、
    ///   サーバーが無い言語でもここが消えることはない。
    /// * シンボルは `documentSymbol` の応答が**このファイルのぶんとして届いている
    ///   ときだけ**足す。後から届いても**高さは変わらない** (常に 1 行)。
    /// * 出すものが無い (untitled / 仮想タブ) ときは**行ごと描かない** (空白を作らない)。
    /// * 幅に収まらないときは中央を「…」で省略する (判断は `breadcrumb::elide`)。
    pub(super) fn breadcrumb_bar(&mut self, ui: &mut egui::Ui) {
        if !self.cfg.breadcrumbs {
            return;
        }
        let Some(active) = self.editor.active else {
            return;
        };
        let Some(path) = self.editor.buffers[active].path.clone() else {
            return; // untitled は行ごと消す
        };
        // 既存の documentSymbol 経路をそのまま使って背景更新を頼む (新経路は作らない)
        self.request_breadcrumb_symbols(&path);

        let caret_line = self.editor.cursor.0.saturating_sub(1);
        let syms: Vec<(String, usize)> = match self.lsp_symbols_path.as_deref() {
            Some(p) if p == path => breadcrumb::symbol_chain(&self.lsp_symbols, caret_line),
            _ => Vec::new(),
        };
        let segs = breadcrumb::segments(&self.roots, &path, &syms);
        if segs.is_empty() {
            return;
        }

        let theme = self.theme.clone();
        let font = FontId::proportional(12.0);
        let measure = |ui: &egui::Ui, s: &str| -> f32 {
            ui.fonts(|f| {
                f.layout_no_wrap(s.to_string(), font.clone(), theme.text)
                    .size()
                    .x
            })
        };
        let sep_w = measure(ui, breadcrumb::SEP) + 8.0;
        let ell_w = measure(ui, breadcrumb::ELLIPSIS);
        let widths: Vec<f32> = segs.iter().map(|s| measure(ui, &s.label)).collect();
        let char_w = ui.fonts(|f| f.glyph_width(&font, 'M')).max(1.0);
        // Frame の内側余白 (左右 8px) を引いた実効幅
        let avail = (ui.available_width() - 16.0).max(0.0);
        let shown = breadcrumb::elide(&widths, sep_w, ell_w, avail);

        let mut act: Option<breadcrumb::SegKind> = None;
        egui::Frame::none()
            .fill(theme.panel_alt)
            .inner_margin(egui::Margin::symmetric(8.0, 3.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    // 高さは常に 1 行ぶん。シンボルが後から届いても伸び縮みしない。
                    ui.set_min_height(font.size + 4.0);
                    ui.spacing_mut().item_spacing.x = 4.0;
                    for (i, piece) in shown.iter().enumerate() {
                        if i > 0 {
                            ui.label(
                                RichText::new(breadcrumb::SEP)
                                    .size(12.0)
                                    .color(theme.text_dim),
                            );
                        }
                        match piece {
                            breadcrumb::Shown::Ellipsis => {
                                ui.label(
                                    RichText::new(breadcrumb::ELLIPSIS)
                                        .size(12.0)
                                        .color(theme.text_dim),
                                )
                                .on_hover_text(tr("幅に収まらないフォルダを省略しています"));
                            }
                            breadcrumb::Shown::Seg(n) => {
                                if let Some(s) = segs.get(*n) {
                                    if breadcrumb_seg(ui, &theme, &s.label, &s.kind) {
                                        act = Some(s.kind.clone());
                                    }
                                }
                            }
                            breadcrumb::Shown::Truncated { index, budget } => {
                                if let Some(s) = segs.get(*index) {
                                    let label =
                                        breadcrumb::truncate_label(&s.label, *budget, char_w);
                                    if breadcrumb_seg(ui, &theme, &label, &s.kind) {
                                        act = Some(s.kind.clone());
                                    }
                                }
                            }
                        }
                    }
                });
            });

        if let Some(kind) = act {
            let ctx = ui.ctx().clone();
            match kind {
                breadcrumb::SegKind::Folder(p) => {
                    self.sidebar_open = true;
                    self.sidebar_tab = SidebarTab::Files;
                    self.tree.reveal_dir(&ctx, &p);
                }
                breadcrumb::SegKind::File(_) => self.palette.open_files(),
                breadcrumb::SegKind::Symbol { line } => self.goto_line(line + 1),
            }
        }
    }

    /// Markdown / HTML 用の 編集/プレビュー 切替バー。
    pub(super) fn md_toggle_bar(&mut self, ui: &mut egui::Ui, is_html: bool) {
        let theme = self.theme.clone();
        let path = self
            .editor
            .active
            .and_then(|i| self.editor.buffers[i].path.clone());
        egui::Frame::none()
            .fill(theme.panel_alt)
            .inner_margin(egui::Margin::symmetric(10.0, 3.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    let label = if is_html { "🌐 HTML" } else { "Ⓜ Markdown" };
                    ui.label(RichText::new(label).size(11.5).color(theme.text_dim));
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let p = ui.selectable_label(
                            self.md_preview,
                            RichText::new(tr("👁 プレビュー")).size(12.0),
                        );
                        if p.on_hover_text(trf(
                            "レンダリング表示 ({key})",
                            &[("key", self.key_hint(BindAction::ToggleMdPreview))],
                        ))
                        .clicked()
                        {
                            self.md_preview = true;
                        }
                        let e = ui.selectable_label(
                            !self.md_preview,
                            RichText::new(tr("✏ 編集")).size(12.0),
                        );
                        if e.on_hover_text(trf(
                            "ソースを編集 ({key})",
                            &[("key", self.key_hint(BindAction::ToggleMdPreview))],
                        ))
                        .clicked()
                        {
                            self.md_preview = false;
                        }
                        // HTML はブラウザで開けば完全な見た目で確認できる
                        if is_html {
                            let b = ui.add_enabled(
                                path.is_some(),
                                egui::Button::new(
                                    RichText::new(tr("🌐 ブラウザで開く")).size(12.0),
                                ),
                            );
                            if b.on_hover_text(tr(
                                "既定ブラウザで完全表示 (ディスクに保存済みの内容)",
                            ))
                            .clicked()
                            {
                                if let Some(p) = &path {
                                    open_external(&p.display().to_string());
                                }
                            }
                        }
                    });
                });
            });
    }

    /// Markdown / HTML のレンダリングプレビュー画面。
    /// HTML は Markdown へ変換してから同じレンダラで描く。
    pub(super) fn markdown_preview_ui(&mut self, ui: &mut egui::Ui, is_html: bool) {
        let Some(active) = self.editor.active else {
            return;
        };
        // プレビューも「そのファイルの表示」なので、ファイル単位のズームが効く。
        // ⌘+ホイールの振り分け対象にもする (本文とプレビューで挙動を変えない)。
        self.zoom_area_next = Some((ui.max_rect(), ZoomArea::File));
        let id = self.editor.buffers[active].id;
        // 変換 (HTML→MD / 埋め込みHTML展開) は重いので内容が変わったときだけ行う
        let h = hash_str(&self.editor.buffers[active].text);
        let cached = self
            .md_pre_cache
            .as_ref()
            .is_some_and(|(cid, ch, _)| *cid == id && *ch == h);
        if !cached {
            let raw = &self.editor.buffers[active].text;
            let processed = if is_html {
                html::html_to_md(raw)
            } else {
                html::preprocess_markdown(raw)
            };
            self.md_pre_cache = Some((id, h, processed));
        }
        let text = match &self.md_pre_cache {
            Some((_, _, t)) => t.clone(),
            None => return,
        };
        let dir = self.editor.buffers[active]
            .path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let theme = self.theme.clone();
        let base = self.editor_font_pt();
        let hl = self.highlighter;
        let images = &mut self.md_images;
        egui::ScrollArea::vertical()
            .id_salt(("md-preview", id))
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // 読みやすい紙面幅に絞って中央寄せする
                let max = 860.0f32.min(ui.available_width());
                let pad = ((ui.available_width() - max) * 0.5).max(0.0);
                ui.horizontal(|ui| {
                    ui.add_space(pad);
                    ui.vertical(|ui| {
                        ui.set_max_width(max);
                        egui::Frame::none()
                            .inner_margin(egui::Margin::symmetric(18.0, 14.0))
                            .show(ui, |ui| {
                                let mut rctx = markdown::RenderCtx {
                                    dir: dir.as_deref(),
                                    images,
                                };
                                markdown::render(ui, &theme, hl, base, &text, &mut rctx);
                            });
                    });
                });
            });
    }

    /// バッファ内検索バー (VS Code の検索ウィジェット相当)。
    ///
    /// 幅による見せ方の判断は [`find_buffer::bar_layout`] (純粋関数 + テーブル
    /// テスト) に閉じてある。ここは決まった配置に従って描くだけで、
    /// 「狭いときは…」の分岐を持たない。
    pub(super) fn find_bar(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();
        let mut step: Option<bool> = None; // Some(forward)
        let mut close = false;
        let mut do_replace = false;
        let mut do_replace_all = false;

        // 打鍵の案内は**生成する**。ベタ書きすると Windows/Linux では表記そのものが
        // 違い、再割り当てでも嘘になる (keybinds の番人テスト参照)。
        let key_next = crate::keybinds::format_shortcut(egui::KeyboardShortcut::new(
            egui::Modifiers::NONE,
            egui::Key::Enter,
        ));
        let key_prev = crate::keybinds::format_shortcut(egui::KeyboardShortcut::new(
            egui::Modifiers::SHIFT,
            egui::Key::Enter,
        ));
        let key_replace_row = self.key_hint(BindAction::OpenReplace);

        // 表示用の要約はクロージャへ入る前に作る (中では self を可変で借りるため)
        let total = self.find_hits.as_ref().map_or(0, |c| c.hits.len());
        let truncated = self.find_hits.as_ref().is_some_and(|c| c.truncated);
        let err = self.find_hits.as_ref().and_then(|c| c.error.clone());
        let cur_no = self.current_hit_index().map(|i| i + 1);
        let has_query = !self.find.query.is_empty();
        let no_match = has_query && (total == 0 || err.is_some());
        let count_text = if !has_query {
            String::new()
        } else if err.is_some() {
            tr("エラー")
        } else if total == 0 {
            tr("結果なし")
        } else {
            // 打ち切ったときは「以上」を付ける (数え切っていないことを隠さない)
            let n = if truncated {
                trf("{n} 件以上", &[("n", total.to_string())])
            } else {
                total.to_string()
            };
            match cur_no {
                Some(i) => trf("{i} / {n}", &[("i", i.to_string()), ("n", n)]),
                // 本文が変わって現在位置を見失っている間は件数だけ出す
                None => trf("{n} 件", &[("n", n)]),
            }
        };
        let wrap_note = self.find.wrapped.map(|fw| {
            if fw {
                tr("末尾から先頭へ折り返しました")
            } else {
                tr("先頭から末尾へ折り返しました")
            }
        });
        let count_display = match wrap_note {
            Some(_) => format!("↩ {count_text}"),
            None => count_text.clone(),
        };
        let count_hover = match &wrap_note {
            Some(note) => format!("{count_text} — {note}"),
            None => count_text.clone(),
        };

        let metrics = find_buffer::BarMetrics::default();
        // 床より狭くても配置は崩さない (これ以上は詰められない下限がある)
        let avail = ui.available_width().max(find_buffer::min_width(&metrics));
        let layout = find_buffer::bar_layout(avail, &metrics);
        debug_assert!(
            layout.total_width(&metrics) <= avail + 0.5,
            "検索バーが可用幅からはみ出した"
        );
        let row_h = ui.spacing().interact_size.y;
        let minimal = layout.density == find_buffer::Density::Minimal;

        let bar = egui::Frame::none()
            .fill(theme.panel_alt)
            .inner_margin(egui::Margin::symmetric(8.0, 5.0))
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.x = layout.spacing;
                ui.horizontal(|ui| {
                    // 置換行の開閉 (VS Code の検索バー左端の ▸/▾ と同じ)
                    if layout.show_caret {
                        let caret = if self.find.replace_open { "▾" } else { "▸" };
                        if ui
                            .add_sized([metrics.caret, row_h], egui::Button::new(caret))
                            .on_hover_text(trf(
                                "置換行の表示切替 ({key})",
                                &[("key", key_replace_row.clone())],
                            ))
                            .clicked()
                        {
                            self.find.replace_open = !self.find.replace_open;
                        }
                    }
                    if layout.show_glyph {
                        ui.label("🔍");
                    }
                    let resp = ui.add(
                        egui::TextEdit::singleline(&mut self.find.query)
                            .desired_width(layout.query_width)
                            .hint_text(tr("ファイル内検索…")),
                    );
                    if no_match {
                        // 0 件 / 不正な正規表現は枠の色で示す (色はテーマから取る)
                        ui.painter().rect_stroke(
                            resp.rect.expand(1.0),
                            ui.visuals().widgets.inactive.rounding,
                            egui::Stroke::new(1.0_f32, find_buffer::no_match_color(&theme)),
                        );
                    }
                    if self.find.focus {
                        resp.request_focus();
                        self.find.focus = false;
                    }
                    if resp.changed() {
                        // 打鍵ごとに探し直す (VS Code のインクリメンタル検索)
                        self.find.current = None;
                        self.find.wrapped = None;
                        step = Some(true);
                    }
                    if resp.lost_focus() {
                        let (enter, shift) =
                            ui.input(|i| (i.key_pressed(egui::Key::Enter), i.modifiers.shift));
                        if enter {
                            step = Some(!shift);
                            // 続けて打てるようにフォーカスを戻す
                            self.find.focus = true;
                        }
                    }

                    // トグル 3 つ。並びは VS Code の検索ウィジェットと同じ
                    // (大小区別 → 単語単位 → 正規表現)。
                    let mut opts_changed = false;
                    if ui
                        .add_sized(
                            [metrics.toggle, row_h],
                            egui::SelectableLabel::new(self.find.opts.case_sensitive, "Aa"),
                        )
                        .on_hover_text(tr("大文字小文字を区別"))
                        .clicked()
                    {
                        self.find.opts.case_sensitive = !self.find.opts.case_sensitive;
                        opts_changed = true;
                    }
                    if ui
                        .add_sized(
                            [metrics.toggle, row_h],
                            egui::SelectableLabel::new(self.find.opts.whole_word, "ab|"),
                        )
                        .on_hover_text(tr("単語単位で一致"))
                        .clicked()
                    {
                        self.find.opts.whole_word = !self.find.opts.whole_word;
                        opts_changed = true;
                    }
                    if ui
                        .add_sized(
                            [metrics.toggle, row_h],
                            egui::SelectableLabel::new(self.find.opts.regex, ".*"),
                        )
                        .on_hover_text(tr("正規表現"))
                        .clicked()
                    {
                        self.find.opts.regex = !self.find.opts.regex;
                        opts_changed = true;
                    }
                    if opts_changed {
                        self.find.current = None;
                        self.find.wrapped = None;
                        step = Some(true);
                    }

                    // 前へ / 次へ。狭いときはアイコンのみへ縮退する。
                    let (prev_label, next_label, nav_w) = if layout.nav_labels {
                        (tr("前へ ↑"), tr("次へ ↓"), metrics.nav_label)
                    } else {
                        ("↑".to_string(), "↓".to_string(), metrics.nav_icon)
                    };
                    if ui
                        .add_sized([nav_w, row_h], egui::Button::new(prev_label))
                        .on_hover_text(trf("前のヒットへ ({key})", &[("key", key_prev.clone())]))
                        .clicked()
                    {
                        step = Some(false);
                    }
                    if ui
                        .add_sized([nav_w, row_h], egui::Button::new(next_label))
                        .on_hover_text(trf("次のヒットへ ({key})", &[("key", key_next.clone())]))
                        .clicked()
                    {
                        step = Some(true);
                    }

                    if layout.show_count && !count_display.is_empty() {
                        let color = if no_match {
                            find_buffer::no_match_color(&theme)
                        } else {
                            theme.text_dim
                        };
                        ui.add_sized(
                            [metrics.count, row_h],
                            egui::Label::new(RichText::new(&count_display).color(color)).truncate(),
                        )
                        .on_hover_text(count_hover.clone());
                    }

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui
                            .add_sized([metrics.close, row_h], egui::Button::new("✕"))
                            .on_hover_text(tr("検索を閉じる"))
                            .clicked()
                        {
                            close = true;
                        }
                    });
                });

                // 正規表現が不正なときは落とさず理由を出す (regex のメッセージそのまま)
                if let Some(e) = &err {
                    ui.add(egui::Label::new(RichText::new(e).color(theme.err).small()).truncate())
                        .on_hover_text(e.clone());
                }

                // 置換行 (VS Code: ⌥⌘F)。折り返しに任せてどの幅でも見切れさせない。
                if self.find.replace_open {
                    ui.horizontal_wrapped(|ui| {
                        ui.label("⇄");
                        let hint = if self.find.opts.regex {
                            tr("置換… ($1 でグループ参照)")
                        } else {
                            tr("置換…")
                        };
                        ui.add(
                            egui::TextEdit::singleline(&mut self.find.replace)
                                .desired_width(layout.query_width)
                                .hint_text(hint),
                        );
                        let (one, all) = if minimal {
                            ("⇄".to_string(), "⇄⇄".to_string())
                        } else {
                            (tr("置換"), tr("すべて置換"))
                        };
                        if ui
                            .button(one)
                            .on_hover_text(tr("いまのヒットを置換して次へ"))
                            .clicked()
                        {
                            do_replace = true;
                        }
                        if ui
                            .button(all)
                            .on_hover_text(tr("このファイルのヒットをすべて置換"))
                            .clicked()
                        {
                            do_replace_all = true;
                        }
                    });
                }
            });
        tutorial::anchor(ui.ctx(), AnchorId::EditorFind, bar.response.rect);

        if let Some(forward) = step {
            self.find_step(forward);
        }
        if do_replace {
            self.replace_current();
        }
        if do_replace_all {
            self.replace_all_in_active();
        }
        if close {
            self.find.open = false;
            self.find.replace_open = false;
            self.find.current = None;
            self.find.wrapped = None;
            // 閉じたらヒット一覧も捨てる (最大 5000 件を抱えたままにしない)
            self.find_hits = None;
        }
    }

    pub(super) fn welcome_ui(&mut self, ui: &mut egui::Ui) {
        let theme = self.theme.clone();
        let mut launch_claude = false;
        let mut open_folder = false;

        // 打鍵の表記はキーバインド表から生成する (ベタ書きは再割り当てで嘘に
        // なり、Windows/Linux では表記そのものが違う)。描画クロージャは
        // self を可変で借りるので、先に文字列だけ作っておく。
        let hints: Vec<(String, String)> = [
            (tr("ファイル検索"), BindAction::PaletteFiles),
            (tr("コマンドパレット"), BindAction::PaletteCommands),
            (
                tr("ターミナル / エージェントパネル"),
                BindAction::ToggleTerminal,
            ),
            (tr("エージェント起動"), BindAction::NewAgent),
            (tr("Cockpit ビュー"), BindAction::ToggleCockpit),
        ]
        .into_iter()
        .map(|(label, a)| (label, self.key_hint(a)))
        .collect();

        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.16);
            ui.label(RichText::new("⚡").size(64.0).color(theme.accent));
            ui.label(
                RichText::new("ZAIVERN CODE")
                    .size(30.0)
                    .strong()
                    .color(theme.text),
            );
            ui.label(
                RichText::new(tr("Rust製 AI-Native エディタ — Zed の速度 × Cmux の並列エージェント × AGI Cockpit の操縦席"))
                    .color(theme.text_dim),
            );
            ui.add_space(22.0);

            if ui
                .add_sized([300.0, 36.0], egui::Button::new(tr("📂 フォルダを開く")))
                .clicked()
            {
                open_folder = true;
            }
            if ui
                .add_sized([300.0, 36.0], egui::Button::new(tr("👾 Claude Code を起動")))
                .clicked()
            {
                launch_claude = true;
            }
            if ui
                .add_sized([300.0, 36.0], egui::Button::new("🎛 Agent Cockpit"))
                .clicked()
            {
                self.cockpit = true;
            }

            ui.add_space(26.0);
            let hint = |s: &str, k: String| -> RichText {
                RichText::new(format!("{k}  —  {s}")).size(12.5).color(theme.text_dim)
            };
            for (label, k) in &hints {
                ui.label(hint(label, k.clone()));
            }
        });

        let ctx = ui.ctx().clone();
        if open_folder {
            self.apply_cmd(Cmd::OpenFolder, &ctx);
        }
        if launch_claude {
            let idx = self
                .cfg
                .agents
                .iter()
                .position(|p| p.command.contains("claude"))
                .unwrap_or(0);
            self.launch_preset(idx, &ctx);
        }
    }
}
