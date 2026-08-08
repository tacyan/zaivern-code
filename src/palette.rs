use crate::i18n::{tr, trf};
use std::path::PathBuf;

#[derive(Clone)]
pub enum Cmd {
    Save,
    SaveAs,
    CloseTab,
    NewFile,
    /// ワークスペースを「置き換える」(従来どおり)
    OpenFolder,
    /// 別プロセスとして新しいウィンドウを開く (VS Code: ⇧⌘N)
    NewWindow,
    /// フォルダを選び、新しいウィンドウ (別プロセス) でそのフォルダを開く
    NewWindowFolder,
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
    /// エージェントデッキ (稼働中 / ローカルのセッション / 新規 を縦 1 本で管理する画面) 切替
    ToggleDeck,
    /// タスク作成フォームを開く (Cockpit も一緒に開く)
    NewTask,
    /// プロンプトレースの開始フォームを開く (Cockpit も一緒に開く)
    OpenRace,
    /// エージェントへのメッセージ送信フォームを開く
    SendAgentMessage,
    /// アクティブな Markdown ファイルのレンダリングプレビュー切替
    ToggleMdPreview,
    ToggleSidebar,
    // ── エディタの分割 (split editor / VS Code の editor group 相当) ──
    /// アクティブなエディタペインを左右に分割する (分割先は同じファイルを開く)
    SplitEditorRight,
    /// アクティブなエディタペインを上下に分割する
    SplitEditorDown,
    /// 分割を解除して 1 枚に戻す (他ペインのタブは吸収される)
    UnsplitEditor,
    /// 次のエディタペインへフォーカスを移す (巡回)
    FocusNextPane,
    /// n 番目 (1 始まり) のエディタペインへフォーカスを移す
    FocusEditorPane(usize),
    /// アクティブなタブを次のペインへ移す (1 枚のときは右へ分割して移す)
    MoveTabToNextPane,
    /// エディタ本文の折り返しを切り替える (config へ永続化)
    ToggleWordWrap,
    /// 空白文字 (スペース「·」/ タブ「→」) の可視化を切り替える (config へ永続化)
    ToggleShowWhitespace,
    /// エディタ右端のミニマップ (遠景ビュー) を切り替える (config へ永続化)
    ToggleMinimap,
    /// エディタ上部のブレッドクラム (パンくず) を切り替える (config へ永続化)
    ToggleBreadcrumbs,
    /// ガターの git blame (著者 · 相対日時) 表示を切り替える (config へ永続化)
    ToggleGitBlame,
    /// サイドバーを Git タブで開く
    OpenGitPanel,
    OpenFind,
    NewAgent(usize),
    /// プリセット `usize` を **専用の git worktree** で起動する。
    /// 同じツリーを他のエージェントと共有しないので、ファイルの取り合いが起きない。
    NewAgentIsolated(usize),
    /// 稼働中のエージェントを**全部**止める (破壊的なので必ず確認を取る)。
    StopAllAgents,
    /// カタログ全 CLI から選んでプリセットを追加するピッカーを開く
    OpenAgentPicker,
    FocusAgent(usize),
    RestartAgent,
    KillAgent,
    SetTheme(String),
    OpenConfig,
    ReloadConfig,
    /// 画面全体を 1 段拡大する (VS Code の「ズームイン」= ⌘+)。
    /// UI の全部 — サイドバー・タブ・メニュー・端末・エディタ — が一緒に大きくなる。
    ZoomIn,
    /// 画面全体を 1 段縮小する (⌘-)。
    ZoomOut,
    /// 画面全体のズームを 100% へ戻す (⌘0)。
    ZoomReset,
    /// アクティブなタブ **だけ** を 1 段拡大する (⌘⌥+)。
    /// 画面全体のズームの上に掛かるので、UI は等倍のままこのファイルだけ拡大できる。
    FileZoomIn,
    /// アクティブなタブだけを 1 段縮小する (⌘⌥-)。
    FileZoomOut,
    /// アクティブなタブのズームを解除して画面全体の倍率に戻す (⌘⌥0)。
    FileZoomReset,
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
    /// 同じ画面を「SSH リモート接続」の用事で開く (トグルではなく必ず開く)。
    /// 外出先のスマホから繋ぐときの入口。
    OpenSshRemote,
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
    /// 次 / 前の診断へ移動 (VS Code: F8 / ⇧F8)
    NextProblem,
    PrevProblem,
    /// 行末の診断メッセージ (Error Lens 相当) の表示切替
    ToggleInlineDiagnostics,
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
    /// `.vscode/tasks.json` の n 番目のタスクを実行 (index は走査キャッシュ順)
    RunJsonTask(usize),
    /// 選択テキストをアクティブなターミナルの入力欄へ送る (Enter は送らない)
    RunSelection,
    /// 新しいターミナル (Shell プリセット) を開く (VS Code: ⌃⇧`)
    NewTerminal,
    /// キーボードショートカット一覧ダイアログ
    ShowShortcuts,
    /// バージョン情報ダイアログ
    ShowAbout,
    /// ライセンスキーの入力・状態表示ダイアログ (オフライン検証・通信ゼロ)
    OpenLicense,
    // ── 横断検索のオプション (サイドバーの検索タブと同じ状態を切り替える) ──
    /// 大文字小文字を区別する
    ToggleSearchCase,
    /// 単語単位で検索する
    ToggleSearchWholeWord,
    /// 正規表現として検索する
    ToggleSearchRegex,
    /// 検索タブを置換行を開いた状態で表示する (VS Code: ⇧⌘H)
    GlobalReplace,
    /// サイドバーの「セッション」タブ (フォルダごとの過去の会話) を開く
    ShowSessions,
    /// プラン使用量・枯渇予測のウィンドウを開く
    ShowQuota,
    /// レート制限時のアカウント自動フェイルオーバーを有効化 / 無効化する (切替)。
    /// 状態と履歴は 📊 プラン使用量ウィンドウに出る。
    ToggleFailover,
    /// 保存時に行末の空白を落とす (切替)
    ToggleTrimTrailingOnSave,
    /// 保存時に最終行へ改行を入れる (切替)
    ToggleFinalNewlineOnSave,
    /// アクティブなファイルの改行コードを揃える
    ConvertLineEnding(crate::textenc::LineEnding),

    // ── PR 風のローカル変更レビュー (git_panel::ReviewPanel) ───────
    /// サイドバーの Git タブを「変更をレビュー」サブタブで開く
    OpenReview,
    /// レビューの比較ベースを変える。値は "head" | "staged" | "unstaged"
    /// (任意リビジョンはレビュー画面のツールバーから入力する)
    SetReviewBase(String),
    /// 差分の表示を 並列 (左右 2 列) ⇔ 一列 (インライン) で切り替える
    ToggleDiffView,
    /// 差分の次の変更へ (VS Code: F7)
    DiffNextChange,
    /// 差分の前の変更へ (VS Code: ⇧F7)
    DiffPrevChange,

