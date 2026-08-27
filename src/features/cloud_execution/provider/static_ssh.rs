//! **SSH さえあれば実行先になる** Provider (§52)。
//!
//! Contabo / OVH / netcup / Hostinger / Oracle / 会社のサーバー / 自宅の Linux —
//! API を持たない相手でも、`IP` と `SSH` と `Linux` の 3 つがあれば
//! Zaivern の実行先になる。**Provider 固有の実装は 1 行も要らない。**
//!
//! ## 何を持っていないか
//!
//! この Provider は台帳を持たない。実行先の一覧は
//! [`crate::features::cloud_execution::store`] が持ち、ここは
//! **「その中から自分のものを選ぶ」だけ**。台帳を 2 か所に持つと必ずずれる。

use crate::features::cloud_execution::model::{
    Capabilities, CloudError, ExecutionTarget, ProviderId, TargetCapacity, TargetEndpoint, TargetId,
    TargetLifecycle, TransportKind,
};
use crate::features::cloud_execution::store;
use crate::features::cloud_execution::transport::ssh::{validate_host, validate_user};

use super::{ExecutionProvider, ProvisionSpec, ProvisioningMode};

/// 利用者が登録した SSH 先。
pub struct StaticSshProvider {
    id: ProviderId,
}

impl StaticSshProvider {
    pub fn new(id: ProviderId) -> Self {
        Self { id }
    }

    /// 既定のプロファイル名 (`zai cloud target add ssh` が使う)。
    pub fn default_id() -> ProviderId {
        ProviderId::new("static-ssh")
    }
}

impl ExecutionProvider for StaticSshProvider {
    fn id(&self) -> ProviderId {
        self.id.clone()
    }

    fn mode(&self) -> ProvisioningMode {
        ProvisioningMode::Static
    }

    fn list_targets(&self) -> Result<Vec<ExecutionTarget>, CloudError> {
        Ok(store::load_targets()?
            .into_iter()
            .filter(|t| t.provider == self.id && t.transport == TransportKind::Ssh)
            .collect())
    }

    fn provision(&self, _spec: &ProvisionSpec) -> Result<ExecutionTarget, CloudError> {
        Err(CloudError::unsupported(
            "SSH の実行先は作れません。zai cloud target add ssh で登録してください",
        ))
    }

    fn destroy(&self, _target: &ExecutionTarget) -> Result<(), CloudError> {
        // **利用者の機械を消せる経路を作らない** (§22 / §51)。
        // 一覧から外すのは `zai cloud target remove` で、機械には触らない。
        Err(CloudError::security(
            "この実行先は Zaivern が作ったものではないので、消せません \
             (一覧から外すには zai cloud target remove を使ってください)",
        ))
    }
}

/// `zai cloud target add ssh` が渡してくる項目。
#[derive(Debug, Clone)]
pub struct SshTargetSpec {
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identity_file: Option<std::path::PathBuf>,
    pub max_jobs: u16,
    pub provider: ProviderId,
}

impl Default for SshTargetSpec {
    fn default() -> Self {
        Self {
            name: String::new(),
            host: String::new(),
            user: String::new(),
            port: 22,
            identity_file: None,
            max_jobs: 1,
            provider: StaticSshProvider::default_id(),
        }
    }
}

