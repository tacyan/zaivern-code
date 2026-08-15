use super::*;

impl ZaivernApp {
    // ─── UI: palette ────────────────────────────────────────────────

    pub(super) fn palette_items(&self) -> Results {
        let q = self.palette.query().to_string();
        // クエリ前処理 (to_lowercase 等) は 1 回だけ。候補ごとの評価は
        // PreparedQuery::score で行う (fuzzy::score と同値・パリティテスト固定済み)。
        let pq = fuzzy::PreparedQuery::new(&q);
        let mut items: Vec<Item> = Vec::new();

        if self.palette.is_command_mode() {
            self.palette_items_command_mode(&pq, &mut items);
        } else if self.palette.is_symbol_mode() {
            self.palette_items_symbol_mode(&pq, &mut items);
        } else if self.palette.is_goto_mode() {
            self.palette_items_goto_mode(&mut items);
        } else if self.palette.is_agent_mode() {
            self.palette_items_agent_mode(&pq, &mut items);
        } else if self.palette.is_root_mode() {
            self.palette_items_root_mode(&pq, &mut items);
        } else {
            self.palette_items_file_mode(&mut items);
        }

        // 行き止まり対策: 何も当たらなかったときだけ、同じクエリを**コマンドとして**
        // 評価し直す。接頭辞を付け忘れる (日本語 IME を ON にしたまま打って
        // `>` が入らなかった等) と、コマンド名がそのままファイル検索へ流れて
        // 「一致するものはありません」で行き止まりになる。当たったときだけ
        // 案内を 1 行出すので、当たらなければ何も増えない。
        let mut command_alt: Vec<Item> = Vec::new();
        if items.is_empty() && !self.palette.is_command_mode() && !q.trim().is_empty() {
            self.palette_items_command_mode(&pq, &mut command_alt);
        }

        // 並べ替え・件数の頭打ち・グループ分け・空/不一致の見せ方は
        // すべて palette 側の純粋関数に任せる (テーブルテストで固定済み)。
        self.palette.results(items, &command_alt)
    }

