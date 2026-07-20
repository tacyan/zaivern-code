//! VS Code snippet engine: parses VS Code-format snippet JSON files
//! (name -> { prefix, body, description }) and expands snippet templates
//! ($1, ${1:default}, ${1|a,b|}, $TM_FILENAME, escapes) for Tab expansion.

#[derive(Clone)]
pub struct Snippet {
    // name / description / language は VS Code スニペット形式の忠実な写しで、
    // 補完UIのラベル表示などに用いる想定(現状 Tab 展開は prefix/body のみ使用)。
    #[allow(dead_code)]
    pub name: String,
    pub prefix: String,
    pub body: String,
    #[allow(dead_code)]
    pub description: String,
    #[allow(dead_code)]
    pub language: String,
}

// ---------------------------------------------------------------------------
// JSONC-tolerant parsing of snippet files
// ---------------------------------------------------------------------------

/// Strip // and /* */ comments (string-aware) and trailing commas so that
/// JSONC snippet files survive serde_json.
fn strip_jsonc(src: &str) -> String {
    let cs: Vec<char> = src.chars().collect();
    let mut out = String::with_capacity(src.len());
    let mut i = 0;
    let mut in_str = false;
    while i < cs.len() {
        let c = cs[i];
        if in_str {
            out.push(c);
            if c == '\\' && i + 1 < cs.len() {
                out.push(cs[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
        } else if c == '"' {
            in_str = true;
            out.push(c);
            i += 1;
        } else if c == '/' && i + 1 < cs.len() && cs[i + 1] == '/' {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
        } else if c == '/' && i + 1 < cs.len() && cs[i + 1] == '*' {
            i += 2;
            while i + 1 < cs.len() && !(cs[i] == '*' && cs[i + 1] == '/') {
                i += 1;
            }
            i = (i + 2).min(cs.len());
        } else {
            out.push(c);
            i += 1;
        }
    }
    // Second pass: drop trailing commas before } or ]
    let cs: Vec<char> = out.chars().collect();
    let mut out2 = String::with_capacity(out.len());
    let mut in_str = false;
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if in_str {
            out2.push(c);
            if c == '\\' && i + 1 < cs.len() {
                out2.push(cs[i + 1]);
                i += 2;
                continue;
            }
            if c == '"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if c == '"' {
            in_str = true;
            out2.push(c);
            i += 1;
            continue;
        }
        if c == ',' {
            let mut j = i + 1;
            while j < cs.len() && cs[j].is_whitespace() {
                j += 1;
            }
            if j < cs.len() && (cs[j] == '}' || cs[j] == ']') {
                i += 1;
                continue;
            }
        }
        out2.push(c);
        i += 1;
    }
    out2
}

fn json_str_or_join(v: &serde_json::Value, sep: &str) -> Option<String> {
    match v {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Array(a) => Some(
            a.iter()
                .filter_map(|x| x.as_str())
                .collect::<Vec<_>>()
                .join(sep),
        ),
        _ => None,
    }
}

/// Parse one VS Code snippet JSON file (JSONC tolerated). `language` is the
/// snippet language id ("rust", "typescript", ...) taken from the file's origin.
pub fn parse_file(path: &std::path::Path, language: &str) -> Vec<Snippet> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let clean = strip_jsonc(&src);
    let val: serde_json::Value = match serde_json::from_str(&clean) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let obj = match val.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let mut result = Vec::new();
    for (name, entry) in obj {
        let e = match entry.as_object() {
            Some(e) => e,
            None => continue,
        };
        // prefix: string or array of strings (first entry wins)
        let prefix = match e.get("prefix") {
            Some(serde_json::Value::String(s)) => s.clone(),
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str())
                .next()
                .unwrap_or("")
                .to_string(),
            _ => continue,
        };
        if prefix.is_empty() {
            continue;
        }
        // body: string or array of lines joined with \n
        let body = match e.get("body").and_then(|v| json_str_or_join(v, "\n")) {
            Some(b) => b,
            None => continue,
        };
        let description = e
            .get("description")
            .and_then(|v| json_str_or_join(v, " "))
            .unwrap_or_default();
        result.push(Snippet {
            name: name.clone(),
            prefix,
            body,
            description,
            language: language.to_string(),
        });
    }
    result
}

