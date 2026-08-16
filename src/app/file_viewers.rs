use super::*;

impl ZaivernApp {
    /// テーマ色 2 色の市松模様テクスチャ (透過画像の背景用)。
    /// 128px 角を敷き詰めるので 1 フレームの描画クアッド数は高々数百で済む。
    /// 色はテーマ由来 (固定の 16 進数は持たない)。テーマが変われば作り直す。
    pub(super) fn checker_texture(&mut self, ctx: &egui::Context) -> egui::TextureHandle {
        let c1 = self.theme.panel;
        let c2 = self.theme.panel_alt;
        if let Some(((a, b), tex)) = &self.checker_tex {
            if *a == c1 && *b == c2 {
                return tex.clone();
            }
        }
        const TILE: usize = 16; // 1 マスのピクセル数
        const SIDE: usize = TILE * 8;
        let mut img = egui::ColorImage::new([SIDE, SIDE], c1);
        for y in 0..SIDE {
            for x in 0..SIDE {
                if ((x / TILE) + (y / TILE)) % 2 == 1 {
                    img.pixels[y * SIDE + x] = c2;
                }
            }
        }
        let tex = ctx.load_texture("zv-img-checker", img, egui::TextureOptions::NEAREST);
        self.checker_tex = Some(((c1, c2), tex.clone()));
        tex
    }

    /// 画像タブのビューア (読み取り専用)。
    /// 市松模様の背景に中央寄せで描く。既定はウィンドウへのフィット表示で、
    /// − / ＋ / 100% / フィット ボタンと Ctrl(⌘)+スクロール (ピンチ) でズームできる。
    /// 壊れた画像はエラーメッセージだけを出す (文字化けテキストは出さない)。
    pub(super) fn image_viewer_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        // ⌘+ホイール / ピンチはこのビューが自分で消費する。
        // 申告しておかないと handle_zoom_gesture が同じジェスチャで
        // 画面全体まで拡大してしまう (画像と UI の二重掛け)。
        self.zoom_area_next = Some((ui.max_rect(), ZoomArea::Image));
        let theme_text = self.theme.text;
        let theme_dim = self.theme.text_dim;
        let theme_warn = self.theme.warn;
        let theme_panel_alt = self.theme.panel_alt;
        let checker = self.checker_texture(ui.ctx());

        // ピクセル→テクスチャの遅延アップロード (ctx が要るので初回描画で行う)。
        // 外部変更で再デコードされると ImageDoc ごと差し替わり texture が None に
        // 戻るため、次のフレームで自動的に新しい絵へ載せ替わる。
        let b = &mut self.editor.buffers[i];
        let id = b.id;
        let title = b.title.clone();
        let Some(doc) = b.image.as_mut() else {
            ui.centered_and_justified(|ui| {
                ui.label(RichText::new(tr("画像を読み込めませんでした")).color(theme_dim));
            });
            return;
        };
        if doc.error.is_none() && doc.texture.is_none() && doc.size[0] > 0 {
            let color = egui::ColorImage::from_rgba_unmultiplied(doc.size, &doc.rgba);
            doc.texture = Some(ui.ctx().load_texture(
                format!("zv-img:{id}"),
                color,
                egui::TextureOptions::LINEAR,
            ));
        }
        let tex = doc.texture.clone();
        let error = doc.error.clone();
        let orig = doc.orig_size;
        let shown = doc.size;
        let file_bytes = doc.file_bytes;

        let total = ui.available_size();
        let bar_h = 30.0;
        let canvas = egui::vec2(total.x, (total.y - bar_h).max(40.0));
        let img_w = shown[0] as f32;
        let img_h = shown[1] as f32;
        // フィット倍率は周囲に少し余白を残して求める
        let fit = crate::editor::image_fit_scale(
            img_w,
            img_h,
            (canvas.x - 16.0).max(1.0),
            (canvas.y - 16.0).max(1.0),
        );
        let is_fit = !self.img_zoom.contains_key(&id);
        let scale = self.img_zoom.get(&id).copied().unwrap_or(fit);
        // Some(Some(z)) = 明示ズームへ / Some(None) = フィットへ戻す
        let mut new_zoom: Option<Option<f32>> = None;

