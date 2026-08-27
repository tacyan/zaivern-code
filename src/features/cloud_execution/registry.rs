//! **実行先の台帳** — 誰が居て、いま何本走っているか。
//!
//! ## 台帳を 1 つにする
//!
//! 「実行先の一覧」を Provider ごとに持つと必ずずれる。ここが唯一の持ち主で、
//! Provider へ問い合わせた結果も**この台帳へ畳んでから**使う。
//!
//! ## 枠の取り合い
//!
//! `active_jobs` は読んで足して書くので、素朴にやると 2 つのインスタンスが
//! 同時に最後の 1 枠を取る。[`claim_slot`] は
//! [`store::with_targets`] のロックの中で確かめてから足すので、
//! **上限を超えて載らない** ([`tests::枠は上限を超えて取れない`])。
//!
//! ## 何を Ready と呼ぶか
//!
//! [`TargetLifecycle::Ready`] を名乗れるのは、**実際に接続を確かめたときだけ**
//! (§50)。Provider が「running」と言っていても、SSH がまだ開いていなければ
//! 仕事は必ず失敗する。

use std::time::Duration;

use super::model::{CloudError, ExecutionTarget, ProbeResult, TargetId, TargetLifecycle};
use super::provider::{
    self, ExecutionProvider, ProviderCtx, ProviderKind, ProviderProfile, ProvisionSpec,
};
use super::store;
use super::transport::{self, ExecutionTransport};

/// 台帳。
pub struct Registry {
    profiles: Vec<ProviderProfile>,
    ctx: ProviderCtx,
    ssh_timeout: Duration,
}

impl Registry {
    /// 設定から組む。
    pub fn load(cfg: &crate::config::Config) -> Result<Self, CloudError> {
        let ssh_timeout = super::ssh_timeout(cfg);
        Ok(Self {
            profiles: store::load_providers()?,
            ctx: ProviderCtx::live(super::api_timeout(cfg), super::default_max_jobs(cfg)),
            ssh_timeout,
        })
    }

    /// HTTP を差し替えて組む (試験用)。
    pub fn with_ctx(
        profiles: Vec<ProviderProfile>,
        ctx: ProviderCtx,
        ssh_timeout: Duration,
    ) -> Self {
        Self {
            profiles,
            ctx,
            ssh_timeout,
        }
    }

    pub fn profiles(&self) -> &[ProviderProfile] {
        &self.profiles
    }

