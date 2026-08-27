//! 情報量を減らす純粋な変換だけを置く。**ファイルも設定も読まない。**
//!
//! ここにある関数は全て `&str → String` (または判定) で、入出力以外に
//! 触るものが無い。walker (`walk.rs`) と道具 (`tools/`) から呼ばれる。
//!
//! ## 担保しないこと (正直に)
//!
//! 出力は **LLM へ渡す文脈**であって、コンパイルできるソースではない。
//! Rust の生文字列 (`r#"…"#`)・Python の docstring・テンプレートリテラルの
//! 入れ子は完全には追えない。**元のファイルを書き換えることは決して無い**
//! ので、取りこぼしの被害は「文脈が少し汚い」で止まる。

use std::sync::OnceLock;

use regex::Regex;
use serde_json::Value;

/// 言語ごとのコメント記法。
pub struct CommentStyle {
    pub line: &'static [&'static str],
    pub block: &'static [(&'static str, &'static str)],
}

/// 拡張子 → コメント記法。知らない拡張子は**何も外さない**
/// (知らないものを削るより、削らないほうが安全)。
pub fn comment_style(ext: &str) -> CommentStyle {
    match ext {
        "rs" | "js" | "jsx" | "ts" | "tsx" | "mjs" | "cjs" | "java" | "c" | "h" | "cpp" | "cc"
        | "hpp" | "go" | "swift" | "kt" | "kts" | "scala" | "cs" | "dart" | "php" | "css"
        | "scss" | "less" | "proto" | "zig" | "m" | "mm" | "json" | "jsonc" | "json5" => {
            CommentStyle {
                line: &["//"],
                block: &[("/*", "*/")],
            }
        }
        "py" | "rb" | "sh" | "bash" | "zsh" | "fish" | "yaml" | "yml" | "toml" | "pl" | "r"
        | "jl" | "ex" | "exs" | "tf" | "nix" | "mk" | "cmake" | "dockerfile" | "gitignore"
        | "env" => CommentStyle {
            line: &["#"],
            block: &[],
        },
        "sql" => CommentStyle {
            line: &["--"],
            block: &[("/*", "*/")],
        },
        "lua" => CommentStyle {
            line: &["--"],
            block: &[("--[[", "]]")],
        },
        "html" | "htm" | "xml" | "vue" | "svelte" | "svg" => CommentStyle {
            line: &[],
            block: &[("<!--", "-->")],
        },
        _ => CommentStyle {
            line: &[],
            block: &[],
        },
    }
}

fn starts_with_at(chars: &[char], i: usize, pat: &str) -> bool {
    let mut j = i;
    for pc in pat.chars() {
        if j >= chars.len() || chars[j] != pc {
            return false;
        }
        j += 1;
    }
    true
}

