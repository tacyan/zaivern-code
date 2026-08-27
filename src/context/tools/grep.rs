//! 検索の道具。`token-slim-mcp` の `grep_slim` にあたる。
//!
//! 出す形は `path:line:本文` の 1 行だけ。前後の文脈も、区切りの罫線も、
//! ファイル名の見出しも出さない — **どれも「どこにあるか」を答えるのに
//! 要らない情報**で、件数ぶんの倍率で効いてくる。
//!
//! 絞り込み (`exclude` / `exclude_tests`) が構造的な検索でいちばん効く。
//! 「この関数を誰が呼んでいるか」でテストが 8 割を占めるのはよくある。

use regex::RegexBuilder;

use super::{Rendered, ToolContext};
use crate::context::metrics::estimate_tokens;
use crate::context::optimizer::ellipsize;
use crate::context::walk::{self, Filter};
use crate::context::ContextError;

/// 検索の指定。
#[derive(Clone, Debug)]
pub struct SearchParams {
    /// 正規表現 (`literal` を立てると素の文字列)。
    pub pattern: String,
    /// 大文字小文字を無視するか。
    pub ignore_case: bool,
    /// パターンを素の文字列として扱うか。
    pub literal: bool,
    /// 走査の絞り込み。
    pub filter: Filter,
    /// 出す件数の上限。`None` なら [`super::super::ContextLimits::max_results`]。
    pub max_results: Option<usize>,
}

impl SearchParams {
    /// パターンだけを与えた既定の指定。
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            pattern: pattern.into(),
            ignore_case: false,
            literal: false,
            filter: Filter::default(),
            max_results: None,
        }
    }
}

/// 1 行に載せる最大文字数。
const MAX_LINE_CHARS: usize = 200;

/// 検索する。
pub fn run(
    cx: &ToolContext,
    root: &std::path::Path,
    params: &SearchParams,
) -> Result<Rendered, ContextError> {
    if params.pattern.is_empty() {
        return Err(ContextError::BadRequest("pattern is empty".into()));
    }
    let start = cx.workspace.resolve(root)?;
    let max_results = params.max_results.unwrap_or(cx.limits.max_results).max(1);

    let pat = if params.literal {
        regex::escape(&params.pattern)
    } else {
        params.pattern.clone()
    };
    // **`size_limit` を置く。** 利用者 (やエージェント) の書いた正規表現は
    // 組み立てただけでメモリを食い尽くしうる (`a{1000}{1000}` の類)。
    // regex はバックトラックしないので時間は線形だが、DFA の大きさは別問題。
    let re = RegexBuilder::new(&pat)
        .case_insensitive(params.ignore_case)
        .size_limit(1 << 22)
        .build()
        .map_err(|e| ContextError::BadRequest(format!("invalid regex: {e}")))?;

    let walked = walk::collect(cx.workspace, &start, &params.filter);
    let mut results: Vec<String> = Vec::new();
    let mut scanned = 0usize;
    let mut original_tokens = 0usize;
    let mut capped = false;

    'outer: for file in &walked.files {
        let Ok(meta) = std::fs::metadata(file.as_path()) else {
            continue;
        };
        if meta.len() > walk::MAX_FILE_BYTES {
            continue;
        }
        let Ok(raw) = std::fs::read(file.as_path()) else {
            continue;
        };
        if walk::is_binary(&raw) {
            continue;
        }
        scanned += 1;
        let content = String::from_utf8_lossy(&raw);
        for (ln, line) in content.lines().enumerate() {
            if re.is_match(line) {
                // 「素直に読んだら何トークンだったか」= 当たった行そのもの。
                // ファイル全体を分母にすると削減率が実態より良く見える。
                original_tokens += estimate_tokens(line) + 1;
                results.push(format!(
                    "{}:{}:{}",
                    file.rel(),
                    ln + 1,
                    ellipsize(line.trim(), MAX_LINE_CHARS)
                ));
                if results.len() >= max_results {
                    capped = true;
                    break 'outer;
                }
            }
        }
    }

    // 絞り込みで落ちた数と、効いている絞り込みそのものを必ず名乗る。
    // 0 件のときに「どこにも無い」と読まれるのを防ぐ唯一の手段。
    let mut filters: Vec<String> = Vec::new();
    if !params.filter.include.is_empty() {
        filters.push(format!("include={}", params.filter.include.join(",")));
    }
    if !params.filter.exclude.is_empty() {
        filters.push(format!(
            "exclude={}",
            summarize_globs(&params.filter.exclude)
        ));
    }
    if !params.filter.exts.is_empty() {
        filters.push(format!("ext={}", params.filter.exts.join(",")));
    }
    if walked.filtered > 0 {
        filters.push(format!("{} files skipped", walked.filtered));
    }
    if walked.capped {
        filters.push(format!("walk capped at {}", walk::MAX_FILES_SCANNED));
    }
    let detail = format!(
        "/{}/ in {}: {} matches, {scanned} files scanned{}{}",
        params.pattern,
        start.rel(),
        results.len(),
        if filters.is_empty() {
            String::new()
        } else {
            format!(", {}", filters.join(", "))
        },
        if capped {
            format!(" [capped at {max_results}]")
        } else {
            String::new()
        }
    );
    let hint = if capped {
        " — narrow the pattern, add an ext filter or exclude globs".to_string()
    } else {
        String::new()
    };
    Ok(Rendered {
        detail,
        body: results.join("\n"),
        original_tokens: original_tokens.max(estimate_tokens(&results.join("\n"))),
        hint,
    })
}