    /// パレット: 組み込みコマンド定義 (icon, label, keybind, Cmd) の一覧。
    pub(super) fn palette_builtin_cmds(&self) -> Vec<(String, String, String, Cmd)> {
        // 実際に効いているキーバインドをそのまま出す (config で上書きされていても
        // パレットの表示とズレない)
        let fmt_key = |a: BindAction| self.keys.label(a);
        let mut rows: Vec<(String, String, String, Cmd)> = vec![
            (
                "💾".into(),
                tr("保存"),
                fmt_key(BindAction::Save),
                Cmd::Save,
            ),
            (
                "💾".into(),
                tr("名前を付けて保存"),
                fmt_key(BindAction::SaveAs),
                Cmd::SaveAs,
            ),
            (
                "📄".into(),
                tr("新規ファイル"),
                fmt_key(BindAction::NewFile),
                Cmd::NewFile,
            ),
            (
                "🪟".into(),
                tr("新しいウィンドウ"),
                fmt_key(BindAction::NewWindow),
                Cmd::NewWindow,
            ),
            (
                "📂".into(),
                tr("フォルダを開く…"),
                String::new(),
                Cmd::OpenFolder,
            ),
            (
                "🪟".into(),
                tr("新しいウィンドウでフォルダーを開く…"),
                String::new(),
                Cmd::NewWindowFolder,
            ),
            (
                "📚".into(),
                tr("フォルダをワークスペースに追加"),
                String::new(),
                Cmd::AddFolder,
            ),
            (
                "❌".into(),
                tr("タブを閉じる"),
                fmt_key(BindAction::CloseTab),
                Cmd::CloseTab,
            ),
            (
                "🔍".into(),
                tr("ファイル内検索"),
                fmt_key(BindAction::Find),
                Cmd::OpenFind,
            ),
            (
                "🖥".into(),
                tr("ターミナル表示切替"),
                fmt_key(BindAction::ToggleTerminal),
                Cmd::ToggleTerminal,
            ),
            (
                "⏱".into(),
                tr("チェックポイント: 一覧"),
                String::new(),
                Cmd::CheckpointList,
            ),
            (
                "⏱".into(),
                tr("チェックポイント: 今すぐ取る"),
                String::new(),
                Cmd::CheckpointNow,
            ),
            (
                "🕰".into(),
                tr("ローカルヒストリ (取り消し履歴)"),
                String::new(),
                Cmd::LocalHistoryOpen,
            ),
            (
                "🎛".into(),
                tr("Cockpit 切替"),
                fmt_key(BindAction::ToggleCockpit),
                Cmd::ToggleCockpit,
            ),
            (
                "📋".into(),
                tr("フリート看板 切替"),
                fmt_key(BindAction::ToggleKanban),
                Cmd::ToggleKanban,
            ),
            (
                deck::DECK_ICON.into(),
                tr("エージェントデッキ"),
                fmt_key(BindAction::ToggleDeck),
                Cmd::ToggleDeck,
            ),
            (
                "🎯".into(),
                tr("エージェントを追従 (編集中の行をエディタが追いかける)"),
                fmt_key(BindAction::FollowAgent),
                Cmd::ToggleFollowAgent,
            ),
            (
                "▶".into(),
                tr("追従を再開"),
                fmt_key(BindAction::FollowResume),
                Cmd::ResumeFollowAgent,
            ),
            (
                "◆".into(),
                tr("次の未読エージェントへ"),
                fmt_key(BindAction::NextUnread),
                Cmd::NextUnreadAgent,
            ),
            (
                "📩".into(),
                tr("あとで見る (未読に戻して次へ)"),
                fmt_key(BindAction::DeferUnread),
                Cmd::DeferUnreadAgent,
            ),
            (
                "📩".into(),
                tr("未読の切り替え"),
                fmt_key(BindAction::ToggleUnread),
                Cmd::ToggleUnreadAgent,
            ),
            (
                "➕".into(),
                tr("エージェントを追加 (対応 CLI の一覧から選ぶ)"),
                String::new(),
                Cmd::OpenAgentPicker,
            ),
            (
                "📋".into(),
                tr("タスクを作成してエージェントに割り当て"),
                String::new(),
                Cmd::NewTask,
            ),
            (
                "🏁".into(),
                tr("プロンプトレース (1 プロンプトを複数エージェントで並走)"),
                String::new(),
                Cmd::OpenRace,
            ),
            (
                "🏆".into(),
                tr("レースの勝者を評価 (勝者と理由を提案。採用はしません)"),
                String::new(),
                Cmd::EvalRace,
            ),
            (
                "📮".into(),
                tr("エージェントへメッセージを送る"),
                String::new(),
                Cmd::SendAgentMessage,
            ),
            (
                "👁".into(),
                tr("Markdown/HTML プレビュー切替"),
                fmt_key(BindAction::ToggleMdPreview),
                Cmd::ToggleMdPreview,
            ),
            (
                "↩".into(),
                tr("折り返し切替"),
                String::new(),
                Cmd::ToggleWordWrap,
            ),
            (
                "·".into(),
                tr("空白文字表示切替"),
                String::new(),
                Cmd::ToggleShowWhitespace,
            ),
            (
                "▥".into(),
                tr("エディタを右に分割"),
                fmt_key(BindAction::SplitEditorRight),
                Cmd::SplitEditorRight,
            ),
            (
                "▤".into(),
                tr("エディタを下に分割"),
                fmt_key(BindAction::SplitEditorDown),
                Cmd::SplitEditorDown,
            ),
            (
                "▢".into(),
                tr("エディタの分割を解除"),
                String::new(),
                Cmd::UnsplitEditor,
            ),
            (
                "⇥".into(),
                tr("次のエディタペインへ"),
                fmt_key(BindAction::FocusPane2),
                Cmd::FocusNextPane,
            ),
            (
                "⇨".into(),
                tr("タブを次のペインへ移動"),
                String::new(),
                Cmd::MoveTabToNextPane,
            ),
            (
                "🗺".into(),
                tr("ミニマップの表示切替"),
                String::new(),
                Cmd::ToggleMinimap,
            ),
            (
                "🐚".into(),
                tr("シェル統合 (OSC 633) の切替 — コマンドの終了コードを画面から推測しない"),
                String::new(),
                Cmd::ToggleShellIntegration,
            ),
            (
                "🔗".into(),
                tr("ブレッドクラムの表示切替"),
                String::new(),
                Cmd::ToggleBreadcrumbs,
            ),
            (
                "👤".into(),
                tr("Git blame: 次の段へ (出さない → カーソル行 → 全行)"),
                String::new(),
                Cmd::ToggleGitBlame,
            ),
            (
                "📁".into(),
                tr("サイドバー切替"),
                fmt_key(BindAction::ToggleSidebar),
                Cmd::ToggleSidebar,
            ),
            (
                "🌿".into(),
                tr("Git パネルを開く"),
                String::new(),
                Cmd::OpenGitPanel,
            ),
            (
                "✔".into(),
                tr("Git: ステージした変更をコミット…"),
                String::new(),
                Cmd::GitCommit(false),
            ),
            (
                "✔".into(),
                tr("Git: すべての変更をコミット…"),
                String::new(),
                Cmd::GitCommit(true),
            ),
            (
                "⬆".into(),
                tr("Git: push (origin へ)"),
                String::new(),
                Cmd::GitPush,
            ),
            (
                "⬇".into(),
                tr("Git: pull (早送りのみ)"),
                String::new(),
                Cmd::GitPull,
            ),
            (
                "🕘".into(),
                tr("Git: コミット履歴"),
                String::new(),
                Cmd::GitHistory,
            ),
            (
                "±".into(),
                tr("マルチバッファ: 未コミットの変更をまとめて直す"),
                String::new(),
                Cmd::OpenChangesMultibuffer,
            ),
            (
                "🔎".into(),
                tr("マルチバッファ: 検索結果をまとめて直す"),
                String::new(),
                Cmd::OpenSearchMultibuffer,
            ),
            (
                "⚠".into(),
                tr("マルチバッファ: 問題をまとめて直す"),
                String::new(),
                Cmd::OpenProblemsMultibuffer,
            ),
            (
                "👾".into(),
                tr("現在のファイルをエージェントに送信 (@path)"),
                String::new(),
                Cmd::SendFileToAgent,
            ),
            (
                "⟳".into(),
                tr("アクティブなエージェントを再起動"),
                String::new(),
                Cmd::RestartAgent,
            ),
            (
                "🗑".into(),
                tr("アクティブなエージェントを終了"),
                String::new(),
                Cmd::KillAgent,
            ),
            (
                "⚙".into(),
                tr("設定を開く"),
                String::new(),
                Cmd::OpenSettings,
            ),
            (
                "📝".into(),
                tr("設定 config.toml を開く"),
                String::new(),
                Cmd::OpenConfig,
            ),
            (
                "🔄".into(),
                tr("設定を再読み込み"),
                String::new(),
                Cmd::ReloadConfig,
            ),
            // ズームは対象が 2 つある。ラベルで対象を必ず言う
            // (「拡大」だけだと何が大きくなるのか分からない)
            (
                "🔍".into(),
                tr("画面全体をズームイン"),
                fmt_key(BindAction::ZoomIn),
                Cmd::ZoomIn,
            ),
            (
                "🔍".into(),
                tr("画面全体をズームアウト"),
                fmt_key(BindAction::ZoomOut),
                Cmd::ZoomOut,
            ),
            (
                "🔍".into(),
                tr("画面全体のズームを 100% に戻す"),
                fmt_key(BindAction::ZoomReset),
                Cmd::ZoomReset,
            ),
            (
                "🔎".into(),
                tr("このファイルだけズームイン"),
                fmt_key(BindAction::FileZoomIn),
                Cmd::FileZoomIn,
            ),
            (
                "🔎".into(),
                tr("このファイルだけズームアウト"),
                fmt_key(BindAction::FileZoomOut),
                Cmd::FileZoomOut,
            ),
            (
                "🔎".into(),
                tr("このファイルのズームを解除"),
                fmt_key(BindAction::FileZoomReset),
                Cmd::FileZoomReset,
            ),
            // 文字サイズは「ズーム」と別物。ラベルで違いを言い切る
            // (ズームは余白まで大きくなり画面に入る情報が減るが、こちらは減らない)。
            (
                "🔠".into(),
                tr("文字サイズを大きく (レイアウトは変えない)"),
                String::new(),
                Cmd::TextSizeIn,
            ),
            (
                "🔠".into(),
                tr("文字サイズを小さく (レイアウトは変えない)"),
                String::new(),
                Cmd::TextSizeOut,
            ),
            (
                "🔠".into(),
                tr("文字サイズを 100% に戻す"),
                String::new(),
                Cmd::TextSizeReset,
            ),
            (
                "🌲".into(),
                tr("ファイルツリー再読み込み"),
                String::new(),
                Cmd::RefreshTree,
            ),
            (
                "🛡".into(),
                tr("承認モード: 毎回ユーザー承認 (Claude/Codex/Antigravity)"),
                String::new(),
                Cmd::SetApproval("ask".into()),
            ),
            (
                "⚡".into(),
                tr("承認モード: 全自動 YES (Claude/Codex/Antigravity)"),
                String::new(),
                Cmd::SetApproval("auto".into()),
            ),
            (
                "👾".into(),
                tr("承認モード: Agent欄優先 (プリセットのコマンドどおり)"),
                String::new(),
                Cmd::SetApproval("agent".into()),
            ),
            (
                "🐾".into(),
                tr("ペット表示切替"),
                String::new(),
                Cmd::TogglePet,
            ),
            (
                "📱".into(),
                tr("スマホリモート (QR コード表示)"),
                String::new(),
                Cmd::ToggleRemote,
            ),
            (
                "🔐".into(),
                tr("リモート接続 (Tailscale / SSH) — 同じ Wi-Fi でなくてもスマホから繋ぐ"),
                String::new(),
                Cmd::OpenSshRemote,
            ),
            (
                "🎤".into(),
                tr("音声入力: 全エージェントの入力欄へ (送信は自分で Enter)"),
                String::new(),
                Cmd::VoiceInput(voice::Target::Broadcast),
            ),
            (
                "🛡".into(),
                tr("実行中の全エージェントの権限モードを切替"),
                String::new(),
                Cmd::CyclePermissionAll,
            ),
            (
                "🖼".into(),
                tr("ペット画像を変更…"),
                String::new(),
                Cmd::SetPetImage,
            ),
            (
                "↺".into(),
                tr("ペット画像を既定に戻す"),
                String::new(),
                Cmd::ResetPetImage,
            ),
            (
                "🐾".into(),
                tr("ペット位置を右下に戻す"),
                String::new(),
                Cmd::ResetPetPos,
            ),
            // ── VS Code 準拠メニューバーのコマンド ──
            (
                "📄".into(),
                tr("ファイルを開く…"),
                fmt_key(BindAction::OpenFile),
                Cmd::OpenFileDialog,
            ),
            (
                "💾".into(),
                tr("すべて保存"),
                fmt_key(BindAction::SaveAll),
                Cmd::SaveAll,
            ),
            (
                "💾".into(),
                tr("自動保存の切替"),
                String::new(),
                Cmd::ToggleAutoSave,
            ),
            (
                "↺".into(),
                tr("ファイルを元に戻す"),
                String::new(),
                Cmd::RevertFile,
            ),
            (
                "🚪".into(),
                tr("すべてのエディターを閉じる"),
                String::new(),
                Cmd::CloseAllTabs,
            ),
            (
                "📌".into(),
                tr("タブのピン留めを切り替える"),
                String::new(),
                Cmd::TogglePinTab,
            ),
            (
                "📑".into(),
                tr("タブ切替を 最近使った順 / 並び順 で切り替える"),
                String::new(),
                Cmd::ToggleTabSwitchMru,
            ),
            (
                "📄".into(),
                tr("プレビュータブの切替"),
                String::new(),
                Cmd::TogglePreviewTabs,
            ),
            (
                "🔎".into(),
                tr("ファイル間で検索"),
                fmt_key(BindAction::GlobalSearch),
                Cmd::GlobalSearch,
            ),
            (
                "⇄".into(),
                tr("置換"),
                fmt_key(BindAction::OpenReplace),
                Cmd::OpenReplace,
            ),
            // 🧭 は同梱フォントに字が無く豆腐(□)になるため「→」を使う
            // (glyph_tests::ui_glyph_symbols_have_glyphs が担保している字)。
            (
                "→".into(),
                tr("行/列へ移動…"),
                fmt_key(BindAction::GoToLine),
                Cmd::GoToLine,
            ),
            (
                "→".into(),
                tr("定義へ移動"),
                fmt_key(BindAction::GoToDefinition),
                Cmd::GoToDefinition,
            ),
            (
                "→".into(),
                tr("ブラケットへ移動"),
                fmt_key(BindAction::GoToBracket),
                Cmd::GoToBracket,
            ),
            (
                "⬅".into(),
                tr("戻る"),
                fmt_key(BindAction::NavBack),
                Cmd::NavBack,
            ),
            (
                "➡".into(),
                tr("進む"),
                fmt_key(BindAction::NavForward),
                Cmd::NavForward,
            ),
            (
                "📑".into(),
                tr("次のエディター"),
                fmt_key(BindAction::NextTab),
                Cmd::NextTab,
            ),
            (
                "📑".into(),
                tr("前のエディター"),
                fmt_key(BindAction::PrevTab),
                Cmd::PrevTab,
            ),
            (
                "🖥".into(),
                tr("新しいターミナル"),
                fmt_key(BindAction::NewTerminal),
                Cmd::NewTerminal,
            ),
            (
                "▶".into(),
                tr("アクティブなファイルを実行"),
                String::new(),
                Cmd::RunActiveFile,
            ),
            (
                "▶".into(),
                tr("選択したテキストをターミナルへ送る"),
                String::new(),
                Cmd::RunSelection,
            ),
            (
                "🔨".into(),
                tr("ビルド タスクの実行…"),
                fmt_key(BindAction::RunBuildTask),
                Cmd::RunBuildTask,
            ),
            (
                "⚠".into(),
                tr("問題パネルの切替"),
                fmt_key(BindAction::ToggleProblems),
                Cmd::ToggleProblems,
            ),
            (
                "⤓".into(),
                tr("次の問題へ"),
                fmt_key(BindAction::NextProblem),
                Cmd::NextProblem,
            ),
            (
                "⤒".into(),
                tr("前の問題へ"),
                fmt_key(BindAction::PrevProblem),
                Cmd::PrevProblem,
            ),
            (
                "💬".into(),
                tr("行末の診断メッセージ切替"),
                String::new(),
                Cmd::ToggleInlineDiagnostics,
            ),
            (
                "🏷".into(),
                tr("インラインヒントの表示切替"),
                String::new(),
                Cmd::ToggleInlayHints,
            ),
            // ── 第 2 次配線: レビュー / 折りたたみ / ブックマーク / 表 / LSP ──
            (
                "🔎".into(),
                tr("変更をレビュー (PR 風のローカルレビュー)"),
                String::new(),
                Cmd::OpenReview,
            ),
            (
                "⇋".into(),
                tr("差分の表示を切替 (並列 ⇔ 一列)"),
                String::new(),
                Cmd::ToggleDiffView,
            ),
            (
                "⤓".into(),
                tr("差分: 次の変更へ"),
                fmt_key(BindAction::DiffNextChange),
                Cmd::DiffNextChange,
            ),
            (
                "⤒".into(),
                tr("差分: 前の変更へ"),
                fmt_key(BindAction::DiffPrevChange),
                Cmd::DiffPrevChange,
            ),
            (
                "⇥".into(),
                tr("レビュー: 次の差分ファイルへ (レビュー済みは飛ばす)"),
                fmt_key(BindAction::DiffNextFile),
                Cmd::DiffNextFile,
            ),
            (
                "⇤".into(),
                tr("レビュー: 前の差分ファイルへ"),
                fmt_key(BindAction::DiffPrevFile),
                Cmd::DiffPrevFile,
            ),
            (
                "☑".into(),
                tr("レビュー: このファイルをレビュー済みにする / 戻す"),
                String::new(),
                Cmd::DiffMarkViewed,
            ),
            (
                "🎯".into(),
                tr("レビュー: 1 ファイルに集中する (Focus Mode)"),
                String::new(),
                Cmd::SetReviewMode("focus".into()),
            ),
            (
                "📋".into(),
                tr("レビュー: 横断ハンクキュー (採用 / 却下)"),
                String::new(),
                Cmd::SetReviewMode("queue".into()),
            ),
            (
                "🗂".into(),
                tr("レビュー: ファイル一覧に戻す"),
                String::new(),
                Cmd::SetReviewMode("files".into()),
            ),
            (
                "⇔".into(),
                tr("保存済みと比較 (編集中の本文 vs ディスク)"),
                String::new(),
                Cmd::CompareWithSaved,
            ),
            (
                "◧".into(),
                tr("比較の左側として選ぶ"),
                String::new(),
                Cmd::SelectForCompare,
            ),
            (
                "◨".into(),
                tr("選んだファイルと比較"),
                String::new(),
                Cmd::CompareWithSelected,
            ),
            (
                "🔎".into(),
                tr("レビューの比較: 作業ツリー vs HEAD"),
                String::new(),
                Cmd::SetReviewBase("head".into()),
            ),
            (
                "🔎".into(),
                tr("レビューの比較: ステージ済みだけ"),
                String::new(),
                Cmd::SetReviewBase("staged".into()),
            ),
            (
                "🔎".into(),
                tr("レビューの比較: 未ステージだけ"),
                String::new(),
                Cmd::SetReviewBase("unstaged".into()),
            ),
            (
                "▾".into(),
                tr("折りたたみ切替"),
                fmt_key(BindAction::ToggleFold),
                Cmd::ToggleFold,
            ),
            (
                "▸".into(),
                tr("すべて折りたたむ"),
                String::new(),
                Cmd::FoldAll,
            ),
            (
                "▾".into(),
                tr("すべて展開する"),
                fmt_key(BindAction::UnfoldAll),
                Cmd::UnfoldAll,
            ),
            (
                "▸".into(),
                tr("レベル 1 で折りたたむ"),
                String::new(),
                Cmd::FoldLevel(1),
            ),
            (
                "▸".into(),
                tr("レベル 2 で折りたたむ"),
                String::new(),
                Cmd::FoldLevel(2),
            ),
            (
                "▸".into(),
                tr("レベル 3 で折りたたむ"),
                String::new(),
                Cmd::FoldLevel(3),
            ),
            (
                "◆".into(),
                tr("ブックマーク切替"),
                fmt_key(BindAction::ToggleBookmark),
                Cmd::ToggleBookmark,
            ),
            (
                "◆".into(),
                tr("次のブックマークへ"),
                String::new(),
                Cmd::NextBookmark,
            ),
            (
                "◆".into(),
                tr("前のブックマークへ"),
                String::new(),
                Cmd::PrevBookmark,
            ),
            (
                "◆".into(),
                tr("ブックマークをすべて解除"),
                String::new(),
                Cmd::ClearBookmarks,
            ),
            (
                "🔖".into(),
                tr("ニーモニック付きブックマーク"),
                fmt_key(BindAction::MarkToggleMnemonic),
                Cmd::MarkToggleMnemonic,
            ),
            (
                "🔖".into(),
                tr("ブックマーク一覧"),
                fmt_key(BindAction::MarksPanel),
                Cmd::MarksPanel,
            ),
            (
                "🔖".into(),
                tr("ブックマークへジャンプ"),
                fmt_key(BindAction::MarkJump),
                Cmd::MarkJump,
            ),
            (
                "🔖".into(),
                tr("ブックマークをプロジェクト全体から消す"),
                String::new(),
                Cmd::MarksClearAll,
            ),
            (
                "📑".into(),
                tr("閉じたタブを開き直す"),
                fmt_key(BindAction::ReopenClosedTab),
                Cmd::ReopenClosedTab,
            ),
            (
                "📊".into(),
                tr("テーブル表示の切替 (CSV / TSV)"),
                String::new(),
                Cmd::ToggleTableView,
            ),
            (
                "💡".into(),
                tr("補完候補を出す"),
                fmt_key(BindAction::LspCompletion),
                Cmd::LspCompletion,
            ),
            (
                "🔍".into(),
                tr("参照を検索"),
                fmt_key(BindAction::LspReferences),
                Cmd::LspReferences,
            ),
            (
                "🔗".into(),
                tr("シンボルにジャンプ"),
                fmt_key(BindAction::LspSymbols),
                Cmd::LspSymbols,
            ),
            (
                "✏".into(),
                tr("リネーム"),
                fmt_key(BindAction::LspRename),
                Cmd::LspRename,
            ),
            (
                "🛠".into(),
                tr("ドキュメントを整形"),
                fmt_key(BindAction::LspFormat),
                Cmd::LspFormat,
            ),
            (
                "💡".into(),
                tr("クイックフィックス"),
                fmt_key(BindAction::LspCodeAction),
                Cmd::LspCodeAction,
            ),
            (
                "()".into(),
                tr("引数ヒントを表示"),
                fmt_key(BindAction::LspSignatureHelp),
                Cmd::LspSignatureHelp,
            ),
            (
                "🔆".into(),
                tr("同一シンボルのハイライト切替"),
                String::new(),
                Cmd::ToggleLspHighlight,
            ),
            (
                "🛠".into(),
                tr("保存時に整形するかの切替"),
                String::new(),
                Cmd::ToggleFormatOnSave,
            ),
            (
                "🖥".into(),
                tr("フルスクリーンの切替"),
                fmt_key(BindAction::ToggleFullScreen),
                Cmd::ToggleFullScreen,
            ),
            (
                "🐙".into(),
                tr("GitHub パネルを開く"),
                String::new(),
                Cmd::ShowGitHubTab,
            ),
            (
                "⌨".into(),
                tr("キーボード ショートカットの設定"),
                fmt_key(BindAction::KeybindEditor),
                Cmd::ShowShortcuts,
            ),
            (
                "ℹ".into(),
                tr("バージョン情報"),
                String::new(),
                Cmd::ShowAbout,
            ),
            (
                "🔑".into(),
                tr("ライセンスキーを入力…"),
                String::new(),
                Cmd::OpenLicense,
            ),
            (
                "➕".into(),
                tr("新規プラグインを作成…"),
                String::new(),
                Cmd::NewPlugin,
            ),
            (
                "📦".into(),
                tr("プラグインをインストール… (.zvplug / .zip)"),
                String::new(),
                Cmd::InstallPlugin,
            ),
            (
                "🔌".into(),
                tr("プラグインを表示"),
                String::new(),
                Cmd::ShowPlugins,
            ),
            (
                "⟳".into(),
                tr("プラグインを再スキャン"),
                String::new(),
                Cmd::RescanPlugins,
            ),
            // ── 横断検索のオプション ──
            (
                "⇄".into(),
                tr("ファイル間で置換…"),
                fmt_key(BindAction::GlobalReplace),
                Cmd::GlobalReplace,
            ),
            (
                "Aa".into(),
                tr("検索: 大文字と小文字を区別する (切替)"),
                String::new(),
                Cmd::ToggleSearchCase,
            ),
            (
                "Ab|".into(),
                tr("検索: 単語単位で検索する (切替)"),
                String::new(),
                Cmd::ToggleSearchWholeWord,
            ),
            (
                ".*".into(),
                tr("検索: 正規表現を使用する (切替)"),
                String::new(),
                Cmd::ToggleSearchRegex,
            ),
            // ── セッション / 使用量 ──
            (
                "💬".into(),
                tr("セッション: 過去の会話を表示"),
                String::new(),
                Cmd::ShowSessions,
            ),
            (
                "📊".into(),
                tr("プラン使用量と枯渇予測を表示"),
                String::new(),
                Cmd::ShowQuota,
            ),
            (
                "🔁".into(),
                if self.failover.enabled() {
                    tr("自動フェイルオーバーを無効化 (レート制限時の自動切替)")
                } else {
                    tr("自動フェイルオーバーを有効化 (レート制限時の自動切替)")
                },
                String::new(),
                Cmd::ToggleFailover,
            ),
            // ── 保存時のクリーンアップ / 改行コード ──
            (
                "·".into(),
                tr("保存時に末尾空白を除去 (切替)"),
                String::new(),
                Cmd::ToggleTrimTrailingOnSave,
            ),
            (
                "↩".into(),
                tr("保存時に最終行へ改行を入れる (切替)"),
                String::new(),
                Cmd::ToggleFinalNewlineOnSave,
            ),
            (
                "⏎".into(),
                tr("保存時に末尾の余分な空行を落とす (切替)"),
                String::new(),
                Cmd::ToggleTrimFinalNewlinesOnSave,
            ),
            // ── 選択範囲への編集コマンド ──
            (
                "AA".into(),
                tr("選択範囲を大文字にする"),
                String::new(),
                Cmd::TransformCase(editor_ops::CaseKind::Upper),
            ),
            (
                "aa".into(),
                tr("選択範囲を小文字にする"),
                String::new(),
                Cmd::TransformCase(editor_ops::CaseKind::Lower),
            ),
            (
                "Aa".into(),
                tr("選択範囲を先頭大文字にする"),
                String::new(),
                Cmd::TransformCase(editor_ops::CaseKind::Title),
            ),
            (
                "↓".into(),
                tr("選択範囲の行を昇順に並べ替える"),
                String::new(),
                Cmd::SortLines(false),
            ),
            (
                "↑".into(),
                tr("選択範囲の行を降順に並べ替える"),
                String::new(),
                Cmd::SortLines(true),
            ),
            (
                "⧉".into(),
                tr("選択範囲の重複行を削除する"),
                String::new(),
                Cmd::DedupeLines,
            ),
            (
                "{}".into(),
                tr("選択範囲を JSON として整形する"),
                String::new(),
                Cmd::FormatJsonSelection,
            ),
            (
                "↩".into(),
                tr("改行コードを変換: LF (Unix)"),
                String::new(),
                Cmd::ConvertLineEnding(crate::textenc::LineEnding::Lf),
            ),
            (
                "↩".into(),
                tr("改行コードを変換: CRLF (Windows)"),
                String::new(),
                Cmd::ConvertLineEnding(crate::textenc::LineEnding::Crlf),
            ),
            (
                "↩".into(),
                tr("改行コードを変換: CR (旧 Mac)"),
                String::new(),
                Cmd::ConvertLineEnding(crate::textenc::LineEnding::Cr),
            ),
            // ── 第 3 次配線: ガイドツアー / 承認キュー / 複数キャレット / 符号化 ──
            (
                "💡".into(),
                tr("チュートリアルを再開"),
                String::new(),
                Cmd::RestartTutorial,
            ),
            (
                "🛡".into(),
                tr("承認キューを開く"),
                String::new(),
                Cmd::OpenApprovals,
            ),
            (
                "📜".into(),
                tr("承認の監査ログを開く"),
                String::new(),
                Cmd::OpenApprovalAudit,
            ),
            (
                "🔌".into(),
                tr("MCP サーバを管理"),
                String::new(),
                Cmd::OpenMcp,
            ),
            (
                "🧩".into(),
                tr("Skills / コマンドを管理"),
                String::new(),
                Cmd::OpenSkills,
            ),
            (
                "✏".into(),
                tr("カーソルを上に追加"),
                String::new(),
                Cmd::AddCursorAbove,
            ),
            (
                "✏".into(),
                tr("カーソルを下に追加"),
                String::new(),
                Cmd::AddCursorBelow,
            ),
            (
                "✏".into(),
                tr("全ての出現を選択"),
                String::new(),
                Cmd::SelectAllOccurrences,
            ),
            (
                "✏".into(),
                tr("次の出現を選択"),
                fmt_key(BindAction::SelectNextOccurrence),
                Cmd::SelectNextOccurrence,
            ),
            (
                "◇".into(),
                tr("矩形選択の開始"),
                String::new(),
                Cmd::ColumnSelectStart,
            ),
            (
                "◇".into(),
                tr("矩形選択の確定"),
                String::new(),
                Cmd::ColumnSelectFinish,
            ),
            (
                "✏".into(),
                tr("複数カーソルを解除"),
                String::new(),
                Cmd::ClearMultiCursor,
            ),
            (
                "✏".into(),
                tr("全キャレットへ貼り付け (取り消しは 1 回)"),
                String::new(),
                Cmd::MultiPaste,
            ),
            (
                "📝".into(),
                tr("エンコーディングを指定して開き直す"),
                String::new(),
                Cmd::ReopenWithEncoding(None),
            ),
            (
                "📝".into(),
                tr("エンコーディングを指定して保存"),
                String::new(),
                Cmd::SaveWithEncoding(None),
            ),
        ];
        // 公開鍵が入っていないビルドでは「ライセンスキーを入力…」を出さない。
        // 番兵 (全ゼロ) のままだと**どんなキーも必ず弾かれる**ので、押しても
        // 絶対に成立しない入口になる。CLAUDE.md の「常に 0 を表示するバッジ」
        // と同じ理由で、機能ではなく雑音として扱い、項目ごと消す。
        // 鍵を入れて焼き直せば同じコードのまま復活する。
        if !crate::license::signing_configured() {
            rows.retain(|(_, _, _, c)| !matches!(c, Cmd::OpenLicense));
        }
        rows
    }

