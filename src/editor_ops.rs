//! テキスト編集操作の純関数モジュール。
//!
//! カーソル/選択範囲はすべて **char インデックス**(バイトではない)で扱う。
//! 全関数はマルチバイト(日本語等)安全。

#![allow(dead_code)]

use crate::textenc::{detect_line_ending, normalize_to, LineEnding};

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

/// カーソル位置 (char) の括弧に対応する相手の括弧位置 (char) を返す。
/// カーソル直後の文字、なければ直前の文字を括弧として解釈する (VS Code と同じ)。
/// 文字列/コメントは考慮しない素朴なネスト数えだが、実用上は十分。
pub fn matching_bracket(text: &str, cursor_char: usize) -> Option<usize> {
    // `<>` は比較演算子と区別できないため対象にしない (VS Code も既定では対象外)
    const PAIRS: [(char, char); 3] = [('(', ')'), ('[', ']'), ('{', '}')];
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return None;
    }
    let cursor = cursor_char.min(chars.len());
    // カーソル直後 → 直前の順で括弧を探す
    let (pos, ch) = if cursor < chars.len() && is_bracket(chars[cursor], &PAIRS) {
        (cursor, chars[cursor])
    } else if cursor > 0 && is_bracket(chars[cursor - 1], &PAIRS) {
        (cursor - 1, chars[cursor - 1])
    } else {
        return None;
    };
    let (open, close, forward) = PAIRS.iter().find_map(|&(o, c)| {
        if ch == o {
            Some((o, c, true))
        } else if ch == c {
            Some((o, c, false))
        } else {
            None
        }
    })?;
    let mut depth = 0i64;
    if forward {
        for (i, &c) in chars.iter().enumerate().skip(pos) {
            if c == open {
                depth += 1;
            } else if c == close {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    } else {
        for i in (0..=pos).rev() {
            let c = chars[i];
            if c == close {
                depth += 1;
            } else if c == open {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
        }
    }
    None
}

fn is_bracket(c: char, pairs: &[(char, char)]) -> bool {
    pairs.iter().any(|&(o, cl)| c == o || c == cl)
}

/// 自動ペア対象 (括弧 + 引用符)。`<>` は比較演算子と衝突するため対象外。
const AUTO_PAIRS: [(char, char); 6] = [
    ('(', ')'),
    ('[', ']'),
    ('{', '}'),
    ('"', '"'),
    ('\'', '\''),
    ('`', '`'),
];

/// 括弧・引用符の自動編集 (VS Code の autoClosingBrackets 相当)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairEdit {
    /// 選択範囲を開き/閉じで囲む。新テキストと新しい選択範囲 (char)。
    Surround {
        text: String,
        select: (usize, usize),
    },
    /// 開き+閉じを挿入してカーソルをペアの間に置く。
    Insert { text: String, cursor: usize },
    /// 既にある閉じを飛び越える (重複挿入を避けてカーソルだけ右へ)。
    SkipOver { cursor: usize },
}

/// 文字 `typed` を打った瞬間の自動ペア判定。該当しなければ None (通常入力)。
/// `sel_min..sel_max` は現在の選択 (char)。空選択なら両者同値。
pub fn pair_on_type(text: &str, sel_min: usize, sel_max: usize, typed: char) -> Option<PairEdit> {
    let closer_of = AUTO_PAIRS
        .iter()
        .find(|(o, _)| *o == typed)
        .map(|(_, c)| *c);
    let chars_len = text.chars().count();
    let sel_min = sel_min.min(chars_len);
    let sel_max = sel_max.min(chars_len);

    if sel_min < sel_max {
        // 選択あり: 開き (または引用符) を打ったら囲む
        let close = closer_of?;
        let a = char_to_byte(text, sel_min);
        let b = char_to_byte(text, sel_max);
        let mut nt = String::with_capacity(text.len() + 2);
        nt.push_str(&text[..a]);
        nt.push(typed);
        nt.push_str(&text[a..b]);
        nt.push(close);
        nt.push_str(&text[b..]);
        return Some(PairEdit::Surround {
            text: nt,
            select: (sel_min + 1, sel_max + 1),
        });
    }

    let next = text.chars().nth(sel_max);
    // スキップ: 閉じ文字を打ったが直後に同じ閉じ文字がもうある
    let is_closer = AUTO_PAIRS.iter().any(|(_, c)| *c == typed);
    if is_closer && next == Some(typed) {
        return Some(PairEdit::SkipOver {
            cursor: sel_max + 1,
        });
    }
    let close = closer_of?;
    // 引用符は単語や同じ引用符の直後では自動閉じしない (don't 等のアポストロフィ)
    if typed == close {
        let prev = sel_min.checked_sub(1).and_then(|i| text.chars().nth(i));
        if prev.is_some_and(|c| c.is_alphanumeric() || c == typed) {
            return None;
        }
    }
    // 直後が空白/行末/閉じ括弧のときだけ自動閉じ (既存コードへの割込を避ける)
    let ok_next = match next {
        None => true,
        Some(c) if c.is_whitespace() => true,
        Some(c) if AUTO_PAIRS.iter().any(|(_, cl)| *cl == c) => true,
        _ => false,
    };
    if !ok_next {
        return None;
    }
    let b = char_to_byte(text, sel_min);
    let mut nt = String::with_capacity(text.len() + 2);
    nt.push_str(&text[..b]);
    nt.push(typed);
    nt.push(close);
    nt.push_str(&text[b..]);
    Some(PairEdit::Insert {
        text: nt,
        cursor: sel_min + 1,
    })
}

/// Backspace で空ペア `()` の間にいたら両方まとめて消す。
/// 該当すれば (新テキスト, 新カーソル) を返す。
pub fn pair_on_backspace(text: &str, cursor: usize) -> Option<(String, usize)> {
    if cursor == 0 {
        return None;
    }
    let prev = text.chars().nth(cursor - 1)?;
    let next = text.chars().nth(cursor)?;
    if !AUTO_PAIRS.iter().any(|(o, c)| *o == prev && *c == next) {
        return None;
    }
    let a = char_to_byte(text, cursor - 1);
    let b = char_to_byte(text, cursor + 1);
    let mut nt = String::with_capacity(text.len().saturating_sub(2));
    nt.push_str(&text[..a]);
    nt.push_str(&text[b..]);
    Some((nt, cursor - 1))
}

/// 大文字小文字を無視して `start_char` 以降 (見つからなければ先頭から) を検索。
/// `hay[from..]` から検索 (from が char 境界でなければ None)。
fn find_at(hay: &str, needle: &str, from: usize) -> Option<usize> {
    hay.get(from..)?.find(needle).map(|p| p + from)
}

/// ヒットの char 位置を返す (start_char から、無ければ先頭から取り直す)。
///
/// 小文字化はバイト境界をずらすことがある (İ は +1、Ω ẞ は −1 など)。
/// 「合計バイト長が同じなら安全」という従来の判定は İ と Ω の相殺で破れ、
/// `text[..byte_pos]` が char 境界 panic を起こしていた。境界を跨ぐスライスは
/// すべて `get()` にして、ずれを検知したら大小区別ありへフォールバックする。
pub fn find_ci(text: &str, query: &str, start_char: usize) -> Option<usize> {
    if query.is_empty() {
        return None;
    }
    let start_byte = char_to_byte(text, start_char);
    let hay_lower = text.to_lowercase();
    let needle_lower = query.to_lowercase();
    if hay_lower.len() == text.len() {
        if let Some(byte_pos) = find_at(&hay_lower, &needle_lower, start_byte.min(hay_lower.len()))
            .or_else(|| hay_lower.find(&needle_lower))
        {
            if let Some(head) = text.get(..byte_pos) {
                return Some(head.chars().count());
            }
        }
    }
    let byte_pos = find_at(text, query, start_byte.min(text.len())).or_else(|| text.find(query))?;
    Some(text[..byte_pos].chars().count())
}

/// 大文字小文字を無視した全置換。(新text, 置換件数)。
/// query が空なら無変更。置換文字列に query を含んでも無限ループしない。
pub fn replace_all_ci(text: &str, query: &str, rep: &str) -> (String, usize) {
    if query.is_empty() {
        return (text.to_string(), 0);
    }
    let hay_lower = text.to_lowercase();
    let needle_lower = query.to_lowercase();
    // find_ci と同じ理由で、小文字化した写しの位置が text の char 境界から
    // ずれたら (get() が None) 大小区別ありでやり直す。
    if hay_lower.len() == text.len() {
        if let Some(r) = replace_all_mapped(text, &hay_lower, &needle_lower, rep) {
            return r;
        }
    }
    replace_all_mapped(text, text, query, rep).unwrap_or_else(|| (text.to_string(), 0))
}

/// hay 上のヒット位置で text を置換する。境界がずれていたら None。
fn replace_all_mapped(text: &str, hay: &str, needle: &str, rep: &str) -> Option<(String, usize)> {
    let mut out = String::with_capacity(text.len());
    let mut count = 0usize;
    let mut byte = 0usize;
    while let Some(p) = hay.get(byte..)?.find(needle) {
        let at = byte + p;
        out.push_str(text.get(byte..at)?);
        out.push_str(rep);
        count += 1;
        byte = at + needle.len();
    }
    out.push_str(text.get(byte..)?);
    Some((out, count))
}

/// char 位置の行番号 (0-based) を返す。スクロール計算用。
pub fn line_of_char(text: &str, char_idx: usize) -> usize {
    text.chars().take(char_idx).filter(|c| *c == '\n').count()
}

/// (行, 桁) [0-based, char 単位] を char インデックスへ変換する。
/// 行・桁とも実在範囲へクランプする。
pub fn char_index_at(text: &str, line: usize, col: usize) -> usize {
    let lines: Vec<&str> = text.split('\n').collect();
    let line = line.min(lines.len().saturating_sub(1));
    let mut idx = 0usize;
    for l in lines.iter().take(line) {
        idx += l.chars().count() + 1;
    }
    idx + col.min(lines[line].chars().count())
}

/// 「行[:列]」形式 (1-based) をパースして 0-based の (行, 列) を返す。
/// 例: "42" -> (41, 0) / "42:5" -> (41, 4)。数値でなければ None。
pub fn parse_goto(s: &str) -> Option<(usize, usize)> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let (l, c) = match s.split_once([':', ',']) {
        Some((l, c)) => (l.trim(), Some(c.trim())),
        None => (s, None),
    };
    let line: usize = l.parse().ok()?;
    let col: usize = match c {
        Some("") | None => 1,
        Some(c) => c.parse().ok()?,
    };
    Some((line.saturating_sub(1), col.saturating_sub(1)))
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
            // プラグインが持ち込んだ言語 (Zig / Elixir / Nix …) は
            // その構文定義に書いてある行コメント記号を使う。
            if let Some(p) = crate::highlight::dynamic_line_comment(lang) {
                return Some(p);
            }
            if l.contains("bash") || l.contains("shell") {
                Some("#")
            } else {
                None
            }
        }
    }
}

// ───────────────────── 保存時のクリーンアップ ─────────────────────
//
// 「行末の空白を落とす」「最終行に改行を入れる」は、他のエディタでは保存時の
// 既定になっていることが多い。差分に無意味な空白変更が混ざらなくなるため。
//
// # UI 側の配線 (このモジュールからは触れないので申し送り)
//
// ```ignore
// // 保存直前 — editor.rs の Buffer::write_to を呼ぶ前に挟む
// let opts = SaveCleanup {
//     trim_trailing: cfg.trim_trailing_whitespace,   // 設定 (既定 false = 今までどおり)
//     final_newline: cfg.insert_final_newline,
//     target_ending: Some(buf.line_ending),          // 開いたときに覚えた改行コード
// };
// let (cleaned, changed) = apply_save_cleanup_checked(&buf.text, &opts);
// if changed {
//     // カーソル・選択範囲は行末が削れたぶんずれるので必ず付け替える
//     cursor = adjust_char_index_after_cleanup(&buf.text, &cleaned, cursor);
//     sel_end = adjust_char_index_after_cleanup(&buf.text, &cleaned, sel_end);
//     buf.text = cleaned;   // dirty 判定・undo スタックもここで更新する
// }
// ```
//
// `changed` が false のときは本文が 1 バイトも変わっていないので、
// 書き込みも undo 積みもカーソル付け替えも丸ごと省ける。