    // ── 折りたたみ (highlight.rs の構造解析 + editor::FoldState) ────
    /// カーソル行の折りたたみを切り替える
    ToggleFold,
    /// すべて折りたたむ
    FoldAll,
    /// すべて展開する
    UnfoldAll,
    /// 深さ N (1 始まり) までを折りたたむ
    FoldLevel(usize),

    // ── ブックマーク / 閉じたタブ (editor::Bookmarks / ClosedTabs) ──
    /// カーソル行のブックマークを切り替える
    ToggleBookmark,
    /// 次のブックマークへ
    NextBookmark,
    /// 前のブックマークへ
    PrevBookmark,
    /// このファイルのブックマークをすべて解除
    ClearBookmarks,
    /// 直前に閉じたタブを開き直す
    ReopenClosedTab,

    // ── CSV/TSV テーブル表示 (editor::TableView) ────────────────────
    /// 表形式ファイルをグリッド表示 / 素のテキスト表示で切り替える
    ToggleTableView,

    // ── LSP (lsp.rs) ───────────────────────────────────────────────
    /// 補完候補を明示的に出す
    LspCompletion,
    /// 参照を検索してパネルに一覧する
    LspReferences,
    /// ドキュメントシンボルの一覧を開く
    LspSymbols,
    /// カーソル位置のシンボルをリネームする
    LspRename,
    /// ドキュメント全体を整形する (選択があればその範囲だけ)
    LspFormat,
    /// カーソル位置 / 選択範囲のクイックフィックス候補を出す
    LspCodeAction,
    /// 関数呼び出しの引数ヒント (シグネチャ) を出す
    LspSignatureHelp,
    /// カーソル下のシンボルを薄くハイライトするかを切り替える
    ToggleLspHighlight,
    /// 保存時に自動で整形するかを切り替える
    ToggleFormatOnSave,

    // ── 第 3 次配線: ガイドツアー / 承認キュー / 複数キャレット / 符号化 ──
    /// 初回起動ガイドツアーを最初から見直す
    RestartTutorial,
    /// 統合承認キューのパネルを開く
    OpenApprovals,
    /// 承認の監査ログ (approvals.jsonl の末尾) を開く
    OpenApprovalAudit,
    /// MCP サーバ管理パネルを開く
    OpenMcp,
    /// Skills / slash command 管理パネルを開く
    OpenSkills,
    /// キャレットを 1 つ上の行に増やす
    AddCursorAbove,
    /// キャレットを 1 つ下の行に増やす
    AddCursorBelow,
    /// 選択語の出現を全部選ぶ
    SelectAllOccurrences,
    /// 選択語の次の出現を 1 つ足す (VS Code の ⌘D)
    SelectNextOccurrence,
    /// 矩形 (列) 選択を開始する — いまのキャレットを角に据える
    ColumnSelectStart,
    /// 矩形 (列) 選択を確定する — いまのキャレットまでを長方形にする
    ColumnSelectFinish,
    /// 複数キャレットを解除して 1 本に戻す
    ClearMultiCursor,
    /// 全キャレットへクリップボードを貼り付ける (1 回の取り消しで戻る)
    MultiPaste,
    /// 符号化を選んで開き直す (`None` ならピッカーを開く)
    ReopenWithEncoding(Option<String>),
    /// 符号化を選んで保存する (`None` ならピッカーを開く)
    SaveWithEncoding(Option<String>),
}

#[derive(Clone)]
pub enum Action {
    OpenFile(PathBuf),
    Cmd(Cmd),
}

#[derive(Clone)]
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
    /// パレットから実行したコマンドの履歴 (先頭が直近)。ランキングの MRU と、
    /// 何も入力していないとき / 何も一致しないときの候補に使う。
    /// プロセス内だけ保つ (config へは書かない = 起動が遅くならない)。
    recent: Vec<Recent>,
}