/// コメントを外す。文字列リテラルの内側は保護する。
///
/// **行番号を保つ**: ブロックコメントの中の改行は残すので、外した後でも
/// `L123:` の番号が元のファイルと一致する (outline と付き合わせられる)。
pub fn strip_comments(src: &str, ext: &str) -> String {
    let style = comment_style(ext);
    if style.line.is_empty() && style.block.is_empty() {
        return src.to_string();
    }
    let chars: Vec<char> = src.chars().collect();
    let n = chars.len();
    let mut out = String::with_capacity(src.len());
    let mut i = 0usize;
    let mut in_str: Option<char> = None;

    while i < n {
        let c = chars[i];
        if let Some(q) = in_str {
            out.push(c);
            if c == '\\' && i + 1 < n {
                out.push(chars[i + 1]);
                i += 2;
                continue;
            }
            if c == q {
                in_str = None;
            }
            i += 1;
            continue;
        }
        if let Some((open, close)) = style
            .block
            .iter()
            .find(|(o, _)| starts_with_at(&chars, i, o))
        {
            i += open.chars().count();
            while i < n && !starts_with_at(&chars, i, close) {
                if chars[i] == '\n' {
                    out.push('\n');
                }
                i += 1;
            }
            if i < n {
                i += close.chars().count();
            }
            continue;
        }
        if style.line.iter().any(|p| starts_with_at(&chars, i, p)) {
            // shebang は実行に要る情報なので残す
            if i == 0 && starts_with_at(&chars, i, "#!") {
                while i < n && chars[i] != '\n' {
                    out.push(chars[i]);
                    i += 1;
                }
                continue;
            }
            while i < n && chars[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '"' || c == '\'' || c == '`' {
            in_str = Some(c);
            out.push(c);
            i += 1;
            continue;
        }
        out.push(c);
        i += 1;
    }
    out
}

/// 行末の空白を落とし、連続する空行を 1 行へ畳む。
///
/// **改行は `\n` へ揃う** (`lines()` が `\r\n` も 1 行として食うため)。
/// CRLF のファイルを渡しても行がずれない。
pub fn collapse_blank(src: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    for line in src.lines() {
        let t = line.trim_end();
        if t.is_empty() {
            blank_run += 1;
            if blank_run <= 1 && !out.is_empty() {
                out.push("");
            }
        } else {
            blank_run = 0;
            out.push(t);
        }
    }
    while out.last() == Some(&"") {
        out.pop();
    }
    out.join("\n")
}

/// 同じ行が 3 回以上続いたら 1 行 + 反復の印へ畳む。
pub fn dedupe_lines(src: &str) -> String {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < lines.len() {
        let mut j = i + 1;
        while j < lines.len() && lines[j] == lines[i] {
            j += 1;
        }
        let run = j - i;
        if run >= 3 && !lines[i].trim().is_empty() {
            out.push(lines[i].to_string());
            out.push(format!("… (same line ×{run})"));
        } else {
            out.extend(lines[i..j].iter().map(|l| (*l).to_string()));
        }
        i = j;
    }
    out.join("\n")
}

/// 行の内側の空白の連なりを 1 つへ畳む (**行頭の字下げは残す**)。
pub fn collapse_inner_spaces(src: &str) -> String {
    src.lines()
        .map(|line| {
            let indent_len = line.len() - line.trim_start().len();
            let (indent, rest) = line.split_at(indent_len);
            let mut collapsed = String::with_capacity(rest.len());
            let mut prev_space = false;
            for c in rest.chars() {
                if c == ' ' || c == '\t' {
                    if !prev_space {
                        collapsed.push(' ');
                    }
                    prev_space = true;
                } else {
                    prev_space = false;
                    collapsed.push(c);
                }
            }
            format!("{indent}{}", collapsed.trim_end())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 構造 (outline) ──────────────────────────────────────────────

static SIG_RE: OnceLock<Regex> = OnceLock::new();
static ARROW_RE: OnceLock<Regex> = OnceLock::new();
static METHOD_RE: OnceLock<Regex> = OnceLock::new();
static NOT_METHOD_RE: OnceLock<Regex> = OnceLock::new();
static WRAPPED_SIG_RE: OnceLock<Regex> = OnceLock::new();
static MD_RE: OnceLock<Regex> = OnceLock::new();

fn sig_re() -> &'static Regex {
    SIG_RE.get_or_init(|| {
        Regex::new(
            r#"^\s*(?:(?:pub(?:\([^)]*\))?|export|default|public|private|protected|internal|package|static|async|abstract|final|sealed|override|open|extern(?:\s+"[^"]*")?|unsafe|inline|virtual|declare|partial|data|suspend)\s+)*(?:fn|func|function|def|class|interface|struct|enum|trait|impl|type|protocol|extension|module|namespace|object|record|macro_rules!)\b"#,
        )
        .expect("組み込みの正規表現")
    })
}

/// TypeScript / Java / C# / Kotlin / Swift のクラスメソッドは**キーワードを
/// 持たない** (`frame(dt: number): void {`)。[`sig_re`] はこれを素の呼び出しと
/// 見分けられないので、形 (字下げ → 修飾子 → 名前 → 引数列 → `{` か `;`) で拾う。
fn method_re() -> &'static Regex {
    METHOD_RE.get_or_init(|| {
        const MODS: &str = r"(?:(?:pub|public|private|protected|internal|static|async|abstract|override|final|readonly|get|set|open|suspend)\s+)*";
        const ARGS: &str = r"\((?:[^()]|\([^()]*\))*\)";
        Regex::new(&format!(
            concat!(
                r"^[ \t]+{mods}(?:\*\s*)?[A-Za-z_$][\w$]*\s*(?:<[^>()]*>)?\s*{args}\s*(?::[^={{;]+)?\s*[{{;]\s*$",
                r"|^[ \t]+{mods}(?:\*\s*)?[A-Za-z_$][\w$]*\s*(?:<[^>()]*>)?\s*{args}\s*:[^={{;]+;?\s*$",
            ),
            mods = MODS,
            args = ARGS,
        ))
        .expect("組み込みの正規表現")
    })
}

/// 名前と開き括弧だけの行。**宣言の引数列が折り返した行**でもあり、
/// **呼び出しの引数列が折り返した行**でもある。1 行だけでは区別できないので
/// [`wrapped_signature_opens_body`] が閉じ側を見る。
fn wrapped_sig_re() -> &'static Regex {
    WRAPPED_SIG_RE.get_or_init(|| {
        Regex::new(
            r"^[ \t]+(?:(?:pub|public|private|protected|internal|static|async|abstract|override|final|readonly|get|set|open|suspend)\s+)*(?:\*\s*)?[A-Za-z_$][\w$]*\s*(?:<[^>()]*>)?\s*\(\s*$",
        )
        .expect("組み込みの正規表現")
    })
}

/// 折り返した引数列の閉じ側を、何行先まで探すか。これより長い引数列は
/// 誰も読まないので構造として扱わない。
const MAX_SIGNATURE_LOOKAHEAD: usize = 40;

/// `start` から始まる折り返した引数列が**宣言**のものか。
///
/// 決めるのは閉じ括弧のある行だけ: 宣言は本体へ続く (`) {`) か返り値を
/// 名乗る (`): void`)。呼び出しはそこで終わる (`)` / `),` / `);`)。
fn wrapped_signature_opens_body(lines: &[&str], start: usize) -> bool {
    let mut parens = 0i32;
    for line in lines.iter().skip(start).take(MAX_SIGNATURE_LOOKAHEAD) {
        for c in line.chars() {
            match c {
                '(' => parens += 1,
                ')' => parens -= 1,
                _ => {}
            }
        }
        if parens <= 0 {
            let tail = line.rsplit(')').next().unwrap_or("");
            return tail.contains('{') || tail.trim_start().starts_with(':');
        }
    }
    false
}

/// [`method_re`] が拾ってしまう制御構文 (`if (ok) {` / `} catch (e) {`)。
fn not_method_re() -> &'static Regex {
    NOT_METHOD_RE.get_or_init(|| {
        Regex::new(
            r"^[ \t]*[});\]]*\s*(?:if|for|while|switch|catch|return|else|do|with|await|new|typeof|throw|yield|case|default|delete|void|in|of)\b",
        )
        .expect("組み込みの正規表現")
    })
}

fn arrow_re() -> &'static Regex {
    ARROW_RE.get_or_init(|| {
        Regex::new(
            r"^\s*(?:export\s+)?(?:const|let|var)\s+[A-Za-z_$][\w$]*\s*(?::[^=]+)?=\s*(?:async\s*)?(?:\([^)]*\)|[A-Za-z_$][\w$]*)\s*=>",
        )
        .expect("組み込みの正規表現")
    })
}