/// 行の切れ目。`(本文開始, 本文終了, 改行を含む終了)` のバイト位置。
///
/// LF / CRLF / CR のどれでも 1 行として切る (CR だけのファイルも 1 行ずつ扱える)。
/// 最終行は改行が無くても 1 行と数える — 本文が `"a\n"` なら
/// 「`a`」と「空の最終行」の 2 行。エディタの見た目と行数が一致する。
fn line_segments(text: &str) -> Vec<(usize, usize, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < b.len() {
        let skip = match b[i] {
            b'\r' if b.get(i + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => {
                i += 1;
                continue;
            }
        };
        out.push((start, i, i + skip));
        i += skip;
        start = i;
    }
    out.push((start, b.len(), b.len()));
    out
}

/// 行末で落としてよい空白か。
///
/// **全角スペース U+3000 と NBSP U+00A0 は落とさない。** 日本語の本文では
/// 字下げ・体裁として意図して置かれる有意な文字で、保存のたびに消えると
/// 書いた内容が変わってしまう (半角スペースやタブと違い「見た目に効く文字」)。
/// `\r` `\n` も対象外 — 改行は本文ではなく行の区切りなので絶対に削らない。
fn is_trimmable_space(c: char) -> bool {
    c.is_whitespace() && !matches!(c, '\u{3000}' | '\u{00a0}' | '\r' | '\n')
}

/// 各行の行末の空白 (半角スペース・タブ等) を落とす。
///
/// - 改行は LF / CRLF / CR のどれでもそのまま残す (CRLF の `\r` を食わない)。
/// - 行数は絶対に変えない — 改行を消さないので、空白だけの行は「空の行」になるだけ。
/// - 落とすものが無ければ入力と同一の文字列を返す。
pub fn trim_trailing_whitespace(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for (s, e, seg_end) in line_segments(text) {
        out.push_str(text[s..e].trim_end_matches(is_trimmable_space));
        out.push_str(&text[e..seg_end]);
    }
    out
}

/// 最終行に改行が無ければ足す。
///
/// - 既に改行で終わっていれば何もしない (空行を増やさない)。
/// - 空の本文には足さない — 何も書いていないファイルを 1 行のファイルにしない。
/// - 空白だけの本文には足す (「1 行書いてある」ので他の行と同じ扱い)。
/// - `ending` が [`LineEnding::Mixed`] のときは最多の様式を使う。
pub fn ensure_final_newline(text: &str, ending: LineEnding) -> String {
    if text.is_empty() || text.ends_with('\n') || text.ends_with('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len() + 2);
    out.push_str(text);
    out.push_str(ending.as_str());
    out
}

/// 末尾の余分な空行を落とす (VS Code の `files.trimFinalNewlines`)。
///
/// 「最後の改行より後ろの改行を全部落とす」— つまり本文の終わりに改行が
/// あれば **1 本だけ**残す。改行で終わっていない本文は 1 文字も変えない
/// (足すのは [`ensure_final_newline`] の仕事で、こちらは削るだけ)。
///
/// CRLF / CR も 1 本の改行として数えるので、`"a\r\n\r\n"` は `"a\r\n"` になる。
pub fn trim_final_newlines(text: &str) -> String {
    let content = text.trim_end_matches(['\n', '\r']);
    if content.len() == text.len() {
        return text.to_string();
    }
    // 本文の直後にある改行 1 本ぶんだけを残す
    let rest = &text[content.len()..];
    let keep = if rest.starts_with("\r\n") {
        2
    } else {
        rest.chars().next().map(|c| c.len_utf8()).unwrap_or(0)
    };
    let mut out = String::with_capacity(content.len() + keep);
    out.push_str(content);
    out.push_str(&rest[..keep]);
    out
}

/// 保存時に本文へかける整形。すべて既定は「何もしない」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveCleanup {
    /// 行末の空白を落とす。
    pub trim_trailing: bool,
    /// 末尾の余分な空行を落とす。
    pub trim_final_newlines: bool,
    /// 最終行に改行を入れる。
    pub final_newline: bool,
    /// 改行コードを揃える。`None` なら本文の改行には触らない。
    pub target_ending: Option<LineEnding>,
}

impl SaveCleanup {
    /// 何も仕事が無い設定か (呼び出し側が丸ごと省けるようにする)。
    pub fn is_noop(&self) -> bool {
        !self.trim_trailing
            && !self.trim_final_newlines
            && !self.final_newline
            && self.target_ending.is_none()
    }
}

/// [`SaveCleanup`] を順に適用する。適用順は固定:
///
/// 1. 行末の空白を落とす
/// 2. 末尾の余分な空行を落とす
/// 3. 改行コードを揃える (`target_ending` があるとき)
/// 4. 最終行に改行を入れる
///
/// この順でないと、1 で行末が空白だけになった行を 4 が数え違えたり、
/// 4 が足した改行を 3 が揃え忘れたりする。2 が 1 の後なのは、
/// 「空白だけの行」が 1 で空行になってはじめて削れるようになるため。
pub fn apply_save_cleanup(text: &str, opts: &SaveCleanup) -> String {
    apply_save_cleanup_checked(text, opts).0
}

/// [`apply_save_cleanup`] に「変わったか」を添えた版。
///
/// `changed == false` なら本文は 1 バイトも変わっていないので、
/// 書き込み・undo 積み・カーソル付け替えをまとめて省ける。
pub fn apply_save_cleanup_checked(text: &str, opts: &SaveCleanup) -> (String, bool) {
    if opts.is_noop() {
        return (text.to_string(), false);
    }
    let mut out = if opts.trim_trailing {
        trim_trailing_whitespace(text)
    } else {
        text.to_string()
    };
    if opts.trim_final_newlines {
        out = trim_final_newlines(&out);
    }
    if let Some(target) = opts.target_ending {
        out = normalize_to(&out, target);
    }
    if opts.final_newline {
        // 変換先が指定されていなければ、その本文で一番多い改行に合わせる
        let ending = opts
            .target_ending
            .unwrap_or_else(|| detect_line_ending(&out));
        out = ensure_final_newline(&out, ending);
    }
    let changed = out != text;
    (out, changed)
}

/// 整形前の**バイト**位置を、整形後の同じ「見た目の位置」へ付け替える。
///
/// 行末の空白が消えると後続の文字位置が全部ずれるので、これを通さないと
/// カーソルが別の行の途中へ飛ぶ。方針は「行と桁を保つ」:
///
/// - 行の途中 → 同じ行の同じ桁。
/// - 消えた空白の中にいた → その行の新しい行末 (ユーザーが見ていた場所に一番近い)。
/// - 改行の途中 (CRLF の `\r` と `\n` の間) → 変換後の改行の中の同じ位置。
/// - 行が減った (整形後のほうが行数が少ない) → 最後の行へ寄せる。
///
/// 返り値は必ず文字境界 — 多バイト文字 (日本語) の途中は返さない。
pub fn adjust_offset_after_cleanup(original: &str, cleaned: &str, offset: usize) -> usize {
    if original == cleaned {
        return snap_char_boundary(cleaned, offset);
    }
    let offset = offset.min(original.len());
    let src = line_segments(original);
    let dst = line_segments(cleaned);
    // offset を含む行 = 行頭が offset 以下である最後の行
    let i = src.iter().rposition(|&(s, _, _)| s <= offset).unwrap_or(0);
    let (s, e, _) = src[i];
    let (ds, de, dseg_end) = dst[i.min(dst.len() - 1)];
    let mapped = if offset <= e {
        // 本文の中 (行末より後ろ = 削られた空白の中なら行末へ寄せる)
        ds + (offset - s).min(de - ds)
    } else {
        // 改行バイトの途中
        de + (offset - e).min(dseg_end - de)
    };
    snap_char_boundary(cleaned, mapped)
}

/// [`adjust_offset_after_cleanup`] の char インデックス版。
///
/// このモジュールの他の関数と egui の `CCursor` は char 単位なので、
/// UI から呼ぶときはこちらを使う (バイト版はファイル入出力側向け)。
pub fn adjust_char_index_after_cleanup(original: &str, cleaned: &str, char_idx: usize) -> usize {
    let byte = char_to_byte(original, char_idx);
    byte_to_char(
        cleaned,
        adjust_offset_after_cleanup(original, cleaned, byte),
    )
}

/// バイト位置を文字境界まで手前へ寄せる (多バイト文字を割らないため)。
fn snap_char_boundary(s: &str, byte: usize) -> usize {
    let mut byte = byte.min(s.len());
    while byte > 0 && !s.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
}

// ═══════════════ マルチカーソル / 矩形選択エンジン ═══════════════
//
// # なぜ「エンジンだけ」なのか
//
// 本文の編集面は `egui::TextEdit::multiline` で、**キャレットは 1 本しか持てない**
// (egui 0.29 の `TextEditState` は `CCursorRange` を 1 つだけ覚える)。
// つまり「N 本のキャレットが同時に点滅し、タイプすると N 箇所へ同時に入る」
// という VS Code の見た目は、egui を差し替えるまで実現できない。
//
// そこで**選択集合の計算と一括編集だけ**をここに置く。UI は 1 本のキャレットの
// ままでも、コマンド 1 回 = [`MultiSel`] を組み立てて一括編集を 1 回、という形で
// 今すぐ価値を出せる (「全ての出現を選択 → まとめて置換」「矩形選択 → 各行の先頭へ挿入」)。
//
// # UI 側の採用手順 (このモジュールを使う側がやること)
//
// 1. `Buffer` に `multi: MultiSel` を 1 本持つ (既存の単一選択とは別に持つ)。
// 2. コマンド発火時、egui の `CCursorRange` (= **char** インデックス) から
//    [`MultiSel::from_char_ranges`] で種を作る。
//    ```ignore
//    let seed = MultiSel::from_char_ranges(&buf.text, [ccur.primary.index..ccur.secondary.index]);
//    let sel = editor_ops::add_cursor_below(&buf.text, &seed, tab_width);
//    ```
// 3. 編集は [`apply_edit_to_all`] 系を 1 回呼ぶ。返る `String` を `buf.text` へ入れ、
//    undo スタックへは**その 1 回ぶん**を積む (VS Code も複数キャレットの編集を
//    1 undo にまとめる)。
// 4. キャレットの復帰は [`MultiSel::to_single_selection_chars`] で char 範囲に
//    直して `TextEditState::cursor.set_char_range(..)` に戻す。
//
// # 今の UI で「できないこと」と回避策
//
// | できないこと | 理由 | 今できる代替 |
// |---|---|---|
// | N 本のキャレットが同時に点滅する | `TextEdit` が `CCursorRange` を 1 つしか持たない | [`MultiSel::to_single_selection`] で 1 本だけ表示 |
// | タイプした 1 文字が N 箇所へ同時に入る | キー入力は egui が単一キャレットへ適用する | コマンド (例:「選択箇所へ入力」) から [`insert_at_all`] を呼ぶ |
// | N 個の選択ハイライトが出る | 同上 | `TextEditOutput.galley` を使い、UI 側で `to_char_ranges` の各範囲の矩形を**自前で塗る**ことは可能 (描画だけなら egui 改造不要) |
// | キャレットごとの undo | undo は本文まるごと | 一括編集を 1 undo として積む |
//
// つまり「描画の足し込み (自前で矩形とキャレットを塗る)」までは今の egui でも到達でき、
// 「入力の分配」だけが `TextEdit` を自前実装に置き換えるまで残る。

use std::ops::Range;

/// 複数キャレット (と、その選択範囲) の集合。
///
/// 中身は**バイト範囲**で、常に次の不変条件を保つ (すべての生成・編集の後で成立):
///
/// 1. `start <= end`
/// 2. 開始位置の昇順に整列
/// 3. 重なりなし — 重なった範囲は 1 つに融合する
/// 4. 端点は必ず UTF-8 の文字境界 (本文を渡す生成関数を通した場合)
///
/// 隣接するだけ (前の `end` == 次の `start`) の**空でない**範囲は融合しない。
/// VS Code も `[0,3)` と `[3,6)` を別々のキャレットとして扱う。
/// ただし片方が空キャレットの場合は融合する (範囲の端に居る空キャレットは
/// その範囲のキャレットと同じ位置を指しているだけなので、残すと二重になる)。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultiSel {
    carets: Vec<Range<usize>>,
}

impl MultiSel {
    /// バイト範囲から作る。整列・融合は自動 (文字境界の検査はしない)。
    /// 本文が手元にあるなら [`MultiSel::in_text`] を使うこと。
    pub fn new(ranges: impl IntoIterator<Item = Range<usize>>) -> Self {
        let mut s = Self {
            carets: ranges.into_iter().collect(),
        };
        s.normalize();
        s
    }

    /// 本文に対して作る。本文長へクランプし、端点を文字境界へ寄せてから整列・融合する。
    /// 開始は手前へ、終了は後ろへ寄せる (寄せて範囲が消えないように)。
    pub fn in_text(text: &str, ranges: impl IntoIterator<Item = Range<usize>>) -> Self {
        let carets = ranges
            .into_iter()
            .map(|r| {
                let (lo, hi) = if r.start <= r.end {
                    (r.start, r.end)
                } else {
                    (r.end, r.start)
                };
                snap_char_boundary(text, lo)..snap_boundary_up(text, hi)
            })
            .collect::<Vec<_>>();
        Self::new(carets)
    }

    /// egui 側の **char** インデックス範囲から作る (`CCursorRange` はこちら)。
    pub fn from_char_ranges(text: &str, ranges: impl IntoIterator<Item = Range<usize>>) -> Self {
        Self::in_text(
            text,
            ranges.into_iter().map(|r| {
                char_to_byte(text, r.start.min(r.end))..char_to_byte(text, r.end.max(r.start))
            }),
        )
    }

    /// 整列済みのバイト範囲。
    pub fn carets(&self) -> &[Range<usize>] {
        &self.carets
    }

    pub fn len(&self) -> usize {
        self.carets.len()
    }

    pub fn is_empty(&self) -> bool {
        self.carets.is_empty()
    }

    /// この範囲がそのまま含まれているか (⌘D が同じ場所を二度拾わないための判定)。
    pub fn contains_range(&self, r: &Range<usize>) -> bool {
        self.carets.iter().any(|c| c == r)
    }