impl Palette {
    pub fn new() -> Self {
        Self {
            open: false,
            input: String::new(),
            selected: 0,
            just_opened: false,
            recent: Vec::new(),
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

// ═══════════════════════════════════════════════════════════════════════
//  分類 / ランキング / 行モデル
//
//  パレットは以前「141 件を 1 本のフラットな一覧」で出していた。開いた瞬間が
//  ただの文字の壁になるので、ここで (1) 別の場所からワンアクションで届く
//  コマンドを外し、(2) 8 つの分類にまとめ、(3) 一致の質で並べ替える。
//
//  見出し (Row::Heading) と行末の淡いタグは**排他**で使う:
//    ・クエリ無し = 一覧を「読む」場面 → 見出しで区切る (browse)
//    ・クエリ有り = 一覧を「絞る」場面 → 見出しは順位と喧嘩するのでタグにする
// ═══════════════════════════════════════════════════════════════════════

/// パレットのコマンド分類。並びはメニューバー (menu_bar.rs) の並びに寄せて、
/// 「メニューのどこにあるか」の記憶がそのまま使えるようにしてある。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    Agent,
    File,
    Edit,
    Go,
    View,
    Git,
    Run,
    Tools,
}

/// 見出しを出す順。エージェントを先頭に置くのは、このエディタで最初に
/// 探されるのがエージェント操作だから (他はメニューバー準拠)。
const GROUP_ORDER: [Group; 8] = [
    Group::Agent,
    Group::File,
    Group::Edit,
    Group::Go,
    Group::View,
    Group::Git,
    Group::Run,
    Group::Tools,
];
impl Group {
    fn title(self) -> String {
        match self {
            Group::Agent => tr("エージェント"),
            Group::File => tr("ファイル"),
            Group::Edit => tr("編集"),
            Group::Go => tr("移動・検索"),
            Group::View => tr("表示"),
            Group::Git => tr("Git"),
            Group::Run => tr("ターミナル・実行"),
            Group::Tools => tr("設定・ヘルプ"),
        }
    }
}

/// コマンドの分類。ワイルドカードを置かないので、`Cmd` に候補を足すと
/// ここがコンパイルエラーになり「分類を決め忘れた」が必ず見つかる。
fn group_of(cmd: &Cmd) -> Group {
    match cmd {
        // ── ファイル ───────────────────────────────────────────────
        Cmd::Save
        | Cmd::SaveAs
        | Cmd::SaveAll
        | Cmd::SaveWithEncoding(_)
        | Cmd::ReopenWithEncoding(_)
        | Cmd::NewFile
        | Cmd::CloseTab
        | Cmd::CloseAllTabs
        | Cmd::OpenFolder
        | Cmd::NewWindow
        | Cmd::NewWindowFolder
        | Cmd::AddFolder
        | Cmd::AddFolderPath(_)
        | Cmd::RemoveFolder(_)
        | Cmd::OpenFileDialog
        | Cmd::OpenRecentFolder(_)
        | Cmd::OpenRecentFile(_)
        | Cmd::ClearRecent
        | Cmd::ToggleAutoSave
        | Cmd::RevertFile
        | Cmd::RefreshTree
        | Cmd::ToggleTrimTrailingOnSave
        | Cmd::ToggleFinalNewlineOnSave
        | Cmd::ToggleFormatOnSave
        | Cmd::ConvertLineEnding(_) => Group::File,

        // ── 編集 ───────────────────────────────────────────────────
        Cmd::Undo
        | Cmd::Redo
        | Cmd::CutSelection
        | Cmd::CopySelection
        | Cmd::PasteClipboard
        | Cmd::SelectAll
        | Cmd::ToggleLineComment
        | Cmd::DuplicateLine
        | Cmd::MoveLineUp
        | Cmd::MoveLineDown
        | Cmd::AddCursorAbove
        | Cmd::AddCursorBelow
        | Cmd::SelectAllOccurrences
        | Cmd::SelectNextOccurrence
        | Cmd::ColumnSelectStart
        | Cmd::ColumnSelectFinish
        | Cmd::ClearMultiCursor
        | Cmd::MultiPaste
        | Cmd::ToggleFold
        | Cmd::FoldAll
        | Cmd::UnfoldAll
        | Cmd::FoldLevel(_)
        | Cmd::LspCompletion
        | Cmd::LspRename
        | Cmd::LspFormat
        | Cmd::LspCodeAction
        | Cmd::LspSignatureHelp => Group::Edit,

        // ── 移動・検索 ─────────────────────────────────────────────
        Cmd::OpenFind
        | Cmd::OpenReplace
        | Cmd::GlobalSearch
        | Cmd::GlobalReplace
        | Cmd::ToggleSearchCase
        | Cmd::ToggleSearchWholeWord
        | Cmd::ToggleSearchRegex
        | Cmd::GoToDefinition
        | Cmd::GoToBracket
        | Cmd::NextProblem
        | Cmd::PrevProblem
        | Cmd::GoToLine
        | Cmd::NavBack
        | Cmd::NavForward
        | Cmd::NextTab
        | Cmd::PrevTab
        | Cmd::ReopenClosedTab
        | Cmd::ToggleBookmark
        | Cmd::NextBookmark
        | Cmd::PrevBookmark
        | Cmd::ClearBookmarks
        | Cmd::LspReferences
        | Cmd::LspSymbols
        | Cmd::OpenCommandPalette
        | Cmd::OpenFilePalette => Group::Go,

        // ── 表示 ───────────────────────────────────────────────────
        Cmd::ToggleSidebar
        | Cmd::SplitEditorRight
        | Cmd::SplitEditorDown
        | Cmd::UnsplitEditor
        | Cmd::FocusNextPane
        | Cmd::FocusEditorPane(_)
        | Cmd::MoveTabToNextPane
        | Cmd::ToggleMdPreview
        | Cmd::ToggleWordWrap
        | Cmd::ToggleShowWhitespace
        | Cmd::ToggleMinimap
        | Cmd::ToggleBreadcrumbs
        | Cmd::ToggleProblems
        | Cmd::ToggleInlineDiagnostics
        | Cmd::ToggleFullScreen
        | Cmd::ToggleTableView
        | Cmd::ToggleLspHighlight
        | Cmd::ZoomIn
        | Cmd::ZoomOut
        | Cmd::ZoomReset
        | Cmd::FileZoomIn
        | Cmd::FileZoomOut
        | Cmd::FileZoomReset
        | Cmd::SetTheme(_)
        | Cmd::ShowExplorer
        | Cmd::TogglePet
        | Cmd::SetPetImage
        | Cmd::ResetPetImage
        | Cmd::ResetPetPos
        | Cmd::SetPetVariant(_)
        | Cmd::SetPetScale(_)
        | Cmd::TogglePetFreeRoam
        | Cmd::TogglePetSleep
        | Cmd::TogglePetSounds
        | Cmd::TogglePetBubbles => Group::View,

        // ── Git ────────────────────────────────────────────────────
        Cmd::OpenGitPanel
        | Cmd::ShowGitHubTab
        | Cmd::OpenReview
        | Cmd::SetReviewBase(_)
        | Cmd::ToggleDiffView
        | Cmd::DiffNextChange
        | Cmd::DiffPrevChange
        | Cmd::ToggleGitBlame => Group::Git,

        // ── ターミナル・実行 ───────────────────────────────────────
        Cmd::ToggleTerminal
        | Cmd::NewTerminal
        | Cmd::RunActiveFile
        | Cmd::RunBuildTask
        | Cmd::RunJsonTask(_)
        | Cmd::RunSelection => Group::Run,

        // ── エージェント ───────────────────────────────────────────
        Cmd::NewAgent(_)
        | Cmd::NewAgentIsolated(_)
        | Cmd::StopAllAgents
        | Cmd::OpenAgentPicker
        | Cmd::FocusAgent(_)
        | Cmd::RestartAgent
        | Cmd::KillAgent
        | Cmd::SendFileToAgent
        | Cmd::SendAgentMessage
        | Cmd::NewTask
        | Cmd::OpenRace
        | Cmd::ToggleCockpit
        | Cmd::ToggleKanban
        | Cmd::ToggleDeck
        | Cmd::SetApproval(_)
        | Cmd::CyclePermissionAll
        | Cmd::TogglePetAutoYes
        | Cmd::OpenApprovals
        | Cmd::OpenApprovalAudit
        | Cmd::OpenMcp
        | Cmd::OpenSkills
        | Cmd::ShowSessions
        | Cmd::ShowQuota
        | Cmd::ToggleFailover
        | Cmd::VoiceInput(_)
        | Cmd::VoiceStop
        | Cmd::SetVoiceTarget(_) => Group::Agent,

        // ── 設定・ヘルプ ───────────────────────────────────────────
        Cmd::OpenConfig
        | Cmd::ReloadConfig
        | Cmd::ToggleRemote
        | Cmd::OpenSshRemote
        | Cmd::SetVoiceEngine(_)
        | Cmd::SetVoiceLang(_)
        | Cmd::SetVoiceKeyword(_)
        | Cmd::NewPlugin
        | Cmd::InstallPlugin
        | Cmd::RescanPlugins
        | Cmd::ShowPlugins
        | Cmd::RunPlugin(_, _)
        | Cmd::OpenInIde(_)
        | Cmd::OpenFolderInIde(_)
        | Cmd::ShowShortcuts
        | Cmd::ShowAbout
        | Cmd::OpenLicense
        | Cmd::RestartTutorial => Group::Tools,
    }
}

/// パレットに出さないコマンド。**別の場所からワンアクションで届く**もの
/// だけをここに入れる (機能は消していない。パレットの行数だけ減らす)。
///
/// | コマンド | パレットから外す理由 (実際の到達経路) |
/// |---|---|
/// | `SetReviewBase` ×3 | レビュー画面のツールバーにコンボボックスが常時出ている (git_panel.rs) |
/// | `ConvertLineEnding` ×3 | ステータスバーの改行コード表示を押すとメニューが出る (app.rs) |
/// | `SetPetImage` / `ResetPetImage` / `ResetPetPos` | ペットの右クリックメニューに同じ項目がある (app.rs)。対象が見えていないと選べない操作でもある |
/// | `NewPlugin` / `InstallPlugin` / `RescanPlugins` | プラグインタブのボタン (app.rs)。タブを開く `ShowPlugins` はパレットに残す |
/// | `ShowAbout` | ヘルプメニュー (menu_bar.rs)。押す頻度がゼロに近い純粋な情報表示 |
///
/// `ShowShortcuts` はヘルプメニューにもあるが、「キーバインドを思い出す」のは
/// パレットで最も起きる用事なので**残す**。
fn hidden_from_palette(cmd: &Cmd) -> bool {
    matches!(
        cmd,
        Cmd::SetReviewBase(_)
            | Cmd::ConvertLineEnding(_)
            | Cmd::SetPetImage
            | Cmd::ResetPetImage
            | Cmd::ResetPetPos
            | Cmd::NewPlugin
            | Cmd::InstallPlugin
            | Cmd::RescanPlugins
            | Cmd::ShowAbout
    )
}

// ── ランキング ────────────────────────────────────────────────────
//
// 一致の質を「段」で表し、段の間隔 (10_000) を MRU の最大加点 (7_000) より
// 大きく取る。こうすると MRU は同じ段の中だけを入れ替え、よく使うからと
// いって前方一致を追い越すことはない。fuzzy::score の素点 (数十〜数百) は
// 最後のタイブレークとしてだけ効く。
const TIER_PREFIX: i32 = 100_000;
const TIER_WORD: i32 = 60_000;
const TIER_SUBSTR: i32 = 30_000;
const TIER_GROUP: i32 = 10_000;
const MRU_RECENCY: i32 = 4_000;
const MRU_RECENCY_STEP: i32 = 400;
const MRU_PER_USE: i32 = 300;
const MRU_USE_CAP: u32 = 10;
/// MRU に覚えておく件数と、見出し「最近使ったコマンド」に出す件数。
const RECENT_MAX: usize = 24;
const RECENT_SHOWN: usize = 6;

/// 語の切れ目。ラテン語の区切りに加え、日本語ラベルで実際に使われている
/// 区切り (全角スペース・中黒・読点・各種括弧) も見る。
fn is_word_break(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '/' | '\\'
                | '_'
                | '-'
                | '.'
                | ':'
                | ','
                | '('
                | ')'
                | '['
                | ']'
                | '（'
                | '）'
                | '「'
                | '」'
                | '・'
                | '、'
                | '。'
                | '＝'
                | '>'
        )
}

/// 一致の質を段で返す。`label` / `group_title` / `q` はすべて小文字化済み。
fn match_tier(label: &str, group_title: Option<&str>, q: &str) -> i32 {
    if q.is_empty() {
        return 0;
    }
    if label.starts_with(q) {
        return TIER_PREFIX;
    }
    // 語頭一致 — 「保存」で「すべて保存」を拾う
    let mut prev_break = false;
    for (i, c) in label.char_indices() {
        if prev_break && label[i..].starts_with(q) {
            return TIER_WORD;
        }
        prev_break = is_word_break(c);
    }
    if label.contains(q) {
        return TIER_SUBSTR;
    }
    // 分類名での一致は「その分類を見たい」の意思表示なので拾うが、いちばん下
    if group_title.is_some_and(|g| g.contains(q)) {
        return TIER_GROUP;
    }
    0
}

/// パレットで実行したコマンドの記録。ラベルをキーにする — ラベルは `tr()`
/// 済みなので UI 言語を切り替えると別物になるが、失うのは「切り替えた直後の
/// 並び順」だけで、機能には影響しない。
#[derive(Clone)]
struct Recent {
    icon: String,
    label: String,
    action: Action,
    uses: u32,
}

/// パレットに描く 1 行。見出しは**選択できない** — 上下キーは必ず飛び越す。
pub enum Row {
    Heading(String),
    Item(usize),
}

/// 絞り込み・ランキング・グループ分けを済ませた表示状態。
pub struct Results {
    /// 実行対象。`Row::Item(i)` の `i` はこの Vec の添字。
    pub items: Vec<Item>,
    /// 実際に描く行 (見出し込み)。`items` の順序とは独立。
    pub rows: Vec<Row>,
    /// 行末に分類の淡いタグを出すか (クエリありのとき true)。
    tags: bool,
    /// 一覧の上に出す案内。**空にはしない** — 0 件でも必ず何か言う。
    notes: Vec<String>,
}
impl Results {
    /// いまの選択位置を「選択できる行」に丸める。見出しに乗っていたら
    /// そこから後ろ向きに (末尾まで行ったら先頭へ) 最初の項目を探す。
    pub fn clamp(&self, selected: usize) -> usize {
        let n = self.rows.len();
        if n == 0 {
            return 0;
        }
        let s = selected.min(n - 1);
        for k in 0..n {
            let i = (s + k) % n;
            if matches!(self.rows[i], Row::Item(_)) {
                return i;
            }
        }
        s
    }