    /// パレット: コマンドモード (`>`) の候補を items へ積む。
    pub(super) fn palette_items_command_mode(
        &self,
        pq: &fuzzy::PreparedQuery,
        items: &mut Vec<Item>,
    ) {
        let mut cmds: Vec<(String, String, String, Cmd)> = self.palette_builtin_cmds();
        // 実際に検出できた外部 IDE だけを出す (検出は起動時にワーカーで走る)
        for (icon, label, cmd) in panels::ide_palette_entries() {
            cmds.push((icon, label, String::new(), cmd));
        }
        // feature.rs のレジストリに登録された機能。**ここが唯一の差し込み口**で、
        // 機能が増えてもこの 1 ブロックは変わらない (並列開発の衝突対策)。
        cmds.extend(crate::feature::palette_entries());
        // 実行中のセッション毎に音声入力エントリを出す (パレットで「音声」検索用)
        for s in self.agents.sessions.iter().take(20) {
            cmds.push((
                "🎤".into(),
                trf(
                    "音声入力: {icon} {title} の入力欄へ (送信は自分で Enter)",
                    &[("icon", s.icon.clone()), ("title", s.title.clone())],
                ),
                String::new(),
                Cmd::VoiceInput(voice::Target::Session(s.id)),
            ));
        }
        for t in theme::all() {
            // 明暗をラベルに入れておくと「ライト」「ダーク」でも絞り込める
            // (テーマ名は英語なので、日本語の入力からは届かない)
            cmds.push((
                "🎨".into(),
                trf(
                    "テーマ: {label} ({kind})",
                    &[
                        ("label", t.label.clone()),
                        ("kind", tr(if t.dark { "ダーク" } else { "ライト" })),
                    ],
                ),
                String::new(),
                Cmd::SetTheme(t.name.clone()),
            ));
        }
        for (label, path) in self.custom_themes.iter().take(80) {
            cmds.push((
                "🔌".into(),
                trf("テーマ (カスタム): {label}", &[("label", label.clone())]),
                String::new(),
                Cmd::SetTheme(path.clone()),
            ));
        }
        for (pi, p) in self.plugins.iter().enumerate() {
            for (ci, c) in p.commands.iter().enumerate() {
                cmds.push((
                    c.icon.clone(),
                    format!("{}: {}", p.name, c.title),
                    c.keybind.clone().unwrap_or_default(),
                    Cmd::RunPlugin(pi, ci),
                ));
            }
        }
        // `.vscode/tasks.json` のタスク。出典が分かる接頭辞を付ける
        // (自動検出のビルドタスクと混ざると、どこを直せばいいか分からなくなる)。
        for (i, t) in self
            .tasks_cache
            .doc
            .tasks
            .iter()
            .enumerate()
            .take(menu_bar::MAX_TASK_ROWS)
        {
            cmds.push((
                "🧰".into(),
                trf(
                    "タスク (tasks.json): {label}",
                    &[("label", t.label.clone())],
                ),
                String::new(),
                Cmd::RunJsonTask(i),
            ));
        }
        // ルートが 2 つ以上のときだけ削除コマンドを出す
        // (最後の 1 つは削除できない = roots は決して空にならない)
        if self.roots.len() > 1 {
            for r in &self.roots {
                cmds.push((
                    "📂".into(),
                    trf(
                        "フォルダをワークスペースから削除: {name}",
                        &[("name", root_name(r))],
                    ),
                    String::new(),
                    Cmd::RemoveFolder(r.clone()),
                ));
            }
        }
        for (i, p) in self.cfg.agents.iter().enumerate() {
            cmds.push((
                p.icon.clone(),
                trf("エージェント起動: {name}", &[("name", p.name.clone())]),
                String::new(),
                Cmd::NewAgent(i),
            ));
        }
        // worktree 隔離での起動。git リポジトリでないフォルダでは候補ごと出さない
        // (押しても必ず失敗するコマンドをパレットに並べない)。
        if worktree::looks_like_git_repo(&self.agent_cwd()) {
            for (i, p) in self.cfg.agents.iter().enumerate() {
                cmds.push((
                    "🌿".into(),
                    trf("worktree 隔離で起動: {name}", &[("name", p.name.clone())]),
                    tr("専用ブランチ agent/… を切って、そこを作業フォルダにする"),
                    Cmd::NewAgentIsolated(i),
                ));
            }
        }
        if self.agents.running_count() > 0 {
            cmds.push((
                "🛑".into(),
                tr("全エージェントを停止"),
                tr("稼働中のエージェントをプロセスツリーごと止めます（確認あり）"),
                Cmd::StopAllAgents,
            ));
        }
        for (i, s) in self.agents.sessions.iter().enumerate() {
            cmds.push((
                s.icon.clone(),
                trf("エージェントへ移動: {title}", &[("title", s.title.clone())]),
                String::new(),
                Cmd::FocusAgent(i),
            ));
            cmds.push((
                "✏️".into(),
                trf(
                    "エージェント名の変更: {title}",
                    &[("title", s.title.clone())],
                ),
                tr("手で付けた名前は、ターン終了時の自動命名に上書きされません"),
                Cmd::RenameAgent(i),
            ));
        }
        for (icon, label, detail, cmd) in cmds {
            if let Some(score) = pq.score(&label) {
                items.push(Item {
                    icon,
                    label,
                    detail,
                    action: Action::Cmd(cmd),
                    score,
                });
            }
        }
    }

