//! **どの実行先で走らせるかを決める、純関数だけの層** (§32)。
//!
//! ## 純関数であること
//!
//! [`select_target`] は Provider API を 1 度も呼ばない。呼ぶと
//!
//! * 同じ入力で同じ結果を返さなくなる (試験が書けない)
//! * 選ぶだけの操作がネットワークの都合で数秒止まる
//! * Provider が 1 つ増えるたびに Scheduler が増える
//!
//! の 3 つが同時に起きる。**渡された一覧から選ぶだけ**にしておくと、
//! Provider をいくつ足しても Scheduler は 1 行も変わらない (§71 の成功条件)。
//!
//! ## 選ぶ順序
//!
//! 1. [`TargetLifecycle::Ready`] であること
//! 2. [`ExecutionRequirements`] を満たすこと
//! 3. 空き枠があること (`active_jobs < max_jobs`)
//! 4. 利用者が名指しした実行先
//! 5. ローカル / リモートの好み
//! 6. 費用の目安が安いほう
//! 7. **同点なら ID 順** — ここまで来ても必ず 1 つに決まる (決定性の要)

use super::model::{Capabilities, ExecutionRequirements, ExecutionTarget, TargetId, TargetLifecycle};

/// ローカルとリモートのどちらを先に見るか。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Prefer {
    /// 手元を先に見る (既定。**お金がかからない側から**)。
    #[default]
    Local,
    /// リモートを先に見る (手元を空けておきたいとき)。
    Remote,
    /// 区別しない。
    Any,
}

impl Prefer {
    pub fn from_id(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "remote" => Self::Remote,
            "any" => Self::Any,
            _ => Self::Local,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::Remote => "remote",
            Self::Any => "any",
        }
    }

    /// 小さいほど先。
    fn rank(self, remote: bool) -> u8 {
        match (self, remote) {
            (Self::Any, _) => 0,
            (Self::Local, false) | (Self::Remote, true) => 0,
            _ => 1,
        }
    }
}

/// なぜ選べなかったのか。**「空きが無い」と「能力が足りない」を混ぜない** —
/// 混ぜると利用者が「VM を増やせばいい」のか「大きいのが要る」のか分からない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reject {
    NotReady,
    /// 破棄を予約した / 結果が分からない実行先。**「未確認」と混ぜない** —
    /// 利用者が次に打つ手が違う (probe で確かめ直すか、台帳から外すか)。
    Destroying,
    Os,
    Arch,
    Cpu,
    Memory,
    Gpu,
    Tools,
    Labels,
    Full,
    NotPreferred,
}

impl Reject {
    pub fn id(self) -> &'static str {
        match self {
            Self::NotReady => "not-ready",
            Self::Destroying => "destroying",
            Self::Os => "os",
            Self::Arch => "arch",
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Gpu => "gpu",
            Self::Tools => "tools",
            Self::Labels => "labels",
            Self::Full => "full",
            Self::NotPreferred => "not-preferred",
        }
    }
}

/// 1 つの実行先が要求を満たすか。満たさないなら**最初に引っかかった理由**を返す。
pub fn evaluate(
    req: &ExecutionRequirements,
    t: &ExecutionTarget,
) -> Result<(), Reject> {
    if t.lifecycle == TargetLifecycle::Destroying {
        return Err(Reject::Destroying);
    }
    if t.lifecycle != TargetLifecycle::Ready {
        return Err(Reject::NotReady);
    }
    if let Some(id) = &req.preferred {
        if &t.id != id && t.name != id.as_str() {
            return Err(Reject::NotPreferred);
        }
    }
    fits(req, &t.capabilities)?;
    if !t.capacity.has_room() {
        return Err(Reject::Full);
    }
    Ok(())
}

/// 能力だけを見る (空きは見ない)。`probe` の結果を当てはめるときにも使う。
pub fn fits(req: &ExecutionRequirements, cap: &Capabilities) -> Result<(), Reject> {
    if let Some(os) = req.os {
        // **`Unknown` は「合う」に数えない。** 数えると、確かめていない実行先が
        // 何にでも当てはまることになる。
        if cap.os != os {
            return Err(Reject::Os);
        }
    }
    if let Some(arch) = req.arch {
        if cap.arch != arch {
            return Err(Reject::Arch);
        }
    }
    if let Some(min) = req.min_cpu_cores {
        // 分からない (None) 実行先は、要求があるなら通さない (fail closed)
        if cap.cpu_cores.unwrap_or(0) < min {
            return Err(Reject::Cpu);
        }
    }
    if let Some(min) = req.min_memory_mib {
        if cap.memory_mib.unwrap_or(0) < min {
            return Err(Reject::Memory);
        }
    }
    if req.requires_gpu && cap.gpu.is_empty() {
        return Err(Reject::Gpu);
    }
    if !req.required_tools.is_subset(&cap.tools) {
        return Err(Reject::Tools);
    }
    if !req.labels.is_subset(&cap.labels) {
        return Err(Reject::Labels);
    }
    Ok(())
}

