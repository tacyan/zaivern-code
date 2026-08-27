//! **秘密を伏せる場所を 1 か所にする** (§41)。
//!
//! ## なぜ 1 か所なのか
//!
//! 伏せ方が 2 か所にあると必ずずれる。ずれた側から漏れるので、伏せる規則は
//! ここだけに置き、エラー・ログ・画面・CLI 出力はすべて [`redact`] を通す。
//!
//! ## 何を伏せるか
//!
//! | 対象 | 例 |
//! |---|---|
//! | `Authorization` ヘッダ | `Authorization: Bearer abc…` → `Authorization: Bearer ***` |
//! | 登録された環境変数の**値** | `HCLOUD_TOKEN` の中身と一致する文字列 |
//! | 秘密鍵の本文 | `-----BEGIN … PRIVATE KEY-----` から `END` まで |
//! | URL / クエリのトークン | `?token=abc` / `access_token=abc` |
//!
//! ## 何を伏せないか (伏せすぎない)
//!
//! * **短い値は伏せない** ([`MIN_SECRET_LEN`])。3 文字の環境変数を伏せ始めると、
//!   ふつうの出力が `***` だらけになって診断できなくなる。
//! * 環境変数の**名前**は伏せない。名前は保存してよい情報で (§18)、
//!   `doctor` が「設定されているか」を言うために要る。

use std::collections::BTreeSet;
use std::sync::{OnceLock, RwLock};

/// これより短い環境変数の値は伏せない (伏せすぎて診断できなくなるため)。
pub const MIN_SECRET_LEN: usize = 8;

/// 伏せ字。
pub const MASK: &str = "***";

/// 既定で秘密として扱う環境変数の名前。
///
/// **Provider プロファイルが `token_env` を宣言したら [`register_secret_env`]
/// で足す**ので、ここは「何も設定していなくても伏せたいもの」だけ。
const DEFAULT_SECRET_ENVS: &[&str] = &["HCLOUD_TOKEN"];

fn registry() -> &'static RwLock<BTreeSet<String>> {
    static R: OnceLock<RwLock<BTreeSet<String>>> = OnceLock::new();
    R.get_or_init(|| {
        RwLock::new(DEFAULT_SECRET_ENVS.iter().map(|s| s.to_string()).collect())
    })
}

/// 秘密を持つ環境変数の名前を足す。**値は覚えない** (読むのは伏せる瞬間だけ)。
pub fn register_secret_env(name: &str) {
    let name = name.trim();
    if name.is_empty() {
        return;
    }
    if let Ok(mut set) = registry().write() {
        set.insert(name.to_string());
    }
}

/// いま秘密として扱っている環境変数の名前 (画面と `doctor` 用)。
pub fn secret_env_names() -> Vec<String> {
    registry()
        .read()
        .map(|s| s.iter().cloned().collect())
        .unwrap_or_default()
}

/// 秘密を伏せた文字列を返す。**外へ出る文字列は必ずここを通す。**
pub fn redact(text: &str) -> String {
    let env_values: Vec<String> = secret_env_names()
        .iter()
        .filter_map(|n| std::env::var(n).ok())
        .map(|v| v.trim().to_string())
        .filter(|v| v.chars().count() >= MIN_SECRET_LEN)
        .collect();
    redact_with(text, &env_values)
}

/// 環境を読まない本体。**テストが表で固定できるように**分けてある
/// (環境変数を差し替えるテストは、並列に走る他のテストへ漏れる)。
pub fn redact_with(text: &str, secret_values: &[String]) -> String {
    let mut out = mask_private_keys(text);
    out = mask_after_marker(&out, "Authorization:");
    out = mask_after_marker(&out, "authorization:");
    out = mask_bearer(&out);
    for key in ["token=", "access_token=", "api_key=", "apikey="] {
        out = mask_query_value(&out, key);
    }
    for v in secret_values {
        if v.chars().count() >= MIN_SECRET_LEN {
            out = out.replace(v.as_str(), MASK);
        }
    }
    out
}