fn md_re() -> &'static Regex {
    MD_RE.get_or_init(|| Regex::new(r"^#{1,6}\s").expect("組み込みの正規表現"))
}

/// その行が関数・クラス・メソッド・アロー束縛を宣言しているか。
///
/// **参照分類 (`tools::refs`) と同じ判定を共有する。** 2 つ持つと
/// 「outline には出るのに定義として数えられない」というずれが出る。
pub fn is_signature(line: &str, is_md: bool) -> bool {
    if is_md {
        return md_re().is_match(line);
    }
    sig_re().is_match(line)
        || arrow_re().is_match(line)
        || (method_re().is_match(line) && !not_method_re().is_match(line))
}

/// 前後の行も見られる版の [`is_signature`]。ファイル全体を持っている
/// 呼び出し側 (outline / 参照分類) は**必ずこちらを使う** — 折り返した
/// 呼び出しを構造として報告しないため。
pub fn is_signature_at(lines: &[&str], i: usize, is_md: bool) -> bool {
    let line = lines[i];
    if is_signature(line, is_md) {
        return true;
    }
    !is_md
        && wrapped_sig_re().is_match(line)
        && !not_method_re().is_match(line)
        && wrapped_signature_opens_body(lines, i)
}

/// outline の 1 行に載せる最大文字数。
const OUTLINE_MAX_CHARS: usize = 160;

