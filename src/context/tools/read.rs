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

impl ReadParams {
    /// 受け付けられる指定かを見る。
    ///
    /// **行番号は 1 始まり**で、0 は「先頭の 1 つ前」という存在しない場所を
    /// 指す。黙って 1 として扱うと、`--offset 0` と打った人は 1 始まりだと
    /// 気付かないまま**ずっと 1 行ずれた読み方**を続ける。
    /// `limit = 0` も同じで、返るのは空。断るほうが親切。
    ///
    /// **道具の側で見る**ので、CLI からでも API から直に
    /// [`ReadParams`] を組んでも同じ規則が効く。
    fn validate(&self) -> Result<(), ContextError> {
        if self.offset == Some(0) {
            return Err(ContextError::BadRequest(
                "offset is 1-based: use 1 or more".into(),
            ));
        }
        if self.limit == Some(0) {
            return Err(ContextError::BadRequest("limit must be 1 or more".into()));
        }
        Ok(())
    }
}

/// `range=L10..14` の表記。
///
/// **飽和演算で組む。** 表示のための計算で panic してはいけない
/// (`offset = usize::MAX` / `limit = 5` で実際に
///  "attempt to add with overflow" で落ちた)。
/// [`ReadParams::validate`] が 0 を弾いているので下限側は起こり得ないが、
/// **不変条件に頼らず**ここでも飽和させる — 表示の都合で落ちる経路を
/// 1 本も残さない。
///
/// ## 引く順序が意味を持つ
///
/// 終了行は `start + (limit - 1)` であって `(start + limit) - 1` ではない。
/// 算術としては同じだが、**飽和させると同じにならない**:
/// `start = usize::MAX` で前者は `MAX` に留まるのに対し、後者は足し算が
/// `MAX` へ飽和したあと 1 引かれて `MAX - 1` になり、
/// **終了行が開始行より小さい**という有り得ない範囲を表示する。
/// 飽和は「上限で止める」だけなので、**止まったあとに引いてはいけない**。
fn range_label(offset: Option<usize>, limit: Option<usize>) -> String {
    let start = offset.unwrap_or(1).max(1);
    match limit {
        Some(l) => format!(
            " range=L{start}..{}",
            start.saturating_add(l.saturating_sub(1))
        ),
        None => format!(" range=L{start}..end"),
    }
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
    params.validate()?;
    let sp = cx.workspace.resolve(path)?;
    let text = walk::read_text(&sp)?;
    let total_lines = text.lines().count();
    let ranged = params.offset.is_some() || params.limit.is_some();

    let base: String = if ranged {
        let off = params.offset.unwrap_or(1).max(1);
        let lim = params.limit.unwrap_or(usize::MAX);
        text.lines()
            .skip(off.saturating_sub(1))
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
        range_label(params.offset, params.limit)
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

    /// **0 は行番号ではない。** 1 始まりなので `offset = 0` は「先頭の 1 つ前」、
    /// `limit = 0` は「0 行返す」を意味する。黙って直すと、打った人は
    /// ずれていることに気付かないまま読み続ける。
    ///
    /// 見るのは**道具の側**なので、CLI からでも API から直に [`ReadParams`]
    /// を組んでも同じ規則が効く。
    #[test]
    fn 零の行域は断る() {
        let lab = Lab::new("read-zero");
        lab.write("a.rs", "fn a() {}\nfn b() {}\n");
        for (offset, limit) in [(Some(0), Some(1)), (Some(1), Some(0)), (Some(0), None)] {
            let p = ReadParams {
                offset,
                limit,
                ..ReadParams::default()
            };
            let e = lab
                .read_result("a.rs", p, ContextStrategy::Raw)
                .expect_err(&format!("offset={offset:?} limit={limit:?} が通った"));
            assert!(matches!(e, ContextError::BadRequest(_)), "{e:?}");
            assert!(
                e.to_string().contains("1 or more"),
                "直し方が書かれていない: {e}"
            );
        }
    }

    /// **表示のための計算で panic せず、範囲が逆向きにもならない。**
    ///
    /// `offset = usize::MAX` / `limit = 5` は実際に
    /// "attempt to add with overflow" で落ちていた (debug ビルド)。
    /// それを飽和演算で塞いだあと、今度は**引く順序**で
    /// `L18446744073709551615..18446744073709551614` という
    /// 開始行より小さい終了行が出ていた。
    /// 純関数に切り出してあるので、境界を表で固定できる。
    #[test]
    fn 行域の表記は飽和して落ちず逆向きにもならない() {
        let cases = [
            (Some(10), Some(5), " range=L10..14".to_string()),
            (Some(1), Some(1), " range=L1..1".to_string()),
            (Some(7), None, " range=L7..end".to_string()),
            (None, None, " range=L1..end".to_string()),
            // 上限付近でも足し算が回らず、**開始行に留まる**
            (
                Some(usize::MAX),
                Some(5),
                format!(" range=L{}..{}", usize::MAX, usize::MAX),
            ),
            (
                Some(usize::MAX),
                Some(usize::MAX),
                format!(" range=L{}..{}", usize::MAX, usize::MAX),
            ),
            (
                Some(usize::MAX - 1),
                Some(1),
                format!(" range=L{}..{}", usize::MAX - 1, usize::MAX - 1),
            ),
            // 0 は validate が弾くので実際には来ないが、来ても
            // 落ちず・逆向きにもならない
            (Some(0), Some(0), " range=L1..1".to_string()),
            (Some(0), Some(1), " range=L1..1".to_string()),
        ];
        for (offset, limit, want) in cases {
            assert_eq!(
                range_label(offset, limit),
                want,
                "offset={offset:?} limit={limit:?}"
            );
        }

        // **終了行が開始行を下回る組が 1 つも無いこと**を、境界の総当たりで
        // 見る (表に書き漏らした組を拾うのはこちら)。
        let edges = [
            1usize,
            2,
            3,
            usize::MAX / 2,
            usize::MAX - 2,
            usize::MAX - 1,
            usize::MAX,
        ];
        for start in edges {
            for l in edges {
                let label = range_label(Some(start), Some(l));
                let (lo, hi) = label
                    .trim_start_matches(" range=L")
                    .split_once("..")
                    .expect("開始..終了 の形");
                let (lo, hi) = (
                    lo.parse::<usize>().expect("開始行"),
                    hi.parse::<usize>().expect("終了行"),
                );
                assert!(hi >= lo, "逆向きの範囲: start={start} limit={l} → {label}");
                assert_eq!(lo, start, "開始行が動いた: {label}");
            }
        }
    }

    /// 上限付近の `offset` を**道具の経路ごと**通しても落ちない
    /// (`range_label` の表だけでは、走査側の `skip(off - 1)` を見ていない)。
    #[test]
    fn 上限付近の開始行でも落ちない() {
        let lab = Lab::new("read-huge-offset");
        lab.write("a.rs", "fn a() {}\n");
        for off in [usize::MAX, usize::MAX - 1, usize::MAX / 2] {
            let p = ReadParams {
                offset: Some(off),
                limit: Some(5),
                ..ReadParams::default()
            };
            let r = lab.read("a.rs", p, ContextStrategy::Raw);
            assert_eq!(r.body, "", "ファイルの外なのに中身が出た");
            assert!(r.detail.contains("range=L"), "{}", r.detail);
        }
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
        // 0 の扱いは `零の行域は断る` が持つ (ここでは範囲外だけを見る)
        let p1 = ReadParams {
            offset: Some(1),
            limit: Some(1),
            ..ReadParams::default()
        };
        assert_eq!(lab.read("s.rs", p1, ContextStrategy::Raw).body, "fn a() {}");
    }
}
