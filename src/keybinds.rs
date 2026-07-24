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
    PaletteFiles,
    PaletteCommands,
    ToggleTerminal,
    ToggleSidebar,
    Find,
    ToggleCockpit,
    ToggleMdPreview,
    NewAgent,
    FontInc,
    FontDec,
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
    /// ビルドタスクの実行 (VS Code: ⇧⌘B)
    RunBuildTask,
    /// 問題パネル (VS Code: ⇧⌘M)
    ToggleProblems,
    /// フルスクリーン (VS Code: ⌃⌘F)
    ToggleFullScreen,
}

/// 全アクションの一覧 (デフォルトマップ構築用)。
const ALL_ACTIONS: [BindAction; 34] = [
    BindAction::Save,
    BindAction::SaveAs,
    BindAction::CloseTab,
    BindAction::NewFile,
    BindAction::PaletteFiles,
    BindAction::PaletteCommands,
    BindAction::ToggleTerminal,
    BindAction::ToggleSidebar,
    BindAction::Find,
    BindAction::ToggleCockpit,
    BindAction::ToggleMdPreview,
    BindAction::NewAgent,
    BindAction::FontInc,
    BindAction::FontDec,
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
    BindAction::GlobalSearch,
    BindAction::OpenReplace,
    BindAction::NewTerminal,
    BindAction::NavBack,
    BindAction::NavForward,
    BindAction::GoToDefinition,
    BindAction::GoToBracket,
    BindAction::RunBuildTask,
    BindAction::ToggleProblems,
    BindAction::ToggleFullScreen,
];

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
        BindAction::PaletteFiles => KeyboardShortcut::new(cmd, Key::P),
        BindAction::PaletteCommands => KeyboardShortcut::new(cmd_shift, Key::P),
        BindAction::ToggleTerminal => KeyboardShortcut::new(cmd, Key::J),
        BindAction::ToggleSidebar => KeyboardShortcut::new(cmd, Key::B),
        BindAction::Find => KeyboardShortcut::new(cmd, Key::F),
        BindAction::ToggleCockpit => KeyboardShortcut::new(cmd_shift, Key::C),
        BindAction::ToggleMdPreview => KeyboardShortcut::new(cmd_shift, Key::V),
        BindAction::NewAgent => KeyboardShortcut::new(cmd_shift, Key::A),
        BindAction::FontInc => KeyboardShortcut::new(cmd, Key::Plus),
        BindAction::FontDec => KeyboardShortcut::new(cmd, Key::Minus),
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
        BindAction::GlobalSearch => KeyboardShortcut::new(cmd_shift, Key::F),
        BindAction::OpenReplace => KeyboardShortcut::new(cmd.plus(Modifiers::ALT), Key::F),
        BindAction::NewTerminal => {
            KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Backtick)
        }
        BindAction::NavBack => KeyboardShortcut::new(Modifiers::CTRL, Key::Minus),
        BindAction::NavForward => {
            KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::SHIFT), Key::Minus)
        }
        BindAction::GoToDefinition => KeyboardShortcut::new(Modifiers::NONE, Key::F12),
        BindAction::GoToBracket => KeyboardShortcut::new(cmd_shift, Key::Backslash),
        BindAction::RunBuildTask => KeyboardShortcut::new(cmd_shift, Key::B),
        BindAction::ToggleProblems => KeyboardShortcut::new(cmd_shift, Key::M),
        BindAction::ToggleFullScreen => {
            KeyboardShortcut::new(Modifiers::CTRL.plus(Modifiers::COMMAND), Key::F)
        }
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
            "palette_files" => PaletteFiles,
            "palette_commands" => PaletteCommands,
            "toggle_terminal" => ToggleTerminal,
            "toggle_sidebar" => ToggleSidebar,
            "find" => Find,
            "toggle_cockpit" => ToggleCockpit,
            "toggle_md_preview" => ToggleMdPreview,
            "new_agent" => NewAgent,
            "font_inc" => FontInc,
            "font_dec" => FontDec,
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
            "global_search" => GlobalSearch,
            "open_replace" => OpenReplace,
            "new_terminal" => NewTerminal,
            "nav_back" => NavBack,
            "nav_forward" => NavForward,
            "goto_definition" => GoToDefinition,
            "goto_bracket" => GoToBracket,
            "run_build_task" => RunBuildTask,
            "toggle_problems" => ToggleProblems,
            "toggle_fullscreen" => ToggleFullScreen,
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
        "slash" => Slash,
        "comma" => Comma,
        "period" => Period,
        "openbracket" => OpenBracket,
        "closebracket" => CloseBracket,
        "backslash" => Backslash,
        _ => return None,
    })
}

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
        assert_eq!(parse_shortcut("cmd+f20"), Some(sc(Modifiers::COMMAND, Key::F20)));
        assert_eq!(parse_shortcut("f21"), None);
    }

    #[test]
    fn parse_alt_up() {
        assert_eq!(
            parse_shortcut("alt+up"),
            Some(sc(Modifiers::ALT, Key::ArrowUp))
        );
        // option は alt の別名
        assert_eq!(parse_shortcut("option+down"), Some(sc(Modifiers::ALT, Key::ArrowDown)));
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
        assert_eq!(parse_shortcut("space"), Some(sc(Modifiers::NONE, Key::Space)));
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
}
