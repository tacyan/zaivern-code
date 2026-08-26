//! ファイルを読む道具。`token-slim-mcp` の `read_slim` にあたる。
//!
//! ## 4 つの戦略
//!
//! | 戦略 | 何を返すか |
//! |---|---|
//! | `Raw` | そのまま |
//! | `Slim` | コメントを外し、空行を畳む |
//! | `Outline` | 構造の行だけ (関数 / クラス / 見出し + 行番号) |
//! | `Auto` | 大きくて構造のあるファイルは `Outline`、それ以外は `Slim` |
//!
//! **行域を指定したときは決して outline しない** — 行域の指定は
//! 「outline を見て、次にここを読む」という手順の 2 歩目そのものなので、
//! そこで構造だけ返すと永久に本文へ辿り着けない。

use super::{Rendered, ToolContext};
use crate::context::metrics::estimate_tokens;
use crate::context::optimizer;
use crate::context::walk;
use crate::context::{ContextError, ContextStrategy};

/// 読み方の指定。
#[derive(Clone, Copy, Debug)]
pub struct ReadParams {
    /// 1 始まりの開始行。`None` なら先頭から。
    pub offset: Option<usize>,
    /// 読む行数。`None` なら最後まで。
    pub limit: Option<usize>,
    /// コメントを外すか (`Slim` / `Auto` のときだけ効く)。
    pub strip_comments: bool,
}

impl Default for ReadParams {
    fn default() -> Self {
        Self {
            offset: None,
            limit: None,
            strip_comments: true,
        }
    }
}

/// ファイルを読んで戦略どおりに畳む。
pub fn run(
    cx: &ToolContext,
    path: &std::path::Path,
    params: ReadParams,
    strategy: ContextStrategy,
) -> Result<Rendered, ContextError> {
    let sp = cx.workspace.resolve(path)?;
    let text = walk::read_text(&sp)?;
    let total_lines = text.lines().count();
    let ranged = params.offset.is_some() || params.limit.is_some();

    let base: String = if ranged {
        let off = params.offset.unwrap_or(1).max(1);
        let lim = params.limit.unwrap_or(usize::MAX);
        text.lines()
            .skip(off - 1)
            .take(lim)
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        text
    };

    // **削減率の分母は「素直にやったらいくらだったか」。**
    // 行域を指定したときの素直なやり方は*その行域を読むこと*なので、
    // ファイル全体を分母にしてはいけない — 4 行読んだだけで
    // 「-100% 削減、8 万トークン節約」という嘘の数字が出る (実際に出た)。
    let original_tokens = estimate_tokens(&base);

    let ext = sp.ext();
    let slimmed = || {
        let t = if params.strip_comments {
            optimizer::strip_comments(&base, &ext)
        } else {
            base.clone()
        };
        optimizer::collapse_blank(&t)
    };

    let (label, body) = match strategy {
        ContextStrategy::Raw => ("raw".to_string(), base.clone()),
        ContextStrategy::Outline => ("outline".to_string(), optimizer::outline(&base, &ext)),
        ContextStrategy::Slim => ("slim".to_string(), slimmed()),
        ContextStrategy::Auto => resolve_auto(
            &base,
            &ext,
            ranged,
            slimmed(),
            cx.limits.max_tokens,
            cx.limits.auto_outline_min_tokens,
        ),
    };

    let range = if ranged {
        format!(
            " range=L{}..{}",
            params.offset.unwrap_or(1).max(1),
            params
                .limit
                .map(|l| (params.offset.unwrap_or(1).max(1) + l - 1).to_string())
                .unwrap_or_else(|| "end".to_string())
        )
    } else {
        String::new()
    };
    let hint = if label.starts_with("outline") {
        " — structure only, no bodies: fetch a function with offset/limit, or the whole file with strategy=slim".to_string()
    } else {
        String::new()
    };
    Ok(Rendered {
        detail: format!("{} strategy={label} lines={total_lines}{range}", sp.rel()),
        body,
        original_tokens,
        hint,
    })
}

