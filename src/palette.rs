use std::path::PathBuf;

#[derive(Clone)]
pub enum Cmd {
    Save,
    SaveAs,
    CloseTab,
    NewFile,
    /// ワークスペースを「置き換える」(従来どおり)
    OpenFolder,
    /// フォルダをワークスペースに追加する (マルチルート)
    AddFolder,
    /// 指定パスをワークスペースに追加する (`#` パレットの git worktree 追加)
    AddFolderPath(PathBuf),
    /// 指定フォルダをワークスペースから削除する (最後の 1 つは削除できない)
    RemoveFolder(PathBuf),
    ToggleTerminal,
    ToggleCockpit,
    /// フリート看板 (全エージェントを状態列で俯瞰・指揮するカンバン画面) 切替
    ToggleKanban,
    /// タスク作成フォームを開く (Cockpit も一緒に開く)
    NewTask,
    /// エージェントへのメッセージ送信フォームを開く
    SendAgentMessage,
    /// アクティブな Markdown ファイルのレンダリングプレビュー切替
    ToggleMdPreview,
    ToggleSidebar,
    /// サイドバーを Git タブで開く
    OpenGitPanel,
    OpenFind,
    NewAgent(usize),
    /// カタログ全 CLI から選んでプリセットを追加するピッカーを開く
    OpenAgentPicker,
    FocusAgent(usize),
    RestartAgent,
    KillAgent,
    SetTheme(String),
    OpenConfig,
    ReloadConfig,
    FontInc,
    FontDec,
    SendFileToAgent,
    RefreshTree,
    /// 既定の承認モード: "ask"(毎回ユーザー承認) | "auto"(全自動YES) | "agent"(Agent欄優先)
    SetApproval(String),
    TogglePet,
    /// 実行中の対応エージェントの権限モードを切替
    CyclePermissionAll,
    SetPetImage,
    ResetPetImage,
    ResetPetPos,
    /// ペットの見た目バリアント ("blocky"|"crab"|"cat"|"cloud")
    SetPetVariant(String),
    /// ペットの表示スケール
    SetPetScale(f32),
    /// アンカーモード時にうろうろ歩くか
    TogglePetFreeRoam,
    /// 放置時に居眠りするか
    TogglePetSleep,
    /// 完了/承認待ち/エラーの効果音
    TogglePetSounds,
    /// 承認待ちの吹き出し表示
    TogglePetBubbles,
    /// 承認プロンプトへの自動YES (オフ=ユーザー承認必須)
    TogglePetAutoYes,
    /// スマホリモートの QR コードウィンドウ表示切替
    ToggleRemote,
    /// 音声入力の録音を開始/停止する。認識テキストは届け先の入力欄へ
    /// 挿入されるだけで、Enter は送られない
    VoiceInput(crate::voice::Target),
    /// 録音を止める (⏹ ボタン)
    VoiceStop,
    /// 音声入力の既定の届け先を変える (アクティブ / ブロードキャスト)
    SetVoiceTarget(crate::voice::Target),
    /// 音声認識エンジン ("auto"|"mac"|"command"|"off")
    SetVoiceEngine(String),
    /// 認識言語 (BCP-47。"ja-JP" など)
    SetVoiceLang(String),
    /// 話すと Enter まで送る合図キーワード (空文字で無効)
    SetVoiceKeyword(String),
    /// 新規プラグインのテンプレートを作成 (名前入力ダイアログを開く)
    NewPlugin,
    /// .zvplug / .zip ファイルを選んでプラグインをインストール
    InstallPlugin,
    /// プラグインを再スキャン
    RescanPlugins,
    /// サイドバーのプラグインタブを開く
    ShowPlugins,
    /// プラグインコマンドを実行 (plugins[i] の commands[j])
    RunPlugin(usize, usize),

    /// 検出済みの外部 IDE (`ide::IdeSpec::key`) で、現在のファイルを
    /// 現在のカーソル行で開く。
    OpenInIde(String),

    /// 検出済みの外部 IDE でワークスペース (primary ルート) を開く。
    OpenFolderInIde(String),