        ui.allocate_ui(canvas, |ui| {
            ui.set_min_size(canvas);
            if let Some(err) = &error {
                ui.centered_and_justified(|ui| {
                    ui.label(
                        RichText::new(trf(
                            "⚠ 画像を表示できません: {err}",
                            &[("err", err.clone())],
                        ))
                        .color(theme_warn),
                    );
                });
                return;
            }
            let Some(tex) = tex else { return };
            egui::ScrollArea::both()
                .id_salt(("image-view", id))
                .auto_shrink(false)
                .show(ui, |ui| {
                    let disp = egui::vec2(img_w * scale, img_h * scale);
                    // 画像が表示域より小さいときに中央へ寄るよう、コンテンツは
                    // 最低でも表示域いっぱいに取る (大きいときはスクロールでパン)
                    let content =
                        egui::vec2(disp.x.max(canvas.x - 16.0), disp.y.max(canvas.y - 16.0));
                    let (rect, resp) = ui.allocate_exact_size(content, egui::Sense::hover());
                    let img_rect = egui::Rect::from_center_size(rect.center(), disp);

                    // 透過部分が分かるように市松模様を画像の下へ敷く。
                    // 敷くのは可視範囲だけ (巨大ズームでクアッドが溢れないように)。
                    let vis = img_rect.intersect(ui.clip_rect());
                    if vis.is_positive() {
                        let p = ui.painter().with_clip_rect(vis);
                        let uv =
                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                        // 模様は画像の左上に揃える (パンしても模様が泳がない)
                        let t = 128.0;
                        let mut y = img_rect.top() + ((vis.top() - img_rect.top()) / t).floor() * t;
                        while y < vis.bottom() {
                            let mut x =
                                img_rect.left() + ((vis.left() - img_rect.left()) / t).floor() * t;
                            while x < vis.right() {
                                p.image(
                                    checker.id(),
                                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(t, t)),
                                    uv,
                                    egui::Color32::WHITE,
                                );
                                x += t;
                            }
                            y += t;
                        }
                        p.image(tex.id(), img_rect, uv, egui::Color32::WHITE);
                    }

                    // Ctrl(⌘)+スクロール / ピンチでズーム (egui が zoom_delta に集約する)
                    if resp.hovered() {
                        let zd = ui.input(|inp| inp.zoom_delta());
                        if (zd - 1.0).abs() > f32::EPSILON {
                            new_zoom = Some(Some((scale * zd).clamp(
                                crate::editor::IMAGE_ZOOM_MIN,
                                crate::editor::IMAGE_ZOOM_MAX,
                            )));
                        }
                    }
                });
        });

        // ステータス行: 寸法・ファイルサイズ・ズーム操作 (読み取り専用の明示付き)
        egui::Frame::none()
            .fill(theme_panel_alt)
            .inner_margin(egui::Margin::symmetric(10.0, 4.0))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new(format!("🖼 {title}"))
                            .size(11.5)
                            .color(theme_dim),
                    );
                    if error.is_none() {
                        let mut meta = format!(
                            "{}×{} px · {}",
                            orig.0,
                            orig.1,
                            crate::editor::human_bytes(file_bytes)
                        );
                        if (shown[0] as u32, shown[1] as u32) != orig {
                            // GPU 上限対策で縮小表示している場合はその旨も出す
                            meta.push_str(&trf(
                                " (表示は {w}×{h} に縮小)",
                                &[("w", shown[0].to_string()), ("h", shown[1].to_string())],
                            ));
                        }
                        ui.label(RichText::new(meta).size(11.5).color(theme_text));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if error.is_none() {
                            ui.label(
                                RichText::new(format!("{:.0}%", scale * 100.0))
                                    .size(11.5)
                                    .color(theme_text),
                            );
                            let f = ui
                                .selectable_label(is_fit, RichText::new(tr("フィット")).size(11.5));
                            if f.on_hover_text(tr("ウィンドウの大きさに合わせる"))
                                .clicked()
                            {
                                new_zoom = Some(None);
                            }
                            if ui
                                .button(RichText::new("100%").size(11.5))
                                .on_hover_text(tr("等倍で表示"))
                                .clicked()
                            {
                                new_zoom = Some(Some(1.0));
                            }
                            if ui
                                .button(RichText::new("＋").size(11.5))
                                .on_hover_text(tr("拡大 (Ctrl/⌘+スクロールでも)"))
                                .clicked()
                            {
                                new_zoom = Some(Some(crate::editor::image_zoom_step(scale, 1)));
                            }
                            if ui
                                .button(RichText::new("−").size(11.5))
                                .on_hover_text(tr("縮小 (Ctrl/⌘+スクロールでも)"))
                                .clicked()
                            {
                                new_zoom = Some(Some(crate::editor::image_zoom_step(scale, -1)));
                            }
                            ui.separator();
                        }
                        ui.label(
                            RichText::new(tr("読み取り専用"))
                                .size(11.0)
                                .color(theme_dim),
                        );
                    });
                });
            });

        match new_zoom {
            Some(Some(z)) => {
                self.img_zoom.insert(id, z);
            }
            Some(None) => {
                self.img_zoom.remove(&id);
            }
            None => {}
        }
    }

    /// 専用ビューア (画像 / 16 進 / メディア / 書庫) を描く。描いたら true。
    ///
    /// 中央ビューの分岐と `code_editor_ui` の二重防御が**同じ 1 か所**を
    /// 通るようにしてある (片方だけ Kind を足し忘れると、そのタブは
    /// TextEdit にバイナリを流し込む — 実際に起きうる事故)。
    pub(super) fn preview_view_ui(&mut self, ui: &mut egui::Ui, i: usize) -> bool {
        use crate::editor::BufferKind;
        use crate::preview::PreviewTag;
        let kind = self.editor.buffers[i].kind;
        if !kind.preview_only() {
            return false;
        }
        if kind == BufferKind::Image {
            self.image_viewer_ui(ui, i);
            return true;
        }
        // 借用を握ったまま `&mut self` のメソッドは呼べないので、
        // どのビューかは Copy な印で先に確定させる
        match self.editor.buffers[i].preview.as_ref().map(|p| p.tag()) {
            Some(PreviewTag::Hex) => self.hex_viewer_ui(ui, i),
            Some(PreviewTag::Media) => self.media_card_ui(ui, i),
            Some(PreviewTag::Archive) => self.archive_list_ui(ui, i),
            Some(PreviewTag::Multi) => self.multibuffer_ui(ui, i),
            // 読み取り自体に失敗した (権限・削除・IO エラー)。
            // 空の TextEdit を出すより「開けなかった」と言い切る。
            None => {
                let dim = self.theme.text_dim;
                ui.centered_and_justified(|ui| {
                    ui.label(RichText::new(tr("このファイルは読み込めませんでした")).color(dim));
                });
            }
        }
        true
    }

    /// 16 進ダンプ。`オフセット | 16 バイトの hex | ASCII 列` の古典的レイアウト。
    ///
    /// 行は**見えている分だけ**組み立てる (`show_rows`)。4 MB のファイルでも
    /// 26 万行を String に展開しないので、開いた瞬間に固まらない。
    pub(super) fn hex_viewer_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let theme = self.theme.clone();
        let ppp = ui.ctx().pixels_per_point();
        let font = FontId::monospace(crate::theme::snap_font_size(self.scaled_editor_font(), ppp));
        let row_h = ui
            .fonts(|f| crate::theme::snap_len(f.row_height(&font), ppp))
            .max(1.0 / ppp);
        let b = &self.editor.buffers[i];
        let id = b.id;
        let Some(crate::preview::PreviewDoc::Hex(doc)) = b.preview.as_ref() else {
            return;
        };
        let rows = crate::preview::hex_row_count(doc.bytes.len());
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(trf(
                    "🔩 {kind} · {size}",
                    &[
                        ("kind", tr(doc.kind.unwrap_or("バイナリ"))),
                        ("size", crate::editor::human_bytes(doc.file_bytes)),
                    ],
                ))
                .color(theme.text_dim)
                .small(),
            );
            if doc.truncated {
                ui.label(
                    RichText::new(trf(
                        "⚠ 先頭 {n} だけ表示しています",
                        &[(
                            "n",
                            crate::editor::human_bytes(crate::preview::HEX_MAX_BYTES),
                        )],
                    ))
                    .color(theme.warn)
                    .small(),
                );
            }
            ui.label(
                RichText::new(tr("読み取り専用"))
                    .color(theme.text_dim)
                    .small(),
            );
        });
        ui.separator();
        if rows == 0 {
            ui.label(
                RichText::new(tr("空のファイルです"))
                    .color(theme.text_dim)
                    .small(),
            );
            return;
        }
        egui::ScrollArea::both()
            .id_salt(("zv-hex", id))
            .auto_shrink(false)
            .show_rows(ui, row_h, rows, |ui, range| {
                // 折り返させない。折り返すと行高が揃わず `show_rows` の
                // 仮想化 (= 行番号 → y 座標) がずれて、桁が踊る。
                // 狭い窓では横スクロールへ逃がすのが正しい。
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                for r in range {
                    ui.label(
                        RichText::new(crate::preview::hex_row(&doc.bytes, r))
                            .font(font.clone())
                            .color(theme.text),
                    );
                }
            });
    }

    /// 動画・音声の情報カード。再生器は持たないので、**中央に 1 枚のカード**で
    /// 分かることだけを出し、再生はシステムのプレイヤーへ渡す。
    pub(super) fn media_card_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let theme = self.theme.clone();
        let b = &self.editor.buffers[i];
        let title = b.title.clone();
        let path = b.path.clone();
        let Some(crate::preview::PreviewDoc::Media(doc)) = b.preview.as_ref() else {
            return;
        };
        // (見出し, 値) の並び。取れなかった項目は「—」で埋める
        // (行ごと消すとファイルによって高さが変わって落ち着かない)。
        let dash = "—".to_string();
        let mut rows: Vec<(String, String)> = vec![
            (tr("種別"), doc.kind.map(tr).unwrap_or_else(|| dash.clone())),
            (tr("サイズ"), crate::editor::human_bytes(doc.file_bytes)),
            (
                tr("再生時間"),
                doc.info
                    .duration_secs
                    .map(crate::preview::format_duration)
                    .unwrap_or_else(|| dash.clone()),
            ),
        ];
        if doc.video {
            rows.push((
                tr("解像度"),
                match (doc.info.width, doc.info.height) {
                    (Some(w), Some(h)) => format!("{w} × {h}"),
                    _ => dash.clone(),
                },
            ));
        }
        rows.push((
            tr("音声"),
            match (doc.info.sample_rate, doc.info.channels) {
                (Some(r), Some(c)) => trf(
                    "{rate} Hz / {ch} ch",
                    &[("rate", r.to_string()), ("ch", c.to_string())],
                ),
                (Some(r), None) => trf("{rate} Hz", &[("rate", r.to_string())]),
                _ => dash.clone(),
            },
        ));
        let icon = if doc.video { "🎬" } else { "🎵" };

        let avail = ui.available_rect_before_wrap().intersect(ui.clip_rect());
        let l = panels::media_card(avail, rows.len(), 2);
        let mut open_it = false;
        let mut copy_it = false;
        ui.allocate_new_ui(egui::UiBuilder::new().max_rect(l.card), |ui| {
            egui::Frame::none()
                .fill(theme.panel_alt)
                .stroke(egui::Stroke::new(1.0_f32, theme.border))
                .rounding(egui::Rounding::same(10.0))
                .inner_margin(egui::Margin::same(panels::space::MD))
                .show(ui, |ui| {
                    ui.set_width(l.card.width() - panels::space::MD * 2.0);
                    let mut body = |ui: &mut egui::Ui| {
                        ui.vertical_centered(|ui| {
                            ui.label(RichText::new(icon).size(44.0));
                            // 長いファイル名でカードを押し広げない (全文はホバーで)
                            ui.add(
                                egui::Label::new(
                                    RichText::new(&title)
                                        .size(16.0)
                                        .strong()
                                        .color(theme.text)
                                        .monospace(),
                                )
                                .wrap_mode(egui::TextWrapMode::Truncate),
                            )
                            .on_hover_text(&title);
                        });
                        for (k, v) in &rows {
                            ui.horizontal(|ui| {
                                ui.label(RichText::new(k).color(theme.text_dim).small());
                                ui.with_layout(
                                    egui::Layout::right_to_left(egui::Align::Center),
                                    |ui| {
                                        ui.label(
                                            RichText::new(v).color(theme.text).small().monospace(),
                                        );
                                    },
                                );
                            });
                        }
                        ui.add_space(panels::space::MD);
                        // 狭くてラベルが入らない幅では短い文言へ縮退させる
                        // (見切れた文字を出すより、短くても読める方がよい)
                        let tight = l.btn_w < panels::MEDIA_BTN_MIN_W;
                        let (open_label, copy_label) = if tight {
                            (tr("▶ 開く"), tr("📋 パス"))
                        } else {
                            (tr("▶ システムのプレイヤーで開く"), tr("📋 パスをコピー"))
                        };
                        let mut buttons = |ui: &mut egui::Ui| {
                            if ui
                                .add_sized(
                                    [l.btn_w, panels::MEDIA_BTN_H],
                                    egui::Button::new(&open_label),
                                )
                                .on_hover_text(tr("システムのプレイヤーで開く"))
                                .clicked()
                            {
                                open_it = true;
                            }
                            if ui
                                .add_sized(
                                    [l.btn_w, panels::MEDIA_BTN_H],
                                    egui::Button::new(&copy_label),
                                )
                                .on_hover_text(tr("フルパスをクリップボードへコピー"))
                                .clicked()
                            {
                                copy_it = true;
                            }
                        };
                        if l.stack {
                            buttons(ui);
                        } else {
                            ui.horizontal(|ui| {
                                ui.spacing_mut().item_spacing.x = panels::space::SM;
                                buttons(ui);
                            });
                        }
                    };
                    if l.scroll {
                        egui::ScrollArea::vertical()
                            .id_salt(("zv-media", self.editor.buffers[i].id))
                            .show(ui, |ui| body(ui));
                    } else {
                        body(ui);
                    }
                });
        });
        if let Some(p) = path {
            if open_it {
                open_external(&p.to_string_lossy());
            }
            if copy_it {
                ui.ctx().copy_text(p.to_string_lossy().to_string());
            }
        }
    }

    /// 書庫 (ZIP 形式) の中身一覧。展開はせず、目次だけを見せる。
    pub(super) fn archive_list_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        let theme = self.theme.clone();
        let b = &self.editor.buffers[i];
        let id = b.id;
        let Some(crate::preview::PreviewDoc::Archive(doc)) = b.preview.as_ref() else {
            return;
        };
        let l = &doc.listing;
        let total_size: u64 = l.entries.iter().map(|e| e.size).sum();
        ui.horizontal_wrapped(|ui| {
            ui.label(
                RichText::new(trf(
                    "📦 {n} 件 · 書庫 {zip} · 展開後 {raw}",
                    &[
                        ("n", l.total.to_string()),
                        ("zip", crate::editor::human_bytes(doc.file_bytes)),
                        ("raw", crate::editor::human_bytes(total_size)),
                    ],
                ))
                .color(theme.text_dim)
                .small(),
            );
            if l.truncated {
                ui.label(
                    RichText::new(trf(
                        "⚠ 先頭 {n} 件だけ表示しています",
                        &[("n", crate::preview::ZIP_MAX_ENTRIES.to_string())],
                    ))
                    .color(theme.warn)
                    .small(),
                );
            }
            if l.error == Some(crate::preview::ZipError::BrokenDirectory) {
                ui.label(
                    RichText::new(tr("⚠ 目次が途中で壊れています (読めた分だけ表示)"))
                        .color(theme.warn)
                        .small(),
                );
            }
            ui.label(
                RichText::new(tr("読み取り専用"))
                    .color(theme.text_dim)
                    .small(),
            );
        });
        ui.separator();
        if l.entries.is_empty() {
            ui.label(
                RichText::new(tr("空の書庫です"))
                    .color(theme.text_dim)
                    .small(),
            );
            return;
        }
        egui::ScrollArea::both()
            .id_salt(("zv-archive", id))
            .auto_shrink(false)
            .show(ui, |ui| {
                // 長いパスは折り返さず横スクロールへ (行高を揃えて表を保つ)
                ui.style_mut().wrap_mode = Some(egui::TextWrapMode::Extend);
                egui::Grid::new("zv-archive-grid")
                    .striped(true)
                    .num_columns(3)
                    .show(ui, |ui| {
                        for h in [tr("名前"), tr("サイズ"), tr("圧縮後")] {
                            ui.label(RichText::new(h).monospace().strong().color(theme.accent));
                        }
                        ui.end_row();
                        for e in &l.entries {
                            let name = if e.name.chars().count() > ARCHIVE_NAME_CHARS {
                                let head: String =
                                    e.name.chars().take(ARCHIVE_NAME_CHARS).collect();
                                format!("{head}…")
                            } else {
                                e.name.clone()
                            };
                            ui.label(
                                RichText::new(if e.dir {
                                    format!("📁 {name}")
                                } else {
                                    format!("📄 {name}")
                                })
                                .monospace()
                                .color(if e.dir {
                                    theme.text_dim
                                } else {
                                    theme.text
                                }),
                            )
                            .on_hover_text(&e.name);
                            let (size, comp) = if e.dir {
                                ("—".to_string(), "—".to_string())
                            } else {
                                (
                                    crate::editor::human_bytes(e.size),
                                    crate::editor::human_bytes(e.compressed),
                                )
                            };
                            ui.label(RichText::new(size).monospace().color(theme.text_dim));
                            ui.label(RichText::new(comp).monospace().color(theme.text_dim));
                            ui.end_row();
                        }
                    });
            });
    }

    /// マルチバッファ (複数ファイルの抜粋を 1 本の面に並べた索引タブ)。
    ///
    /// 行は**見えている分だけ**組み立てる (`show_rows`)。ワークスペース全体の
    /// 検索ヒット数百件でも、開いた瞬間にフレームが伸びない。
    ///
    /// 高さは全行で同じ (見出しも本文も注記も 1 行) なので `show_rows` の
    /// 前提 (等高) を満たす。ここを崩すとスクロール位置が飛ぶ。
    /// マルチバッファ (複数ファイルの抜粋を 1 面に集めた索引) を描く。
    ///
    /// **読むだけの面ではない。** 行をクリックするとその場で直せて、
    /// 「書き戻す」で各ファイルへ一度に反映する
    /// ([`ZaivernApp::multibuffer_writeback`])。行番号の桁をクリックすると
    /// これまでどおりそのファイルを開く。
    pub(super) fn multibuffer_ui(&mut self, ui: &mut egui::Ui, i: usize) {
        use crate::multibuffer::{self as mbuf, Row};
        let theme = self.theme.clone();
        let ppp = ui.ctx().pixels_per_point();
        let font = FontId::monospace(crate::theme::snap_font_size(self.scaled_editor_font(), ppp));
        let row_h = ui
            .fonts(|f| crate::theme::snap_len(f.row_height(&font), ppp))
            .max(1.0 / ppp);
        let glyph_w = ui.fonts(|f| f.glyph_width(&font, '0')).max(1.0);
        let id = self.editor.buffers[i].id;
        // **面を取り出してから描く。** 描画のあいだに `&mut self` (書き戻し)
        // を使うので、`preview` の借用を握ったままにはできない。
        // ここから先は途中で return せず、最後に必ず戻す。
        let Some(crate::preview::PreviewDoc::Multi(slot)) = self.editor.buffers[i].preview.as_mut()
        else {
            return;
        };
        let mut mb = std::mem::take(slot);

        let mut open: Option<(PathBuf, usize)> = None;
        let mut do_writeback = false;
        let mut do_undo = false;
        let mut do_revert = false;
        let mut do_replace = false;
        let mut collapse_all: Option<bool> = None;
        let mut step: Option<bool> = None;

        // ── 見出し行 ──────────────────────────────────────────────
        // 出す部品は可用幅から決める (`head_layout` が唯一の判断)。
        let pending = mb.pending_lines();
        let pending_files = mb.pending_files();
        let has_marks = mb.excerpts.iter().any(|e| !e.marks.is_empty());
        let undoable = !mb.writebacks.is_empty();
        let head = mbuf::head_layout(
            ui.available_width(),
            glyph_w,
            has_marks,
            pending > 0,
            undoable,
        );
        let info = trf(
            "{icon} {n} 件 · {files} ファイル",
            &[
                ("icon", mb.source.icon().to_string()),
                ("n", mb.focus_count().to_string()),
                (
                    "files",
                    mb.excerpts
                        .iter()
                        .map(|e| e.label.as_str())
                        .collect::<std::collections::BTreeSet<_>>()
                        .len()
                        .to_string(),
                ),
            ],
        );
        let subtitle = mb.subtitle.clone();
        let (dropped, unreadable) = (mb.dropped, mb.unreadable);
        let expanded = mb.any_expanded();
        // 件数のラベルにファイルごとの要約を添える。**打ち切ったぶんは必ず数で言う**
        // (黙って切ると「7 ファイルしか変わっていない」という嘘の読み取りを作る)。
        let overview = {
            let ov = mbuf::overview(&mb, mbuf::OVERVIEW_MAX_FILES);
            let mut t = ov.lines.join("\n");
            if ov.cut > 0 {
                if !t.is_empty() {
                    t.push('\n');
                }
                t.push_str(&trf(mbuf::OVERVIEW_CUT_MSG, &[("n", ov.cut.to_string())]));
            }
            t
        };
        let mut replace_with = std::mem::take(&mut mb.replace_with);
        ui.horizontal_wrapped(|ui| {
            let r = ui.label(RichText::new(info).color(theme.text_dim).small());
            if !overview.is_empty() {
                r.on_hover_text(overview);
            }
            if !subtitle.is_empty() {
                ui.label(
                    RichText::new(format!("“{subtitle}”"))
                        .color(theme.accent)
                        .small(),
                );
            }
            if dropped > 0 {
                ui.label(
                    RichText::new(trf(
                        "⚠ {n} 件は表示していません",
                        &[("n", dropped.to_string())],
                    ))
                    .color(theme.warn)
                    .small(),
                )
                .on_hover_text(tr("抜粋の上限に達しました。条件を絞ってください。"));
            }
            if unreadable > 0 {
                ui.label(
                    RichText::new(trf(
                        "⚠ {n} ファイルは読めませんでした",
                        &[("n", unreadable.to_string())],
                    ))
                    .color(theme.warn)
                    .small(),
                );
            }
            // 書き戻していない編集があるときだけ出す (常に 0 を出すバッジは作らない)
            if pending > 0 {
                ui.label(
                    RichText::new(trf(
                        "✎ {n} 行 · {f} ファイル",
                        &[
                            ("n", pending.to_string()),
                            ("f", pending_files.to_string()),
                        ],
                    ))
                    .color(theme.accent)
                    .small(),
                );
                let (wb, rv) = if head.labels {
                    (tr("書き戻す"), tr("編集を捨てる"))
                } else {
                    ("⤓".to_string(), "✕".to_string())
                };
                if ui
                    .button(wb)
                    .on_hover_text(tr(
                        "直した行を元のファイルへ書き戻します。他のインスタンスが保有しているファイルと、開いたあとに変わったファイルは飛ばして名指しで知らせます。",
                    ))
                    .clicked()
                {
                    do_writeback = true;
                }
                if ui
                    .button(rv)
                    .on_hover_text(tr("この面の編集を全部捨てて開いた直後へ戻す"))
                    .clicked()
                {
                    do_revert = true;
                }
            }
            if undoable
                && ui
                    .button("↩")
                    .on_hover_text(tr("直前の書き戻しを取り消す (1 回の書き戻しがまとめて戻ります)"))
                    .clicked()
            {
                do_undo = true;
            }
            // 一括置換は「一致 (marks)」がある面だけ。何も置換できない面に
            // 入力欄を出すと、押しても何も起きないボタンになる。
            if head.replace_w > 0.0 {
                let r = ui.add(
                    egui::TextEdit::singleline(&mut replace_with)
                        .id_salt(("zv-mb-replace", id))
                        .desired_width(head.replace_w)
                        .hint_text(tr("一致をまとめて置換")),
                );
                if r.lost_focus() && ui.input(|inp| inp.key_pressed(egui::Key::Enter)) {
                    do_replace = true;
                }
            }
            if ui
                .button(if expanded { "⊟" } else { "⊞" })
                .on_hover_text(if expanded {
                    tr("すべて畳む")
                } else {
                    tr("すべて開く")
                })
                .clicked()
            {
                collapse_all = Some(expanded);
            }
            if ui.button("↑").on_hover_text(tr("前の一致へ")).clicked() {
                step = Some(false);
            }
            if ui.button("↓").on_hover_text(tr("次の一致へ")).clicked() {
                step = Some(true);
            }
        });
        mb.replace_with = replace_with;
        ui.separator();

        if mb.is_empty() {
            // 空状態は利用可能領域の**中央に 1 枚のカード**で出す (下に取り残さない)。
            // 文言は出所ごとに変える (`変更はありません` / `一致はありません` …) —
            // 「表示するものがありません」だけだと、何を探した結果なのかが読めない。
            // 割り付けは `mbuf::empty_card` が唯一の判断 (テーブルテストで固定)。
            let avail = ui.available_rect_before_wrap();
            let card = mbuf::empty_card(avail, row_h);
            let p = ui.painter().clone();
            p.rect_filled(card.card, 6.0, theme.panel_alt);
            p.text(
                card.title.center(),
                egui::Align2::CENTER_CENTER,
                tr(mbuf::empty_message(mb.source)),
                font.clone(),
                theme.text,
            );
            p.text(
                card.hint.center(),
                egui::Align2::CENTER_CENTER,
                tr(mbuf::empty_hint(mb.source)),
                font.clone(),
                theme.text_dim,
            );
            ui.allocate_rect(avail, egui::Sense::hover());
        } else {
            // ── 本体 ─────────────────────────────────────────────
            let rows = mbuf::rows(&mb);
            let cursor = self.multibuffer_cursor.get(&id).copied().unwrap_or(0);
            let mut scroll_to: Option<usize> = None;
            if let Some(fwd) = step {
                if let Some(next) = mbuf::step_focus(&rows, &mb, cursor, fwd) {
                    scroll_to = Some(next);
                }
            }
            let gutter_w = glyph_w * 6.0;
            // 編集中の文字列は `mb` から**外へ出して**持つ (抜粋への可変借用と
            // 描画のための不変借用がぶつからない)。
            let mut edit_state = mb.editing.take();
            let mut start_edit: Option<(usize, usize)> = None;
            let mut end_edit = false;
            let mut toggle: Option<usize> = None;
            let mbr = &mb;
            egui::ScrollArea::vertical()
                .id_salt(("zv-multibuffer", id))
                .auto_shrink(false)
                .show_rows(ui, row_h, rows.len(), |ui, range| {
                    for r in range {
                        let Some(&row) = rows.get(r) else { continue };
                        let Some(e) = mbr.excerpts.get(row.excerpt()) else {
                            continue;
                        };
                        let w = ui.available_width();
                        let (rect, resp) =
                            ui.allocate_exact_size(egui::vec2(w, row_h), egui::Sense::click());
                        if scroll_to == Some(r) {
                            ui.scroll_to_rect(rect, Some(egui::Align::Center));
                        }
                        let editing_here = matches!(
                            (&edit_state, row),
                            (Some((a, b, _)), Row::Line { ex, idx }) if *a == ex && *b == idx
                        );
                        if !ui.is_rect_visible(rect) && !editing_here {
                            continue;
                        }
                        // 編集中の行だけ `TextEdit` に差し替える。
                        // 可変長リストの中なので salt に (タブ, 抜粋, 行) を混ぜる。
                        if editing_here {
                            let Some((_, _, buf)) = edit_state.as_mut() else {
                                continue;
                            };
                            let r = ui.put(
                                rect,
                                egui::TextEdit::singleline(buf)
                                    .id_salt(("zv-mb-line", id, row.excerpt(), r))
                                    .font(font.clone())
                                    .frame(false)
                                    .margin(egui::Margin::symmetric(4.0, 0.0))
                                    .desired_width(f32::INFINITY),
                            );
                            // 入ったフレームで 1 回だけ焦点を取る (他の部品が
                            // 焦点を持っているときは横取りしない)。
                            if !r.has_focus() && ui.memory(|m| m.focused().is_none()) {
                                r.request_focus();
                            }
                            if r.lost_focus() || ui.input(|inp| inp.key_pressed(egui::Key::Escape))
                            {
                                end_edit = true;
                            }
                            continue;
                        }
                        let painter = ui.painter_at(rect);
                        match row {
                            Row::Header { ex } => {
                                painter.rect_filled(rect, 0.0, theme.panel_alt);
                                // 見出しの文言は `mbuf::header_title` が唯一の判断。
                                // 変更の面では**そのファイルの先頭の抜粋にだけ**
                                // 要約 (`+12 −3`) が付くので、1 ファイルが 3 抜粋に
                                // 割れていても数字は 1 度しか出ない。
                                let title = mbuf::header_title(mbr, ex);
                                // 見出しの色は**そのファイルで最も重い深刻度**。
                                // 畳んだままでも「どのファイルが赤いか」が読める。
                                let col = match e.worst_severity() {
                                    Some(1) => theme.err,
                                    Some(2) => theme.warn,
                                    _ => theme.accent,
                                };
                                let g = truncated_galley(
                                    ui,
                                    &title,
                                    font.clone(),
                                    col,
                                    (w - 8.0).max(1.0),
                                );
                                painter.galley(
                                    rect.left_top() + egui::vec2(4.0, (row_h - g.size().y) * 0.5),
                                    g,
                                    col,
                                );
                                if resp.clicked() {
                                    toggle = Some(ex);
                                }
                                if resp.double_clicked() {
                                    open = mbuf::target_of(&rows, mbr, r);
                                }
                                // 畳んでいる間は中身が見えないので、最初の一致行を
                                // ホバーに出す (開かずに当たりを付けられる)
                                let peek = e
                                    .focus
                                    .first()
                                    .and_then(|&l| e.line_text(l))
                                    .map(|t| format!("\n{}: {}", e.focus[0], t.trim()))
                                    .unwrap_or_default();
                                resp.on_hover_text(trf(
                                    "{path} · クリックで開閉 / ダブルクリックで開く{peek}",
                                    &[
                                        ("path", e.path.to_string_lossy().into_owned()),
                                        ("peek", peek),
                                    ],
                                ));
                            }
                            Row::Line { ex, idx } => {
                                let line_no = e.first_line + idx;
                                let focused = e.focus.binary_search(&line_no).is_ok();
                                if focused {
                                    painter.rect_filled(rect, 0.0, theme.accent_soft);
                                } else if resp.hovered() {
                                    painter.rect_filled(rect, 0.0, theme.panel);
                                }
                                let text = e.lines.get(idx).map(|s| s.as_str()).unwrap_or("");
                                let marks: Vec<(usize, usize)> = e
                                    .marks
                                    .iter()
                                    .filter(|m| m.line == line_no)
                                    .map(|m| (m.start, m.end))
                                    .collect();
                                let job =
                                    search_row_job(&theme, line_no, text, &marks, font.clone());
                                let g = wrap_job_to_one_row(ui, job, (w - 8.0).max(1.0));
                                painter.galley(
                                    rect.left_top() + egui::vec2(4.0, (row_h - g.size().y) * 0.5),
                                    g,
                                    theme.text,
                                );
                                if resp.clicked() {
                                    // 行番号の桁を押したらファイルを開く。
                                    // 本文側を押したら**その場で直す**。
                                    let on_gutter = resp
                                        .interact_pointer_pos()
                                        .map(|p| p.x < rect.left() + gutter_w)
                                        .unwrap_or(false);
                                    if on_gutter {
                                        open = mbuf::target_of(&rows, mbr, r);
                                    } else {
                                        start_edit = Some((ex, idx));
                                    }
                                }
                                resp.on_hover_text(tr(
                                    "クリックでこの行を直す / 行番号を押すとファイルを開く",
                                ));
                            }
                            Row::Note { note, .. } => {
                                let Some(n) = e.notes.get(note) else { continue };
                                let col = match n.severity {
                                    1 => theme.err,
                                    2 => theme.warn,
                                    0 => theme.text_dim,
                                    _ => theme.accent,
                                };
                                let g = truncated_galley(
                                    ui,
                                    &format!("↳ {}", n.text),
                                    font.clone(),
                                    col,
                                    (w - gutter_w - 8.0).max(1.0),
                                );
                                painter.galley(
                                    rect.left_top()
                                        + egui::vec2(gutter_w, (row_h - g.size().y) * 0.5),
                                    g,
                                    col,
                                );
                                if resp.clicked() {
                                    open = mbuf::target_of(&rows, mbr, r);
                                }
                                resp.on_hover_text(&n.text);
                            }
                            Row::Separator { .. } => {
                                let y = rect.center().y;
                                painter.hline(
                                    rect.x_range(),
                                    y,
                                    egui::Stroke::new(1.0_f32, theme.border),
                                );
                            }
                        }
                    }
                });

            // ── 記録した操作を反映 (借用が切れてから) ───────────────
            if let Some(next) = scroll_to {
                self.multibuffer_cursor.insert(id, next);
            }
            if let Some((ex, idx)) = start_edit {
                // 別の行へ移るときは、いま直していた行を先に確定する
                mbuf::commit_line_edit(&mut mb, edit_state.take());
                edit_state = mb
                    .excerpts
                    .get(ex)
                    .and_then(|e| e.lines.get(idx))
                    .map(|t| (ex, idx, t.clone()));
            } else if end_edit {
                mbuf::commit_line_edit(&mut mb, edit_state.take());
            }
            mb.editing = edit_state;
            if let Some(collapsed) = collapse_all {
                let st = mb.editing.take();
                mbuf::commit_line_edit(&mut mb, st);
                mb.set_all_collapsed(collapsed);
                self.multibuffer_cursor.insert(id, 0);
            }
            if let Some(ex) = toggle {
                let st = mb.editing.take();
                mbuf::commit_line_edit(&mut mb, st);
                if let Some(e) = mb.excerpts.get_mut(ex) {
                    e.collapsed = !e.collapsed;
                }
                self.multibuffer_cursor.insert(id, 0);
            }
        }

        if do_replace && !mb.replace_with.is_empty() {
            let to = mb.replace_with.clone();
            let n = mbuf::replace_marks(&mut mb, &to);
            if n == 0 {
                self.toast_warn(tr("置き換えるものがありませんでした"));
            } else {
                self.toast(
                    trf(
                        "{n} 件を置き換えました (まだ書き戻していません)",
                        &[("n", n.to_string())],
                    ),
                    true,
                );
            }
        }
        if do_revert {
            mb.revert_edits();
            self.toast(tr("編集を捨てました"), true);
        }
        if do_writeback {
            self.multibuffer_writeback(&mut mb);
        }
        if do_undo {
            self.multibuffer_undo_writeback(&mut mb);
        }

        // **必ず戻す。** 途中でタブが動いていても取り違えないよう id で引き直す。
        if let Some(b) = self.editor.buffers.iter_mut().find(|b| b.id == id) {
            if let Some(crate::preview::PreviewDoc::Multi(slot)) = b.preview.as_mut() {
                *slot = mb;
            }
        }
        if let Some((path, line)) = open {
            // multibuffer は 1-based、jump_to_lsp_pos は 0-based
            self.jump_to_lsp_pos(&path, line.saturating_sub(1), 0);
        }
    }

    /// 1 回の書き戻しで触れるファイル数の上限。
    ///
    /// 書き戻しはユーザーの単発操作なので同期で書くが、数百ファイルを
    /// 1 フレームで書くと目に見えて固まる。超えたぶんは名指しで残し、
    /// もう一度押せば続きが書ける。
    pub(super) const MULTIBUFFER_WRITE_MAX_FILES: usize = 64;

    /// 取り消し 1 段として抱えてよい本文の合計バイト数。
    /// 超えたら取り消し情報を捨てる (抱え込んでメモリを食うより正直に諦める)。
    pub(super) const MULTIBUFFER_UNDO_MAX_BYTES: usize = 16 * 1024 * 1024;

    /// そのパスの**いまの本文**。エディタで開いていれば未保存の本文を返す。
    ///
    /// マルチバッファを組み立てたときと同じ規則にしてある
    /// (違えると、開いているファイルが必ず「変わっている」判定になる)。
    pub(super) fn multibuffer_current_text(editor: &editor::Editor, path: &Path) -> Option<String> {
        if let Some(b) = editor
            .buffers
            .iter()
            .find(|b| b.kind == editor::BufferKind::File && b.path.as_deref() == Some(path))
        {
            return Some(b.text.clone());
        }
        let meta = std::fs::metadata(path).ok()?;
        if !meta.is_file() || meta.len() > Self::MULTIBUFFER_MAX_FILE_BYTES {
            return None;
        }
        let bytes = std::fs::read(path).ok()?;
        Some(crate::textenc::decode_bytes(&bytes).0)
    }

    /// 1 ファイルへ書き戻す。**リースの門は必ず通る。**
    ///
    /// エディタで開いているファイルは、バッファへ `apply_edit` してから
    /// 保存する — こうすると**そのタブでも ⌘Z 1 回で戻る**し、画面に出て
    /// いる本文とディスクが食い違わない。
    pub(super) fn multibuffer_write_one(
        &mut self,
        path: &Path,
        new_text: &str,
    ) -> std::io::Result<()> {
        if let Some(j) = self
            .editor
            .buffers
            .iter()
            .position(|b| b.kind == editor::BufferKind::File && b.path.as_deref() == Some(path))
        {
            let ed = self.edit_step();
            let b = &mut self.editor.buffers[j];
            b.apply_edit(new_text.to_string(), ed);
            // `write_to` が `lease::check_write` を通る唯一の書き込み口。
            b.write_to(path)?;
            b.mark_saved();
            b.disk_mtime = disk_mtime(path);
            b.conflict_notified = None;
            self.queue_lsp_change(j);
            return Ok(());
        }
        // 開いていないファイル。**同じ門を自分で通す** (書き込み口は 2 つある
        // ので、片方だけ守っても穴になる)。
        if let crate::lease::Verdict::Deny(msg) = crate::lease::check_write(path) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                msg,
            ));
        }
        // 読み込んだときの文字コードで書き戻す (CP932 を勝手に UTF-8 にしない)。
        let enc = std::fs::read(path)
            .map(|b| crate::textenc::decode_bytes(&b).1)
            .unwrap_or(crate::textenc::Encoding::Utf8);
        let (bytes, _) = crate::textenc::encode_bytes(new_text, enc);
        std::fs::write(path, bytes)
    }

    /// 直した抜粋を各ファイルへ書き戻す。**1 回の操作 = 取り消し 1 段**。
    ///
    /// fail-closed の関門が 2 つある。どちらも**そのファイルだけ**を落とし、
    /// 通せるものは通す (全部を諦めると 1 件の衝突で作業が止まる):
    ///
    ///  1. マルチバッファを開いたあとに元ファイルが変わっている
    ///     (別のエージェントが書いた) → [`multibuffer::Reject::Changed`]
    ///  2. 他のインスタンスが保有している (`lease::check_write`)
    ///     → [`multibuffer::Reject::Leased`]
    pub(super) fn multibuffer_writeback(&mut self, mb: &mut crate::multibuffer::Multibuffer) {
        use crate::multibuffer as mbuf;
        let st = mb.editing.take();
        mbuf::commit_line_edit(mb, st);
        let before = mb.baseline();
        let plan = {
            let editor = &self.editor;
            mbuf::plan_writeback(
                &before,
                mb,
                |path| Self::multibuffer_current_text(editor, path),
                |path| match crate::lease::check_write(path) {
                    crate::lease::Verdict::Deny(m) => Some(m),
                    crate::lease::Verdict::Allow => None,
                },
            )
        };
        if plan.items.is_empty() {
            self.toast_warn(tr("書き戻すものがありません"));
            return;
        }
        let mut wb = mbuf::WriteBack::default();
        let mut wrote = 0usize;
        let mut refused: Vec<String> = Vec::new();
        for item in &plan.items {
            let new_text = match &item.outcome {
                Err(r) => {
                    refused.push(format!("{}: {}", item.label, r.reason()));
                    continue;
                }
                Ok(t) => t,
            };
            if wrote >= Self::MULTIBUFFER_WRITE_MAX_FILES {
                refused.push(format!(
                    "{}: {}",
                    item.label,
                    tr("一度に書き戻せる上限を超えました (もう一度押すと続きます)")
                ));
                continue;
            }
            match self.multibuffer_write_one(&item.path, new_text) {
                Ok(()) => {
                    let snap = mbuf::settle_file(mb, &item.path, &mbuf::split_lines(new_text));
                    wb.excerpts.extend(snap);
                    wb.files
                        .push((item.path.clone(), item.before.clone(), new_text.clone()));
                    wrote += 1;
                }
                Err(e) => refused.push(format!("{}: {e}", item.label)),
            }
        }
        if wrote > 0 {
            // 取り消しは 1 段。抱える本文が大きすぎるときは正直に諦める。
            if wb.bytes() <= Self::MULTIBUFFER_UNDO_MAX_BYTES {
                mb.writebacks.push(wb);
                let over = mb.writebacks.len().saturating_sub(4);
                mb.writebacks.drain(..over);
            } else {
                mb.writebacks.clear();
            }
            self.tree.invalidate();
            self.local_history.note(&tr("マルチバッファの書き戻し"));
        }
        // 通した数と落とした理由を**両方**出す。落ちたことに気付かないのが
        // 一番困る (拒否は「あとで発見させない」ための機能なので)。
        let msg = if refused.is_empty() {
            trf("{n} ファイルへ書き戻しました", &[("n", wrote.to_string())])
        } else {
            trf(
                "{n} ファイルへ書き戻し / {m} 件は書けませんでした — {why}",
                &[
                    ("n", wrote.to_string()),
                    ("m", refused.len().to_string()),
                    ("why", refused.join(" / ")),
                ],
            )
        };
        if refused.is_empty() {
            self.toast(msg, true);
        } else {
            self.toast_warn(msg);
        }
    }

    /// 直前の書き戻しを取り消す。**1 回の書き戻しがまとめて戻る。**
    ///
    /// 書き戻したあとに誰かがそのファイルを触っていたら戻さない
    /// (fail-closed)。戻せたファイルの抜粋だけ「編集あり」の状態へ復す。
    pub(super) fn multibuffer_undo_writeback(&mut self, mb: &mut crate::multibuffer::Multibuffer) {
        use crate::multibuffer as mbuf;
        let Some(wb) = mb.writebacks.pop() else {
            self.toast_warn(tr("取り消せる書き戻しがありません"));
            return;
        };
        let mut undone: Vec<PathBuf> = Vec::new();
        let mut refused: Vec<String> = Vec::new();
        for (path, before, after) in &wb.files {
            let label = self.rel_label(path);
            match Self::multibuffer_current_text(&self.editor, path) {
                Some(cur) if cur == *after => match self.multibuffer_write_one(path, before) {
                    Ok(()) => undone.push(path.clone()),
                    Err(e) => refused.push(format!("{label}: {e}")),
                },
                Some(_) => {
                    refused.push(format!("{label}: {}", tr("書き戻したあとに変わっています")))
                }
                None => refused.push(format!("{label}: {}", tr("読めません"))),
            }
        }
        // 戻せたファイルの抜粋だけ、書き戻し前の姿へ復す。
        let snap: Vec<_> = wb
            .excerpts
            .iter()
            .filter(|(i, ..)| {
                mb.excerpts
                    .get(*i)
                    .is_some_and(|e| undone.iter().any(|p| p == &e.path))
            })
            .cloned()
            .collect();
        mbuf::restore_excerpts(mb, &snap);
        if !undone.is_empty() {
            self.tree.invalidate();
        }
        let msg = if refused.is_empty() {
            trf(
                "{n} ファイルの書き戻しを取り消しました",
                &[("n", undone.len().to_string())],
            )
        } else {
            trf(
                "{n} ファイルを取り消し / {m} 件は戻せませんでした — {why}",
                &[
                    ("n", undone.len().to_string()),
                    ("m", refused.len().to_string()),
                    ("why", refused.join(" / ")),
                ],
            )
        };
        if refused.is_empty() {
            self.toast(msg, true);
        } else {
            self.toast_warn(msg);
        }
    }
}