/// `Auto` の決め方。
///
/// outline を採るのは「outline のほうが小さい」かつ
/// 「どうせ上限で畳まれる / 半分以下になる」とき。畳まれる本文は途中が
/// 抜けるので、そこまで行くなら**抜けの無い構造**のほうが情報が多い。
fn resolve_auto(
    base: &str,
    ext: &str,
    ranged: bool,
    slim_text: String,
    cap: usize,
    min_tokens: usize,
) -> (String, String) {
    if ranged {
        // 行域の指定は「outline の次の一歩」なので、決して outline しない
        return ("slim(auto)".to_string(), slim_text);
    }
    let slim_tok = estimate_tokens(&slim_text);
    if let Some(o) = optimizer::outline_opt(base, ext) {
        let o_tok = estimate_tokens(&o);
        let would_be_capped = cap > 0 && slim_tok > cap;
        let halves_it = slim_tok > min_tokens && o_tok * 2 <= slim_tok;
        if o_tok < slim_tok && (would_be_capped || halves_it) {
            return ("outline(auto)".to_string(), o);
        }
    }
    ("slim(auto)".to_string(), slim_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests_support::Lab;

    #[test]
    fn 小さなファイルはslimで返る() {
        let lab = Lab::new("read-small");
        lab.write("a.rs", "// コメント\nfn a() {}\n\n\n\nfn b() {}\n");
        let r = lab.read("a.rs", ReadParams::default(), ContextStrategy::Auto);
        assert!(r.detail.contains("strategy=slim(auto)"), "{}", r.detail);
        assert!(!r.body.contains("コメント"));
        assert_eq!(r.body, "fn a() {}\n\nfn b() {}");
        assert!(r.original_tokens > estimate_tokens(&r.body));
    }

    #[test]
    fn 大きな構造のあるファイルはoutlineへ降りる() {
        let lab = Lab::new("read-big");
        // 本体のある関数を並べる。**outline が効くのはこういうファイル**
        // (1 行の関数ばかりのファイルでは構造も本体もほぼ同じ大きさになる)。
        let mut src = String::new();
        for i in 0..300 {
            src.push_str(&format!(
                "pub fn f{i}(a: u32, b: u32) -> u32 {{\n\
                 \x20   let scaled = a.wrapping_mul({i}).wrapping_add(b);\n\
                 \x20   let clamped = scaled.clamp(0, u32::MAX / 2);\n\
                 \x20   let mut acc = 0u32;\n\
                 \x20   for step in 0..clamped.min(8) {{\n\
                 \x20       acc = acc.wrapping_add(step).wrapping_mul(3);\n\
                 \x20   }}\n\
                 \x20   acc.wrapping_add(clamped)\n\
                 }}\n\n"
            ));
        }
        lab.write("big.rs", &src);
        let r = lab.read("big.rs", ReadParams::default(), ContextStrategy::Auto);
        assert!(r.detail.contains("outline(auto)"), "{}", r.detail);
        assert!(r.body.contains("L1: pub fn f0"));
        assert!(!r.body.contains("wrapping_mul"), "本体まで出ている");
        assert!(r.hint.contains("offset/limit"), "次の一歩の案内が無い");
        assert!(estimate_tokens(&r.body) * 3 < r.original_tokens);
    }

    /// 行域を指定したら**決して outline しない**。
    #[test]
    fn 行域の指定はoutlineへ降りない() {
        let lab = Lab::new("read-range");
        let mut src = String::new();
        for i in 0..300 {
            src.push_str(&format!("pub fn f{i}() {{\n    work({i});\n}}\n"));
        }
        lab.write("big.rs", &src);
        let p = ReadParams {
            offset: Some(4),
            limit: Some(3),
            ..ReadParams::default()
        };
        let r = lab.read("big.rs", p, ContextStrategy::Auto);
        assert!(r.detail.contains("slim(auto)"), "{}", r.detail);
        assert!(r.detail.contains("range=L4..6"));
        assert_eq!(r.body, "pub fn f1() {\n    work(1);\n}");
        // **分母は行域そのもの。** ファイル全体を分母にすると
        // 「4 行読んだのに 8 万トークン節約」という嘘が出る。
        assert_eq!(
            r.original_tokens,
            estimate_tokens(&r.body),
            "行域の分母がファイル全体になっている"
        );
    }

    #[test]
    fn 戦略は明示すれば効く() {
        let lab = Lab::new("read-strategy");
        lab.write("a.rs", "// c\nfn a() {}\n");
        let raw = lab.read("a.rs", ReadParams::default(), ContextStrategy::Raw);
        assert_eq!(raw.body, "// c\nfn a() {}\n");
        let out = lab.read("a.rs", ReadParams::default(), ContextStrategy::Outline);
        assert_eq!(out.body, "L2: fn a() {}");
        let keep = ReadParams {
            strip_comments: false,
            ..ReadParams::default()
        };
        let slim = lab.read("a.rs", keep, ContextStrategy::Slim);
        assert!(slim.body.contains("// c"), "コメントを残す指定が効かない");
    }

    /// Unicode と CRLF。**行数がずれない**ことと panic しないこと。
    #[test]
    fn unicodeとcrlfでも行がずれない() {
        let lab = Lab::new("read-unicode");
        lab.write(
            "u.rs",
            "// 日本語のコメント 🎌\r\nfn あ() {}\r\n\r\n\r\nfn い() {}\r\n",
        );
        let r = lab.read("u.rs", ReadParams::default(), ContextStrategy::Slim);
        assert!(r.detail.contains("lines=5"), "{}", r.detail);
        assert_eq!(r.body, "fn あ() {}\n\nfn い() {}");
        assert!(!r.body.contains('\r'), "CR が残っている");

        let out = lab.read("u.rs", ReadParams::default(), ContextStrategy::Outline);
        assert!(out.body.contains("L2:"), "{}", out.body);
        assert!(
            out.body.contains("L5:"),
            "CRLF で行番号がずれた: {}",
            out.body
        );
    }

    /// 空のファイル・行域が範囲外でも panic せず、正直な結果を返す。
    #[test]
    fn 空と範囲外でも落ちない() {
        let lab = Lab::new("read-empty");
        lab.write("e.rs", "");
        let r = lab.read("e.rs", ReadParams::default(), ContextStrategy::Auto);
        assert_eq!(r.body, "");
        assert_eq!(r.original_tokens, 0);
        lab.write("s.rs", "fn a() {}\n");
        let p = ReadParams {
            offset: Some(9999),
            limit: Some(10),
            ..ReadParams::default()
        };
        assert_eq!(lab.read("s.rs", p, ContextStrategy::Auto).body, "");
        // offset=0 は 1 として扱う (1 始まりの契約)
        let p0 = ReadParams {
            offset: Some(0),
            limit: Some(1),
            ..ReadParams::default()
        };
        assert_eq!(lab.read("s.rs", p0, ContextStrategy::Raw).body, "fn a() {}");
    }
}