    /// パレット: `@` エージェントモードの候補を items へ積む。
    pub(super) fn palette_items_agent_mode(
        &self,
        pq: &fuzzy::PreparedQuery,
        items: &mut Vec<Item>,
    ) {
        // `@` — エージェントセッションへジャンプ / プリセット起動
        for (i, s) in self.agents.sessions.iter().enumerate() {
            let state = tr(if s.running() {
                if s.attention {
                    "🔔 承認待ち"
                } else if s.rate_limited.is_some() {
                    "⏳ レート制限"
                } else {
                    "稼働中"
                }
            } else {
                "終了"
            });
            let unread = if s.has_unread() {
                tr(" ◆未読")
            } else {
                String::new()
            };
            if let Some(score) = pq.score(&s.title) {
                items.push(Item {
                    icon: if s.icon.is_empty() {
                        "👾".into()
                    } else {
                        s.icon.clone()
                    },
                    label: s.title.clone(),
                    detail: format!("{state}{unread} ・ {}", s.uptime()),
                    action: Action::Cmd(Cmd::FocusAgent(i)),
                    // 承認待ち・未読を上へ
                    score: score
                        + if s.attention { 40 } else { 0 }
                        + if s.has_unread() { 20 } else { 0 },
                });
            }
        }
        for (i, p) in self.cfg.agents.iter().enumerate() {
            // 訳語のラベルに対して検索する (英語UIなら英語で当たるように)
            let label = trf("起動: {name}", &[("name", p.name.clone())]);
            if let Some(score) = pq.score(&label) {
                items.push(Item {
                    icon: p.icon.clone(),
                    label,
                    detail: p.command.clone(),
                    action: Action::Cmd(Cmd::NewAgent(i)),
                    score: score - 50, // 既存セッションより下に出す
                });
            }
        }
    }

