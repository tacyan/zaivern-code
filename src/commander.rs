//! 指名スーパーエージェント(指揮官)の純ロジック。
//!
//! 選んだ 1 つの CLI エージェントを「指揮官」にするための、UI にもセッションにも
//! 依存しない純関数だけを置く。
//!
//! - [`parse_directives`] — 指揮官の画面出力から `@対象: 指示` を拾う。
//! - [`title_matches`] — 配線側の小道具(宛先タイトルの照合)。
//!
//! ## 方針
//! - **セッションの入力欄へは何も書かない**。拾った指示はユーザー宛の通知に
//!   なるだけで、実際に他エージェントへ流すかはユーザーが決める。
//!   (以前は指示の配達と状況フィードを各端末へ自動注入していたが、ユーザーが
//!   入力中の欄に勝手に文字が流れ込むため廃止した。)
//! - **破壊的操作は一切扱わない**。停止・再起動は表現できない。
//! - 判断(指揮の中身)は選んだエージェント自身が端末内で行う。外部の subagent へは
//!   投げない。

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 指示の宛先。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    /// 全エージェント宛(配達側で送信元は自動的に除かれる)。
    All,
    /// タイトルで指す 1 体。
    Named(String),
}

/// 指揮官が出した 1 件の指示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Directive {
    pub target: Target,
    pub body: String,
    /// 同じ指示の二重配達を防ぐための決定論ハッシュ。
    pub hash: u64,
}

/// 全員宛と解釈するキーワードか。
fn is_all_keyword(name: &str) -> bool {
    let n = name.trim();
    matches!(n.to_ascii_lowercase().as_str(), "all" | "broadcast" | "everyone" | "*")
        || matches!(n, "全員" | "全部" | "みんな")
}

/// 指示 1 件の決定論ハッシュ。宛先表記と本文から作る
/// (`DefaultHasher` は固定初期値なので毎回同じ値になる)。
fn directive_hash(target_key: &str, body: &str) -> u64 {
    let mut h = DefaultHasher::new();
    target_key.hash(&mut h);
    0u8.hash(&mut h); // 宛先と本文の境界
    body.hash(&mut h);
    h.finish()
}

/// `name: body` を最初のコロン(半角 `:` / 全角 `：`)で分ける。
fn split_target(rest: &str) -> Option<(&str, &str)> {
    let (i, c) = rest.char_indices().find(|&(_, c)| c == ':' || c == '：')?;
    Some((&rest[..i], &rest[i + c.len_utf8()..]))
}

/// 画面テキストから `@対象: 指示` 形式の行を拾う。
///
/// - `@all: …` / `@全員: …` などは全員宛。
/// - `inject_prefix` で始まる行(= こちらが注入した行)は無視してループを防ぐ。
/// - 宛先または本文が空の行は無視する。
/// - 同じ (宛先, 本文) は 1 回にまとめる。
///
/// コロン必須にしているのは、文章中の何気ない `@メンション` を指示と誤解しないため。
pub fn parse_directives(screen: &str, inject_prefix: &str) -> Vec<Directive> {
    let mut out: Vec<Directive> = Vec::new();
    let mut seen: Vec<u64> = Vec::new();
    for raw in screen.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(inject_prefix) {
            continue;
        }
        let Some(rest) = line.strip_prefix('@') else {
            continue;
        };
        let Some((name, body)) = split_target(rest) else {
            continue;
        };
        let name = name.trim();
        let body = body.trim();
        if name.is_empty() || body.is_empty() {
            continue;
        }
        let (target, key) = if is_all_keyword(name) {
            (Target::All, "*all*".to_string())
        } else {
            (Target::Named(name.to_string()), name.to_ascii_lowercase())
        };
        let hash = directive_hash(&key, body);
        if seen.contains(&hash) {
            continue;
        }
        seen.push(hash);
        out.push(Directive {
            target,
            body: body.to_string(),
            hash,
        });
    }
    out
}

/// 指示の宛先タイトルが、あるセッションのタイトルに一致するとみなせるか。
/// 大文字小文字を無視した部分一致(短い呼び名でも当たるように)。
pub fn title_matches(title: &str, query: &str) -> bool {
    let t = title.to_lowercase();
    let q = query.to_lowercase();
    let q = q.trim();
    !q.is_empty() && (t == q || t.contains(q))
}

#[cfg(test)]
mod tests {
    use super::*;

    const PFX: &str = "[ZAI-AGENT]";

    #[test]
    fn parses_named_and_all() {
        let screen = "考え中...\n@agent2: テストを直して\n@all: いったん停止せず続けて\n普通の行";
        let ds = parse_directives(screen, PFX);
        assert_eq!(ds.len(), 2);
        assert_eq!(ds[0].target, Target::Named("agent2".into()));
        assert_eq!(ds[0].body, "テストを直して");
        assert_eq!(ds[1].target, Target::All);
    }

    #[test]
    fn full_width_colon_and_keywords() {
        let ds = parse_directives("@全員：ビルドを回して", PFX);
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].target, Target::All);
        assert_eq!(ds[0].body, "ビルドを回して");
    }

    #[test]
    fn ignores_injected_lines_and_bare_mentions() {
        // 自分が注入した行は拾わない(ループ防止)
        let injected = format!("{PFX} #3 supervisor から(状況): ...");
        let screen = format!("{injected}\n@agent1 これはコロン無しなので無視\nmail @user in body");
        assert!(parse_directives(&screen, PFX).is_empty());
    }

    #[test]
    fn dedups_same_directive() {
        // 画面に同じ行が残り続けても 1 回だけ
        let screen = "@a: go\n@a: go\n@a: go";
        assert_eq!(parse_directives(screen, PFX).len(), 1);
    }

    #[test]
    fn hash_is_deterministic_and_target_sensitive() {
        let a = parse_directives("@x: do", PFX);
        let b = parse_directives("@x: do", PFX);
        let c = parse_directives("@y: do", PFX);
        assert_eq!(a[0].hash, b[0].hash);
        assert_ne!(a[0].hash, c[0].hash);
    }

    #[test]
    fn title_match() {
        assert!(title_matches("Claude — main", "claude"));
        assert!(!title_matches("Claude", "codex"));
    }
}