/// 構造の行だけを行番号つきで返す。構造が 1 つも無ければ `None`
/// (設定ファイル・データファイルはここへ落ちる)。
pub fn outline_opt(src: &str, ext: &str) -> Option<String> {
    let mut out: Vec<String> = Vec::new();
    let is_md = matches!(ext, "md" | "mdx" | "markdown");
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if is_signature_at(&lines, i, is_md) {
            out.push(format!(
                "L{}: {}",
                i + 1,
                ellipsize(line.trim_end(), OUTLINE_MAX_CHARS)
            ));
        }
    }
    if out.is_empty() {
        return None;
    }
    Some(out.join("\n"))
}

/// 構造が無いときに理由を返す版の [`outline_opt`]。
pub fn outline(src: &str, ext: &str) -> String {
    outline_opt(src, ext)
        .unwrap_or_else(|| "(no signatures/headings found — try strategy=slim)".to_string())
}

/// 文字数で切って `…` を足す。**文字境界で切る** (バイトで切ると
/// マルチバイトの途中で panic する)。
pub fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect::<String>() + "…"
}

// ── JSON ────────────────────────────────────────────────────────

/// JSON を刈るときの上限。
#[derive(Clone, Copy, Debug)]
pub struct JsonLimits {
    pub max_depth: usize,
    pub max_array: usize,
    pub max_string: usize,
}

impl Default for JsonLimits {
    fn default() -> Self {
        Self {
            max_depth: 6,
            max_array: 20,
            max_string: 200,
        }
    }
}