    // ── VS Code 準拠メニューバー (menu_bar.rs) 用 ──────────────────
    /// ファイルを開くダイアログ (VS Code: ⌘O)
    OpenFileDialog,
    /// 最近使ったフォルダをワークスペースとして開き直す
    OpenRecentFolder(PathBuf),
    /// 最近使ったファイルを開く
    OpenRecentFile(PathBuf),
    /// 最近使った項目の履歴をクリア
    ClearRecent,
    /// 開いている全タブを保存 (VS Code: ⌥⌘S)
    SaveAll,
    /// 自動保存 (afterDelay 方式) の切替
    ToggleAutoSave,
    /// アクティブなファイルをディスクの内容へ戻す (VS Code: Revert File)
    RevertFile,
    /// すべてのエディタタブを閉じる (未保存タブは確認を挟む)
    CloseAllTabs,
    /// エディタの編集操作 (フォーカス経由で egui TextEdit に委譲)
    Undo,
    Redo,
    CutSelection,
    CopySelection,
    PasteClipboard,
    SelectAll,
    /// 行コメント切替 (メニューから。ショートカットは EditOp 経由)
    ToggleLineComment,
    /// 行を複製 / 行を上下に移動 (メニューから)
    DuplicateLine,
    MoveLineUp,
    MoveLineDown,
    /// 検索バーを置換モードで開く (VS Code: ⌥⌘F)
    OpenReplace,
    /// サイドバーの横断検索タブを開く (VS Code: ⇧⌘F)
    GlobalSearch,
    /// コマンドパレット / ファイルパレットを開く
    OpenCommandPalette,
    OpenFilePalette,
    /// サイドバーの各タブを開く
    ShowExplorer,
    ShowGitHubTab,
    /// 問題 (LSP 診断) パネルの表示切替 (VS Code: ⇧⌘M)
    ToggleProblems,
    /// フルスクリーン切替 (VS Code: ⌃⌘F)
    ToggleFullScreen,
    /// ナビゲーション履歴 (VS Code: ⌃- / ⌃⇧-)
    NavBack,
    NavForward,
    /// タブ切替 (VS Code: ⇧⌘] / ⇧⌘[)
    NextTab,
    PrevTab,
    /// 定義へ移動 (LSP。VS Code: F12)
    GoToDefinition,
    /// 対応する括弧へ移動 (VS Code: ⇧⌘\)
    GoToBracket,
    /// 行/列へ移動ダイアログ (VS Code: ⌃G)
    GoToLine,
    /// アクティブなファイルを新しいターミナルで実行
    RunActiveFile,
    /// ビルドタスク (cargo build / npm run build / make) を実行 (VS Code: ⇧⌘B)
    RunBuildTask,
    /// 選択テキストをアクティブなターミナルの入力欄へ送る (Enter は送らない)
    RunSelection,
    /// 新しいターミナル (Shell プリセット) を開く (VS Code: ⌃⇧`)
    NewTerminal,
    /// キーボードショートカット一覧ダイアログ
    ShowShortcuts,
    /// バージョン情報ダイアログ
    ShowAbout,
}

#[derive(Clone)]
pub enum Action {
    OpenFile(PathBuf),
    Cmd(Cmd),
}

pub struct Item {
    pub icon: String,
    pub label: String,
    pub detail: String,
    pub action: Action,
    pub score: i32,
}

pub struct Palette {
    pub open: bool,
    pub input: String,
    pub selected: usize,
    pub just_opened: bool,
}

impl Palette {
    pub fn new() -> Self {
        Self {
            open: false,
            input: String::new(),
            selected: 0,
            just_opened: false,
        }
    }

    pub fn open_files(&mut self) {
        self.open = true;
        self.input.clear();
        self.selected = 0;
        self.just_opened = true;
    }

    pub fn open_commands(&mut self) {
        self.open = true;
        self.input = ">".into();
        self.selected = 0;
        self.just_opened = true;
    }

    pub fn close(&mut self) {
        self.open = false;
        self.input.clear();
        self.selected = 0;
    }

    pub fn is_command_mode(&self) -> bool {
        self.input.trim_start().starts_with('>')
    }

    /// `@` で始まる = エージェントセッション / プリセットの横断検索モード。
    pub fn is_agent_mode(&self) -> bool {
        self.input.trim_start().starts_with('@')
    }

    /// `#` で始まる = ワークスペースルート / git worktree の横断検索モード。
    pub fn is_root_mode(&self) -> bool {
        self.input.trim_start().starts_with('#')
    }

    pub fn query(&self) -> &str {
        let t = self.input.trim_start();
        t.strip_prefix('>')
            .or_else(|| t.strip_prefix('@'))
            .or_else(|| t.strip_prefix('#'))
            .map(|s| s.trim_start())
            .unwrap_or(t)
    }
}

#[cfg(test)]
mod tests {
    use super::Palette;

    #[test]
    fn prefixes_route_to_modes_and_query_strips_them() {
        let mut p = Palette::new();
        p.input = "> save".into();
        assert!(p.is_command_mode());
        assert_eq!(p.query(), "save");

        p.input = "@ claude".into();
        assert!(p.is_agent_mode() && !p.is_command_mode() && !p.is_root_mode());
        assert_eq!(p.query(), "claude");

        p.input = "#issue".into();
        assert!(p.is_root_mode());
        assert_eq!(p.query(), "issue");

        // 素の入力はファイル検索 (どのモードでもない)
        p.input = "main.rs".into();
        assert!(!p.is_command_mode() && !p.is_agent_mode() && !p.is_root_mode());
        assert_eq!(p.query(), "main.rs");
    }

