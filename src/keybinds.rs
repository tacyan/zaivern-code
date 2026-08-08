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
}

/// 全アクションの一覧 (デフォルトマップ構築用)。
pub const ALL_ACTIONS: [BindAction; 65] = [
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
    }
}

/// アクション → ショートカットの解決テーブル。
pub struct Keybinds {
    map: HashMap<BindAction, KeyboardShortcut>,
}

impl Keybinds {
    /// デフォルト + config の上書き (action名文字列 → ショートカット文字列) から構築。
    /// 不正な文字列は無視してデフォルト維持。
    pub fn from_overrides(overrides: &HashMap<String, String>) -> Self {
        let mut map = HashMap::with_capacity(ALL_ACTIONS.len());
        for a in ALL_ACTIONS {
            map.insert(a, default_shortcut(a));
        }
        for (name, spec) in overrides {
            if let (Some(action), Some(shortcut)) =
                (Self::action_from_name(name), parse_shortcut(spec))
            {
                map.insert(action, shortcut);
            }
        }
        Self { map }
    }

    pub fn get(&self, a: BindAction) -> KeyboardShortcut {
        self.map
            .get(&a)
            .copied()
            .unwrap_or_else(|| default_shortcut(a))
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
            d.insert_temp(hint_id(*a), format_shortcut(keys.get(*a)));
        }
    });
}

/// [`publish_key_hints`] が配った打鍵表記を読む。
/// 配られていなければ既定の打鍵へ落ちるので、表示が空になることはない。
pub fn key_hint(ctx: &egui::Context, a: BindAction) -> String {
    ctx.data(|d| d.get_temp::<String>(hint_id(a)))
        .unwrap_or_else(|| format_shortcut(default_shortcut(a)))
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
    #[test]
    fn every_action_has_a_config_name() {
        let names = [
            "save",
            "save_as",
            "close_tab",
            "new_file",
            "new_window",
            "palette_files",
            "palette_commands",
            "toggle_terminal",
            "toggle_sidebar",
            "find",
            "toggle_cockpit",
            "toggle_kanban",
            "toggle_deck",
            "toggle_md_preview",
            "new_agent",
            "zoom_in",
            "zoom_out",
            "zoom_reset",
            "file_zoom_in",
            "file_zoom_out",
            "file_zoom_reset",
            "toggle_comment",
            "duplicate_line",
            "move_line_up",
            "move_line_down",
            "focus_explorer",
            "open_file",
            "save_all",
            "goto_line",
            "next_tab",
            "prev_tab",
            "switch_tab",
            "switch_tab_back",
            "global_search",
            "global_replace",
            "open_replace",
            "new_terminal",
            "nav_back",
            "nav_forward",
            "goto_definition",
            "goto_bracket",
            "next_problem",
            "prev_problem",
            "run_build_task",
            "toggle_problems",
            "toggle_fullscreen",
            "toggle_fold",
            "unfold_all",
            "toggle_bookmark",
            "reopen_closed_tab",
            "lsp_completion",
            "lsp_references",
            "lsp_symbols",
            "lsp_rename",
            "lsp_format",
            "lsp_code_action",
            "lsp_signature_help",
            "select_next_occurrence",
            "split_editor_right",
            "split_editor_down",
            "focus_pane_1",
            "focus_pane_2",
            "focus_pane_3",
            "diff_next_change",
            "diff_prev_change",
        ];
        assert_eq!(
            names.len(),
            ALL_ACTIONS.len(),
            "名前表とアクション一覧の数が合わない"
        );
        let mut resolved: Vec<BindAction> = names
            .iter()
            .map(|n| Keybinds::action_from_name(n).unwrap())
            .collect();
        for a in ALL_ACTIONS {
            let i = resolved
                .iter()
                .position(|r| *r == a)
                .unwrap_or_else(|| panic!("{a:?} を引ける config 名が無い"));
            resolved.remove(i);
        }
        assert!(resolved.is_empty());
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
        // 素の添字スライスは日本語コメントの途中で切れると panic するので
        // バイト単位で切ってから lossy 変換する (`window_before` と同じ理由)。
        let head = String::from_utf8_lossy(&body.as_bytes()[..body.len().min(600)]);
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
