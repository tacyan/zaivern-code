//! **最初の Dynamic Provider** (§17)。
//!
//! 選んだ理由は価格ではなく、API が単純で SSH が標準で Provision が速いこと —
//! つまり **Provider abstraction の検証に向いている**から。ここが
//! [`ExecutionProvider`] を素直に満たせるなら、他のクラウドも同じ形で足せる。
//!
//! ## この Provider がしないこと
//!
//! * **コマンドを実行しない。** VM を作る / 数える / 消す / [`ExecutionTarget`]
//!   へ変換する、まで。走らせるのは [`SshTransport`] の仕事。
//! * **価格表をコードへ埋め込まない** (§21)。取れたら API から、取れなければ
//!   [`BillingModel::Unknown`]。
//! * **トークンをファイルへ書かない** (§18)。読むのは環境変数から、使ったら捨てる。
//! * **Zaivern が作っていないサーバーを消さない** (§22)。印が無ければ fail closed。
//!
//! ## 状態の進み方 (§50)
//!
//! ```text
//! Requested → Provisioning → Running → SSH Waiting → Ready
//! ```
//!
//! **SSH が開く前に Scheduler へ渡さない。** 渡すと最初の仕事が必ず失敗する。
//!
//! [`SshTransport`]: crate::features::cloud_execution::transport::ssh::SshTransport
//! [`BillingModel::Unknown`]: crate::features::cloud_execution::model::BillingModel

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;

use crate::features::cloud_execution::model::{
    ids, BillingModel, Capabilities, CloudError, ExecutionTarget, OsFamily, ProviderId,
    TargetCapacity, TargetEndpoint, TargetId, TargetLifecycle, TransportKind,
};

use super::http::{HttpClient, HttpRequest};
use super::{
    is_managed, managed_labels, ExecutionProvider, ProviderProfile, ProvisionSpec,
    ProvisioningMode, LABEL_TARGET_ID,
};

/// API の入口。**プロファイルで差し替えられる**ので、試験は偽サーバーを指す。
pub const DEFAULT_API_BASE: &str = "https://api.hetzner.cloud/v1";

/// 再試行の上限。**無限に繰り返さない** (§49)。
pub const MAX_ATTEMPTS: u32 = 4;

/// 作った VM の SSH が開くまで待つ上限。
pub const READY_TIMEOUT: Duration = Duration::from_secs(300);

/// Hetzner Cloud。
pub struct HetznerProvider {
    profile: ProviderProfile,
    http: Arc<dyn HttpClient>,
    timeout: Duration,
}

impl HetznerProvider {
    pub fn new(profile: ProviderProfile, http: Arc<dyn HttpClient>, timeout: Duration) -> Self {
        Self {
            profile,
            http,
            timeout,
        }
    }

    fn base(&self) -> &str {
        if self.profile.api_base.is_empty() {
            DEFAULT_API_BASE
        } else {
            self.profile.api_base.trim_end_matches('/')
        }
    }

    /// 1 本送って JSON を返す。**再試行はここで面倒を見る** (§49)。
    fn call(&self, make: impl Fn() -> HttpRequest) -> Result<Value, CloudError> {
        let token = self.profile.token()?;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let req = make().bearer(&token).json();
            let outcome = self.http.send(&req).and_then(|res| classify(&res));
            match outcome {
                Ok(v) => return Ok(v),
                Err(e) => {
                    match retry_delay(attempt, &e) {
                        Some(d) => std::thread::sleep(d),
                        None => return Err(e),
                    }
                }
            }
        }
    }

    fn get(&self, path: &str) -> Result<Value, CloudError> {
        let url = format!("{}{path}", self.base());
        let t = self.timeout;
        self.call(move || HttpRequest::get(url.clone(), t))
    }

    /// サーバーを 1 台読む (**破棄の前の照合に使う**)。
    pub fn get_server(&self, id: &str) -> Result<Value, CloudError> {
        let v = self.get(&format!("/servers/{id}"))?;
        v.get("server")
            .cloned()
            .ok_or_else(|| CloudError::provider("hetzner", None, "server が応答にありません"))
    }

    /// 使えるサーバー種別 (`zai cloud provider types`)。
    pub fn list_server_types(&self) -> Result<Vec<ServerType>, CloudError> {
        let v = self.get("/server_types?per_page=50")?;
        Ok(v.get("server_types")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(ServerType::from_json).collect())
            .unwrap_or_default())
    }

    /// 使える場所。
    pub fn list_locations(&self) -> Result<Vec<String>, CloudError> {
        let v = self.get("/locations")?;
        Ok(v.get("locations")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
                    .collect()
            })
            .unwrap_or_default())
    }
}

impl ExecutionProvider for HetznerProvider {
    fn id(&self) -> ProviderId {
        self.profile.id()
    }

    fn mode(&self) -> ProvisioningMode {
        ProvisioningMode::Dynamic
    }