    /// 単一キャレットの UI へ返す 1 本。**最後 (最も後ろ) の範囲**を返す。
    ///
    /// [`add_cursor_below`] / [`select_next_occurrence`] は新しいキャレットが
    /// 後ろに付くのでこれで「増えた側」が見える。[`add_cursor_above`] のように
    /// 前へ伸びるコマンドでは `carets().first()` を使うとよい。
    /// 空集合のときは `0..0`。
    pub fn to_single_selection(&self) -> Range<usize> {
        self.carets.last().cloned().unwrap_or(0..0)
    }

    /// 全範囲を char インデックスへ直す (egui へ戻す用)。
    pub fn to_char_ranges(&self, text: &str) -> Vec<Range<usize>> {
        self.carets
            .iter()
            .map(|r| byte_to_char(text, r.start)..byte_to_char(text, r.end))
            .collect()
    }

    /// [`to_single_selection`] の char インデックス版。
    pub fn to_single_selection_chars(&self, text: &str) -> Range<usize> {
        let r = self.to_single_selection();
        byte_to_char(text, r.start)..byte_to_char(text, r.end)
    }

    /// 各範囲が指す本文。空キャレットは空文字列になる。
    pub fn slices<'a>(&self, text: &'a str) -> Vec<&'a str> {
        self.carets
            .iter()
            .map(|r| &text[r.start.min(text.len())..r.end.min(text.len())])
            .collect()
    }

    /// 不変条件を回復する。生成・編集のたびに必ず通す。
    fn normalize(&mut self) {
        for r in self.carets.iter_mut() {
            if r.start > r.end {
                *r = r.end..r.start;
            }
        }
        self.carets
            .sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
        let mut out: Vec<Range<usize>> = Vec::with_capacity(self.carets.len());
        for r in std::mem::take(&mut self.carets) {
            match out.last_mut() {
                // 重なっている / 片方が空キャレットで端点が一致 → 融合
                Some(p)
                    if r.start < p.end
                        || (r.start == p.end && (r.start == r.end || p.start == p.end)) =>
                {
                    p.end = p.end.max(r.end);
                }
                _ => out.push(r),
            }
        }
        self.carets = out;
    }
}

/// バイト位置を文字境界まで**後ろへ**寄せる ([`snap_char_boundary`] の逆向き)。
fn snap_boundary_up(s: &str, byte: usize) -> usize {
    let mut byte = byte.min(s.len());
    while byte < s.len() && !s.is_char_boundary(byte) {
        byte += 1;
    }
    byte
}

/// 行内のバイト位置 `byte` (行頭からの相対) の**表示桁** (0 起点)。
///
/// タブは次の `tab_width` の倍数まで進む。**タブ以外は全て 1 桁**として数える
/// (VS Code の「桁」の定義。全角文字を 2 桁と数えないので、日本語の行でも
/// 「上下のカーソル追加」が文字数どおりに並ぶ)。
fn visual_col_in_line(line: &str, byte: usize, tab_width: usize) -> usize {
    let tw = tab_width.max(1);
    let mut col = 0usize;
    for (b, c) in line.char_indices() {
        if b >= byte {
            break;
        }
        col += if c == '\t' { tw - (col % tw) } else { 1 };
    }
    col
}

/// 表示桁 `col` に当たる行内バイト位置 (行頭からの相対)。
///
/// - 行がその桁まで届かない → 行末を返す (短い行では行末に寄る = VS Code と同じ)。
/// - タブの途中に当たった → 近い方の端へ丸める (タブを割らない)。
fn byte_at_visual_col(line: &str, col: usize, tab_width: usize) -> usize {
    let tw = tab_width.max(1);
    let mut cur = 0usize;
    for (b, c) in line.char_indices() {
        if col <= cur {
            return b;
        }
        let next = cur + if c == '\t' { tw - (cur % tw) } else { 1 };
        if col < next {
            // 文字の途中 (タブの内側) — 近い端へ寄せる
            return if col - cur <= next - col {
                b
            } else {
                b + c.len_utf8()
            };
        }
        cur = next;
    }
    line.len()
}

/// バイト位置を含む行のインデックス。`segs` は [`line_segments`] の結果。
fn line_index_of_byte(segs: &[(usize, usize, usize)], byte: usize) -> usize {
    segs.partition_point(|(s, _, _)| *s <= byte)
        .saturating_sub(1)
}

/// 各キャレットの**真上**の行に、同じ表示桁のキャレットを足す (VS Code の
/// 「カーソルを上に追加」)。
///
/// VS Code と同じく「既存のキャレット全部 + それぞれの 1 行上」を集合として返す。
/// 重複は [`MultiSel`] の融合規則で消えるので、繰り返し呼ぶと上へ 1 行ずつ伸びる。
/// 最上行のキャレットからは何も増えない。桁は**タブ展開後の表示桁**で保つので、
/// タブ幅の違う行へ移っても見た目の位置が揃う。短い行では行末に寄る。
///
/// 空集合を渡すと空集合が返る (UI は現在のキャレットを必ず種にすること)。
///
/// **sticky column について**: 短い行を通り抜けたあとも元の桁へ戻る VS Code の
/// 挙動が要るなら、押し始めの桁を [`visual_column_of`] で取って
/// [`add_cursor_above_at`] へ渡し続けること (VS Code もカーソル状態として
/// 桁を覚えている。純関数のこちらでは覚えようがない)。
pub fn add_cursor_above(text: &str, sel: &MultiSel, tab_width: usize) -> MultiSel {
    add_cursor_vertical(text, sel, tab_width, true, None)
}

/// [`add_cursor_above`] の下向き。最終行のキャレットからは何も増えない。
pub fn add_cursor_below(text: &str, sel: &MultiSel, tab_width: usize) -> MultiSel {
    add_cursor_vertical(text, sel, tab_width, false, None)
}

/// sticky column を明示する [`add_cursor_above`]。
/// `desired_col` が `Some` なら、途中に短い行があってもその桁へ戻る。
pub fn add_cursor_above_at(
    text: &str,
    sel: &MultiSel,
    tab_width: usize,
    desired_col: Option<usize>,
) -> MultiSel {
    add_cursor_vertical(text, sel, tab_width, true, desired_col)
}

/// sticky column を明示する [`add_cursor_below`]。
pub fn add_cursor_below_at(
    text: &str,
    sel: &MultiSel,
    tab_width: usize,
    desired_col: Option<usize>,
) -> MultiSel {
    add_cursor_vertical(text, sel, tab_width, false, desired_col)
}

/// 本文のバイト位置 `byte` の**表示桁** (0 起点)。sticky column の種を取るのに使う。
pub fn visual_column_of(text: &str, byte: usize, tab_width: usize) -> usize {
    let (ls, le) = line_bounds(text, byte);
    visual_col_in_line(&text[ls..le], byte.min(le).saturating_sub(ls), tab_width)
}

fn add_cursor_vertical(
    text: &str,
    sel: &MultiSel,
    tab_width: usize,
    up: bool,
    desired_col: Option<usize>,
) -> MultiSel {
    if sel.is_empty() {
        return sel.clone();
    }
    let segs = line_segments(text);
    let mut out: Vec<Range<usize>> = sel.carets().to_vec();
    for r in sel.carets() {
        // 上へ伸ばすときは範囲の上端、下へ伸ばすときは下端を基準にする
        let anchor = if up { r.start } else { r.end };
        let li = line_index_of_byte(&segs, anchor);
        let target = if up {
            li.checked_sub(1)
        } else {
            (li + 1 < segs.len()).then_some(li + 1)
        };
        let Some(ti) = target else { continue };
        let (ls, le, _) = segs[li];
        let col = desired_col.unwrap_or_else(|| {
            visual_col_in_line(&text[ls..le], anchor.saturating_sub(ls), tab_width)
        });
        let (ts, te, _) = segs[ti];
        let b = ts + byte_at_visual_col(&text[ts..te], col, tab_width);
        out.push(b..b);
    }
    MultiSel::in_text(text, out)
}

/// 検索語の当て方。[`select_all_occurrences`] / [`select_next_occurrence`] 共通。
///
/// 実体は `file_search::Matcher` に委譲する (「ファイル間で検索」と**同じ**
/// 一致規則 — 大文字小文字の畳み込みも単語境界の定義も 1 か所しかない)。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MatchOpts {
    pub case_sensitive: bool,
    /// 前後が単語文字でない箇所だけ拾う (`file_search` と同じ定義)。
    pub whole_word: bool,
    /// `needle` を正規表現として解釈する。
    /// **注意**: 本文全体を 1 つの入力として当てるので `^` / `$` は
    /// 「バッファの先頭 / 末尾」であって行頭 / 行末ではない (`(?m)` を付ければ行単位になる)。
    pub regex: bool,
}

fn compile_matcher(needle: &str, opts: MatchOpts) -> Option<crate::file_search::Matcher> {
    let so = crate::file_search::SearchOptions {
        query: needle.to_string(),
        case_sensitive: opts.case_sensitive,
        whole_word: opts.whole_word,
        regex: opts.regex,
        ..crate::file_search::SearchOptions::default()
    };
    crate::file_search::Matcher::compile(&so).ok()
}

/// 本文中の `needle` の出現を**全部**選択する (VS Code の「全ての出現を選択」)。
///
/// 一致しない / パターンが壊れているときは空集合。
pub fn select_all_occurrences(text: &str, needle: &str, opts: MatchOpts) -> MultiSel {
    let Some(m) = compile_matcher(needle, opts) else {
        return MultiSel::default();
    };
    MultiSel::in_text(text, m.find_all(text).into_iter().map(|(s, e)| s..e))
}

/// `sel` に「次の出現」を 1 つだけ足す (VS Code の ⌘D)。
///
/// - 探し始めるのは**最後のキャレットの終端**。そこから後ろに無ければ先頭へ回り込む。
/// - 既に集合に入っている範囲は飛ばす。全部入っていれば `sel` をそのまま返す
///   (押し続けても増えなくなる = VS Code と同じ止まり方)。
pub fn select_next_occurrence(
    text: &str,
    sel: &MultiSel,
    needle: &str,
    opts: MatchOpts,
) -> MultiSel {
    let Some(m) = compile_matcher(needle, opts) else {
        return sel.clone();
    };
    let all: Vec<Range<usize>> = m.find_all(text).into_iter().map(|(s, e)| s..e).collect();
    if all.is_empty() {
        return sel.clone();
    }
    let from = sel.carets().last().map(|r| r.end).unwrap_or(0);
    // from 以降 → 先頭から、の順に「まだ選ばれていない出現」を探す
    let next = all
        .iter()
        .find(|r| r.start >= from && !sel.contains_range(r))
        .or_else(|| all.iter().find(|r| !sel.contains_range(r)));
    match next {
        Some(r) => MultiSel::in_text(text, sel.carets().iter().cloned().chain([r.clone()])),
        None => sel.clone(),
    }
}

/// 矩形 (列) 選択。行 × 表示桁の長方形を、行ごとの範囲の集合に変換する。
///
/// - 行・桁は 0 起点。順序は問わない (逆向きに指定しても同じ矩形になる)。
/// - 桁は**タブ展開後の表示桁**。タブの途中に辺が来たら近い端へ丸める
///   (タブを割ったバイト位置は作らない)。
/// - 矩形より短い行は**行末の空キャレット**になる (VS Code と同じ。行を飛ばさないので
///   「各行の同じ桁に挿入」が短い行でも効く)。
/// - 幅 0 の矩形は各行の空キャレット = 「複数行の先頭にカーソルを立てる」になる。
/// - 行番号は本文の行数へクランプする。
pub fn column_selection(
    text: &str,
    anchor_line: usize,
    anchor_col: usize,
    head_line: usize,
    head_col: usize,
    tab_width: usize,
) -> MultiSel {
    let segs = line_segments(text);
    let last = segs.len() - 1;
    let (lo_line, hi_line) = if anchor_line <= head_line {
        (anchor_line, head_line)
    } else {
        (head_line, anchor_line)
    };
    let (lo_line, hi_line) = (lo_line.min(last), hi_line.min(last));
    let (lo_col, hi_col) = if anchor_col <= head_col {
        (anchor_col, head_col)
    } else {
        (head_col, anchor_col)
    };
    let mut out = Vec::with_capacity(hi_line - lo_line + 1);
    for (ls, le, _) in segs[lo_line..=hi_line].iter().copied() {
        let line = &text[ls..le];
        let s = ls + byte_at_visual_col(line, lo_col, tab_width);
        let e = ls + byte_at_visual_col(line, hi_col, tab_width);
        out.push(s..e);
    }
    MultiSel::in_text(text, out)
}

/// 全キャレットの範囲を `f` の返す文字列で置き換える。**後ろから前へ**適用する。
///
/// 後ろから当てるので、まだ適用していない (= より手前の) 範囲のバイト位置が
/// ずれない。`f` には**編集前**の本文の当該範囲がそのまま渡る (空キャレットなら `""`)。
///
/// 返る [`MultiSel`] は**編集後の本文**に対する範囲で、挿入した文字列を覆う。
/// 不変条件 (整列・重なりなし・文字境界) は再び成立する。
pub fn apply_edit_to_all<F>(text: &str, sel: &MultiSel, mut f: F) -> (String, MultiSel)
where
    F: FnMut(&str) -> String,
{
    let reps: Vec<(Range<usize>, String)> = sel
        .carets()
        .iter()
        .map(|r| {
            let (s, e) = (r.start.min(text.len()), r.end.min(text.len()));
            (s..e, f(&text[s..e]))
        })
        .collect();
    let mut out = text.to_string();
    // ── 後ろから前へ。前の範囲のバイト位置は一切動かない ──
    for (r, new) in reps.iter().rev() {
        out.replace_range(r.clone(), new);
    }
    // 新しい範囲は前から累積差分で求める (置換後の本文に対する位置)
    let mut delta: isize = 0;
    let mut carets = Vec::with_capacity(reps.len());
    for (r, new) in reps.iter() {
        let start = (r.start as isize + delta) as usize;
        carets.push(start..start + new.len());
        delta += new.len() as isize - (r.end - r.start) as isize;
    }
    let sel = MultiSel::new(carets);
    (out, sel)
}

