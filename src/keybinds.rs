//! カスタマイズ可能なキーバインドモジュール。
//!
//! デフォルトのショートカット一式を持ち、config.toml の `[keybindings]`
//! (action名 → "cmd+shift+p" 形式の文字列) で個別に上書きできる。
//! 不正な action 名・ショートカット文字列は黙って無視し、デフォルトを維持する。
#![allow(dead_code)]

use egui::{Key, KeyboardShortcut, Modifiers};
use std::collections::HashMap;

/// キーバインド可能なアクション。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum BindAction {
    Save,
    SaveAs,
    CloseTab,
    NewFile,
    /// 新しいウィンドウ (別プロセス) を開く (VS Code: ⇧⌘N)
    NewWindow,
    PaletteFiles,
    PaletteCommands,
    ToggleTerminal,
    ToggleSidebar,
    Find,
    ToggleCockpit,
    /// フリート看板 (エージェントのカンバン画面) 切替
    ToggleKanban,
    /// エージェントデッキ (縦 1 本のエージェント管理画面) 切替
    ToggleDeck,
    ToggleMdPreview,
    NewAgent,
    /// 画面全体のズーム (VS Code: ⌘+ / ⌘- / ⌘0)。UI 全部が拡大縮小する。
    ZoomIn,
    ZoomOut,
    ZoomReset,
    /// アクティブなタブだけのズーム (⌘⌥+ / ⌘⌥- / ⌘⌥0)。
    FileZoomIn,
    FileZoomOut,
    FileZoomReset,
    /// 取り消し (VS Code: ⌘Z / Ctrl+Z)。
    ///
    /// **`TextEdit` より先にここで消費する。** egui 0.29 の `TextEdit` は
    /// 自前の undoer を持っていて外す API が無いため、打鍵を先に取らないと
    /// 「egui の粒度」と「バッファの履歴」が二重に動いてしまう。
    Undo,
    /// やり直し (VS Code: ⇧⌘Z / Ctrl+Y)。
    Redo,
    ToggleComment,
    DuplicateLine,
    MoveLineUp,
    MoveLineDown,
    /// エクスプローラー(ファイルツリー)へフォーカス (VS Code: ⌘⇧E / Ctrl+Shift+E)
    FocusExplorer,
    /// ファイルを開くダイアログ (VS Code: ⌘O)
    OpenFile,
    /// すべて保存 (VS Code: ⌥⌘S)
    SaveAll,
    /// 行/列へ移動 (VS Code: ⌃G)
    GoToLine,
    /// 次/前のエディタタブ (VS Code: ⇧⌘] / ⇧⌘[)
    NextTab,
    PrevTab,
    /// **最近使った順のタブ切替** (VS Code / Zed と同じ ⌃Tab / ⌃⇧Tab)。
    ///
    /// 押しっぱなしの間だけ候補一覧を出し、修飾キーを離したところで確定する
    /// (`config.toml` の `tab_switch_mru = false` なら位置巡回に戻る)。
    /// ⌃Tab は macOS の予約表 ([`MACOS_RESERVED`]) に無い — 予約されているのは
    /// ⌘Tab (アプリ切替) の方で、⌃Tab はアプリまで届く。
    SwitchTab,
    SwitchTabBack,
    /// ファイル横断検索 (VS Code: ⇧⌘F)
    GlobalSearch,
    /// 置換 (VS Code: ⌥⌘F)
    OpenReplace,
    /// 新しいターミナル (VS Code: ⌃⇧`)
    NewTerminal,
    /// ナビゲーション 戻る/進む (VS Code: ⌃- / ⌃⇧-)
    NavBack,
    NavForward,
    /// 定義へ移動 (VS Code: F12)
    GoToDefinition,
    /// 対応する括弧へ移動 (VS Code: ⇧⌘\)
    GoToBracket,
    /// 次の診断へ移動 (VS Code: F8)
    NextProblem,
    /// 前の診断へ移動 (VS Code: ⇧F8)
    PrevProblem,
    /// ビルドタスクの実行 (VS Code: ⇧⌘B)
    RunBuildTask,
    /// 問題パネル (VS Code: ⇧⌘M)
    ToggleProblems,
    /// フルスクリーン (VS Code: ⌃⌘F)
    ToggleFullScreen,
    /// ワークスペース全体の置換 (VS Code: ⇧⌘H)
    GlobalReplace,
    /// カーソル行の折りたたみ切替 (VS Code Mac の「折りたたみ」= ⌥⌘[)
    ///
    /// VS Code の ⌘K ⌘0 / ⌘K ⌘J のような **2 打鍵のコード (chord)** は
    /// この解決テーブルが 1 つの [`KeyboardShortcut`] しか持てないため
    /// 表現できない。単打で空いている ⌥⌘[ / ⌥⌘] を割り当て、
    /// 段数指定の折りたたみはコマンドパレット専用にしてある。
    ToggleFold,
    /// すべて展開 (VS Code Mac の「展開」= ⌥⌘])
    UnfoldAll,
    /// カーソル行のブックマーク切替
    ToggleBookmark,
    /// 直前に閉じたタブを開き直す (VS Code: ⇧⌘T)
    ReopenClosedTab,
    /// 補完候補を出す (VS Code mac の第 2 割り当てと同じ ⌘I)。
    ///
    /// VS Code 既定の ⌃Space は **macOS が予約している** ("前の入力ソースを
    /// 選択"; `com.apple.symbolichotkeys` の id 60 が既定で enabled)。
    /// 入力ソースを 2 つ以上持つ環境 (日本語 IME を使えば必ずそうなる) では
    /// アプリまでイベントが届かないので、既定は ⌘I にしてある。
    /// ⌃Space は「効く環境では効く」おまけの打鍵として残してある。
    LspCompletion,
    /// 参照を検索 (VS Code: ⇧F12)
    LspReferences,
    /// シンボルにジャンプ (VS Code: ⇧⌘O)
    LspSymbols,
    /// リネーム (VS Code: F2)
    LspRename,
    /// ドキュメントの整形 (VS Code: ⇧⌥F)。選択があれば選択範囲だけを整形する
    LspFormat,
    /// クイックフィックス / コードアクション (VS Code: ⌘. / Ctrl+.)
    LspCodeAction,
    /// 引数ヒント (シグネチャヘルプ) を出す (VS Code: ⇧⌘Space)
    LspSignatureHelp,
    /// 次の出現を選択してキャレットを増やす (VS Code: ⌘D)
    SelectNextOccurrence,
    /// エディタを右に分割 (VS Code: ⌘\ と同じ)
    SplitEditorRight,
    /// エディタを下に分割
    SplitEditorDown,
    /// 1 番目のエディタペインへフォーカス (VS Code: ⌘1)
    FocusPane1,
    /// 2 番目のエディタペインへフォーカス (VS Code: ⌘2)
    FocusPane2,
    /// 3 番目のエディタペインへフォーカス (VS Code: ⌘3)
    FocusPane3,
    /// 差分ビューで次の変更へ (VS Code: F7)
    DiffNextChange,
    /// 差分ビューで前の変更へ (VS Code: ⇧F7)
    DiffPrevChange,
    /// レビューで**次の「差分のあるファイル」へ**ジャンプ (cmux: `]f`)。
    /// 既定は 2 打鍵 ([`Binding::Chord`])。
    DiffNextFile,
    /// レビューで前の「差分のあるファイル」へジャンプ (cmux: `[f`)。
    DiffPrevFile,
    /// キーバインド編集 UI を開く (VS Code: ⌘K ⌘S)。
    /// 既定は 2 打鍵 ([`Binding::Chord`])。
    KeybindEditor,
    /// **アクティブなエージェントの追従**を開始 / 解除する (Zed の "follow")。
    /// エディタのビューポートが、そのエージェントが触っている行を追いかける。
    FollowAgent,
    /// 追従の**再開**。ユーザーが自分でスクロールすると追従は一時停止するので、
    /// 戻すのは必ずこの明示操作から (勝手に再開はしない)。
    FollowResume,
    /// **次の未読エージェントへ飛ぶ** (端で折り返す)。視線移動ゼロで
    /// 「今どれが自分待ちか」へ移るための動線。
    NextUnread,
    /// **いまの相手を未読に戻して次の未読へ** (後回し宣言)。
    DeferUnread,
    /// いまの相手の未読フラグを反転する。
    ToggleUnread,
}

/// 全アクションの一覧 (デフォルトマップ構築用)。
pub const ALL_ACTIONS: [BindAction; 75] = [
    BindAction::Save,
    BindAction::SaveAs,
    BindAction::CloseTab,
    BindAction::NewFile,
    BindAction::NewWindow,
    BindAction::PaletteFiles,
    BindAction::PaletteCommands,
    BindAction::ToggleTerminal,
    BindAction::ToggleSidebar,
    BindAction::Find,
    BindAction::ToggleCockpit,
    BindAction::ToggleKanban,
    BindAction::ToggleDeck,
    BindAction::ToggleMdPreview,
    BindAction::NewAgent,
    BindAction::ZoomIn,
    BindAction::ZoomOut,
    BindAction::ZoomReset,
    BindAction::FileZoomIn,
    BindAction::FileZoomOut,
    BindAction::FileZoomReset,
    BindAction::Undo,
    BindAction::Redo,
    BindAction::ToggleComment,
    BindAction::DuplicateLine,
    BindAction::MoveLineUp,
    BindAction::MoveLineDown,
    BindAction::FocusExplorer,
    BindAction::OpenFile,
    BindAction::SaveAll,
    BindAction::GoToLine,
    BindAction::NextTab,
    BindAction::PrevTab,
    BindAction::SwitchTab,
    BindAction::SwitchTabBack,
    BindAction::GlobalSearch,
    BindAction::OpenReplace,
    BindAction::NewTerminal,
    BindAction::NavBack,
    BindAction::NavForward,
    BindAction::GoToDefinition,
    BindAction::GoToBracket,
    BindAction::NextProblem,
    BindAction::PrevProblem,
    BindAction::RunBuildTask,
    BindAction::ToggleProblems,
    BindAction::ToggleFullScreen,
    BindAction::GlobalReplace,
    BindAction::ToggleFold,
    BindAction::UnfoldAll,
    BindAction::ToggleBookmark,
    BindAction::ReopenClosedTab,
    BindAction::LspCompletion,
    BindAction::LspReferences,
    BindAction::LspSymbols,
    BindAction::LspRename,
    BindAction::LspFormat,
    BindAction::LspCodeAction,
    BindAction::LspSignatureHelp,
    BindAction::SelectNextOccurrence,
    BindAction::SplitEditorRight,
    BindAction::SplitEditorDown,
    BindAction::FocusPane1,
    BindAction::FocusPane2,
    BindAction::FocusPane3,
    BindAction::DiffNextChange,
    BindAction::DiffPrevChange,
    BindAction::DiffNextFile,
    BindAction::DiffPrevFile,
    BindAction::KeybindEditor,
    BindAction::FollowAgent,
    BindAction::FollowResume,
    BindAction::NextUnread,
    BindAction::DeferUnread,
    BindAction::ToggleUnread,
];

/// ファイル単位ズームの修飾キー。macOS は ⌥⌘、他は Ctrl+Alt+Shift。
///
/// 他 OS で ⇧ まで足しているのは、Ctrl+Alt+- が「戻る」(VS Code 準拠) と
/// 重なるため。[`Modifiers::matches_logically`] は修飾キーの **上位集合** も
/// 一致とみなすので、重ねると消費順だけが頼りになり事故りやすい。
fn file_zoom_mods() -> Modifiers {
    if cfg!(target_os = "macos") {
        Modifiers::COMMAND.plus(Modifiers::ALT)
    } else {
        Modifiers::COMMAND
            .plus(Modifiers::ALT)
            .plus(Modifiers::SHIFT)
    }
}

