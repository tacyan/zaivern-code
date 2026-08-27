//! 参照の分類。`token-slim-mcp` の `refs_slim` にあたる。
//!
//! ## grep では答えられない問い
//!
//! 「この関数を誰が呼んでいるか」を grep で聞くと、定義も import も
//! コメントもテストも同じ 1 行として返る。実測では 36 行のうち
//! **本当の呼び出しは 2 行**で、残りはテスト 27・import 3・定義 1 だった。
//! 形で分類すれば、聞かれた問いへ戻せる。索引を持たないので**腐らない**。
//!
//! 分類の中核 ([`RefKind`] / [`SymbolPatterns`] / [`enclosing_symbol`]) は
//! 純粋 — ファイルにも設定にも触らない。走査は [`run`] が行う。

use regex::Regex;

use super::{Rendered, ToolContext};
use crate::context::metrics::estimate_tokens;
use crate::context::optimizer::{self, ellipsize};
use crate::context::walk::{self, Filter, SafePath};
use crate::context::ContextError;

/// その行が記号に対して何をしているか。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    /// `function foo(` / `class foo` / `const foo =` — 宣言している。
    Definition,
    /// 本体コードでの呼び出し。
    Call,
    /// テスト置き場での呼び出し。
    TestCall,
    /// `import { foo }` / `use crate::foo` / `require('…')`。
    Import,
    /// コメントの中。
    Comment,
    /// 呼ばずに名前だけが出てくる (型の位置・文字列・キー名)。
    Mention,
}

impl RefKind {
    /// 出力に載る短い名前。**訳さない** (機械が読む値)。
    pub fn label(self) -> &'static str {
        match self {
            RefKind::Definition => "definition",
            RefKind::Call => "call",
            RefKind::TestCall => "test-call",
            RefKind::Import => "import",
            RefKind::Comment => "comment",
            RefKind::Mention => "mention",
        }
    }

    /// 報告の順 — 答えが先、雑音が後。
    pub const ORDER: [RefKind; 6] = [
        RefKind::Definition,
        RefKind::Call,
        RefKind::TestCall,
        RefKind::Import,
        RefKind::Comment,
        RefKind::Mention,
    ];
}

/// 記号ごとに 1 度だけ組む照合器。
pub struct SymbolPatterns {
    symbol: String,
    call: Regex,
    def: Regex,
    import: Regex,
}

impl SymbolPatterns {
    /// `sym` の照合器を作る。
    pub fn new(sym: &str) -> Result<Self, ContextError> {
        if sym.trim().is_empty() {
            return Err(ContextError::BadRequest("symbol is empty".into()));
        }
        let e = regex::escape(sym);
        let bad =
            |err: regex::Error| ContextError::BadRequest(format!("bad symbol {sym:?}: {err}"));
        let call = Regex::new(&format!(r"\b{e}\s*(?:<[^>()]*>)?\s*\(")).map_err(bad)?;
        // キーワード付きの宣言だけをここで見る。キーワードの無いメソッド
        // (`  frame(): void {`) は optimizer::is_signature_at が見るので、
        // **outline と参照分類が「宣言とは何か」で食い違わない**。
        let def = Regex::new(&format!(
            r"\b(?:fn|func|function|def|class|interface|struct|enum|trait|type|const|let|var|static)\s+{e}\b"
        ))
        .map_err(bad)?;
        let import = Regex::new(
            r"^\s*(?:import\b|export\b.*\bfrom\b|(?:const|let|var)\s.*\brequire\s*\()|^\s*(?:from\s+\S+\s+import\b)|^\s*use\s",
        )
        .map_err(bad)?;
        Ok(Self {
            symbol: sym.to_string(),
            call,
            def,
            import,
        })
    }