    /// ↑ / ↓ の移動。見出しを飛ばし、端で巻き戻る。
    pub fn step(&self, selected: usize, down: bool, up: bool) -> usize {
        let sel = self.clamp(selected);
        // 同時押し (どちらも true) は打ち消し合う = 動かさない
        if down == up || self.rows.is_empty() {
            return sel;
        }
        let n = self.rows.len() as isize;
        let delta: isize = if down { 1 } else { -1 };
        let mut i = sel as isize;
        for _ in 0..n {
            i = (i + delta).rem_euclid(n);
            if matches!(self.rows[i as usize], Row::Item(_)) {
                return i as usize;
            }
        }
        sel
    }

    /// Enter で実行される項目。
    pub fn selected_item(&self, selected: usize) -> Option<&Item> {
        match self.rows.get(self.clamp(selected))? {
            Row::Item(i) => self.items.get(*i),
            Row::Heading(_) => None,
        }
    }
}
impl Palette {
    /// 実行したコマンドを覚える (MRU)。ファイルを開いた履歴は recent.rs が
    /// 持っているので、ここではコマンドだけ数える。
    pub fn note_used(&mut self, item: &Item) {
        if !matches!(item.action, Action::Cmd(_)) {
            return;
        }
        if let Some(i) = self.recent.iter().position(|r| r.label == item.label) {
            let mut r = self.recent.remove(i);
            r.uses = r.uses.saturating_add(1);
            self.recent.insert(0, r);
        } else {
            self.recent.insert(
                0,
                Recent {
                    icon: item.icon.clone(),
                    label: item.label.clone(),
                    action: item.action.clone(),
                    uses: 1,
                },
            );
            self.recent.truncate(RECENT_MAX);
        }
    }

    fn mru_bonus(&self, label: &str) -> i32 {
        match self.recent.iter().position(|r| r.label == label) {
            Some(i) => {
                let recency = (MRU_RECENCY - i as i32 * MRU_RECENCY_STEP).max(0);
                let uses = self.recent[i].uses.min(MRU_USE_CAP) as i32 * MRU_PER_USE;
                recency + uses
            }
            None => 0,
        }
    }