/// 現行 app.rs::handle_shortcuts と同一のデフォルト。
fn default_shortcut(a: BindAction) -> KeyboardShortcut {
    let cmd = Modifiers::COMMAND;
    let cmd_shift = Modifiers::COMMAND.plus(Modifiers::SHIFT);
    let alt = Modifiers::ALT;
    match a {
        BindAction::Save => KeyboardShortcut::new(cmd, Key::S),
        BindAction::SaveAs => KeyboardShortcut::new(cmd_shift, Key::S),
        BindAction::CloseTab => KeyboardShortcut::new(cmd, Key::W),
        BindAction::NewFile => KeyboardShortcut::new(cmd, Key::N),
        BindAction::NewWindow => KeyboardShortcut::new(cmd_shift, Key::N),
        BindAction::PaletteFiles => KeyboardShortcut::new(cmd, Key::P),
        BindAction::PaletteCommands => KeyboardShortcut::new(cmd_shift, Key::P),
        BindAction::ToggleTerminal => KeyboardShortcut::new(cmd, Key::J),
        BindAction::ToggleSidebar => KeyboardShortcut::new(cmd, Key::B),
        BindAction::Find => KeyboardShortcut::new(cmd, Key::F),
        // ⌘⇧C / ⌘⇧V は egui-winit 0.29 が **押下イベントごと** 握り潰して
        // `Event::Copy` / `Event::Paste` にすり替える (shift の有無を見ていない)。
        // 素の `InputState::consume_shortcut` では絶対に発火しないので、
        // [`consume_shortcut_compat`] を通して拾い直すこと。
        BindAction::ToggleCockpit => KeyboardShortcut::new(cmd_shift, Key::C),
        BindAction::ToggleKanban => KeyboardShortcut::new(cmd_shift, Key::K),
        // ⌥⌘D = デッキ (Deck)。⇧⌘D は「行を複製」に埋まっているので ⌥ 側を使う。
        // ⌥⌘ 系の既存割り当ては S / F / [ / ] / B だけなので衝突しない。
        // 中央ビューの切替は ⌘⇧ 系で揃える (Cockpit=⌘⇧C / 看板=⌘⇧K / デッキ=⌘⇧L)。
        // ⌘⌥ 系は macOS で OS 機能 (⌘⌥D=Dock) や文字合成のデッドキー (⌥E=´) に
        // 取られてアプリまで届かないため使わない — 実機で 2 回踏んだ。
        BindAction::ToggleDeck => KeyboardShortcut::new(cmd_shift, Key::L),
        BindAction::ToggleMdPreview => KeyboardShortcut::new(cmd_shift, Key::V),
        BindAction::NewAgent => KeyboardShortcut::new(cmd_shift, Key::A),
        // 画面全体のズーム。ブラウザ / VS Code と同じ ⌘+ / ⌘- / ⌘0。
        // (⌘= も同義として app.rs 側で拾う — US 配列では + が ⇧= で打ちにくいため)
        BindAction::ZoomIn => KeyboardShortcut::new(cmd, Key::Plus),
        BindAction::ZoomOut => KeyboardShortcut::new(cmd, Key::Minus),
        BindAction::ZoomReset => KeyboardShortcut::new(cmd, Key::Num0),
        // ファイル単位のズームは画面全体に ⌥ を足した形にする
        // (「同じ操作の、対象が狭い版」が打鍵でも伝わる)。
        //
        // ⌥⌘ 系の既存割り当ては S / F / B / [ / ] だけなので衝突しない。
        // macOS の「アクセシビリティ拡大」も ⌥⌘= / ⌥⌘- を使うが、
        // これは既定でオフ (システム設定 → アクセシビリティ → ズーム →
        // 「キーボードショートカットを使用してズーム」)。有効にしている人は
        // config.toml の [keybindings] で `file_zoom_in` などを付け替えられる。
        BindAction::FileZoomIn => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::Plus),
        BindAction::FileZoomOut => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::Minus),
        BindAction::FileZoomReset => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::Num0),
        BindAction::Undo => KeyboardShortcut::new(cmd, Key::Z),
        BindAction::Redo => KeyboardShortcut::new(cmd_shift, Key::Z),
        BindAction::ToggleComment => KeyboardShortcut::new(cmd, Key::Slash),
        BindAction::DuplicateLine => KeyboardShortcut::new(cmd_shift, Key::D),
        BindAction::MoveLineUp => KeyboardShortcut::new(alt, Key::ArrowUp),
        BindAction::MoveLineDown => KeyboardShortcut::new(alt, Key::ArrowDown),
        BindAction::FocusExplorer => KeyboardShortcut::new(cmd_shift, Key::E),
        BindAction::OpenFile => KeyboardShortcut::new(cmd, Key::O),
        BindAction::SaveAll => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::S),
        BindAction::GoToLine => KeyboardShortcut::new(Modifiers::CTRL, Key::G),
        BindAction::NextTab => KeyboardShortcut::new(cmd_shift, Key::CloseBracket),
        BindAction::PrevTab => KeyboardShortcut::new(cmd_shift, Key::OpenBracket),
        // ⌃Tab / ⌃⇧Tab は VS Code / Zed / ブラウザ共通の「最近使ったタブへ」。
        // `Modifiers::CTRL` は **どの OS でも物理 Ctrl** に当たる
        // (mac: ctrl だけ / Windows・Linux: ctrl と command の両方が立つが、
        //  `cmd_ctrl_matches` は「パターンが ctrl を求めるなら command は
        //  どちらでもよい」なので両方で一致する)。⌘Tab は macOS の
        // アプリ切替に取られているので**使わない** (`MACOS_RESERVED`)。
        BindAction::SwitchTab => KeyboardShortcut::new(Modifiers::CTRL, Key::Tab),
        BindAction::SwitchTabBack => {
            KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Tab)
        }
        BindAction::GlobalSearch => KeyboardShortcut::new(cmd_shift, Key::F),
        // VS Code の「ファイル間で置換」と同じ ⇧⌘H。既存の割り当てとは重ならない
        BindAction::GlobalReplace => KeyboardShortcut::new(cmd_shift, Key::H),
        BindAction::OpenReplace => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::F),
        BindAction::NewTerminal => {
            KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Backtick)
        }
        // 戻る。**macOS 以外では ⌥ を足す** — Windows/Linux の Modifiers::CTRL は
        // 「⌘ (COMMAND)」と同じ打鍵なので、Ctrl+- のままだとブラウザでも
        // VS Code でも縮小である Ctrl+- を先に食ってしまい、画面全体の
        // ズームアウトが永久に効かなくなる (VS Code の Windows 版も
        // 「戻る」は Ctrl+Alt+- を使っている)。
        BindAction::NavBack => {
            if cfg!(target_os = "macos") {
                KeyboardShortcut::new(Modifiers::CTRL, Key::Minus)
            } else {
                KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::ALT), Key::Minus)
            }
        }
        BindAction::NavForward => {
            KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Minus)
        }
        BindAction::GoToDefinition => KeyboardShortcut::new(Modifiers::NONE, Key::F12),
        BindAction::GoToBracket => KeyboardShortcut::new(cmd_shift, Key::Backslash),
        // 診断ジャンプは VS Code と同じ F8 / ⇧F8。F7 / ⇧F7 (差分の変更ジャンプ) の
        // 隣で、macOS の予約表 (F11 = デスクトップ表示) とも衝突しない。
        BindAction::NextProblem => KeyboardShortcut::new(Modifiers::NONE, Key::F8),
        BindAction::PrevProblem => KeyboardShortcut::new(Modifiers::SHIFT, Key::F8),
        BindAction::RunBuildTask => KeyboardShortcut::new(cmd_shift, Key::B),
        BindAction::ToggleProblems => KeyboardShortcut::new(cmd_shift, Key::M),
        BindAction::ToggleFullScreen => {
            KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::COMMAND), Key::F)
        }
        // ⌥⌘[ / ⌥⌘] は VS Code (mac) の折りたたみ / 展開と同じ。
        // 既存の ⌥⌘ 割り当ては S (すべて保存) と F (置換) だけなので空いている。
        BindAction::ToggleFold => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::OpenBracket),
        BindAction::UnfoldAll => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::CloseBracket),
        BindAction::ToggleBookmark => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::B),
        BindAction::ReopenClosedTab => KeyboardShortcut::new(cmd_shift, Key::T),
        // ⌃Space は macOS が「前の入力ソース」に予約している (実測: 本文の
        // `MACOS_RESERVED` を参照)。既定は ⌘I にして、⌃Space は
        // `app.rs::lsp_completion_tick` が追加で拾う。
        BindAction::LspCompletion => KeyboardShortcut::new(cmd, Key::I),
        BindAction::LspReferences => KeyboardShortcut::new(Modifiers::SHIFT, Key::F12),
        BindAction::LspSymbols => KeyboardShortcut::new(cmd_shift, Key::O),
        BindAction::LspRename => KeyboardShortcut::new(Modifiers::NONE, Key::F2),
        BindAction::LspFormat => {
            KeyboardShortcut::new(Modifiers::SHIFT.plus(Modifiers::ALT), Key::F)
        }
        // VS Code と同じ ⌘. / Ctrl+.。⌘ 単独の既存割り当ては
        // S W N P J B F +/- / O D だけで `.` は空いている。
        // macOS の OS 予約 (⌘Space=Spotlight, ⌘⌥D=Dock, ⌃Space=入力ソース) にも
        // 当たらない — ⌘. はダイアログのキャンセル相当だがアプリまで届く。
        BindAction::LspCodeAction => KeyboardShortcut::new(cmd, Key::Period),
        // VS Code (mac) の「パラメーターヒントを表示」と同じ ⇧⌘Space。
        // ⌘Space (Spotlight) / ⌥⌘Space (Finder 検索) / ⌃Space (入力ソース切替 =
        // 補完に使用中) のいずれとも別で、⇧⌘ 系にも Space の割り当ては無い。
        BindAction::LspSignatureHelp => KeyboardShortcut::new(cmd_shift, Key::Space),
        // VS Code と同じ ⌘D。⇧⌘D (行の複製) とは別で、既存の割り当てとは重ならない
        BindAction::SelectNextOccurrence => KeyboardShortcut::new(cmd, Key::D),
        // ── エディタの分割 ────────────────────────────────────────
        // ⌘\ は VS Code の「エディターの分割」そのもの。⌥⌘\ は下方向。
        // どちらも既定表にも、端末側の分割コード (⌘⌥ / Ctrl+Alt + N W Z E
        // H J K L と矢印) にも無い。macOS の OS 予約 (⌘⌥D=Dock・⌘⌥Esc・
        // ⌘Space・⌃↑/↓) とも重ならない。
        BindAction::SplitEditorRight => KeyboardShortcut::new(cmd, Key::Backslash),
        BindAction::SplitEditorDown => {
            KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::ALT), Key::Backslash)
        }
        // ⌘1..⌘3 は VS Code の「エディターグループへフォーカス」と同じ。
        // 既定表に数字キーの割り当ては 1 つも無い。
        BindAction::FocusPane1 => KeyboardShortcut::new(cmd, Key::Num1),
        BindAction::FocusPane2 => KeyboardShortcut::new(cmd, Key::Num2),
        BindAction::FocusPane3 => KeyboardShortcut::new(cmd, Key::Num3),
        // 差分の次/前の変更 = VS Code と同じ F7 / ⇧F7。
        // ファンクションキーの既存割り当ては F2 (リネーム) / F12 (定義へ) /
        // ⇧F12 (参照検索) だけなので F7 系は空いている。修飾キー無しの単打だが、
        // 差分ビューが出ていないときは `diff::take_jump` が捨てるので誤爆しない。
        // macOS が予約するのは ⌘⌥D (Dock) や F11 (Mission Control) 系で、
        // F7 は既定でメディアキー扱いでもアプリへ届く (fn 併用が要る機種はある)。
        BindAction::DiffNextChange => KeyboardShortcut::new(Modifiers::NONE, Key::F7),
        BindAction::DiffPrevChange => KeyboardShortcut::new(Modifiers::SHIFT, Key::F7),
        // `]f` / `[f` (cmux) の **1 打鍵目**。2 打鍵目は [`default_binding`] が付ける。
        // 修飾キー無しの `]` `[` を prefix にしているので、**テキスト入力に
        // フォーカスがあるフレームでは消費しない** (app.rs 側のガード)。
        // macOS の予約表 ([`MACOS_RESERVED`]) にブラケットは無く、既存の
        // ⌘⇧] / ⌘⇧[ (タブ移動) とは修飾キーが違うので食い合わない。
        BindAction::DiffNextFile => KeyboardShortcut::new(Modifiers::NONE, Key::CloseBracket),
        BindAction::DiffPrevFile => KeyboardShortcut::new(Modifiers::NONE, Key::OpenBracket),
        // ⌘K ⌘S (VS Code の「キーボードショートカット」) の **1 打鍵目**。
        // 2 打鍵目は [`default_binding`] が付ける。⌘K 単打の既定は他に無い。
        BindAction::KeybindEditor => KeyboardShortcut::new(cmd, Key::K),
        // ── 追従 / 未読カーソル ──────────────────────────────────
        // ⇧⌘ 系で空いている G / R / U / J / I を使う。
        // * 既存の ⇧⌘ 割り当ては C K L V A Z D E ] [ F H T O B M Backslash
        //   Space S N P。
        // * `MACOS_RESERVED` の ⇧⌘ 系は Tab / 3 / 4 / 5 / Slash / Q だけ。
        // * ⌥ を混ぜないのは、mac の ⌥ + 母音 (E I N U) がアクセント合成の
        //   デッドキーになりアプリまで届かないため (⌘⌥D = Dock と同じ轍)。
        BindAction::FollowAgent => KeyboardShortcut::new(cmd_shift, Key::G),
        BindAction::FollowResume => KeyboardShortcut::new(cmd_shift, Key::R),
        BindAction::NextUnread => KeyboardShortcut::new(cmd_shift, Key::U),
        BindAction::DeferUnread => KeyboardShortcut::new(cmd_shift, Key::J),
        BindAction::ToggleUnread => KeyboardShortcut::new(cmd_shift, Key::I),
    }
}

/// 既定のバインド。2 打鍵 (chord) を持つのはここだけが知っている。
///
/// [`default_shortcut`] は「1 打鍵目」を返すので、chord の全体は
/// こちらを見ること (画面表示・衝突検出・config への書き戻しは全部これ)。
pub fn default_binding(a: BindAction) -> Binding {
    match a {
        // VS Code と同じ ⌘K ⌘S。⌘K は prefix 専用で、単打の割り当ては持たない。
        BindAction::KeybindEditor => Binding::Chord(
            KeyboardShortcut::new(Modifiers::COMMAND, Key::K),
            KeyboardShortcut::new(Modifiers::COMMAND, Key::S),
        ),
        // cmux と同じ `]f` / `[f`。**ファイル間のジャンプが並列レビューの単位**
        // なので、スクロール系 (F7) とは別の打鍵にしてある。
        BindAction::DiffNextFile => Binding::Chord(
            KeyboardShortcut::new(Modifiers::NONE, Key::CloseBracket),
            KeyboardShortcut::new(Modifiers::NONE, Key::F),
        ),
        BindAction::DiffPrevFile => Binding::Chord(
            KeyboardShortcut::new(Modifiers::NONE, Key::OpenBracket),
            KeyboardShortcut::new(Modifiers::NONE, Key::F),
        ),
        _ => Binding::Single(default_shortcut(a)),
    }
}

/// config.toml の `[keybindings]` に書くときの action 名。
///
/// **網羅 match にしてある** — `BindAction` に変種を足した瞬間にここが
/// コンパイルエラーになり、「名前を付け忘れて GUI から編集できない」
/// アクションが生まれない。
pub fn config_name(a: BindAction) -> &'static str {
    use BindAction::*;
    match a {
        Save => "save",
        SaveAs => "save_as",
        CloseTab => "close_tab",
        NewFile => "new_file",
        NewWindow => "new_window",
        PaletteFiles => "palette_files",
        PaletteCommands => "palette_commands",
        ToggleTerminal => "toggle_terminal",
        ToggleSidebar => "toggle_sidebar",
        Find => "find",
        ToggleCockpit => "toggle_cockpit",
        ToggleKanban => "toggle_kanban",
        ToggleDeck => "toggle_deck",
        ToggleMdPreview => "toggle_md_preview",
        NewAgent => "new_agent",
        ZoomIn => "zoom_in",
        ZoomOut => "zoom_out",
        ZoomReset => "zoom_reset",
        FileZoomIn => "file_zoom_in",
        FileZoomOut => "file_zoom_out",
        FileZoomReset => "file_zoom_reset",
        Undo => "undo",
        Redo => "redo",
        ToggleComment => "toggle_comment",
        DuplicateLine => "duplicate_line",
        MoveLineUp => "move_line_up",
        MoveLineDown => "move_line_down",
        FocusExplorer => "focus_explorer",
        OpenFile => "open_file",
        SaveAll => "save_all",
        GoToLine => "goto_line",
        NextTab => "next_tab",
        PrevTab => "prev_tab",
        SwitchTab => "switch_tab",
        SwitchTabBack => "switch_tab_back",
        GlobalSearch => "global_search",
        GlobalReplace => "global_replace",
        OpenReplace => "open_replace",
        NewTerminal => "new_terminal",
        NavBack => "nav_back",
        NavForward => "nav_forward",
        GoToDefinition => "goto_definition",
        GoToBracket => "goto_bracket",
        NextProblem => "next_problem",
        PrevProblem => "prev_problem",
        RunBuildTask => "run_build_task",
        ToggleProblems => "toggle_problems",
        ToggleFullScreen => "toggle_fullscreen",
        ToggleFold => "toggle_fold",
        UnfoldAll => "unfold_all",
        ToggleBookmark => "toggle_bookmark",
        ReopenClosedTab => "reopen_closed_tab",
        LspCompletion => "lsp_completion",
        LspReferences => "lsp_references",
        LspSymbols => "lsp_symbols",
        LspRename => "lsp_rename",
        LspFormat => "lsp_format",
        LspCodeAction => "lsp_code_action",
        LspSignatureHelp => "lsp_signature_help",
        SelectNextOccurrence => "select_next_occurrence",
        SplitEditorRight => "split_editor_right",
        SplitEditorDown => "split_editor_down",
        FocusPane1 => "focus_pane_1",
        FocusPane2 => "focus_pane_2",
        FocusPane3 => "focus_pane_3",
        DiffNextChange => "diff_next_change",
        DiffPrevChange => "diff_prev_change",
        DiffNextFile => "diff_next_file",
        DiffPrevFile => "diff_prev_file",
        KeybindEditor => "keybind_editor",
        FollowAgent => "follow_agent",
        FollowResume => "follow_resume",
        NextUnread => "next_unread",
        DeferUnread => "defer_unread",
        ToggleUnread => "toggle_unread",
    }
}

/// 画面に出すアクション名 (キーバインド編集 UI の 1 列目)。
///
/// 表示文字列なので呼び出し側が [`crate::i18n::tr`] へ通す。ここでは
/// 翻訳前の原文だけを持つ (i18n の辞書キーになる)。
pub fn action_label(a: BindAction) -> &'static str {
    use BindAction::*;
    match a {
        Save => "保存",
        SaveAs => "名前を付けて保存",
        CloseTab => "タブを閉じる",
        NewFile => "新規ファイル",
        NewWindow => "新しいウィンドウ",
        PaletteFiles => "ファイルへ移動",
        PaletteCommands => "コマンド パレット",
        ToggleTerminal => "ターミナルの表示切替",
        ToggleSidebar => "サイドバーの表示切替",
        Find => "検索",
        ToggleCockpit => "コックピットの表示切替",
        ToggleKanban => "フリート看板の表示切替",
        ToggleDeck => "エージェントデッキの表示切替",
        ToggleMdPreview => "Markdown プレビュー切替",
        NewAgent => "新しいエージェント",
        ZoomIn => "画面をズームイン",
        ZoomOut => "画面をズームアウト",
        ZoomReset => "画面のズームを戻す",
        FileZoomIn => "このファイルをズームイン",
        FileZoomOut => "このファイルをズームアウト",
        FileZoomReset => "このファイルのズームを戻す",
        Undo => "元に戻す",
        Redo => "やり直し",
        ToggleComment => "行コメントの切り替え",
        DuplicateLine => "行を複製",
        MoveLineUp => "行を上へ移動",
        MoveLineDown => "行を下へ移動",
        FocusExplorer => "エクスプローラーへフォーカス",
        OpenFile => "ファイルを開く",
        SaveAll => "すべて保存",
        GoToLine => "行/列へ移動",
        NextTab => "次のエディター",
        PrevTab => "前のエディター",
        SwitchTab => "最近のタブへ切替",
        SwitchTabBack => "最近のタブへ切替 (逆順)",
        GlobalSearch => "ファイル間で検索",
        GlobalReplace => "ファイル間で置換",
        OpenReplace => "置換",
        NewTerminal => "新しいターミナル",
        NavBack => "戻る",
        NavForward => "進む",
        GoToDefinition => "定義へ移動",
        GoToBracket => "ブラケットへ移動",
        NextProblem => "次の問題へ移動",
        PrevProblem => "前の問題へ移動",
        RunBuildTask => "ビルドタスクの実行",
        ToggleProblems => "問題パネルの表示切替",
        ToggleFullScreen => "フルスクリーン切替",
        ToggleFold => "折りたたみの切り替え",
        UnfoldAll => "すべて展開",
        ToggleBookmark => "ブックマークの切り替え",
        ReopenClosedTab => "閉じたエディターを開き直す",
        LspCompletion => "補完候補を表示",
        LspReferences => "参照を検索",
        LspSymbols => "シンボルへジャンプ",
        LspRename => "名前の変更",
        LspFormat => "ドキュメントの整形",
        LspCodeAction => "クイックフィックス",
        LspSignatureHelp => "引数ヒントを表示",
        SelectNextOccurrence => "次の出現を選択",
        SplitEditorRight => "エディタを右に分割",
        SplitEditorDown => "エディタを下に分割",
        FocusPane1 => "1 番目のペインへ",
        FocusPane2 => "2 番目のペインへ",
        FocusPane3 => "3 番目のペインへ",
        DiffNextChange => "次の変更へ",
        DiffPrevChange => "前の変更へ",
        DiffNextFile => "次の差分ファイルへ",
        DiffPrevFile => "前の差分ファイルへ",
        KeybindEditor => "キーボード ショートカットの設定",
        FollowAgent => "エージェントを追従 (開始/解除)",
        FollowResume => "追従を再開",
        NextUnread => "次の未読エージェントへ",
        DeferUnread => "あとで見る (未読に戻して次へ)",
        ToggleUnread => "未読の切り替え",
    }
}