    /// パレット: `#` ルート / worktree モードの候補を items へ積む。
    pub(super) fn palette_items_root_mode(&self, pq: &fuzzy::PreparedQuery, items: &mut Vec<Item>) {
        // `#` — ワークスペースルートと git worktree の横断
        for r in &self.roots {
            let label = trf(
                "フォルダを外す: {name}",
                &[("name", root_name(r).to_string())],
            );
            if self.roots.len() > 1 {
                if let Some(score) = pq.score(&label) {
                    items.push(Item {
                        icon: "📚".into(),
                        label,
                        detail: r.display().to_string(),
                        action: Action::Cmd(Cmd::RemoveFolder(r.clone())),
                        score: score - 30,
                    });
                }
            }
        }
        for (branch, path, added) in self.palette_worktrees.as_deref().unwrap_or(&[]) {
            if *added {
                continue; // 既にワークスペースにあるものは「外す」側で出る
            }
            let label = trf("worktree を開く: {branch}", &[("branch", branch.clone())]);
            if let Some(score) = pq.score(&label) {
                items.push(Item {
                    icon: "🌿".into(),
                    label,
                    detail: path.display().to_string(),
                    action: Action::Cmd(Cmd::AddFolderPath(path.clone())),
                    score,
                });
            }
        }
        let add_label = tr("フォルダをワークスペースに追加…");
        if let Some(score) = pq.score(&add_label) {
            items.push(Item {
                icon: "📂".into(),
                label: add_label,
                detail: String::new(),
                action: Action::Cmd(Cmd::AddFolder),
                score: score - 60,
            });
        }
    }