    /// 何も一致しなかったときに「代わりに」出す候補。最近使ったものが
    /// あればそれ、無ければ定番 4 つ。**必ず 1 件以上返す**。
    fn fallback_items(&self) -> Vec<Item> {
        if !self.recent.is_empty() {
            return self
                .recent
                .iter()
                .take(RECENT_SHOWN)
                .map(|r| Item {
                    icon: r.icon.clone(),
                    label: r.label.clone(),
                    detail: String::new(),
                    action: r.action.clone(),
                    score: 0,
                })
                .collect();
        }
        // ラベルは app.rs の組み込み一覧と同じ文字列にしてある (辞書が効き、
        // MRU のキーも一致する)。アイコンは UI_SYMBOLS にある字だけ。
        [
            ("💾", tr("保存"), Cmd::Save),
            ("🔍", tr("ファイル内検索"), Cmd::OpenFind),
            ("🔎", tr("ファイル間で検索"), Cmd::GlobalSearch),
            ("🖥", tr("ターミナル表示切替"), Cmd::ToggleTerminal),
        ]
        .into_iter()
        .map(|(icon, label, cmd)| Item {
            icon: icon.to_string(),
            label,
            detail: String::new(),
            action: Action::Cmd(cmd),
            score: 0,
        })
        .collect()
    }

    /// スコア付きの候補を、絞り込み → ランキング → 行モデルへ変換する。
    ///
    /// `items` は app.rs が fuzzy 素点付きで積んだもの。ここでの並べ替えが
    /// パレットの見え方すべてを決める。
    pub fn results(&self, mut items: Vec<Item>) -> Results {
        let cmd_mode = self.is_command_mode();
        let q = self.query().trim().to_lowercase();

        // 1) 別の場所からワンアクションで届くコマンドを落とす
        if cmd_mode {
            items.retain(|it| match &it.action {
                Action::Cmd(c) => !hidden_from_palette(c),
                Action::OpenFile(_) => true,
            });
        }

        // 2) ランキング: 一致の質 (段) + MRU を fuzzy 素点に足す
        for it in items.iter_mut() {
            let group = match &it.action {
                Action::Cmd(c) => Some(group_of(c)),
                Action::OpenFile(_) => None,
            };
            let gt = group.map(|g| g.title().to_lowercase());
            let tier = match_tier(&it.label.to_lowercase(), gt.as_deref(), &q);
            it.score = it
                .score
                .saturating_add(tier)
                .saturating_add(self.mru_bonus(&it.label));
        }
        items.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.label.cmp(&b.label)));
        // コマンドモードの母集団は数百件で頭打ちなので、見出しごと切り落として
        // 「設定・ヘルプが常に見えない」が起きないよう上限を分ける。
        items.truncate(if cmd_mode { 400 } else { 100 });

        let mut notes: Vec<String> = Vec::new();
        // 見出しを出すのは「読む」場面 = コマンドモードでクエリが空のときだけ
        let browse = cmd_mode && q.is_empty();
        // 見出しと行末タグは排他 — 両方出すと同じ情報が 2 回出る
        let mut tags = cmd_mode && !browse;
        let mut rows: Vec<Row> = Vec::new();

        if items.is_empty() {
            // 3a) 該当なし — 空白にせず、理由と代わりの候補を必ず出す
            notes.push(if q.is_empty() {
                tr("候補がありません")
            } else {
                trf("{q} に一致するものはありません", &[("q", q.clone())])
            });
            if cmd_mode {
                items = self.fallback_items();
                rows.push(Row::Heading(if self.recent.is_empty() {
                    tr("代わりに: よく使う操作")
                } else {
                    tr("代わりに: 最近使ったコマンド")
                }));
                rows.extend((0..items.len()).map(Row::Item));
                tags = false; // 見出しを出したのでタグは出さない
            } else {
                notes.push(tr(
                    "> でコマンド、@ でエージェント、# で worktree を探せます",
                ));
            }
        } else if browse {
            // 3b) 素の一覧 — 最近使ったものを先頭に、あとは分類ごとに見出し
            let mut pinned: Vec<usize> = Vec::new();
            for r in self.recent.iter().take(RECENT_SHOWN) {
                if let Some(i) = items.iter().position(|it| it.label == r.label) {
                    pinned.push(i);
                }
            }
            if !pinned.is_empty() {
                rows.push(Row::Heading(tr("最近使ったコマンド")));
                rows.extend(pinned.iter().copied().map(Row::Item));
            }
            for g in GROUP_ORDER {
                let mut idx: Vec<usize> = items
                    .iter()
                    .enumerate()
                    .filter(|(i, it)| !pinned.contains(i) && group_of_item(it) == Some(g))
                    .map(|(i, _)| i)
                    .collect();
                if idx.is_empty() {
                    continue;
                }
                idx.sort_by(|a, b| items[*a].label.cmp(&items[*b].label));
                rows.push(Row::Heading(g.title()));
                rows.extend(idx.into_iter().map(Row::Item));
            }
            let rest: Vec<usize> = items
                .iter()
                .enumerate()
                .filter(|(i, it)| !pinned.contains(i) && group_of_item(it).is_none())
                .map(|(i, _)| i)
                .collect();
            if !rest.is_empty() {
                rows.push(Row::Heading(tr("その他")));
                rows.extend(rest.into_iter().map(Row::Item));
            }
        } else {
            // 3c) 絞り込み中 — 見出しは順位と喧嘩するので、分類は行末のタグへ
            rows.extend((0..items.len()).map(Row::Item));
        }

        Results {
            items,
            rows,
            tags,
            notes,
        }
    }
}
fn group_of_item(it: &Item) -> Option<Group> {
    match &it.action {
        Action::Cmd(c) => Some(group_of(c)),
        Action::OpenFile(_) => None,
    }
}