/// 1 アクションへの割り当て。単打と 2 打鍵 (chord) の両方を表せる。
///
/// 以前は 1 アクション = 1 [`KeyboardShortcut`] だったため ⌘K ⌘S のような
/// VS Code の chord を表現できなかった。ここを enum にしたことで、
/// 「⌘K は prefix、単打の割り当ては持てない」という関係も
/// [`Conflict::Prefix`] として検出できるようになっている。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Binding {
    Single(KeyboardShortcut),
    Chord(KeyboardShortcut, KeyboardShortcut),
}

impl Binding {
    /// 1 打鍵目。単打ならそれ自身。
    pub fn first(self) -> KeyboardShortcut {
        match self {
            Binding::Single(a) | Binding::Chord(a, _) => a,
        }
    }

    /// 2 打鍵目 (単打なら None)。
    pub fn second(self) -> Option<KeyboardShortcut> {
        match self {
            Binding::Single(_) => None,
            Binding::Chord(_, b) => Some(b),
        }
    }

    /// 修飾キーの表現ゆれを畳んだ形。比較・保存はこれを通す。
    pub fn canonical(self) -> Self {
        match self {
            Binding::Single(a) => Binding::Single(canonical_shortcut(a)),
            Binding::Chord(a, b) => Binding::Chord(canonical_shortcut(a), canonical_shortcut(b)),
        }
    }
}

/// アクション → バインドの解決テーブル。
pub struct Keybinds {
    map: HashMap<BindAction, Binding>,
}

impl Keybinds {
    /// デフォルト + config の上書き (action名文字列 → 打鍵文字列) から構築。
    /// 不正な文字列は無視してデフォルト維持。
    pub fn from_overrides(overrides: &HashMap<String, String>) -> Self {
        let mut map = HashMap::with_capacity(ALL_ACTIONS.len());
        for a in ALL_ACTIONS {
            map.insert(a, default_binding(a));
        }
        for (name, spec) in overrides {
            if let (Some(action), Some(binding)) =
                (Self::action_from_name(name), parse_binding(spec))
            {
                map.insert(action, binding.canonical());
            }
        }
        Self { map }
    }

    /// **1 打鍵目**のショートカット。chord でも 1 打鍵目しか返さないので、
    /// 画面表示には [`Self::label`]、消費には [`Self::binding`] を使うこと。
    pub fn get(&self, a: BindAction) -> KeyboardShortcut {
        self.binding(a).first()
    }

    /// 割り当ての全体 (chord を含む)。
    pub fn binding(&self, a: BindAction) -> Binding {
        self.map
            .get(&a)
            .copied()
            .unwrap_or_else(|| default_binding(a))
    }

    /// 画面に出す打鍵表記。chord は "⌘K ⌘S" のように 2 つ並べる。
    pub fn label(&self, a: BindAction) -> String {
        format_binding(self.binding(a))
    }

    /// GUI からの再割り当て。
    pub fn set(&mut self, a: BindAction, b: Binding) {
        self.map.insert(a, b.canonical());
    }

    /// 1 行を既定へ戻す。
    pub fn reset(&mut self, a: BindAction) {
        self.map.insert(a, default_binding(a));
    }

    /// 全部を既定へ戻す。
    pub fn reset_all(&mut self) {
        for a in ALL_ACTIONS {
            self.map.insert(a, default_binding(a));
        }
    }

    pub fn is_default(&self, a: BindAction) -> bool {
        self.binding(a).canonical() == default_binding(a).canonical()
    }

    /// config.toml の `[keybindings]` へ書き戻す形。
    /// **既定と同じ行は入れない** — 既定を変えたときに古い値へ固定されないため。
    pub fn overrides(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for a in ALL_ACTIONS {
            if !self.is_default(a) {
                out.insert(config_name(a).to_string(), binding_spec(self.binding(a)));
            }
        }
        out
    }

    /// config で使う action 名 → アクション。
    pub fn action_from_name(name: &str) -> Option<BindAction> {
        use BindAction::*;
        Some(match name {
            "save" => Save,
            "save_as" => SaveAs,
            "close_tab" => CloseTab,
            "new_file" => NewFile,
            "new_window" => NewWindow,
            "palette_files" => PaletteFiles,
            "palette_commands" => PaletteCommands,
            "toggle_terminal" => ToggleTerminal,
            "toggle_sidebar" => ToggleSidebar,
            "find" => Find,
            "toggle_cockpit" => ToggleCockpit,
            "toggle_kanban" => ToggleKanban,
            "toggle_deck" => ToggleDeck,
            "toggle_md_preview" => ToggleMdPreview,
            "new_agent" => NewAgent,
            "zoom_in" => ZoomIn,
            "zoom_out" => ZoomOut,
            "zoom_reset" => ZoomReset,
            "file_zoom_in" => FileZoomIn,
            "file_zoom_out" => FileZoomOut,
            "file_zoom_reset" => FileZoomReset,
            // v0.5.1 までの名前。既存の config.toml を黙って壊さないための別名。
            "font_inc" => ZoomIn,
            "font_dec" => ZoomOut,
            "undo" => Undo,
            "redo" => Redo,
            "toggle_comment" => ToggleComment,
            "duplicate_line" => DuplicateLine,
            "move_line_up" => MoveLineUp,
            "move_line_down" => MoveLineDown,
            "focus_explorer" => FocusExplorer,
            "open_file" => OpenFile,
            "save_all" => SaveAll,
            "goto_line" => GoToLine,
            "next_tab" => NextTab,
            "prev_tab" => PrevTab,
            "switch_tab" => SwitchTab,
            "switch_tab_back" => SwitchTabBack,
            "global_search" => GlobalSearch,
            "global_replace" => GlobalReplace,
            "open_replace" => OpenReplace,
            "new_terminal" => NewTerminal,
            "nav_back" => NavBack,
            "nav_forward" => NavForward,
            "goto_definition" => GoToDefinition,
            "goto_bracket" => GoToBracket,
            "next_problem" => NextProblem,
            "prev_problem" => PrevProblem,
            "run_build_task" => RunBuildTask,
            "toggle_problems" => ToggleProblems,
            "toggle_fullscreen" => ToggleFullScreen,
            "toggle_fold" => ToggleFold,
            "unfold_all" => UnfoldAll,
            "toggle_bookmark" => ToggleBookmark,
            "reopen_closed_tab" => ReopenClosedTab,
            "lsp_completion" => LspCompletion,
            "lsp_references" => LspReferences,
            "lsp_symbols" => LspSymbols,
            "lsp_rename" => LspRename,
            "lsp_format" => LspFormat,
            "lsp_code_action" => LspCodeAction,
            "lsp_signature_help" => LspSignatureHelp,
            "select_next_occurrence" => SelectNextOccurrence,
            "split_editor_right" => SplitEditorRight,
            "split_editor_down" => SplitEditorDown,
            "focus_pane_1" => FocusPane1,
            "focus_pane_2" => FocusPane2,
            "focus_pane_3" => FocusPane3,
            "diff_next_change" => DiffNextChange,
            "diff_prev_change" => DiffPrevChange,
            "diff_next_file" => DiffNextFile,
            "diff_prev_file" => DiffPrevFile,
            "keybind_editor" => KeybindEditor,
            "follow_agent" => FollowAgent,
            "follow_resume" => FollowResume,
            "next_unread" => NextUnread,
            "defer_unread" => DeferUnread,
            "toggle_unread" => ToggleUnread,
            _ => return None,
        })
    }
}

impl Default for Keybinds {
    fn default() -> Self {
        Self::from_overrides(&HashMap::new())
    }
}

/// "cmd+shift+p" / "ctrl+`" / "alt+up" / "cmd+/" 形式をパース。
/// modifier: cmd|ctrl|shift|alt|option(=alt)。key は最後の要素。
/// 解釈できない場合は None。
pub fn parse_shortcut(s: &str) -> Option<KeyboardShortcut> {
    let s = s.trim().to_ascii_lowercase();
    if s.is_empty() {
        return None;
    }
    let parts: Vec<&str> = s.split('+').map(str::trim).collect();
    let (key_part, mod_parts) = parts.split_last()?;
    let key = key_from_name(key_part)?;
    let mut mods = Modifiers::NONE;
    for m in mod_parts {
        mods = mods.plus(modifier_from_name(m)?);
    }
    Some(KeyboardShortcut::new(mods, key))
}

/// "cmd+s" (単打) / "cmd+k cmd+s" (2 打鍵) をパースする。
///
/// **単打の解釈を先に試す**のが要点。旧来の config.toml には
/// `"cmd + shift + p"` のように空白を挟んだ書き方があり得るので、
/// 先に空白で割ると既存ユーザーの設定を黙って壊す。
/// 3 打鍵以上・空文字・不正な語は `None` (panic しない)。
pub fn parse_binding(s: &str) -> Option<Binding> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(sc) = parse_shortcut(s) {
        return Some(Binding::Single(sc));
    }
    let mut it = s.split_whitespace();
    let a = parse_shortcut(it.next()?)?;
    let b = parse_shortcut(it.next()?)?;
    if it.next().is_some() {
        // 3 打鍵以上は非対応 (VS Code / Zed も 2 打鍵まで)
        return None;
    }
    Some(Binding::Chord(a, b))
}

/// バインドを画面表示用の文字列にする ("⌘K ⌘S" / "Ctrl+K Ctrl+S")。
pub fn format_binding(b: Binding) -> String {
    match b {
        Binding::Single(a) => format_shortcut(a),
        Binding::Chord(a, c) => format!("{} {}", format_shortcut(a), format_shortcut(c)),
    }
}

/// config.toml へ書く形 ("cmd+shift+p")。[`parse_shortcut`] の逆。
pub fn shortcut_spec(sc: KeyboardShortcut) -> String {
    let m = canonical_mods(sc.modifiers);
    let mut parts: Vec<&str> = Vec::new();
    if m.ctrl {
        parts.push("ctrl");
    }
    if m.alt {
        parts.push("alt");
    }
    if m.shift {
        parts.push("shift");
    }
    if m.command || m.mac_cmd {
        parts.push("cmd");
    }
    let mut s = parts.join("+");
    if !s.is_empty() {
        s.push('+');
    }
    s.push_str(key_spec(sc.logical_key));
    s
}

/// config.toml へ書く形 ("cmd+k cmd+s")。[`parse_binding`] の逆。
pub fn binding_spec(b: Binding) -> String {
    match b {
        Binding::Single(a) => shortcut_spec(a),
        Binding::Chord(a, c) => format!("{} {}", shortcut_spec(a), shortcut_spec(c)),
    }
}

/// [`key_from_name`] が受け取れる綴り。全 [`Key`] を網羅する必要はないが、
/// 往復テスト (`spec_roundtrip_*`) が「戻せない綴りを書いていない」ことを見張る。
fn key_spec(key: Key) -> &'static str {
    use Key::*;
    match key {
        ArrowUp => "up",
        ArrowDown => "down",
        ArrowLeft => "left",
        ArrowRight => "right",
        Enter => "enter",
        Escape => "escape",
        Tab => "tab",
        Space => "space",
        Backtick => "backtick",
        Plus => "plus",
        Minus => "minus",
        Equals => "equals",
        Slash => "slash",
        Comma => "comma",
        Period => "period",
        OpenBracket => "openbracket",
        CloseBracket => "closebracket",
        Backslash => "backslash",
        Num0 => "0",
        Num1 => "1",
        Num2 => "2",
        Num3 => "3",
        Num4 => "4",
        Num5 => "5",
        Num6 => "6",
        Num7 => "7",
        Num8 => "8",
        Num9 => "9",
        A => "a",
        B => "b",
        C => "c",
        D => "d",
        E => "e",
        F => "f",
        G => "g",
        H => "h",
        I => "i",
        J => "j",
        K => "k",
        L => "l",
        M => "m",
        N => "n",
        O => "o",
        P => "p",
        Q => "q",
        R => "r",
        S => "s",
        T => "t",
        U => "u",
        V => "v",
        W => "w",
        X => "x",
        Y => "y",
        Z => "z",
        F1 => "f1",
        F2 => "f2",
        F3 => "f3",
        F4 => "f4",
        F5 => "f5",
        F6 => "f6",
        F7 => "f7",
        F8 => "f8",
        F9 => "f9",
        F10 => "f10",
        F11 => "f11",
        F12 => "f12",
        F13 => "f13",
        F14 => "f14",
        F15 => "f15",
        F16 => "f16",
        F17 => "f17",
        F18 => "f18",
        F19 => "f19",
        F20 => "f20",
        // 上のどれでもないキー (Insert / Home / …) は綴りを持たない。
        // 記録 UI 側が [`is_recordable`] で弾くので、ここへは来ない。
        _ => "",
    }
}

/// このキーを config.toml へ書き戻せるか (= 記録して良いか)。
pub fn is_recordable(key: Key) -> bool {
    !key_spec(key).is_empty()
}

// ─────────────────────────────────────────────────────────────────────────
// 修飾キーの正規化 — 「同じ打鍵かどうか」を 1 か所で決める
// ─────────────────────────────────────────────────────────────────────────

/// 修飾キーの表現ゆれを畳む。
///
/// egui は同じ物理キーを複数のフラグへ写す:
/// macOS の ⌘ は `command` と `mac_cmd` の両方、Windows/Linux の Ctrl は
/// `command` と `ctrl` の両方に立つ。素の構造体比較で衝突検出をすると
/// 「⌘S と ⌘S が別物」になるので、必ずここを通してから比べる。
pub fn canonical_mods(m: Modifiers) -> Modifiers {
    let mut out = Modifiers::NONE;
    out.alt = m.alt;
    out.shift = m.shift;
    if cfg!(target_os = "macos") {
        // mac: ⌘ (command/mac_cmd) と ⌃ (ctrl) は別のキー
        out.command = m.command || m.mac_cmd;
        out.ctrl = m.ctrl;
    } else {
        // 他 OS: Ctrl は command にも写る。同じ打鍵なので 1 つへ畳む
        out.command = m.command || m.ctrl || m.mac_cmd;
    }
    out
}

/// 打鍵を正規形にする (キーはそのまま、修飾キーだけ畳む)。
pub fn canonical_shortcut(sc: KeyboardShortcut) -> KeyboardShortcut {
    KeyboardShortcut::new(canonical_mods(sc.modifiers), sc.logical_key)
}

/// 2 つの打鍵が「同じ打鍵」か。修飾キーの表現ゆれを畳んでから比べる。
pub fn same_stroke(a: KeyboardShortcut, b: KeyboardShortcut) -> bool {
    canonical_shortcut(a) == canonical_shortcut(b)
}

// ─────────────────────────────────────────────────────────────────────────
// 衝突と OS 予約
// ─────────────────────────────────────────────────────────────────────────

/// 割り当てをそのまま採用できない理由。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Conflict {
    /// 同じ打鍵が別のアクションにも割り当たっている
    Duplicate(BindAction),
    /// chord の prefix と単打がぶつかっている
    /// (⌘K が chord の 1 打鍵目なら、⌘K 単打は永久に発火しない)
    Prefix(BindAction),
    /// macOS が OS 側で握っていてアプリまで届かない
    Reserved(&'static str),
}

/// この打鍵を macOS が OS 側で握っているか。握っていれば理由を返す。
pub fn macos_reservation(sc: KeyboardShortcut) -> Option<&'static str> {
    let sc = canonical_shortcut(sc);
    MACOS_RESERVED.iter().find_map(|(m, k, why)| {
        (sc.logical_key == *k && canonical_mods(*m) == sc.modifiers).then_some(*why)
    })
}

