//! 素のテキストを畳む道具。`token-slim-mcp` の `text_slim` / `token_count`
//! にあたる。長いログや CLI の出力を文脈へ引用する前に通す。

use super::{Rendered, ToolContext};
use crate::context::metrics::estimate_tokens;
use crate::context::optimizer;
use crate::context::walk;
use crate::context::ContextError;

/// 畳み方の強さ。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TextLevel {
    /// 行末の空白を落とし、連続する空行を 1 行へ。
    #[default]
    Normal,
    /// さらに、行の内側の空白の連なりと、繰り返す行を畳む。
    Aggressive,
}

impl TextLevel {
    /// 出力に載る安定 ID。
    pub fn id(self) -> &'static str {
        match self {
            TextLevel::Normal => "normal",
            TextLevel::Aggressive => "aggressive",
        }
    }

    /// 文字列から。知らない語は `None`。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "normal" => Some(TextLevel::Normal),
            "aggressive" => Some(TextLevel::Aggressive),
            _ => None,
        }
    }
}

/// テキストの入力元。
pub enum TextInput<'a> {
    Text(&'a str),
    File(&'a std::path::Path),
}

/// テキストを畳む。
pub fn run(cx: &ToolContext, input: TextInput, level: TextLevel) -> Result<Rendered, ContextError> {
    let (label, text) = read(cx, input)?;
    let original_tokens = estimate_tokens(&text);
    let mut body = optimizer::collapse_blank(&text);
    if level == TextLevel::Aggressive {
        body = optimizer::collapse_inner_spaces(&body);
        body = optimizer::dedupe_lines(&body);
    }
    Ok(Rendered {
        detail: format!("{label} level={}", level.id()),
        body,
        original_tokens,
        hint: String::new(),
    })
}

/// トークン数を見積もるだけ (**何も畳まない**)。
///
/// 「文脈へ貼る価値があるか」を決めるための道具なので、削減は 0 で正しい。
pub fn count(cx: &ToolContext, input: TextInput) -> Result<Rendered, ContextError> {
    let (label, text) = read(cx, input)?;
    let chars = text.chars().count();
    let ascii = text.chars().filter(char::is_ascii).count();
    let body = format!(
        "{label}: ~{} tokens (chars={chars}, ascii={ascii}, non-ascii={}, lines={}) heuristic ±20%",
        estimate_tokens(&text),
        chars - ascii,
        text.lines().count()
    );
    Ok(Rendered::plain(label, body))
}

fn read(cx: &ToolContext, input: TextInput) -> Result<(String, String), ContextError> {
    Ok(match input {
        TextInput::Text(t) => ("text".to_string(), t.to_string()),
        TextInput::File(p) => {
            let sp = cx.workspace.resolve(p)?;
            let body = walk::read_text(&sp)?;
            (sp.rel().to_string(), body)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests_support::Lab;

    #[test]
    fn 空行と行末の空白を畳む() {
        let lab = Lab::new("text-normal");
        let r = lab.text("a   \n\n\n\nb\n\n", TextLevel::Normal);
        assert_eq!(r.body, "a\n\nb");
        assert!(r.original_tokens >= estimate_tokens(&r.body));
        assert!(r.detail.contains("level=normal"));
    }

    #[test]
    fn 強い畳み方は繰り返しと内側の空白も畳む() {
        let lab = Lab::new("text-aggressive");
        let src = format!("  keep    this\n{}end\n", "same\n".repeat(20));
        let src = src.as_str();
        let normal = lab.text(src, TextLevel::Normal);
        assert!(normal.body.contains("keep    this"));
        let hard = lab.text(src, TextLevel::Aggressive);
        assert!(hard.body.contains("  keep this"), "{}", hard.body);
        assert!(hard.body.contains("… (same line ×20)"), "{}", hard.body);
        assert!(hard.body.contains("end"));
        assert!(estimate_tokens(&hard.body) < estimate_tokens(&normal.body));
    }

    /// 大きなログでも線形に畳めて、削減が実際に出る。
    #[test]
    fn 巨大なログを畳む() {
        let lab = Lab::new("text-huge");
        let mut src = String::new();
        for i in 0..20_000 {
            src.push_str("   INFO   waiting for the lock   \n");
            if i % 1000 == 0 {
                src.push_str(&format!("STEP {i}\n\n\n"));
            }
        }
        let r = lab.text(&src, TextLevel::Aggressive);
        assert!(r.body.contains("same line ×"), "繰り返しを畳んでいない");
        assert!(r.body.contains("STEP 19000"), "末尾が消えた");
        assert!(
            estimate_tokens(&r.body) * 50 < r.original_tokens,
            "削減が出ていない ({} → {})",
            r.original_tokens,
            estimate_tokens(&r.body)
        );
    }

    #[test]
    fn 数えるだけの道具は何も畳まない() {
        let lab = Lab::new("text-count");
        let r = lab.count("abcd日本\n");
        // ascii 5 (改行込み) / 4 → 2 + 非 ascii 2 = 4
        assert!(r.body.contains("~4 tokens"), "{}", r.body);
        assert!(r.body.contains("non-ascii=2"), "{}", r.body);
        assert!(r.body.contains("lines=1"), "{}", r.body);
        assert_eq!(r.original_tokens, estimate_tokens(&r.body), "削減 0 のはず");
    }

    #[test]
    fn ファイルからも読めて境界を守る() {
        let lab = Lab::new("text-file");
        lab.write("log.txt", "a\n\n\n\nb\n");
        assert_eq!(lab.text_file("log.txt", TextLevel::Normal).body, "a\n\nb");
        assert!(matches!(
            lab.text_file_result("../outside.txt", TextLevel::Normal),
            Err(ContextError::OutsideWorkspace { .. })
        ));
    }

    #[test]
    fn 強さの往復() {
        assert_eq!(TextLevel::parse("normal"), Some(TextLevel::Normal));
        assert_eq!(TextLevel::parse("aggressive"), Some(TextLevel::Aggressive));
        assert_eq!(TextLevel::parse("つよい"), None);
        assert_eq!(TextLevel::default().id(), "normal");
    }
}