/// 全キャレットの**手前**に `ins` を挿入する (既存の選択内容は消さない)。
///
/// 返る範囲は挿入文字列を含まない = 元の選択内容がそのまま選ばれたまま後ろへずれる。
/// 空キャレットなら挿入直後の空キャレットになる (「そこにタイプした」形)。
pub fn insert_at_all(text: &str, sel: &MultiSel, ins: &str) -> (String, MultiSel) {
    let (out, moved) = apply_edit_to_all(text, sel, |old| format!("{ins}{old}"));
    let carets = moved
        .carets()
        .iter()
        .map(|r| (r.start + ins.len())..r.end)
        .collect::<Vec<_>>();
    (out, MultiSel::new(carets))
}

/// 全キャレットの選択内容を消す。空キャレットは何も消さない
/// (Backspace 相当が要るなら [`apply_edit_to_all`] で範囲を作ってから呼ぶ)。
pub fn delete_at_all(text: &str, sel: &MultiSel) -> (String, MultiSel) {
    apply_edit_to_all(text, sel, |_| String::new())
}

/// 全キャレットの選択内容を `rep` に置き換える (「全ての出現を選択 → まとめて置換」)。
pub fn replace_all_ranges(text: &str, sel: &MultiSel, rep: &str) -> (String, MultiSel) {
    apply_edit_to_all(text, sel, |_| rep.to_string())
}

// ═══════════════ インデントの推定と変換 (VS Code 相当) ═══════════════
//
// VS Code の `editor.detectIndentation` は「開いたファイルの中身から
// タブ / スペースと 1 段の桁数を当てる」機能。ステータスバーの
// 「スペース: 4」表示と、そこからの切り替えがこの型を通る。

/// インデントの様式。`width` はタブのときも意味を持つ
/// (タブ 1 個を何桁として**表示**するか)。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndentStyle {
    /// タブでインデントしているか (false = スペース)。
    pub tabs: bool,
    /// 1 段ぶんの桁数。必ず 1..=[`MAX_INDENT_WIDTH`] ([`IndentStyle::new`] が丸める)。
    pub width: usize,
}

/// 推定・設定で許す最大の桁数。9 桁以上のインデントは実在しないので
/// 候補から外す (「たまたま 12 桁ずれた行」を 1 段と誤認しないため)。
pub const MAX_INDENT_WIDTH: usize = 8;

impl IndentStyle {
    /// 何も判らないときの桁数 (VS Code の `editor.tabSize` 既定と同じ)。
    pub const DEFAULT_WIDTH: usize = 4;

    /// 幅を 1..=[`MAX_INDENT_WIDTH`] に丸めて作る。
    pub fn new(tabs: bool, width: usize) -> Self {
        Self {
            tabs,
            width: width.clamp(1, MAX_INDENT_WIDTH),
        }
    }

    /// 1 段ぶんの実体 (タブなら `"\t"`、スペースなら幅ぶんの空白)。
    pub fn unit(&self) -> String {
        if self.tabs {
            "\t".to_string()
        } else {
            " ".repeat(self.width)
        }
    }
}

impl Default for IndentStyle {
    /// 何も判らないときの既定。VS Code の `editor.tabSize` 既定と同じ 4 スペース。
    fn default() -> Self {
        Self {
            tabs: false,
            width: Self::DEFAULT_WIDTH,
        }
    }
}

/// 行頭の空白を「(タブ数, スペース数, 空白の終わりのバイト位置)」で返す。
///
/// タブとスペースが混ざっていても順序どおり数えるだけで、判定はしない。
/// 空白しか無い行 (= 本文が無い行) は `None`。
fn leading_ws(line: &str) -> Option<(usize, usize, usize)> {
    let mut tabs = 0usize;
    let mut spaces = 0usize;
    let mut end = 0usize;
    for c in line.chars() {
        match c {
            '\t' => tabs += 1,
            ' ' => spaces += 1,
            _ => return Some((tabs, spaces, end)),
        }
        end += c.len_utf8();
    }
    // 行末まで空白だけ = 本文が無い行
    None
}

/// 行頭の空白を「桁」に直す (タブは `tab_width` 桁のタブストップ)。
fn leading_columns(line: &str, tab_width: usize) -> usize {
    let tw = tab_width.max(1);
    let mut col = 0usize;
    for c in line.chars() {
        match c {
            ' ' => col += 1,
            '\t' => col = (col / tw + 1) * tw,
            _ => break,
        }
    }
    col
}

/// ブロックコメントの継続行 (`* …`) か。
///
/// JSDoc / rustdoc の `*` 行は本文より 1 桁だけ深いので、素直に数えると
/// 「1 桁インデント」が最頻値になってしまう。VS Code も同じ理由で外す。
fn is_block_comment_cont(line: &str) -> bool {
    line.trim_start().starts_with('*')
}

/// 本文からインデントの様式を推定する (VS Code の `editor.detectIndentation`)。
///
/// 判らないときは `fallback` をそのまま返すので、**必ず何かを返す**
/// (推定に失敗しても設定値で動き続ける)。手順:
///
/// 1. 行頭がタブで始まる行とスペースで始まる行を数え、多いほうを採る。
/// 2. スペースなら、隣り合う行のインデント桁の**差**を集計して最頻値を幅にする。
///    差が一度も出なければ (全行が同じ深さ) 最小のインデント桁を使う。
/// 3. どちらの証拠も無ければ `fallback`。
///
/// 空白だけの行とブロックコメントの継続行 (`* …`) は数えない。
pub fn detect_indent(text: &str, fallback: IndentStyle) -> IndentStyle {
    let mut tab_lines = 0usize;
    let mut space_lines = 0usize;
    // スペース字下げの桁数 (行順)。`usize::MAX` は「タブ字下げの行」の目印で、
    // ここで統計を切る (混在ファイルでタブ行を跨いだ差を数えないため)。
    let mut cols: Vec<usize> = Vec::new();
    for raw in text.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let Some((tabs, spaces, _)) = leading_ws(line) else {
            continue;
        };
        if is_block_comment_cont(line) {
            continue;
        }
        if tabs > 0 {
            tab_lines += 1;
            cols.push(usize::MAX);
            continue;
        }
        if spaces > 0 {
            space_lines += 1;
        }
        cols.push(spaces);
    }
    if tab_lines == 0 && space_lines == 0 {
        return fallback;
    }
    if tab_lines > space_lines {
        return IndentStyle::new(true, fallback.width);
    }
    // 隣り合う行の差を集計 (0 と 9 桁以上は無視)
    let mut hist = [0usize; MAX_INDENT_WIDTH + 1];
    let mut prev: Option<usize> = None;
    for c in &cols {
        if *c == usize::MAX {
            prev = None;
            continue;
        }
        if let Some(p) = prev {
            let d = c.abs_diff(p);
            if (1..=MAX_INDENT_WIDTH).contains(&d) {
                hist[d] += 1;
            }
        }
        prev = Some(*c);
    }
    // 最頻値。同数なら小さいほうを採る (2 と 4 が並んだら「2 スペースを
    // 2 段」のほうが実在しやすい)。
    let best = (1..=MAX_INDENT_WIDTH).fold((0usize, 0usize), |(bw, bn), w| {
        if hist[w] > bn {
            (w, hist[w])
        } else {
            (bw, bn)
        }
    });
    if best.1 > 0 {
        return IndentStyle::new(false, best.0);
    }
    // 差が一度も出なかった = 全行が同じ深さ。最小の字下げ桁を 1 段とみなす。
    match cols
        .iter()
        .filter(|c| **c != usize::MAX && **c > 0)
        .min()
        .copied()
    {
        Some(c) if c <= MAX_INDENT_WIDTH => IndentStyle::new(false, c),
        _ => fallback,
    }
}

/// 行頭のインデントだけを `from` の様式から `to` の様式へ書き換える。
///
/// 段数を保つ変換: `桁 / from.width` を段数、余りを桁ずれとして扱い、
/// `段数 * to.width + 余り` 桁を新しい様式で描き直す。これで
/// 「スペース 4 → スペース 2」が本当に半分になり、「タブ → スペース」も
/// 見た目どおりの桁数になる。行数・改行コード・本文は 1 文字も変えない。
pub fn convert_indentation(text: &str, from: IndentStyle, to: IndentStyle) -> String {
    if from == to {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    for (s, e, seg_end) in line_segments(text) {
        let line = &text[s..e];
        match leading_ws(line) {
            Some((_, _, ws_end)) => {
                let col = leading_columns(line, from.width);
                let level = col / from.width;
                let rem = col % from.width;
                let new_col = level * to.width + rem;
                if to.tabs {
                    out.push_str(&"\t".repeat(new_col / to.width));
                    out.push_str(&" ".repeat(new_col % to.width));
                } else {
                    out.push_str(&" ".repeat(new_col));
                }
                out.push_str(&line[ws_end..]);
            }
            // 空白だけの行は触らない (行末空白の除去は保存時の仕事)
            None => out.push_str(line),
        }
        out.push_str(&text[e..seg_end]);
    }
    out
}

// ═══════════════ 選択範囲への編集コマンド (VS Code 相当) ═══════════════

/// 大文字小文字の変換の種類。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaseKind {
    /// すべて大文字へ
    Upper,
    /// すべて小文字へ
    Lower,
    /// 単語の先頭だけ大文字へ (残りは小文字)
    Title,
}

/// 文字列の大文字小文字を変換する。
///
/// Unicode の規則に従うので、`ß` → `SS` のように**文字数が変わることがある**
/// (呼び出し側は選択範囲を「変換後の長さ」で取り直すこと)。
/// 日本語・絵文字はそのまま通る (大文字小文字を持たない)。
pub fn transform_case(s: &str, kind: CaseKind) -> String {
    match kind {
        CaseKind::Upper => s.to_uppercase(),
        CaseKind::Lower => s.to_lowercase(),
        CaseKind::Title => {
            let mut out = String::with_capacity(s.len());
            // 直前が「単語を構成する文字」だったか
            let mut in_word = false;
            for c in s.chars() {
                let is_word = c.is_alphanumeric() || c == '_';
                if is_word && !in_word {
                    out.extend(c.to_uppercase());
                } else if is_word {
                    out.extend(c.to_lowercase());
                } else {
                    out.push(c);
                }
                in_word = is_word;
            }
            out
        }
    }
}

/// 選択範囲 (char) を覆う「行の範囲」を char 添字で返す。
///
/// 選択が無い (start == end) ときはその 1 行。選択が行頭ちょうどで終わって
/// いるときは、その行を巻き込まない (VS Code と同じ)。
/// 返り値は `(行頭 char, 行末 char)` で、行末は**最後の行の改行の手前**。
fn line_span_of(text: &str, start_char: usize, end_char: usize) -> (usize, usize) {
    let (a, b) = (start_char.min(end_char), start_char.max(end_char));
    let sb = char_to_byte(text, a);
    let eb = char_to_byte(text, b);
    let segs = line_segments(text);
    let first = segs.iter().rposition(|&(s, _, _)| s <= sb).unwrap_or(0);
    let mut last = segs.iter().rposition(|&(s, _, _)| s <= eb).unwrap_or(first);
    if last > first && eb == segs[last].0 {
        last -= 1;
    }
    (
        byte_to_char(text, segs[first].0),
        byte_to_char(text, segs[last].1),
    )
}

/// 行単位の編集を選択範囲へ当てる共通の骨。
///
/// `f` は「選択が覆う行の並び」を受け取り、新しい行の並びを返す。
/// 改行コードは元の本文のものを使い回すので、CRLF のファイルが LF に化けない。
/// 返り値は `(新しい本文, 選択の開始 char, 選択の終わり char)` で、
/// 選択は書き換えた行の全体になる (VS Code と同じ)。
fn edit_lines<F>(text: &str, start_char: usize, end_char: usize, f: F) -> (String, usize, usize)
where
    F: FnOnce(&[&str]) -> Vec<String>,
{
    let (ls, le) = line_span_of(text, start_char, end_char);
    let sb = char_to_byte(text, ls);
    let eb = char_to_byte(text, le);
    let block = &text[sb..eb];
    // 元の本文で使われている改行を拾う (無ければ LF)
    let nl = if block.contains("\r\n") {
        "\r\n"
    } else if block.contains('\r') {
        "\r"
    } else {
        "\n"
    };
    let segs = line_segments(block);
    let rows: Vec<&str> = segs.iter().map(|&(s, e, _)| &block[s..e]).collect();
    let joined = f(&rows).join(nl);
    let mut out = String::with_capacity(text.len());
    out.push_str(&text[..sb]);
    out.push_str(&joined);
    out.push_str(&text[eb..]);
    let new_end = ls + joined.chars().count();
    (out, ls, new_end)
}

