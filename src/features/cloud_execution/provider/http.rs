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

/// 応答。**4xx / 5xx でも本文を持つ** ([`UreqClient`] の説明を参照)。
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

/// 応答の本文を読む上限。
///
/// **無制限にしない** — 壊れた相手や中間装置が延々と流してくると、
/// メモリを食い潰す。Hetzner の一覧は 1 ページ 50 件で数十 KB なので、
/// 4 MiB は実用上ぶつからない (ぶつかったときの扱いは
/// [`UreqClient::send`] の説明を参照)。
pub const MAX_BODY_BYTES: u64 = 4 * 1024 * 1024;

/// 本文を読めなかったときに、その代わりに入れる印。
///
/// **空文字にしない。** 空だと「本文の無い応答」と区別が付かず、
/// Provider が「エラーの理由が書かれていなかった」と読んでしまう。
pub const BODY_UNREADABLE: &str = "(応答の本文を読めませんでした)";

/// 実 HTTP。**`ureq` を呼ぶのはここだけ。**
///
/// ## Agent は使い回す
///
/// `ureq` の `Agent` は**接続の池**を持っていて、`Clone` しても池を共有する
/// (中身は全部 `Arc`)。1 本ごとに作り直すと、そのたびに TCP と TLS の
/// 握手からやり直すことになる。一覧 API はページごとに 1 本ずつ送るので、
/// ページが増えるほど無駄が積み上がる。
///
/// 待ち時間だけは要求ごとに変えたいので、**Agent には既定を、要求には上書きを**
/// 置く (`ureq` は要求ごとの設定の上書きを持っている)。
pub struct UreqClient {
    default_timeout: Duration,
    agent: ureq::Agent,
}

impl UreqClient {
    pub fn new(default_timeout: Duration) -> Self {
        Self {
            default_timeout,
            agent: Self::agent(default_timeout),
        }
    }

    /// 送るときの作法。**試験も同じここを通す** — 設定を写して試験すると、
    /// 写しだけが正しくて製品が壊れている、という嘘の緑になる。
    fn agent(timeout: Duration) -> ureq::Agent {
        ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .user_agent(concat!("zaivern-code/", env!("CARGO_PKG_VERSION")))
            // **状態コードでエラーにしない。** 本文ごと Provider へ渡す
            .http_status_as_error(false)
            .build()
            .new_agent()
    }
}

impl HttpClient for UreqClient {
    /// 1 本送る。
    ///
    /// ## 4xx / 5xx でも本文を捨てない
    ///
    /// `ureq` は既定で 4xx / 5xx を `Error::StatusCode(code)` にするが、
    /// **その形には本文が入っていない**。Hetzner は失敗の理由を
    /// `{"error":{"message":…}}` に入れて返すので、既定のままだと
    /// 「401 でした」までしか言えず、*なぜ*断られたのかが永久に分からない。
    /// 偽 HTTP の試験では本文を渡せてしまうため、**試験は緑のまま実運用でだけ
    /// 診断情報が消える**という形の壊れ方になる。
    ///
    /// そこで `http_status_as_error(false)` にして、状態コードの解釈は
    /// **Provider の仕事**に戻す ([`super::hetzner::classify`] が唯一の出所)。
    /// この層は「送って、返ってきたものを渡す」だけになる。
    ///
    /// ## 本文を読めなかったとき
    ///
    /// **状態コードを失わない。** 読めない理由 (上限超え・切断・不正な文字) は
    /// あっても、`401` だったことは確かなので、その分類だけは Provider へ渡す。
    /// ただし成功した応答が読めなかったときは、中身が要るので失敗として返す
    /// (空の本文を成功として渡すと、Provider が「空の一覧」と読む)。
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
        let agent = &self.agent;