/// 登録する実行先を組む。**検査はここで済ませる** — 台帳へ入ってから
/// 「使えない host だった」と分かるのがいちばん遅い。
pub fn make_target(spec: &SshTargetSpec) -> Result<ExecutionTarget, CloudError> {
    if spec.name.trim().is_empty() {
        return Err(CloudError::config("--name を指定してください"));
    }
    // 名前は CLI で指す語になるので、`-` 始まりや空白を許さない
    if spec.name.starts_with('-')
        || spec
            .name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(CloudError::config(format!(
            "実行先の名前に使えない文字が入っています: {}",
            spec.name
        )));
    }
    validate_host(&spec.host)?;
    validate_user(&spec.user)?;
    if spec.port == 0 {
        return Err(CloudError::config("--port は 1 以上を指定してください"));
    }

    Ok(ExecutionTarget {
        // **ID は名前でも IP でもない** (§7)。名前を変えても同一性が続く。
        id: TargetId::new(crate::features::cloud_execution::model::ids::new_id("t-")),
        name: spec.name.clone(),
        provider: spec.provider.clone(),
        transport: TransportKind::Ssh,
        endpoint: TargetEndpoint::Ssh {
            host: spec.host.clone(),
            user: spec.user.clone(),
            port: spec.port,
            identity_file: spec.identity_file.clone(),
        },
        // **能力はまだ分からない。** 分からないものを推測で埋めない —
        // 埋めると `probe` する前から「16GB ある」ことになる。
        capabilities: Capabilities::default(),
        capacity: TargetCapacity::new(spec.max_jobs.max(1)),
        // **まだ Ready ではない。** 届くことを確かめる前に Scheduler へ
        // 渡すと、最初の仕事が必ず失敗する (§50)。
        lifecycle: TargetLifecycle::Unknown,
        managed: false,
        labels: Default::default(),
        billing: Default::default(),
        provider_ref: None,
        note: String::new(),
        generation: 0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::test_support::home_guard;

    fn spec() -> SshTargetSpec {
        SshTargetSpec {
            name: "dev-01".into(),
            host: "example.com".into(),
            user: "zaivern".into(),
            max_jobs: 4,
            ..SshTargetSpec::default()
        }
    }

    #[test]
    fn static_ssh_target_round_trip() {
        let _home = home_guard("static-ssh-round-trip");
        let t = make_target(&spec()).expect("組める");
        store::save_targets(std::slice::from_ref(&t)).expect("書ける");

        let p = StaticSshProvider::new(StaticSshProvider::default_id());
        let back = p.list_targets().expect("数えられる");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0], t, "書いたものがそのまま戻る");
        assert_eq!(back[0].capacity.max_jobs, 4);
        match &back[0].endpoint {
            TargetEndpoint::Ssh {
                host, user, port, ..
            } => {
                assert_eq!((host.as_str(), user.as_str(), *port), ("example.com", "zaivern", 22));
            }
            other => panic!("SSH ではない: {other:?}"),
        }
    }

    #[test]
    fn idは名前でもipでもない() {
        let a = make_target(&spec()).expect("組める");
        let b = make_target(&spec()).expect("組める");
        assert_ne!(a.id, b.id, "同じ指定でも ID は別");
        assert!(!a.id.as_str().contains("example.com"), "{}", a.id);
        assert!(!a.id.as_str().contains("dev-01"), "{}", a.id);
    }

    #[test]
    fn 登録の時点で危ない値を断る() {
        let mut s = spec();
        s.host = "-oProxyCommand=id".into();
        assert!(matches!(make_target(&s), Err(CloudError::Security(_))));

        let mut s = spec();
        s.host = "example.com; id".into();
        assert!(make_target(&s).is_err());

        let mut s = spec();
        s.name = "--force".into();
        assert!(matches!(make_target(&s), Err(CloudError::Config(_))));

        let mut s = spec();
        s.port = 0;
        assert!(make_target(&s).is_err());
    }

    #[test]
    fn 確かめる前は準備済みにしない() {
        let t = make_target(&spec()).expect("組める");
        assert_eq!(t.lifecycle, TargetLifecycle::Unknown);
        // 能力を推測で埋めない
        assert_eq!(t.capabilities, Capabilities::default());
    }

    #[test]
    fn 利用者の機械は消せない() {
        let p = StaticSshProvider::new(StaticSshProvider::default_id());
        let t = make_target(&spec()).expect("組める");
        let e = p.destroy(&t).expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
    }

    #[test]
    fn 自分の実行先だけを数える() {
        let _home = home_guard("static-ssh-scope");
        let mine = make_target(&spec()).expect("組める");
        let mut other = make_target(&SshTargetSpec {
            name: "other".into(),
            provider: ProviderId::new("hetzner-eu"),
            ..spec()
        })
        .expect("組める");
        other.provider = ProviderId::new("hetzner-eu");
        store::save_targets(&[mine.clone(), other]).expect("書ける");

        let p = StaticSshProvider::new(StaticSshProvider::default_id());
        let back = p.list_targets().expect("数えられる");
        assert_eq!(back, vec![mine]);
    }
}