/// `action` に `candidate` を割り当てたときの問題を全部並べる。
///
/// GUI の「記録モード」で押した瞬間に出すためのもの。**自分自身とは
/// 衝突しない** (同じ打鍵を割り当て直しただけ、を警告にしない)。
pub fn conflicts_for(keys: &Keybinds, action: BindAction, candidate: Binding) -> Vec<Conflict> {
    let cand = candidate.canonical();
    let mut out: Vec<Conflict> = Vec::new();
    for other in ALL_ACTIONS {
        if other == action {
            continue;
        }
        let ob = keys.binding(other).canonical();
        if ob == cand {
            out.push(Conflict::Duplicate(other));
            continue;
        }
        // prefix と単打の食い合い。片方が chord・もう片方が単打で、
        // 1 打鍵目が同じなら単打の方は絶対に発火しない。
        let clash = match (cand, ob) {
            (Binding::Single(s), Binding::Chord(p, _)) => same_stroke(s, p),
            (Binding::Chord(p, _), Binding::Single(s)) => same_stroke(p, s),
            _ => false,
        };
        if clash {
            out.push(Conflict::Prefix(other));
        }
    }
    // macOS の実測予約表。1 打鍵目・2 打鍵目の両方を見る。
    for sc in [Some(cand.first()), cand.second()].into_iter().flatten() {
        if let Some(why) = macos_reservation(sc) {
            out.push(Conflict::Reserved(why));
        }
    }
    out
}

// ─────────────────────────────────────────────────────────────────────────
// chord (2 打鍵) の待機
// ─────────────────────────────────────────────────────────────────────────

/// chord の 1 打鍵目を押してから 2 打鍵目を待つ時間。
/// VS Code / Zed と同じ約 1 秒 (ベタ書きせずここから取ること)。
pub const CHORD_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1000);

/// chord の待機状態。**UI より長生きさせる持ち物ではない**が、
/// フレームをまたぐので `App` が持つ。
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ChordState {
    pending: Option<KeyboardShortcut>,
    /// `InputState::time` (アプリ起動からの秒) で表した期限。
    deadline: f64,
    /// IME 変換中か (フレームをまたいで持続する)。
    ime: bool,
}

/// [`ChordState::begin_frame`] の結果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChordTick {
    /// 待機していない
    Idle,
    /// prefix を押して 2 打鍵目を待っている
    Waiting,
    /// 時間切れで待機を捨てた
    TimedOut,
    /// Escape で待機を中断した
    Cancelled,
}

impl ChordState {
    /// 待機中の prefix。ステータスバーの表示はこれを見る。
    pub fn pending(&self) -> Option<KeyboardShortcut> {
        self.pending
    }

    pub fn is_waiting(&self) -> bool {
        self.pending.is_some()
    }

    pub fn ime_active(&self) -> bool {
        self.ime
    }

    pub fn clear(&mut self) {
        self.pending = None;
        self.deadline = 0.0;
    }

    /// prefix を受け取って待機に入る。
    pub fn arm(&mut self, prefix: KeyboardShortcut, now: f64) {
        self.pending = Some(prefix);
        self.deadline = now + CHORD_TIMEOUT.as_secs_f64();
    }

    /// 残り時間 (秒)。待機していなければ 0。
    pub fn remaining(&self, now: f64) -> f64 {
        if self.pending.is_some() {
            (self.deadline - now).max(0.0)
        } else {
            0.0
        }
    }

    /// IME 状態の追従だけを行う (egui に触らない純粋版。テスト用)。
    pub fn note_ime(&mut self, blocked: bool, ended: bool) {
        if blocked {
            self.ime = true;
        } else if ended {
            self.ime = false;
        }
    }

    /// 時間切れ / Escape の判定だけを行う純粋版。
    /// `escape` は「このフレームで Escape が押されたか」。
    pub fn tick(&mut self, now: f64, escape: bool) -> ChordTick {
        if self.pending.is_none() {
            return ChordTick::Idle;
        }
        if escape {
            self.clear();
            return ChordTick::Cancelled;
        }
        if now >= self.deadline {
            self.clear();
            return ChordTick::TimedOut;
        }
        ChordTick::Waiting
    }

    /// フレーム頭の更新。時間切れと Escape での中断をまとめて行う。
    /// **待機中の Escape はここで消費する** (他所へ渡さない)。
    ///
    /// IME 変換中かは呼び出し側が [`ime_blocks_shortcuts_now`] で 1 回だけ
    /// 判定して [`Self::note_ime`] で渡す (判定を 2 か所に持たない)。
    pub fn begin_frame(&mut self, i: &mut egui::InputState) -> ChordTick {
        if self.pending.is_none() {
            return ChordTick::Idle;
        }
        let esc = consume_shortcut_compat(i, KeyboardShortcut::new(Modifiers::NONE, Key::Escape));
        self.tick(i.time, esc)
    }
}

/// [`consume_shortcut_compat`] の chord 対応版。**アプリの消費は必ずここを通す。**
///
/// - 待機していないとき: 単打はそのまま消費、chord は 1 打鍵目だけ消費して待機に入る
///   (この時点では何も発火しない)
/// - 待機中: prefix の一致する chord だけが 2 打鍵目を消費できる。
///   単打は 1 つも発火しない (VS Code と同じ「prefix を握っている間は素通しなし」)
///
/// 2 打鍵目が X / C / V のとき (VS Code の ⌘K ⌘C 相当) は egui-winit が
/// 押下イベントごと捨てるので、**必ず** [`consume_shortcut_compat`] を通す。
pub fn consume_binding(i: &mut egui::InputState, b: Binding, chord: &mut ChordState) -> bool {
    match (chord.pending(), b) {
        (Some(prefix), Binding::Chord(p, second)) if same_stroke(p, prefix) => {
            if consume_shortcut_compat(i, second) {
                chord.clear();
                true
            } else {
                false
            }
        }
        // 待機中は他のバインドを一切通さない
        (Some(_), _) => false,
        (None, Binding::Single(sc)) => consume_shortcut_compat(i, sc),
        (None, Binding::Chord(p, _)) => {
            // IME 変換中は待機に入らない (変換確定の打鍵を prefix として食わない)
            if !chord.ime_active() && consume_shortcut_compat(i, p) {
                chord.arm(p, i.time);
            }
            false
        }
    }
}

// ─────────────────────────────────────────────────────────────────────────
// 打鍵の記録 (VS Code の "Record Keys")
// ─────────────────────────────────────────────────────────────────────────

/// 記録中の 1 行の状態。`App` が `Option<Recorder>` で持つ。
#[derive(Clone, Debug, PartialEq)]
pub struct Recorder {
    pub action: BindAction,
    first: Option<KeyboardShortcut>,
    /// 1 打鍵目を受けてから、2 打鍵目を待つ期限。
    deadline: f64,
}

impl Recorder {
    pub fn new(action: BindAction) -> Self {
        Self {
            action,
            first: None,
            deadline: 0.0,
        }
    }

    /// 途中経過 (画面に出す)。まだ何も押していなければ None。
    pub fn preview(&self) -> Option<Binding> {
        self.first.map(Binding::Single)
    }

    /// 打鍵を 1 つ受け取る。確定したらそのバインドを返す。
    ///
    /// 1 打鍵目からは [`CHORD_TIMEOUT`] だけ 2 打鍵目を待つ。
    /// 来れば chord、来なければ [`Self::tick`] が単打として確定させる。
    pub fn push(&mut self, sc: KeyboardShortcut, now: f64) -> Option<Binding> {
        let sc = canonical_shortcut(sc);
        match self.first {
            None => {
                self.first = Some(sc);
                self.deadline = now + CHORD_TIMEOUT.as_secs_f64();
                None
            }
            Some(a) => Some(Binding::Chord(a, sc)),
        }
    }

    /// 時間の経過だけを与える。単打として確定したらそれを返す。
    pub fn tick(&mut self, now: f64) -> Option<Binding> {
        let a = self.first?;
        (now >= self.deadline).then_some(Binding::Single(a))
    }

    /// 記録中に画面を動かし続けるための残り時間 (秒)。
    pub fn remaining(&self, now: f64) -> f64 {
        if self.first.is_some() {
            (self.deadline - now).max(0.0)
        } else {
            0.0
        }
    }
}

/// 記録モードでこのフレームの打鍵を 1 つ取り出す (取り出したイベントは捨てる)。
///
/// `Escape` は呼び出し側が中止に使うのでここでは返さない。
/// egui-winit に飲み込まれた ⌘⇧C 等は `Event::Cut/Copy/Paste` として届くので、
/// [`clipboard_alias`] の逆写像で X / C / V に戻す。
pub fn record_stroke(i: &mut egui::InputState) -> Option<KeyboardShortcut> {
    let mods = i.modifiers;
    let mut found: Option<KeyboardShortcut> = None;
    i.events.retain(|e| {
        if found.is_some() {
            return true;
        }
        let hit = match e {
            egui::Event::Key {
                key,
                pressed: true,
                modifiers,
                ..
            } => {
                if *key == Key::Escape || !is_recordable(*key) {
                    None
                } else {
                    Some(KeyboardShortcut::new(*modifiers, *key))
                }
            }
            // 押下イベントごとすり替えられた ⌘⇧X / ⌘⇧C / ⌘⇧V を拾い直す
            egui::Event::Cut => Some(KeyboardShortcut::new(mods, Key::X)),
            egui::Event::Copy => Some(KeyboardShortcut::new(mods, Key::C)),
            egui::Event::Paste(_) => Some(KeyboardShortcut::new(mods, Key::V)),
            _ => None,
        };
        match hit {
            Some(sc) => {
                found = Some(canonical_shortcut(sc));
                false
            }
            None => true,
        }
    });
    found
}

// ─────────────────────────────────────────────────────────────────────────
// キーバインド編集 UI のレイアウト (純粋関数)
// ─────────────────────────────────────────────────────────────────────────

/// 1 行の列幅。**どの幅でも見切れない**ことを保証するために純関数へ切り出す。
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct KeybindColumns {
    /// アクション名
    pub label_w: f32,
    /// 打鍵表記
    pub keys_w: f32,
    /// 衝突 / OS 予約の注記 (0 なら出さない)
    pub note_w: f32,
    /// 行末のボタン列
    pub buttons_w: f32,
    /// 狭いのでボタンをアイコンだけへ縮退させるか
    pub icon_only: bool,
}

impl KeybindColumns {
    /// 左端からの [開始, 終了] を列順に返す (重なり検査用)。
    pub fn spans(&self) -> Vec<(f32, f32)> {
        let mut out = Vec::with_capacity(4);
        let mut x = 0.0f32;
        for w in [self.label_w, self.keys_w, self.note_w, self.buttons_w] {
            if w > 0.0 {
                out.push((x, x + w));
                x += w + KEYBIND_COL_GAP;
            }
        }
        out
    }

    pub fn total_w(&self) -> f32 {
        self.spans().last().map(|(_, e)| *e).unwrap_or(0.0)
    }
}

/// 列と列のあいだ。
pub const KEYBIND_COL_GAP: f32 = 8.0;
/// ボタン列 (「記録」「既定へ戻す」) の幅。
const KEYBIND_BUTTONS_W: f32 = 116.0;
/// アイコンだけへ縮退したときのボタン列の幅。
const KEYBIND_BUTTONS_ICON_W: f32 = 52.0;
/// 打鍵表記の幅。chord ("⌘K ⌘S") が入る幅。
const KEYBIND_KEYS_W: f32 = 104.0;
/// これより狭ければボタンをアイコンだけにする。
const KEYBIND_NARROW_W: f32 = 520.0;
/// これより狭ければ注記の列を畳む (ホバーで全文を出す)。
const KEYBIND_NO_NOTE_W: f32 = 420.0;
/// アクション名に最低限残す幅。
const KEYBIND_LABEL_MIN_W: f32 = 72.0;

/// 可用幅から列幅を決める。**戻り値の合計は必ず `avail_w` 以下**。
pub fn keybind_columns(avail_w: f32, has_note: bool) -> KeybindColumns {
    let avail = avail_w.max(0.0);
    let icon_only = avail < KEYBIND_NARROW_W;
    let buttons_w = if icon_only {
        KEYBIND_BUTTONS_ICON_W
    } else {
        KEYBIND_BUTTONS_W
    };
    let show_note = has_note && avail >= KEYBIND_NO_NOTE_W;
    let gaps = KEYBIND_COL_GAP * if show_note { 3.0 } else { 2.0 };

    // 固定幅の列から先に取り、残りをアクション名へ回す。
    // 残りが最低幅を割るときは、固定幅の方を縮めて可用幅へ収める。
    let fixed = buttons_w + KEYBIND_KEYS_W + gaps;
    let note_w = if show_note {
        ((avail - fixed - KEYBIND_LABEL_MIN_W) * 0.4).clamp(0.0, 180.0)
    } else {
        0.0
    };
    let label_w = avail - fixed - note_w;
    if label_w >= KEYBIND_LABEL_MIN_W {
        return KeybindColumns {
            label_w,
            keys_w: KEYBIND_KEYS_W,
            note_w,
            buttons_w,
            icon_only,
        };
    }
    // 極端に狭い: 名前と打鍵だけを、比率で分ける
    let gaps = KEYBIND_COL_GAP;
    let usable = (avail - gaps).max(0.0);
    let keys_w = (usable * 0.45).min(KEYBIND_KEYS_W);
    KeybindColumns {
        label_w: (usable - keys_w).max(0.0),
        keys_w,
        note_w: 0.0,
        buttons_w: 0.0,
        icon_only: true,
    }
}

fn modifier_from_name(name: &str) -> Option<Modifiers> {
    Some(match name {
        "cmd" => Modifiers::COMMAND,
        "ctrl" => Modifiers::CTRL,
        "shift" => Modifiers::SHIFT,
        "alt" | "option" => Modifiers::ALT,
        _ => return None,
    })
}

fn key_from_name(name: &str) -> Option<Key> {
    use Key::*;
    // 1文字キー: a-z / 0-9 / 記号
    if name.chars().count() == 1 {
        let c = name.chars().next()?;
        return Some(match c {
            'a' => A,
            'b' => B,
            'c' => C,
            'd' => D,
            'e' => E,
            'f' => F,
            'g' => G,
            'h' => H,
            'i' => I,
            'j' => J,
            'k' => K,
            'l' => L,
            'm' => M,
            'n' => N,
            'o' => O,
            'p' => P,
            'q' => Q,
            'r' => R,
            's' => S,
            't' => T,
            'u' => U,
            'v' => V,
            'w' => W,
            'x' => X,
            'y' => Y,
            'z' => Z,
            '0' => Num0,
            '1' => Num1,
            '2' => Num2,
            '3' => Num3,
            '4' => Num4,
            '5' => Num5,
            '6' => Num6,
            '7' => Num7,
            '8' => Num8,
            '9' => Num9,
            '`' => Backtick,
            '/' => Slash,
            ',' => Comma,
            '.' => Period,
            '-' => Minus,
            '=' => Equals,
            '[' => OpenBracket,
            ']' => CloseBracket,
            '\\' => Backslash,
            _ => return None,
        });
    }
    Some(match name {
        "f1" => F1,
        "f2" => F2,
        "f3" => F3,
        "f4" => F4,
        "f5" => F5,
        "f6" => F6,
        "f7" => F7,
        "f8" => F8,
        "f9" => F9,
        "f10" => F10,
        "f11" => F11,
        "f12" => F12,
        // F13 以降は macOS で輝度/音量キーと競合しないので音声入力向き
        "f13" => F13,
        "f14" => F14,
        "f15" => F15,
        "f16" => F16,
        "f17" => F17,
        "f18" => F18,
        "f19" => F19,
        "f20" => F20,
        "up" => ArrowUp,
        "down" => ArrowDown,
        "left" => ArrowLeft,
        "right" => ArrowRight,
        "enter" => Enter,
        "tab" => Tab,
        "escape" | "esc" => Escape,
        "space" => Space,
        "backtick" => Backtick,
        "plus" => Plus,
        "minus" => Minus,
        "equals" | "equal" => Equals,
        "slash" => Slash,
        "comma" => Comma,
        "period" => Period,
        "openbracket" => OpenBracket,
        "closebracket" => CloseBracket,
        "backslash" => Backslash,
        _ => return None,
    })
}

// ─────────────────────────────────────────────────────────────────────────
// egui-winit 0.29 がキーイベントごと飲み込む打鍵の救出
// ─────────────────────────────────────────────────────────────────────────

/// クリップボードイベントにすり替えられてしまう打鍵の種類。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ClipboardAlias {
    Cut,
    Copy,
    Paste,
}

/// この修飾キーの組み合わせを egui-winit が `command` として扱うか。
///
/// egui-winit 0.29 は macOS では ⌘ を、それ以外の OS では Ctrl を
/// `Modifiers::command` に写す。判定はその写像に合わせる。
fn acts_as_command(m: Modifiers) -> bool {
    if cfg!(target_os = "macos") {
        m.command || m.mac_cmd
    } else {
        m.command || m.ctrl
    }
}