/// 並べ替えの鍵。**同じ入力なら必ず同じ順**になるよう、最後に ID を置く。
fn sort_key(prefer: Prefer, t: &ExecutionTarget) -> (u8, std::cmp::Reverse<u16>, u64, String) {
    (
        prefer.rank(t.is_remote()),
        // 空きが多いほうが先 (仕事を 1 台へ寄せない)
        std::cmp::Reverse(t.capacity.free()),
        t.billing.cost_hint(),
        t.id.as_str().to_string(),
    )
}

/// 要求を満たす実行先を 1 つ選ぶ。**同じ入力なら必ず同じ結果**。
///
/// 好み ([`Prefer`]) は [`ExecutionRequirements::prefer`] が持つ —
/// 引数で別に受けると「要求」が 2 か所に割れて、片方だけ渡し忘れる。
pub fn select_target(
    requirements: &ExecutionRequirements,
    targets: &[ExecutionTarget],
) -> Option<TargetId> {
    targets
        .iter()
        .filter(|t| evaluate(requirements, t).is_ok())
        .min_by_key(|t| sort_key(requirements.prefer, t))
        .map(|t| t.id.clone())
}

/// 選べなかったとき、**なぜ**選べなかったのかを実行先ごとに返す。
///
/// 「空きがありません」だけを出すと、利用者は VM を増やして直そうとする。
/// 実際には RAM が足りなかった、という取り違えを防ぐために要る。
pub fn explain(
    requirements: &ExecutionRequirements,
    targets: &[ExecutionTarget],
) -> Vec<(TargetId, Reject)> {
    let mut out: Vec<(TargetId, Reject)> = targets
        .iter()
        .filter_map(|t| evaluate(requirements, t).err().map(|r| (t.id.clone(), r)))
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// `--target <名前>` を要求へ写す。`auto` は名指し無しと同じ。
pub fn requirements_for_target(name: &str) -> ExecutionRequirements {
    let mut req = ExecutionRequirements::default();
    let name = name.trim();
    if !name.is_empty() && name != "auto" {
        req.preferred = Some(TargetId::new(name));
    }
    req
}


#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::cloud_execution::test_support::{target, TargetOpts};

    #[test]
    fn scheduler_same_input_same_target() {
        let targets = vec![
            target("b", TargetOpts::default()),
            target("a", TargetOpts::default()),
            target("c", TargetOpts::default()),
        ];
        let req = ExecutionRequirements::default();
        let first = select_target(&req, &targets).expect("選べる");
        // 100 回まわしても、順序を入れ替えても同じ答え
        for _ in 0..100 {
            assert_eq!(select_target(&req, &targets), Some(first.clone()));
        }
        let mut shuffled = targets.clone();
        shuffled.reverse();
        assert_eq!(select_target(&req, &shuffled), Some(first.clone()));
        // 同点は ID 順で決まる (試験用の実行先の ID は `id-<名前>`)
        assert_eq!(first.as_str(), "id-a");
    }

    #[test]
    fn scheduler_rejects_insufficient_memory() {
        let small = target(
            "small",
            TargetOpts {
                memory_mib: Some(1024),
                ..TargetOpts::default()
            },
        );
        let big = target(
            "big",
            TargetOpts {
                memory_mib: Some(16384),
                ..TargetOpts::default()
            },
        );
        let req = ExecutionRequirements {
            min_memory_mib: Some(8192),
            ..ExecutionRequirements::default()
        };
        assert_eq!(evaluate(&req, &small), Err(Reject::Memory));
        assert_eq!(
            select_target(&req, &[small.clone(), big.clone()]),
            Some(big.id.clone())
        );
        // 理由が「空きが無い」に化けていないこと
        let why = explain(&req, &[small]);
        assert_eq!(why.len(), 1);
        assert_eq!(why[0].1, Reject::Memory);
    }

    #[test]
    fn scheduler_rejects_full_target() {
        let full = target(
            "full",
            TargetOpts {
                max_jobs: 2,
                active_jobs: 2,
                ..TargetOpts::default()
            },
        );
        let free = target(
            "free",
            TargetOpts {
                max_jobs: 2,
                active_jobs: 1,
                ..TargetOpts::default()
            },
        );
        let req = ExecutionRequirements::default();
        assert_eq!(evaluate(&req, &full), Err(Reject::Full));
        assert_eq!(
            select_target(&req, &[full, free.clone()]),
            Some(free.id.clone())
        );
    }

    #[test]
    fn 準備できていない実行先は選ばない() {
        let mut t = target("a", TargetOpts::default());
        for state in [
            TargetLifecycle::Unknown,
            TargetLifecycle::Provisioning,
            TargetLifecycle::Draining,
            TargetLifecycle::Stopped,
            TargetLifecycle::Failed,
        ] {
            t.lifecycle = state;
            assert_eq!(
                evaluate(&ExecutionRequirements::default(), &t),
                Err(Reject::NotReady),
                "{} を選んでしまった",
                state.id()
            );
            assert_eq!(select_target(&ExecutionRequirements::default(), std::slice::from_ref(&t)), None);
        }
    }

    /// **削除中は「未確認」と別の理由で断る。** 次に打つ手が違うので、
    /// 同じ文面にすると利用者が probe を繰り返すことになる。
    #[test]
    fn 削除中は専用の理由で断る() {
        let mut t = target("a", TargetOpts::default());
        t.lifecycle = TargetLifecycle::Destroying;
        let req = ExecutionRequirements::default();
        assert_eq!(evaluate(&req, &t), Err(Reject::Destroying));
        assert_eq!(select_target(&req, std::slice::from_ref(&t)), None);
        assert_eq!(explain(&req, &[t])[0].1.id(), "destroying");
    }

    #[test]
    fn 空き枠の多いほうへ寄せる() {
        // 1 台へ全部寄せると、そこが詰まったとき全部止まる
        let a = target(
            "a",
            TargetOpts {
                max_jobs: 4,
                active_jobs: 3,
                ..TargetOpts::default()
            },
        );
        let b = target(
            "b",
            TargetOpts {
                max_jobs: 4,
                ..TargetOpts::default()
            },
        );
        assert_eq!(select_target(&ExecutionRequirements::default(), &[a, b.clone()]), Some(b.id));
    }

    #[test]
    fn 名指しした実行先は名前でも引ける() {
        let t = target("dev-01", TargetOpts::default());
        let req = requirements_for_target("dev-01");
        // ID ではなく利用者が付けた名前で引ける
        assert!(evaluate(&req, &t).is_ok());
        assert_eq!(select_target(&req, &[t]), Some(TargetId::new("id-dev-01")));
        // auto は名指し無しと同じ
        assert!(requirements_for_target("auto").preferred.is_none());
        assert!(requirements_for_target("").preferred.is_none());
    }

    #[test]
    fn 名指ししても能力と空きは必ず見る() {
        // 「名指しすれば通る」にすると、満杯の機械へ 5 本目を載せてしまう
        let t = target(
            "dev-01",
            TargetOpts {
                max_jobs: 1,
                active_jobs: 1,
                ..TargetOpts::default()
            },
        );
        let req = requirements_for_target("dev-01");
        assert_eq!(evaluate(&req, &t), Err(Reject::Full));
    }

    #[test]
    fn 好みは能力の次にしか効かない() {
        let local = ExecutionTarget::local(2);
        let remote = target("r", TargetOpts::default());
        // 好みだけを変えると選ぶ先が変わる
        let want = |prefer| ExecutionRequirements {
            prefer,
            ..ExecutionRequirements::default()
        };
        assert_eq!(
            select_target(&want(Prefer::Local), &[local.clone(), remote.clone()]),
            Some(local.id.clone())
        );
        assert_eq!(
            select_target(&want(Prefer::Remote), &[local.clone(), remote.clone()]),
            Some(remote.id.clone())
        );
        // ただし能力が足りなければ好みは効かない
        let req = ExecutionRequirements {
            min_memory_mib: Some(1_000_000),
            prefer: Prefer::Remote,
            ..ExecutionRequirements::default()
        };
        assert_eq!(select_target(&req, &[local, remote]), None);
    }

    #[test]
    fn 道具と札は部分集合で見る() {
        let mut t = target("a", TargetOpts::default());
        t.capabilities.tools.insert("git".into());
        t.capabilities.labels.insert("eu".into());
        let mut req = ExecutionRequirements::default();
        req.required_tools.insert("git".into());
        assert!(evaluate(&req, &t).is_ok());
        req.required_tools.insert("docker".into());
        assert_eq!(evaluate(&req, &t), Err(Reject::Tools));
    }

    #[test]
    fn 分からない能力は要求があれば通さない() {
        // fail closed。「分からない = 何でもできる」にすると必ず落ちる
        let t = target(
            "unknown",
            TargetOpts {
                memory_mib: None,
                cpu_cores: None,
                ..TargetOpts::default()
            },
        );
        let req = ExecutionRequirements {
            min_cpu_cores: Some(2),
            ..ExecutionRequirements::default()
        };
        assert_eq!(evaluate(&req, &t), Err(Reject::Cpu));
        // 要求が無ければ通る
        assert!(evaluate(&ExecutionRequirements::default(), &t).is_ok());
    }

    /// **Scheduler が Provider を呼んでいないことを、ソースの走査で固定する。**
    #[test]
    fn schedulerはproviderを呼ばない() {
        let src = include_str!("scheduler.rs").replace("\r\n", "\n");
        let body = src.split("#[cfg(test)]").next().unwrap_or_default();
        for banned in ["provider::", "transport::", "reqwest", "ureq", "Command"] {
            assert!(
                !body.contains(banned),
                "scheduler が {banned:?} に触れている。\n\
                 選ぶだけの層に I/O が入ると、同じ入力で同じ結果を返せなくなる"
            );
        }
    }
}