// ---------------------------------------------------------------------------
// Template expansion
// ---------------------------------------------------------------------------

struct ExpandState {
    out: String,
    len: usize, // char count of `out`
    pos1: Option<usize>,
    pos0: Option<usize>,
}

impl ExpandState {
    fn new() -> Self {
        ExpandState {
            out: String::new(),
            len: 0,
            pos1: None,
            pos0: None,
        }
    }
    fn push(&mut self, c: char) {
        self.out.push(c);
        self.len += 1;
    }
    fn push_str(&mut self, s: &str) {
        self.out.push_str(s);
        self.len += s.chars().count();
    }
    fn mark(&mut self, n: u32) {
        if n == 1 && self.pos1.is_none() {
            self.pos1 = Some(self.len);
        }
        if n == 0 && self.pos0.is_none() {
            self.pos0 = Some(self.len);
        }
    }
}

fn resolve_var(name: &str, filename: &str) -> Option<String> {
    let path = std::path::Path::new(filename);
    let base = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    match name {
        "TM_FILENAME" => Some(base),
        "TM_FILENAME_BASE" => {
            let b = match base.rfind('.') {
                Some(0) | None => base,
                Some(i) => base[..i].to_string(),
            };
            Some(b)
        }
        "TM_FILEPATH" => Some(filename.to_string()),
        "TM_DIRECTORY" => Some(
            path.parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        ),
        "TM_LINE_INDEX" => Some("0".to_string()),
        "TM_LINE_NUMBER" => Some("1".to_string()),
        "TM_SELECTED_TEXT" | "TM_CURRENT_LINE" | "TM_CURRENT_WORD" | "CLIPBOARD"
        | "WORKSPACE_NAME" | "WORKSPACE_FOLDER" | "RELATIVE_FILEPATH" | "UUID" | "RANDOM"
        | "RANDOM_HEX" => Some(String::new()),
        "BLOCK_COMMENT_START" => Some("/*".to_string()),
        "BLOCK_COMMENT_END" => Some("*/".to_string()),
        "LINE_COMMENT" => Some("//".to_string()),
        n if n.starts_with("CURRENT_") => Some(String::new()),
        _ => None,
    }
}

fn is_var_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Skip to the unescaped `}` that closes the construct starting inside a
/// `${...}` (depth-aware for nested braces). Returns the index just past it.
fn skip_brace(cs: &[char], mut i: usize) -> usize {
    let mut depth = 0usize;
    while i < cs.len() {
        let c = cs[i];
        if c == '\\' && i + 1 < cs.len() {
            i += 2;
            continue;
        }
        if c == '{' {
            depth += 1;
        } else if c == '}' {
            if depth == 0 {
                return i + 1;
            }
            depth -= 1;
        }
        i += 1;
    }
    i
}

/// Parse the segment starting at `i`. If `stop_at_brace`, stop after
/// consuming the unescaped `}` that closes the current placeholder.
/// Returns the index of the next unconsumed char.
fn parse_segment(
    cs: &[char],
    mut i: usize,
    st: &mut ExpandState,
    filename: &str,
    stop_at_brace: bool,
) -> usize {
    while i < cs.len() {
        let c = cs[i];
        if c == '\\' && i + 1 < cs.len() {
            let n = cs[i + 1];
            if n == '$' || n == '\\' || n == '}' {
                st.push(n);
                i += 2;
                continue;
            }
        }
        if c == '}' && stop_at_brace {
            return i + 1;
        }
        if c == '$' && i + 1 < cs.len() {
            i = parse_dollar(cs, i, st, filename);
            continue;
        }
        st.push(c);
        i += 1;
    }
    i
}