/// egui-winit 0.29 が **押した瞬間のキーイベントごと捨ててしまう** 打鍵か。
///
/// `egui-winit-0.29.1/src/lib.rs:758-774` は、押下イベントが
/// `is_cut_command` / `is_copy_command` / `is_paste_command` に当たると
/// `Event::Cut` / `Event::Copy` / `Event::Paste` を積んで **その場で return** する
/// (= `Event::Key { pressed: true }` を一切積まない)。判定は
/// `modifiers.command && key == X|C|V` だけで、**shift / alt の有無を見ていない**
/// (同ファイル 1023-1039 行)。
///
/// そのため ⌘⇧C (Cockpit) や ⌘⇧V (プレビュー) のようなバインドは
/// `InputState::consume_shortcut` では**構造的に絶対発火しない**。
/// リリースイベントは飲まれない (`if pressed` の内側なので) が、
/// 「⌘ を先に離す」と修飾キーが落ちるため押下側で拾い直すのが正しい。
///
/// 素の ⌘C / ⌘X / ⌘V は本物のクリップボード操作なので対象外にする
/// (ここで横取りするとコピー&ペーストが壊れる)。
pub fn clipboard_alias(sc: KeyboardShortcut) -> Option<ClipboardAlias> {
    if !acts_as_command(sc.modifiers) {
        return None;
    }
    if !(sc.modifiers.shift || sc.modifiers.alt) {
        return None;
    }
    match sc.logical_key {
        Key::X => Some(ClipboardAlias::Cut),
        Key::C => Some(ClipboardAlias::Copy),
        Key::V => Some(ClipboardAlias::Paste),
        _ => None,
    }
}

/// [`egui::InputState::consume_shortcut`] の代替。
///
/// 通常の経路で拾えなかったときだけ、[`clipboard_alias`] のすり替えを
/// 逆再生して該当イベントを消費する。**アプリのショートカット消費は
/// 必ずこれを通すこと** — 素の `consume_shortcut` を使うと
/// 「画面には ⌘⇧C と書いてあるのに効かない」が再発する。
pub fn consume_shortcut_compat(i: &mut egui::InputState, sc: KeyboardShortcut) -> bool {
    if i.consume_shortcut(&sc) {
        return true;
    }
    let Some(alias) = clipboard_alias(sc) else {
        return false;
    };
    // `Event::Copy` 等は修飾キーを持たないので、フレームの修飾キー状態で照合する。
    if !i.modifiers.matches_logically(sc.modifiers) {
        return false;
    }
    let mut hit = false;
    i.events.retain(|e| {
        let is_match = matches!(
            (e, alias),
            (egui::Event::Cut, ClipboardAlias::Cut)
                | (egui::Event::Copy, ClipboardAlias::Copy)
                | (egui::Event::Paste(_), ClipboardAlias::Paste)
        );
        hit |= is_match;
        !is_match
    });
    hit
}

// ─────────────── IME 変換中はショートカットを消費しない ───────────────
//
// ターミナル側は `terminal::translate_input` が「未確定文字列 (preedit) が
// 空でない間はキーを IME に任せる」規則を持っている。**エディタ側には
// それが無く**、変換中の生キーが `handle_shortcuts` まで届く環境がある
// (winit は IME 合成中も KeyboardInput を配ることがある)。そのままだと
// 「ひらがなを打っているのに ⌘S 相当のバインドが発火する」「変換確定の
// Enter がコマンドとして食われる」が起きる。
//
// 判定は `terminal::ime_ended_in_frame` と**同じ考え方** — イベントの並びは
// 環境依存 (Windows は Commit の前後に Enabled/Disabled を出し、macOS は
// Disabled を出さない) なので、順序に依存せずフレーム単位で見る。

/// 変換中フラグの置き場所 (egui の一時データ)。アプリの構造体に持たせないのは、
/// 「ショートカットを消費してよいか」の判断材料が消費地点の隣にある方が
/// 落ちにくいため (状態と規則が離れると片方だけ直されて壊れる)。
fn ime_state_id() -> egui::Id {
    egui::Id::new("zv-ime-composing")
}

/// このフレームを処理し終えた時点で「まだ変換中」か。
///
/// * `Preedit(非空)` → 変換中に入る / 続く
/// * `Preedit("")` → 未確定文字列が消えた = 確定 or 取り消しで変換が閉じた
/// * `Commit` / `Enabled` / `Disabled` → 変換は開いていない
fn ime_composing_after(events: &[egui::Event], composing: bool) -> bool {
    let mut composing = composing;
    for ev in events {
        if let egui::Event::Ime(ime) = ev {
            composing = match ime {
                egui::ImeEvent::Preedit(t) => !t.is_empty(),
                egui::ImeEvent::Commit(_) | egui::ImeEvent::Enabled | egui::ImeEvent::Disabled => {
                    false
                }
            };
        }
    }
    composing
}

/// このフレームはショートカットの消費を止めるべきか。
///
/// 止めるのは (1) フレーム開始時点で変換中だった (2) このフレームで未確定
/// 文字列が出た (3) このフレームで確定した、のいずれか。(3) を含めるのは
/// **確定に使った Enter が同じフレームに載る**ため — ハングルは 1 打鍵ごとに
/// 音節が組み上がって確定するので、ここを外すと変換のたびに Enter バインドが
/// 発火する。
fn ime_blocks_shortcuts(events: &[egui::Event], composing_at_start: bool) -> bool {
    if composing_at_start {
        return true;
    }
    events.iter().any(|ev| match ev {
        egui::Event::Ime(egui::ImeEvent::Commit(_)) => true,
        egui::Event::Ime(egui::ImeEvent::Preedit(t)) => !t.is_empty(),
        _ => false,
    })
}

/// IME 変換中か (変換中フラグを次フレームへ持ち越しつつ判定する)。
///
/// **1 フレームに 1 回だけ呼ぶこと** (状態を更新するため)。呼び出し地点は
/// `App::handle_shortcuts` の先頭 1 か所。
/// IME 変換中でショートカットを止めるべきか — **状態を進めない読み取り専用版**。
///
/// [`ime_blocks_shortcuts_now`] は IME の状態機械を 1 フレーム分**進める**ので、
/// 同じフレームで 2 回呼んではいけない (2 回目が変換の開始/終了を食う)。
/// `handle_shortcuts` 以外の場所 — ファイルツリーのキー処理など — から
/// 「いま変換中か」を見たいときは必ずこちらを使う。
pub fn ime_blocks_shortcuts_peek(ctx: &egui::Context) -> bool {
    let was = ctx
        .data(|d| d.get_temp::<bool>(ime_state_id()))
        .unwrap_or(false);
    ctx.input(|i| ime_blocks_shortcuts(&i.events, was))
}

pub fn ime_blocks_shortcuts_now(ctx: &egui::Context) -> bool {
    let was = ctx
        .data(|d| d.get_temp::<bool>(ime_state_id()))
        .unwrap_or(false);
    let (blocked, next) = ctx.input(|i| {
        (
            ime_blocks_shortcuts(&i.events, was),
            ime_composing_after(&i.events, was),
        )
    });
    ctx.data_mut(|d| d.insert_temp(ime_state_id(), next));
    blocked
}

fn hint_id(a: BindAction) -> egui::Id {
    egui::Id::new(("zv-key-hint", format!("{a:?}")))
}

/// `Keybinds` を持てない描画関数へ打鍵表記を配る。
///
/// ターミナルの右クリックメニューのように、キーバインド表を引数で持ち回すと
/// 呼び出し側を全部書き換える羽目になる場所がある。アプリが毎フレームここで
/// 配り、描画側は [`key_hint`] で読む。
pub fn publish_key_hints(ctx: &egui::Context, keys: &Keybinds, actions: &[BindAction]) {
    ctx.data_mut(|d| {
        for a in actions {
            d.insert_temp(hint_id(*a), keys.label(*a));
        }
    });
}

/// [`publish_key_hints`] が配った打鍵表記を読む。
/// 配られていなければ既定の打鍵へ落ちるので、表示が空になることはない。
pub fn key_hint(ctx: &egui::Context, a: BindAction) -> String {
    ctx.data(|d| d.get_temp::<String>(hint_id(a)))
        .unwrap_or_else(|| format_binding(default_binding(a)))
}

/// macOS が OS 側で握っていて、アプリまで届かない打鍵の表。
///
/// 出典は推測ではなく実測 —
/// `~/Library/Preferences/com.apple.symbolichotkeys.plist` の
/// `AppleSymbolicHotKeys` で `enabled = true` になっている項目を読み出して作った
/// (id 60 = ⌃Space「前の入力ソース」、id 52 = ⌥⌘D「Dock を隠す」、
/// id 64 = ⌘Space「Spotlight」、id 27 = ⌘\`「次のウィンドウ」など)。
/// ここに載る打鍵を既定バインドにすると、UI には出るのに**絶対に効かない**。
pub const MACOS_RESERVED: &[(Modifiers, Key, &str)] = &[
    (Modifiers::COMMAND, Key::Space, "Spotlight (id 64)"),
    (
        Modifiers::COMMAND.plus(Modifiers::ALT),
        Key::Space,
        "Finder の検索ウィンドウ (id 65)",
    ),
    (Modifiers::CTRL, Key::Space, "前の入力ソース (id 60)"),
    (
        Modifiers::CTRL.plus(Modifiers::ALT),
        Key::Space,
        "次の入力ソース (id 61)",
    ),
    (
        Modifiers::CTRL.plus(Modifiers::SHIFT),
        Key::Space,
        "入力メニュー (id 156)",
    ),
    (
        Modifiers::COMMAND.plus(Modifiers::ALT),
        Key::D,
        "Dock を隠す/表示 (id 52)",
    ),
    (
        Modifiers::COMMAND.plus(Modifiers::CTRL),
        Key::D,
        "辞書で調べる (id 70)",
    ),
    (
        Modifiers::COMMAND.plus(Modifiers::ALT),
        Key::Escape,
        "強制終了ダイアログ",
    ),
    (Modifiers::CTRL, Key::ArrowUp, "Mission Control (id 32)"),
    (
        Modifiers::CTRL,
        Key::ArrowDown,
        "アプリケーションウィンドウ (id 33)",
    ),
    (
        Modifiers::CTRL.plus(Modifiers::SHIFT),
        Key::ArrowUp,
        "Mission Control (逆) (id 34)",
    ),
    (
        Modifiers::CTRL.plus(Modifiers::SHIFT),
        Key::ArrowDown,
        "アプリケーションウィンドウ (逆) (id 35)",
    ),
    (Modifiers::CTRL, Key::ArrowLeft, "左のスペースへ (id 79)"),
    (Modifiers::CTRL, Key::ArrowRight, "右のスペースへ (id 81)"),
    (Modifiers::COMMAND, Key::Tab, "アプリケーション切替"),
    (
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        Key::Tab,
        "アプリケーション切替 (逆)",
    ),
    (Modifiers::COMMAND, Key::Backtick, "次のウィンドウ (id 27)"),
    (
        Modifiers::COMMAND.plus(Modifiers::ALT),
        Key::Backtick,
        "前のウィンドウ (id 51)",
    ),
    (
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        Key::Num3,
        "スクリーンショット (id 28)",
    ),
    (
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        Key::Num4,
        "選択部分を撮影 (id 30)",
    ),
    (
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        Key::Num5,
        "スクリーンショットと画面収録",
    ),
    (
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        Key::Slash,
        "ヘルプメニューの検索 (id 98)",
    ),
    (Modifiers::NONE, Key::F11, "デスクトップを表示 (id 36)"),
    (Modifiers::COMMAND, Key::H, "アプリケーションを隠す"),
    (Modifiers::COMMAND, Key::M, "ウィンドウをしまう"),
    (Modifiers::COMMAND, Key::Q, "アプリケーションを終了"),
    (
        Modifiers::COMMAND.plus(Modifiers::SHIFT),
        Key::Q,
        "ログアウト",
    ),
];

/// ショートカットをメニュー表示用の文字列にする。
/// macOS は VS Code と同じ記号表記 (⌃⌥⇧⌘ + キー)、他 OS は "Ctrl+Shift+P" 形式。
pub fn format_shortcut(sc: KeyboardShortcut) -> String {
    let mac = cfg!(target_os = "macos");
    let key = key_label(sc.logical_key);
    if mac {
        let mut s = String::new();
        if sc.modifiers.ctrl {
            s.push('⌃');
        }
        if sc.modifiers.alt {
            s.push('⌥');
        }
        if sc.modifiers.shift {
            s.push('⇧');
        }
        if sc.modifiers.command || sc.modifiers.mac_cmd {
            s.push('⌘');
        }
        s.push_str(&key);
        s
    } else {
        let mut parts: Vec<&str> = Vec::new();
        if sc.modifiers.command || sc.modifiers.ctrl {
            parts.push("Ctrl");
        }
        if sc.modifiers.alt {
            parts.push("Alt");
        }
        if sc.modifiers.shift {
            parts.push("Shift");
        }
        let mut s = parts.join("+");
        if !s.is_empty() {
            s.push('+');
        }
        s.push_str(&key);
        s
    }
}