/// `Bearer <値>` の値を伏せる。
fn mask_bearer(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("Bearer ") {
        out.push_str(&rest[..at + "Bearer ".len()]);
        let tail = &rest[at + "Bearer ".len()..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '"' || c == '\'' || c == ',')
            .unwrap_or(tail.len());
        if end > 0 {
            out.push_str(MASK);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `<marker> …行末` を伏せる (ヘッダ 1 行まるごと)。
fn mask_after_marker(text: &str, marker: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(marker) {
        out.push_str(&rest[..at + marker.len()]);
        let tail = &rest[at + marker.len()..];
        let end = tail.find(['\n', '\r']).unwrap_or(tail.len());
        if tail[..end].trim().is_empty() {
            out.push_str(&tail[..end]);
        } else {
            out.push(' ');
            out.push_str(MASK);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// `token=<値>` のような鍵と値の組の、値だけを伏せる。
fn mask_query_value(text: &str, key: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(key) {
        out.push_str(&rest[..at + key.len()]);
        let tail = &rest[at + key.len()..];
        let end = tail
            .find(|c: char| c.is_whitespace() || c == '&' || c == '"' || c == '\'' || c == ',')
            .unwrap_or(tail.len());
        if end > 0 {
            out.push_str(MASK);
        }
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// PEM の秘密鍵本文を伏せる。**行ごとに見る** — 終端が無いまま切れていても
/// 「BEGIN 以降は全部伏せる」で安全側に倒す。
fn mask_private_keys(text: &str) -> String {
    if !text.contains("PRIVATE KEY-----") {
        return text.to_string();
    }
    let mut out: Vec<String> = Vec::new();
    let mut inside = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\n', '\r']);
        if trimmed.contains("BEGIN") && trimmed.contains("PRIVATE KEY-----") {
            inside = true;
            out.push(format!("{MASK}\n"));
            continue;
        }
        if inside {
            if trimmed.contains("END") && trimmed.contains("PRIVATE KEY-----") {
                inside = false;
            }
            continue;
        }
        out.push(line.to_string());
    }
    out.concat()
}

#[cfg(test)]
mod tests {
    use super::*;

    const TOKEN: &str = "super-secret-test-token";

    #[test]
    fn ベアラートークンを伏せる() {
        let s = redact_with("Authorization: Bearer abcdef123456", &[]);
        assert!(!s.contains("abcdef123456"), "{s}");
        assert!(s.contains(MASK));
    }

    #[test]
    fn ヘッダ一行ごと伏せる() {
        let s = redact_with(
            "GET /x\nAuthorization: Basic zzzzzzzzzz\nAccept: application/json\n",
            &[],
        );
        assert!(!s.contains("zzzzzzzzzz"), "{s}");
        // 関係の無い行は残る (伏せすぎない)
        assert!(s.contains("Accept: application/json"), "{s}");
    }

    #[test]
    fn 環境変数の値を伏せる() {
        let text = format!("failed with {TOKEN} while calling api");
        let s = redact_with(&text, &[TOKEN.to_string()]);
        assert!(!s.contains(TOKEN), "{s}");
        assert!(s.contains("while calling api"), "文脈は残る: {s}");
    }

    #[test]
    fn 短い値は伏せない() {
        // 3 文字の値まで伏せると、ふつうの出力が伏せ字だらけになる
        let s = redact_with("path is /usr/bin", &["usr".to_string()]);
        assert_eq!(s, "path is /usr/bin");
    }

    #[test]
    fn 秘密鍵の本文を伏せる() {
        let pem = "before\n-----BEGIN OPENSSH PRIVATE KEY-----\nAAAAB3Nz\nmore\n-----END OPENSSH PRIVATE KEY-----\nafter\n";
        let s = redact_with(pem, &[]);
        assert!(!s.contains("AAAAB3Nz"), "{s}");
        assert!(s.contains("before") && s.contains("after"), "{s}");
    }

    #[test]
    fn 終端の無い秘密鍵も伏せる() {
        // 切れて届いても「BEGIN 以降は全部伏せる」で安全側に倒す
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nAAAAB3Nz";
        let s = redact_with(pem, &[]);
        assert!(!s.contains("AAAAB3Nz"), "{s}");
    }

    #[test]
    fn クエリのトークンを伏せる() {
        let s = redact_with("https://api.example/x?token=abcdef12345&page=2", &[]);
        assert!(!s.contains("abcdef12345"), "{s}");
        assert!(s.contains("page=2"), "他の引数は残る: {s}");
    }

    #[test]
    fn 名前は伏せない() {
        // 名前は保存してよい情報で、doctor が「設定されているか」を言うのに要る
        let s = redact_with("token env: HCLOUD_TOKEN", &[]);
        assert!(s.contains("HCLOUD_TOKEN"), "{s}");
    }

    #[test]
    fn 登録した名前は一覧に出る() {
        register_secret_env("ZAIVERN_TEST_SECRET_ENV");
        assert!(secret_env_names()
            .iter()
            .any(|n| n == "ZAIVERN_TEST_SECRET_ENV"));
        // 既定も残っている
        assert!(secret_env_names().iter().any(|n| n == "HCLOUD_TOKEN"));
    }
}