/// 選択範囲の行を並べ替える (`desc` が真なら降順)。
///
/// 比較はロケールに依存しないバイト列順 = どの環境でも同じ結果になる。
/// 選択が 1 行なら並べ替えるものが無いので本文は変わらない。
pub fn sort_lines(
    text: &str,
    start_char: usize,
    end_char: usize,
    desc: bool,
) -> (String, usize, usize) {
    edit_lines(text, start_char, end_char, |rows| {
        let mut v: Vec<String> = rows.iter().map(|r| r.to_string()).collect();
        v.sort();
        if desc {
            v.reverse();
        }
        v
    })
}

/// 選択範囲から重複行を削る (最初の 1 本だけ残す。並び順は変えない)。
pub fn dedupe_lines(text: &str, start_char: usize, end_char: usize) -> (String, usize, usize) {
    edit_lines(text, start_char, end_char, |rows| {
        let mut seen = std::collections::HashSet::new();
        rows.iter()
            .filter(|r| seen.insert(r.to_string()))
            .map(|r| r.to_string())
            .collect()
    })
}

/// 文字列を JSON として整形する。
///
/// パースできなければ `Err` にメッセージを返す (本文には触らない) —
/// 壊れた JSON を黙って書き換えると、元に戻せない事故になるため。
/// インデントは呼び出し側が [`IndentStyle::unit`] から渡す
/// (エディタの設定と違う字下げで整形しない)。
pub fn format_json(src: &str, unit: &str) -> Result<String, String> {
    let v: serde_json::Value = serde_json::from_str(src.trim()).map_err(|e| e.to_string())?;
    let fmt = serde_json::ser::PrettyFormatter::with_indent(unit.as_bytes());
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, fmt);
    serde::Serialize::serialize(&v, &mut ser).map_err(|e| e.to_string())?;
    String::from_utf8(buf).map_err(|e| e.to_string())
}