fn key_label(key: Key) -> String {
    use Key::*;
    match key {
        ArrowUp => "↑".into(),
        ArrowDown => "↓".into(),
        ArrowLeft => "←".into(),
        ArrowRight => "→".into(),
        Enter => "↩".into(),
        Escape => "Esc".into(),
        Backtick => "`".into(),
        Plus => "+".into(),
        Minus => "-".into(),
        Slash => "/".into(),
        Comma => ",".into(),
        Period => ".".into(),
        OpenBracket => "[".into(),
        CloseBracket => "]".into(),
        Backslash => "\\".into(),
        Space => "Space".into(),
        Tab => "Tab".into(),
        _ => {
            // Key::name() は "A" や "F12" を返す
            key.name().to_string()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Key, KeyboardShortcut, Modifiers};

    fn sc(mods: Modifiers, key: Key) -> KeyboardShortcut {
        KeyboardShortcut::new(mods, key)
    }

    #[test]
    fn parse_cmd_shift_p() {
        assert_eq!(
            parse_shortcut("cmd+shift+p"),
            Some(sc(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::P))
        );
    }

    #[test]
    fn parse_ctrl_backtick() {
        assert_eq!(
            parse_shortcut("ctrl+`"),
            Some(sc(Modifiers::CTRL, Key::Backtick))
        );
        assert_eq!(parse_shortcut("ctrl+backtick"), parse_shortcut("ctrl+`"));
        // F13 以降 (macOS で輝度/音量キーと競合しない) もバインドできる
        assert_eq!(parse_shortcut("f13"), Some(sc(Modifiers::NONE, Key::F13)));
        assert_eq!(
            parse_shortcut("cmd+f20"),
            Some(sc(Modifiers::COMMAND, Key::F20))
        );
        assert_eq!(parse_shortcut("f21"), None);
    }

    #[test]
    fn parse_alt_up() {
        assert_eq!(
            parse_shortcut("alt+up"),
            Some(sc(Modifiers::ALT, Key::ArrowUp))
        );
        // option は alt の別名
        assert_eq!(
            parse_shortcut("option+down"),
            Some(sc(Modifiers::ALT, Key::ArrowDown))
        );
    }

    #[test]
    fn parse_cmd_slash() {
        assert_eq!(
            parse_shortcut("cmd+/"),
            Some(sc(Modifiers::COMMAND, Key::Slash))
        );
        assert_eq!(parse_shortcut("cmd+slash"), parse_shortcut("cmd+/"));
    }

    #[test]
    fn parse_invalid_returns_none() {
        assert_eq!(parse_shortcut(""), None);
        assert_eq!(parse_shortcut("cmd+"), None);
        assert_eq!(parse_shortcut("cmd"), None); // 修飾キーのみ
        assert_eq!(parse_shortcut("foo+p"), None); // 不明な修飾キー
        assert_eq!(parse_shortcut("cmd+unknownkey"), None); // 不明なキー
    }

    #[test]
    fn parse_mixed_case() {
        assert_eq!(parse_shortcut("CMD+Shift+P"), parse_shortcut("cmd+shift+p"));
        assert_eq!(parse_shortcut(" Ctrl+` "), parse_shortcut("ctrl+`"));
    }

    #[test]
    fn parse_f5() {
        assert_eq!(parse_shortcut("f5"), Some(sc(Modifiers::NONE, Key::F5)));
        assert_eq!(
            parse_shortcut("ctrl+f5"),
            Some(sc(Modifiers::CTRL, Key::F5))
        );
    }

    #[test]
    fn parse_space() {
        assert_eq!(
            parse_shortcut("space"),
            Some(sc(Modifiers::NONE, Key::Space))
        );
        assert_eq!(
            parse_shortcut("cmd+space"),
            Some(sc(Modifiers::COMMAND, Key::Space))
        );
    }

    #[test]
    fn parse_plus_minus() {
        assert_eq!(
            parse_shortcut("cmd+plus"),
            Some(sc(Modifiers::COMMAND, Key::Plus))
        );
        assert_eq!(
            parse_shortcut("cmd+minus"),
            Some(sc(Modifiers::COMMAND, Key::Minus))
        );
    }

    #[test]
    fn from_overrides_applies_valid_and_ignores_invalid() {
        let mut ov = HashMap::new();
        ov.insert("save".to_string(), "ctrl+shift+s".to_string());
        ov.insert("bogus_action".to_string(), "cmd+s".to_string()); // 不明action → 無視
        ov.insert("find".to_string(), "not+a+key".to_string()); // 不正文字列 → デフォルト維持
        let kb = Keybinds::from_overrides(&ov);
        assert_eq!(
            kb.get(BindAction::Save),
            sc(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::S)
        );
        assert_eq!(kb.get(BindAction::Find), sc(Modifiers::COMMAND, Key::F));
        assert_eq!(kb.get(BindAction::NewFile), sc(Modifiers::COMMAND, Key::N));
        assert_eq!(
            kb.get(BindAction::MoveLineUp),
            sc(Modifiers::ALT, Key::ArrowUp)
        );
    }

    // ---- 整形方向 (format_shortcut) ----

    #[test]
    fn format_cmd_s() {
        let f = format_shortcut(sc(Modifiers::COMMAND, Key::S));
        if cfg!(target_os = "macos") {
            assert_eq!(f, "⌘S");
        } else {
            assert_eq!(f, "Ctrl+S");
        }
    }

    #[test]
    fn format_modifier_order_is_ctrl_alt_shift_cmd() {
        let all = Modifiers::CTRL
            .plus(Modifiers::ALT)
            .plus(Modifiers::SHIFT)
            .plus(Modifiers::COMMAND);
        let f = format_shortcut(sc(all, Key::S));
        if cfg!(target_os = "macos") {
            assert_eq!(f, "⌃⌥⇧⌘S");
        } else {
            // command と ctrl はどちらも "Ctrl" に集約され、重複しない
            assert_eq!(f, "Ctrl+Alt+Shift+S");
        }
    }

    #[test]
    fn format_cmd_shift_p() {
        let f = format_shortcut(sc(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::P));
        if cfg!(target_os = "macos") {
            assert_eq!(f, "⇧⌘P");
        } else {
            assert_eq!(f, "Ctrl+Shift+P");
        }
    }

    #[test]
    fn format_alt_arrow() {
        let f = format_shortcut(sc(Modifiers::ALT, Key::ArrowUp));
        if cfg!(target_os = "macos") {
            assert_eq!(f, "⌥↑");
        } else {
            assert_eq!(f, "Alt+↑");
        }
    }

    #[test]
    fn format_unmodified_key_is_label_only() {
        // 修飾キーなし → キーラベルのみ (両OS共通で区切り記号なし)
        assert_eq!(format_shortcut(sc(Modifiers::NONE, Key::F12)), "F12");
    }

    // ---- キーラベル (key_label) ----

    #[test]
    fn key_label_letter_digit_fkey() {
        // フォールバックの Key::name() 経由 (egui 0.29: "A" / "7" / "F12")
        assert_eq!(key_label(Key::A), "A");
        assert_eq!(key_label(Key::Num7), "7");
        assert_eq!(key_label(Key::F12), "F12");
    }

    #[test]
    fn key_label_arrows_and_special() {
        assert_eq!(key_label(Key::ArrowUp), "↑");
        assert_eq!(key_label(Key::ArrowDown), "↓");
        assert_eq!(key_label(Key::ArrowLeft), "←");
        assert_eq!(key_label(Key::ArrowRight), "→");
        assert_eq!(key_label(Key::Enter), "↩");
        assert_eq!(key_label(Key::Escape), "Esc");
        assert_eq!(key_label(Key::Space), "Space");
        assert_eq!(key_label(Key::Tab), "Tab");
        assert_eq!(key_label(Key::Backtick), "`");
        assert_eq!(key_label(Key::Backslash), "\\");
    }

    // ---- action_from_name ----

    #[test]
    fn action_from_name_known() {
        assert_eq!(Keybinds::action_from_name("save"), Some(BindAction::Save));
        assert_eq!(
            Keybinds::action_from_name("palette_commands"),
            Some(BindAction::PaletteCommands)
        );
        assert_eq!(
            Keybinds::action_from_name("move_line_down"),
            Some(BindAction::MoveLineDown)
        );
        assert_eq!(
            Keybinds::action_from_name("toggle_fullscreen"),
            Some(BindAction::ToggleFullScreen)
        );
        assert_eq!(
            Keybinds::action_from_name("nav_back"),
            Some(BindAction::NavBack)
        );
    }

    #[test]
    fn action_from_name_unknown_or_wrong_case() {
        assert_eq!(Keybinds::action_from_name(""), None);
        assert_eq!(Keybinds::action_from_name("bogus"), None);
        // 大文字小文字は区別する (config 名は小文字固定)
        assert_eq!(Keybinds::action_from_name("Save"), None);
        // 前後空白もトリムしない
        assert_eq!(Keybinds::action_from_name(" save"), None);
    }

    // ---- ラウンドトリップ (parse → format → parse) ----

    #[test]
    fn roundtrip_known_specs_parse_format_parse() {
        // デフォルト群を網羅する既知表記の集合
        let specs = [
            "cmd+s",
            "cmd+shift+s",
            "cmd+w",
            "cmd+p",
            "cmd+shift+p",
            "cmd+j",
            "cmd+/",
            "cmd+plus",
            "cmd+minus",
            "alt+up",
            "alt+down",
            "ctrl+g",
            "ctrl+-",
            "ctrl+shift+-",
            "ctrl+shift+`",
            "cmd+alt+s",
            "cmd+alt+f",
            "cmd+shift+]",
            "cmd+shift+[",
            "cmd+shift+\\",
            "ctrl+cmd+f",
            "f12",
        ];
        for s in specs {
            let sc1 = parse_shortcut(s).unwrap_or_else(|| panic!("parse failed: {s}"));
            // parse は安定 (同じ入力から同じ結果)
            assert_eq!(parse_shortcut(s), Some(sc1));
            let f1 = format_shortcut(sc1);
            // format 出力は必ずキーラベルで終わる (両OS共通)
            assert!(f1.ends_with(&key_label(sc1.logical_key)), "{s} -> {f1}");
            // format が正規化した表記が再パース可能なら、再 parse → 再 format で固定点になる
            // (macOS の記号表記や "↑" などパース不能な表記はここでは対象外)
            if let Some(sc2) = parse_shortcut(&f1) {
                let f2 = format_shortcut(sc2);
                assert_eq!(f2, f1, "format unstable for {s}");
                assert_eq!(parse_shortcut(&f2), Some(sc2), "reparse unstable for {s}");
            }
        }
    }

    #[test]
    fn roundtrip_alias_specs_agree() {
        // 別名表記は同じショートカットにパースされ、同じ表示に整形される
        for (a, b) in [
            ("cmd+/", "cmd+slash"),
            ("ctrl+`", "ctrl+backtick"),
            ("alt+up", "option+up"),
            ("cmd+-", "cmd+minus"),
            ("cmd+shift+[", "cmd+shift+openbracket"),
            ("escape", "esc"),
        ] {
            let sa = parse_shortcut(a).unwrap_or_else(|| panic!("parse failed: {a}"));
            let sb = parse_shortcut(b).unwrap_or_else(|| panic!("parse failed: {b}"));
            assert_eq!(sa, sb, "{a} vs {b}");
            assert_eq!(format_shortcut(sa), format_shortcut(sb), "{a} vs {b}");
        }
    }

    /// 既定のショートカットは全アクションで重複しない。
    /// 「空いている打鍵に割り当てた」という主張をここで固定する
    /// (新しいバインドを足したとき、既存を黙って奪っていないか検出する)。
    #[test]
    fn default_shortcuts_are_unique() {
        let mut seen: HashMap<String, BindAction> = HashMap::new();
        for a in ALL_ACTIONS {
            let sc = default_shortcut(a);
            let key = format!("{:?}+{:?}", sc.modifiers, sc.logical_key);
            if let Some(prev) = seen.insert(key.clone(), a) {
                panic!("ショートカット衝突: {key} を {prev:?} と {a:?} が共有している");
            }
        }
        assert_eq!(seen.len(), ALL_ACTIONS.len());
    }

    /// `action_from_name` は全アクションを名前で引ける (config からの上書き経路)。
    ///
    /// 名前表は [`config_name`] の **網羅 match** が持つ。変種を足した瞬間に
    /// そちらがコンパイルエラーになるので、名前の付け忘れは起こらない。
    #[test]
    fn every_action_has_a_config_name() {
        let mut seen: HashMap<&'static str, BindAction> = HashMap::new();
        for a in ALL_ACTIONS {
            let n = config_name(a);
            assert!(!n.is_empty(), "{a:?} の config 名が空");
            assert_eq!(
                Keybinds::action_from_name(n),
                Some(a),
                "{a:?} の config 名 {n} が引けない"
            );
            if let Some(prev) = seen.insert(n, a) {
                panic!("config 名の重複: {n} を {prev:?} と {a:?} が共有している");
            }
        }
        assert_eq!(seen.len(), ALL_ACTIONS.len());
    }

    /// 画面に出すアクション名が全部埋まっていて、重複していない。
    #[test]
    fn every_action_has_a_unique_label() {
        let mut seen: HashMap<&'static str, BindAction> = HashMap::new();
        for a in ALL_ACTIONS {
            let l = action_label(a);
            assert!(!l.is_empty(), "{a:?} の表示名が空");
            if let Some(prev) = seen.insert(l, a) {
                panic!("表示名の重複: {l} を {prev:?} と {a:?} が共有している");
            }
        }
    }

    /// **`ALL_ACTIONS` の要素数と `BindAction` の変種数が一致する。**
    ///
    /// 「変種は足したが `ALL_ACTIONS` に入れ忘れた」= 既定表にも編集 UI にも
    /// 現れないアクションが生まれる、という取りこぼしの検出器。
    /// ソースの enum 本体を数えるので、今後どれだけ増えても効き続ける。
    #[test]
    fn all_actionsの数がbindactionの変種数と一致する() {
        let src = src_of(include_str!("keybinds.rs"));
        let body = src
            .split("pub enum BindAction {")
            .nth(1)
            .expect("BindAction の定義がある");
        let body = body.split("\n}\n").next().expect("enum の終わり");
        let variants: Vec<&str> = body
            .lines()
            .map(str::trim)
            .filter(|l| {
                // ドキュメントコメント・属性・空行を落とし、`Name,` だけを数える
                !l.is_empty()
                    && !l.starts_with("//")
                    && !l.starts_with('#')
                    && l.ends_with(',')
                    && l[..l.len() - 1]
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
            })
            .collect();
        assert_eq!(
            variants.len(),
            ALL_ACTIONS.len(),
            "BindAction の変種 {:?} と ALL_ACTIONS の数が合わない",
            variants
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // 「画面に書いてあることが実行できる」ことの番人
    // ─────────────────────────────────────────────────────────────────

    /// 改行を正規化したソース (Windows の CRLF チェックアウト対策)。
    fn src_of(raw: &str) -> String {
        raw.replace("\r\n", "\n")
    }

    /// `at` の手前 `n` バイトを、UTF-8 の境界を壊さずに取り出す。
    /// (日本語コメントが混ざるので、素の添字スライスは panic する)
    fn window_before(src: &str, at: usize, n: usize) -> String {
        let start = at.saturating_sub(n);
        String::from_utf8_lossy(&src.as_bytes()[start..at]).into_owned()
    }

    /// **全 `BindAction` が実際に消費されている**ことをソースで固定する。
    ///
    /// 「バインドは足したが `handle_shortcuts` に繋いでいない」= 画面には
    /// ショートカットが出るのに押しても何も起きない、という事故の検出器。
    /// 1 つでも欠けたら落ちる。
    #[test]
    fn 全アクションが消費地点に繋がっている() {
        let src = src_of(include_str!("app.rs"));
        let mut missing: Vec<String> = Vec::new();
        for a in ALL_ACTIONS {
            // 使用箇所は必ず `self.keys.get(BindAction::X)` の形なので、
            // 閉じ括弧まで含めて照合する (Save と SaveAs の取り違え防止)。
            let needle = format!("BindAction::{a:?})");
            let mut found = false;
            let mut from = 0usize;
            while let Some(rel) = src[from..].find(&needle) {
                let at = from + rel;
                if window_before(&src, at, 96).contains("consume") {
                    found = true;
                    break;
                }
                from = at + needle.len();
            }
            if !found {
                missing.push(format!("{a:?}"));
            }
        }
        assert!(
            missing.is_empty(),
            "ショートカットを消費していない BindAction がある \
             (画面に出ても押せない): {missing:?}"
        );
    }

    /// ショートカットの消費は **必ず** [`consume_shortcut_compat`] を通す。
    ///
    /// 素の `InputState::consume_shortcut` へ戻すと ⌘⇧C / ⌘⇧V が
    /// 二度と発火しなくなる (egui-winit がキーイベントごと捨てるため)。
    #[test]
    fn ショートカット消費は互換経路を通っている() {
        let src = src_of(include_str!("app.rs"));
        let body = src
            .split("fn handle_shortcuts(&mut self, ctx: &egui::Context) {")
            .nth(1)
            .expect("handle_shortcuts がある");
        // 素の添字スライスは日本語コメントの途中で切れると panic するので、
        // **文字境界で** 切る (`window_before` と同じ理由)。
        let head: String = body.chars().take(600).collect();
        assert!(
            head.contains("crate::keybinds::consume_shortcut_compat"),
            "handle_shortcuts の consume が互換経路を通っていない"
        );
        // 素の consume_shortcut を app.rs 側で直接呼んでいないこと
        // (compat 側の呼び出しは keybinds.rs にだけある)
        assert!(
            !src.contains(".consume_shortcut(&"),
            "app.rs が素の InputState::consume_shortcut を直接呼んでいる"
        );
    }

    /// **chord の 2 打鍵目も互換経路を通る**ことをソースで固定する。
    ///
    /// VS Code の ⌘K ⌘C (行をコメント) のように 2 打鍵目が X / C / V に
    /// なる組み合わせは egui-winit が押下イベントごと捨てるので、
    /// [`consume_shortcut_compat`] を通さないと**構造的に絶対発火しない**。
    #[test]
    fn chordの両打鍵が互換経路を通っている() {
        let src = src_of(include_str!("keybinds.rs"));
        let body = src
            .split("pub fn consume_binding(")
            .nth(1)
            .expect("consume_binding がある");
        let body = body.split("\n}\n").next().expect("関数の終わり");
        // 1 打鍵目 (prefix) / 2 打鍵目 / 単打 の 3 か所すべて
        assert_eq!(
            body.matches("consume_shortcut_compat(i,").count(),
            3,
            "consume_binding が互換経路を通していない打鍵がある:\n{body}"
        );
        assert!(
            !body.contains("i.consume_shortcut(&"),
            "consume_binding が素の consume_shortcut を呼んでいる"
        );
    }

    /// **記録中は通常のショートカット消費より先に打鍵を取る。**
    ///
    /// 順番が逆だと、記録しようとした ⌘S でファイルが保存される
    /// (VS Code の "Record Keys" が守っているのと同じ順序)。
    #[test]
    fn 記録の取り込みは通常の消費より先に来る() {
        let src = src_of(include_str!("app.rs"));
        let body = src
            .split("fn handle_shortcuts(&mut self, ctx: &egui::Context) {")
            .nth(1)
            .expect("handle_shortcuts がある");
        let rec = body
            .find("self.keybind_record_tick(ctx)")
            .expect("記録の取り込みを呼んでいない");
        let first_consume = body
            .find("if consume(ctx, self.keys.binding(")
            .expect("消費地点がある");
        assert!(
            rec < first_consume,
            "記録の取り込みが通常の消費より後ろにある (記録中の ⌘S で保存されてしまう)"
        );
        // 取り込んだフレームはそこで戻る (二重に消費しない)
        let tail = &body[rec..first_consume];
        assert!(
            tail.contains("return;"),
            "記録中に handle_shortcuts が戻っていない"
        );
    }

    /// 記録モードでも、飲み込まれる打鍵を拾い直せている。
    #[test]
    fn 記録は飲み込まれた打鍵も拾える() {
        let src = src_of(include_str!("keybinds.rs"));
        let body = src
            .split("pub fn record_stroke(")
            .nth(1)
            .expect("record_stroke がある");
        let body = body.split("\n}\n").next().expect("関数の終わり");
        for want in ["Event::Cut", "Event::Copy", "Event::Paste"] {
            assert!(
                body.contains(want),
                "record_stroke が {want} を拾っていない (⌘⇧C が記録できない)"
            );
        }
    }

    /// 既定バインドが macOS の OS 予約と衝突していない。
    #[test]
    fn 既定バインドはシステム予約と衝突しない() {
        let mut clashes: Vec<String> = Vec::new();
        for a in ALL_ACTIONS {
            let sc = default_shortcut(a);
            for (mods, key, why) in MACOS_RESERVED {
                if sc.logical_key == *key && sc.modifiers == *mods {
                    clashes.push(format!("{a:?} = {} は {why}", format_shortcut(sc)));
                }
            }
        }
        assert!(
            clashes.is_empty(),
            "macOS が握っていてアプリに届かない打鍵を既定にしている: {clashes:?}"
        );
    }

    /// 予約表そのものの健全性 (空でない・重複していない)。
    #[test]
    fn システム予約表に重複が無い() {
        assert!(!MACOS_RESERVED.is_empty());
        let mut seen: Vec<String> = Vec::new();
        for (m, k, _) in MACOS_RESERVED {
            let id = format!("{m:?}+{k:?}");
            assert!(!seen.contains(&id), "予約表が重複している: {id}");
            seen.push(id);
        }
    }

    /// egui-winit に飲み込まれる既定バインドが、互換経路で拾えている。
    ///
    /// これが落ちるときは「画面には出るのに押しても効かない」状態。
    #[test]
    fn クリップボードに化ける打鍵を拾い直せる() {
        let mut checked = 0;
        for a in ALL_ACTIONS {
            let sc = default_shortcut(a);
            let Some(alias) = clipboard_alias(sc) else {
                continue;
            };
            checked += 1;
            // egui-winit が実際に積むイベント (キーイベントは積まれない)
            let swallowed = match alias {
                ClipboardAlias::Cut => egui::Event::Cut,
                ClipboardAlias::Copy => egui::Event::Copy,
                ClipboardAlias::Paste => egui::Event::Paste("x".into()),
            };
            let raw = egui::RawInput {
                modifiers: sc.modifiers,
                events: vec![swallowed],
                ..Default::default()
            };
            let ctx = egui::Context::default();
            let mut plain = false;
            let mut compat = false;
            let _ = ctx.run(raw, |ctx| {
                plain = ctx.input_mut(|i| i.consume_shortcut(&sc));
                compat = ctx.input_mut(|i| consume_shortcut_compat(i, sc));
            });
            assert!(
                !plain,
                "{a:?}: 素の consume_shortcut が拾えたなら前提が変わった"
            );
            assert!(compat, "{a:?} = {} を拾えていない", format_shortcut(sc));
        }
        assert!(
            checked >= 2,
            "⌘⇧C / ⌘⇧V が対象から外れている ({checked} 件)"
        );
    }

    /// 素の ⌘C / ⌘V は横取りしない (コピー&ペーストを壊さない)。
    #[test]
    fn 素のクリップボード打鍵は横取りしない() {
        for (key, ev) in [
            (Key::C, egui::Event::Copy),
            (Key::V, egui::Event::Paste("x".into())),
            (Key::X, egui::Event::Cut),
        ] {
            let sc = KeyboardShortcut::new(Modifiers::COMMAND, key);
            assert_eq!(clipboard_alias(sc), None, "{key:?}: 素の ⌘{key:?} は対象外");
            let ctx = egui::Context::default();
            let mut left = 0;
            let _ = ctx.run(
                egui::RawInput {
                    modifiers: Modifiers::COMMAND,
                    events: vec![ev],
                    ..Default::default()
                },
                |ctx| {
                    ctx.input_mut(|i| {
                        let _ = consume_shortcut_compat(i, sc);
                        left = i.events.len();
                    });
                },
            );
            assert_eq!(left, 1, "{key:?}: クリップボードイベントを食べてしまった");
        }
    }

    /// ⌘⇧C は「キーイベントとして届けば」ちゃんと発火する。
    ///
    /// = アプリ側の配線 (handle_shortcuts → Cmd::ToggleCockpit) は正しく、
    ///   効かない原因はイベントが届かないことだけ、という切り分けの固定。
    #[test]
    fn キーイベントとして届けば発火する() {
        let sc = default_shortcut(BindAction::ToggleCockpit);
        let ctx = egui::Context::default();
        let mut fired = false;
        let _ = ctx.run(
            egui::RawInput {
                modifiers: sc.modifiers,
                events: vec![egui::Event::Key {
                    key: Key::C,
                    physical_key: None,
                    pressed: true,
                    repeat: false,
                    modifiers: sc.modifiers,
                }],
                ..Default::default()
            },
            |ctx| {
                fired = ctx.input_mut(|i| consume_shortcut_compat(i, sc));
            },
        );
        assert!(fired, "キーイベントが届けば ⌘⇧C は消費できる");
    }

    // ──────────────── IME 変換中のショートカット保護 ────────────────

    fn preedit(t: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Preedit(t.to_string()))
    }
    fn commit(t: &str) -> egui::Event {
        egui::Event::Ime(egui::ImeEvent::Commit(t.to_string()))
    }
    fn enter() -> egui::Event {
        egui::Event::Key {
            key: Key::Enter,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::NONE,
        }
    }

    /// 日本語入力: `にほんご` を打って変換 → 確定するまで一度も消費しない。
    ///
    /// 変換中に生の `Text` / `Key` が漏れる環境があり、そのまま
    /// `handle_shortcuts` へ流すと「かなを打っているだけでコマンドが走る」。
    #[test]
    fn 日本語の変換中はショートカットを消費しない() {
        // (このフレームのイベント, 開始時の変換中フラグ → 期待: 止める?, 次フレームの変換中?)
        let steps: &[(Vec<egui::Event>, bool, bool, &str)] = &[
            (vec![], false, false, "何も起きていないフレームは素通し"),
            (
                vec![preedit("に")],
                true,
                true,
                "未確定が出た瞬間から止める",
            ),
            (
                vec![preedit("にほん"), enter()],
                true,
                true,
                "変換中の Enter (候補確定) はアプリへ渡さない",
            ),
            (
                vec![commit("日本語"), enter()],
                true,
                false,
                "確定フレームの Enter も渡さない (確定は IME への操作)",
            ),
            (
                vec![enter()],
                false,
                false,
                "確定の次フレームの Enter は通常どおり",
            ),
        ];
        let mut composing = false;
        for (events, want_block, want_after, what) in steps {
            assert_eq!(
                ime_blocks_shortcuts(events, composing),
                *want_block,
                "{what}"
            );
            composing = ime_composing_after(events, composing);
            assert_eq!(composing, *want_after, "{what}: 変換中フラグ");
        }
    }

    /// ハングル: 1 打鍵ごとに音節が組み上がって確定する (ㅎ → 하 → 한)。
    ///
    /// 分解字母のあいだも「変換中」であり続けること。ここを取りこぼすと
    /// 韓国語入力のあいだ中ショートカットが暴発する。
    #[test]
    fn ハングルの分解字母の途中でもショートカットを消費しない() {
        let mut composing = false;
        // 分解字母 (초성 → +중성 → +종성) が preedit として届く
        for step in ["\u{1112}", "\u{1112}\u{1161}", "\u{1112}\u{1161}\u{11AB}"] {
            let events = vec![preedit(step)];
            assert!(
                ime_blocks_shortcuts(&events, composing),
                "分解字母 {step:?} の途中"
            );
            composing = ime_composing_after(&events, composing);
            assert!(composing, "分解字母 {step:?} のあとも変換中");
        }
        // 確定 (完成形 한) — Windows は Commit の前後に Enabled/Disabled を出す
        let events = vec![
            egui::Event::Ime(egui::ImeEvent::Enabled),
            commit("한"),
            egui::Event::Ime(egui::ImeEvent::Disabled),
        ];
        assert!(
            ime_blocks_shortcuts(&events, composing),
            "確定フレームも止める"
        );
        assert!(
            !ime_composing_after(&events, composing),
            "確定したら変換中は終わる"
        );
    }

    /// 変換の**取り消し** (Escape) も、順序に依存せず変換の終わりとして扱う。
    #[test]
    fn 変換の取り消しでも変換中フラグが残らない() {
        let composing = ime_composing_after(&[preedit("にほん")], false);
        assert!(composing);
        // macOS は Disabled を出さず Preedit("") だけ、Windows は両方出す
        assert!(
            !ime_composing_after(&[preedit("")], composing),
            "macOS の並び"
        );
        assert!(
            !ime_composing_after(
                &[preedit(""), egui::Event::Ime(egui::ImeEvent::Disabled)],
                composing
            ),
            "Windows の並び"
        );
    }

    /// 実際の `egui::Context` を 2 フレーム回して、変換中フラグが
    /// フレームをまたいで持ち越されることを確かめる。
    #[test]
    fn 変換中フラグはフレームをまたいで持ち越される() {
        let ctx = egui::Context::default();
        let run = |events: Vec<egui::Event>| -> bool {
            let mut blocked = false;
            let _ = ctx.run(
                egui::RawInput {
                    events,
                    ..Default::default()
                },
                |ctx| blocked = ime_blocks_shortcuts_now(ctx),
            );
            blocked
        };
        assert!(!run(vec![]), "変換していないフレームは素通し");
        assert!(run(vec![preedit("に")]), "未確定が出たフレーム");
        // イベントの無いフレームでも「変換中」は続く (IME は開いたまま)
        assert!(run(vec![]), "変換中はイベントが無くても止める");
        assert!(run(vec![commit("に")]), "確定フレーム");
        assert!(!run(vec![]), "確定の次フレームから通常どおり");
    }

    /// `handle_shortcuts` の先頭に IME ガードがある (消費の前に必ず通る)。
    ///
    /// ガードを消費地点の**後ろ**へ動かすと、そのフレームのショートカットは
    /// もう食われている。位置が仕様なので構造で固定する。
    #[test]
    fn ショートカット消費の前に必ず変換中ガードを通る() {
        let src = include_str!("app.rs").replace("\r\n", "\n");
        let body = src
            .split("fn handle_shortcuts(&mut self, ctx: &egui::Context)")
            .nth(1)
            .expect("ショートカット処理がある");
        let guard = body
            .find("keybinds::ime_blocks_shortcuts_now(ctx)")
            .expect("IME ガードが handle_shortcuts に無い");
        let first_consume = body.find("if consume(ctx,").expect("消費地点がある");
        assert!(
            guard < first_consume,
            "IME ガードが最初の consume より後ろにある (変換中に食われる)"
        );
    }

    /// UI に出るショートカット文字列をベタ書きしていない。
    ///
    /// ベタ書きは (1) 再割り当てで嘘になり (2) Windows/Linux で表記が違う、
    /// の二重の嘘になる。許す例外は理由付きでここに並べる。
    #[test]
    fn 画面のショートカット表記をベタ書きしていない() {
        // 修飾キーの記号。これが文字列リテラルに出てきたら生成経路に直す。
        const GLYPHS: [char; 4] = ['⌘', '⌥', '⌃', '⇧'];
        // 例外 (どれも「キーバインド表に対応する行動が無い」もの)。
        const ALLOWED: &[&str] = &[
            // アイコン/ラベルとしての記号 (打鍵の案内ではない)
            "\"⌘\"",
            // マウス操作の案内 (BindAction を持たない)
            "Ctrl/⌘+スクロール",
            "⌘スクロール",
            // OS/フレームワークが固定で持つ打鍵 (キーバインド表に無い)。
            // どれも `h(mac, other)` で OS 分岐済み。
            "⌘V",
            "⌘X",
            "⌘C",
            // ツリーのファイル操作の取り消し。BindAction ではなく
            // `FileTree::keys_undo` が直接拾う OS 固定キー (本文の ⌘Z と同じ打鍵)
            "⌘Z",
            // エージェント入力欄の送信キー (`panels::send_hint` が OS で書き分け)
            "⌘+Enter",
            "⌘⌫",
            "⌘↓",
            "⌥⌘C",
            "⇧⌥⌘C",
        ];
        let files: [(&str, &str); 7] = [
            ("app.rs", include_str!("app.rs")),
            ("menu_bar.rs", include_str!("menu_bar.rs")),
            ("palette.rs", include_str!("palette.rs")),
            ("tutorial.rs", include_str!("tutorial.rs")),
            ("panels.rs", include_str!("panels.rs")),
            ("terminal.rs", include_str!("terminal.rs")),
            ("file_tree.rs", include_str!("file_tree.rs")),
        ];
        let mut bad: Vec<String> = Vec::new();
        for (name, raw) in files {
            let src = src_of(raw);
            for line in src.lines() {
                // 期待値として記号を書くテストは対象外
                if line.contains("assert") {
                    continue;
                }
                if !line.contains('"') || !line.chars().any(|c| GLYPHS.contains(&c)) {
                    continue;
                }
                let (a, b) = match (line.find('"'), line.rfind('"')) {
                    (Some(a), Some(b)) if b > a => (a, b),
                    _ => continue,
                };
                let lit = &line[a..=b];
                if !lit.chars().any(|c| GLYPHS.contains(&c)) {
                    continue;
                }
                if ALLOWED.iter().any(|a| lit.contains(a)) {
                    continue;
                }
                bad.push(format!("{name}: {}", line.trim()));
            }
        }
        assert!(
            bad.is_empty(),
            "ショートカット表記がベタ書きされている \
             (fmt_key / format_shortcut から生成すること):\n{}",
            bad.join("\n")
        );
    }

    /// 旧名 (`font_inc` / `font_dec`) を書いた config.toml を壊さない。
    /// ズーム系の改名で既存ユーザーのキーバインドが黙って無効化されると、
    /// 「アップデートしたら自分の設定が効かなくなった」になるため。
    #[test]
    fn legacy_font_action_names_still_resolve() {
        assert_eq!(
            Keybinds::action_from_name("font_inc"),
            Some(BindAction::ZoomIn)
        );
        assert_eq!(
            Keybinds::action_from_name("font_dec"),
            Some(BindAction::ZoomOut)
        );
        // 旧名で上書きすると、新しいアクションの割り当てが変わる
        let mut o = HashMap::new();
        o.insert("font_inc".to_string(), "ctrl+shift+u".to_string());
        let kb = Keybinds::from_overrides(&o);
        assert_eq!(
            kb.get(BindAction::ZoomIn),
            parse_shortcut("ctrl+shift+u").unwrap()
        );
    }

    /// 画面全体とファイル単位のズームは、別々の打鍵で両方到達できる。
    #[test]
    fn zoom_bindings_cover_both_scopes() {
        let kb = Keybinds::default();
        for (a, b) in [
            (BindAction::ZoomIn, BindAction::FileZoomIn),
            (BindAction::ZoomOut, BindAction::FileZoomOut),
            (BindAction::ZoomReset, BindAction::FileZoomReset),
        ] {
            let (sa, sb) = (kb.get(a), kb.get(b));
            assert_eq!(
                sa.logical_key, sb.logical_key,
                "{a:?} と {b:?} でキーが違う"
            );
            assert!(!sa.modifiers.alt, "{a:?} に ⌥ が付いている");
            assert!(sb.modifiers.alt, "{b:?} に ⌥ が付いていない");
            assert_ne!(sa, sb);
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // chord (2 打鍵)
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn chordのパースと往復() {
        let b = parse_binding("cmd+k cmd+s").expect("chord が読めない");
        assert_eq!(
            b,
            Binding::Chord(
                KeyboardShortcut::new(Modifiers::COMMAND, Key::K),
                KeyboardShortcut::new(Modifiers::COMMAND, Key::S),
            )
        );
        // config へ書いて読み直しても同じ
        assert_eq!(binding_spec(b), "cmd+k cmd+s");
        assert_eq!(parse_binding(&binding_spec(b)), Some(b));
        // 単打も同じ経路で読める (既存の config.toml を壊さない)
        let single = parse_binding("cmd+shift+p").expect("単打が読めない");
        assert_eq!(
            single,
            Binding::Single(parse_shortcut("cmd+shift+p").unwrap())
        );
        assert_eq!(binding_spec(single), "shift+cmd+p");
        assert_eq!(parse_binding(&binding_spec(single)), Some(single));
        // 大文字小文字と前後の空白は吸収する
        assert_eq!(parse_binding("  CMD+K   CMD+S  "), Some(b));
    }

    #[test]
    fn 不正なchord文字列でpanicしない() {
        for bad in [
            "",
            "   ",
            "cmd+",
            "+",
            "cmd+k cmd+",
            "cmd+k cmd+s cmd+t", // 3 打鍵は非対応
            "nope nope",
            "cmd+k ",
            " cmd+k",
            "🙂 🙂",
            "cmd+k+cmd+s",
        ] {
            // 期待値は「None か Some のどちら」でもよい。**落ちない**ことが要点。
            let got = parse_binding(bad);
            if let Some(b) = got {
                // 読めたなら必ず往復できる形であること
                assert_eq!(parse_binding(&binding_spec(b)), Some(b), "{bad}");
            }
        }
        // 空白だけ / 空は必ず None
        assert_eq!(parse_binding(""), None);
        assert_eq!(parse_binding("   "), None);
        // 3 打鍵は受け付けない
        assert_eq!(parse_binding("cmd+k cmd+s cmd+t"), None);
    }

    #[test]
    fn 単打とchordが同じ表に共存できる() {
        let mut o = HashMap::new();
        o.insert("save".to_string(), "cmd+s".to_string());
        o.insert("keybind_editor".to_string(), "cmd+k cmd+b".to_string());
        let kb = Keybinds::from_overrides(&o);
        assert_eq!(
            kb.binding(BindAction::Save),
            Binding::Single(parse_shortcut("cmd+s").unwrap())
        );
        assert!(kb.binding(BindAction::KeybindEditor).second().is_some());
        assert_eq!(kb.get(BindAction::KeybindEditor).logical_key, Key::K);
        // 表示は 2 打鍵ぶん出る
        let label = kb.label(BindAction::KeybindEditor);
        assert!(
            label.contains(' '),
            "chord が 1 打鍵ぶんしか出ていない: {label}"
        );
    }

    #[test]
    fn keybind_editorの既定はcmd_k_cmd_s() {
        let kb = Keybinds::default();
        assert_eq!(
            kb.binding(BindAction::KeybindEditor),
            Binding::Chord(
                KeyboardShortcut::new(Modifiers::COMMAND, Key::K),
                KeyboardShortcut::new(Modifiers::COMMAND, Key::S),
            )
        );
    }

    #[test]
    fn プレフィックス待ちはタイムアウトで消える() {
        let mut c = ChordState::default();
        let prefix = KeyboardShortcut::new(Modifiers::COMMAND, Key::K);
        c.arm(prefix, 10.0);
        assert_eq!(c.pending(), Some(prefix));
        // 締切の直前はまだ待っている
        let almost = 10.0 + CHORD_TIMEOUT.as_secs_f64() - 0.01;
        assert_eq!(c.tick(almost, false), ChordTick::Waiting);
        assert!(c.is_waiting());
        assert!(c.remaining(almost) > 0.0);
        // 締切を過ぎたら捨てる
        let after = 10.0 + CHORD_TIMEOUT.as_secs_f64();
        assert_eq!(c.tick(after, false), ChordTick::TimedOut);
        assert!(!c.is_waiting());
        assert_eq!(c.remaining(after), 0.0);
        // 待っていないときは Idle
        assert_eq!(c.tick(after, false), ChordTick::Idle);
    }

    #[test]
    fn プレフィックス待ちはescapeで中断できる() {
        let mut c = ChordState::default();
        c.arm(KeyboardShortcut::new(Modifiers::COMMAND, Key::K), 0.0);
        assert_eq!(c.tick(0.1, true), ChordTick::Cancelled);
        assert!(!c.is_waiting(), "Esc で待機が解けていない");
        // 中断後は Escape を撃っても Idle のまま
        assert_eq!(c.tick(0.2, true), ChordTick::Idle);
    }

    #[test]
    fn ime変換中はchordの待機に入らない() {
        let mut c = ChordState::default();
        c.note_ime(true, false);
        assert!(c.ime_active());
        let mut i = egui::InputState::default();
        i.time = 5.0;
        let b = Binding::Chord(
            KeyboardShortcut::new(Modifiers::COMMAND, Key::K),
            KeyboardShortcut::new(Modifiers::COMMAND, Key::S),
        );
        assert!(!consume_binding(&mut i, b, &mut c));
        assert!(!c.is_waiting(), "IME 変換中に prefix を握ってしまった");
        // 変換が終われば待機に入れる
        c.note_ime(false, true);
        assert!(!c.ime_active());
    }

    #[test]
    fn 待機中は単打が素通りしない() {
        let mut c = ChordState::default();
        c.arm(KeyboardShortcut::new(Modifiers::COMMAND, Key::K), 0.0);
        let mut i = egui::InputState::default();
        // ⌘S の押下イベントを積む
        i.events.push(egui::Event::Key {
            key: Key::S,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        });
        i.modifiers = Modifiers::COMMAND;
        let single = Binding::Single(KeyboardShortcut::new(Modifiers::COMMAND, Key::S));
        assert!(
            !consume_binding(&mut i, single, &mut c),
            "prefix を握っている間に単打が発火した"
        );
        // 同じ prefix を持つ chord なら 2 打鍵目として拾える
        let chord = Binding::Chord(
            KeyboardShortcut::new(Modifiers::COMMAND, Key::K),
            KeyboardShortcut::new(Modifiers::COMMAND, Key::S),
        );
        assert!(consume_binding(&mut i, chord, &mut c), "2 打鍵目が拾えない");
        assert!(!c.is_waiting(), "発火後も待機が残っている");
    }

    #[test]
    fn chordの2打鍵目がcopyにすり替わっても拾える() {
        // ⌘K ⌘⇧C (VS Code の ⌘K ⌘C 相当)。egui-winit が押下イベントを
        // 捨てて Event::Copy に化けさせるので、compat 経路が要る。
        let mut c = ChordState::default();
        let prefix = KeyboardShortcut::new(Modifiers::COMMAND, Key::K);
        c.arm(prefix, 0.0);
        let mut i = egui::InputState::default();
        i.events.push(egui::Event::Copy);
        i.modifiers = Modifiers::COMMAND.plus(Modifiers::SHIFT);
        let chord = Binding::Chord(
            prefix,
            KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::C),
        );
        assert!(
            consume_binding(&mut i, chord, &mut c),
            "飲み込まれた 2 打鍵目が拾えていない"
        );
        assert!(i.events.is_empty(), "拾ったイベントが残っている");
    }

    // ─────────────────────────────────────────────────────────────────
    // 衝突検出
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn 同じ打鍵を2アクションに割り当てると衝突する() {
        let keys = Keybinds::default();
        let save = keys.binding(BindAction::Save);
        let got = conflicts_for(&keys, BindAction::NewFile, save);
        assert!(
            got.contains(&Conflict::Duplicate(BindAction::Save)),
            "重複を検出できていない: {got:?}"
        );
        // 自分自身の割り当てを入れ直しただけなら衝突ではない
        assert!(conflicts_for(&keys, BindAction::Save, save)
            .iter()
            .all(|c| !matches!(c, Conflict::Duplicate(_))));
    }

    #[test]
    fn プレフィックスと単打の衝突を検出する() {
        let keys = Keybinds::default();
        // ⌘K は KeybindEditor (⌘K ⌘S) の 1 打鍵目なので、⌘K 単打は成立しない
        let prefix = keys.binding(BindAction::KeybindEditor).first();
        let got = conflicts_for(&keys, BindAction::NewFile, Binding::Single(prefix));
        assert!(
            got.contains(&Conflict::Prefix(BindAction::KeybindEditor)),
            "prefix と単打の衝突を検出できていない: {got:?}"
        );
        // 逆向き (先に単打があるところへ chord を足す) も検出する
        let mut keys2 = Keybinds::default();
        keys2.set(BindAction::NewFile, Binding::Single(prefix));
        let chord = Binding::Chord(prefix, KeyboardShortcut::new(Modifiers::COMMAND, Key::Y));
        let got = conflicts_for(&keys2, BindAction::ToggleFold, chord);
        assert!(
            got.contains(&Conflict::Prefix(BindAction::NewFile)),
            "chord 側から見た衝突を検出できていない: {got:?}"
        );
        // prefix が同じでも「両方 chord」なら共存できる (⌘K ⌘S と ⌘K ⌘Y)
        let other = Binding::Chord(prefix, KeyboardShortcut::new(Modifiers::COMMAND, Key::Z));
        assert!(
            conflicts_for(&Keybinds::default(), BindAction::ToggleFold, other)
                .iter()
                .all(|c| !matches!(c, Conflict::Prefix(_)))
        );
    }

    #[test]
    fn os予約と一致したら理由が出る() {
        let keys = Keybinds::default();
        // ⌘Q = アプリケーションを終了 (実測の予約表にある)
        let quit = Binding::Single(KeyboardShortcut::new(Modifiers::COMMAND, Key::Q));
        let got = conflicts_for(&keys, BindAction::Save, quit);
        assert!(
            got.iter().any(|c| matches!(c, Conflict::Reserved(_))),
            "OS 予約を検出できていない: {got:?}"
        );
        assert!(macos_reservation(KeyboardShortcut::new(Modifiers::COMMAND, Key::Q)).is_some());
        // chord の 2 打鍵目に予約が来ても拾う
        let chord = Binding::Chord(
            KeyboardShortcut::new(Modifiers::COMMAND, Key::Y),
            KeyboardShortcut::new(Modifiers::COMMAND, Key::Q),
        );
        assert!(conflicts_for(&keys, BindAction::Save, chord)
            .iter()
            .any(|c| matches!(c, Conflict::Reserved(_))));
        // 予約されていない打鍵では出ない
        assert!(macos_reservation(KeyboardShortcut::new(Modifiers::COMMAND, Key::Y)).is_none());
    }

    #[test]
    fn 修飾キーと大文字小文字は正規化してから比べる() {
        // 大文字小文字: パース経路で吸収される
        assert_eq!(parse_binding("CMD+S"), parse_binding("cmd+s"));
        // ⌘ は command と mac_cmd の両方に立ちうる。同じ打鍵として扱う
        let a = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);
        let mut m = Modifiers::NONE;
        m.command = true;
        m.mac_cmd = true;
        let b = KeyboardShortcut::new(m, Key::S);
        assert!(same_stroke(a, b), "⌘ の表現ゆれを畳めていない");
        assert_eq!(canonical_shortcut(a), canonical_shortcut(b));
        // 別のキーは当然別物
        assert!(!same_stroke(
            a,
            KeyboardShortcut::new(Modifiers::COMMAND, Key::T)
        ));
        // 修飾キーの数が違えば別物
        assert!(!same_stroke(
            a,
            KeyboardShortcut::new(Modifiers::COMMAND.plus(Modifiers::SHIFT), Key::S)
        ));
        // 正規形は冪等
        assert_eq!(
            canonical_shortcut(canonical_shortcut(b)),
            canonical_shortcut(b)
        );
    }

    // ─────────────────────────────────────────────────────────────────
    // 再割り当て・既定へ戻す・config への書き戻し
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn 再割り当てと既定へ戻す() {
        let mut kb = Keybinds::default();
        assert!(kb.is_default(BindAction::Save));
        assert!(kb.overrides().is_empty(), "既定だけなら書き戻す行は無い");

        let new = Binding::Chord(
            KeyboardShortcut::new(Modifiers::COMMAND, Key::K),
            KeyboardShortcut::new(Modifiers::COMMAND, Key::W),
        );
        kb.set(BindAction::Save, new);
        assert_eq!(kb.binding(BindAction::Save), new);
        assert!(!kb.is_default(BindAction::Save));
        let ov = kb.overrides();
        assert_eq!(ov.get("save").map(String::as_str), Some("cmd+k cmd+w"));
        assert_eq!(ov.len(), 1, "既定のままの行まで書いている: {ov:?}");

        // 1 行だけ戻す
        kb.reset(BindAction::Save);
        assert!(kb.is_default(BindAction::Save));
        assert!(kb.overrides().is_empty());

        // 全部戻す
        kb.set(BindAction::Save, new);
        kb.set(
            BindAction::NewFile,
            Binding::Single(KeyboardShortcut::new(Modifiers::COMMAND, Key::Y)),
        );
        assert_eq!(kb.overrides().len(), 2);
        kb.reset_all();
        assert!(kb.overrides().is_empty());
        for a in ALL_ACTIONS {
            assert!(kb.is_default(a), "{a:?} が既定へ戻っていない");
        }
    }

    #[test]
    fn overridesは読み直しても同じ割り当てになる() {
        let mut kb = Keybinds::default();
        kb.set(
            BindAction::ToggleFold,
            Binding::Chord(
                KeyboardShortcut::new(Modifiers::COMMAND, Key::K),
                KeyboardShortcut::new(Modifiers::COMMAND, Key::Num0),
            ),
        );
        kb.set(
            BindAction::Find,
            Binding::Single(KeyboardShortcut::new(
                Modifiers::CTRL.plus(Modifiers::ALT),
                Key::F7,
            )),
        );
        let back = Keybinds::from_overrides(&kb.overrides());
        for a in ALL_ACTIONS {
            assert_eq!(back.binding(a), kb.binding(a), "{a:?} が往復していない");
        }
    }

    // ─────────────────────────────────────────────────────────────────
    // 打鍵の記録 (Record Keys)
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn 記録は単打とchordの両方を作れる() {
        let cmd_k = KeyboardShortcut::new(Modifiers::COMMAND, Key::K);
        let cmd_s = KeyboardShortcut::new(Modifiers::COMMAND, Key::S);

        // 1 打鍵だけ押して待つと単打として確定する
        let mut r = Recorder::new(BindAction::Save);
        assert_eq!(r.preview(), None);
        assert_eq!(r.push(cmd_k, 0.0), None, "1 打鍵目で確定してはいけない");
        assert_eq!(r.preview(), Some(Binding::Single(cmd_k)));
        assert_eq!(r.tick(0.1), None, "締切前に確定した");
        assert_eq!(
            r.tick(CHORD_TIMEOUT.as_secs_f64()),
            Some(Binding::Single(cmd_k))
        );

        // 締切内に 2 打鍵目が来れば chord
        let mut r = Recorder::new(BindAction::Save);
        assert_eq!(r.push(cmd_k, 0.0), None);
        assert_eq!(r.push(cmd_s, 0.2), Some(Binding::Chord(cmd_k, cmd_s)));

        // 何も押していなければ時間が経っても確定しない (= 中止できる)
        let mut r = Recorder::new(BindAction::Save);
        assert_eq!(r.tick(999.0), None);
        assert_eq!(r.remaining(999.0), 0.0);
    }

    // ─────────────────────────────────────────────────────────────────
    // 編集 UI のレイアウト (純粋関数)
    // ─────────────────────────────────────────────────────────────────

    #[test]
    fn キーバインド表の列はどの幅でも収まり重ならない() {
        // 極端な幅まで含めて、全ての列が可用領域に収まり重ならないこと
        for w in [
            1200.0f32, 900.0, 700.0, 520.0, 460.0, 400.0, 260.0, 120.0, 0.0,
        ] {
            for has_note in [true, false] {
                let c = keybind_columns(w, has_note);
                let spans = c.spans();
                assert!(!spans.is_empty() || w == 0.0, "幅 {w} で列が 1 つも無い");
                assert!(
                    c.total_w() <= w + 0.01,
                    "幅 {w} (注記 {has_note}) で列がはみ出した: {c:?} -> {}",
                    c.total_w()
                );
                for s in &spans {
                    assert!(s.0 >= -0.01 && s.1 <= w + 0.01, "幅 {w}: {s:?} が範囲外");
                    assert!(s.1 >= s.0, "幅 {w}: 負の幅 {s:?}");
                }
                for pair in spans.windows(2) {
                    assert!(
                        pair[0].1 <= pair[1].0 + 0.01,
                        "幅 {w}: 列が重なっている {pair:?}"
                    );
                }
                // 名前の列は必ず残る (何のキーか分からない表にしない)
                if w > 0.0 {
                    assert!(c.label_w > 0.0, "幅 {w}: アクション名の列が消えた");
                    assert!(c.keys_w > 0.0, "幅 {w}: 打鍵の列が消えた");
                }
            }
        }
    }

    #[test]
    fn 狭いときだけボタンがアイコンへ縮退する() {
        let wide = keybind_columns(1200.0, true);
        assert!(!wide.icon_only, "広いのにアイコンだけになっている");
        assert!(wide.note_w > 0.0, "広いのに注記の列が出ていない");
        let narrow = keybind_columns(400.0, true);
        assert!(narrow.icon_only, "狭いのに縮退していない");
        assert_eq!(narrow.note_w, 0.0, "狭いのに注記の列を確保している");
        // 注記が 1 つも無ければ、広くても列を作らない (空白を作らない)
        assert_eq!(keybind_columns(1200.0, false).note_w, 0.0);
    }

    #[test]
    fn roundtrip_all_default_shortcuts_format_stable() {
        // 全デフォルトについて format が空でなくキーラベルで終わり、
        // 再パース可能な表記なら format の固定点であること
        for a in ALL_ACTIONS {
            let sc1 = default_shortcut(a);
            let f1 = format_shortcut(sc1);
            assert!(!f1.is_empty(), "{a:?}");
            assert!(f1.ends_with(&key_label(sc1.logical_key)), "{a:?} -> {f1}");
            if let Some(sc2) = parse_shortcut(&f1) {
                assert_eq!(format_shortcut(sc2), f1, "{a:?}");
            }
        }
    }
}