    /// 名前で Provider プロファイルを引く。
    pub fn profile(&self, name: &str) -> Result<&ProviderProfile, CloudError> {
        self.profiles
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| {
                CloudError::config(format!(
                    "Provider {name} は登録されていません (zai cloud provider list で確認できます)"
                ))
            })
    }

    /// 名前の Provider を組む。
    pub fn provider(&self, name: &str) -> Result<Box<dyn ExecutionProvider>, CloudError> {
        provider::build(self.profile(name)?, &self.ctx)
    }

    /// **台帳に載っている実行先 + 常に在るローカル**。
    ///
    /// Provider API は呼ばない (一覧を出すたびにネットワークを叩くと、
    /// 電波の無いところで `zai cloud target list` が固まる)。
    /// Provider へ問い合わせたいときは [`Registry::refresh_from`] を明示的に呼ぶ。
    pub fn targets(&self) -> Result<Vec<ExecutionTarget>, CloudError> {
        let mut out = vec![ExecutionTarget::local(self.ctx.local_max_jobs)];
        out.extend(store::load_targets()?);
        Ok(out)
    }

    /// 名前か ID で実行先を引く。
    pub fn find(&self, name_or_id: &str) -> Result<ExecutionTarget, CloudError> {
        let all = self.targets()?;
        let hit: Vec<&ExecutionTarget> = all
            .iter()
            .filter(|t| t.name == name_or_id || t.id.as_str() == name_or_id)
            .collect();
        match hit.len() {
            1 => Ok(hit[0].clone()),
            0 => Err(CloudError::config(format!(
                "実行先 {name_or_id} が見つかりません (zai cloud target list で確認できます)"
            ))),
            // **後勝ちにしない。** 同じ名前が 2 つあるなら、どちらへ載せるかは
            // 利用者に決めてもらう (黙って片方を選ぶと、片方が永久に死ぬ)
            n => Err(CloudError::config(format!(
                "{name_or_id} に {n} 件が一致します。ID で指定してください"
            ))),
        }
    }

    /// 実行先を 1 件足す。**同じ名前は拒否する** (後勝ちにしない)。
    pub fn add_target(&self, target: ExecutionTarget) -> Result<(), CloudError> {
        store::with_targets(|list| {
            if list.iter().any(|t| t.name == target.name) {
                return Err(CloudError::config(format!(
                    "実行先 {} はすでに登録されています",
                    target.name
                )));
            }
            list.push(target);
            Ok(())
        })?
    }

    /// 一覧から外す。**機械には触らない** (消すのは `worker destroy`)。
    pub fn remove_target(&self, name_or_id: &str) -> Result<ExecutionTarget, CloudError> {
        let t = self.find(name_or_id)?;
        if t.transport == super::model::TransportKind::Local {
            return Err(CloudError::config(
                "手元の機械は一覧から外せません",
            ));
        }
        store::with_targets(|list| list.retain(|x| x.id != t.id))?;
        Ok(t)
    }

    /// 実行先を確かめて、分かったことを台帳へ書き戻す。
    ///
    /// **ここだけが [`TargetLifecycle::Ready`] を付ける。**
    pub fn probe(&self, name_or_id: &str) -> Result<(ExecutionTarget, ProbeResult), CloudError> {
        let target = self.find(name_or_id)?;
        let tr = transport::for_target(&target, self.ssh_timeout);
        let probe = tr.probe(&target)?;
        let updated = apply_probe(&target, &probe);
        // 手元の機械は台帳に載っていないので書き戻さない
        if updated.transport != super::model::TransportKind::Local {
            store::with_targets(|list| {
                if let Some(slot) = list.iter_mut().find(|t| t.id == updated.id) {
                    // **枠の使用中の数は上書きしない。** 探りの結果で
                    // `active_jobs` を 0 に戻すと、走っている仕事が消える。
                    let active = slot.capacity.active_jobs;
                    *slot = updated.clone();
                    slot.capacity.active_jobs = active;
                }
            })?;
        }
        Ok((updated, probe))
    }

    /// Provider へ問い合わせて、台帳へ足りない実行先を取り込む。
    ///
    /// **Ready にはしない** — 取り込んだ直後は接続を確かめていないため。
    pub fn refresh_from(&self, provider_name: &str) -> Result<Vec<ExecutionTarget>, CloudError> {
        let p = self.provider(provider_name)?;
        let found = p.list_targets().map_err(|e| {
            // どの Provider の話かを必ず添える (プロファイルが複数あると分からない)
            CloudError::config(format!("{} から取り込めません: {e}", p.id()))
        })?;
        let mut added = Vec::new();
        store::with_targets(|list| {
            for t in &found {
                match list.iter_mut().find(|x| x.id == t.id) {
                    Some(slot) => {
                        // 接続情報と能力は Provider が正しい。枠の使用中は手元が正しい
                        let active = slot.capacity.active_jobs;
                        let lifecycle = slot.lifecycle;
                        *slot = t.clone();
                        slot.capacity.active_jobs = active;
                        // 一度 Ready と分かっているものを降格させない
                        if lifecycle == TargetLifecycle::Ready {
                            slot.lifecycle = TargetLifecycle::Ready;
                        }
                    }
                    None => {
                        list.push(t.clone());
                        added.push(t.clone());
                    }
                }
            }
        })?;
        Ok(added)
    }

    /// 有料の VM を作る。**呼ぶのは明示操作のときだけ** (§33)。
    pub fn provision(
        &self,
        provider_name: &str,
        spec: &ProvisionSpec,
    ) -> Result<ExecutionTarget, CloudError> {
        let profile = self.profile(provider_name)?;
        let p = provider::build(profile, &self.ctx)?;
        // **作れるかどうかは Provider 自身に聞く。** プロファイルの種別から
        // 推測すると、種別と実装がずれた日に「作れるはずが作れない」になる。
        if p.mode() != provider::ProvisioningMode::Dynamic {
            return Err(CloudError::unsupported(format!(
                "{provider_name} は実行先を作れません (登録済みのものを使ってください)"
            )));
        }
        let target = p.provision(spec)?;
        self.add_target(target.clone())?;
        Ok(target)
    }

    /// VM を消す。**Zaivern が作ったものだけ** (§22)。
    pub fn destroy(&self, name_or_id: &str) -> Result<ExecutionTarget, CloudError> {
        let target = self.find(name_or_id)?;
        if !target.managed {
            return Err(CloudError::security(format!(
                "{name_or_id} は Zaivern が作った実行先ではないので消しません。\n\
                 一覧から外すだけなら zai cloud target remove を使ってください"
            )));
        }
        if target.capacity.active_jobs > 0 {
            return Err(CloudError::config(format!(
                "{name_or_id} ではまだ {} 本の仕事が走っています",
                target.capacity.active_jobs
            )));
        }
        let p = self.provider(target.provider.as_str())?;
        p.destroy(&target)?;
        store::with_targets(|list| list.retain(|x| x.id != target.id))?;
        Ok(target)
    }

    /// 作りたての実行先が**SSH で入れるようになるまで**待つ (§50)。
    ///
    /// * まだ誰も鍵を知らないので、**初回だけ** `accept-new` を許す (§15)。
    ///   一度覚えたら以後は strict — 作り直した機械は known_hosts の行を
    ///   消すまで断られる (それが正しい: 中間者と区別が付かない)。
    /// * 上限に当たったら諦める。**永久には待たない** (§48)。
    pub fn wait_ready(&self, id: &TargetId) -> Result<ExecutionTarget, CloudError> {
        use super::provider::hetzner::READY_TIMEOUT;
        let deadline = std::time::Instant::now() + READY_TIMEOUT;
        let mut wait = Duration::from_secs(5);
        loop {
            let target = self
                .targets()?
                .into_iter()
                .find(|t| &t.id == id)
                .ok_or_else(|| CloudError::config(format!("実行先 {id} が見つかりません")))?;
            let tr = super::transport::ssh::SshTransport::new(self.ssh_timeout)
                .with_host_key(super::transport::ssh::HostKeyPolicy::AcceptNew);
            let probe = tr.probe(&target)?;
            if probe.reachable {
                let updated = apply_probe(&target, &probe);
                store::with_targets(|list| {
                    if let Some(slot) = list.iter_mut().find(|t| t.id == updated.id) {
                        let active = slot.capacity.active_jobs;
                        *slot = updated.clone();
                        slot.capacity.active_jobs = active;
                    }
                })?;
                return Ok(updated);
            }
            if std::time::Instant::now() >= deadline {
                return Err(CloudError::timeout(format!(
                    "{} の SSH が {} 秒で開きませんでした\n{}",
                    target.name,
                    READY_TIMEOUT.as_secs(),
                    probe.error
                )));
            }
            std::thread::sleep(wait);
            wait = (wait * 2).min(Duration::from_secs(20));
        }
    }

    pub fn ssh_timeout(&self) -> Duration {
        self.ssh_timeout
    }
}

