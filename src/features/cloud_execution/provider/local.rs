//! 手元の機械を 1 つの実行先として返す Provider。
//!
//! **これが在ることで、上位の層に「ローカルかどうか」の分岐が要らなくなる。**
//! 実行先が 0 件の環境でも `zai cloud target list` が 1 行返るので、
//! 「まだ何も設定していない」と「壊れている」を取り違えない。

use crate::features::cloud_execution::model::{CloudError, ExecutionTarget, ProviderId};

use super::{ExecutionProvider, ProvisionSpec, ProvisioningMode};

/// 手元の機械。
pub struct LocalProvider {
    max_jobs: u16,
}

impl LocalProvider {
    pub fn new(max_jobs: u16) -> Self {
        Self {
            max_jobs: max_jobs.max(1),
        }
    }
}

impl ExecutionProvider for LocalProvider {
    fn id(&self) -> ProviderId {
        ProviderId::new("local")
    }

    fn mode(&self) -> ProvisioningMode {
        ProvisioningMode::Static
    }

    fn list_targets(&self) -> Result<Vec<ExecutionTarget>, CloudError> {
        Ok(vec![ExecutionTarget::local(self.max_jobs)])
    }

    fn provision(&self, _spec: &ProvisionSpec) -> Result<ExecutionTarget, CloudError> {
        Err(CloudError::unsupported(
            "手元の機械は作れません (すでに在ります)",
        ))
    }

    fn destroy(&self, _target: &ExecutionTarget) -> Result<(), CloudError> {
        // **手元の機械を消せる経路を作らない。**
        Err(CloudError::security(
            "手元の機械は消せません",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::model::{OsFamily, TargetLifecycle, TransportKind};

    #[test]
    fn 手元は常に一件あって準備できている() {
        let p = LocalProvider::new(3);
        let t = p.list_targets().expect("数えられる");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].transport, TransportKind::Local);
        assert_eq!(t[0].lifecycle, TargetLifecycle::Ready);
        assert_eq!(t[0].capacity.max_jobs, 3);
        assert_ne!(t[0].capabilities.os, OsFamily::Unknown);
        // 手元は Zaivern が作ったものではない = 消す対象にならない
        assert!(!t[0].managed);
    }

    #[test]
    fn 枠は最低一本() {
        assert_eq!(
            LocalProvider::new(0).list_targets().expect("数えられる")[0]
                .capacity
                .max_jobs,
            1
        );
    }

    #[test]
    fn 手元は作れないし消せない() {
        let p = LocalProvider::new(1);
        assert!(matches!(
            p.provision(&ProvisionSpec::default()),
            Err(CloudError::Unsupported(_))
        ));
        assert!(matches!(
            p.destroy(&ExecutionTarget::local(1)),
            Err(CloudError::Security(_))
        ));
    }
}