    /// `i` 行目の参照を分類する。
    ///
    /// 前後の行も見るのは、折り返した宣言と折り返した呼び出しが**同じ形**
    /// だから。`in_test_file` は**パスから**決める (本体コードの `test` と
    /// いう名前のヘルパをテストと数えないため)。
    pub fn classify(&self, lines: &[&str], i: usize, in_test_file: bool) -> RefKind {
        let line = lines[i];
        let t = line.trim_start();
        if t.starts_with("//") || t.starts_with("/*") || t.starts_with('*') || t.starts_with('#') {
            return RefKind::Comment;
        }
        if self.import.is_match(line) {
            return RefKind::Import;
        }
        // 宣言は呼び出しと同じ形をしているので、先に勝たせる
        // (でなければ全てのメソッドが「自分を呼んでいる」と報告される)
        if self.def.is_match(line) {
            return RefKind::Definition;
        }
        if optimizer::is_signature_at(lines, i, false)
            && signature_name(line).as_deref() == Some(self.symbol.as_str())
        {
            return RefKind::Definition;
        }
        if self.call.is_match(line) {
            return if in_test_file {
                RefKind::TestCall
            } else {
                RefKind::Call
            };
        }
        RefKind::Mention
    }
}

/// 1 行の `{` − `}` と `(` − `)`。コメント・文字列の中は数えない。
fn net_delims(line: &str) -> (i32, i32) {
    let chars: Vec<char> = line.chars().collect();
    let mut braces = 0i32;
    let mut parens = 0i32;
    let mut i = 0usize;
    let mut quote: Option<char> = None;
    while i < chars.len() {
        let c = chars[i];
        match quote {
            Some(q) => {
                if c == '\\' {
                    i += 2;
                    continue;
                }
                if c == q {
                    quote = None;
                }
            }
            None => {
                if c == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                    break;
                }
                match c {
                    '"' | '\'' | '`' => quote = Some(c),
                    '{' => braces += 1,
                    '}' => braces -= 1,
                    '(' => parens += 1,
                    ')' => parens -= 1,
                    _ => {}
                }
            }
        }
        i += 1;
    }
    (braces, parens)
}

/// その宣言行が名乗っている識別子。名前を持たない行は `None`。
pub fn signature_name(line: &str) -> Option<String> {
    static KW: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static BINDING: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    static METHOD: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    let kw = KW.get_or_init(|| {
        Regex::new(
            r"\b(?:fn|func|function|def|class|interface|struct|enum|trait|type|impl|namespace|module|object|record)\s+([A-Za-z_$][\w$]*)",
        )
        .expect("組み込みの正規表現")
    });
    if let Some(c) = kw.captures(line) {
        return Some(c[1].to_string());
    }
    let binding = BINDING.get_or_init(|| {
        Regex::new(r"\b(?:const|let|var|static)\s+([A-Za-z_$][\w$]*)").expect("組み込みの正規表現")
    });
    if let Some(c) = binding.captures(line) {
        return Some(c[1].to_string());
    }
    let method = METHOD.get_or_init(|| {
        Regex::new(
            r"^[ \t]*(?:(?:pub|public|private|protected|internal|static|async|abstract|override|final|readonly|get|set|open|suspend)\s+)*(?:\*\s*)?([A-Za-z_$][\w$]*)\s*(?:<[^>()]*>)?\s*\(",
        )
        .expect("組み込みの正規表現")
    });
    method.captures(line).map(|c| c[1].to_string())
}