    #[test]
    fn new_starts_closed_with_empty_state() {
        let p = Palette::new();
        assert!(!p.open);
        assert!(p.input.is_empty());
        assert_eq!(p.selected, 0);
        assert!(!p.just_opened);
        assert!(!p.is_command_mode() && !p.is_agent_mode() && !p.is_root_mode());
        assert_eq!(p.query(), "");
    }

    #[test]
    fn open_files_resets_input_and_selection() {
        let mut p = Palette::new();
        p.input = "stale".into();
        p.selected = 7;
        p.open_files();
        assert!(p.open);
        assert!(p.input.is_empty());
        assert_eq!(p.selected, 0);
        assert!(p.just_opened);
        assert!(!p.is_command_mode());
    }

    #[test]
    fn open_commands_seeds_prompt_prefix() {
        let mut p = Palette::new();
        p.input = "stale".into();
        p.selected = 3;
        p.open_commands();
        assert!(p.open);
        assert_eq!(p.input, ">");
        assert_eq!(p.selected, 0);
        assert!(p.just_opened);
        assert!(p.is_command_mode());
        // プレフィックスだけならクエリは空
        assert_eq!(p.query(), "");
    }

    #[test]
    fn close_clears_input_and_selection() {
        let mut p = Palette::new();
        p.open_commands();
        p.input = ">save".into();
        p.selected = 2;
        p.close();
        assert!(!p.open);
        assert!(p.input.is_empty());
        assert_eq!(p.selected, 0);
        // close() は just_opened を触らない (現実装どおり)
        assert!(p.just_opened);
        assert!(!p.is_command_mode() && !p.is_agent_mode() && !p.is_root_mode());
        assert_eq!(p.query(), "");
    }

    #[test]
    fn switching_files_to_commands_resets_state() {
        let mut p = Palette::new();
        p.open_files();
        p.input = "main.rs".into();
        p.selected = 5;
        p.open_commands();
        assert!(p.open);
        assert_eq!(p.input, ">");
        assert_eq!(p.selected, 0);
        assert!(p.is_command_mode());
    }

    #[test]
    fn switching_commands_to_files_resets_state() {
        let mut p = Palette::new();
        p.open_commands();
        p.input = "> sav".into();
        p.selected = 4;
        p.open_files();
        assert!(p.open);
        assert!(p.input.is_empty());
        assert_eq!(p.selected, 0);
        assert!(!p.is_command_mode());
    }

    #[test]
    fn mode_predicates_are_mutually_exclusive() {
        let mut p = Palette::new();
        for (input, cmd, agent, root) in [
            (">x", true, false, false),
            ("@x", false, true, false),
            ("#x", false, false, true),
            ("x", false, false, false),
        ] {
            p.input = input.into();
            assert_eq!(p.is_command_mode(), cmd, "input={input:?}");
            assert_eq!(p.is_agent_mode(), agent, "input={input:?}");
            assert_eq!(p.is_root_mode(), root, "input={input:?}");
        }
    }

    #[test]
    fn predicates_ignore_leading_whitespace() {
        let mut p = Palette::new();
        p.input = "   >save".into();
        assert!(p.is_command_mode());
        p.input = "\t@claude".into();
        assert!(p.is_agent_mode());
        p.input = "  #wt".into();
        assert!(p.is_root_mode());
    }

    #[test]
    fn prefix_not_at_start_is_no_mode() {
        let mut p = Palette::new();
        p.input = "a>b".into();
        assert!(!p.is_command_mode() && !p.is_agent_mode() && !p.is_root_mode());
        // プレフィックス扱いされないので入力がそのままクエリになる
        assert_eq!(p.query(), "a>b");
    }

    #[test]
    fn query_prefix_only_is_empty() {
        let mut p = Palette::new();
        for input in [">", "@", "#", ">   ", "  @\t"] {
            p.input = input.into();
            assert_eq!(p.query(), "", "input={input:?}");
        }
    }

    #[test]
    fn query_strips_only_one_prefix() {
        let mut p = Palette::new();
        // 2 文字目以降のプレフィックス文字は残る (現実装どおり)
        p.input = ">>foo".into();
        assert_eq!(p.query(), ">foo");
        p.input = ">@foo".into();
        assert_eq!(p.query(), "@foo");
        p.input = "@#foo".into();
        assert_eq!(p.query(), "#foo");
    }

    #[test]
    fn query_trims_leading_but_keeps_inner_and_trailing() {
        let mut p = Palette::new();
        p.input = "  >  open file ".into();
        assert_eq!(p.query(), "open file ");
        p.input = "  foo  bar ".into();
        assert_eq!(p.query(), "foo  bar ");
    }

    #[test]
    fn query_empty_input_is_empty() {
        let mut p = Palette::new();
        p.input = String::new();
        assert_eq!(p.query(), "");
        p.input = "   ".into();
        assert_eq!(p.query(), "");
    }
}