        // **本文の有無で型が分かれる** (ureq 3 の `RequestBuilder<WithBody>` /
        // `<WithoutBody>`)。1 つの `match` に畳めないので、ここだけ 2 本に割る。
        let sent = match request.method {
            "GET" | "DELETE" => {
                let mut b = if request.method == "GET" {
                    agent.get(&request.url)
                } else {
                    agent.delete(&request.url)
                };
                // 待ち時間はこの 1 本のぶんだけ上書きする (池は共有のまま)
                b = b.config().timeout_global(Some(timeout)).build();
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
                b = b.config().timeout_global(Some(timeout)).build();
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
                let read = res
                    .body_mut()
                    .with_config()
                    .limit(MAX_BODY_BYTES)
                    .lossy_utf8(true)
                    .read_to_string();
                match read {
                    Ok(body) => Ok(HttpResponse { status, body }),
                    // 成功したのに中身が読めないなら、それは失敗として返す
                    Err(e) if (200..300).contains(&status) => Err(CloudError::transport(format!(
                        "{} の応答を読めません: {e}",
                        request.safe_summary()
                    ))),
                    // 失敗の応答なら、**理由が読めなくても分類は渡す**
                    Err(_) => Ok(HttpResponse {
                        status,
                        body: BODY_UNREADABLE.to_string(),
                    }),
                }
            }
            // `http_status_as_error(false)` にしたのでここへは来ないはずだが、
            // 来たとしても**応答として**渡す (どう扱うかは Provider の仕事)。
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

    /// 1 本だけ応答して閉じる HTTP サーバー。**本物の `ureq` を通す**ために
    /// 立てる (偽の [`HttpClient`] では、この層の振る舞いを 1 バイトも見ていない)。
    fn serve_once(status_line: &'static str, body: &'static str) -> u16 {
        use std::io::{Read, Write};
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("空いている番号を借りる");
        let port = listener.local_addr().expect("番号が分かる").port();
        std::thread::spawn(move || {
            let Ok((mut sock, _)) = listener.accept() else {
                return;
            };
            // 要求を読み捨てる (読まずに閉じると相手が RST を受ける)
            let mut buf = [0u8; 2048];
            let _ = sock.read(&mut buf);
            let head = "Content-Type: application/json";
            let res = format!(
                "HTTP/1.1 {status_line}\r\n{head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(res.as_bytes());
            let _ = sock.flush();
        });
        port
    }

    /// **これが直したかった欠陥。** `ureq` は既定で 4xx / 5xx を
    /// `Error::StatusCode(code)` にし、そこには**本文が入っていない**。
    /// 偽 HTTP の試験では本文を渡せるので、実運用でだけ診断情報が消える。
    ///
    /// `UreqClient::agent` と同じ作法で本物の HTTP を 1 本通し、
    /// 本文が残ることを確かめる (TLS が要らないよう `ureq` を直に呼ぶ —
    /// `UreqClient::send` は https しか通さないままにしておく)。
    #[test]
    fn 状態コードがエラーでも本文が残る() {
        const BODY: &str = r#"{"error":{"message":"invalid token"}}"#;
        let port = serve_once("401 Unauthorized", BODY);
        let agent = UreqClient::agent(Duration::from_secs(10));
        let mut res = agent
            .get(&format!("http://127.0.0.1:{port}/servers"))
            .call()
            .expect("状態コードで Err にしない");
        assert_eq!(res.status().as_u16(), 401);
        let body = res
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .lossy_utf8(true)
            .read_to_string()
            .expect("本文を読める");
        assert_eq!(body, BODY, "本文が失われた");
        // Provider が読む形になっている
        let msg = super::super::hetzner::api_error_message(&body);
        assert_eq!(msg, "invalid token");
    }

    /// 5xx でも同じ (再試行の判断は Provider がするので、本文は要る)。
    #[test]
    fn サーバー側の失敗でも本文が残る() {
        const BODY: &str = r#"{"error":{"message":"internal"}}"#;
        let port = serve_once("500 Internal Server Error", BODY);
        let agent = UreqClient::agent(Duration::from_secs(10));
        let mut res = agent
            .get(&format!("http://127.0.0.1:{port}/servers"))
            .call()
            .expect("状態コードで Err にしない");
        assert_eq!(res.status().as_u16(), 500);
        assert!(res
            .body_mut()
            .with_config()
            .limit(MAX_BODY_BYTES)
            .lossy_utf8(true)
            .read_to_string()
            .expect("読める")
            .contains("internal"));
    }

    /// 繋がるが**何も返さない**相手。待ち時間の試験に使う。
    fn serve_hang() -> u16 {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("空いている番号を借りる");
        let port = listener.local_addr().expect("番号が分かる").port();
        std::thread::spawn(move || {
            if let Ok((sock, _)) = listener.accept() {
                // **返事をしないまま握り続ける。** 途中で手放すと、上限が
                // 効いていなくても接続が切れて終わってしまい、試験が
                // 何も見ていないことになる (実際に空回りした)。
                // 手放すのはプロセスの終わり。
                std::mem::forget(sock);
            }
            std::mem::forget(listener);
        });
        port
    }

    /// **Agent は使い回す。** 1 本ごとに作り直すと、そのたびに TCP と TLS の
    /// 握手からやり直す (一覧はページごとに 1 本送るので効いてくる)。
    ///
    /// 使い回しても**要求ごとの待ち時間の上書きが効く**ことを、製品の経路
    /// ([`UreqClient::send`]) で確かめる — これが効かないなら使い回しは採れない。
    /// 繋がるが何も返さない相手なので、上限が効かなければ既定の 600 秒待つ。
    #[test]
    fn agentを使い回しても待ち時間は要求ごとに効く() {
        let port = serve_hang();
        // 既定は 120 秒。上書きが効かなければ、ここで 120 秒待たされる
        let c = UreqClient::new(Duration::from_secs(120));
        let began = std::time::Instant::now();
        let e = c
            .send(&HttpRequest::get(
                format!("https://127.0.0.1:{port}/servers"),
                Duration::from_millis(300),
            ))
            .expect_err("返事が来ない");
        assert!(
            matches!(e, CloudError::Timeout(_) | CloudError::Transport(_)),
            "{e:?}"
        );
        // 既定の 120 秒ではなく、この要求の 0.3 秒で諦めている
        // (絶対時間そのものではなく「既定を使っていないこと」を見る)
        assert!(
            began.elapsed() < Duration::from_secs(30),
            "要求ごとの上限が効いていない: {:?}",
            began.elapsed()
        );

        // 同じ client から 2 本目も送れる (Agent を使い切っていない)
        let port2 = serve_hang();
        assert!(c
            .send(&HttpRequest::get(
                format!("https://127.0.0.1:{port2}/servers"),
                Duration::from_millis(300)
            ))
            .is_err());
    }

    /// 本文の読み取りには上限がある (無制限に読み込まない)。
    #[test]
    fn 本文の読み取りに上限がある() {
        assert!(MAX_BODY_BYTES > 0);
        // 4 MiB。Hetzner の 1 ページ (50 件) は数十 KB なので実用でぶつからない
        assert_eq!(MAX_BODY_BYTES, 4 * 1024 * 1024);
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