/// `line_no` (1 始まり) を本体に含む関数 / メソッド / クラスの名前。
///
/// 波括弧の深さを追うので、**既に閉じた内側の束縛**が後の行を横取りしない。
/// 「直前の宣言行」を素朴に探す実装は、まさにメソッドの居るクラスの中で
/// 間違える。
pub fn enclosing_symbol(src: &str, line_no: usize) -> Option<String> {
    let mut stack: Vec<(String, i32)> = Vec::new();
    let mut depth = 0i32;
    let mut pending: Option<String> = None;
    let mut open_parens = 0i32;
    let lines: Vec<&str> = src.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        if i + 1 > line_no {
            break;
        }
        let name = if optimizer::is_signature_at(&lines, i, false) {
            signature_name(line)
        } else {
            None
        };
        let before = depth;
        let (d_brace, d_paren) = net_delims(line);
        depth += d_brace;
        open_parens = (open_parens + d_paren).max(0);
        if let Some(nm) = name {
            if depth > before {
                stack.push((nm, before));
                pending = None;
            } else if open_parens > 0 {
                // 引数列が折り返している宣言。本体はまだ開いていない
                pending = Some(nm);
            } else {
                pending = None;
            }
        } else if depth > before && open_parens == 0 {
            if let Some(nm) = pending.take() {
                stack.push((nm, before));
            }
        } else if open_parens == 0 {
            pending = None;
        }
        while let Some((_, opened_at)) = stack.last() {
            if depth <= *opened_at {
                stack.pop();
            } else {
                break;
            }
        }
    }
    stack.last().map(|(n, _)| n.clone())
}

// ── 走査 ────────────────────────────────────────────────────────

/// 参照を辿るときの指定。
#[derive(Clone, Debug)]
pub struct RefsParams {
    /// 追う記号。
    pub symbol: String,
    /// 1 = 直接の呼び出し元。2 = そのさらに呼び出し元 (変更の影響範囲)。
    pub depth: usize,
    /// テストの呼び出しも一覧に出すか (既定は数えるだけ)。
    pub include_tests: bool,
    /// 走査の絞り込み。
    pub filter: Filter,
    /// 一覧に出す上限。
    pub max_results: Option<usize>,
}

impl RefsParams {
    /// 記号だけを与えた既定の指定。
    pub fn new(symbol: impl Into<String>) -> Self {
        Self {
            symbol: symbol.into(),
            depth: 1,
            include_tests: false,
            filter: Filter::default(),
            max_results: None,
        }
    }
}

/// 1 度だけ読んだファイル。`depth=2` が**2 度目の走査ではなく 2 度目の
/// 分類**で済むようにする。
struct Loaded {
    rel: String,
    content: String,
    is_test: bool,
}

/// 分類済みの 1 件。
struct Hit {
    rel: String,
    line: usize,
    text: String,
    kind: RefKind,
    enclosing: Option<String>,
}

/// 1 行に載せる最大文字数。
const MAX_LINE_CHARS: usize = 160;

fn load(cx: &ToolContext, start: &SafePath, filter: &Filter) -> Vec<Loaded> {
    let test_globs: Vec<String> = walk::TEST_GLOBS.iter().map(|s| (*s).to_string()).collect();
    walk::collect(cx.workspace, start, filter)
        .files
        .into_iter()
        .filter_map(|f| {
            let meta = std::fs::metadata(f.as_path()).ok()?;
            if meta.len() > walk::MAX_FILE_BYTES {
                return None;
            }
            let raw = std::fs::read(f.as_path()).ok()?;
            if walk::is_binary(&raw) {
                return None;
            }
            Some(Loaded {
                is_test: crate::context::glob::any_match(&test_globs, f.rel()),
                rel: f.rel().to_string(),
                content: String::from_utf8_lossy(&raw).into_owned(),
            })
        })
        .collect()
}

fn scan(files: &[Loaded], sym: &str, resolve_enclosing: bool) -> Result<Vec<Hit>, ContextError> {
    let pats = SymbolPatterns::new(sym)?;
    let word = Regex::new(&format!(r"\b{}\b", regex::escape(sym)))
        .map_err(|e| ContextError::BadRequest(format!("bad symbol {sym:?}: {e}")))?;
    let mut hits = Vec::new();
    for f in files {
        // ファイル単位で先に落とす。ほとんどのファイルはこの記号を持たない
        if !word.is_match(&f.content) {
            continue;
        }
        let lines: Vec<&str> = f.content.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            if !word.is_match(line) {
                continue;
            }
            let kind = pats.classify(&lines, i, f.is_test);
            let enclosing =
                if resolve_enclosing && matches!(kind, RefKind::Call | RefKind::TestCall) {
                    enclosing_symbol(&f.content, i + 1)
                } else {
                    None
                };
            hits.push(Hit {
                rel: f.rel.clone(),
                line: i + 1,
                text: ellipsize(line.trim(), MAX_LINE_CHARS),
                kind,
                enclosing,
            });
        }
    }
    Ok(hits)
}

