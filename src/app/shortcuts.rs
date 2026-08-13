use super::*;

impl ZaivernApp {
    // ─── フルスクリーン ─────────────────────────────────────────────
    //
    // macOS + winit 0.30 は「縦オフセット配置のサブディスプレイ」でネイティブ
    // 全画面にすると、ウィンドウ/レイアウトをモニタ実寸より大きく作ってしまう
    // (実測: 1080 の外部モニタで縦 1120 = メインとの配置差 40px ぶん過大)。
    // 描画は画面に押し込まれ、当たり判定は素の座標のままなので、メニューも
    // ファイルツリーも「見えている場所を押しても効かない」状態になる。
    // フルスクリーン中のリサイズ命令は AppKit が拒否して全画面が解除される
    // だけなので、その場では直せない。→ 検知したら全画面を抜け、枠なし最大化
    // (疑似フルスクリーン) に切り替える。健全なディスプレイでは何もしない。

    /// 毎フレーム: 壊れたネイティブ全画面 (ウィンドウがモニタより大きい) を
    /// 検知したら解除し、解除が完了したフレームで疑似フルスクリーンへ入る。
    pub(super) fn fullscreen_guard(&mut self, ctx: &egui::Context) {
        if !cfg!(target_os = "macos") {
            return;
        }
        let (fs, inner, mon) = ctx.input(|i| {
            let v = i.viewport();
            (v.fullscreen.unwrap_or(false), v.inner_rect, v.monitor_size)
        });

        // 疑似フルスクリーン復帰の後半: Maximized(false) を送った 150ms 後に
        // 枠と位置を戻す。zoom: のアニメーションと setStyleMask: を同一ターンに
        // 重ねないため、必ずターンを分けて送る。
        if let Some((pos, size, at)) = self.fake_fs_restore {
            if at.elapsed().as_millis() >= 150 {
                self.fake_fs_restore = None;
                ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(true));
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(pos));
                ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(size));
            }
            crate::perf::repaint_after(
                ctx,
                std::time::Duration::from_millis(50),
                "fullscreen_guard",
            );
        }

        // 矩形の「真の安定」観測: 前フレームから 1px 超動いたら時刻を取り直す。
        // 全画面の遷移アニメーション中はここが毎フレーム更新され続けるので、
        // 「最後に動いてから N ms」で遷移が本当に終わったかを判定できる。
        let rect_moved = match (inner, self.fs_last_rect) {
            (Some(now), Some(prev)) => {
                (now.min - prev.min).length() > 1.0 || (now.size() - prev.size()).length() > 1.0
            }
            (now, prev) => now.is_some() != prev.is_some(),
        };
        if rect_moved || self.fs_rect_moved_at.is_none() {
            self.fs_rect_moved_at = Some(Instant::now());
        }
        self.fs_last_rect = inner;
        let rect_stable_ms = self
            .fs_rect_moved_at
            .map(|t| t.elapsed().as_millis())
            .unwrap_or(0);

        if fs {
            let broken = match (inner, mon) {
                (Some(r), Some(m)) => r.width() > m.x + 1.0 || r.height() > m.y + 1.0,
                _ => false,
            };
            if !broken {
                self.fs_broken_since = None;
                return;
            }
            // 進入アニメーション (~1秒) の最中に Fullscreen(false) を送ると winit が
            // 取りこぼし、「フラグは解除・実体は全画面のまま」で固まる (実測)。
            // 時間 (1.5 秒) に加えて「矩形が 0.5 秒動いていない」ことも要求する —
            // 負荷でアニメが 1.5 秒を超えても、動いている間は絶対に送らない。
            let since = *self.fs_broken_since.get_or_insert_with(Instant::now);
            crate::perf::repaint_after(
                ctx,
                std::time::Duration::from_millis(200),
                "fullscreen_guard",
            );
            if since.elapsed().as_millis() >= 1500
                && rect_stable_ms >= 500
                && !self.fs_rescue_pending
            {
                self.fs_rescue_pending = true;
                self.fs_rescue_from = inner;
                self.fs_rescue_at = Some(Instant::now());
                if let Some(m) = mon {
                    if !self
                        .broken_native_fs
                        .iter()
                        .any(|b| (*b - m).length() < 1.0)
                    {
                        self.broken_native_fs.push(m);
                    }
                }
                // 稀な環境依存の分岐なので、あとから追えるよう痕跡を残す
                eprintln!(
                    "zaivern: broken native fullscreen detected (window={:?} > monitor={:?}) — \
                     switching to borderless maximize",
                    inner.map(|r| r.size()),
                    mon
                );
                self.toast(
                    tr("このディスプレイのネイティブ全画面は表示がずれるため、\
                        全画面相当の最大化へ切り替えます (ESC で元に戻せます)"),
                    true,
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::Fullscreen(false));
            }
        } else {
            self.fs_broken_since = None;
            if self.fs_rescue_pending {
                // 全画面解除のアニメーション中に styleMask/zoom を触ると AppKit が
                // NSException を投げてプロセスごと落ちる (実測)。fullscreen フラグは
                // アニメより先に false になるので、「矩形が救出開始時の壊れた値から
                // 実際に変化し、かつ 0.4 秒動いていない」ことを解除完了の合図にする。
                // 注意: 以前は「動き始めてから 0.4 秒経過」で発火していた — 遅い
                // マシンでは解除アニメがまだ続いている最中に styleMask を送って
                // しまい、まさにこの NSException で落ちていた (v0.4.14 まで)。
                let moved = match (inner, self.fs_rescue_from) {
                    (Some(now), Some(from)) => {
                        (now.min - from.min).length() > 1.0
                            || (now.size() - from.size()).length() > 1.0
                    }
                    _ => inner.is_some(),
                };
                if moved && rect_stable_ms >= 400 {
                    self.fs_rescue_pending = false;
                    self.fs_rescue_from = None;
                    self.fs_rescue_at = None;
                    self.enter_fake_fullscreen(ctx);
                } else if !moved {
                    // 解除コマンドが取りこぼされて実体が全画面のまま固まったら、
                    // これ以上は触らず諦める (この状態で styleMask を触ると落ちる)。
                    if self
                        .fs_rescue_at
                        .is_some_and(|at| at.elapsed().as_secs() >= 6)
                    {
                        eprintln!(
                            "zaivern: fullscreen exit seems lost — giving up rescue \
                             (use the green button / Mission Control to leave fullscreen)"
                        );
                        self.fs_rescue_pending = false;
                        self.fs_rescue_from = None;
                        self.fs_rescue_at = None;
                    }
                }
                // 入力が無くても状態機械が進むようフレームを回し続ける
                crate::perf::repaint_after(
                    ctx,
                    std::time::Duration::from_millis(100),
                    "fullscreen_guard",
                );
            }
        }
    }

    /// 疑似フルスクリーン: 現在ジオメトリを覚えてから枠を消して最大化する。
    pub(super) fn enter_fake_fullscreen(&mut self, ctx: &egui::Context) {
        // 復帰の後半 (枠復元) が予約中に再進入したら、その予約の座標こそが
        // 「戻るべき姿」なので引き継ぐ (今の見た目は復元途中の中間状態)。
        let restore = if let Some((pos, size, _)) = self.fake_fs_restore.take() {
            (pos, size)
        } else {
            let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
            match (outer, inner) {
                (Some(o), Some(inn)) => (o.min, inn.size()),
                _ => (egui::pos2(80.0, 80.0), egui::vec2(1280.0, 860.0)),
            }
        };
        self.fake_fullscreen = Some(restore);
        ctx.send_viewport_cmd(egui::ViewportCommand::Decorations(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(true));
    }

    /// 疑似フルスクリーンから復帰。Maximized(false) はゾーン前の枠を正しく
    /// 戻さないことがある (実測: 幅が変わる) ので、覚えた位置/サイズを明示的に戻す。
    /// ただし zoom: (Maximized) と setStyleMask: (Decorations) を同一ターンで
    /// 送らず、復元の後半は fullscreen_guard の予約消化 (150ms 後) に分ける。
    pub(super) fn exit_fake_fullscreen(&mut self, ctx: &egui::Context) {
        if let Some((pos, size)) = self.fake_fullscreen.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Maximized(false));
            self.fake_fs_restore = Some((pos, size, Instant::now()));
            crate::perf::repaint_after(
                ctx,
                std::time::Duration::from_millis(50),
                "exit_fake_fullscreen",
            );
        }
    }

    /// UI に出す打鍵表記。**必ずキーバインド表から生成する。**
    ///
    /// ベタ書きすると (1) config.toml で再割り当てされた瞬間に嘘になり
    /// (2) Windows/Linux では表記そのものが違う (⌘⇧C ではなく Ctrl+Shift+C)、
    /// という二重の嘘になる。`keybinds::画面のショートカット表記をベタ書きしていない`
    /// が番人。
    pub(super) fn key_hint(&self, a: BindAction) -> String {
        self.keys.label(a)
    }

    pub(super) fn handle_shortcuts(&mut self, ctx: &egui::Context) {
        use egui::{Key, KeyboardShortcut, Modifiers};
        // 素の `consume_shortcut` ではなく互換経路を通す。egui-winit 0.29 は
        // ⌘⇧C / ⌘⇧V の **押下イベントごと** 捨てて Copy/Paste にすり替えるため、
        // 素のままだと「画面には ⌘⇧C と出ているのに効かない」になる
        // (詳細は `keybinds::clipboard_alias`)。
        let consume_sc = |ctx: &egui::Context, sc: KeyboardShortcut| -> bool {
            ctx.input_mut(|i| crate::keybinds::consume_shortcut_compat(i, sc))
        };
        // **IME 変換中は 1 つも消費しない。** 日本語/中国語/韓国語を打っている
        // 最中の生キーは IME のものであってアプリのものではない。ここを通すと
        // 「かなを打っているだけでコマンドが走る」「変換確定の Enter が
        // ショートカットに食われる」が起きる (ターミナル側は
        // `terminal::translate_input` が同じ規則を持っている — エディタ側にだけ
        // 無かった)。1 フレーム 1 回の呼び出しで変換中フラグも更新される。
        // chord の待機に入らないのも同じ理由なので、判定は 1 回で共有する。
        let ime = crate::keybinds::ime_blocks_shortcuts_now(ctx);
        self.chord.note_ime(ime, !ime);
        if ime {
            return;
        }
        // キーバインドの記録中は、打鍵をここで **最初に** 取り込んでから戻る。
        // 通常の消費より前に置くのが要点 — 記録しようとした ⌘S でファイルが
        // 保存されたり、エディタへ文字が入ったりしたら意味がない。
        if self.keybind_record_tick(ctx) {
            return;
        }
        // chord の待機は毎フレームここで進める。時間切れ / Esc で捨てる。
        // self から取り出して回し、消費し終えたら戻す (self の借用を跨がせない)。
        let mut chord = std::mem::take(&mut self.chord);
        let tick = ctx.input_mut(|i| chord.begin_frame(i));
        if chord.is_waiting() {
            // 待機中だけ再描画を予約する (時間切れを画面へ反映するため)。
            // 待っていないフレームでは 1 回も呼ばない = アイドルのコストは 0。
            let left = chord.remaining(ctx.input(|i| i.time));
            crate::perf::repaint_after(
                ctx,
                std::time::Duration::from_secs_f64(left.min(0.1)),
                "handle_shortcuts",
            );
        } else if matches!(
            tick,
            crate::keybinds::ChordTick::TimedOut | crate::keybinds::ChordTick::Cancelled
        ) {
            crate::perf::repaint(ctx, "handle_shortcuts");
        }
        let mut consume = |ctx: &egui::Context, b: crate::keybinds::Binding| -> bool {
            ctx.input_mut(|i| crate::keybinds::consume_binding(i, b, &mut chord))
        };
        let mut cmds: Vec<Cmd> = Vec::new();
        let mut ops: Vec<EditOp> = Vec::new();

        // 修飾キーの多いものを先に消費する
        if consume(ctx, self.keys.binding(BindAction::PaletteCommands)) {
            self.palette.open_commands();
        }
        if consume(ctx, self.keys.binding(BindAction::PaletteFiles)) {
            // VS Code と同じで、開いている最中の ⌘P は「開き直す」ではなく
            // 「次の候補へ」。連打で最近開いたファイルを下っていける。
            if self.palette.open && !self.palette.is_command_mode() {
                self.palette.bump_cycle();
            } else {
                self.palette.open_files();
            }
        }
        // ⌘K ⌘S (2 打鍵)。prefix の ⌘K を握っている間は他のバインドが
        // 素通ししない — `keybinds::consume_binding` がそこを見ている。
        if consume(ctx, self.keys.binding(BindAction::KeybindEditor)) {
            cmds.push(Cmd::ShowShortcuts);
        }
        if consume(ctx, self.keys.binding(BindAction::SaveAll)) {
            cmds.push(Cmd::SaveAll);
        }
        if consume(ctx, self.keys.binding(BindAction::SaveAs)) {
            cmds.push(Cmd::SaveAs);
        }
        if consume(ctx, self.keys.binding(BindAction::Save)) {
            cmds.push(Cmd::Save);
        }
        if consume(ctx, self.keys.binding(BindAction::OpenFile)) {
            cmds.push(Cmd::OpenFileDialog);
        }
        // ⇧⌘F (横断検索) / ⌥⌘F (置換) は ⌘F (検索) より修飾キーが多いので先に
        // エディタの分割 (⌘\ / ⌥⌘\ / ⌘1-3)。修飾の多いものを先に消費する。
        if consume(ctx, self.keys.binding(BindAction::SplitEditorDown)) {
            cmds.push(Cmd::SplitEditorDown);
        }
        if consume(ctx, self.keys.binding(BindAction::SplitEditorRight)) {
            cmds.push(Cmd::SplitEditorRight);
        }
        // ── 起動バー (⌃1〜⌃9 / 他 OS は ⌃⌥1〜⌃⌥9) ────────────────────
        // **必ず FocusPane1..3 より先に消費する。** 他 OS では ⌃⌥1 が
        // ⌘1 (= Ctrl+1) のパターンにも一致してしまう (egui の
        // `matches_logically` は「余分に押された修飾キー」を許す) ので、
        // 修飾キーの多い方を先に取らないとエディタのペイン移動に化ける。
        // ここもループで畳まない (下のコメントと同じ理由)。
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch1)) {
            cmds.push(Cmd::QuickLaunch(1));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch2)) {
            cmds.push(Cmd::QuickLaunch(2));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch3)) {
            cmds.push(Cmd::QuickLaunch(3));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch4)) {
            cmds.push(Cmd::QuickLaunch(4));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch5)) {
            cmds.push(Cmd::QuickLaunch(5));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch6)) {
            cmds.push(Cmd::QuickLaunch(6));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch7)) {
            cmds.push(Cmd::QuickLaunch(7));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch8)) {
            cmds.push(Cmd::QuickLaunch(8));
        }
        if consume(ctx, self.keys.binding(BindAction::QuickLaunch9)) {
            cmds.push(Cmd::QuickLaunch(9));
        }
        // ここはループで畳まない。畳むと `consume(ctx, self.keys.binding(BindAction::X))`
        // という一様な形が崩れ、`keybinds::tests::全アクションが消費地点に
        // 繋がっている` が「押せない」と誤検出する (実際に 3 OS で落ちた)。
        // 番人を緩めるより、消費地点を 1 アクション 1 行に揃える方を選ぶ。
        if consume(ctx, self.keys.binding(BindAction::FocusPane1)) {
            cmds.push(Cmd::FocusEditorPane(1));
        }
        if consume(ctx, self.keys.binding(BindAction::FocusPane2)) {
            cmds.push(Cmd::FocusEditorPane(2));
        }
        if consume(ctx, self.keys.binding(BindAction::FocusPane3)) {
            cmds.push(Cmd::FocusEditorPane(3));
        }
        if consume(ctx, self.keys.binding(BindAction::GlobalSearch)) {
            cmds.push(Cmd::GlobalSearch);
        }
        if consume(ctx, self.keys.binding(BindAction::GlobalReplace)) {
            cmds.push(Cmd::GlobalReplace);
        }
        if consume(ctx, self.keys.binding(BindAction::OpenReplace)) {
            cmds.push(Cmd::OpenReplace);
        }
        if consume(ctx, self.keys.binding(BindAction::NewTerminal)) {
            cmds.push(Cmd::NewTerminal);
        }
        if consume(ctx, self.keys.binding(BindAction::NextTab)) {
            cmds.push(Cmd::NextTab);
        }
        if consume(ctx, self.keys.binding(BindAction::PrevTab)) {
            cmds.push(Cmd::PrevTab);
        }
        // ⌃⇧Tab を先に取る。`Modifiers::matches_logically` は「パターンに
        // 無い修飾キーは押されていてもよい」判定なので、⌃Tab を先に消費すると
        // ⌃⇧Tab が吸われて逆順が永久に効かなくなる。
        if consume(ctx, self.keys.binding(BindAction::SwitchTabBack)) {
            cmds.push(Cmd::SwitchTabBack);
        }
        if consume(ctx, self.keys.binding(BindAction::SwitchTab)) {
            cmds.push(Cmd::SwitchTab);
        }
        // ファイル単位ズーム (⌥⌘+ / Ctrl+Alt+Shift++ …) は「戻る/進む」より先。
        // egui の `Modifiers::matches_logically` は「パターンに無い修飾キーは
        // 押されていてもよい」判定なので、修飾キーの多い方から取らないと
        // 少ない方 (戻る・画面全体ズーム) に吸われる。
        // ⌥⌘= の別名も足す: macOS の ⌥= は「≠」を打つ組み合わせで論理キーが
        // 取れず、winit が物理キー (Equal) へフォールバックするため。
        if consume(ctx, self.keys.binding(BindAction::FileZoomIn))
            || consume_sc(
                ctx,
                KeyboardShortcut::new(self.keys.get(BindAction::FileZoomIn).modifiers, Key::Equals),
            )
        {
            cmds.push(Cmd::FileZoomIn);
        }
        if consume(ctx, self.keys.binding(BindAction::FileZoomOut)) {
            cmds.push(Cmd::FileZoomOut);
        }
        if consume(ctx, self.keys.binding(BindAction::FileZoomReset)) {
            cmds.push(Cmd::FileZoomReset);
        }
        if consume(ctx, self.keys.binding(BindAction::NavForward)) {
            cmds.push(Cmd::NavForward);
        }
        if consume(ctx, self.keys.binding(BindAction::NavBack)) {
            cmds.push(Cmd::NavBack);
        }
        if consume(ctx, self.keys.binding(BindAction::RunBuildTask)) {
            cmds.push(Cmd::RunBuildTask);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleProblems)) {
            cmds.push(Cmd::ToggleProblems);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleFullScreen)) {
            cmds.push(Cmd::ToggleFullScreen);
        }
        // ── 第 2 次配線 ──────────────────────────────────────────
        // ⇧⌘T は ⌘T を持っていないので順序の縛りはない。
        if consume(ctx, self.keys.binding(BindAction::ReopenClosedTab)) {
            cmds.push(Cmd::ReopenClosedTab);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleFold)) {
            cmds.push(Cmd::ToggleFold);
        }
        if consume(ctx, self.keys.binding(BindAction::UnfoldAll)) {
            cmds.push(Cmd::UnfoldAll);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleBookmark)) {
            cmds.push(Cmd::ToggleBookmark);
        }
        if consume(ctx, self.keys.binding(BindAction::MarkToggleMnemonic)) {
            cmds.push(Cmd::MarkToggleMnemonic);
        }
        if consume(ctx, self.keys.binding(BindAction::MarksPanel)) {
            cmds.push(Cmd::MarksPanel);
        }
        if consume(ctx, self.keys.binding(BindAction::MarkJump)) {
            cmds.push(Cmd::MarkJump);
        }
        // 数字ニーモニックへの直行。**打鍵は OS ごとに `marks` が固定して持つ**
        // (⌃ + 数字は mac の起動バー、⌃⌥ + 数字は非 mac の起動バーが既に使う)。
        for d in 0u8..=9 {
            if let Some(sc) = marks::digit_jump_shortcut(d) {
                if consume_sc(ctx, sc) {
                    cmds.push(Cmd::MarkJumpDigit(d));
                }
            }
        }
        if consume(ctx, self.keys.binding(BindAction::LspReferences)) {
            cmds.push(Cmd::LspReferences);
        }
        if consume(ctx, self.keys.binding(BindAction::LspSymbols)) {
            cmds.push(Cmd::LspSymbols);
        }
        if consume(ctx, self.keys.binding(BindAction::LspRename)) {
            cmds.push(Cmd::LspRename);
        }
        if consume(ctx, self.keys.binding(BindAction::LspFormat)) {
            cmds.push(Cmd::LspFormat);
        }
        if consume(ctx, self.keys.binding(BindAction::LspCodeAction)) {
            cmds.push(Cmd::LspCodeAction);
        }
        if consume(ctx, self.keys.binding(BindAction::LspSignatureHelp)) {
            cmds.push(Cmd::LspSignatureHelp);
        }
        // ⌘D: 次の出現を選択 (⇧⌘D の行複製より修飾キーが少ないので後に見る)
        if consume(ctx, self.keys.binding(BindAction::SelectNextOccurrence)) {
            cmds.push(Cmd::SelectNextOccurrence);
        }
        // 差分の変更ジャンプ。⇧F7 を F7 より先に見る (修飾キーが多い方が先)。
        if consume(ctx, self.keys.binding(BindAction::DiffPrevChange)) {
            cmds.push(Cmd::DiffPrevChange);
        }
        if consume(ctx, self.keys.binding(BindAction::DiffNextChange)) {
            cmds.push(Cmd::DiffNextChange);
        }
        // `]f` / `[f` (cmux) — **テキスト入力にフォーカスが無いときだけ**。
        // 1 打鍵目が修飾キー無しの `]` `[` なので、本文を打っている最中に
        // 待機へ入ると 1 秒間ほかのショートカットが素通ししなくなる
        // (`consume_binding` は prefix を握っている間、単打を 1 つも通さない)。
        if ctx.memory(|m| m.focused().is_none()) {
            if consume(ctx, self.keys.binding(BindAction::DiffPrevFile)) {
                cmds.push(Cmd::DiffPrevFile);
            }
            if consume(ctx, self.keys.binding(BindAction::DiffNextFile)) {
                cmds.push(Cmd::DiffNextFile);
            }
        }
        // 診断のジャンプ。差分と同じく ⇧F8 を F8 より先に見る。
        if consume(ctx, self.keys.binding(BindAction::PrevProblem)) {
            cmds.push(Cmd::PrevProblem);
        }
        if consume(ctx, self.keys.binding(BindAction::NextProblem)) {
            cmds.push(Cmd::NextProblem);
        }
        if consume(ctx, self.keys.binding(BindAction::CloseTab)) {
            cmds.push(Cmd::CloseTab);
        }
        // ⇧⌘N (新しいウィンドウ) は ⌘N (新規ファイル) より修飾キーが多いので先に
        if consume(ctx, self.keys.binding(BindAction::NewWindow)) {
            cmds.push(Cmd::NewWindow);
        }
        if consume(ctx, self.keys.binding(BindAction::NewFile)) {
            cmds.push(Cmd::NewFile);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleTerminal))
            || consume_sc(
                ctx,
                KeyboardShortcut::new(Modifiers::COMMAND, Key::Backtick),
            )
        {
            cmds.push(Cmd::ToggleTerminal);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleSidebar)) {
            cmds.push(Cmd::ToggleSidebar);
        }
        // VS Code: ⌘⇧E / Ctrl+Shift+E = エクスプローラーを表示してフォーカス
        if consume(ctx, self.keys.binding(BindAction::FocusExplorer)) {
            self.sidebar_open = true;
            self.sidebar_tab = SidebarTab::Files;
            // エディタ等が持つキーボードフォーカスを外し、ツリーへ渡す
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
            self.tree.focus();
        }
        if consume(ctx, self.keys.binding(BindAction::Find)) {
            // 端末フォーカス中の Cmd+F は端末内検索 (前フレームで terminal::draw が
            // 残したフォーカス中セッションIDで振り分ける)。それ以外はエディタ検索。
            let term_sid: Option<u64> =
                ctx.data(|d| d.get_temp(egui::Id::new("zv-focused-terminal")));
            let routed = term_sid
                .and_then(|sid| self.agents.sessions.iter_mut().find(|s| s.id == sid))
                .map(|s| {
                    s.search.open = true;
                    s.search.focus_pending = true;
                })
                .is_some();
            if !routed {
                cmds.push(Cmd::OpenFind);
            }
        }
        // ── 端末: 前/次のプロンプトへ跳ぶ (Ghostty の "killer feature") ──
        // **端末にフォーカスがあるときだけ消費する。** 無条件に消費すると
        // エディタでの ⌘↑ / ⌘↓ (他 OS では Ctrl+↑/↓) まで飲み込む。
        // 前フレームで terminal::draw が残したセッション ID で振り分ける
        // (Cmd+F の端末内検索と同じ経路)。
        if let Some(sid) = ctx.data(|d| d.get_temp::<u64>(egui::Id::new("zv-focused-terminal"))) {
            // シェル統合が来ていなければ候補が無く、`shell_jump_prompt` は
            // false を返して**何も起きない** (嘘の移動をしない)。
            let mut jump: Option<bool> = None;
            if consume(ctx, self.keys.binding(BindAction::TermPrevPrompt)) {
                jump = Some(false);
            }
            if consume(ctx, self.keys.binding(BindAction::TermNextPrompt)) {
                jump = Some(true);
            }
            if let Some(forward) = jump {
                if let Some(s) = self.agents.sessions.iter_mut().find(|s| s.id == sid) {
                    s.shell_jump_prompt(forward);
                }
            }
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleCockpit)) {
            cmds.push(Cmd::ToggleCockpit);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleKanban)) {
            cmds.push(Cmd::ToggleKanban);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleDeck)) {
            cmds.push(Cmd::ToggleDeck);
        }
        // ── 追従 / 未読カーソル ────────────────────────────────
        // 中央ビュー (Cockpit / 看板 / デッキ / エディタ) を問わず同じキーで
        // 効く。着地は `focus_agent_in_place` が**今見ているビューのまま**行う。
        if consume(ctx, self.keys.binding(BindAction::FollowAgent)) {
            cmds.push(Cmd::ToggleFollowAgent);
        }
        if consume(ctx, self.keys.binding(BindAction::FollowResume)) {
            cmds.push(Cmd::ResumeFollowAgent);
        }
        if consume(ctx, self.keys.binding(BindAction::NextUnread)) {
            cmds.push(Cmd::NextUnreadAgent);
        }
        if consume(ctx, self.keys.binding(BindAction::DeferUnread)) {
            cmds.push(Cmd::DeferUnreadAgent);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleUnread)) {
            cmds.push(Cmd::ToggleUnreadAgent);
        }
        if consume(ctx, self.keys.binding(BindAction::ToggleMdPreview)) {
            cmds.push(Cmd::ToggleMdPreview);
        }
        if consume(ctx, self.keys.binding(BindAction::NewAgent)) {
            cmds.push(Cmd::NewAgent(DEFAULT_PRESET_IX));
        }
        // ズーム: 画面全体 (⌘+ / ⌘- / ⌘0) とファイル単位 (⌥ を足したもの)。
        //
        // `=` も拾うのはブラウザと同じ理由 — US 配列の `+` は ⇧= なので、
        // ⌘+ を打つのに毎回 shift を押させたくない。
        // **⌥ 付きを必ず先に消費すること。** egui の `matches_logically` は
        // 「パターンが要求していない修飾キーが余分に押されていても一致する」
        // ので、⌘⌥- は ⌘- のパターンにも一致してしまう。先に ⌘⌥- を
        // 消費して初めて、ファイル単位と画面全体が別の操作として成立する
        // (このファイル冒頭の「修飾キーの多いものを先に消費する」と同じ理由)。
        // 順序を入れ替えると **ファイル単位のズームが画面全体になる**。
        // ファイル単位 (⌘⌥+ / ⌘⌥- / ⌘⌥0) は上でもう消費済み — ⌥ 付きを
        // 先に取らないと、少ない方 (画面全体) へ吸われるため。
        // `=` の別名も割り当てから作る (再割り当てしても別名がついてくる)。
        if consume(ctx, self.keys.binding(BindAction::ZoomIn))
            || consume_sc(
                ctx,
                KeyboardShortcut::new(self.keys.get(BindAction::ZoomIn).modifiers, Key::Equals),
            )
        {
            cmds.push(Cmd::ZoomIn);
        }
        if consume(ctx, self.keys.binding(BindAction::ZoomOut)) {
            cmds.push(Cmd::ZoomOut);
        }
        if consume(ctx, self.keys.binding(BindAction::ZoomReset)) {
            cmds.push(Cmd::ZoomReset);
        }

        // プラグインコマンドの keybind (plugin.toml の keybind = "cmd+alt+u" など)
        for (sc, pi, ci) in self.plugin_keys.clone() {
            if consume_sc(ctx, sc) {
                cmds.push(Cmd::RunPlugin(pi, ci));
            }
        }

        // エディタ編集操作はエディタにフォーカスがあるときだけ消費する
        // (ターミナル内の alt+↑ 等を奪わないため)
        let editor_focused = self.editor_body_focused(ctx);
        let mut pages: Vec<bool> = Vec::new();
        if editor_focused {
            // 取り消し / やり直しは `TextEdit` より**先に**消費する。
            // egui 0.29 の TextEdit は自前 undoer を持ち外す API が無いので、
            // ここで取らないと egui の粒度で二重に戻ってしまう。
            // 修飾キーの多い ⇧⌘Z を先に消費する。
            if consume(ctx, self.keys.binding(BindAction::Redo)) {
                cmds.push(Cmd::Redo);
            }
            if consume(ctx, self.keys.binding(BindAction::Undo)) {
                cmds.push(Cmd::Undo);
            }
            // egui 0.29 の `TextEdit` は ⌘Y / Ctrl+Y も内蔵 redo として扱う。
            // 取らずに残すと egui の undoer が勝手に本文を戻してしまうので、
            // ここで食べて自前の「やり直し」へ回す (Windows の慣習とも一致)。
            if consume_sc(ctx, KeyboardShortcut::new(Modifiers::COMMAND, Key::Y)) {
                cmds.push(Cmd::Redo);
            }
            // ⌃G (行移動)・F12 (定義)・⇧⌘\ (括弧) はエディタにフォーカスが
            // あるときだけ奪う (ターミナルの ⌃G = BEL 等と衝突させない)
            if consume(ctx, self.keys.binding(BindAction::GoToLine)) {
                cmds.push(Cmd::GoToLine);
            }
            if consume(ctx, self.keys.binding(BindAction::GoToDefinition)) {
                cmds.push(Cmd::GoToDefinition);
            }
            if consume(ctx, self.keys.binding(BindAction::GoToBracket)) {
                cmds.push(Cmd::GoToBracket);
            }
            if consume(ctx, self.keys.binding(BindAction::ToggleComment)) {
                ops.push(EditOp::ToggleComment);
            }
            if consume(ctx, self.keys.binding(BindAction::DuplicateLine)) {
                ops.push(EditOp::Duplicate);
            }
            if consume(ctx, self.keys.binding(BindAction::MoveLineUp)) {
                ops.push(EditOp::Move(true));
            }
            if consume(ctx, self.keys.binding(BindAction::MoveLineDown)) {
                ops.push(EditOp::Move(false));
            }
            // PageUp / PageDown: VS Code 同様に 1 画面ぶんカーソル移動+スクロール
            let (pgup, pgdn) = ctx.input_mut(|i| {
                (
                    i.consume_key(Modifiers::NONE, Key::PageUp),
                    i.consume_key(Modifiers::NONE, Key::PageDown),
                )
            });
            if pgup {
                pages.push(true);
            }
            if pgdn {
                pages.push(false);
            }
        }

        // ESC でフルスクリーン解除 (macOS 標準の感覚)。ESC を使う相手がいる間は
        // 奪わない: パレット/検索/各小窓が開いている・メニュー等のポップアップが
        // 出ている・エディタやターミナルにフォーカスがある (vim の ESC 等) とき。
        // それらが閉じた/外れたあとの「素の ESC」だけがフルスクリーンを解除する。
        if (ctx.input(|i| i.viewport().fullscreen.unwrap_or(false))
            || self.fake_fullscreen.is_some())
            && !self.palette.open
            && !self.find.open
            && !self.goto_open
            && !self.shortcuts_open
            && !self.about_open
            && self.whats_new.is_empty()
            && !self.license_open
            && !self.remote_open
            && ctx.memory(|m| m.focused().is_none() && !m.any_popup_open())
            && ctx.input_mut(|i| i.consume_key(Modifiers::NONE, Key::Escape))
        {
            cmds.push(Cmd::ToggleFullScreen);
        }

        // 機能レジストリの打鍵は**組み込みを全部消費し終えてから**見る。
        // 同じ打鍵が両方に割り当たったときは組み込みを勝たせる (既定同士の
        // 食い合いは `keybinds` の番人テストが統合前に落とす)。
        if let Some(id) = crate::keybinds::feature_hit(ctx, &self.feature_keys, &mut chord) {
            cmds.push(Cmd::Feature(id));
        }

        // 消費が終わったので chord の待機状態を self へ戻す (フレームを跨ぐ持ち物)。
        self.chord = chord;

        for c in cmds {
            self.apply_cmd(c, ctx);
        }
        for op in ops {
            self.editor_op(ctx, op);
        }
        for up in pages {
            self.page_move(ctx, up);
        }
    }

    /// PageUp/PageDown: カーソルを 1 画面ぶん上下の行へ移動し、
    /// ビューも同じ量だけスクロールする (VS Code の挙動)。
    pub(super) fn page_move(&mut self, ctx: &egui::Context, up: bool) {
        let Some(i) = self.editor.active else {
            return;
        };
        let page = ((self.last_view_h / self.last_row_h.max(1.0)).floor() as usize)
            .saturating_sub(2)
            .max(1);
        let ed_id = buf_edit_id(self.cur_pane, self.editor.buffers[i].id);
        let cur = egui::TextEdit::load_state(ctx, ed_id)
            .and_then(|st| st.cursor.char_range())
            .map(|r| r.primary.index)
            .unwrap_or(0);
        let text = &self.editor.buffers[i].text;

        // 現在の (行, 桁) を求める
        let mut line = 0usize;
        let mut col = 0usize;
        for ch in text.chars().take(cur) {
            if ch == '\n' {
                line += 1;
                col = 0;
            } else {
                col += 1;
            }
        }
        let lines: Vec<&str> = text.split('\n').collect();
        let target = if up {
            line.saturating_sub(page)
        } else {
            (line + page).min(lines.len().saturating_sub(1))
        };

        // 移動先の char インデックス (桁は VS Code 同様できるだけ維持)
        let mut idx = 0usize;
        for l in lines.iter().take(target) {
            idx += l.chars().count() + 1;
        }
        idx += col.min(lines[target].chars().count());

        self.pending_select = Some((idx, idx));
        let dir = if up { -1.0 } else { 1.0 };
        self.pending_scroll =
            Some((self.last_scroll_y + dir * page as f32 * self.last_row_h).max(0.0));
    }
}