    /// **Zaivern が作ったものだけを数える。** 利用者が別の用途で持っている
    /// サーバーを一覧に出すと、そこへ仕事を載せてしまう。
    fn list_targets(&self) -> Result<Vec<ExecutionTarget>, CloudError> {
        let v = self.get(&format!(
            "/servers?label_selector={}%3D{}",
            super::LABEL_MANAGED_BY,
            super::MANAGED_BY_VALUE
        ))?;
        let servers = v
            .get("servers")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let mut out = Vec::new();
        for s in &servers {
            if let Ok(t) = target_from_server(s, &self.profile) {
                out.push(t);
            }
        }
        Ok(out)
    }

    fn provision(&self, spec: &ProvisionSpec) -> Result<ExecutionTarget, CloudError> {
        let target_id = TargetId::new(ids::new_id("t-"));
        let body = create_body(spec, &target_id, &self.profile)?;
        let url = format!("{}/servers", self.base());
        let t = self.timeout;
        let v = self.call(move || HttpRequest::post(url.clone(), body.clone(), t))?;
        let server = v
            .get("server")
            .ok_or_else(|| CloudError::provider("hetzner", None, "server が応答にありません"))?;
        let mut target = target_from_server(server, &self.profile)?;
        // 応答の label より、いま自分が付けた ID を信じる (作り立ては
        // label がまだ返らないことがある)
        target.id = target_id;
        target.name = spec.name.clone();
        target.capacity = TargetCapacity {
            max_jobs: spec.max_jobs.max(1),
            active_jobs: 0,
        };
        // **まだ Ready ではない。** SSH が開くのを待つのは呼び出し側 (§50)。
        target.lifecycle = TargetLifecycle::Provisioning;
        Ok(target)
    }

    fn destroy(&self, target: &ExecutionTarget) -> Result<(), CloudError> {
        let Some(server_id) = target.provider_ref.clone() else {
            return Err(CloudError::security(
                "この実行先には Provider 側の ID がありません。消せません",
            ));
        };
        // **手元の印だけを信じない。** 消す直前に Provider へ問い合わせて、
        // 本当に Zaivern が作ったものかを確かめる (手元の台帳は編集できる)。
        let server = self.get_server(&server_id)?;
        let labels = labels_of(&server);
        if !is_managed(&labels) {
            return Err(CloudError::security(format!(
                "サーバー {server_id} には {}={} の印がありません。\n\
                 Zaivern が作っていないサーバーは消しません",
                super::LABEL_MANAGED_BY,
                super::MANAGED_BY_VALUE
            )));
        }
        // **実行先 ID の印は「有れば見る」ではなく必須。**
        //
        // 最初の版は `if let Some(marked)` だったので、`managed_by` だけが
        // 付いていて `zaivern_target_id` が無いサーバーは素通りして DELETE まで
        // 進んでいた。印を失ったサーバーや、別の Zaivern が作ったものを
        // 消しうる。**無い・空・食い違いは、どれも消さない理由**である
        // (fail closed)。
        let marked = labels
            .get(LABEL_TARGET_ID)
            .map(String::as_str)
            .unwrap_or("")
            .trim();
        if marked.is_empty() {
            return Err(CloudError::security(format!(
                "サーバー {server_id} に {LABEL_TARGET_ID} の印がありません。\n\
                 どの実行先のものか確かめられないので消しません"
            )));
        }
        if marked != target.id.as_str() {
            return Err(CloudError::security(format!(
                "サーバー {server_id} の印 ({marked}) が、消そうとしている実行先 ({}) と違います",
                target.id
            )));
        }
        let url = format!("{}/servers/{server_id}", self.base());
        let t = self.timeout;
        self.call(move || HttpRequest::delete(url.clone(), t))?;
        Ok(())
    }
}

// ───────────────────────── 純関数 ─────────────────────────

/// 応答を分類する。**ここが「何が再試行してよい失敗か」の唯一の出所** (§49)。
pub fn classify(res: &super::http::HttpResponse) -> Result<Value, CloudError> {
    if res.ok() {
        if res.body.trim().is_empty() {
            return Ok(Value::Null);
        }
        return serde_json::from_str(&res.body).map_err(|e| {
            CloudError::provider("hetzner", Some(res.status), &format!("JSON を読めません: {e}"))
        });
    }
    match res.status {
        // **401 / 403 は再試行しない。** 何度やっても同じで、失敗が積み上がるだけ。
        401 => Err(CloudError::auth(format!(
            "Hetzner の API トークンが受け付けられませんでした (401)。{}",
            api_error_message(&res.body)
        ))),
        403 => Err(CloudError::auth(format!(
            "この API トークンには権限がありません (403)。{}",
            api_error_message(&res.body)
        ))),
        status => Err(CloudError::provider(
            "hetzner",
            Some(status),
            &api_error_message(&res.body),
        )),
    }
}

/// Hetzner の `{"error":{"message":…}}` から人が読む部分だけ取り出す。
fn api_error_message(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|v| {
            v.get("error")
                .and_then(|e| e.get("message"))
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string())
}