    /// パレット: ファイル検索モードの候補を items へ積む。
    ///
    /// VS Code の ⌘P と同じで、**何も打っていないときは「最近開いた順」**。
    /// 索引の残りはその後ろにアルファベット順で続く (並べ替えは palette 側)。
    /// `ファイル名:123[:45]` と書くとその位置を開く候補になる。
    pub(super) fn palette_items_file_mode(&self, items: &mut Vec<Item>) {
        // ランキングは純粋関数に閉じる (テーブルテストで固定できるように)。
        // クエリは `:行` を剥がしてから作り直すので、ここでは共有 pq を使わない。
        items.extend(file_mode_items(
            &self.file_index,
            &self.menu_state.recent_files,
            self.active_file_path().as_deref(),
            self.palette.query(),
        ));
    }

    /// パレット: `@` シンボルモード。
    ///
    /// **新しい LSP 経路は作らない** — ⌘⇧O のピッカーと同じ
    /// `textDocument/documentSymbol` の結果 (`self.lsp_symbols`) を読むだけ。
    /// 要求は `palette_ui` が `request_breadcrumb_symbols` 経由で静かに出す。
    pub(super) fn palette_items_symbol_mode(
        &self,
        pq: &fuzzy::PreparedQuery,
        items: &mut Vec<Item>,
    ) {
        let Some(path) = self.active_file_path() else {
            return;
        };
        if self.lsp_symbols_path.as_deref() != Some(path.as_path()) {
            return; // 別ファイルの結果は出さない (取り違え防止)
        }
        let mut flat: Vec<(usize, String, u8, lsp::Position)> = Vec::new();
        flatten_symbols(&self.lsp_symbols, 0, &mut flat);
        for (depth, name, kind, pos) in flat.into_iter().take(MAX_SYMBOL_ROWS) {
            let Some(score) = pq.score(&name) else {
                continue;
            };
            items.push(Item {
                icon: "◇".into(),
                label: name,
                detail: format!("{}{}", "  ".repeat(depth), symbol_kind_label(kind)),
                action: Action::Cmd(Cmd::GoToLspPos(pos.line, pos.character)),
                // 浅い階層 (トップレベルの定義) を上に出す
                score: score - (depth as i32 * 5),
            });
        }
    }