/// 探りの結果を実行先へ当てはめる。**純関数**なので表で固定できる。
pub fn apply_probe(target: &ExecutionTarget, probe: &ProbeResult) -> ExecutionTarget {
    let mut t = target.clone();
    if probe.reachable {
        t.lifecycle = TargetLifecycle::Ready;
        // **能力は探りの結果で置き換える。** 登録時の指定より、実際に
        // その機械が答えたことのほうが確か。
        let labels = std::mem::take(&mut t.capabilities.labels);
        t.capabilities = probe.capabilities.clone();
        t.capabilities.labels = labels;
        t.note = format!("{} / {}", probe.kernel, probe.shell);
    } else {
        t.lifecycle = TargetLifecycle::Failed;
        t.note = probe.error.lines().next().unwrap_or_default().to_string();
    }
    t
}

/// 枠を 1 つ取る。**上限を超えたら [`CloudError::NoCapacity`]** (§31)。
///
/// ロックの中で確かめてから足すので、同時に呼ばれても上限を超えない。
pub fn claim_slot(id: &TargetId) -> Result<(), CloudError> {
    store::with_targets(|list| {
        let Some(t) = list.iter_mut().find(|t| &t.id == id) else {
            return Err(CloudError::config(format!("実行先 {id} が見つかりません")));
        };
        if !t.capacity.has_room() {
            return Err(CloudError::no_capacity(format!(
                "{} はすでに {} 本を実行中です (max_jobs = {})",
                t.name, t.capacity.active_jobs, t.capacity.max_jobs
            )));
        }
        t.capacity.active_jobs += 1;
        Ok(())
    })?
}

