//! **実行先をどこから持ってくるか**だけを担う層 (§11)。
//!
//! Provider は**コマンドを 1 つも実行しない**。作る・数える・消す・
//! [`ExecutionTarget`] へ変換する、までが仕事で、そこで何を走らせるかは
//! [`super::transport`] の担当。分けてあるので、
//!
//! * Hetzner で作った VM も自宅の Linux も**同じ SshTransport** で動く
//! * Provider を 1 つ足しても Transport と Scheduler は 1 行も変わらない
//!
//! ## v1 の Provider
//!
//! | Provider | 方式 | 何をするか |
//! |---|---|---|
//! | [`local::LocalProvider`] | Static | 手元の機械を 1 つ返す |
//! | [`static_ssh::StaticSshProvider`] | Static | 利用者が登録した SSH 先を返す |
//! | [`hetzner::HetznerProvider`] | Dynamic | API で VM を作る / 消す |
//!
//! **v1.0 最大の価値は真ん中の行** (§52)。API を持たない VPS でも、
//! `IP` + `SSH` + `Linux` の 3 つがあれば Zaivern の実行先になる。

pub mod hetzner;
pub mod http;
pub mod local;
pub mod static_ssh;

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::model::{CloudError, ExecutionTarget, ProviderId};

pub use hetzner::HetznerProvider;
pub use http::HttpClient;
pub use local::LocalProvider;
pub use static_ssh::StaticSshProvider;

/// 実行先の湧き方。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProvisioningMode {
    /// すでに在るものを返すだけ。
    Static,
    /// 頼まれたら作る (**お金がかかる**)。
    Dynamic,
}

/// Provider の面。
pub trait ExecutionProvider: Send + Sync {
    fn id(&self) -> ProviderId;

    fn mode(&self) -> ProvisioningMode;

    /// いま在る実行先を数える。
    fn list_targets(&self) -> Result<Vec<ExecutionTarget>, CloudError>;

    /// 実行先を作る。**Static の Provider は [`CloudError::Unsupported`]** を返す。
    fn provision(&self, spec: &ProvisionSpec) -> Result<ExecutionTarget, CloudError>;

    /// 実行先を消す。**Zaivern が作ったものでなければ必ず断る** (§22)。
    fn destroy(&self, target: &ExecutionTarget) -> Result<(), CloudError>;
}

/// 作るときの指定 (§21)。**価格をここに書かない。**
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionSpec {
    pub name: String,
    pub server_type: String,
    pub location: Option<String>,
    pub image: String,
    pub ssh_key: String,
    pub labels: BTreeMap<String, String>,
    /// 使い捨てか。**途中で失敗したときに消してよいのはこれだけ** (§51)。
    pub ephemeral: bool,
    /// 同時実行枠。利用者が決める (§31)。
    pub max_jobs: u16,
}

impl Default for ProvisionSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            server_type: String::new(),
            location: None,
            image: String::new(),
            ssh_key: String::new(),
            labels: BTreeMap::new(),
            ephemeral: false,
            max_jobs: 1,
        }
    }
}

/// Zaivern が管理している印。**これが無いサーバーは決して消さない** (§22)。
pub const LABEL_MANAGED_BY: &str = "managed_by";
/// [`LABEL_MANAGED_BY`] の値。
pub const MANAGED_BY_VALUE: &str = "zaivern";
/// Zaivern 側の実行先 ID を載せる印。
pub const LABEL_TARGET_ID: &str = "zaivern_target_id";
/// どのプロファイルが作ったかを載せる印。
pub const LABEL_PROFILE: &str = "zaivern_profile";

/// Zaivern が付ける印を組む。
pub fn managed_labels(target_id: &str, profile: &str) -> BTreeMap<String, String> {
    let mut m = BTreeMap::new();
    m.insert(LABEL_MANAGED_BY.to_string(), MANAGED_BY_VALUE.to_string());
    m.insert(LABEL_TARGET_ID.to_string(), target_id.to_string());
    m.insert(LABEL_PROFILE.to_string(), profile.to_string());
    m
}