    /// パレット: `:123` / `:123:45` の行 (列) ジャンプ。
    ///
    /// パースは `editor_ops::parse_goto` ただ 1 本 (⌃G の小窓と同じもの)。
    /// 行数を超える値・0 行目は `char_index_at` が末尾へ丸めるので、
    /// ここでクランプはしない (二重の丸めは挙動を読みにくくする)。
    ///
    /// 日本語入力中は `：１２：４５` のように**数字まで全角**で入るので、
    /// パースの手前で `palette::fold_goto` に通す。畳んでよいのはこのモードの
    /// クエリだけ (理由は `palette::fold_fullwidth_ascii` のドキュメント)。
    pub(super) fn palette_items_goto_mode(&self, items: &mut Vec<Item>) {
        let q = crate::palette::fold_goto(self.palette.query());
        let Some((line, col)) = editor_ops::parse_goto(&q) else {
            return;
        };
        if self.editor.active.is_none() {
            return;
        }
        items.push(Item {
            icon: "↧".into(),
            label: trf("{line} 行目へ移動", &[("line", (line + 1).to_string())]),
            detail: if col > 0 {
                trf("{col} 桁目", &[("col", (col + 1).to_string())])
            } else {
                String::new()
            },
            action: Action::Cmd(Cmd::GoToLineAt(line, col)),
            score: 0,
        });
    }

    /// 各ルートの git worktree 一覧。パレットを開いている間だけキャッシュされる。
    pub(super) fn list_git_worktrees(&self) -> Vec<(String, PathBuf, bool)> {
        let canon = |p: &Path| -> PathBuf { p.canonicalize().unwrap_or_else(|_| p.to_path_buf()) };
        let roots: Vec<PathBuf> = self.roots.iter().map(|r| canon(r)).collect();
        let mut out: Vec<(String, PathBuf, bool)> = Vec::new();
        let mut seen: HashSet<PathBuf> = HashSet::new();
        for root in &self.roots {
            let Ok(o) = crate::procx::hidden_command("git")
                .arg("-C")
                .arg(root)
                .args(["worktree", "list", "--porcelain"])
                .output()
            else {
                continue;
            };
            if !o.status.success() {
                continue;
            }
            let text = crate::textenc::decode_output(&o.stdout);
            let mut path: Option<PathBuf> = None;
            let mut branch = String::new();
            for line in text.lines().chain(std::iter::once("")) {
                if let Some(p) = line.strip_prefix("worktree ") {
                    path = Some(PathBuf::from(p));
                } else if let Some(b) = line.strip_prefix("branch refs/heads/") {
                    branch = b.to_string();
                } else if line == "detached" {
                    branch = "(detached)".into();
                } else if line.is_empty() {
                    if let Some(p) = path.take() {
                        let cp = canon(&p);
                        if seen.insert(cp.clone()) {
                            let added = roots.contains(&cp);
                            let label = if branch.is_empty() {
                                p.file_name()
                                    .map(|s| s.to_string_lossy().into_owned())
                                    .unwrap_or_default()
                            } else {
                                std::mem::take(&mut branch)
                            };
                            out.push((label, p, added));
                        }
                    }
                    branch.clear();
                }
            }
        }
        out
    }