/// 枠を返す。**足りなくならないよう飽和で引く。**
pub fn release_slot(id: &TargetId) -> Result<(), CloudError> {
    store::with_targets(|list| {
        if let Some(t) = list.iter_mut().find(|t| &t.id == id) {
            t.capacity.active_jobs = t.capacity.active_jobs.saturating_sub(1);
        }
    })
}

/// 枠の増減を、途中で失敗しても必ず返す形で包む。
pub struct SlotGuard {
    id: TargetId,
    active: bool,
}

impl SlotGuard {
    /// 取れたら番人を返す。取れなければ [`CloudError::NoCapacity`]。
    pub fn claim(id: &TargetId) -> Result<Self, CloudError> {
        claim_slot(id)?;
        Ok(Self {
            id: id.clone(),
            active: true,
        })
    }

    /// ローカル (台帳に載っていない実行先) 用の、何もしない番人。
    pub fn none(id: &TargetId) -> Self {
        Self {
            id: id.clone(),
            active: false,
        }
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.active {
            // **失敗しても黙って落とす。** ここで panic すると、
            // 仕事の失敗が「後始末の失敗」に化けて原因が見えなくなる。
            let _ = release_slot(&self.id);
        }
    }
}

/// 既定の Provider プロファイル (`local` と `static-ssh`) を用意する。
///
/// **利用者が何も設定していなくても動く**ようにするためで、保存もしない
/// (保存すると「設定した覚えのないもの」が providers.json に増える)。
pub fn builtin_profiles() -> Vec<ProviderProfile> {
    vec![
        ProviderProfile {
            name: "local".into(),
            kind: ProviderKind::Local,
            ..ProviderProfile::default()
        },
        ProviderProfile {
            name: "static-ssh".into(),
            kind: ProviderKind::StaticSsh,
            ..ProviderProfile::default()
        },
    ]
}