/// **Zaivern が作ったものか。** 印が無ければ `false` (fail closed)。
pub fn is_managed(labels: &BTreeMap<String, String>) -> bool {
    labels.get(LABEL_MANAGED_BY).map(String::as_str) == Some(MANAGED_BY_VALUE)
}

// ───────────────────── プロファイル ─────────────────────

/// Provider の種別。**利用者が付ける名前 ([`ProviderProfile::name`]) とは別物。**
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    Local,
    StaticSsh,
    Hetzner,
}

impl ProviderKind {
    pub fn id(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::StaticSsh => "static-ssh",
            Self::Hetzner => "hetzner",
        }
    }

    pub fn from_id(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "local" => Some(Self::Local),
            "static-ssh" | "static_ssh" | "ssh" => Some(Self::StaticSsh),
            "hetzner" => Some(Self::Hetzner),
            _ => None,
        }
    }

    pub fn mode(self) -> ProvisioningMode {
        match self {
            Self::Local | Self::StaticSsh => ProvisioningMode::Static,
            Self::Hetzner => ProvisioningMode::Dynamic,
        }
    }
}

/// `providers.json` に保存される 1 件。
///
/// **保存してよいのは §40 の一覧だけ。** トークンの**値**は入らない
/// (入る形にすると、いつか誰かが入れる)。入るのは環境変数の**名前**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// 利用者が付けた名前 (`hetzner-eu` 等)。CLI と UI はこれで指す。
    pub name: String,
    pub kind: ProviderKind,
    /// API トークンを持つ**環境変数の名前** (値ではない)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token_env: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub location: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub server_type: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub image: String,
    /// Provider 側に登録済みの SSH 公開鍵の**名前**。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_key: String,
    /// 作った VM へ入るときのユーザー名。**root を既定にしない** (§23)。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ssh_user: String,
    #[serde(default)]
    pub max_jobs: u16,
    /// API の入口を差し替える (試験用)。空なら Provider の既定。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub api_base: String,
    /// 秘密鍵の**パス** (中身ではない)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_file: Option<PathBuf>,
}

impl Default for ProviderProfile {
    fn default() -> Self {
        Self {
            name: String::new(),
            kind: ProviderKind::StaticSsh,
            token_env: String::new(),
            location: String::new(),
            server_type: String::new(),
            image: String::new(),
            ssh_key: String::new(),
            ssh_user: String::new(),
            max_jobs: 1,
            api_base: String::new(),
            identity_file: None,
        }
    }
}

impl ProviderProfile {
    /// **保存する前に、秘密を抱えていないことを確かめる** (§40)。
    ///
    /// 「保存しないつもり」ではなく、保存する経路そのもので止める。
    pub fn assert_no_secret(&self) -> Result<(), CloudError> {
        if !self.token_env.is_empty() && !is_env_name(&self.token_env) {
            return Err(CloudError::security(format!(
                "token_env には環境変数の**名前**だけを入れてください \
                 (大文字・数字・_ のみ)。値そのものは保存しません: {}",
                mask_shape(&self.token_env)
            )));
        }
        // 値を取り違えて他の欄へ入れた場合も止める
        for (label, v) in [
            ("location", &self.location),
            ("server_type", &self.server_type),
            ("image", &self.image),
            ("ssh_key", &self.ssh_key),
            ("ssh_user", &self.ssh_user),
        ] {
            if looks_like_secret(v) {
                return Err(CloudError::security(format!(
                    "{label} に秘密らしい値が入っています。プロファイルへ秘密は保存できません"
                )));
            }
        }
        Ok(())
    }

    /// トークンを環境変数から読む。**読むのはここ 1 か所**で、値はどこにも
    /// 保存しない (使ったらすぐ捨てる)。
    pub fn token(&self) -> Result<String, CloudError> {
        if self.token_env.is_empty() {
            return Err(CloudError::config(format!(
                "{} には API トークンの環境変数名が設定されていません",
                self.name
            )));
        }
        super::redact::register_secret_env(&self.token_env);
        match std::env::var(&self.token_env) {
            Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
            _ => Err(CloudError::auth(format!(
                "環境変数 {} が設定されていません。\n  export {}=…  を実行してから再試行してください",
                self.token_env, self.token_env
            ))),
        }
    }