/// パレットの候補一覧を描く。クリックされた項目を返す。
///
/// 呼び出し側は `egui::ScrollArea` の中でこれを呼ぶだけでよい。
/// アニメーションも毎フレームのタイマーも持たない (アイドルは 0 コスト)。
pub fn list_ui(
    ui: &mut egui::Ui,
    theme: &crate::theme::Theme,
    res: &Results,
    selected: usize,
    scroll_to_selected: bool,
) -> Option<Item> {
    let mut clicked: Option<Item> = None;
    let sel = res.clamp(selected);

    for note in &res.notes {
        ui.label(egui::RichText::new(note).size(12.5).color(theme.text_dim));
        ui.add_space(4.0);
    }

    for (ri, row) in res.rows.iter().enumerate() {
        match row {
            Row::Heading(title) => {
                if ri > 0 {
                    ui.add_space(8.0);
                }
                ui.label(
                    egui::RichText::new(title)
                        .size(11.0)
                        .color(theme.text_dim)
                        .strong(),
                );
                ui.add_space(2.0);
            }
            Row::Item(i) => {
                let Some(it) = res.items.get(*i) else {
                    continue;
                };
                let is_sel = ri == sel;
                let fill = if is_sel {
                    theme.accent_soft
                } else {
                    egui::Color32::TRANSPARENT
                };
                let fr = egui::Frame::none()
                    .fill(fill)
                    .rounding(egui::Rounding::same(6.0))
                    .inner_margin(egui::Margin::symmetric(8.0, 5.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        // 右端の分類タグを**先に**置いて幅を予約し、残りの幅で
                        // ラベルを省略する。逆順にするとタグの分だけ行がはみ出す。
                        let tag = if res.tags { group_of_item(it) } else { None };
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if let Some(g) = tag {
                                ui.label(
                                    egui::RichText::new(g.title())
                                        .size(11.0)
                                        .color(theme.text_dim.gamma_multiply(0.75)),
                                );
                            }
                            ui.with_layout(
                                egui::Layout::left_to_right(egui::Align::Center),
                                |ui| {
                                    ui.label(&it.icon);
                                    // 長いラベル・詳細は省略する (どの幅でも
                                    // 行からはみ出さない。全文はホバーで出る)
                                    ui.add(
                                        egui::Label::new(
                                            egui::RichText::new(&it.label).color(theme.text),
                                        )
                                        .truncate(),
                                    );
                                    if !it.detail.is_empty() {
                                        ui.add(
                                            egui::Label::new(
                                                egui::RichText::new(&it.detail)
                                                    .size(11.5)
                                                    .color(theme.text_dim),
                                            )
                                            .truncate(),
                                        );
                                    }
                                },
                            );
                        });
                    });
                let r = ui.interact(
                    fr.response.rect,
                    egui::Id::new(("pal-item", ri)),
                    egui::Sense::click(),
                );
                if r.clicked() {
                    clicked = Some(it.clone());
                }
                if is_sel && scroll_to_selected {
                    r.scroll_to_me(None);
                }
            }
        }
    }
    clicked
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

#[cfg(test)]
mod group_tests {
    use super::*;

    fn cmd(label: &str, c: Cmd) -> Item {
        Item {
            icon: "●".into(),
            label: label.into(),
            detail: String::new(),
            action: Action::Cmd(c),
            score: 0,
        }
    }

    fn cmd_palette(query: &str) -> Palette {
        let mut p = Palette::new();
        p.open_commands();
        p.input = format!(">{query}");
        p
    }

    /// 表示順に並べたラベル (見出しは `# ` を付ける)。
    fn rendered(res: &Results) -> Vec<String> {
        res.rows
            .iter()
            .map(|r| match r {
                Row::Heading(t) => format!("# {t}"),
                Row::Item(i) => res.items[*i].label.clone(),
            })
            .collect()
    }

    // ── 分類 ──────────────────────────────────────────────────────

    /// `Cmd` に候補を足したら `group_of` にも足す。ここが唯一の検出器。
    /// ソースを読むテストなので改行は正規化する (Windows は CRLF)。
    #[test]
    fn every_command_variant_is_classified() {
        let src = include_str!("palette.rs").replace("\r\n", "\n");
        let (_, rest) = src.split_once("pub enum Cmd {").expect("enum Cmd");
        let (body, _) = rest.split_once("\n}\n").expect("end of enum Cmd");
        let (_, gsrc) = src.split_once("fn group_of(cmd: &Cmd)").expect("group_of");
        let (garm, _) = gsrc
            .split_once("\n/// パレットに出さない")
            .expect("end of group_of");

        let mut variants: Vec<&str> = Vec::new();
        for line in body.lines() {
            let t = line.trim();
            if t.is_empty() || t.starts_with("//") || t.starts_with("///") {
                continue;
            }
            let name: String = t
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() && name.chars().next().is_some_and(|c| c.is_uppercase()) {
                variants.push(&t[..name.len()]);
            }
        }
        assert!(variants.len() > 100, "変換に失敗している: {variants:?}");

        let missing: Vec<&&str> = variants
            .iter()
            .filter(|v| !garm.contains(&format!("Cmd::{v}")))
            .collect();
        assert!(
            missing.is_empty(),
            "group_of に分類が無い Cmd がある (パレットの見出しに出ない): {missing:?}"
        );
    }

    #[test]
    fn group_titles_are_distinct_and_non_empty() {
        let mut seen: Vec<String> = Vec::new();
        for g in GROUP_ORDER {
            let t = g.title();
            assert!(!t.trim().is_empty(), "{g:?} の見出しが空");
            assert!(!seen.contains(&t), "見出しが重複: {t}");
            seen.push(t);
        }
        assert_eq!(
            GROUP_ORDER.len(),
            8,
            "分類は 8 つまで (増やすとまた壁になる)"
        );
    }

    // ── 削除 ──────────────────────────────────────────────────────