/// 台帳に載っている Provider と、常にある組み込みを合わせる。
pub fn all_profiles(stored: &[ProviderProfile]) -> Vec<ProviderProfile> {
    let mut out = builtin_profiles();
    for p in stored {
        // 同じ名前が登録されていたら、利用者の指定を優先する
        if let Some(slot) = out.iter_mut().find(|x| x.name == p.name) {
            *slot = p.clone();
        } else {
            out.push(p.clone());
        }
    }
    out
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::model::{Capabilities, OsFamily};
    use crate::features::cloud_execution::provider::static_ssh::{make_target, SshTargetSpec};
    use crate::features::cloud_execution::test_support::{home_guard, target, TargetOpts};

    fn registry() -> Registry {
        Registry::with_ctx(
            all_profiles(&[]),
            ProviderCtx {
                http: std::sync::Arc::new(
                    crate::features::cloud_execution::test_support::FakeHttpClient::new(vec![]),
                ),
                timeout: Duration::from_secs(5),
                local_max_jobs: 2,
            },
            Duration::from_secs(5),
        )
    }

    fn add(reg: &Registry, name: &str, max_jobs: u16) -> ExecutionTarget {
        let t = make_target(&SshTargetSpec {
            name: name.into(),
            host: "example.com".into(),
            user: "zaivern".into(),
            max_jobs,
            ..SshTargetSpec::default()
        })
        .expect("組める");
        reg.add_target(t.clone()).expect("足せる");
        t
    }

    #[test]
    fn 手元は設定しなくても一覧に出る() {
        let _home = home_guard("registry-local");
        let reg = registry();
        let all = reg.targets().expect("数えられる");
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].name, "local");
        // 「まだ何も設定していない」と「壊れている」を取り違えない
        assert_eq!(all[0].lifecycle, TargetLifecycle::Ready);
    }

    #[test]
    fn 同じ名前は後勝ちにしない() {
        let _home = home_guard("registry-dup");
        let reg = registry();
        add(&reg, "dev-01", 2);
        let again = make_target(&SshTargetSpec {
            name: "dev-01".into(),
            host: "other.example".into(),
            user: "zaivern".into(),
            ..SshTargetSpec::default()
        })
        .expect("組める");
        assert!(reg.add_target(again).is_err(), "上書きしてしまった");
        assert_eq!(reg.targets().expect("数えられる").len(), 2);
    }

    #[test]
    fn 枠は上限を超えて取れない() {
        let _home = home_guard("registry-slots");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        let g1 = SlotGuard::claim(&t.id).expect("1 本目");
        let g2 = SlotGuard::claim(&t.id).expect("2 本目");
        let e = match SlotGuard::claim(&t.id) {
            Ok(_) => panic!("3 本目を通してしまった"),
            Err(e) => e,
        };
        assert!(matches!(e, CloudError::NoCapacity(_)), "{e:?}");
        assert_eq!(e.exit_code(), 4, "空きが無いときの終了コード");

        // 返せばまた取れる
        drop(g1);
        let _g3 = SlotGuard::claim(&t.id).expect("返した分は取れる");
        drop(g2);
        // 台帳の数と実際の番人の数が合っている
        let after = reg.find("dev-01").expect("引ける");
        assert_eq!(after.capacity.active_jobs, 1);
    }

    #[test]
    fn 枠は同時に取り合っても上限を超えない() {
        let _home = home_guard("registry-slots-race");
        let reg = registry();
        let t = add(&reg, "dev-01", 4);
        // **8 本が同時に取りに行く。** 通ってよいのは 4 本だけ。
        let id = t.id.clone();
        let dir = store::cloud_dir();
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let id = id.clone();
                let dir = dir.clone();
                std::thread::spawn(move || {
                    // 置き場の差し替えはスレッドごとなので、子でも指し直す
                    store::set_test_dir(Some(dir));
                    claim_slot(&id).is_ok()
                })
            })
            .collect();
        let granted = handles
            .into_iter()
            .filter_map(|h| h.join().ok())
            .filter(|ok| *ok)
            .count();
        assert_eq!(granted, 4, "上限を超えて配った");
        assert_eq!(
            reg.find("dev-01").expect("引ける").capacity.active_jobs,
            4
        );
    }

    #[test]
    fn 探りの結果だけがreadyを付ける() {
        let t = target("dev-01", TargetOpts {
            lifecycle: TargetLifecycle::Unknown,
            cpu_cores: None,
            memory_mib: None,
            ..TargetOpts::default()
        });
        let probe = ProbeResult {
            reachable: true,
            latency_ms: 12,
            capabilities: Capabilities {
                os: OsFamily::Linux,
                arch: crate::features::cloud_execution::model::Architecture::Aarch64,
                cpu_cores: Some(8),
                memory_mib: Some(16000),
                ..Capabilities::default()
            },
            shell: "/bin/bash".into(),
            kernel: "6.8.0".into(),
            error: String::new(),
        };
        let after = apply_probe(&t, &probe);
        assert_eq!(after.lifecycle, TargetLifecycle::Ready);
        assert_eq!(after.capabilities.cpu_cores, Some(8));
        assert!(after.note.contains("6.8.0"));

        // 届かなければ Failed。**理由を残す**
        let bad = ProbeResult {
            reachable: false,
            error: "接続できませんでした\nssh: connect to host".into(),
            ..ProbeResult::default()
        };
        let after = apply_probe(&t, &bad);
        assert_eq!(after.lifecycle, TargetLifecycle::Failed);
        assert_eq!(after.note, "接続できませんでした");
    }

    #[test]
    fn 走っている仕事の数を探りで消さない() {
        let mut t = target("dev-01", TargetOpts {
            active_jobs: 2,
            ..TargetOpts::default()
        });
        t.capacity.active_jobs = 2;
        let probe = ProbeResult {
            reachable: true,
            ..ProbeResult::default()
        };
        // apply_probe 自身は capacity を触らない
        assert_eq!(apply_probe(&t, &probe).capacity.active_jobs, 2);
    }

    #[test]
    fn 一覧から外すのと機械を消すのは別() {
        let _home = home_guard("registry-remove");
        let reg = registry();
        let t = add(&reg, "dev-01", 1);
        // Zaivern が作っていないものは destroy できない
        let e = reg.destroy("dev-01").expect_err("断る");
        assert!(matches!(e, CloudError::Security(_)), "{e:?}");
        // 外すだけなら通る
        let removed = reg.remove_target("dev-01").expect("外せる");
        assert_eq!(removed.id, t.id);
        assert_eq!(reg.targets().expect("数えられる").len(), 1);
    }

    #[test]
    fn 手元は外せない() {
        let _home = home_guard("registry-remove-local");
        let reg = registry();
        assert!(reg.remove_target("local").is_err());
    }

    #[test]
    fn 仕事が走っている実行先は消さない() {
        let _home = home_guard("registry-destroy-busy");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        store::with_targets(|list| {
            let slot = list.iter_mut().find(|x| x.id == t.id).expect("居る");
            slot.managed = true;
            slot.capacity.active_jobs = 1;
        })
        .expect("書ける");
        let e = reg.destroy("dev-01").expect_err("断る");
        assert!(format!("{e}").contains("1 本"), "{e}");
    }

    #[test]
    fn 組み込みのプロファイルは常にある() {
        let all = all_profiles(&[]);
        assert!(all.iter().any(|p| p.name == "local"));
        assert!(all.iter().any(|p| p.name == "static-ssh"));
        // 同じ名前を登録したら利用者の指定が勝つ
        let mine = ProviderProfile {
            name: "static-ssh".into(),
            kind: ProviderKind::StaticSsh,
            ssh_user: "me".into(),
            ..ProviderProfile::default()
        };
        let all = all_profiles(&[mine]);
        assert_eq!(all.iter().filter(|p| p.name == "static-ssh").count(), 1);
        assert_eq!(
            all.iter()
                .find(|p| p.name == "static-ssh")
                .expect("居る")
                .ssh_user,
            "me"
        );
    }

    #[test]
    fn 静的なproviderは作れない() {
        let _home = home_guard("registry-static-provision");
        let reg = registry();
        let e = reg
            .provision("static-ssh", &ProvisionSpec::default())
            .expect_err("断る");
        assert!(matches!(e, CloudError::Unsupported(_)), "{e:?}");
    }

    #[test]
    fn 知らないproviderは設定の誤りとして返る() {
        let _home = home_guard("registry-unknown-provider");
        let reg = registry();
        let e = match reg.provider("aws") {
            Ok(_) => panic!("知らない Provider を組んでしまった"),
            Err(e) => e,
        };
        assert_eq!(e.exit_code(), 3);
    }
}
