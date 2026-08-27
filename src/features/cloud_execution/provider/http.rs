//! **HTTP の実装を 1 か所に閉じ込める** (§19)。
//!
//! ## なぜ面を切るのか
//!
//! Provider が `ureq` を直に呼ぶと、
//!
//! * 試験のたびに本物のクラウドへ課金する (§54 が禁じている)
//! * 429 / 5xx / タイムアウトの扱いを Provider ごとに書き直すことになる
//! * HTTP の実装を替える日に Provider を全部触ることになる
//!
//! ので、[`HttpClient`] を挟む。`ureq` という名前が出てよいのはこのファイルだけで、
//! [`tests::ureqの名前はこのファイルにしか出ない`] が番人。
//!
//! ## なぜ ureq なのか
//!
//! * **同期**。Cloud API のためだけに tokio ランタイムを持ち込まない
//! * **純 Rust + rustls**。ネイティブの OpenSSL も、実行時のダウンロードも要らない
//! * **TLS 検証あり**。証明書を確かめない経路をこのアプリに作らない
//! * MSRV 1.85 ≤ 本体の 1.88
//!
//! `curl` / `Invoke-WebRequest` を子プロセスで呼ぶ形は採らない (§19) —
//! 外部コマンドの有無で挙動が変わるうえ、URL とトークンがコマンド行に載る。

use std::collections::BTreeMap;
use std::time::Duration;

use crate::features::cloud_execution::model::CloudError;
use crate::features::cloud_execution::redact::redact;

/// 要求。**本文は文字列** (この層が扱うのは JSON だけ)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: &'static str,
    pub url: String,
    /// **`Authorization` もここへ入る**が、外へ出るときは必ず伏せる。
    pub headers: BTreeMap<String, String>,
    pub body: Option<String>,
    pub timeout: Duration,
}

impl HttpRequest {
    pub fn get(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method: "GET",
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            timeout,
        }
    }

    pub fn post(url: impl Into<String>, body: String, timeout: Duration) -> Self {
        Self {
            method: "POST",
            url: url.into(),
            headers: BTreeMap::new(),
            body: Some(body),
            timeout,
        }
    }

    pub fn delete(url: impl Into<String>, timeout: Duration) -> Self {
        Self {
            method: "DELETE",
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            timeout,
        }
    }

    pub fn bearer(mut self, token: &str) -> Self {
        self.headers
            .insert("Authorization".to_string(), format!("Bearer {token}"));
        self
    }

    pub fn json(mut self) -> Self {
        self.headers
            .insert("Content-Type".to_string(), "application/json".to_string());
        self
    }

    /// ログと画面に出してよい 1 行。**ヘッダは出さない。**
    pub fn safe_summary(&self) -> String {
        redact(&format!("{} {}", self.method, self.url))
    }
}

/// 応答。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn ok(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// HTTP を送る面。**試験は [`super::super::test_support::FakeHttpClient`]** を挿す。
pub trait HttpClient: Send + Sync {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, CloudError>;
}

/// 実 HTTP。**`ureq` を呼ぶのはここだけ。**
pub struct UreqClient {
    default_timeout: Duration,
}

impl UreqClient {
    pub fn new(default_timeout: Duration) -> Self {
        Self { default_timeout }
    }
}

impl HttpClient for UreqClient {
    fn send(&self, request: &HttpRequest) -> Result<HttpResponse, CloudError> {
        // **https しか通さない。** 平文だとトークンがそのまま流れる。
        if !request.url.starts_with("https://") {
            return Err(CloudError::security(format!(
                "https 以外へは送りません: {}",
                redact(&request.url)
            )));
        }
        let timeout = if request.timeout.is_zero() {
            self.default_timeout
        } else {
            request.timeout
        };
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .user_agent(concat!("zaivern-code/", env!("CARGO_PKG_VERSION")))
            .build()
            .new_agent();

        // **本文の有無で型が分かれる** (ureq 3 の `RequestBuilder<WithBody>` /
        // `<WithoutBody>`)。1 つの `match` に畳めないので、ここだけ 2 本に割る。
        let sent = match request.method {
            "GET" | "DELETE" => {
                let mut b = if request.method == "GET" {
                    agent.get(&request.url)
                } else {
                    agent.delete(&request.url)
                };
                for (k, v) in &request.headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b.call()
            }
            "POST" | "PUT" => {
                let mut b = if request.method == "POST" {
                    agent.post(&request.url)
                } else {
                    agent.put(&request.url)
                };
                for (k, v) in &request.headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b.send(request.body.as_deref().unwrap_or(""))
            }
            other => {
                return Err(CloudError::unsupported(format!(
                    "対応していない HTTP メソッドです: {other}"
                )))
            }
        };

        match sent {
            Ok(mut res) => {
                let status = res.status().as_u16();
                let body = res.body_mut().read_to_string().unwrap_or_default();
                Ok(HttpResponse { status, body })
            }
            // 4xx / 5xx は「失敗」ではなく**応答**として返す。
            // どう扱うか (再試行するか等) を決めるのは Provider の仕事。
            Err(ureq::Error::StatusCode(code)) => Ok(HttpResponse {
                status: code,
                body: String::new(),
            }),
            Err(ureq::Error::Timeout(_)) => Err(CloudError::timeout(format!(
                "{} が {} 秒で終わりませんでした",
                request.safe_summary(),
                timeout.as_secs()
            ))),
            Err(e) => Err(CloudError::transport(format!(
                "{}: {e}",
                request.safe_summary()
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 要約に秘密が出ない() {
        let r = HttpRequest::get("https://api.example/servers?token=abcdef12345", Duration::ZERO)
            .bearer("super-secret-test-token");
        let s = r.safe_summary();
        assert!(!s.contains("super-secret-test-token"), "{s}");
        assert!(!s.contains("abcdef12345"), "{s}");
        // どこへ行ったかは残る (診断できること)
        assert!(s.contains("api.example"), "{s}");
    }

    #[test]
    fn 平文へは送らない() {
        let c = UreqClient::new(Duration::from_secs(1));
        let e = c
            .send(&HttpRequest::get("http://api.example/x", Duration::from_secs(1)))
            .expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
    }

    /// **`ureq` の名前が出てよいのはこのファイルだけ。**
    #[test]
    fn ureqの名前はこのファイルにしか出ない() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("src/features/cloud_execution");
        let mut checked = 0usize;
        let mut stack = vec![dir];
        while let Some(d) = stack.pop() {
            for e in std::fs::read_dir(&d).expect("読める").flatten() {
                let p = e.path();
                if p.is_dir() {
                    stack.push(p);
                    continue;
                }
                if p.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                if p.file_name().and_then(|s| s.to_str()) == Some("http.rs") {
                    continue;
                }
                let raw = std::fs::read_to_string(&p).expect("読める");
                // **製品コードだけを見る。** 番人テストが禁止語として
                // 書いている行まで咎めると、番人どうしが噛み合わなくなる。
                let src = raw
                    .replace("\r\n", "\n")
                    .split("#[cfg(test)]")
                    .next()
                    .unwrap_or_default()
                    .to_string();
                checked += 1;
                assert!(
                    !src.contains("ureq"),
                    "{} に ureq が出てくる。HTTP の実装は http.rs に閉じること",
                    p.display()
                );
            }
        }
        assert!(checked >= 10, "走査が空振りしている ({checked} 件)");
    }
}