/// 次の試行までどれだけ待つか。**待たないなら `None`** (= 諦める)。
///
/// 純関数にしてあるので、試験が**実際に眠らずに**方針を固定できる。
pub fn retry_delay(attempt: u32, e: &CloudError) -> Option<Duration> {
    if attempt >= MAX_ATTEMPTS || !e.retryable() {
        return None;
    }
    // 上限つきの指数バックオフ (0.4s → 0.8s → 1.6s)
    let ms = 400u64 << (attempt - 1).min(4);
    Some(Duration::from_millis(ms.min(8_000)))
}

/// サーバーの label を取り出す。
fn labels_of(server: &Value) -> BTreeMap<String, String> {
    server
        .get("labels")
        .and_then(Value::as_object)
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                .collect()
        })
        .unwrap_or_default()
}

/// サーバー種別 (`zai cloud provider types` が出す)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerType {
    pub name: String,
    pub cores: u16,
    pub memory_mib: u64,
    pub disk_mib: u64,
    pub billing: BillingModel,
}

impl ServerType {
    pub fn from_json(v: &Value) -> Option<Self> {
        let name = v.get("name")?.as_str()?.to_string();
        Some(Self {
            name,
            cores: v.get("cores").and_then(Value::as_u64).unwrap_or(0) as u16,
            // Hetzner は memory を GB の**実数**で返す
            memory_mib: v
                .get("memory")
                .and_then(Value::as_f64)
                .map(|g| (g * 1024.0) as u64)
                .unwrap_or(0),
            disk_mib: v
                .get("disk")
                .and_then(Value::as_f64)
                .map(|g| (g * 1024.0) as u64)
                .unwrap_or(0),
            billing: billing_from_prices(v.get("prices")),
        })
    }
}

/// 価格を**取れたら**読む。取れなければ [`BillingModel::Unknown`] (§21)。
///
/// **表をコードへ書かない。** `CX33 = €8.49` のような行を 1 つでも書くと、
/// 値上げの日に Zaivern が嘘をつく。
pub fn billing_from_prices(prices: Option<&Value>) -> BillingModel {
    let Some(list) = prices.and_then(Value::as_array) else {
        return BillingModel::Unknown;
    };
    let Some(first) = list.first() else {
        return BillingModel::Unknown;
    };
    let read = |key: &str| -> Option<u64> {
        first
            .get(key)?
            .get("gross")
            .or_else(|| first.get(key)?.get("net"))?
            .as_str()?
            .parse::<f64>()
            .ok()
            .map(|v| (v * 100.0).round() as u64)
    };
    match (read("price_hourly"), read("price_monthly")) {
        (Some(h), Some(m)) => BillingModel::HourlyWithMonthlyCap {
            hourly_minor: h,
            monthly_cap_minor: m,
            // 通貨は応答から取る。決め打つと、別通貨の請求先で嘘になる
            currency: first
                .get("price_monthly")
                .and_then(|p| p.get("currency"))
                .and_then(Value::as_str)
                .unwrap_or("EUR")
                .to_string(),
        },
        (None, Some(m)) => BillingModel::FixedMonthly {
            monthly_minor: m,
            currency: "EUR".to_string(),
        },
        _ => BillingModel::Unknown,
    }
}