    /// トークンが設定されているか (**値は返さない**。`doctor` 用)。
    pub fn token_present(&self) -> bool {
        !self.token_env.is_empty()
            && std::env::var(&self.token_env)
                .map(|v| !v.trim().is_empty())
                .unwrap_or(false)
    }

    pub fn id(&self) -> ProviderId {
        ProviderId::new(&self.name)
    }
}

/// 環境変数の名前として妥当か。
fn is_env_name(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

/// 値の**形**だけを言う (中身は言わない)。
fn mask_shape(s: &str) -> String {
    format!("{} 文字の値", s.chars().count())
}

/// 見るからに秘密の値か (長い英数字の羅列 / `Bearer` を含む)。
fn looks_like_secret(s: &str) -> bool {
    if s.to_lowercase().contains("bearer") {
        return true;
    }
    let alnum = s.chars().filter(|c| c.is_ascii_alphanumeric()).count();
    s.chars().count() >= 32 && alnum * 10 >= s.chars().count() * 9
}

/// Provider を作るときに要るもの。**HTTP の実装を差し替えられる**ので、
/// 試験で本物のクラウドへ課金しない (§54)。
pub struct ProviderCtx {
    pub http: Arc<dyn HttpClient>,
    pub timeout: Duration,
    /// 手元の同時実行枠 (`LocalProvider` 用)。
    pub local_max_jobs: u16,
}

impl ProviderCtx {
    /// 本番用 (実 HTTP)。
    pub fn live(timeout: Duration, local_max_jobs: u16) -> Self {
        Self {
            http: Arc::new(http::UreqClient::new(timeout)),
            timeout,
            local_max_jobs,
        }
    }
}

/// プロファイルから Provider を作る。
///
/// **ここが「Provider 名で分岐する」唯一の場所**であり、これは分岐ではなく
/// **構築**である (作った後は誰も種別を見ない)。新しい Provider を足すときに
/// 触るのはこの `match` 1 つで、Scheduler にも Transport にも波及しない。
pub fn build(
    profile: &ProviderProfile,
    ctx: &ProviderCtx,
) -> Result<Box<dyn ExecutionProvider>, CloudError> {
    match profile.kind {
        ProviderKind::Local => Ok(Box::new(LocalProvider::new(ctx.local_max_jobs))),
        ProviderKind::StaticSsh => Ok(Box::new(StaticSshProvider::new(profile.id()))),
        ProviderKind::Hetzner => Ok(Box::new(HetznerProvider::new(
            profile.clone(),
            ctx.http.clone(),
            ctx.timeout,
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 管理していないサーバーは管理下と見なさない() {
        // 印が無ければ false (fail closed)
        assert!(!is_managed(&BTreeMap::new()));
        let mut other = BTreeMap::new();
        other.insert("managed_by".into(), "terraform".into());
        assert!(!is_managed(&other));
        assert!(is_managed(&managed_labels("t-1", "hetzner-eu")));
    }

    #[test]
    fn 印には実行先idとプロファイル名が載る() {
        let l = managed_labels("t-abc", "hetzner-eu");
        assert_eq!(l.get(LABEL_TARGET_ID).map(String::as_str), Some("t-abc"));
        assert_eq!(l.get(LABEL_PROFILE).map(String::as_str), Some("hetzner-eu"));
    }

    #[test]
    fn トークンの値をプロファイルへ入れさせない() {
        let mut p = ProviderProfile {
            name: "h".into(),
            kind: ProviderKind::Hetzner,
            token_env: "super-secret-test-token-value-here".into(),
            ..ProviderProfile::default()
        };
        let e = p.assert_no_secret().expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        // 断る理由の中に値そのものが出ていないこと
        assert!(!format!("{e}").contains("super-secret"), "{e}");

        p.token_env = "HCLOUD_TOKEN".into();
        assert!(p.assert_no_secret().is_ok());
    }

    #[test]
    fn 他の欄へ入れた秘密も止める() {
        let p = ProviderProfile {
            name: "h".into(),
            kind: ProviderKind::Hetzner,
            token_env: "HCLOUD_TOKEN".into(),
            image: "Bearer abc".into(),
            ..ProviderProfile::default()
        };
        assert!(matches!(p.assert_no_secret(), Err(CloudError::Security(_))));
    }

    #[test]
    fn 環境変数名の判定() {
        assert!(is_env_name("HCLOUD_TOKEN"));
        assert!(is_env_name("A1_B"));
        assert!(!is_env_name("hcloud_token"), "小文字は名前として扱わない");
        assert!(!is_env_name("1ABC"));
        assert!(!is_env_name(""));
        assert!(!is_env_name("ABC-DEF"));
    }

    #[test]
    fn トークンが無ければ認証の失敗として返る() {
        let p = ProviderProfile {
            name: "h".into(),
            kind: ProviderKind::Hetzner,
            token_env: "ZAIVERN_TEST_TOKEN_THAT_IS_NOT_SET".into(),
            ..ProviderProfile::default()
        };
        let e = p.token().expect_err("失敗する");
        assert!(matches!(e, CloudError::Auth(_)), "{e:?}");
        // 終了コードは 3 (設定・認証)
        assert_eq!(e.exit_code(), 3);
        assert!(!p.token_present());
    }

    /// **どの Provider も守る約束**を、偽物で 1 か所に固定する。
    ///
    /// 実装ごとに書くと、新しい Provider を足した人がこの約束を知らずに
    /// 通り抜ける。ここに置いておけば、偽物が満たせる形かどうかで
    /// 「trait がその約束を表現できているか」も同時に確かめられる。
    #[test]
    fn providerの約束() {
        use crate::features::cloud_execution::test_support::{target, FakeProvider, TargetOpts};

        // (1) Static な Provider は作れない
        let stat = FakeProvider::new("p", vec![target("a", TargetOpts::default())]);
        assert_eq!(stat.mode(), ProvisioningMode::Static);
        assert!(matches!(
            stat.provision(&ProvisionSpec::default()),
            Err(CloudError::Unsupported(_))
        ));

        // (2) **Zaivern が作っていない実行先は消さない** (fail closed)
        let unmanaged = target("a", TargetOpts::default());
        assert!(!unmanaged.managed);
        let e = stat.destroy(&unmanaged).expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        assert!(stat.destroyed.lock().expect("読める").is_empty());
        // 断っても一覧から消えていない
        assert_eq!(stat.list_targets().expect("数えられる").len(), 1);

        // (3) Dynamic は作れて、作ったものは managed になり、消せる
        let dynamic = FakeProvider::new("d", vec![]).dynamic();
        assert_eq!(dynamic.mode(), ProvisioningMode::Dynamic);
        let made = dynamic
            .provision(&ProvisionSpec {
                name: "w1".into(),
                ..ProvisionSpec::default()
            })
            .expect("作れる");
        assert!(made.managed, "作ったものに印が付いていない");
        // **作った直後は Ready ではない** (SSH をまだ確かめていない)
        assert_ne!(made.lifecycle, crate::features::cloud_execution::model::TargetLifecycle::Ready);
        assert_eq!(dynamic.list_targets().expect("数えられる").len(), 1);
        dynamic.destroy(&made).expect("消せる");
        assert_eq!(dynamic.destroyed.lock().expect("読める").len(), 1);
        assert!(dynamic.list_targets().expect("数えられる").is_empty());
    }

    #[test]
    fn 種別と方式の対応() {
        assert_eq!(ProviderKind::Local.mode(), ProvisioningMode::Static);
        assert_eq!(ProviderKind::StaticSsh.mode(), ProvisioningMode::Static);
        assert_eq!(ProviderKind::Hetzner.mode(), ProvisioningMode::Dynamic);
        assert_eq!(ProviderKind::from_id("ssh"), Some(ProviderKind::StaticSsh));
        assert_eq!(ProviderKind::from_id("HETZNER"), Some(ProviderKind::Hetzner));
        assert_eq!(ProviderKind::from_id("aws"), None);
    }
}
