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
    let (open, close, forward) = PAIRS
        .iter()
        .find_map(|&(o, c)| {
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
    Surround { text: String, select: (usize, usize) },
    /// 開き+閉じを挿入してカーソルをペアの間に置く。
    Insert { text: String, cursor: usize },
    /// 既にある閉じを飛び越える (重複挿入を避けてカーソルだけ右へ)。
    SkipOver { cursor: usize },
}

/// 文字 `typed` を打った瞬間の自動ペア判定。該当しなければ None (通常入力)。
/// `sel_min..sel_max` は現在の選択 (char)。空選択なら両者同値。
pub fn pair_on_type(
    text: &str,
    sel_min: usize,
    sel_max: usize,
    typed: char,
) -> Option<PairEdit> {
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
        return Some(PairEdit::SkipOver { cursor: sel_max + 1 });
    }
    let close = closer_of?;
    // 引用符は単語や同じ引用符の直後では自動閉じしない (don't 等のアポストロフィ)
    if typed == close {
        let prev = sel_min
            .checked_sub(1)
            .and_then(|i| text.chars().nth(i));
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
    let byte_pos =
        find_at(text, query, start_byte.min(text.len())).or_else(|| text.find(query))?;
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

/// 保存時に本文へかける整形。すべて既定は「何もしない」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SaveCleanup {
    /// 行末の空白を落とす。
    pub trim_trailing: bool,
    /// 最終行に改行を入れる。
    pub final_newline: bool,
    /// 改行コードを揃える。`None` なら本文の改行には触らない。
    pub target_ending: Option<LineEnding>,
}

impl SaveCleanup {
    /// 何も仕事が無い設定か (呼び出し側が丸ごと省けるようにする)。
    pub fn is_noop(&self) -> bool {
        !self.trim_trailing && !self.final_newline && self.target_ending.is_none()
    }
}

/// [`SaveCleanup`] を順に適用する。適用順は固定:
///
/// 1. 行末の空白を落とす
/// 2. 改行コードを揃える (`target_ending` があるとき)
/// 3. 最終行に改行を入れる
///
/// この順でないと、1 で行末が空白だけになった行を 3 が数え違えたり、
/// 3 が足した改行を 2 が揃え忘れたりする。
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
    byte_to_char(cleaned, adjust_offset_after_cleanup(original, cleaned, byte))
}

/// バイト位置を文字境界まで手前へ寄せる (多バイト文字を割らないため)。
fn snap_char_boundary(s: &str, byte: usize) -> usize {
    let mut byte = byte.min(s.len());
    while byte > 0 && !s.is_char_boundary(byte) {
        byte -= 1;
    }
    byte
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
        assert_eq!(trim_trailing_whitespace("    let x = 1;  \n"), "    let x = 1;\n");
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
        assert_eq!(trim_trailing_whitespace("x\u{a0}\n"), "x\u{a0}\n", "NBSP も残す");
        // 全角スペースの後ろに付いた半角だけが落ちる
        assert_eq!(trim_trailing_whitespace("日本語　  \t\n"), "日本語　\n");
    }

    // ---- ensure_final_newline ----

    #[test]
    fn ensure_final_newline_adds_only_when_missing() {
        assert_eq!(ensure_final_newline("a", LineEnding::Lf), "a\n");
        assert_eq!(ensure_final_newline("a\n", LineEnding::Lf), "a\n", "二重にしない");
        assert_eq!(ensure_final_newline("a\r\n", LineEnding::Crlf), "a\r\n");
        assert_eq!(ensure_final_newline("a\r", LineEnding::Cr), "a\r");
        assert_eq!(ensure_final_newline("", LineEnding::Lf), "", "空のファイルは空のまま");
        // 空白だけの本文は「1 行書いてある」ので他の行と同じ扱い
        assert_eq!(ensure_final_newline("   ", LineEnding::Lf), "   \n");
        assert_eq!(ensure_final_newline("日本語", LineEnding::Crlf), "日本語\r\n");
        // 混在は最多の様式で足す
        let mixed = crate::textenc::detect_line_ending("a\r\nb\r\nc\n");
        assert_eq!(ensure_final_newline("x", mixed), "x\r\n");
    }

    // ---- apply_save_cleanup ----

    #[test]
    fn save_cleanup_composes_trim_ending_and_final_newline() {
        let opts = SaveCleanup {
            trim_trailing: true,
            final_newline: true,
            target_ending: Some(LineEnding::Crlf),
        };
        let (out, changed) = apply_save_cleanup_checked("a  \nb\t", &opts);
        assert_eq!(out, "a\r\nb\r\n");
        assert!(changed);
        assert_eq!(apply_save_cleanup("a  \nb\t", &opts), out, "短い版も同じ結果");
    }

    #[test]
    fn save_cleanup_changed_flag_is_exact() {
        let all = SaveCleanup {
            trim_trailing: true,
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
            final_newline: true,
            target_ending: None,
        };
        // 変換先を指定しなければ、その本文で一番多い改行に合わせる
        assert_eq!(apply_save_cleanup("a\r\nb", &opts), "a\r\nb\r\n");
        assert_eq!(apply_save_cleanup("a\nb", &opts), "a\nb\n");
        // 改行コードだけ揃える (本文には触らない)
        let only_ending = SaveCleanup {
            trim_trailing: false,
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
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 999), cleaned.len());
    }

    #[test]
    fn adjust_offset_is_multibyte_safe() {
        let original = "日本語  \nあ";
        let cleaned = trim_trailing_whitespace(original);
        assert_eq!(cleaned, "日本語\nあ");
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 3), 3, "日と本の間");
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 9), 9, "語の直後");
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 10), 9, "空白の中");
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 12), 10, "次の行の行頭");
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 15), 13, "EOF");
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
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 4), 2, "\\r と \\n の間");
        assert_eq!(adjust_offset_after_cleanup(original, &cleaned, 5), 3, "次の行の行頭");
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
        assert_eq!(adjust_char_index_after_cleanup(original, &cleaned, 6), 4, "次の行の行頭");
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
}
