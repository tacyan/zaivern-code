//! テキスト編集操作の純関数モジュール。
//!
//! カーソル/選択範囲はすべて **char インデックス**(バイトではない)で扱う。
//! 全関数はマルチバイト(日本語等)安全。

#![allow(dead_code)]

/// char インデックス -> バイトインデックス変換。範囲外は文字列末尾にクランプ。
pub fn char_to_byte(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

/// バイトインデックス -> char インデックス変換(byte_idx は char 境界であること)。
fn byte_to_char(s: &str, byte_idx: usize) -> usize {
    s[..byte_idx.min(s.len())].chars().count()
}

/// byte 位置を含む行の (行頭 byte, 行末 byte) を返す。行末は '\n' を含まない。
fn line_bounds(text: &str, byte: usize) -> (usize, usize) {
    let byte = byte.min(text.len());
    let start = text[..byte].rfind('\n').map(|i| i + 1).unwrap_or(0);
    let end = text[byte..]
        .find('\n')
        .map(|i| byte + i)
        .unwrap_or(text.len());
    (start, end)
}

/// lines(split('\n') 済み)内でのカーソルの (行インデックス, カラム[char]) を返す。
fn locate_line_col(lines: &[&str], cursor_char: usize) -> (usize, usize) {
    let mut col = cursor_char;
    let last = lines.len().saturating_sub(1);
    for (i, line) in lines.iter().enumerate() {
        let len = line.chars().count();
        if col <= len || i == last {
            return (i, col.min(len));
        }
        col -= len + 1;
    }
    (0, 0)
}

/// Enter 押下直後(text の cursor_char 直前が '\n')に呼ぶ。
/// 直前行の先頭空白を新しい行に複製し、直前行が `{` `(` `[` `:` で終わるなら
/// さらに4スペース追加。適用したら Some((新text, 新cursor_char))。
pub fn auto_indent_after_newline(text: &str, cursor_char: usize) -> Option<(String, usize)> {
    if cursor_char == 0 {
        return None;
    }
    let cursor_byte = char_to_byte(text, cursor_char);
    let before = &text[..cursor_byte];
    if !before.ends_with('\n') {
        return None;
    }
    // 直前行 = 挿入された '\n' の手前の行
    let prev = &before[..before.len() - 1];
    let prev_line_start = prev.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let prev_line = &prev[prev_line_start..];

    let mut indent: String = prev_line
        .chars()
        .take_while(|c| *c == ' ' || *c == '\t')
        .collect();
    let opens_block = matches!(
        prev_line.trim_end().chars().last(),
        Some('{') | Some('(') | Some('[') | Some(':')
    );
    if opens_block {
        indent.push_str("    ");
    }
    if indent.is_empty() {
        return None;
    }
    let added_chars = indent.chars().count();
    let mut out = String::with_capacity(text.len() + indent.len());
    out.push_str(before);
    out.push_str(&indent);
    out.push_str(&text[cursor_byte..]);
    Some((out, cursor_char + added_chars))
}

/// 選択範囲(char 範囲, start==end なら現在行のみ)の各行の行コメントをトグル。
/// 全行(空白のみの行を除く)がコメント済みなら外す、そうでなければ付ける(prefix + " ")。
/// 戻り値: (新text, 新sel_start, 新sel_end)
pub fn toggle_comment(
    text: &str,
    sel_start: usize,
    sel_end: usize,
    prefix: &str,
) -> (String, usize, usize) {
    let (s_char, e_char) = if sel_start <= sel_end {
        (sel_start, sel_end)
    } else {
        (sel_end, sel_start)
    };
    let s_byte = char_to_byte(text, s_char);
    let e_byte = char_to_byte(text, e_char);
    let (range_start, _) = line_bounds(text, s_byte);
    let (_, range_end) = line_bounds(text, e_byte);
    let block = &text[range_start..range_end];

    // 空白のみの行を除いた全行がコメント済みか判定
    let mut has_content = false;
    let mut all_commented = true;
    for line in block.split('\n') {
        let t = line.trim_start();
        if t.is_empty() {
            continue;
        }
        has_content = true;
        if !t.starts_with(prefix) {
            all_commented = false;
        }
    }
    if !has_content {
        return (text.to_string(), s_char, e_char);
    }
    let remove = all_commented;

    // 行ごとに再構築しつつ、(元テキスト上の char 位置, 増減) を記録
    let mut new_block = String::with_capacity(block.len() + 8);
    let mut edits: Vec<(usize, isize)> = Vec::new();
    let mut line_start_byte = range_start;
    for (i, line) in block.split('\n').enumerate() {
        if i > 0 {
            new_block.push('\n');
        }
        let trimmed = line.trim_start();
        let ws_bytes = line.len() - trimmed.len();
        if trimmed.is_empty() {
            new_block.push_str(line);
        } else if remove {
            let after = &trimmed[prefix.len()..];
            let (removed_chars, rest) = if let Some(stripped) = after.strip_prefix(' ') {
                (prefix.chars().count() + 1, stripped)
            } else {
                (prefix.chars().count(), after)
            };
            new_block.push_str(&line[..ws_bytes]);
            new_block.push_str(rest);
            let pos_char = byte_to_char(text, line_start_byte + ws_bytes);
            edits.push((pos_char, -(removed_chars as isize)));
        } else {
            new_block.push_str(&line[..ws_bytes]);
            new_block.push_str(prefix);
            new_block.push(' ');
            new_block.push_str(trimmed);
            let pos_char = byte_to_char(text, line_start_byte + ws_bytes);
            edits.push((pos_char, (prefix.chars().count() + 1) as isize));
        }
        line_start_byte += line.len() + 1;
    }

    let mut new_text = String::with_capacity(text.len() + 16);
    new_text.push_str(&text[..range_start]);
    new_text.push_str(&new_block);
    new_text.push_str(&text[range_end..]);

    let adjust = |sel: usize| -> usize {
        let mut new = sel;
        for &(pos, delta) in &edits {
            if delta > 0 {
                if sel >= pos {
                    new += delta as usize;
                }
            } else {
                let removed = (-delta) as usize;
                if sel >= pos + removed {
                    new -= removed;
                } else if sel > pos {
                    new -= sel - pos;
                }
            }
        }
        new
    };
    let new_start = adjust(s_char);
    let new_end = adjust(e_char);
    (new_text, new_start, new_end)
}

/// カーソル行を下に複製。(新text, 新cursor_char=複製行の同カラム)
pub fn duplicate_line(text: &str, cursor_char: usize) -> (String, usize) {
    let cursor_char = cursor_char.min(text.chars().count());
    let cursor_byte = char_to_byte(text, cursor_char);
    let (line_start, line_end) = line_bounds(text, cursor_byte);
    let line = &text[line_start..line_end];
    let mut out = String::with_capacity(text.len() + line.len() + 1);
    out.push_str(&text[..line_end]);
    out.push('\n');
    out.push_str(line);
    out.push_str(&text[line_end..]);
    (out, cursor_char + line.chars().count() + 1)
}

/// カーソル行を上/下の行と入れ替え。端では無変更。(新text, 新cursor_char)
pub fn move_line(text: &str, cursor_char: usize, up: bool) -> (String, usize) {
    let cursor_char = cursor_char.min(text.chars().count());
    let lines: Vec<&str> = text.split('\n').collect();
    let (idx, col) = locate_line_col(&lines, cursor_char);
    let target = if up {
        if idx == 0 {
            return (text.to_string(), cursor_char);
        }
        idx - 1
    } else {
        if idx + 1 >= lines.len() {
            return (text.to_string(), cursor_char);
        }
        idx + 1
    };
    let mut new_lines = lines;
    new_lines.swap(idx, target);
    let new_text = new_lines.join("\n");
    let mut new_cursor = 0;
    for line in &new_lines[..target] {
        new_cursor += line.chars().count() + 1;
    }
    new_cursor += col;
    (new_text, new_cursor)
}

/// syntect の言語名から行コメントプレフィックスを返す。不明なら None。
pub fn comment_prefix_for(lang: &str) -> Option<&'static str> {
    let l = lang.to_ascii_lowercase();
    match l.as_str() {
        "rust" | "c" | "c++" | "javascript" | "javascript (babel)" | "typescript" | "tsx"
        | "jsx" | "go" | "java" | "c#" | "csharp" | "swift" | "kotlin" | "scala" | "dart"
        | "objective-c" | "php" => Some("//"),
        "python" | "ruby" | "shell" | "shell script" | "shell-unix-generic" | "bash" | "sh"
        | "zsh" | "toml" | "yaml" | "makefile" | "perl" | "r" | "dockerfile" => Some("#"),
        "lua" | "sql" | "haskell" => Some("--"),
        _ => {
            if l.contains("bash") || l.contains("shell") {
                Some("#")
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- auto_indent_after_newline ----

    #[test]
    fn auto_indent_after_open_brace() {
        let text = "fn main() {\n";
        let got = auto_indent_after_newline(text, 12);
        assert_eq!(got, Some(("fn main() {\n    ".to_string(), 16)));
    }

    #[test]
    fn auto_indent_copies_existing_indent() {
        let text = "    let x = 1;\n";
        let got = auto_indent_after_newline(text, 15);
        assert_eq!(got, Some(("    let x = 1;\n    ".to_string(), 19)));
    }

    #[test]
    fn auto_indent_nested_brace_adds_four_more() {
        let text = "    if x {\n";
        let got = auto_indent_after_newline(text, 11);
        assert_eq!(got, Some(("    if x {\n        ".to_string(), 19)));
    }

    #[test]
    fn auto_indent_japanese_colon_line() {
        let text = "if 条件:\n";
        let got = auto_indent_after_newline(text, 7);
        assert_eq!(got, Some(("if 条件:\n    ".to_string(), 11)));
    }

    #[test]
    fn auto_indent_none_when_nothing_to_insert() {
        // インデントなし・ブロック開始でもない
        assert_eq!(auto_indent_after_newline("abc\n", 4), None);
        // カーソル直前が '\n' でない
        assert_eq!(auto_indent_after_newline("abc", 2), None);
        assert_eq!(auto_indent_after_newline("", 0), None);
    }

    #[test]
    fn auto_indent_with_text_after_cursor() {
        let text = "    foo\nbar";
        let got = auto_indent_after_newline(text, 8);
        assert_eq!(got, Some(("    foo\n    bar".to_string(), 12)));
    }

    // ---- toggle_comment ----

    #[test]
    fn toggle_comment_adds_on_single_line() {
        let (t, s, e) = toggle_comment("let x = 1;", 3, 3, "//");
        assert_eq!(t, "// let x = 1;");
        assert_eq!((s, e), (6, 6));
    }

    #[test]
    fn toggle_comment_removes_on_single_line() {
        let (t, s, e) = toggle_comment("// let x = 1;", 0, 0, "//");
        assert_eq!(t, "let x = 1;");
        assert_eq!((s, e), (0, 0));
    }

    #[test]
    fn toggle_comment_adds_on_multiline_selection() {
        let (t, s, e) = toggle_comment("a\nb\nc", 0, 5, "//");
        assert_eq!(t, "// a\n// b\n// c");
        assert_eq!((s, e), (3, 14));
    }

    #[test]
    fn toggle_comment_removes_japanese_lines() {
        // "# こんにちは\n# 世界" 全12 chars を選択
        let (t, s, e) = toggle_comment("# こんにちは\n# 世界", 0, 12, "#");
        assert_eq!(t, "こんにちは\n世界");
        assert_eq!((s, e), (0, 8));
    }

    #[test]
    fn toggle_comment_respects_indentation() {
        let (t, s, e) = toggle_comment("    foo", 4, 4, "//");
        assert_eq!(t, "    // foo");
        assert_eq!((s, e), (7, 7));
    }

    #[test]
    fn toggle_comment_removes_prefix_without_space() {
        let (t, s, e) = toggle_comment("//x", 0, 0, "//");
        assert_eq!(t, "x");
        assert_eq!((s, e), (0, 0));
    }

    #[test]
    fn toggle_comment_mixed_lines_comments_all() {
        // 一部のみコメント済み → 全行にコメントを付ける
        let (t, _, _) = toggle_comment("// a\nb", 0, 6, "//");
        assert_eq!(t, "// // a\n// b");
    }

    // ---- duplicate_line ----

    #[test]
    fn duplicate_line_single_line() {
        let (t, c) = duplicate_line("hello", 2);
        assert_eq!(t, "hello\nhello");
        assert_eq!(c, 8); // 複製行の同カラム(col=2)
    }

    #[test]
    fn duplicate_line_middle_line() {
        let (t, c) = duplicate_line("a\nbb\nc", 3);
        assert_eq!(t, "a\nbb\nbb\nc");
        assert_eq!(c, 6);
    }

    #[test]
    fn duplicate_line_japanese_last_line() {
        let (t, c) = duplicate_line("こんにちは", 3);
        assert_eq!(t, "こんにちは\nこんにちは");
        assert_eq!(c, 9);
    }

    // ---- move_line ----

    #[test]
    fn move_line_up_swaps_lines() {
        let (t, c) = move_line("a\nb", 2, true);
        assert_eq!(t, "b\na");
        assert_eq!(c, 0);
    }

    #[test]
    fn move_line_up_at_first_line_is_noop() {
        let (t, c) = move_line("a\nb", 0, true);
        assert_eq!(t, "a\nb");
        assert_eq!(c, 0);
    }

    #[test]
    fn move_line_down_at_last_line_is_noop() {
        let (t, c) = move_line("a\nb", 2, false);
        assert_eq!(t, "a\nb");
        assert_eq!(c, 2);
    }

    #[test]
    fn move_line_down_japanese_keeps_column() {
        // "あい" 行(col=1)を下へ
        let (t, c) = move_line("あい\nうえ\nお", 1, false);
        assert_eq!(t, "うえ\nあい\nお");
        assert_eq!(c, 4); // "うえ\n" = 3 chars + col 1
    }

    // ---- comment_prefix_for ----

    #[test]
    fn comment_prefix_for_known_languages() {
        assert_eq!(comment_prefix_for("Rust"), Some("//"));
        assert_eq!(comment_prefix_for("TypeScript"), Some("//"));
        assert_eq!(comment_prefix_for("C#"), Some("//"));
        assert_eq!(comment_prefix_for("Python"), Some("#"));
        assert_eq!(comment_prefix_for("YAML"), Some("#"));
        assert_eq!(comment_prefix_for("Bourne Again Shell (bash)"), Some("#"));
        assert_eq!(comment_prefix_for("Lua"), Some("--"));
        assert_eq!(comment_prefix_for("SQL"), Some("--"));
        assert_eq!(comment_prefix_for("Haskell"), Some("--"));
    }

    #[test]
    fn comment_prefix_for_unknown_is_none() {
        assert_eq!(comment_prefix_for("HTML"), None);
        assert_eq!(comment_prefix_for("CSS"), None);
        assert_eq!(comment_prefix_for("Markdown"), None);
        assert_eq!(comment_prefix_for("Plain Text"), None);
    }
}
