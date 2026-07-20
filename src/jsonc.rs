//! JSONC (コメント・末尾カンマ入り JSON) を素の JSON へ変換する共通ヘルパ。
//! テーマ JSON (`theme_json`) と VS Code スニペット JSON (`snippets`) の
//! 双方が serde_json へ渡す前段としてこれを使う。

/// JSONC (コメント・末尾カンマ入りJSON) を素のJSONへ変換する。
/// マルチバイト安全のため全処理をバイト単位で行う。
///
/// 除去するもの:
/// - `//` 行コメント / `/* */` ブロックコメント (未終端なら以降を全て捨てる)
/// - `}` `]` の直前の末尾カンマ
///
/// いずれも文字列リテラルの内側は保護し、`\"` などのエスケープを正しく追跡する。
pub(crate) fn strip_jsonc(s: &str) -> String {
    let b = s.as_bytes();
    let mut pass1: Vec<u8> = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    let mut escape = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            pass1.push(c);
            if escape {
                escape = false;
            } else if c == b'\\' {
                escape = true;
            } else if c == b'"' {
                in_str = false;
            }
            i += 1;
        } else if c == b'"' {
            in_str = true;
            pass1.push(c);
            i += 1;
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'/' {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
        } else if c == b'/' && i + 1 < b.len() && b[i + 1] == b'*' {
            i += 2;
            while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(b.len());
        } else {
            pass1.push(c);
            i += 1;
        }
    }

    // 末尾カンマ除去
    let mut out: Vec<u8> = Vec::with_capacity(pass1.len());
    let mut in_str = false;
    let mut escape = false;
    for (idx, &ch) in pass1.iter().enumerate() {
        if in_str {
            out.push(ch);
            if escape {
                escape = false;
            } else if ch == b'\\' {
                escape = true;
            } else if ch == b'"' {
                in_str = false;
            }
            continue;
        }
        if ch == b'"' {
            in_str = true;
            out.push(ch);
            continue;
        }
        if ch == b',' {
            let mut j = idx + 1;
            while j < pass1.len() && pass1[j].is_ascii_whitespace() {
                j += 1;
            }
            if j < pass1.len() && (pass1[j] == b'}' || pass1[j] == b']') {
                continue;
            }
        }
        out.push(ch);
    }
    String::from_utf8(out).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strip_jsonc の結果が素の JSON として解釈できることを確認する。
    fn strip_and_parse(src: &str) -> serde_json::Value {
        let clean = strip_jsonc(src);
        serde_json::from_str(&clean)
            .unwrap_or_else(|e| panic!("strip_jsonc の出力が JSON として不正: {e} / {clean:?}"))
    }

    // ---- strip_jsonc: 文字列リテラルを壊さないこと ----

    #[test]
    fn strip_jsonc_keeps_double_slash_inside_string() {
        let src = r#"{"url": "http://example.com"}"#;
        assert_eq!(strip_jsonc(src), src);
        assert_eq!(strip_and_parse(src)["url"], "http://example.com");
    }

    #[test]
    fn strip_jsonc_keeps_block_comment_markers_inside_string() {
        let src = r#"{"a": "/* not a comment */", "b": "x /* y"}"#;
        assert_eq!(strip_jsonc(src), src);
        let v = strip_and_parse(src);
        assert_eq!(v["a"], "/* not a comment */");
        assert_eq!(v["b"], "x /* y");
    }

    #[test]
    fn strip_jsonc_keeps_comment_after_escaped_quote_in_string() {
        // 文字列内の \" で in_str が誤って閉じないこと
        let src = r#"{"a": "say \" // still inside"}"#;
        assert_eq!(strip_jsonc(src), src);
        assert_eq!(strip_and_parse(src)["a"], r#"say " // still inside"#);
    }

    #[test]
    fn strip_jsonc_keeps_trailing_comma_lookalike_inside_string() {
        let src = r#"{"a": "1,]", "b": "2,}"}"#;
        assert_eq!(strip_jsonc(src), src);
        let v = strip_and_parse(src);
        assert_eq!(v["a"], "1,]");
        assert_eq!(v["b"], "2,}");
    }

    #[test]
    fn strip_jsonc_keeps_backslash_pair_before_closing_quote() {
        // "C:\\" の直後の // が文字列外のコメントとして扱われること
        let src = "{\"a\": \"C:\\\\\" // tail\n}";
        let v = strip_and_parse(src);
        assert_eq!(v["a"], "C:\\");
    }

    // ---- strip_jsonc: コメント・末尾カンマの除去 ----

    #[test]
    fn strip_jsonc_removes_line_comment() {
        let src = "{\n  // これはコメント\n  \"a\": 1\n}";
        let v = strip_and_parse(src);
        assert_eq!(v["a"], 1);
        assert!(!strip_jsonc(src).contains("コメント"));
    }

    #[test]
    fn strip_jsonc_removes_block_comment() {
        let src = r#"{"a": /* mid */ 1, /* multi
line */ "b": 2}"#;
        let v = strip_and_parse(src);
        assert_eq!(v["a"], 1);
        assert_eq!(v["b"], 2);
    }

    #[test]
    fn strip_jsonc_removes_trailing_comma_in_object_and_array() {
        let v = strip_and_parse(r#"{"a": [1, 2, ], "b": 3,}"#);
        assert_eq!(v["a"][1], 2);
        assert_eq!(v["b"], 3);
    }

    #[test]
    fn strip_jsonc_removes_trailing_comma_followed_by_comment() {
        let src = "{\n  \"a\": 1, // 末尾カンマ + コメント\n}";
        assert_eq!(strip_and_parse(src)["a"], 1);
    }

    #[test]
    fn strip_jsonc_keeps_multibyte_text_intact() {
        let src = r#"{"名前": "日本語テーマ"}"#;
        assert_eq!(strip_jsonc(src), src);
        assert_eq!(strip_and_parse(src)["名前"], "日本語テーマ");
    }

    #[test]
    fn strip_jsonc_empty_input_stays_empty() {
        assert_eq!(strip_jsonc(""), "");
    }

    #[test]
    fn strip_jsonc_unterminated_block_comment_consumes_rest() {
        assert_eq!(strip_jsonc("{\"a\": 1 /* oops"), "{\"a\": 1 ");
    }
}