/// 参照を辿る。
pub fn run(
    cx: &ToolContext,
    root: &std::path::Path,
    params: &RefsParams,
) -> Result<Rendered, ContextError> {
    let start = cx.workspace.resolve(root)?;
    let depth = params.depth.clamp(1, 2);
    let max_results = params.max_results.unwrap_or(cx.limits.max_results).max(1);
    let files = load(cx, &start, &params.filter);
    let hits = scan(&files, &params.symbol, true)?;

    // 素直に grep したら渡ることになる行数 = 分母。
    let original_tokens: usize = hits.iter().map(|h| estimate_tokens(&h.text) + 1).sum();

    let mut counts: Vec<String> = Vec::new();
    for k in RefKind::ORDER {
        let n = hits.iter().filter(|h| h.kind == k).count();
        if n > 0 {
            counts.push(format!("{} {n}", k.label()));
        }
    }
    let summary = if counts.is_empty() {
        "no references".to_string()
    } else {
        counts.join(", ")
    };

    let mut body: Vec<String> = Vec::new();
    let mut listed = 0usize;
    let mut capped = false;
    for k in [RefKind::Definition, RefKind::Call, RefKind::TestCall] {
        if k == RefKind::TestCall && !params.include_tests {
            continue;
        }
        for h in hits.iter().filter(|h| h.kind == k) {
            if listed >= max_results {
                capped = true;
                break;
            }
            let tag = match k {
                RefKind::Definition => "def ",
                RefKind::Call => "call",
                _ => "test",
            };
            let scope = match &h.enclosing {
                Some(e) if e.as_str() != params.symbol.as_str() => format!("  in {e}"),
                _ => String::new(),
            };
            body.push(format!("{tag} {}:{}{scope}: {}", h.rel, h.line, h.text));
            listed += 1;
        }
    }

    if depth >= 2 {
        body.extend(hop2(&files, &hits, &params.symbol)?);
    }

    let detail = format!(
        "{} in {}{}: {summary}; {} files scanned{}",
        params.symbol,
        start.rel(),
        if depth >= 2 { " depth=2" } else { "" },
        files.len(),
        if capped {
            format!(" [listing capped at {max_results}]")
        } else {
            String::new()
        }
    );
    Ok(Rendered {
        detail,
        body: body.join("\n"),
        original_tokens: original_tokens.max(estimate_tokens(&body.join("\n"))),
        hint: String::new(),
    })
}