    #[test]
    fn hidden_commands_are_dropped_in_command_mode() {
        let p = cmd_palette("");
        let res = p.results(vec![
            cmd("保存", Cmd::Save),
            cmd(
                "レビューの比較: ステージ済みだけ",
                Cmd::SetReviewBase("staged".into()),
            ),
            cmd("ペット画像を変更…", Cmd::SetPetImage),
            cmd("プラグインを再スキャン", Cmd::RescanPlugins),
            cmd("バージョン情報", Cmd::ShowAbout),
            cmd(
                "改行コードを変換: LF (Unix)",
                Cmd::ConvertLineEnding(crate::textenc::LineEnding::Lf),
            ),
        ]);
        let labels: Vec<&str> = res.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["保存"], "外したはずのコマンドが残っている");
    }

    /// 残すと決めたものを取り違えていないか (回帰防止)。
    #[test]
    fn kept_commands_are_not_hidden() {
        for c in [
            Cmd::ShowShortcuts,
            Cmd::ShowPlugins,
            Cmd::OpenReview,
            Cmd::TogglePet,
            Cmd::SetApproval("ask".into()),
            Cmd::RefreshTree,
            Cmd::ToggleSearchRegex,
        ] {
            assert!(!hidden_from_palette(&c), "残す判断のコマンドが消えている");
        }
    }

    /// 外したコマンドは本当に別の場所から届くか — 到達経路をソースで固定する。
    #[test]
    fn removed_commands_stay_reachable_elsewhere() {
        let menu = include_str!("menu_bar.rs").replace("\r\n", "\n");
        assert!(
            menu.contains("Cmd::ShowAbout"),
            "ShowAbout がヘルプメニューから消えた"
        );
        let git = include_str!("git_panel.rs").replace("\r\n", "\n");
        for b in [
            "ReviewBase::Head",
            "ReviewBase::Staged",
            "ReviewBase::Unstaged",
        ] {
            assert!(
                git.contains(b),
                "レビューのベース切替 {b} がツールバーから消えた"
            );
        }
    }

    // ── ランキング ────────────────────────────────────────────────

    #[test]
    fn ranking_prefix_beats_word_beats_substring_beats_group() {
        let p = cmd_palette("save");
        let res = p.results(vec![
            // 分類名だけ一致 (Group::File の英訳ではなく、ここでは日本語見出しに
            // 当たらないので素点のみ) — 明示的に「分類一致」を作る
            cmd("まったく別の操作", Cmd::ToggleTerminal),
            cmd("autosave toggle", Cmd::ToggleAutoSave), // 部分一致
            cmd("file save all", Cmd::SaveAll),          // 語頭一致
            cmd("save", Cmd::Save),                      // 前方一致
        ]);
        let labels: Vec<&str> = res.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels[0], "save", "前方一致が先頭に来ていない");
        assert_eq!(labels[1], "file save all", "語頭一致が 2 番目に来ていない");
        assert_eq!(
            labels[2], "autosave toggle",
            "部分一致が 3 番目に来ていない"
        );
    }

    #[test]
    fn group_name_match_ranks_last_but_is_kept() {
        // 「git」は Group::Git の見出しと一致する = 拾うが最下段
        let p = cmd_palette("git");
        let res = p.results(vec![
            cmd("git パネルを開く", Cmd::OpenGitPanel), // 前方一致
            cmd("変更をレビュー", Cmd::OpenReview),     // 分類名だけ一致
        ]);
        let labels: Vec<&str> = res.items.iter().map(|i| i.label.as_str()).collect();
        assert_eq!(labels, vec!["git パネルを開く", "変更をレビュー"]);
    }

    #[test]
    fn recent_commands_float_up_within_the_same_tier() {
        let mut p = cmd_palette("");
        let a = cmd("ずっと後ろのはずの操作", Cmd::ToggleRemote);
        p.note_used(&a);
        let res = p.results(vec![
            cmd("あ", Cmd::Save),
            cmd("ずっと後ろのはずの操作", Cmd::ToggleRemote),
        ]);
        assert_eq!(
            res.items[0].label, "ずっと後ろのはずの操作",
            "MRU が効いていない"
        );
    }

    /// MRU は段を飛び越えない — よく使うからといって前方一致を追い越さない。
    #[test]
    fn mru_never_outranks_a_better_match() {
        let mut p = Palette::new();
        p.open_commands();
        let fav = cmd("まったく無関係だがよく使う", Cmd::ToggleRemote);
        for _ in 0..20 {
            p.note_used(&fav);
        }
        p.input = ">保存".into();
        let res = p.results(vec![
            cmd("保存", Cmd::Save),
            cmd("まったく無関係だがよく使う保存", Cmd::ToggleRemote),
        ]);
        assert_eq!(res.items[0].label, "保存", "MRU が前方一致を追い越した");
    }

    #[test]
    fn note_used_ignores_files_and_counts_repeats() {
        let mut p = Palette::new();
        p.note_used(&Item {
            icon: "📄".into(),
            label: "main.rs".into(),
            detail: String::new(),
            action: Action::OpenFile(std::path::PathBuf::from("main.rs")),
            score: 0,
        });
        assert!(p.recent.is_empty(), "ファイルは MRU に入れない");
        let s = cmd("保存", Cmd::Save);
        p.note_used(&s);
        p.note_used(&s);
        assert_eq!(p.recent.len(), 1);
        assert_eq!(p.recent[0].uses, 2);
    }

    #[test]
    fn recent_list_is_capped() {
        let mut p = Palette::new();
        for i in 0..(RECENT_MAX + 10) {
            p.note_used(&cmd(&format!("cmd {i}"), Cmd::Save));
        }
        assert!(p.recent.len() <= RECENT_MAX, "MRU が無制限に伸びている");
    }

    // ── 空状態 / 該当なし ─────────────────────────────────────────

    #[test]
    fn empty_query_groups_with_headings_and_is_never_blank() {
        let p = cmd_palette("");
        let res = p.results(vec![
            cmd("保存", Cmd::Save),
            cmd("ターミナル表示切替", Cmd::ToggleTerminal),
            cmd("エージェントへ移動", Cmd::FocusAgent(0)),
        ]);
        let out = rendered(&res);
        assert!(!out.is_empty(), "空状態が空白になっている");
        let headings: Vec<&String> = out.iter().filter(|s| s.starts_with("# ")).collect();
        assert_eq!(headings.len(), 3, "分類ごとの見出しが出ていない: {out:?}");
        assert!(out[0].starts_with("# "), "先頭は見出しのはず: {out:?}");
        assert!(!res.tags, "見出しがあるときは行末タグを出さない");
    }

    #[test]
    fn empty_query_pins_recent_commands_on_top() {
        let mut p = cmd_palette("");
        p.note_used(&cmd("ターミナル表示切替", Cmd::ToggleTerminal));
        let res = p.results(vec![
            cmd("保存", Cmd::Save),
            cmd("ターミナル表示切替", Cmd::ToggleTerminal),
        ]);
        let out = rendered(&res);
        assert_eq!(out[0], format!("# {}", tr("最近使ったコマンド")));
        assert_eq!(out[1], "ターミナル表示切替");
        // 「最近使った」に出したものを分類側で二重に出さない
        assert_eq!(
            out.iter().filter(|s| *s == "ターミナル表示切替").count(),
            1,
            "同じコマンドが 2 回出ている: {out:?}"
        );
    }

    #[test]
    fn no_match_says_so_and_offers_the_nearest_thing() {
        let p = cmd_palette("ないよこんなコマンド");
        let res = p.results(vec![]);
        assert!(!res.notes.is_empty(), "該当なしの説明が無い (空白になる)");
        assert!(res.notes[0].contains("ないよこんなコマンド"));
        assert!(!res.items.is_empty(), "代わりの候補が出ていない");
        assert!(!res.rows.is_empty(), "該当なしで一覧が空白になっている");
        // 代わりの候補も Enter で実行できること
        assert!(res.selected_item(0).is_some());
        // 見出しを出したので行末タグは出さない (同じ情報を 2 回出さない)
        assert!(!res.tags);
        assert!(matches!(res.rows[0], Row::Heading(_)));
        assert!(matches!(res.rows[res.clamp(0)], Row::Item(_)));
    }

    #[test]
    fn no_match_offers_recents_when_there_are_any() {
        let mut p = cmd_palette("zzzz");
        p.note_used(&cmd("ターミナル表示切替", Cmd::ToggleTerminal));
        let res = p.results(vec![]);
        assert_eq!(res.items[0].label, "ターミナル表示切替");
    }

    #[test]
    fn no_match_in_file_mode_teaches_the_prefixes() {
        let mut p = Palette::new();
        p.open_files();
        p.input = "zzzz".into();
        let res = p.results(vec![]);
        assert_eq!(
            res.notes.len(),
            2,
            "ファイルモードの該当なしが 1 行しかない"
        );
        assert!(res.notes[1].contains('>') && res.notes[1].contains('@'));
        // ファイルモードでコマンドを勝手に出さない
        assert!(res.items.is_empty());
    }

    // ── キーボード操作 ────────────────────────────────────────────

    #[test]
    fn arrows_skip_headings_and_wrap() {
        let p = cmd_palette("");
        let res = p.results(vec![
            cmd("保存", Cmd::Save),                         // File
            cmd("ターミナル表示切替", Cmd::ToggleTerminal), // Run
        ]);
        let out = rendered(&res);
        // # Agent なし / # ファイル, 保存, # ターミナル・実行, ターミナル表示切替
        assert_eq!(out.len(), 4, "{out:?}");

        // 初期選択 (0) は見出しなので最初の項目へ丸められる
        let first = res.clamp(0);
        assert!(matches!(res.rows[first], Row::Item(_)));
        assert_eq!(first, 1);

        // ↓ で次の「項目」へ (見出しを飛ばす)
        let n = res.step(first, true, false);
        assert_eq!(n, 3, "見出しを飛ばせていない");
        // ↓ でもう一度 = 先頭の項目へ巻き戻る
        assert_eq!(res.step(n, true, false), 1, "巻き戻りが見出しに止まった");
        // ↑ も同様
        assert_eq!(res.step(1, false, true), 3);
        assert_eq!(res.step(3, false, true), 1);
    }

    #[test]
    fn headings_are_never_selectable() {
        let p = cmd_palette("");
        let res = p.results(vec![
            cmd("保存", Cmd::Save),
            cmd("ターミナル表示切替", Cmd::ToggleTerminal),
            cmd("エージェントへ移動", Cmd::FocusAgent(0)),
        ]);
        // どこから何回動かしても見出しには乗らない
        let mut sel = 0usize;
        for k in 0..(res.rows.len() * 3) {
            sel = res.step(sel, k % 2 == 0, k % 2 == 1);
            assert!(
                matches!(res.rows[sel], Row::Item(_)),
                "見出しが選択された: {sel} {:?}",
                rendered(&res)
            );
            assert!(res.selected_item(sel).is_some());
        }
        // 範囲外の selected も安全に丸まる
        assert!(matches!(res.rows[res.clamp(9_999)], Row::Item(_)));
        assert!(res.selected_item(9_999).is_some());
    }

    #[test]
    fn step_is_a_no_op_without_input_or_on_both_keys() {
        let p = cmd_palette("");
        let res = p.results(vec![cmd("保存", Cmd::Save)]);
        let sel = res.clamp(0);
        assert_eq!(res.step(sel, false, false), sel);
        assert_eq!(res.step(sel, true, true), sel);
    }

    #[test]
    fn empty_results_never_panic() {
        let mut p = Palette::new();
        p.open_files();
        let res = p.results(vec![]);
        assert_eq!(res.clamp(0), 0);
        assert_eq!(res.step(0, true, false), 0);
        assert!(res.selected_item(0).is_none());
    }

    // ── 絞り込み中の見せ方 ────────────────────────────────────────

    #[test]
    fn filtering_uses_row_tags_instead_of_headings() {
        let p = cmd_palette("保存");
        let res = p.results(vec![
            cmd("保存", Cmd::Save),
            cmd("すべて保存", Cmd::SaveAll),
        ]);
        assert!(
            res.rows.iter().all(|r| matches!(r, Row::Item(_))),
            "絞り込み中に見出しが混ざっている (順位と喧嘩する)"
        );
        assert!(res.tags, "絞り込み中は行末に分類タグを出す");
    }

    #[test]
    fn file_mode_has_no_groups_and_no_tags() {
        let mut p = Palette::new();
        p.open_files();
        p.input = "main".into();
        let res = p.results(vec![Item {
            icon: "📄".into(),
            label: "main.rs".into(),
            detail: "src/main.rs".into(),
            action: Action::OpenFile(std::path::PathBuf::from("src/main.rs")),
            score: 5,
        }]);
        assert!(!res.tags);
        assert!(res.rows.iter().all(|r| matches!(r, Row::Item(_))));
        assert!(group_of_item(&res.items[0]).is_none());
    }

    // ── 語頭判定 ──────────────────────────────────────────────────

    #[test]
    fn word_start_matching_handles_japanese_separators() {
        assert_eq!(match_tier("save", None, "save"), TIER_PREFIX);
        assert_eq!(match_tier("file save", None, "save"), TIER_WORD);
        assert_eq!(
            match_tier("検索: 正規表現を使用する", None, "正規表現"),
            TIER_WORD
        );
        assert_eq!(match_tier("レビュー (pr 風)", None, "pr"), TIER_WORD);
        assert_eq!(match_tier("autosave", None, "save"), TIER_SUBSTR);
        assert_eq!(match_tier("まったく別", Some("git"), "git"), TIER_GROUP);
        assert_eq!(match_tier("まったく別", None, "git"), 0);
        // クエリが空ならどれも段は付かない (素点と MRU だけで並ぶ)
        assert_eq!(match_tier("save", Some("git"), ""), 0);
    }

    // ── 描画 ──────────────────────────────────────────────────────

    /// `list_ui` をヘッドレスで描く。落ちないことと、**どの幅でも行が
    /// 与えられた幅からはみ出さない**ことを見る (600px でも 320px でも)。
    #[test]
    fn list_ui_stays_inside_its_width_in_every_state() {
        let long = "とても長いコマンド名 ".repeat(12);
        let mut used = cmd_palette("");
        used.note_used(&cmd("保存", Cmd::Save));

        let states = [
            // 見出しあり (素の一覧)
            used.results(vec![
                cmd("保存", Cmd::Save),
                cmd(&long, Cmd::ToggleTerminal),
                cmd("エージェントへ移動", Cmd::FocusAgent(0)),
            ]),
            // 行末タグあり (絞り込み中)
            cmd_palette("保存").results(vec![cmd("保存", Cmd::Save), cmd(&long, Cmd::SaveAll)]),
            // 該当なし (代わりの候補 + 案内文)
            cmd_palette("zzzz").results(vec![]),
            // まったくの空 (ファイルモードの該当なし)
            {
                let mut p = Palette::new();
                p.open_files();
                p.input = "zzzz".into();
                p.results(vec![])
            },
        ];

        for th in crate::theme::all() {
            for (si, res) in states.iter().enumerate() {
                for width in [320.0_f32, 640.0, 1200.0] {
                    let ctx = egui::Context::default();
                    let mut inner = 0.0_f32;
                    let _ = ctx.run(Default::default(), |ctx| {
                        egui::CentralPanel::default().show(ctx, |ui| {
                            ui.set_width(width);
                            egui::ScrollArea::vertical().show(ui, |ui| {
                                ui.set_width(width);
                                let _ = list_ui(ui, &th, res, 0, false);
                                inner = ui.min_rect().width();
                            });
                        });
                    });
                    assert!(
                        inner <= width + 1.0,
                        "{} state={si} 幅 {width} に対し行が {inner} まではみ出した",
                        th.name
                    );
                }
            }
        }
    }

    /// 段の間隔は MRU の最大加点より大きいこと (追い越しの構造的な防止)。
    #[test]
    fn tier_gap_exceeds_max_mru_bonus() {
        let max_mru = MRU_RECENCY + MRU_USE_CAP as i32 * MRU_PER_USE;
        assert!(TIER_GROUP > max_mru, "MRU が段を飛び越えうる");
        for (hi, lo) in [
            (TIER_PREFIX, TIER_WORD),
            (TIER_WORD, TIER_SUBSTR),
            (TIER_SUBSTR, TIER_GROUP),
        ] {
            assert!(hi - lo > max_mru, "段の間隔が MRU より狭い: {hi} - {lo}");
        }
    }
}