/// Hetzner の `server` オブジェクトを [`ExecutionTarget`] へ写す。
///
/// **純関数**なので、応答の形が変わったときにネットワーク無しで固定できる。
pub fn target_from_server(
    server: &Value,
    profile: &ProviderProfile,
) -> Result<ExecutionTarget, CloudError> {
    let server_id = server
        .get("id")
        .map(|v| match v {
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            other => other.to_string(),
        })
        .ok_or_else(|| CloudError::provider("hetzner", None, "server.id がありません"))?;
    let name = server
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let ip = server
        .get("public_net")
        .and_then(|n| n.get("ipv4"))
        .and_then(|v| v.get("ip"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| {
            CloudError::provider("hetzner", None, "サーバーに公開 IPv4 がありません")
        })?;

    let labels = labels_of(server);
    let id = labels
        .get(LABEL_TARGET_ID)
        .cloned()
        .unwrap_or_else(|| ids::new_id("t-"));

    let st = server.get("server_type");
    let cores = st
        .and_then(|s| s.get("cores"))
        .and_then(Value::as_u64)
        .map(|c| c as u16);
    let memory_mib = st
        .and_then(|s| s.get("memory"))
        .and_then(Value::as_f64)
        .map(|g| (g * 1024.0) as u64);
    let disk_mib = st
        .and_then(|s| s.get("disk"))
        .and_then(Value::as_f64)
        .map(|g| (g * 1024.0) as u64);
    let arch = st
        .and_then(|s| s.get("architecture"))
        .and_then(Value::as_str)
        .map(crate::features::cloud_execution::model::Architecture::from_uname)
        .unwrap_or(crate::features::cloud_execution::model::Architecture::X86_64);

    let user = if profile.ssh_user.is_empty() {
        // **root を既定にしない** (§23)。cloud-init が作るのはこの名前。
        "zaivern".to_string()
    } else {
        profile.ssh_user.clone()
    };

    Ok(ExecutionTarget {
        id: TargetId::new(id),
        name,
        provider: profile.id(),
        transport: TransportKind::Ssh,
        endpoint: TargetEndpoint::Ssh {
            host: ip,
            user,
            port: 22,
            identity_file: profile.identity_file.clone(),
        },
        capabilities: Capabilities {
            // イメージは Linux 前提 (v1 の対象。§9)
            os: OsFamily::Linux,
            arch,
            cpu_cores: cores,
            memory_mib,
            disk_mib,
            ..Capabilities::default()
        },
        capacity: TargetCapacity {
            max_jobs: profile.max_jobs.max(1),
            active_jobs: 0,
        },
        lifecycle: lifecycle_from_status(server.get("status").and_then(Value::as_str)),
        managed: is_managed(&labels),
        labels,
        billing: billing_from_prices(st.and_then(|s| s.get("prices"))),
        provider_ref: Some(server_id),
        note: String::new(),
    })
}

/// Hetzner の `status` を [`TargetLifecycle`] へ。
///
/// **`running` でも [`TargetLifecycle::Ready`] にしない。** OS が起動していても
/// SSH がまだ開いていないことがあり、そこへ仕事を載せると必ず失敗する (§50)。
/// `Ready` を名乗れるのは、実際に接続を確かめた [`probe`] だけ。
///
/// [`probe`]: crate::features::cloud_execution::transport::ExecutionTransport::probe
pub fn lifecycle_from_status(status: Option<&str>) -> TargetLifecycle {
    match status.unwrap_or("") {
        "running" => TargetLifecycle::Provisioning,
        "initializing" | "starting" | "migrating" | "rebuilding" => TargetLifecycle::Provisioning,
        // **Provider が消している最中なら、こちらも新しい枠を配らない。**
        "deleting" => TargetLifecycle::Destroying,
        "off" | "stopping" => TargetLifecycle::Stopped,
        "unknown" | "" => TargetLifecycle::Unknown,
        _ => TargetLifecycle::Unknown,
    }
}

/// 作成要求の本文。
///
/// **cloud-init に認証情報を埋め込まない** (§23)。作るのは、
/// 作業用ユーザー・SSH 鍵の写し・パスワード認証の停止・置き場だけ。
pub fn create_body(
    spec: &ProvisionSpec,
    target_id: &TargetId,
    profile: &ProviderProfile,
) -> Result<String, CloudError> {
    if spec.name.trim().is_empty() {
        return Err(CloudError::config("--name を指定してください"));
    }
    if spec.server_type.trim().is_empty() {
        return Err(CloudError::config(
            "サーバー種別が決まっていません (--server-type かプロファイルで指定してください)",
        ));
    }
    if spec.image.trim().is_empty() {
        return Err(CloudError::config("イメージが決まっていません"));
    }
    if spec.ssh_key.trim().is_empty() {
        return Err(CloudError::config(
            "SSH 鍵の名前が決まっていません。\n\
             Hetzner 側に登録した公開鍵の名前を --ssh-key で指定してください \
             (Zaivern は秘密鍵を送りません)",
        ));
    }

    let user = if profile.ssh_user.is_empty() {
        "zaivern"
    } else {
        &profile.ssh_user
    };
    let mut labels = managed_labels(target_id.as_str(), &profile.name);
    for (k, v) in &spec.labels {
        labels.insert(k.clone(), v.clone());
    }

    let mut body = serde_json::Map::new();
    body.insert("name".into(), Value::String(spec.name.clone()));
    body.insert(
        "server_type".into(),
        Value::String(spec.server_type.clone()),
    );
    body.insert("image".into(), Value::String(spec.image.clone()));
    if let Some(loc) = &spec.location {
        if !loc.is_empty() {
            body.insert("location".into(), Value::String(loc.clone()));
        }
    }
    body.insert(
        "ssh_keys".into(),
        Value::Array(vec![Value::String(spec.ssh_key.clone())]),
    );
    body.insert("start_after_create".into(), Value::Bool(true));
    body.insert(
        "labels".into(),
        Value::Object(
            labels
                .into_iter()
                .map(|(k, v)| (k, Value::String(v)))
                .collect(),
        ),
    );
    body.insert("user_data".into(), Value::String(cloud_init(user)));
    serde_json::to_string(&Value::Object(body))
        .map_err(|e| CloudError::io(format!("要求を組めません: {e}")))
}

/// cloud-init。**秘密は 1 バイトも入らない** (§23 / §24)。
pub fn cloud_init(user: &str) -> String {
    // ユーザー名は検査済みの値しか来ないが、ここでも念のため確かめる
    // (YAML へ入る値なので、改行が混ざると別のキーを足せてしまう)
    let user = user
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>();
    let user = if user.is_empty() {
        "zaivern".to_string()
    } else {
        user
    };
    format!(
        "#cloud-config\n\
         users:\n\
         \x20 - name: {user}\n\
         \x20   groups: [sudo]\n\
         \x20   shell: /bin/bash\n\
         \x20   sudo: ['ALL=(ALL) NOPASSWD:ALL']\n\
         \x20   ssh_authorized_keys: []\n\
         disable_root: true\n\
         ssh_pwauth: false\n\
         package_update: true\n\
         packages: [git]\n\
         runcmd:\n\
         \x20 - [ sh, -c, \"cp -r /root/.ssh /home/{user}/.ssh 2>/dev/null || true\" ]\n\
         \x20 - [ sh, -c, \"chown -R {user}:{user} /home/{user}/.ssh 2>/dev/null || true\" ]\n\
         \x20 - [ sh, -c, \"install -d -o {user} -g {user} /home/{user}/.zaivern/cloud\" ]\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::provider::http::HttpResponse;
    use crate::features::cloud_execution::provider::ProviderKind;
    use crate::features::cloud_execution::test_support::{FakeHttpClient, RecordedCall};

    /// 製品コードだけを残す (コメント行と `#[cfg(test)]` 以降を落とす)。
    fn product_code(src: &str) -> String {
        let text = src.replace("\r\n", "\n");
        let head = text.split("#[cfg(test)]").next().unwrap_or_default().to_string();
        head.lines()
            .filter(|l| {
                let t = l.trim_start();
                !t.starts_with("//") && !t.starts_with("/*") && !t.starts_with('*')
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn profile() -> ProviderProfile {
        ProviderProfile {
            name: "hetzner-eu".into(),
            kind: ProviderKind::Hetzner,
            token_env: "ZAIVERN_TEST_HCLOUD_TOKEN".into(),
            location: "fsn1".into(),
            server_type: "cx33".into(),
            image: "ubuntu-24.04".into(),
            ssh_key: "zaivern".into(),
            ssh_user: "zaivern".into(),
            max_jobs: 4,
            api_base: "https://api.example/v1".into(),
            identity_file: None,
        }
    }

    fn server_json() -> Value {
        serde_json::json!({
            "id": 42,
            "name": "zai-worker-01",
            "status": "running",
            "public_net": { "ipv4": { "ip": "203.0.113.10" } },
            "server_type": {
                "name": "cx33",
                "cores": 4,
                "memory": 8.0,
                "disk": 80.0,
                "architecture": "x86",
                "prices": [{
                    "location": "fsn1",
                    "price_hourly": { "net": "0.0140", "gross": "0.0167" },
                    "price_monthly": { "net": "8.4900", "gross": "10.1031", "currency": "EUR" }
                }]
            },
            "labels": {
                "managed_by": "zaivern",
                "zaivern_target_id": "t-abc",
                "zaivern_profile": "hetzner-eu"
            }
        })
    }

    #[test]
    fn hetzner_401_is_auth_error() {
        let e = classify(&HttpResponse {
            status: 401,
            body: r#"{"error":{"message":"unable to authenticate"}}"#.into(),
        })
        .expect_err("失敗する");
        assert!(matches!(e, CloudError::Auth(_)), "{e:?}");
        // 再試行しない (何度やっても同じ)
        assert!(!e.retryable());
        assert_eq!(retry_delay(1, &e), None);
        assert_eq!(e.exit_code(), 3, "設定・認証の終了コード");
        assert!(format!("{e}").contains("unable to authenticate"), "{e}");
    }

    #[test]
    fn hetzner_429_is_retryable() {
        let e = classify(&HttpResponse {
            status: 429,
            body: r#"{"error":{"message":"rate limit exceeded"}}"#.into(),
        })
        .expect_err("失敗する");
        assert!(e.retryable(), "{e:?}");
        // **上限つき**の指数バックオフ。無限には繰り返さない
        assert_eq!(retry_delay(1, &e), Some(Duration::from_millis(400)));
        assert_eq!(retry_delay(2, &e), Some(Duration::from_millis(800)));
        assert_eq!(retry_delay(3, &e), Some(Duration::from_millis(1600)));
        assert_eq!(retry_delay(MAX_ATTEMPTS, &e), None, "上限で諦める");
        // 5xx も同じ
        let e5 = classify(&HttpResponse {
            status: 503,
            body: String::new(),
        })
        .expect_err("失敗する");
        assert!(e5.retryable());
        // 4xx の他は再試行しない
        let e4 = classify(&HttpResponse {
            status: 404,
            body: String::new(),
        })
        .expect_err("失敗する");
        assert!(!e4.retryable());
    }

    /// **再試行が止まること**を、実際に呼び出し回数で確かめる。
    ///
    /// 「上限つき」と書いてあるだけでは、ループが本当に抜けるか分からない。
    /// 時間ではなく**回数**で見る (時間で線を引くと必ず嘘をつく)。
    #[test]
    fn 再試行は上限で止まる() {
        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");
        let http = FakeHttpClient::repeating(
            HttpResponse {
                status: 429,
                body: r#"{"error":{"message":"rate limit exceeded"}}"#.into(),
            },
            // 上限より多く用意する。**余った分が使われたら赤くなる**
            (MAX_ATTEMPTS + 3) as usize,
        );
        let calls = http.calls();
        let p = HetznerProvider::new(profile(), std::sync::Arc::new(http), Duration::from_secs(5));
        let e = p.list_targets().expect_err("諦める");
        assert!(format!("{e}").contains("429"), "{e}");
        assert_eq!(
            calls.lock().expect("読める").len(),
            MAX_ATTEMPTS as usize,
            "上限どおりに止まっていない"
        );
    }

    #[test]
    fn hetzner_create_response_to_target() {
        let t = target_from_server(&server_json(), &profile()).expect("写せる");
        assert_eq!(t.id.as_str(), "t-abc", "印の実行先 ID を引き継ぐ");
        assert_eq!(t.name, "zai-worker-01");
        assert_eq!(t.transport, TransportKind::Ssh);
        assert_eq!(t.provider_ref.as_deref(), Some("42"));
        assert!(t.managed);
        match &t.endpoint {
            TargetEndpoint::Ssh {
                host, user, port, ..
            } => {
                assert_eq!(host, "203.0.113.10");
                // **root を既定にしない**
                assert_eq!(user, "zaivern");
                assert_eq!(*port, 22);
            }
            other => panic!("SSH ではない: {other:?}"),
        }
        assert_eq!(t.capabilities.cpu_cores, Some(4));
        assert_eq!(t.capabilities.memory_mib, Some(8192));
        assert_eq!(t.capacity.max_jobs, 4);
        // running でも Ready にしない (SSH がまだ開いていない)
        assert_eq!(t.lifecycle, TargetLifecycle::Provisioning);
        assert_ne!(t.lifecycle, TargetLifecycle::Ready);
    }

    #[test]
    fn 価格はapiから読みコードへ埋め込まない() {
        let t = target_from_server(&server_json(), &profile()).expect("写せる");
        match &t.billing {
            BillingModel::HourlyWithMonthlyCap {
                hourly_minor,
                monthly_cap_minor,
                currency,
            } => {
                assert_eq!(*hourly_minor, 2, "0.0167 → 2 セント (四捨五入)");
                assert_eq!(*monthly_cap_minor, 1010);
                assert_eq!(currency, "EUR");
            }
            other => panic!("価格を読めていない: {other:?}"),
        }
        // 取れなければ Unknown。**推測しない**
        assert_eq!(billing_from_prices(None), BillingModel::Unknown);
        assert_eq!(
            billing_from_prices(Some(&serde_json::json!([]))),
            BillingModel::Unknown
        );

        // 価格表がソースへ埋め込まれていないこと。
        // **コメント行は除く** — 「書かない」と説明した文そのものを咎めると、
        // 番人が自分の説明で赤くなる (このリポジトリが 3 度踏んだ形)。
        let body = product_code(include_str!("hetzner.rs"));
        assert!(!body.contains("8.49"), "価格をコードへ書いている");
        assert!(!body.to_lowercase().contains("cx33"), "サーバー種別をコードへ書いている");
        // **番人が空回りしていないことを証明する**
        assert!(
            product_code("let p = 8.49;\n// 8.49 とは書かない\n").contains("8.49"),
            "番人が空回りしている"
        );
    }

    #[test]
    fn hetzner_does_not_destroy_unmanaged_server() {
        // 手元の台帳では「Zaivern のもの」と言い張っているが、
        // Provider 側には印が無い、という状況を作る
        let mut unmanaged = server_json();
        unmanaged["labels"] = serde_json::json!({ "owner": "someone-else" });
        let http = FakeHttpClient::new(vec![
            HttpResponse {
                status: 200,
                body: serde_json::json!({ "server": unmanaged }).to_string(),
            },
            // 消す要求まで進んだら、これが返って**テストが緑になってしまう**
            HttpResponse {
                status: 200,
                body: "{}".into(),
            },
        ]);
        let calls = http.calls();
        let p = HetznerProvider::new(profile(), std::sync::Arc::new(http), Duration::from_secs(5));

        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");
        let mut target = target_from_server(&server_json(), &profile()).expect("写せる");
        target.managed = true; // 手元の台帳は嘘をついている

        let e = p.destroy(&target).expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        // **DELETE を 1 度も送っていないこと** (断ったつもりで送っていた、を防ぐ)
        let sent = calls.lock().expect("読める").clone();
        assert_eq!(sent.len(), 1, "余計な要求を送っている: {sent:?}");
        assert_eq!(sent[0].method, "GET");
        assert!(!sent.iter().any(|c| c.method == "DELETE"), "{sent:?}");
    }

    #[test]
    fn 印の実行先idが違えば消さない() {
        let http = FakeHttpClient::new(vec![HttpResponse {
            status: 200,
            body: serde_json::json!({ "server": server_json() }).to_string(),
        }]);
        let calls = http.calls();
        let p = HetznerProvider::new(profile(), std::sync::Arc::new(http), Duration::from_secs(5));
        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");

        let mut target = target_from_server(&server_json(), &profile()).expect("写せる");
        target.id = TargetId::new("t-someone-else");
        let e = p.destroy(&target).expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        assert!(!calls
            .lock()
            .expect("読める")
            .iter()
            .any(|c| c.method == "DELETE"));
    }

    /// 破棄を頼んだときに **DELETE が飛んだかどうか**を数える。
    ///
    /// 「エラーが返ること」だけを見ると、`DELETE` を撃ってからエラーを返す
    /// 実装でも緑になる。**送った要求の中身**で確かめる。
    fn destroy_with_labels(labels: Value, target_id: &str) -> (CloudError, Vec<RecordedCall>) {
        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");
        let mut server = server_json();
        server["labels"] = labels;
        let http = FakeHttpClient::new(vec![
            HttpResponse {
                status: 200,
                body: serde_json::json!({ "server": server }).to_string(),
            },
            // 破棄まで進んでしまったら、これが返って**テストが緑になってしまう**
            HttpResponse {
                status: 200,
                body: "{}".into(),
            },
        ]);
        let calls = http.calls();
        let p = HetznerProvider::new(profile(), std::sync::Arc::new(http), Duration::from_secs(5));

        let mut target = target_from_server(&server_json(), &profile()).expect("写せる");
        target.id = TargetId::new(target_id);
        // **手元の台帳は「Zaivern のもの」と言い張っている。**
        // それだけで消せてはいけない (台帳はただのテキストファイルで、編集できる)
        target.managed = true;
        let e = p.destroy(&target).expect_err("断る");
        let sent = calls.lock().expect("読める").clone();
        (e, sent)
    }

    fn assert_no_delete(sent: &[RecordedCall]) {
        assert!(
            !sent.iter().any(|c| c.method == "DELETE"),
            "DELETE を送ってしまっている: {sent:?}"
        );
        // 問い合わせの 1 本だけで止まっている
        assert_eq!(sent.len(), 1, "余計な要求を送っている: {sent:?}");
        assert_eq!(sent[0].method, "GET");
    }

    /// **実行先 ID の印が無ければ消さない。**
    ///
    /// 最初の版は「印が有れば一致を確かめる」だったので、`managed_by` だけ
    /// 付いていて `zaivern_target_id` が無いサーバーは**素通りして DELETE**
    /// まで進んでいた。別の Zaivern が作ったものや、印を途中で失ったものを
    /// 消しうる。
    #[test]
    fn hetzner_requires_target_id_label_to_destroy() {
        // (1) 管理の印そのものが無い
        let (e, sent) = destroy_with_labels(serde_json::json!({ "owner": "someone-else" }), "t-abc");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        assert_no_delete(&sent);

        // (2) 管理の印はあるが、実行先 ID の印が無い ← これが指摘の本体
        let (e, sent) = destroy_with_labels(
            serde_json::json!({ "managed_by": "zaivern", "zaivern_profile": "hetzner-eu" }),
            "t-abc",
        );
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        assert!(
            format!("{e}").contains(LABEL_TARGET_ID),
            "何が足りないのか言っていない: {e}"
        );
        assert_no_delete(&sent);

        // (3) 実行先 ID の印が空文字
        let (e, sent) = destroy_with_labels(
            serde_json::json!({ "managed_by": "zaivern", "zaivern_target_id": "" }),
            "t-abc",
        );
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        assert_no_delete(&sent);

        // (4) 実行先 ID が食い違う
        let (e, sent) = destroy_with_labels(
            serde_json::json!({ "managed_by": "zaivern", "zaivern_target_id": "t-other" }),
            "t-abc",
        );
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        assert_no_delete(&sent);

        // 断る理由に秘密が出ていないこと
        assert!(!format!("{e}").contains("super-secret-test-token"), "{e}");
    }

    /// **両方の印が正しく揃ったときだけ、意図したサーバーへ 1 回だけ DELETE する。**
    #[test]
    fn hetzner_destroys_only_the_marked_server() {
        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");
        let http = FakeHttpClient::new(vec![
            HttpResponse {
                status: 200,
                body: serde_json::json!({ "server": server_json() }).to_string(),
            },
            HttpResponse {
                status: 200,
                body: "{}".into(),
            },
        ]);
        let calls = http.calls();
        let p = HetznerProvider::new(profile(), std::sync::Arc::new(http), Duration::from_secs(5));

        let target = target_from_server(&server_json(), &profile()).expect("写せる");
        assert_eq!(target.id.as_str(), "t-abc");
        assert!(target.managed);
        p.destroy(&target).expect("消せる");

        let sent = calls.lock().expect("読める").clone();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[0].method, "GET", "先に問い合わせていない");
        assert_eq!(sent[1].method, "DELETE");
        // **意図したサーバーへ**送っている (server_json の id は 42)
        assert!(sent[1].url.ends_with("/servers/42"), "{}", sent[1].url);
    }

    #[test]
    fn 作成要求に秘密が入らない() {
        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");
        let spec = ProvisionSpec {
            name: "zai-worker-01".into(),
            server_type: "cx33".into(),
            location: Some("fsn1".into()),
            image: "ubuntu-24.04".into(),
            ssh_key: "zaivern".into(),
            max_jobs: 4,
            ..ProvisionSpec::default()
        };
        let body = create_body(&spec, &TargetId::new("t-abc"), &profile()).expect("組める");
        assert!(
            !body.contains("super-secret-test-token"),
            "トークンが本文に入っている: {body}"
        );
        assert!(!body.contains("PRIVATE KEY"), "{body}");
        // 印が付いている (これが無いと後で消せない)
        assert!(body.contains("\"managed_by\":\"zaivern\""), "{body}");
        assert!(body.contains("\"zaivern_target_id\":\"t-abc\""), "{body}");
        // 鍵は**名前**だけを渡す
        assert!(body.contains("\"ssh_keys\":[\"zaivern\"]"), "{body}");
    }

    #[test]
    fn cloud_initに認証情報を埋め込まない() {
        let ci = cloud_init("zaivern");
        assert!(ci.contains("ssh_pwauth: false"), "{ci}");
        assert!(ci.contains("disable_root: true"), "{ci}");
        for banned in ["TOKEN", "PRIVATE KEY", "password", "api_key"] {
            assert!(!ci.contains(banned), "{banned} が cloud-init に入っている");
        }
        // ユーザー名へ改行を混ぜても YAML を壊せない
        let evil = cloud_init("x\nruncmd:\n - [sh, -c, id]");
        assert!(!evil.contains("\nruncmd:\n - [sh, -c, id]"), "{evil}");
    }

    #[test]
    fn 足りない指定は作る前に断る() {
        let id = TargetId::new("t-1");
        let p = profile();
        for spec in [
            ProvisionSpec::default(),
            ProvisionSpec {
                name: "a".into(),
                ..ProvisionSpec::default()
            },
            ProvisionSpec {
                name: "a".into(),
                server_type: "cx33".into(),
                ..ProvisionSpec::default()
            },
            ProvisionSpec {
                name: "a".into(),
                server_type: "cx33".into(),
                image: "ubuntu".into(),
                ..ProvisionSpec::default()
            },
        ] {
            assert!(create_body(&spec, &id, &p).is_err(), "{spec:?} を通した");
        }
    }

    #[test]
    fn 管理下のものだけを数える() {
        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");
        let http = FakeHttpClient::new(vec![HttpResponse {
            status: 200,
            body: serde_json::json!({ "servers": [server_json()] }).to_string(),
        }]);
        let calls = http.calls();
        let p = HetznerProvider::new(profile(), std::sync::Arc::new(http), Duration::from_secs(5));
        let list = p.list_targets().expect("数えられる");
        assert_eq!(list.len(), 1);
        // 問い合わせ自体が印で絞っている (取ってから捨てるのではない)
        let url = calls.lock().expect("読める")[0].url.clone();
        assert!(url.contains("label_selector=managed_by"), "{url}");
    }

    #[test]
    fn サーバー種別を読む() {
        let t = ServerType::from_json(&server_json()["server_type"]).expect("読める");
        assert_eq!(t.name, "cx33");
        assert_eq!(t.cores, 4);
        assert_eq!(t.memory_mib, 8192);
    }

    /// **本物の Hetzner を触る唯一のテスト** (§60)。
    ///
    /// `ZAIVERN_CLOUD_E2E=1` と `HCLOUD_TOKEN` の**両方**が無ければ降りる。
    /// ふつうの `cargo test` で課金が起きることはない。
    ///
    /// 触るのは**読み取りだけ** (`/server_types`)。テストが VM を作らないのは、
    /// 作ったものを片付け損ねると請求が止まらないため — 作る経路は
    /// 手動 Acceptance Test の担当 (`docs/cloud-execution.md`)。
    #[test]
    fn 実物のapiに繋がる() {
        let enabled = std::env::var("ZAIVERN_CLOUD_E2E").as_deref() == Ok("1");
        let has_token = std::env::var("HCLOUD_TOKEN")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false);
        if !enabled || !has_token {
            // **[skip] は緑ではない。** どちらが無くて降りたのかを必ず言う
            eprintln!(
                "[skip] 実物の Hetzner は触りません (ZAIVERN_CLOUD_E2E={} / HCLOUD_TOKEN={})",
                if enabled { "1" } else { "未設定" },
                if has_token { "設定あり" } else { "未設定" }
            );
            return;
        }
        let live = ProviderProfile {
            name: "e2e".into(),
            kind: ProviderKind::Hetzner,
            token_env: "HCLOUD_TOKEN".into(),
            ..ProviderProfile::default()
        };
        let p = HetznerProvider::new(
            live,
            std::sync::Arc::new(super::super::http::UreqClient::new(Duration::from_secs(30))),
            Duration::from_secs(30),
        );
        let types = p.list_server_types().expect("サーバー種別を読める");
        assert!(!types.is_empty(), "1 件も返ってこない");
        // 価格が読めていること (読めなければ Unknown になる = 実装の穴)
        assert!(
            types.iter().any(|t| t.billing != BillingModel::Unknown),
            "どの種別も価格を読めていない"
        );
    }

    #[test]
    fn 公開ipのないサーバーは実行先にしない() {
        // 届かない実行先を一覧へ出すと、選ばれた瞬間に失敗する
        let mut s = server_json();
        s["public_net"] = serde_json::json!({});
        assert!(target_from_server(&s, &profile()).is_err());
    }
}