/// Parse a `$...` construct with `i` pointing at the `$`.
/// Returns the index of the next unconsumed char.
fn parse_dollar(cs: &[char], i: usize, st: &mut ExpandState, filename: &str) -> usize {
    let next = cs[i + 1];
    // $1 $12 $0
    if next.is_ascii_digit() {
        let mut j = i + 1;
        let mut n: u32 = 0;
        while j < cs.len() && cs[j].is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(cs[j] as u32 - '0' as u32);
            j += 1;
        }
        st.mark(n);
        return j;
    }
    // $TM_FILENAME etc.
    if is_var_char(next) {
        let mut j = i + 1;
        while j < cs.len() && is_var_char(cs[j]) {
            j += 1;
        }
        let name: String = cs[i + 1..j].iter().collect();
        if let Some(v) = resolve_var(&name, filename) {
            st.push_str(&v);
        }
        return j;
    }
    if next != '{' {
        st.push('$');
        return i + 1;
    }
    // ${...}
    let mut j = i + 2;
    if j >= cs.len() {
        st.push_str("${");
        return j;
    }
    if cs[j].is_ascii_digit() {
        let mut n: u32 = 0;
        while j < cs.len() && cs[j].is_ascii_digit() {
            n = n.saturating_mul(10).saturating_add(cs[j] as u32 - '0' as u32);
            j += 1;
        }
        if j >= cs.len() {
            st.mark(n);
            return j;
        }
        match cs[j] {
            '}' => {
                // ${1}
                st.mark(n);
                j + 1
            }
            ':' => {
                // ${1:default} — cursor sits at the start of the default text
                st.mark(n);
                parse_segment(cs, j + 1, st, filename, true)
            }
            '|' => {
                // ${1|a,b,c|} — first choice wins
                st.mark(n);
                j += 1;
                let mut first = String::new();
                while j < cs.len() && cs[j] != ',' && cs[j] != '|' {
                    if cs[j] == '\\' && j + 1 < cs.len() {
                        first.push(cs[j + 1]);
                        j += 2;
                    } else {
                        first.push(cs[j]);
                        j += 1;
                    }
                }
                st.push_str(&first);
                // skip remaining choices to the closing `|`
                while j < cs.len() && cs[j] != '|' {
                    if cs[j] == '\\' && j + 1 < cs.len() {
                        j += 2;
                    } else {
                        j += 1;
                    }
                }
                if j + 1 < cs.len() && cs[j] == '|' && cs[j + 1] == '}' {
                    j + 2
                } else {
                    skip_brace(cs, j.min(cs.len()))
                }
            }
            '/' => {
                // ${1/regex/replace/} — transform unsupported: treat as tabstop
                st.mark(n);
                skip_brace(cs, j + 1)
            }
            _ => {
                // malformed — emit literally
                st.push_str("${");
                j
            }
        }
    } else if is_var_char(cs[j]) {
        let start = j;
        while j < cs.len() && is_var_char(cs[j]) {
            j += 1;
        }
        let name: String = cs[start..j].iter().collect();
        if j >= cs.len() {
            if let Some(v) = resolve_var(&name, filename) {
                st.push_str(&v);
            }
            return j;
        }
        match cs[j] {
            '}' => {
                // ${TM_FILENAME}
                if let Some(v) = resolve_var(&name, filename) {
                    st.push_str(&v);
                }
                j + 1
            }
            ':' => {
                // ${VAR:default} — default kicks in when the var is empty/unknown
                match resolve_var(&name, filename) {
                    Some(v) if !v.is_empty() => {
                        st.push_str(&v);
                        let mut scratch = ExpandState::new();
                        parse_segment(cs, j + 1, &mut scratch, filename, true)
                    }
                    _ => parse_segment(cs, j + 1, st, filename, true),
                }
            }
            '/' => {
                // ${VAR/regex/replace/} — transform unsupported: raw value
                if let Some(v) = resolve_var(&name, filename) {
                    st.push_str(&v);
                }
                skip_brace(cs, j + 1)
            }
            _ => {
                st.push_str("${");
                start
            }
        }
    } else {
        st.push('$');
        i + 1
    }
}

/// Expand a snippet body. Tabstops/placeholders/choices/variables are
/// resolved; returns (inserted text, cursor char position). Cursor goes to
/// the first $1, else $0, else end of text. Char-indexed, multibyte safe.
pub fn expand(body: &str, filename: &str) -> (String, usize) {
    let cs: Vec<char> = body.chars().collect();
    let mut st = ExpandState::new();
    parse_segment(&cs, 0, &mut st, filename, false);
    let cursor = st.pos1.or(st.pos0).unwrap_or(st.len);
    (st.out, cursor)
}

// ---------------------------------------------------------------------------
// Tab expansion at cursor
// ---------------------------------------------------------------------------