/// `depth=2`: 呼び出し元の、そのまた呼び出し元。**名前だけ**を出す
/// (正確な位置は 1 ホップ目の行が既に持っている)。
fn hop2(files: &[Loaded], hits: &[Hit], symbol: &str) -> Result<Vec<String>, ContextError> {
    let mut frontier: Vec<String> = hits
        .iter()
        .filter(|h| h.kind == RefKind::Call)
        .filter_map(|h| h.enclosing.clone())
        .collect();
    frontier.sort();
    frontier.dedup();
    frontier.retain(|f| f != symbol);

    let mut out: Vec<String> = Vec::new();
    for caller in &frontier {
        for h in scan(files, caller, true)?
            .iter()
            .filter(|h| h.kind == RefKind::Call)
        {
            let via = h.enclosing.clone().unwrap_or_else(|| "(top level)".into());
            if via == *caller {
                continue; // 再帰は新しい呼び出し元ではない
            }
            let entry = format!("hop2 {via} -> {caller}  ({}:{})", h.rel, h.line);
            if !out.contains(&entry) {
                out.push(entry);
            }
        }
    }
    out.sort();
    if !out.is_empty() {
        return Ok(out);
    }
    // 「これ以上の呼び出し元は無い」と「呼び出し元を割り出せなかった」は
    // 別の答えで、事実なのは前者だけ。スコープの解決は波括弧を追うので、
    // 字下げで囲う言語 (Python / Ruby) では 1 件も割り出せない。
    // そこで「無い」と言うのは、自信を持った誤答になる。
    let calls = hits.iter().filter(|h| h.kind == RefKind::Call).count();
    let attributed = hits
        .iter()
        .filter(|h| h.kind == RefKind::Call && h.enclosing.is_some())
        .count();
    Ok(vec![if calls > 0 && attributed == 0 {
        "hop2 (unavailable: no call site could be attributed to an enclosing function \
— depth=2 needs a brace-delimited language)"
            .to_string()
    } else {
        "hop2 (no further callers)".to_string()
    }])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests_support::Lab;

    fn pats() -> SymbolPatterns {
        SymbolPatterns::new("computeFrameComp").unwrap()
    }

    #[test]
    fn classifies_the_shapes_grep_cannot_tell_apart() {
        let p = pats();
        assert_eq!(
            p.classify(&["export function computeFrameComp(i: I): O {"], 0, false),
            RefKind::Definition
        );
        assert_eq!(
            p.classify(&["  const { a } = computeFrameComp({ dt })"], 0, false),
            RefKind::Call
        );
        assert_eq!(
            p.classify(&["  expect(computeFrameComp(x)).toBe(1)"], 0, true),
            RefKind::TestCall
        );
        assert_eq!(
            p.classify(&["import { computeFrameComp } from './m'"], 0, false),
            RefKind::Import
        );
        assert_eq!(
            p.classify(&["  // computeFrameComp is shared"], 0, false),
            RefKind::Comment
        );
        assert_eq!(
            p.classify(&["  type T = typeof computeFrameComp"], 0, false),
            RefKind::Mention
        );
    }

    #[test]
    fn a_method_declaration_is_not_a_call_to_itself() {
        let p = SymbolPatterns::new("frame").unwrap();
        assert_eq!(
            p.classify(&["  frame(dt: number, time: number): void {"], 0, false),
            RefKind::Definition
        );
        assert_eq!(
            p.classify(&["    this.renderer.frame(dt, time)"], 0, false),
            RefKind::Call
        );
    }

    #[test]
    fn net_delims_ignores_strings_and_comments() {
        assert_eq!(net_delims("class A {").0, 1);
        assert_eq!(net_delims("}").0, -1);
        assert_eq!(net_delims(r#"const s = "{{{" // }}}"#).0, 0);
        assert_eq!(net_delims("const t = `${x}`").0, 0);
        assert_eq!(net_delims("if (a) { b() } else { c() }").0, 0);
    }

    #[test]
    fn signature_name_reads_every_declaration_shape() {
        assert_eq!(
            signature_name("export function foo(a: T) {").as_deref(),
            Some("foo")
        );
        assert_eq!(signature_name("export class Bar {").as_deref(), Some("Bar"));
        assert_eq!(
            signature_name("  private syncLookModes(): void {").as_deref(),
            Some("syncLookModes")
        );
        assert_eq!(
            signature_name("const blurBG = (v: V) =>").as_deref(),
            Some("blurBG")
        );
        assert_eq!(signature_name("  // nothing here"), None);
    }

    #[test]
    fn enclosing_symbol_survives_a_closed_inner_binding() {
        let src = "\
export class R {
  private setup(): void {
    const blurBG = (v: V) => {
      use(v)
    }
    blurBG(x)
  }
  frame(dt: number): void {
    compute({ dt })
  }
}
";
        assert_eq!(enclosing_symbol(src, 4).as_deref(), Some("blurBG"));
        assert_eq!(enclosing_symbol(src, 6).as_deref(), Some("setup"));
        assert_eq!(enclosing_symbol(src, 9).as_deref(), Some("frame"));
        assert_eq!(enclosing_symbol(src, 1).as_deref(), Some("R"));
    }

    #[test]
    fn enclosing_symbol_follows_a_wrapped_argument_list() {
        let src = "\
export class R {
  private constructor(
    private readonly device: GPUDevice,
  ) {
    this.blend = resolveBlendMode(opts.blendMode)
  }
}
";
        assert_eq!(enclosing_symbol(src, 5).as_deref(), Some("constructor"));
        assert_eq!(enclosing_symbol(src, 6).as_deref(), Some("R"));
    }

    #[test]
    fn a_wrapped_call_does_not_own_the_brace_its_callback_opens() {
        let src = "\
export class C {
  run(): void {
    registerHandler(
      'name',
      () => {
        target(1)
      },
    )
    target(2)
  }
}
";
        assert_eq!(enclosing_symbol(src, 6).as_deref(), Some("run"));
        assert_eq!(enclosing_symbol(src, 9).as_deref(), Some("run"));
        // クラス宣言の行そのものはクラスに属する
        assert_eq!(enclosing_symbol(src, 1).as_deref(), Some("C"));
    }

    #[test]
    fn 走査は分類ごとに数えて呼び出しだけ並べる() {
        let lab = Lab::new("refs-run");
        lab.write(
            "src/a.ts",
            "import { target } from './t'\nexport function caller(): void {\n  target(1)\n}\n// target is shared\n",
        );
        lab.write("src/t.ts", "export function target(n: number): void {}\n");
        lab.write("tests/a.spec.ts", "it('x', () => { target(2) })\n");

        let r = lab.refs(&RefsParams::new("target"));
        assert!(r.detail.contains("definition 1"), "{}", r.detail);
        assert!(r.detail.contains("call 1"), "{}", r.detail);
        assert!(r.detail.contains("test-call 1"), "{}", r.detail);
        assert!(r.detail.contains("import 1"), "{}", r.detail);
        assert!(r.detail.contains("comment 1"), "{}", r.detail);
        // 一覧に出るのは定義と本体の呼び出しだけ
        assert!(r.body.contains("call src/a.ts:3  in caller"), "{}", r.body);
        assert!(!r.body.contains("tests/a.spec.ts"), "テストが並んでいる");

        let mut p = RefsParams::new("target");
        p.include_tests = true;
        assert!(lab.refs(&p).body.contains("test tests/a.spec.ts:1"));
    }

    #[test]
    fn 深さ2は呼び出し元の呼び出し元を出す() {
        let lab = Lab::new("refs-hop2");
        lab.write(
            "src/a.ts",
            "export function target(): void {}\nexport function mid(): void {\n  target()\n}\nexport function top(): void {\n  mid()\n}\n",
        );
        let mut p = RefsParams::new("target");
        p.depth = 2;
        let r = lab.refs(&p);
        assert!(r.detail.contains("depth=2"));
        assert!(r.body.contains("hop2 top -> mid"), "{}", r.body);
    }

    /// 波括弧を使わない言語では「呼び出し元なし」と断言しない。
    #[test]
    fn 割り出せないことと無いことを区別する() {
        let lab = Lab::new("refs-python");
        lab.write("a.py", "def caller():\n    target(1)\n");
        let mut p = RefsParams::new("target");
        p.depth = 2;
        let r = lab.refs(&p);
        assert!(r.body.contains("hop2 (unavailable"), "{}", r.body);
    }

    #[test]
    fn 空の記号は断る() {
        let lab = Lab::new("refs-empty");
        lab.write("a.rs", "fn a() {}\n");
        assert!(matches!(
            lab.refs_result(&RefsParams::new("  ")),
            Err(ContextError::BadRequest(_))
        ));
    }
}