/// 除外の一覧を 3 件 + 残数へ畳む (16 件の glob をそのまま出すと本末転倒)。
fn summarize_globs(globs: &[String]) -> String {
    let shown: Vec<&str> = globs.iter().take(3).map(String::as_str).collect();
    let more = globs.len().saturating_sub(shown.len());
    if more > 0 {
        format!("{},+{more}", shown.join(","))
    } else {
        shown.join(",")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests_support::Lab;

    fn lab() -> Lab {
        let lab = Lab::new("grep");
        lab.write("src/a.rs", "fn target() {}\nfn other() {}\n");
        lab.write("src/b.toml", "target = 1\n");
        lab.write("tests/c.rs", "fn t() { target(); }\n");
        lab
    }

    #[test]
    fn 件数とファイル数を正直に出す() {
        let lab = lab();
        let r = lab.grep(&SearchParams::new("target"));
        assert_eq!(r.body.lines().count(), 3);
        assert!(r.detail.contains("3 matches"), "{}", r.detail);
        assert!(r.detail.contains("3 files scanned"), "{}", r.detail);
    }

    #[test]
    fn 拡張子と包含と除外が効く() {
        let lab = lab();
        let mut p = SearchParams::new("target");
        p.filter = Filter::default().with_exts("rs");
        assert_eq!(lab.grep(&p).body.lines().count(), 2);

        let mut p = SearchParams::new("target");
        p.filter = Filter::default().with_exts("rs").exclude_tests();
        let r = lab.grep(&p);
        assert_eq!(r.body.lines().count(), 1);
        assert!(r.body.starts_with("src/a.rs:1:"), "{}", r.body);
        assert!(r.detail.contains("exclude="), "絞り込みを名乗っていない");
        assert!(r.detail.contains("+"), "除外の件数を畳んでいない");

        let mut p = SearchParams::new("target");
        p.filter = Filter {
            include: vec!["tests/**".into()],
            ..Filter::default()
        };
        let r = lab.grep(&p);
        assert_eq!(r.body.lines().count(), 1);
        assert!(r.detail.contains("include=tests/**"));
        assert!(r.detail.contains("files skipped"), "落とした数が出ていない");
    }

    #[test]
    fn 件数の上限で打ち切ったことを言う() {
        let lab = lab();
        let mut p = SearchParams::new("target");
        p.max_results = Some(2);
        let r = lab.grep(&p);
        assert_eq!(r.body.lines().count(), 2);
        assert!(r.detail.contains("[capped at 2]"), "{}", r.detail);
        assert!(r.hint.contains("narrow"), "次の一歩の案内が無い");
    }

    #[test]
    fn 正規表現と素の文字列と大小無視() {
        let lab = Lab::new("grep-re");
        lab.write("a.rs", "fn a1() {}\nfn A2() {}\nlet x = a.b;\n");
        assert_eq!(
            lab.grep(&SearchParams::new(r"fn a\d")).body.lines().count(),
            1
        );
        let mut ci = SearchParams::new(r"fn a\d");
        ci.ignore_case = true;
        assert_eq!(lab.grep(&ci).body.lines().count(), 2);
        // 素の文字列指定では `.` がワイルドカードにならない
        let mut lit = SearchParams::new("a.b");
        lit.literal = true;
        assert_eq!(lab.grep(&lit).body.lines().count(), 1);
    }

    #[test]
    fn 壊れた正規表現と空のパターンは断る() {
        let lab = lab();
        assert!(matches!(
            lab.grep_result(&SearchParams::new("(")),
            Err(ContextError::BadRequest(_))
        ));
        assert!(matches!(
            lab.grep_result(&SearchParams::new("")),
            Err(ContextError::BadRequest(_))
        ));
        // 組み立てただけでメモリを食う指定も断る (時間ではなく大きさの上限)
        assert!(matches!(
            lab.grep_result(&SearchParams::new(r"(?:a{500}){500}")),
            Err(ContextError::BadRequest(_))
        ));
    }

    #[test]
    fn 見つからないときも絞り込みを名乗る() {
        let lab = lab();
        let mut p = SearchParams::new("存在しない語");
        p.filter = Filter::default().exclude_tests();
        let r = lab.grep(&p);
        assert!(r.body.is_empty());
        assert!(r.detail.contains("0 matches"));
        assert!(
            r.detail.contains("exclude="),
            "0 件のとき「どこにも無い」と読まれる: {}",
            r.detail
        );
    }
}