    pub(super) fn palette_ui(&mut self, ctx: &egui::Context) {
        if !self.palette.open {
            return;
        }
        // `#` モードに入った最初のフレームで worktree 一覧を取り込む
        // (git はここでしか叩かない。閉じたら破棄)。
        if self.palette.is_root_mode() && self.palette_worktrees.is_none() {
            self.palette_worktrees = Some(self.list_git_worktrees());
        }
        // `@` モードは ⌘⇧O のピッカーと同じ documentSymbol の結果を読む。
        // 無ければ静かに取りに行く (ピッカーは開かない = 画面は急に変わらない)。
        if self.palette.is_symbol_mode() {
            if let Some(p) = self.active_file_path() {
                self.request_breadcrumb_symbols(&p);
            }
        }
        // 復元した MRU (state.toml) に実体を結び直す。組み込みコマンド表は
        // パレットを開いている間しか作らないので、ここが唯一の機会。
        if self.palette.needs_rehydrate() {
            let table: Vec<(String, String, Cmd)> = self
                .palette_builtin_cmds()
                .into_iter()
                .map(|(icon, label, _, cmd)| (icon, label, cmd))
                .collect();
            self.palette.rehydrate(|label| {
                table
                    .iter()
                    .find(|(_, l, _)| l == label)
                    .map(|(icon, _, cmd)| (icon.clone(), Action::Cmd(cmd.clone())))
            });
        }
        let theme = self.theme.clone();
        let results = self.palette_items();
        // ⌘P 連打で 1 つずつ下へ (端で先頭へ折り返す)。
        let cycles = self.palette.take_cycle();
        if cycles > 0 {
            self.palette.selected = crate::palette::cycle(&results, self.palette.selected, cycles);
        }
        let mut execute: Option<Item> = None;
        let mut close = false;

        egui::Area::new(egui::Id::new("zv-palette"))
            .order(egui::Order::Foreground)
            .anchor(Align2::CENTER_TOP, egui::vec2(0.0, 100.0))
            .show(ctx, |ui| {
                egui::Frame::none()
                    .fill(theme.panel)
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        theme.accent.gamma_multiply(0.55),
                    ))
                    .rounding(egui::Rounding::same(10.0))
                    .inner_margin(egui::Margin::same(10.0))
                    .shadow(egui::epaint::Shadow {
                        offset: egui::vec2(0.0, 8.0),
                        blur: 24.0,
                        spread: 0.0,
                        color: Color32::from_black_alpha(140),
                    })
                    .show(ui, |ui| {
                        ui.set_width(640.0);

                        let resp = ui.add(
                            egui::TextEdit::singleline(&mut self.palette.input)
                                .hint_text(tr(
                                    "ファイル検索…  （> コマンド / @ シンボル / :行 / % エージェント / # worktree）",
                                ))
                                .font(FontId::proportional(16.0))
                                .desired_width(f32::INFINITY),
                        );
                        tutorial::anchor(ctx, AnchorId::CommandPalette, resp.rect);
                        if self.palette.just_opened {
                            resp.request_focus();
                            self.palette.just_opened = false;
                        }
                        if resp.changed() {
                            self.palette.selected = 0;
                        }

                        let (down, up, enter, escape) = ctx.input(|i| {
                            // IME イベントと同じフレームの Enter は変換確定なので
                            // コマンド実行に使わない (Windows / Linux の IME 対策。
                            // macOS は winit 側で確定 Enter が抑止される)
                            let ime = i.events.iter().any(|e| matches!(e, egui::Event::Ime(_)));
                            (
                                i.key_pressed(egui::Key::ArrowDown),
                                i.key_pressed(egui::Key::ArrowUp),
                                i.key_pressed(egui::Key::Enter) && !ime,
                                i.key_pressed(egui::Key::Escape),
                            )
                        });
                        if escape {
                            close = true;
                        }
                        // 見出しは選べない。↑↓ は見出しを飛び越して端で折り返す。
                        self.palette.selected = results.step(self.palette.selected, down, up);
                        if enter && !close {
                            if let Some(it) = results.selected_item(self.palette.selected) {
                                execute = Some(it.clone());
                            }
                            close = true;
                        }
                        if !close && !resp.has_focus() {
                            resp.request_focus();
                        }

                        // ファイル検索モードのときは索引の状態を必ず出す
                        // (作成中か、上限で打ち切ったか)。黙って切らない。
                        if !self.palette.is_command_mode()
                            && !self.palette.is_agent_mode()
                            && !self.palette.is_root_mode()
                        {
                            if let Some(note) = self.index_note() {
                                ui.add_space(4.0);
                                ui.add(
                                    egui::Label::new(
                                        RichText::new(note).size(11.5).color(theme.warn),
                                    )
                                    .truncate(),
                                );
                            }
                        }
                        ui.add_space(6.0);
                        egui::ScrollArea::vertical()
                            .id_salt("palette-list")
                            .max_height(420.0)
                            .show(ui, |ui| {
                                // 行の描き方 (見出し / 候補 / 空・不一致の案内) は
                                // palette 側の 1 本にまとめてある。ここは繋ぐだけ。
                                if let Some(it) = crate::palette::list_ui(
                                    ui,
                                    &theme,
                                    &results,
                                    self.palette.selected,
                                    down || up || cycles > 0,
                                ) {
                                    execute = Some(it);
                                    close = true;
                                }
                            });
                    });
            });

        if close {
            self.palette.close();
            self.palette_worktrees = None;
        }
        if let Some(it) = execute {
            // 使った実績を憶えて次回の並びに効かせる (よく使う操作が上がる)。
            self.palette.note_used(&it);
            self.persist_palette_recent();
            self.run_action(it.action, ctx);
        }
    }

    /// パレットの MRU を state.toml へ書き戻す。**変わったときだけ**書く
    /// (ファイルを開いただけで毎回ディスクを触らない)。
    pub(super) fn persist_palette_recent(&mut self) {
        let now: Vec<config::PaletteRecent> = self
            .palette
            .recent_snapshot()
            .into_iter()
            .map(|(label, icon, uses)| config::PaletteRecent { label, icon, uses })
            .collect();
        if now == self.cfg.palette_recent {
            return;
        }
        self.cfg.palette_recent = now;
        config::save_state(&self.cfg);
    }
}