#[cfg(test)]
// `MultiSel` は範囲の**集合**を受け取るので、要素 1 つの配列リテラル `[a..b]` は
// 意図どおり (clippy はベクタ初期化との取り違えを疑って警告するが、ここでは誤検知)。
#[allow(clippy::single_range_in_vec_init)]
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

    // ---- matching_bracket ----

    #[test]
    fn bracket_forward_and_backward() {
        //            0123456789
        let text = "fn f(a, b) {}";
        assert_eq!(matching_bracket(text, 4), Some(9)); // カーソルが ( の直前
        assert_eq!(matching_bracket(text, 10), Some(4)); // ) の直後 → 相手の (
        assert_eq!(matching_bracket(text, 11), Some(12)); // { → }
    }

    #[test]
    fn bracket_nested_pairs() {
        let text = "((a)[b])";
        assert_eq!(matching_bracket(text, 0), Some(7));
        assert_eq!(matching_bracket(text, 1), Some(3));
        assert_eq!(matching_bracket(text, 4), Some(6));
    }

    #[test]
    fn bracket_none_when_not_on_bracket_or_unbalanced() {
        assert_eq!(matching_bracket("abc", 1), None);
        assert_eq!(matching_bracket("(abc", 0), None);
        assert_eq!(matching_bracket("", 0), None);
    }

    #[test]
    fn bracket_multibyte_safe() {
        let text = "「(あ)」";
        assert_eq!(matching_bracket(text, 1), Some(3));
    }

    // ---- find_ci / replace_all_ci ----

    #[test]
    fn find_ci_wraps_and_ignores_case() {
        assert_eq!(find_ci("Hello World", "world", 0), Some(6));
        // start 以降に無ければ先頭へ戻る
        assert_eq!(find_ci("Hello World", "hello", 6), Some(0));
        assert_eq!(find_ci("abc", "zzz", 0), None);
        assert_eq!(find_ci("abc", "", 0), None);
    }

    #[test]
    fn replace_all_ci_counts_and_replaces() {
        let (t, n) = replace_all_ci("foo Foo FOO bar", "foo", "x");
        assert_eq!(t, "x x x bar");
        assert_eq!(n, 3);
    }

    #[test]
    fn replace_all_ci_rep_containing_query_terminates() {
        let (t, n) = replace_all_ci("aaa", "a", "aa");
        assert_eq!(t, "aaaaaa");
        assert_eq!(n, 3);
    }

    #[test]
    fn replace_all_ci_japanese() {
        let (t, n) = replace_all_ci("こんにちは世界。世界!", "世界", "World");
        assert_eq!(t, "こんにちはWorld。World!");
        assert_eq!(n, 2);
    }

    #[test]
    fn find_ci_exotic_case_folding_does_not_panic() {
        // İ (U+0130) は小文字化で 2→3 バイト、Ω (U+2126 OHM SIGN) は 3→2 バイト。
        // 両方を含むと「合計バイト長は同じなのに char 境界がずれる」状態になり、
        // 以前は text[..byte_pos] が char 境界 panic を起こしていた (回帰テスト)。
        let text = "\u{130}\u{2126} abc ABC"; // İΩ abc ABC
        assert!(find_ci(text, "abc", 0).is_some());
        // ずれた写し上のヒットでも panic しない (途中の char 境界計算を通す)
        let _ = find_ci(text, "\u{130}", 0);
        let _ = find_ci(text, "\u{3c9}", 1); // ω
        let _ = find_ci(text, "ABC", 0);
        // 境界がずれても後続の ASCII は見つかる
        assert_eq!(find_ci("\u{130}\u{2126}x", "x", 0), Some(2));
    }

    #[test]
    fn replace_all_ci_exotic_case_folding_does_not_panic() {
        let text = "\u{130}\u{2126} abc ABC"; // İΩ abc ABC
        let (t, n) = replace_all_ci(text, "abc", "x");
        assert_eq!(n, 2, "境界ずれ文字が混ざっても両方置換される: {t:?}");
        assert!(
            t.starts_with("\u{130}\u{2126}"),
            "対象外の部分は保たれる: {t:?}"
        );
        // ヒットなしでも本文が壊れない
        let (t, n) = replace_all_ci("\u{130}\u{2126}", "zzz", "x");
        assert_eq!((t.as_str(), n), ("\u{130}\u{2126}", 0));
    }

    #[test]
    fn line_of_char_counts_newlines() {
        assert_eq!(line_of_char("a\nb\nc", 0), 0);
        assert_eq!(line_of_char("a\nb\nc", 2), 1);
        assert_eq!(line_of_char("a\nb\nc", 4), 2);
    }

    // ---- char_index_at / parse_goto ----

    #[test]
    fn char_index_at_clamps_line_and_col() {
        let text = "ab\nこんにちは\nxyz";
        assert_eq!(char_index_at(text, 0, 0), 0);
        assert_eq!(char_index_at(text, 1, 2), 5); // "ab\n" = 3 chars + 2
        assert_eq!(char_index_at(text, 1, 99), 8); // 行末へクランプ
        assert_eq!(char_index_at(text, 99, 0), 9); // 最終行へクランプ
    }

    #[test]
    fn parse_goto_line_and_col() {
        assert_eq!(parse_goto("42"), Some((41, 0)));
        assert_eq!(parse_goto("42:5"), Some((41, 4)));
        assert_eq!(parse_goto(" 7 , 3 "), Some((6, 2)));
        assert_eq!(parse_goto("1:"), Some((0, 0)));
        assert_eq!(parse_goto(""), None);
        assert_eq!(parse_goto("abc"), None);
        assert_eq!(parse_goto("0"), Some((0, 0))); // 0 は 1 行目扱い
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

    // ---- 括弧・引用符の自動ペア ----

    #[test]
    fn pair_insert_at_end_and_before_whitespace() {
        // 行末 → 自動閉じ
        assert_eq!(
            pair_on_type("let x = ", 8, 8, '('),
            Some(PairEdit::Insert {
                text: "let x = ()".into(),
                cursor: 9
            })
        );
        // 直後が空白 → 自動閉じ
        assert_eq!(
            pair_on_type("f x", 1, 1, '('),
            Some(PairEdit::Insert {
                text: "f() x".into(),
                cursor: 2
            })
        );
        // 直後が英数字 → 割り込まない (通常入力)
        assert_eq!(pair_on_type("fx", 1, 1, '('), None);
        // 直後が閉じ括弧 → 自動閉じ (ネスト)
        assert_eq!(
            pair_on_type("f()", 2, 2, '['),
            Some(PairEdit::Insert {
                text: "f([])".into(),
                cursor: 3
            })
        );
    }

    #[test]
    fn pair_skip_over_existing_closer() {
        assert_eq!(
            pair_on_type("f()", 2, 2, ')'),
            Some(PairEdit::SkipOver { cursor: 3 })
        );
        assert_eq!(
            pair_on_type("s\"\"", 2, 2, '"'),
            Some(PairEdit::SkipOver { cursor: 3 })
        );
    }

    #[test]
    fn pair_surround_selection() {
        // "abc" の bc を選択して ( → a(bc)
        assert_eq!(
            pair_on_type("abc", 1, 3, '('),
            Some(PairEdit::Surround {
                text: "a(bc)".into(),
                select: (2, 4)
            })
        );
        // 引用符でも囲める。閉じ文字では囲まない
        assert_eq!(
            pair_on_type("abc", 0, 3, '"'),
            Some(PairEdit::Surround {
                text: "\"abc\"".into(),
                select: (1, 4)
            })
        );
        assert_eq!(pair_on_type("abc", 1, 3, ')'), None);
    }

    #[test]
    fn pair_quote_not_after_word() {
        // don't のアポストロフィ: 単語直後の引用符は自動閉じしない
        assert_eq!(pair_on_type("don", 3, 3, '\''), None);
        // 空白の後なら自動閉じ
        assert_eq!(
            pair_on_type("x ", 2, 2, '\''),
            Some(PairEdit::Insert {
                text: "x ''".into(),
                cursor: 3
            })
        );
    }

    #[test]
    fn pair_backspace_deletes_empty_pair() {
        assert_eq!(pair_on_backspace("f()", 2), Some(("f".into(), 1)));
        assert_eq!(pair_on_backspace("\"\"", 1), Some((String::new(), 0)));
        // ペアでなければ通常の Backspace
        assert_eq!(pair_on_backspace("f(x)", 2), None);
        assert_eq!(pair_on_backspace("", 0), None);
        // マルチバイト安全
        assert_eq!(pair_on_backspace("あ()い", 2), Some(("あい".into(), 1)));
    }

    #[test]
    fn pair_multibyte_selection_surround() {
        assert_eq!(
            pair_on_type("日本語", 0, 3, '('),
            Some(PairEdit::Surround {
                text: "(日本語)".into(),
                select: (1, 4)
            })
        );
    }

    // ---- trim_trailing_whitespace ----

    #[test]
    fn trim_removes_spaces_and_tabs_at_line_end() {
        assert_eq!(trim_trailing_whitespace("a  \nb\t\t\nc   "), "a\nb\nc");
        // 行頭・行中の空白は触らない
        assert_eq!(
            trim_trailing_whitespace("    let x = 1;  \n"),
            "    let x = 1;\n"
        );
    }

    #[test]
    fn trim_keeps_crlf_and_cr_intact() {
        assert_eq!(trim_trailing_whitespace("a  \r\nb\t\r\n"), "a\r\nb\r\n");
        // CR だけのファイルも 1 行ずつ扱える (\r を本文と間違えて消さない)
        assert_eq!(trim_trailing_whitespace("a  \rb \r"), "a\rb\r");
    }

    #[test]
    fn trim_empties_whitespace_only_lines_without_changing_line_count() {
        let text = "a\n   \n\t\nb";
        let got = trim_trailing_whitespace(text);
        assert_eq!(got, "a\n\n\nb");
        assert_eq!(
            got.matches('\n').count(),
            text.matches('\n').count(),
            "改行を消してはいけない = 行数が変わらない"
        );
        // 末尾の「改行なしの空白だけの行」も空行として残る (行数は 2 のまま)
        let tail = trim_trailing_whitespace("a\n   ");
        assert_eq!(tail, "a\n");
        assert_eq!(tail.matches('\n').count(), 1);
    }

    #[test]
    fn trim_returns_an_identical_string_when_nothing_to_do() {
        let text = "fn main() {\n    println!();\n}\n";
        assert_eq!(trim_trailing_whitespace(text), text);
        assert_eq!(trim_trailing_whitespace(""), "");
        assert_eq!(trim_trailing_whitespace("\n\n"), "\n\n");
    }

    /// 全角スペース U+3000 は**落とさない**。日本語の本文では字下げや体裁として
    /// 意図して置かれる有意な文字で、保存のたびに消えると書いた内容が変わる。
    #[test]
    fn trim_keeps_ideographic_space_and_nbsp() {
        let text = "日本語　\n次の行　";
        assert_eq!(trim_trailing_whitespace(text), text, "全角スペースは残す");
        assert_eq!(
            trim_trailing_whitespace("x\u{a0}\n"),
            "x\u{a0}\n",
            "NBSP も残す"
        );
        // 全角スペースの後ろに付いた半角だけが落ちる
        assert_eq!(trim_trailing_whitespace("日本語　  \t\n"), "日本語　\n");
    }

    // ---- ensure_final_newline ----

    #[test]
    fn ensure_final_newline_adds_only_when_missing() {
        assert_eq!(ensure_final_newline("a", LineEnding::Lf), "a\n");
        assert_eq!(
            ensure_final_newline("a\n", LineEnding::Lf),
            "a\n",
            "二重にしない"
        );
        assert_eq!(ensure_final_newline("a\r\n", LineEnding::Crlf), "a\r\n");
        assert_eq!(ensure_final_newline("a\r", LineEnding::Cr), "a\r");
        assert_eq!(
            ensure_final_newline("", LineEnding::Lf),
            "",
            "空のファイルは空のまま"
        );
        // 空白だけの本文は「1 行書いてある」ので他の行と同じ扱い
        assert_eq!(ensure_final_newline("   ", LineEnding::Lf), "   \n");
        assert_eq!(
            ensure_final_newline("日本語", LineEnding::Crlf),
            "日本語\r\n"
        );
        // 混在は最多の様式で足す
        let mixed = crate::textenc::detect_line_ending("a\r\nb\r\nc\n");
        assert_eq!(ensure_final_newline("x", mixed), "x\r\n");
    }

    // ---- apply_save_cleanup ----

    #[test]
    fn save_cleanup_composes_trim_ending_and_final_newline() {
        let opts = SaveCleanup {
            trim_trailing: true,
            trim_final_newlines: false,
            final_newline: true,
            target_ending: Some(LineEnding::Crlf),
        };
        let (out, changed) = apply_save_cleanup_checked("a  \nb\t", &opts);
        assert_eq!(out, "a\r\nb\r\n");
        assert!(changed);
        assert_eq!(
            apply_save_cleanup("a  \nb\t", &opts),
            out,
            "短い版も同じ結果"
        );
    }

    #[test]
    fn save_cleanup_changed_flag_is_exact() {
        let all = SaveCleanup {
            trim_trailing: true,
            trim_final_newlines: false,
            final_newline: true,
            target_ending: None,
        };
        // 既に整っている本文は 1 バイトも変わらない
        let (out, changed) = apply_save_cleanup_checked("a\nb\n", &all);
        assert_eq!(out, "a\nb\n");
        assert!(!changed, "変わっていないなら書き込みを省けること");
        // 何もしない設定は本文を素通しする
        let noop = SaveCleanup::default();
        assert!(noop.is_noop());
        let (out, changed) = apply_save_cleanup_checked("a  \n", &noop);
        assert_eq!(out, "a  \n");
        assert!(!changed);
        // 空の本文は最終改行を足さないので変化なし
        assert_eq!(apply_save_cleanup_checked("", &all), (String::new(), false));
    }

    #[test]
    fn save_cleanup_final_newline_follows_the_existing_ending() {
        let opts = SaveCleanup {
            trim_trailing: false,
            trim_final_newlines: false,
            final_newline: true,
            target_ending: None,
        };
        // 変換先を指定しなければ、その本文で一番多い改行に合わせる
        assert_eq!(apply_save_cleanup("a\r\nb", &opts), "a\r\nb\r\n");
        assert_eq!(apply_save_cleanup("a\nb", &opts), "a\nb\n");
        // 改行コードだけ揃える (本文には触らない)
        let only_ending = SaveCleanup {
            trim_trailing: false,
            trim_final_newlines: false,
            final_newline: false,
            target_ending: Some(LineEnding::Lf),
        };
        assert_eq!(apply_save_cleanup("a\r\nb  \r\n", &only_ending), "a\nb  \n");
    }

    // ---- adjust_offset_after_cleanup ----

    #[test]
    fn adjust_offset_keeps_the_caret_where_the_user_sees_it() {
        let original = "abc   \ndef  \nghi";
        let cleaned = trim_trailing_whitespace(original);
        assert_eq!(cleaned, "abc\ndef\nghi");
        // (整形前 byte, 期待する整形後 byte, 説明)
        let cases = [
            (0, 0, "行頭"),
            (2, 2, "削られる前の位置はそのまま"),
            (3, 3, "削られる空白の直前 = 新しい行末"),
            (5, 3, "削られた空白の中 → 行末へ寄せる"),
            (6, 3, "改行の直前"),
            (7, 4, "次の行の行頭 (前の行が縮んだぶんずれる)"),
            (13, 8, "最終行の行頭"),
            (16, 11, "EOF は EOF のまま"),
        ];
        for (before, after, why) in cases {
            assert_eq!(
                adjust_offset_after_cleanup(original, &cleaned, before),
                after,
                "{why}"
            );
        }
        // 範囲外を渡しても落ちない
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 999),
            cleaned.len()
        );
    }

    #[test]
    fn adjust_offset_is_multibyte_safe() {
        let original = "日本語  \nあ";
        let cleaned = trim_trailing_whitespace(original);
        assert_eq!(cleaned, "日本語\nあ");
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 3),
            3,
            "日と本の間"
        );
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 9),
            9,
            "語の直後"
        );
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 10),
            9,
            "空白の中"
        );
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 12),
            10,
            "次の行の行頭"
        );
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 15),
            13,
            "EOF"
        );
        // どのバイト位置から呼んでも多バイト文字の途中には落ちない
        for off in 0..=original.len() {
            let got = adjust_offset_after_cleanup(original, &cleaned, off);
            assert!(
                cleaned.is_char_boundary(got),
                "byte {off} → {got} が文字境界でない"
            );
        }
    }

    #[test]
    fn adjust_offset_handles_line_ending_changes() {
        // CRLF → 行末の空白だけ消える: \r と \n の間にいてもそこへ付け替わる
        let original = "a  \r\nb";
        let cleaned = trim_trailing_whitespace(original);
        assert_eq!(cleaned, "a\r\nb");
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 4),
            2,
            "\\r と \\n の間"
        );
        assert_eq!(
            adjust_offset_after_cleanup(original, &cleaned, 5),
            3,
            "次の行の行頭"
        );
        // LF → CRLF で位置が増える向きも追随する
        let cleaned = normalize_to("a\nb", LineEnding::Crlf);
        assert_eq!(adjust_offset_after_cleanup("a\nb", &cleaned, 2), 3);
        // 最終改行を足した場合、カーソルは足した改行の手前に残る
        let cleaned = ensure_final_newline("a", LineEnding::Lf);
        assert_eq!(adjust_offset_after_cleanup("a", &cleaned, 1), 1);
        // 何も変わっていなければ位置も変わらない
        assert_eq!(adjust_offset_after_cleanup("a b", "a b", 2), 2);
    }

    #[test]
    fn adjust_char_index_matches_the_byte_version() {
        let original = "日本語  \nあ";
        let cleaned = trim_trailing_whitespace(original);
        // char 4 = 2 つ目の半角スペース → 削られたので行末 (char 3) へ
        assert_eq!(adjust_char_index_after_cleanup(original, &cleaned, 4), 3);
        assert_eq!(adjust_char_index_after_cleanup(original, &cleaned, 2), 2);
        assert_eq!(
            adjust_char_index_after_cleanup(original, &cleaned, 6),
            4,
            "次の行の行頭"
        );
        assert_eq!(
            adjust_char_index_after_cleanup(original, &cleaned, 99),
            cleaned.chars().count(),
            "範囲外は末尾へ"
        );
    }

    /// 保存経路をひととおり通す: 整形して、カーソルを付け替えて、結果が壊れないこと。
    #[test]
    fn save_cleanup_and_caret_adjust_work_together() {
        let original = "行1  \n行2\t\t\n";
        let opts = SaveCleanup {
            trim_trailing: true,
            trim_final_newlines: false,
            final_newline: true,
            target_ending: Some(LineEnding::Crlf),
        };
        let (cleaned, changed) = apply_save_cleanup_checked(original, &opts);
        assert!(changed);
        assert_eq!(cleaned, "行1\r\n行2\r\n");
        // 「行2」の \t の上にいたカーソルは行2の行末へ
        let caret = adjust_char_index_after_cleanup(original, &cleaned, 8);
        assert_eq!(caret, 6, "行1(2文字)+改行+行2(2文字) の直後");
        assert!(cleaned.is_char_boundary(char_to_byte(&cleaned, caret)));
    }

    // ──────────────── MultiSel: 不変条件 ────────────────

    /// [`MultiSel`] の不変条件をすべて検査する。生成・編集の**あと**で必ず通す。
    fn assert_invariants(text: &str, sel: &MultiSel, what: &str) {
        let cs = sel.carets();
        for (i, r) in cs.iter().enumerate() {
            assert!(r.start <= r.end, "{what}: 逆順の範囲 {r:?}");
            assert!(r.end <= text.len(), "{what}: 本文長を超える {r:?}");
            assert!(
                text.is_char_boundary(r.start) && text.is_char_boundary(r.end),
                "{what}: 文字境界でない {r:?}"
            );
            if i > 0 {
                let p = &cs[i - 1];
                assert!(p.start <= r.start, "{what}: 未整列 {p:?} {r:?}");
                assert!(p.end <= r.start, "{what}: 重なり {p:?} {r:?}");
                // 端点が一致するのは「両方とも空でない」ときだけ (融合規則)
                if p.end == r.start {
                    assert!(
                        p.start < p.end && r.start < r.end,
                        "{what}: 空キャレットが融合されていない {p:?} {r:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn multisel_sorts_and_merges_overlaps() {
        let text = "abcdefghij";
        let sel = MultiSel::in_text(text, [6..9, 0..3, 2..5]);
        // 0..3 と 2..5 は重なるので 0..5 に融合、6..9 は独立
        assert_eq!(sel.carets(), &[0..5, 6..9]);
        assert_invariants(text, &sel, "重なりの融合");
    }

    #[test]
    fn multisel_keeps_adjacent_nonempty_ranges_apart() {
        let text = "abcdefghij";
        let sel = MultiSel::in_text(text, [3..6, 0..3]);
        assert_eq!(
            sel.carets(),
            &[0..3, 3..6],
            "隣接するだけの範囲は別キャレット"
        );
        assert_invariants(text, &sel, "隣接");
    }

    #[test]
    fn multisel_absorbs_empty_caret_at_range_edge() {
        let text = "abcdefghij";
        assert_eq!(MultiSel::in_text(text, [0..3, 3..3]).carets(), &[0..3]);
        assert_eq!(MultiSel::in_text(text, [3..3, 3..6]).carets(), &[3..6]);
        assert_eq!(MultiSel::in_text(text, [3..3, 3..3]).carets(), &[3..3]);
    }

    #[test]
    fn multisel_reversed_range_is_normalized() {
        let text = "abcdefghij";
        // 逆順の範囲 (アンカーがヘッドより後ろ) も同じ選択として扱う
        let backwards = Range { start: 5, end: 2 };
        assert_eq!(MultiSel::in_text(text, [backwards]).carets(), &[2..5]);
    }

    #[test]
    fn multisel_snaps_to_char_boundaries_in_japanese() {
        let text = "日本語";
        // 1,2,4,5 は多バイト文字の途中。start は手前へ、end は後ろへ寄る
        let sel = MultiSel::in_text(text, [1..5]);
        assert_eq!(sel.carets(), &[0..6], "「日本」を覆う範囲へ寄る");
        assert_invariants(text, &sel, "文字境界");
    }

    #[test]
    fn multisel_char_range_round_trip() {
        let text = "あいうえお";
        let sel = MultiSel::from_char_ranges(text, [1..3]);
        assert_eq!(sel.carets(), &[3..9], "char 1..3 = byte 3..9");
        assert_eq!(sel.to_char_ranges(text), vec![1..3]);
        assert_eq!(sel.slices(text), vec!["いう"]);
        assert_eq!(sel.to_single_selection_chars(text), 1..3);
    }

    #[test]
    fn multisel_single_selection_fallback() {
        let text = "aXbXc";
        let sel = MultiSel::in_text(text, [1..2, 3..4]);
        assert_eq!(sel.to_single_selection(), 3..4, "最後の範囲");
        assert_eq!(
            MultiSel::default().to_single_selection(),
            0..0,
            "空なら 0..0"
        );
    }

    // ──────────────── カーソルを上/下に追加 ────────────────

    const RAGGED: &str = "abcdef\nab\nabcdefgh\n";

    #[test]
    fn add_cursor_below_clamps_on_short_line() {
        let sel = MultiSel::in_text(RAGGED, [4..4]); // 1行目の桁4
        let got = add_cursor_below(RAGGED, &sel, 4);
        // 2行目 "ab" は桁4まで無いので行末 (byte 9) に寄る
        assert_eq!(got.carets(), &[4..4, 9..9]);
        assert_invariants(RAGGED, &got, "下に追加");
    }

    #[test]
    fn add_cursor_below_grows_by_one_each_press() {
        let mut sel = MultiSel::in_text(RAGGED, [4..4]);
        for expect in [2, 3, 4] {
            sel = add_cursor_below(RAGGED, &sel, 4);
            assert_eq!(sel.len(), expect, "押すたびに 1 本ずつ増える");
            assert_invariants(RAGGED, &sel, "連打");
        }
        // 最終行まで来たらそれ以上増えない
        let last = add_cursor_below(RAGGED, &sel, 4);
        assert_eq!(last.len(), 4, "最終行の先には行が無い");
    }

    #[test]
    fn add_cursor_below_sticky_column_returns_to_original() {
        let sel = MultiSel::in_text(RAGGED, [4..4]);
        let col = visual_column_of(RAGGED, 4, 4);
        assert_eq!(col, 4);
        let step1 = add_cursor_below_at(RAGGED, &sel, 4, Some(col));
        let step2 = add_cursor_below_at(RAGGED, &step1, 4, Some(col));
        // 3行目 "abcdefgh" は桁4まであるので byte 10+4 = 14 へ戻る
        assert_eq!(step2.carets(), &[4..4, 9..9, 14..14]);
        // sticky を渡さないと短い行の桁 (2) を引き継いでしまう
        let naive = add_cursor_below(RAGGED, &step1, 4);
        assert_eq!(naive.carets(), &[4..4, 9..9, 12..12]);
    }

    #[test]
    fn add_cursor_above_extends_upward() {
        let sel = MultiSel::in_text(RAGGED, [12..12]); // 3行目の桁2
        let got = add_cursor_above(RAGGED, &sel, 4);
        assert_eq!(got.carets(), &[9..9, 12..12], "2行目 'ab' の桁2 = 行末");
        let up2 = add_cursor_above(RAGGED, &got, 4);
        assert_eq!(up2.carets(), &[2..2, 9..9, 12..12]);
        assert_invariants(RAGGED, &up2, "上に追加");
        // 最上行より上は無い
        assert_eq!(add_cursor_above(RAGGED, &up2, 4).len(), 3);
    }

    #[test]
    fn add_cursor_preserves_visual_column_across_tabs() {
        let text = "\tab\nxxxxxxxxxxxx\n";
        // 1行目: '\t' が桁0→4、'a'=4、'b'=5、行末=6
        assert_eq!(visual_column_of(text, 3, 4), 6);
        let sel = MultiSel::in_text(text, [3..3]);
        let got = add_cursor_below(text, &sel, 4);
        assert_eq!(got.carets(), &[3..3, 10..10], "2行目の桁6 = byte 4+6");
        // タブ幅が変われば着地点も変わる (決め打ちしていない)
        assert_eq!(visual_column_of(text, 3, 8), 10);
        let got8 = add_cursor_below(text, &sel, 8);
        assert_eq!(got8.carets(), &[3..3, 14..14], "2行目の桁10 = byte 4+10");
    }

    #[test]
    fn add_cursor_on_empty_selection_is_noop() {
        let sel = MultiSel::default();
        assert!(add_cursor_below(RAGGED, &sel, 4).is_empty());
        assert!(add_cursor_above(RAGGED, &sel, 4).is_empty());
    }

    #[test]
    fn add_cursor_below_with_japanese_lines() {
        let text = "日本語です\nあい\n漢字かな文字\n";
        let sel = MultiSel::from_char_ranges(text, [3..3]); // 1行目の桁3
        let got = add_cursor_below(text, &sel, 4);
        assert_invariants(text, &got, "日本語の下に追加");
        // 2行目 "あい" は 2 文字しかないので行末へ
        let cols: Vec<usize> = got.to_char_ranges(text).iter().map(|r| r.start).collect();
        assert_eq!(cols, vec![3, 8], "char 8 = 「日本語です\\nあい」の直後");
    }

    // ──────────────── 矩形選択 ────────────────

    #[test]
    fn column_selection_short_lines_get_empty_caret_at_eol() {
        let text = "abcdef\nab\nabcdefgh";
        let sel = column_selection(text, 0, 2, 2, 5, 4);
        assert_eq!(
            sel.carets(),
            &[2..5, 9..9, 12..15],
            "短い2行目は行末の空キャレット"
        );
        assert_invariants(text, &sel, "矩形");
    }

    #[test]
    fn column_selection_is_direction_independent() {
        let text = "abcdef\nab\nabcdefgh";
        let a = column_selection(text, 0, 2, 2, 5, 4);
        let b = column_selection(text, 2, 5, 0, 2, 4);
        let c = column_selection(text, 2, 2, 0, 5, 4);
        assert_eq!(a, b);
        assert_eq!(a, c, "桁の順序も問わない");
    }

    #[test]
    fn column_selection_zero_width_is_one_caret_per_line() {
        let text = "abcdef\nab\nabcdefgh";
        let sel = column_selection(text, 0, 3, 2, 3, 4);
        assert_eq!(sel.carets(), &[3..3, 9..9, 13..13]);
        assert!(sel.slices(text).iter().all(|s| s.is_empty()));
    }

    #[test]
    fn column_selection_rounds_inside_tabs() {
        let text = "\tab\nxxxx";
        // 桁2 はタブ (桁0..4) の内側 — 手前寄りなのでタブの前へ丸める
        assert_eq!(column_selection(text, 0, 2, 0, 2, 4).carets(), &[0..0]);
        // 桁3 は後ろ寄りなのでタブの後ろへ
        assert_eq!(column_selection(text, 0, 3, 0, 3, 4).carets(), &[1..1]);
        // タブを割ったバイト位置は絶対に作らない
        let sel = column_selection(text, 0, 1, 1, 3, 4);
        assert_invariants(text, &sel, "タブの丸め");
        assert_eq!(
            sel.carets(),
            &[0..1, 5..7],
            "1行目はタブ1文字、2行目は桁1..3"
        );
    }

    #[test]
    fn column_selection_clamps_line_numbers() {
        let text = "ab\ncd";
        let sel = column_selection(text, 0, 0, 99, 2, 4);
        assert_eq!(sel.carets(), &[0..2, 3..5], "本文の行数へクランプ");
    }

    #[test]
    fn column_selection_over_japanese_uses_character_columns() {
        let text = "あいうえお\nかきくけこ";
        let sel = column_selection(text, 0, 1, 1, 3, 4);
        assert_eq!(sel.slices(text), vec!["いう", "きく"], "全角も1桁と数える");
        assert_invariants(text, &sel, "日本語の矩形");
    }

    // ──────────────── 一括編集 (後ろから前へ) ────────────────

    #[test]
    fn apply_edit_back_to_front_with_adjacent_ranges() {
        let text = "aaabbbccc";
        let sel = MultiSel::in_text(text, [0..3, 3..6, 6..9]); // 隣接3本
        let (out, new) = apply_edit_to_all(text, &sel, |s| format!("[{s}]"));
        assert_eq!(out, "[aaa][bbb][ccc]");
        assert_eq!(new.carets(), &[0..5, 5..10, 10..15]);
        assert_invariants(&out, &new, "隣接の一括編集");
        assert_eq!(new.slices(&out), vec!["[aaa]", "[bbb]", "[ccc]"]);
    }

    #[test]
    fn apply_edit_sees_pre_edit_text_for_every_caret() {
        let text = "one two three";
        let sel = MultiSel::in_text(text, [0..3, 4..7, 8..13]);
        let mut seen = Vec::new();
        let (out, _) = apply_edit_to_all(text, &sel, |s| {
            seen.push(s.to_string());
            s.to_uppercase()
        });
        assert_eq!(seen, vec!["one", "two", "three"], "編集前の本文が渡る");
        assert_eq!(out, "ONE TWO THREE");
    }

    #[test]
    fn apply_edit_shrinking_ranges_keeps_offsets_valid() {
        let text = "xxxx-yyyy-zzzz";
        let sel = MultiSel::in_text(text, [0..4, 5..9, 10..14]);
        let (out, new) = apply_edit_to_all(text, &sel, |_| "-".to_string());
        assert_eq!(out, "-----", "3 つの塊が 1 文字ずつに縮む");
        assert_eq!(new.carets(), &[0..1, 2..3, 4..5]);
        assert_invariants(&out, &new, "縮む編集");
    }

    #[test]
    fn batch_edits_are_char_boundary_safe_in_japanese() {
        let text = "日本語です\n日本語でした\n";
        let sel = select_all_occurrences(text, "日本語", MatchOpts::default());
        assert_eq!(sel.len(), 2);
        assert_invariants(text, &sel, "日本語の全出現");
        let (out, new) = replace_all_ranges(text, &sel, "にほんご");
        assert_eq!(out, "にほんごです\nにほんごでした\n");
        assert_invariants(&out, &new, "日本語の一括置換");
        assert_eq!(new.slices(&out), vec!["にほんご", "にほんご"]);
        // 後ろから当てているので 2 番目の範囲も正しい位置を指したまま
        assert_eq!(&out[new.carets()[1].clone()], "にほんご");
    }

    #[test]
    fn insert_at_all_keeps_the_selected_text_selected() {
        let text = "あ\nい\nう";
        let sel = column_selection(text, 0, 0, 2, 1, 4);
        let (out, new) = insert_at_all(text, &sel, "# ");
        assert_eq!(out, "# あ\n# い\n# う");
        assert_eq!(new.slices(&out), vec!["あ", "い", "う"], "選択内容は残る");
        assert_invariants(&out, &new, "一括挿入");
    }

    #[test]
    fn insert_at_all_on_empty_carets_lands_after_the_text() {
        let text = "ab\ncd";
        let sel = column_selection(text, 0, 0, 1, 0, 4);
        let (out, new) = insert_at_all(text, &sel, "→");
        assert_eq!(out, "→ab\n→cd");
        assert_eq!(new.carets(), &[3..3, 9..9], "挿入した文字列の直後");
        assert_invariants(&out, &new, "空キャレットへ挿入");
    }

    #[test]
    fn delete_at_all_removes_every_range() {
        let text = "日本語A日本語B日本語";
        let sel = select_all_occurrences(text, "日本語", MatchOpts::default());
        let (out, new) = delete_at_all(text, &sel);
        assert_eq!(out, "AB");
        assert_eq!(new.carets(), &[0..0, 1..1, 2..2]);
        assert_invariants(&out, &new, "一括削除");
    }

    #[test]
    fn edits_on_empty_selection_change_nothing() {
        let text = "そのまま";
        let sel = MultiSel::default();
        assert_eq!(replace_all_ranges(text, &sel, "x").0, text);
        assert_eq!(delete_at_all(text, &sel).0, text);
        assert_eq!(insert_at_all(text, &sel, "x").0, text);
    }

    // ──────────────── 出現の選択 ────────────────

    #[test]
    fn select_all_occurrences_respects_case_and_whole_word() {
        let text = "foo Foo foobar foo";
        let ci = select_all_occurrences(text, "foo", MatchOpts::default());
        assert_eq!(ci.len(), 4, "既定は大文字小文字を無視・部分一致");
        let cs = select_all_occurrences(
            text,
            "foo",
            MatchOpts {
                case_sensitive: true,
                ..MatchOpts::default()
            },
        );
        assert_eq!(cs.slices(text), vec!["foo", "foo", "foo"]);
        let ww = select_all_occurrences(
            text,
            "foo",
            MatchOpts {
                whole_word: true,
                ..MatchOpts::default()
            },
        );
        assert_eq!(ww.len(), 3, "foobar は単語として一致しない");
        assert_invariants(text, &ww, "単語単位");
    }

    #[test]
    fn select_all_occurrences_handles_no_match_and_bad_regex() {
        let text = "abc";
        assert!(select_all_occurrences(text, "zzz", MatchOpts::default()).is_empty());
        assert!(select_all_occurrences(text, "", MatchOpts::default()).is_empty());
        let bad = MatchOpts {
            regex: true,
            ..MatchOpts::default()
        };
        assert!(
            select_all_occurrences(text, "(", bad).is_empty(),
            "壊れた正規表現でも落ちない"
        );
    }

    #[test]
    fn select_next_occurrence_cmd_d_sequence() {
        let text = "let a = 1;\nlet b = 2;\nlet c = 3;";
        let opts = MatchOpts::default();
        // 1 回目: 先頭の出現を掴む
        let mut sel = select_next_occurrence(text, &MultiSel::default(), "let", opts);
        assert_eq!(sel.carets(), &[0..3]);
        // 2 回目・3 回目で下へ 1 つずつ増える
        sel = select_next_occurrence(text, &sel, "let", opts);
        assert_eq!(sel.len(), 2);
        sel = select_next_occurrence(text, &sel, "let", opts);
        assert_eq!(sel.slices(text), vec!["let", "let", "let"]);
        assert_invariants(text, &sel, "⌘D 連打");
        // 4 回目: もう増えない (全部選び終わっている)
        let done = select_next_occurrence(text, &sel, "let", opts);
        assert_eq!(done, sel, "全部選んだら押しても変わらない");
    }

    #[test]
    fn select_next_occurrence_wraps_around() {
        let text = "x a x b x";
        let opts = MatchOpts::default();
        // 最後の出現から始めると先頭へ回り込む
        let sel = MultiSel::in_text(text, [8..9]);
        let got = select_next_occurrence(text, &sel, "x", opts);
        assert_eq!(got.carets(), &[0..1, 8..9], "先頭へ回り込んだ");
    }

    #[test]
    fn select_next_occurrence_with_no_match_is_noop() {
        let text = "abc";
        let sel = MultiSel::in_text(text, [0..1]);
        assert_eq!(
            select_next_occurrence(text, &sel, "zzz", MatchOpts::default()),
            sel
        );
    }

    #[test]
    fn select_next_occurrence_japanese() {
        let text = "犬と猫と犬と鳥と犬";
        let opts = MatchOpts::default();
        let mut sel = MultiSel::default();
        for n in 1..=3 {
            sel = select_next_occurrence(text, &sel, "犬", opts);
            assert_eq!(sel.len(), n);
            assert_invariants(text, &sel, "日本語の⌘D");
        }
        assert_eq!(sel.carets(), &[0..3, 12..15, 24..27]);
        let (out, _) = replace_all_ranges(text, &sel, "🐕");
        assert_eq!(out, "🐕と猫と🐕と鳥と🐕");
    }

    #[test]
    fn multisel_invariants_hold_after_every_constructor() {
        let texts = [
            "",
            "a",
            "a\n",
            "日本語\nかな\n",
            "\tタブ\tab\n短\nxxxxxxxxxx\n",
            "one\r\ntwo\r\n",
        ];
        for text in texts {
            for tab in [1usize, 4, 8] {
                let seed = MultiSel::in_text(text, [0..0]);
                for sel in [
                    MultiSel::in_text(text, [0..text.len(), 1..2, text.len()..text.len()]),
                    add_cursor_below(text, &seed, tab),
                    add_cursor_above(
                        text,
                        &MultiSel::in_text(text, [text.len()..text.len()]),
                        tab,
                    ),
                    column_selection(text, 0, 0, 5, 3, tab),
                    column_selection(text, 3, 7, 0, 0, tab),
                    select_all_occurrences(text, "a", MatchOpts::default()),
                    select_next_occurrence(text, &seed, "n", MatchOpts::default()),
                ] {
                    assert_invariants(text, &sel, &format!("{text:?} tab={tab}"));
                    let (out, after) = apply_edit_to_all(text, &sel, |s| format!("<{s}>"));
                    assert_invariants(&out, &after, &format!("編集後 {text:?} tab={tab}"));
                }
            }
        }
    }

    // ──────────────── 保存時のクリーンアップ (追加分) ────────────────

    /// 行末空白の除去が、決めた規則どおりに効く (表で固定する)。
    ///
    /// **全角スペース U+3000 と NBSP は落とさない** — 日本語の本文では
    /// 体裁として意図して置かれる文字なので、保存のたびに消えては困る。
    #[test]
    fn 末尾空白の除去は決めた規則どおり() {
        for (name, src, want) in [
            ("空行だけの行", "a\n   \nb\n", "a\n\nb\n"),
            ("タブだけの行", "a\n\t\t\nb\n", "a\n\nb\n"),
            ("CRLF は \\r を食わない", "a  \r\nb\t\r\n", "a\r\nb\r\n"),
            ("最終行に改行が無い", "a  \nb  ", "a\nb"),
            ("CR だけの改行", "a  \rb  \r", "a\rb\r"),
            (
                "全角スペースは残す",
                "a\u{3000}\u{3000}\n",
                "a\u{3000}\u{3000}\n",
            ),
            ("NBSP は残す", "a\u{00a0}\n", "a\u{00a0}\n"),
            ("全角の後ろの半角は落とす", "a\u{3000} \n", "a\u{3000}\n"),
            ("空の本文", "", ""),
            ("改行だけ", "\n\n", "\n\n"),
        ] {
            assert_eq!(trim_trailing_whitespace(src), want, "{name}");
        }
    }

    /// 最終行の改行の挿入。空ファイルには足さない。
    #[test]
    fn 最終改行の挿入は表のとおり() {
        let lf = LineEnding::Lf;
        for (name, src, want) in [
            ("既にある", "a\n", "a\n"),
            ("無い", "a", "a\n"),
            ("空ファイル", "", ""),
            ("改行だけのファイル", "\n", "\n"),
            ("空白だけの本文には足す", "  ", "  \n"),
            ("CR で終わっていれば足さない", "a\r", "a\r"),
        ] {
            assert_eq!(ensure_final_newline(src, lf), want, "{name}");
        }
        // CRLF の本文には CRLF を足す
        assert_eq!(ensure_final_newline("a", LineEnding::Crlf), "a\r\n");
    }

    /// 末尾の余分な空行を落とす (`files.trimFinalNewlines`)。
    #[test]
    fn 末尾の余分な空行を落とす() {
        for (name, src, want) in [
            ("2 本以上は 1 本へ", "a\n\n\n", "a\n"),
            ("1 本はそのまま", "a\n", "a\n"),
            ("改行で終わっていなければ触らない", "a", "a"),
            ("途中の空行は残す", "a\n\n\nb\n\n", "a\n\n\nb\n"),
            ("CRLF は CRLF を 1 本残す", "a\r\n\r\n\r\n", "a\r\n"),
            ("CR も 1 本残す", "a\r\r", "a\r"),
            ("空ファイル", "", ""),
            ("改行だけのファイル", "\n\n\n", "\n"),
        ] {
            assert_eq!(trim_final_newlines(src), want, "{name}");
        }
    }

    /// 3 つの整形を一緒にかけたときの合成 (順序が効いていること)。
    #[test]
    fn 保存時の整形は行末空白のあとに空行を落とす() {
        let opts = SaveCleanup {
            trim_trailing: true,
            trim_final_newlines: true,
            final_newline: true,
            target_ending: None,
        };
        // 「空白だけの最終行」は行末空白の除去で空行になり、そのあと落とせる
        let (out, changed) = apply_save_cleanup_checked("a\n   \n\t\n", &opts);
        assert_eq!(out, "a\n");
        assert!(changed);
        // 整い切っている本文は 1 バイトも変わらない
        assert_eq!(
            apply_save_cleanup_checked("a\n", &opts),
            ("a\n".to_string(), false)
        );
        // trim_final_newlines だけでも is_noop にならない
        let only = SaveCleanup {
            trim_final_newlines: true,
            ..Default::default()
        };
        assert!(!only.is_noop());
    }

    // ──────────────── インデントの推定と変換 ────────────────

    /// インデント推定の表テスト。**必ず何かを返す**ことも同時に固定する。
    #[test]
    fn インデントの推定は表のとおり() {
        let fb = IndentStyle::default();
        for (name, src, want) in [
            (
                "タブのみ",
                "fn a() {\n\tb();\n\tc();\n}\n",
                IndentStyle::new(true, 4),
            ),
            (
                "スペース2",
                "a\n  b\n    c\n  d\n",
                IndentStyle::new(false, 2),
            ),
            (
                "スペース4",
                "fn a() {\n    if x {\n        y();\n    }\n}\n",
                IndentStyle::new(false, 4),
            ),
            (
                "混在 (タブが多い)",
                "\ta\n\tb\n  c\n",
                IndentStyle::new(true, 4),
            ),
            ("インデント無し", "a\nb\nc\n", fb),
            ("1 行だけ (字下げ無し)", "hello", fb),
            (
                "1 行だけ (字下げあり)",
                "    hello",
                IndentStyle::new(false, 4),
            ),
            ("空ファイル", "", fb),
            ("空白だけのファイル", "   \n\t\n", fb),
            ("コメント行だけ", "// a\n// b\n", fb),
            (
                "ブロックコメントの継続行は数えない",
                "/**\n * a\n */\nfn b() {\n  c\n}\n",
                IndentStyle::new(false, 2),
            ),
            (
                "CRLF でも同じ",
                "a\r\n    b\r\n",
                IndentStyle::new(false, 4),
            ),
            ("9 桁以上の差は候補にしない", "a\n            b\n", fb),
        ] {
            assert_eq!(detect_indent(src, fb), want, "{name}");
        }
        // fallback がタブなら「判らない」ときはタブのまま返る
        let tab_fb = IndentStyle::new(true, 8);
        assert_eq!(detect_indent("a\nb\n", tab_fb), tab_fb);
        // 幅は必ず 1..=MAX_INDENT_WIDTH へ丸まる
        assert_eq!(IndentStyle::new(false, 0).width, 1);
        assert_eq!(IndentStyle::new(false, 999).width, MAX_INDENT_WIDTH);
    }

    /// インデントの変換は段数を保ち、本文と改行には触らない。
    #[test]
    fn インデントの変換は段数を保つ() {
        let s4 = IndentStyle::new(false, 4);
        let s2 = IndentStyle::new(false, 2);
        let t4 = IndentStyle::new(true, 4);
        for (name, src, from, to, want) in [
            (
                "スペース4 → スペース2",
                "    a\n        b\n",
                s4,
                s2,
                "  a\n    b\n",
            ),
            (
                "スペース4 → タブ",
                "    a\n        b\n",
                s4,
                t4,
                "\ta\n\t\tb\n",
            ),
            (
                "タブ → スペース4",
                "\ta\n\t\tb\n",
                t4,
                s4,
                "    a\n        b\n",
            ),
            ("CRLF はそのまま", "    a\r\n", s4, s2, "  a\r\n"),
            (
                "空白だけの行は触らない",
                "   \n    a\n",
                s4,
                s2,
                "   \n  a\n",
            ),
            ("CJK でも桁だけ変わる", "    あ\n", s4, s2, "  あ\n"),
            ("同じ様式なら恒等", "    a\n", s4, s4, "    a\n"),
            ("半端な桁は余りとして残る", "     a\n", s4, s2, "   a\n"),
        ] {
            assert_eq!(convert_indentation(src, from, to), want, "{name}");
        }
        // 行数は絶対に変わらない
        for src in ["", "a", "a\n", "\ta\n\tb\n"] {
            let out = convert_indentation(src, t4, s2);
            assert_eq!(
                out.split('\n').count(),
                src.split('\n').count(),
                "行数が変わった: {src:?}"
            );
        }
        // 1 段ぶんの実体
        assert_eq!(s2.unit(), "  ");
        assert_eq!(t4.unit(), "\t");
    }

    // ──────────────── 選択範囲への編集コマンド ────────────────

    /// 大文字小文字の変換 (CJK・絵文字は素通し)。
    #[test]
    fn 大文字小文字の変換は表のとおり() {
        for (name, src, kind, want) in [
            ("空選択", "", CaseKind::Upper, ""),
            ("大文字へ", "hello world", CaseKind::Upper, "HELLO WORLD"),
            ("小文字へ", "Hello World", CaseKind::Lower, "hello world"),
            (
                "先頭大文字へ",
                "hello world",
                CaseKind::Title,
                "Hello World",
            ),
            (
                "先頭大文字は残りを小文字にする",
                "hELLO wORLD",
                CaseKind::Title,
                "Hello World",
            ),
            (
                "記号で区切る",
                "foo-bar baz",
                CaseKind::Title,
                "Foo-Bar Baz",
            ),
            ("下線は単語の途中", "foo_bar", CaseKind::Title, "Foo_bar"),
            ("CJK は変わらない", "あいう", CaseKind::Upper, "あいう"),
            ("絵文字は変わらない", "🎉a🎉", CaseKind::Upper, "🎉A🎉"),
            // 数字も単語の一部なので、"1st" の s は大文字にならない
            ("数字始まりの語", "1st place", CaseKind::Title, "1st Place"),
        ] {
            assert_eq!(transform_case(src, kind), want, "{name}");
        }
    }

    /// 行の並べ替え / 重複削除は、選択が覆う行だけを書き換える。
    #[test]
    fn 行の並べ替えと重複削除は選択した行だけに効く() {
        // 全選択 (末尾の改行を巻き込まない)
        let text = "b\na\nc\n";
        let (out, s, e) = sort_lines(text, 0, text.chars().count(), false);
        assert_eq!(out, "a\nb\nc\n");
        assert_eq!((s, e), (0, 5), "書き換えた行の全体が選択される");
        let (out, ..) = sort_lines(text, 0, text.chars().count(), true);
        assert_eq!(out, "c\nb\na\n");

        // 1 行だけ (選択なし) は並べ替えるものが無い
        let (out, s, e) = sort_lines(text, 0, 0, false);
        assert_eq!(out, text);
        assert_eq!((s, e), (0, 1));

        // 選択が覆う行だけ (2〜3 行目)
        let text = "z\nc\nb\na\n";
        let (out, ..) = sort_lines(text, 2, 5, false);
        assert_eq!(out, "z\nb\nc\na\n");

        // 重複削除: 最初の 1 本だけ残り、並び順は変わらない
        let text = "b\na\nb\na\n";
        let (out, ..) = dedupe_lines(text, 0, text.chars().count());
        assert_eq!(out, "b\na\n");
        // 重複が無ければ本文は変わらない
        let text = "a\nb\nc\n";
        let (out, ..) = dedupe_lines(text, 0, text.chars().count());
        assert_eq!(out, text);

        // CRLF は保たれる
        let text = "b\r\na\r\n";
        let (out, ..) = sort_lines(text, 0, text.chars().count(), false);
        assert_eq!(out, "a\r\nb\r\n");

        // CJK / 絵文字でもバイト境界を割らない
        let text = "い\nあ\n🎉\n";
        let (out, s, e) = sort_lines(text, 0, text.chars().count(), false);
        assert_eq!(out, "あ\nい\n🎉\n");
        assert!(e >= s && e <= out.chars().count());
    }

    /// JSON 整形。壊れた JSON は本文を変えずにエラーを返す。
    #[test]
    fn json整形はインデントを設定から採る() {
        assert_eq!(format_json("{\"a\":1}", "  ").unwrap(), "{\n  \"a\": 1\n}");
        assert_eq!(
            format_json("{\"a\":[1,2]}", "\t").unwrap(),
            "{\n\t\"a\": [\n\t\t1,\n\t\t2\n\t]\n}"
        );
        // 前後の空白は無視する (選択が改行を含んでいても通る)
        assert!(format_json("\n  [1]\n", "  ").is_ok());
        // 壊れた JSON は Err (本文には触らせない)
        assert!(format_json("{", "  ").is_err());
        assert!(format_json("", "  ").is_err());
        // CJK はエスケープせずそのまま (読める形で残す)
        assert_eq!(
            format_json("{\"a\":\"あ\"}", " ").unwrap(),
            "{\n \"a\": \"あ\"\n}"
        );
    }
}
