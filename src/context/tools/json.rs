//! JSON を刈る道具。`token-slim-mcp` の `json_slim` にあたる。
//!
//! **JSONC (コメント・末尾カンマ) も受ける。** `tsconfig.json` /
//! `.vscode/*.json` / `bun.lock` は素の JSON パーサが弾く形で書かれている
//! ことが多く、「壊れている」と返すのは事実に反する。
//!
//! JSONC の正規化は [`crate::jsonc::strip_jsonc`] を**そのまま使う**。
//! 同じ仕事の実装を 2 つ持つと必ずずれるので、リポジトリ内に既にある
//! ほうへ寄せた (テーマ JSON とスニペット JSON が同じものを通っている)。

use super::{Rendered, ToolContext};
use crate::context::metrics::estimate_tokens;
use crate::context::optimizer::{prune_json, JsonLimits};
use crate::context::walk;
use crate::context::ContextError;

/// JSON の入力元。
pub enum JsonInput<'a> {
    /// その場の文字列。
    Text(&'a str),
    /// ワークスペース内のファイル。
    File(&'a std::path::Path),
}

/// JSON を刈る。上限を指定しなければ [`crate::context::ContextLimits::json`]。
pub fn run(
    cx: &ToolContext,
    input: JsonInput,
    limits: Option<JsonLimits>,
) -> Result<Rendered, ContextError> {
    let (label, text) = match input {
        JsonInput::Text(t) => ("text".to_string(), t.to_string()),
        JsonInput::File(p) => {
            let sp = cx.workspace.resolve(p)?;
            let body = walk::read_text(&sp)?;
            (sp.rel().to_string(), body)
        }
    };
    let original_tokens = estimate_tokens(&text);
    let lim = limits.unwrap_or(cx.limits.json);

    // 素の JSON を先に試し、駄目なら JSONC として読み直す。
    // **両方の理由を残す** — 「JSON としても JSONC としても駄目」と
    // 「JSON では駄目だが JSONC なら通る」は別の話。
    let (value, jsonc) = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(v) => (v, false),
        Err(strict) => {
            let relaxed = crate::jsonc::strip_jsonc(&text);
            match serde_json::from_str::<serde_json::Value>(&relaxed) {
                Ok(v) => (v, true),
                Err(loose) => {
                    let (se, le) = (strict.to_string(), loose.to_string());
                    return Err(ContextError::BadRequest(if se == le {
                        format!("invalid JSON: {se}")
                    } else {
                        format!("invalid JSON: {se} (also unparsable as JSONC: {le})")
                    }));
                }
            }
        }
    };

    let pruned = prune_json(&value, lim.max_depth, &lim);
    let body =
        serde_json::to_string(&pruned).map_err(|e| ContextError::Io(format!("serialize: {e}")))?;
    Ok(Rendered {
        detail: format!(
            "{label}{} depth<={} array<={} string<={}",
            if jsonc {
                " [jsonc: comments/trailing commas stripped]"
            } else {
                ""
            },
            lim.max_depth,
            lim.max_array,
            lim.max_string
        ),
        body,
        original_tokens,
        hint: String::new(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::context::tests_support::Lab;

    #[test]
    fn 素のjsonを最小化して刈る() {
        let lab = Lab::new("json-plain");
        let src = serde_json::json!({
            "name": "x",
            "items": (0..100).collect::<Vec<i32>>(),
        })
        .to_string();
        let r = lab.json_text(&src, None);
        assert!(r.body.contains("…+80 more items"), "{}", r.body);
        assert!(!r.detail.contains("jsonc"), "素の JSON を JSONC と言った");
        assert!(estimate_tokens(&r.body) < r.original_tokens);
    }

    #[test]
    fn jsoncも読めてそう名乗る() {
        let lab = Lab::new("json-jsonc");
        let src =
            "{\n  // lockfile version\n  \"v\": 1, /* inline */\n  \"list\": [1, 2, 3,],\n}\n";
        let r = lab.json_text(src, None);
        assert!(r.detail.contains("jsonc"), "{}", r.detail);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["list"].as_array().unwrap().len(), 3);
    }

    /// 文字列の中の `//` や `,` を JSONC のコメント / 末尾カンマと誤らない。
    #[test]
    fn 文字列の中身は守られる() {
        let lab = Lab::new("json-strings");
        let src = "{\"url\": \"https://x.dev//a\", \"c\": \"/* not a comment */\", \"t\": \"a,\",}";
        let r = lab.json_text(src, None);
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["url"], "https://x.dev//a");
        assert_eq!(v["c"], "/* not a comment */");
        assert_eq!(v["t"], "a,");
    }

    #[test]
    fn 深い入れ子と巨大な配列を上限で止める() {
        let lab = Lab::new("json-deep");
        let mut v = serde_json::json!(1);
        for _ in 0..100 {
            v = serde_json::json!({ "n": v });
        }
        let r = lab.json_text(
            &v.to_string(),
            Some(JsonLimits {
                max_depth: 3,
                max_array: 2,
                max_string: 8,
            }),
        );
        assert!(r.body.contains("keys}"), "{}", r.body);
        assert!(r.body.len() < 100);
        assert!(
            r.original_tokens > estimate_tokens(&r.body) * 4,
            "分母が元の全体になっていない ({} vs {})",
            r.original_tokens,
            estimate_tokens(&r.body)
        );

        let huge = serde_json::json!({ "a": (0..50_000).collect::<Vec<i32>>() }).to_string();
        let r = lab.json_text(&huge, None);
        assert!(r.body.contains("…+49980 more items"), "{}", &r.body[..80]);
        assert!(estimate_tokens(&r.body) * 100 < r.original_tokens);
    }

    #[test]
    fn 壊れたjsonは両方の理由を残して断る() {
        let lab = Lab::new("json-bad");
        // JSONC として読み直しても同じところで転ぶなら、理由は 1 つでよい
        let msg = lab
            .json_text_result("{ これは JSON ではない", None)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("invalid JSON"), "{msg}");
        assert!(!msg.contains("JSONC"), "同じ理由を 2 度言っている: {msg}");

        // 転ぶ場所が変わるなら**両方**残す
        // (「JSON では駄目」と「JSONC でも駄目」は別の話)
        let msg = lab
            .json_text_result("{\"a\": 1 /* 閉じていないコメント", None)
            .unwrap_err()
            .to_string();
        assert!(msg.contains("JSONC"), "JSONC としての理由が無い: {msg}");
    }

    /// **パーサ自身の再帰上限を超える入力は、読む前に断る。**
    /// ここで stack overflow を起こすと、プロセスごと落ちる (捕まえられない)。
    #[test]
    fn パーサの再帰上限を超える入力は断る() {
        let lab = Lab::new("json-toodeep");
        let deep = format!("{}1{}", "[".repeat(500), "]".repeat(500));
        let e = lab.json_text_result(&deep, None).unwrap_err();
        assert!(matches!(e, ContextError::BadRequest(_)), "{e:?}");
        assert!(e.to_string().contains("recursion"), "{e}");
    }

    #[test]
    fn ファイルからも読める() {
        let lab = Lab::new("json-file");
        lab.write("a.json", "{\"a\": [1,2,3]}");
        let r = lab.json_file("a.json", None);
        assert!(r.detail.starts_with("a.json"), "{}", r.detail);
        assert_eq!(r.body, "{\"a\":[1,2,3]}");
        // ワークスペースの外は読めない
        assert!(matches!(
            lab.json_file_result("../outside.json", None),
            Err(ContextError::OutsideWorkspace { .. })
        ));
    }
}
