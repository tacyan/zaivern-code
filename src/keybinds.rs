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
}

/// 全アクションの一覧 (デフォルトマップ構築用)。
const ALL_ACTIONS: [BindAction; 18] = [
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
        _ => return None,
    })
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