/// If the word immediately before `cursor_char` equals a snippet prefix,
/// replace it with the expanded snippet. Returns (new text, new cursor).
pub fn try_expand_at(
    text: &str,
    cursor_char: usize,
    snippets: &[Snippet],
    filename: &str,
) -> Option<(String, usize)> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor_char.min(chars.len());
    let mut start = cursor;
    while start > 0 {
        let c = chars[start - 1];
        if c.is_ascii_alphanumeric() || c == '_' {
            start -= 1;
        } else {
            break;
        }
    }
    if start == cursor {
        return None;
    }
    let word: String = chars[start..cursor].iter().collect();
    for sn in snippets {
        if sn.prefix == word {
            let (ins, rel) = expand(&sn.body, filename);
            let mut new_text: String = chars[..start].iter().collect();
            new_text.push_str(&ins);
            new_text.extend(chars[cursor..].iter());
            return Some((new_text, start + rel));
        }
    }
    None
}

// ---------------------------------------------------------------------------
// syntect name -> snippet language id
// ---------------------------------------------------------------------------

/// Map an editor syntect syntax name ("Rust", "JavaScript", ...) to a
/// VS Code snippet language id ("rust", "javascript", ...).
pub fn lang_id_for(syntect_name: &str) -> &'static str {
    match syntect_name {
        "Rust" => "rust",
        "JavaScript" | "JavaScript (Babel)" => "javascript",
        "TypeScript" => "typescript",
        "TypeScriptReact" | "TSX" => "typescriptreact",
        "JSX" | "JavaScriptReact" => "javascriptreact",
        "Python" => "python",
        "C" => "c",
        "C++" => "cpp",
        "C#" => "csharp",
        "Go" => "go",
        "Java" => "java",
        "Kotlin" => "kotlin",
        "Swift" => "swift",
        "Objective-C" => "objective-c",
        "Objective-C++" => "objective-cpp",
        "Ruby" => "ruby",
        "PHP" => "php",
        "Perl" => "perl",
        "Lua" => "lua",
        "R" => "r",
        "Scala" => "scala",
        "Haskell" => "haskell",
        "Erlang" => "erlang",
        "Elixir" => "elixir",
        "Dart" => "dart",
        "HTML" | "HTML (ASP)" => "html",
        "CSS" => "css",
        "SCSS" => "scss",
        "Sass" => "sass",
        "Less" => "less",
        "JSON" => "json",
        "XML" => "xml",
        "YAML" => "yaml",
        "TOML" => "toml",
        "Markdown" => "markdown",
        "SQL" => "sql",
        "Shell-Unix-Generic" | "Bourne Again Shell (bash)" | "Shell Script (Bash)" => {
            "shellscript"
        }
        "Batch File" => "bat",
        "PowerShell" => "powershell",
        "Makefile" => "makefile",
        "Dockerfile" => "dockerfile",
        "Graphviz (DOT)" => "dot",
        "LaTeX" => "latex",
        "Vue Component" | "Vue" => "vue",
        "Svelte" => "svelte",
        _ => "plaintext",
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- expand ----

    #[test]
    fn expand_simple_body() {
        let (s, c) = expand("hello", "/tmp/a.rs");
        assert_eq!(s, "hello");
        assert_eq!(c, 5); // no tabstop: cursor at end
    }

    #[test]
    fn expand_placeholder_default() {
        let (s, c) = expand("${1:default}", "/tmp/a.rs");
        assert_eq!(s, "default");
        assert_eq!(c, 0); // cursor at start of the placeholder text
    }

    #[test]
    fn expand_dollar_zero_position() {
        let (s, c) = expand("ab$0cd", "/tmp/a.rs");
        assert_eq!(s, "abcd");
        assert_eq!(c, 2);
    }

    #[test]
    fn expand_dollar_one_wins_over_zero() {
        let (s, c) = expand("a$0b$1c", "/tmp/a.rs");
        assert_eq!(s, "abc");
        assert_eq!(c, 2); // $1 preferred over $0
    }

    #[test]
    fn expand_escapes() {
        let (s, c) = expand(r"\$1 \\ \}", "/tmp/a.rs");
        assert_eq!(s, r"$1 \ }");
        assert_eq!(c, 6); // literal text, cursor at end
    }

    #[test]
    fn expand_variable_tm_filename() {
        let (s, _) = expand("// $TM_FILENAME / ${TM_FILENAME_BASE}", "/tmp/foo.rs");
        assert_eq!(s, "// foo.rs / foo");
    }

    #[test]
    fn expand_choice_takes_first() {
        let (s, c) = expand("${1|red,green,blue|}!", "/tmp/a.rs");
        assert_eq!(s, "red!");
        assert_eq!(c, 0);
    }

    #[test]
    fn expand_japanese_body_char_safe() {
        let (s, c) = expand("こんにちは$1世界", "/tmp/a.rs");
        assert_eq!(s, "こんにちは世界");
        assert_eq!(c, 5); // char units, not bytes
    }

    #[test]
    fn expand_nested_variable_in_placeholder() {
        let (s, c) = expand("class ${1:$TM_FILENAME_BASE} {}", "/dir/MyMod.rs");
        assert_eq!(s, "class MyMod {}");
        assert_eq!(c, 6);
    }

    #[test]
    fn expand_multiline_and_var_default() {
        let (s, c) = expand("if $1 {\n\t$0\n}\n${TM_SELECTED_TEXT:done}", "/tmp/a.rs");
        assert_eq!(s, "if  {\n\t\n}\ndone");
        assert_eq!(c, 3); // $1 position
    }

    // ---- try_expand_at ----

    fn test_snippets() -> Vec<Snippet> {
        vec![Snippet {
            name: "For".to_string(),
            prefix: "fo".to_string(),
            body: "for $1 {}$0".to_string(),
            description: String::new(),
            language: "rust".to_string(),
        }]
    }

    #[test]
    fn try_expand_match() {
        let sn = test_snippets();
        let (t, c) = try_expand_at("let fo", 6, &sn, "/tmp/a.rs").unwrap();
        assert_eq!(t, "let for  {}");
        assert_eq!(c, 8); // "let " (4) + "for " (4) => $1 at char 8
    }

    #[test]
    fn try_expand_no_match() {
        let sn = test_snippets();
        assert!(try_expand_at("let foo", 7, &sn, "/tmp/a.rs").is_none());
        assert!(try_expand_at("fo ", 3, &sn, "/tmp/a.rs").is_none()); // cursor after space
    }

    #[test]
    fn try_expand_prefix_after_japanese() {
        let sn = test_snippets();
        // word boundary right after multibyte chars: "値はfo|"
        let (t, c) = try_expand_at("値はfo", 4, &sn, "/tmp/a.rs").unwrap();
        assert_eq!(t, "値はfor  {}");
        assert_eq!(c, 6); // 2 japanese chars + "for " => $1 at char 6
    }

    // ---- parse_file ----

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "zaivern_snippets_test_{}_{}.json",
            tag,
            std::process::id()
        ))
    }

    #[test]
    fn parse_file_plain_json() {
        let p = tmp_path("plain");
        std::fs::write(
            &p,
            r#"{"Print":{"prefix":"pr","body":["println!(\"$1\");","$0"],"description":"print macro"}}"#,
        )
        .unwrap();
        let v = parse_file(&p, "rust");
        let _ = std::fs::remove_file(&p);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].name, "Print");
        assert_eq!(v[0].prefix, "pr");
        assert_eq!(v[0].body, "println!(\"$1\");\n$0"); // lines joined with \n
        assert_eq!(v[0].description, "print macro");
        assert_eq!(v[0].language, "rust");
    }

    #[test]
    fn parse_file_jsonc_with_comments_and_trailing_commas() {
        let p = tmp_path("jsonc");
        std::fs::write(
            &p,
            r#"// file comment
{
    /* block comment */
    "For Loop": {
        "prefix": ["for", "forloop"], // multiple prefixes: first wins
        "body": "for ${1:x} in ${2:xs} { $0 } // see https://example.com",
        "description": "For loop",
    },
}"#,
        )
        .unwrap();
        let v = parse_file(&p, "rust");
        let _ = std::fs::remove_file(&p);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].prefix, "for"); // first of the prefix array
        assert!(v[0].body.contains("${1:x}"));
        assert!(v[0].body.contains("https://example.com")); // // inside string kept
        assert_eq!(v[0].description, "For loop");
    }
}