/// JSON を刈る: 深さ上限・配列の先頭 N 件・長い文字列の打ち切り。
///
/// **落としたものは必ず数で残す** (`…+95 more items`)。黙って消すと
/// 受け取った側が「これで全部だ」と読む。
pub fn prune_json(v: &Value, depth: usize, o: &JsonLimits) -> Value {
    match v {
        Value::String(s) => {
            let len = s.chars().count();
            if len > o.max_string {
                Value::String(format!(
                    "{}…[+{} chars]",
                    s.chars().take(o.max_string).collect::<String>(),
                    len - o.max_string
                ))
            } else {
                v.clone()
            }
        }
        Value::Array(arr) => {
            if depth == 0 {
                return Value::String(format!("[…{} items]", arr.len()));
            }
            let mut out: Vec<Value> = arr
                .iter()
                .take(o.max_array)
                .map(|x| prune_json(x, depth - 1, o))
                .collect();
            if arr.len() > o.max_array {
                out.push(Value::String(format!(
                    "…+{} more items",
                    arr.len() - o.max_array
                )));
            }
            Value::Array(out)
        }
        Value::Object(m) => {
            if depth == 0 {
                return Value::String(format!("{{…{} keys}}", m.len()));
            }
            let mut out = serde_json::Map::new();
            for (k, val) in m {
                out.insert(k.clone(), prune_json(val, depth - 1, o));
            }
            Value::Object(out)
        }
        _ => v.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn strips_rust_comments_but_not_strings() {
        let src = "// header\nfn main() { /* block */ let s = \"// not a comment\"; }\n";
        let out = strip_comments(src, "rs");
        assert!(!out.contains("header"));
        assert!(!out.contains("block"));
        assert!(out.contains("// not a comment"));
        assert!(out.contains("fn main()"));
    }

    #[test]
    fn strips_python_comments_keeps_shebang() {
        let src = "#!/usr/bin/env python\n# comment\nx = 1  # trailing\ns = \"# not comment\"\n";
        let out = strip_comments(src, "py");
        assert!(out.contains("#!/usr/bin/env python"));
        assert!(!out.contains("# comment"));
        assert!(out.contains("\"# not comment\""));
    }

    /// 知らない拡張子では**何も外さない**。
    #[test]
    fn 知らない拡張子はそのまま返す() {
        let src = "// これはコメントに見えるが記法が不明\n";
        assert_eq!(strip_comments(src, "とんでもない拡張子"), src);
        assert_eq!(strip_comments(src, ""), src);
    }

    /// ブロックコメントを外しても**行番号がずれない**。
    #[test]
    fn コメントを外しても行番号が動かない() {
        let src = "fn a() {}\n/* 1\n2\n3 */\nfn b() {}\n";
        let out = strip_comments(src, "rs");
        assert_eq!(out.lines().count(), src.lines().count());
        assert_eq!(out.lines().nth(4), Some("fn b() {}"));
    }

    #[test]
    fn collapses_blanks_and_crlf() {
        assert_eq!(collapse_blank("a\n\n\n\nb   \n\n"), "a\n\nb");
        // CRLF を渡しても行がずれず、改行は \n へ揃う
        assert_eq!(collapse_blank("a\r\n\r\n\r\nb\r\n"), "a\n\nb");
    }

    #[test]
    fn dedupes_repeats() {
        let out = dedupe_lines("x\nx\nx\nx\ny");
        assert!(out.contains("… (same line ×4)"));
        assert!(out.contains('y'));
        // 2 回だけなら畳まない (印のほうが長い)
        assert_eq!(dedupe_lines("x\nx\ny"), "x\nx\ny");
    }

    #[test]
    fn collapse_inner_spaces_keeps_indent() {
        assert_eq!(collapse_inner_spaces("    a    b  c"), "    a b c");
    }

    #[test]
    fn outline_finds_signatures() {
        let src =
            "use std;\n\npub fn hello(a: u32) -> u32 {\n  a\n}\nstruct Foo;\nconst X: u8 = 1;\n";
        let out = outline(src, "rs");
        assert!(out.contains("L3: pub fn hello"));
        assert!(out.contains("L6: struct Foo;"));
        assert!(!out.contains("use std"));
    }

    /// 折り返した**呼び出し**を構造として報告しない。
    #[test]
    fn outline_skips_a_call_whose_arguments_wrap() {
        let src = "\
export class C {
  run(): void {
    someLongFunctionCall(
      argumentOne,
    )
  }
  static async create(
    device: GPUDevice,
  ): Promise<C> {
    return new C()
  }
  bodyless(
    n: number,
  ): void
}
";
        let out = outline(src, "ts");
        assert!(!out.contains("someLongFunctionCall"), "got: {out}");
        assert!(out.contains("static async create("), "got: {out}");
        assert!(out.contains("bodyless("), "got: {out}");
        assert!(out.contains("run(): void {"), "got: {out}");
    }

    #[test]
    fn outline_markdown_headings_and_none_for_data() {
        let out = outline("# Title\ntext\n## Sub\n", "md");
        assert!(out.contains("L1: # Title"));
        assert!(out.contains("L3: ## Sub"));
        assert!(outline_opt("{\"a\": 1}", "json").is_none());
        assert!(outline("{\"a\": 1}", "json").contains("no signatures"));
    }

    /// 長い行はマルチバイトの**文字境界**で切る (バイトで切ると panic する)。
    #[test]
    fn 長い行は文字境界で切る() {
        let long = "日".repeat(300);
        let cut = ellipsize(&long, 160);
        assert_eq!(cut.chars().count(), 161);
        assert!(cut.ends_with('…'));
        assert_eq!(ellipsize("短い", 160), "短い");
        let src = format!("pub fn f() {{ // {long}\n");
        assert!(outline(&src, "rs").chars().count() < 200);
    }

    #[test]
    fn prunes_json() {
        let v = json!({
            "big": (0..100).collect::<Vec<i32>>(),
            "long": "x".repeat(500),
            "deep": {"a": {"b": {"c": {"d": 1}}}}
        });
        let o = JsonLimits {
            max_depth: 3,
            max_array: 5,
            max_string: 10,
        };
        let s = serde_json::to_string(&prune_json(&v, o.max_depth, &o)).unwrap();
        assert!(s.contains("…+95 more items"));
        assert!(s.contains("…[+490 chars]"));
        assert!(s.contains("1 keys"));
    }

    /// 深い入れ子でも**再帰の深さは `max_depth` で止まる**。
    #[test]
    fn 深い入れ子は上限で止まる() {
        let mut v = json!(1);
        for _ in 0..500 {
            v = json!({ "n": v });
        }
        let o = JsonLimits {
            max_depth: 4,
            ..JsonLimits::default()
        };
        let s = serde_json::to_string(&prune_json(&v, o.max_depth, &o)).unwrap();
        assert!(s.contains("keys}"), "上限で畳めていない: {s}");
        assert!(s.len() < 200, "上限を超えて降りている ({} bytes)", s.len());
    }
}
