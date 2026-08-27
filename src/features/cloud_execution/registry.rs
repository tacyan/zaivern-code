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
    /// **手元の枠だけは台帳から引き継ぐ。** 手元の実行先そのものは設定
    /// (`default_max_jobs`) から毎回組み直すが、*いま何本走っているか*は
    /// ロックの中で数えている値 ([`claim_local_slot`]) が正しい。
    pub fn targets(&self) -> Result<Vec<ExecutionTarget>, CloudError> {
        Ok(with_local(self.ctx.local_max_jobs, store::load_targets()?))
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
                        // 一度 Ready と分かっているものを降格させない。
                        // **ただし Provider が「消えかけ / 消えた」と言うなら
                        // そちらを採る** — 手元の記憶より、向こうの現状が正しい。
                        if lifecycle == TargetLifecycle::Ready
                            && !t.lifecycle.blocks_new_jobs()
                        {
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
    ///
    /// ## 3 段に分けてある理由 (この版で直した競合)
    ///
    /// 最初の版は「実行中 0 件を確かめる → Provider へ問い合わせる →
    /// 台帳から消す」を**ロック無しで**並べていた。確認と削除のあいだに
    /// 別のプロセスが [`claim_slot`] できるので、
    ///
    /// 1. A: `active_jobs == 0` を確認
    /// 2. B: 枠を取って仕事を載せる
    /// 3. A: VM を消す ← **走っている仕事ごと消える**
    ///
    /// が起こりうる。しかも Provider への往復は数秒あるので、窓は広い。
    ///
    /// だから **「確認」と「削除中への遷移」を台帳ロックの中で原子的に**
    /// 行い ([`Registry::reserve_destroy`])、その後で**ロックを手放してから**
    /// Provider を呼ぶ。[`claim_slot`] は同じロックの中で最新の状態を
    /// 読み直すので、遷移後の枠取りは必ず断られる。
    ///
    /// ## 失敗したとき
    ///
    /// * **届いていないと確実に分かる失敗** (認証・設定・安全のための拒否) →
    ///   元の状態へ戻す。サーバーは消えていない
    /// * **届いたか分からない失敗** (時間切れ・通信・Provider の 5xx) →
    ///   [`TargetLifecycle::Destroying`] のまま残す。**安易に Ready へ戻さない** —
    ///   消えかけの機械へ次の仕事を載せるより、止まっているほうが安全。
    ///   回復は `zai cloud target probe` (Provider の状態を確かめ直す) か
    ///   `zai cloud target remove` (台帳から外す)
    pub fn destroy(&self, name_or_id: &str) -> Result<ExecutionTarget, CloudError> {
        // 1) ロックの中で確かめて、削除中へ移す (ここまで原子的)
        let target = self.reserve_destroy(name_or_id)?;

        // 2) **ロックの外で** Provider を呼ぶ (数秒かかりうる)
        let outcome = self
            .provider(target.provider.as_str())
            .and_then(|p| p.destroy(&target));

        // 3) 結果で台帳を確定させる
        match outcome {
            Ok(()) => {
                store::with_targets(|list| list.retain(|x| x.id != target.id))?;
                Ok(target)
            }
            Err(e) if certainly_untried(&e) => {
                self.release_destroy(&target, &e)?;
                Err(e)
            }
            Err(e) => {
                self.hold_destroying(&target, &e)?;
                Err(e)
            }
        }
    }

    /// 台帳ロックの中で「消してよいか」を確かめ、[`TargetLifecycle::Destroying`]
    /// へ移す。**名前の解決もこの中で行う** — 外で引いた写しを使うと、
    /// 引いてから予約するまでのあいだに状態が変わりうる。
    fn reserve_destroy(&self, name_or_id: &str) -> Result<ExecutionTarget, CloudError> {
        store::with_targets(|list| {
            let hits: Vec<usize> = list
                .iter()
                .enumerate()
                .filter(|(_, t)| t.name == name_or_id || t.id.as_str() == name_or_id)
                .map(|(i, _)| i)
                .collect();
            match hits.len() {
                0 => {
                    return Err(CloudError::config(format!(
                        "実行先 {name_or_id} が見つかりません (zai cloud target list で確認できます)"
                    )))
                }
                1 => {}
                n => {
                    return Err(CloudError::config(format!(
                        "{name_or_id} に {n} 件が一致します。ID で指定してください"
                    )))
                }
            }
            let t = &mut list[hits[0]];
            if !t.managed {
                return Err(CloudError::security(format!(
                    "{name_or_id} は Zaivern が作った実行先ではないので消しません。\n\
                     一覧から外すだけなら zai cloud target remove を使ってください"
                )));
            }
            if t.lifecycle == TargetLifecycle::Destroying {
                return Err(CloudError::config(format!(
                    "{name_or_id} はすでに削除中です ({})",
                    t.note
                )));
            }
            // **ここが要。** 確認と遷移が同じロックの中にあるので、
            // 「0 件だと確かめた直後に 1 本載る」が起こり得ない。
            if t.capacity.active_jobs > 0 {
                return Err(CloudError::config(format!(
                    "{name_or_id} ではまだ {} 本の仕事が走っています",
                    t.capacity.active_jobs
                )));
            }
            let before = t.clone();
            t.lifecycle = TargetLifecycle::Destroying;
            t.note = "削除中 (Provider へ問い合わせています)".to_string();
            Ok(before)
        })?
    }

    /// 削除が**始まっていない**と分かったので、元の状態へ戻す。
    fn release_destroy(&self, before: &ExecutionTarget, why: &CloudError) -> Result<(), CloudError> {
        store::with_targets(|list| {
            if let Some(t) = list.iter_mut().find(|t| t.id == before.id) {
                // 自分が付けた予約だけを外す (別の誰かが動かしていたら触らない)
                if t.lifecycle == TargetLifecycle::Destroying {
                    t.lifecycle = before.lifecycle;
                    t.note = crate::features::cloud_execution::redact::redact(&format!(
                        "削除は行われませんでした: {why}"
                    ));
                }
            }
        })
    }

    /// 削除が**届いたか分からない**ので、削除中のまま留め置く。
    fn hold_destroying(&self, before: &ExecutionTarget, why: &CloudError) -> Result<(), CloudError> {
        store::with_targets(|list| {
            if let Some(t) = list.iter_mut().find(|t| t.id == before.id) {
                t.lifecycle = TargetLifecycle::Destroying;
                t.note = crate::features::cloud_execution::redact::redact(&format!(
                    "削除の結果が不明です: {why} / probe で確かめ直すまで新しい仕事は載せません"
                ));
            }
        })
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
///
/// ## ここが「載せる」の最終確定点
///
/// Scheduler は**選ぶだけ**で、選んだ瞬間の写ししか見ていない。選んでから
/// ここへ来るまでに実行先は準備中へ戻ることも、抜け始めることも、壊れることも
/// ある (`probe` / `refresh` / `destroy` が別のスレッド・別のプロセスから
/// 書き換える)。だから**確定させるのはここだけ**にして、
/// [`TargetLifecycle::Ready`] **でなければ配らない**。
///
/// 判定に使うのは**そのとき台帳に書いてあるもの**だけ。呼び出し側が
/// 古い [`ExecutionTarget`] を握っていても、ここで読み直すので
/// 「消えかけの実行先へ仕事を載せる」が起こらない
/// ([`Registry::destroy`] の予約と同じロックを通る)。
///
/// Scheduler 側の判定 ([`super::scheduler::evaluate`]) を消すわけではない —
/// あちらは「どれが良いか」を説明つきで選ぶための純関数で、こちらは
/// 「いま本当に載せてよいか」の 1 点確認。**同じ規則を 2 度書くのではなく、
/// 最後の 1 回だけがロックの中にある**という関係になっている。
pub fn claim_slot(id: &TargetId) -> Result<(), CloudError> {
    store::with_targets(|list| {
        let Some(t) = list.iter_mut().find(|t| &t.id == id) else {
            return Err(CloudError::config(format!("実行先 {id} が見つかりません")));
        };
        // **Ready 以外へは配らない。** 消えかけ (`destroying`) も、
        // まだ出来ていないもの (`provisioning` / `unknown`) も、抜けかけ
        // (`draining`) も、壊れたもの (`failed`) も同じ扱いにする。
        if t.lifecycle != TargetLifecycle::Ready {
            let note = if t.note.is_empty() {
                "zai cloud target probe で確かめ直してください".to_string()
            } else {
                t.note.clone()
            };
            return Err(CloudError::no_capacity(format!(
                "{} は {} なので新しい仕事を載せません ({note})",
                t.name,
                t.lifecycle.id(),
            )));
        }
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

/// 手元の実行先を先頭に置いた一覧を組む。
///
/// **台帳側の手元の行は枠を数えるためだけに在る** ([`claim_local_slot`]) ので、
/// 上限も能力も毎回設定から組み直し、*いま何本走っているか*だけを引き継ぐ。
/// 引き継がずに素朴へ足すと、**`local` が 2 行**並ぶ (画面と `find` の両方が
/// 壊れる) ので、組み立ては**この 1 か所**だけにする。
pub fn with_local(local_max_jobs: u16, stored: Vec<ExecutionTarget>) -> Vec<ExecutionTarget> {
    let mut local = ExecutionTarget::local(local_max_jobs);
    if let Some(s) = stored.iter().find(|t| t.id == local.id) {
        local.capacity.active_jobs = s.capacity.active_jobs;
    }
    let mut out = vec![local];
    out.extend(
        stored
            .into_iter()
            .filter(|t| t.transport != super::model::TransportKind::Local),
    );
    out
}

/// 手元の枠を 1 つ取る。**手元にも上限がある** (§31 の `default_max_jobs`)。
///
/// ## なぜ台帳の中で数えるのか
///
/// 手元の実行先は設定から組み直すので台帳には載っていない。だが枠の数え上げ
/// だけは**プロセスをまたいで**合っていないと意味がない — `zai cloud exec` を
/// 2 つの端末から叩けば、それは 2 つのプロセスである。数を覚える場所を新しく
/// 作る (DB / 常駐) のではなく、**すでにロックのある台帳**へ手元の行を
/// 1 つ置いて、遠隔の実行先とまったく同じ道 ([`store::with_targets`]) を通す。
///
/// 台帳の行が持つ意味は `capacity.active_jobs` **だけ**。上限も能力も
/// 毎回設定から組み直す ([`Registry::targets`]) ので、古い値が残ることはない。
pub fn claim_local_slot(max_jobs: u16) -> Result<(), CloudError> {
    let local = ExecutionTarget::local(max_jobs);
    store::with_targets(|list| {
        let t = match list.iter_mut().position(|t| t.id == local.id) {
            Some(i) => &mut list[i],
            None => {
                list.push(local.clone());
                list.last_mut().expect("いま入れた")
            }
        };
        // 上限は設定が正 (`config.toml` を書き換えたら次の仕事から効く)
        t.capacity.max_jobs = max_jobs;
        if !t.capacity.has_room() {
            return Err(CloudError::no_capacity(format!(
                "手元ではすでに {} 本を実行中です (default_max_jobs = {})",
                t.capacity.active_jobs, t.capacity.max_jobs
            )));
        }
        t.capacity.active_jobs += 1;
        Ok(())
    })?
}

/// **その失敗は「Provider へ届いていない」と確実に言えるか。**
///
/// 言えるものだけを元の状態へ戻す。言えないもの (時間切れ・通信・5xx) を
/// 戻すと、実際には消えている機械を Ready として配ってしまう。
fn certainly_untried(e: &CloudError) -> bool {
    matches!(
        e,
        // 印が合わずに断った / トークンが無い / プロファイルが無い —
        // どれも DELETE を送る前に止まっている
        CloudError::Security(_) | CloudError::Auth(_) | CloudError::Config(_)
    )
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

    /// 手元の枠を取る。取れなければ [`CloudError::NoCapacity`]。
    pub fn claim_local(max_jobs: u16) -> Result<Self, CloudError> {
        claim_local_slot(max_jobs)?;
        Ok(Self {
            id: ExecutionTarget::local(max_jobs).id,
            active: true,
        })
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

    /// 探りが通った状態にする。**枠を配るのは `Ready` だけ**なので、
    /// 枠の試験は「探りが通った実行先」から始める必要がある
    /// (製品では [`Registry::probe`] → [`apply_probe`] がここを付ける)。
    fn mark_ready(id: &TargetId) {
        store::with_targets(|l| {
            if let Some(t) = l.iter_mut().find(|t| &t.id == id) {
                t.lifecycle = TargetLifecycle::Ready;
            }
        })
        .expect("書ける");
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
        mark_ready(&t.id);
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
        mark_ready(&t.id);
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

    // ───────── 削除と枠取得の排他 (指摘 3) ─────────

    use crate::features::cloud_execution::provider::http::HttpResponse;
    use crate::features::cloud_execution::provider::ProviderKind;
    use crate::features::cloud_execution::test_support::{FakeHttpClient, GatedHttpClient};
    use std::sync::Arc;

    fn hetzner_profile() -> ProviderProfile {
        ProviderProfile {
            name: "hetzner-eu".into(),
            kind: ProviderKind::Hetzner,
            token_env: "ZAIVERN_TEST_HCLOUD_TOKEN".into(),
            api_base: "https://api.example/v1".into(),
            max_jobs: 2,
            ..ProviderProfile::default()
        }
    }

    /// Provider 側が返す「Zaivern が作った印つきの」サーバー。
    fn managed_server(target_id: &str) -> serde_json::Value {
        serde_json::json!({
            "id": 42,
            "name": "w1",
            "status": "running",
            "public_net": { "ipv4": { "ip": "203.0.113.10" } },
            "server_type": { "name": "t", "cores": 2, "memory": 4.0, "disk": 40.0 },
            "labels": {
                "managed_by": "zaivern",
                "zaivern_target_id": target_id,
                "zaivern_profile": "hetzner-eu"
            }
        })
    }

    fn get_ok(target_id: &str) -> HttpResponse {
        HttpResponse {
            status: 200,
            body: serde_json::json!({ "server": managed_server(target_id) }).to_string(),
        }
    }

    /// 台帳へ「Zaivern が作った」実行先を 1 つ置く。
    fn put_managed(name: &str) -> TargetId {
        let mut t = target(name, TargetOpts::default());
        t.provider = crate::features::cloud_execution::model::ProviderId::new("hetzner-eu");
        t.managed = true;
        t.provider_ref = Some("42".into());
        t.lifecycle = TargetLifecycle::Ready;
        let id = t.id.clone();
        store::save_targets(&[t]).expect("書ける");
        id
    }

    fn registry_with(http: Arc<dyn crate::features::cloud_execution::provider::HttpClient>) -> Registry {
        std::env::set_var("ZAIVERN_TEST_HCLOUD_TOKEN", "super-secret-test-token");
        Registry::with_ctx(
            all_profiles(&[hetzner_profile()]),
            ProviderCtx {
                http,
                timeout: Duration::from_secs(5),
                local_max_jobs: 2,
            },
            Duration::from_secs(5),
        )
    }

    /// 仕事が走っている実行先は、**Provider を 1 度も呼ばずに**断る。
    #[test]
    fn 実行中の仕事があれば削除処理を呼ばない() {
        let _home = home_guard("destroy-busy-no-call");
        let id = put_managed("w1");
        let http = Arc::new(FakeHttpClient::new(vec![]));
        let calls = http.calls();
        let reg = registry_with(http.clone());

        let _slot = SlotGuard::claim(&id).expect("枠を取れる");
        let e = reg.destroy("w1").expect_err("断る");
        assert!(format!("{e}").contains("1 本"), "{e}");
        // **Provider へ 1 本も送っていない** (台本が空なので、送れば失敗する)
        assert!(calls.lock().expect("読める").is_empty(), "Provider を呼んだ");
        // 予約もしていない (状態は元のまま)
        assert_eq!(
            store::load_targets().expect("読める")[0].lifecycle,
            TargetLifecycle::Ready
        );
    }

    /// **削除を予約したあと、Provider の応答を待っているあいだの枠取りは断られる。**
    ///
    /// 順序は `sleep` ではなく門で作る。判定も時間で見ない —
    /// もし台帳ロックを握ったまま Provider を呼んでいたら、こちらの
    /// `claim_slot` はロック待ちで時間切れ ([`CloudError::Timeout`]) になる。
    /// **返ってきたのが `NoCapacity` であること自体が「ロックを手放している」
    /// 証拠**になる。
    #[test]
    fn 削除予約中の枠取りは断られる() {
        let _home = home_guard("destroy-gate-claim");
        let dir = store::cloud_dir();
        let id = put_managed("w1");

        let (http, gate) = GatedHttpClient::new(
            "DELETE",
            get_ok(id.as_str()),
            HttpResponse {
                status: 200,
                body: "{}".into(),
            },
        );
        let http = Arc::new(http);
        let calls = http.calls();

        let reg = registry_with(http.clone());
        let handle = std::thread::spawn(move || {
            store::set_test_dir(Some(dir));
            reg.destroy("w1")
        });

        // DELETE に入るまで待つ (= 予約は済み、ロックは手放している)
        gate.wait_entered();

        // 古い写しを持っている呼び出し側でも、ここで断られる
        let e = claim_slot(&id).expect_err("断る");
        assert!(
            matches!(e, CloudError::NoCapacity(_)),
            "ロックを握ったまま Provider を呼んでいる可能性がある: {e:?}"
        );
        assert!(format!("{e}").contains("destroying"), "{e}");

        gate.release();
        handle.join().expect("終わる").expect("消せる");

        // 消えた実行先は台帳から居なくなる
        assert!(store::load_targets().expect("読める").is_empty());
        let sent = calls.lock().expect("読める").clone();
        assert_eq!(sent.len(), 2, "{sent:?}");
        assert_eq!(sent[1].method, "DELETE");
    }

    /// 先に枠を取られていたら、削除側が断られる (逆順でも守る)。
    #[test]
    fn 先に枠を取られたら削除側が断られる() {
        let _home = home_guard("destroy-lost-race");
        let id = put_managed("w1");
        let http = Arc::new(FakeHttpClient::new(vec![]));
        let calls = http.calls();
        let reg = registry_with(http);

        let held = SlotGuard::claim(&id).expect("枠を取れる");
        assert!(reg.destroy("w1").is_err());
        assert!(calls.lock().expect("読める").is_empty());

        // 枠を返せば消せる
        drop(held);
        let http2 = Arc::new(FakeHttpClient::new(vec![
            get_ok(id.as_str()),
            HttpResponse {
                status: 200,
                body: "{}".into(),
            },
        ]));
        let reg2 = registry_with(http2);
        reg2.destroy("w1").expect("消せる");
        assert!(store::load_targets().expect("読める").is_empty());
    }

    /// **古い写しでは削除予約を突破できない。**
    #[test]
    fn 古い写しでは削除予約を突破できない() {
        let _home = home_guard("destroy-stale-view");
        let id = put_managed("w1");
        // 呼び出し側が持っている「まだ Ready で空きがある」写し
        let stale = store::load_targets().expect("読める")[0].clone();
        assert_eq!(stale.lifecycle, TargetLifecycle::Ready);
        assert!(stale.capacity.has_room());

        // 台帳の側だけが削除中になる
        store::with_targets(|l| {
            l[0].lifecycle = TargetLifecycle::Destroying;
            l[0].note = "削除中".into();
        })
        .expect("書ける");

        // 写しは Ready のままだが、枠取りは台帳を読み直すので断られる
        let e = match SlotGuard::claim(&stale.id) {
            Ok(_) => panic!("古い写しで削除中の実行先へ枠を配ってしまった"),
            Err(e) => e,
        };
        assert!(matches!(e, CloudError::NoCapacity(_)), "{e:?}");
        assert_eq!(e.exit_code(), 4);
        let _ = id;
    }

    /// **手元の行は台帳にも在るが、一覧には 1 行しか出ない。**
    #[test]
    fn 手元の実行先が二重に出ない() {
        let _home = home_guard("registry-local-once");
        let reg = registry();
        // 手元で 1 本走った後の台帳を模す
        claim_local_slot(2).expect("取れる");
        let all = reg.targets().expect("数えられる");
        assert_eq!(
            all.iter()
                .filter(|t| t.transport == super::super::model::TransportKind::Local)
                .count(),
            1,
            "local が二重に出た: {all:#?}"
        );
        // 使用中の数は引き継ぎ、上限は設定から組み直す
        assert_eq!(all[0].capacity.active_jobs, 1);
        assert_eq!(all[0].capacity.max_jobs, reg.ctx.local_max_jobs);
        // 名前でも 1 件に決まる (2 件あると find が断る)
        reg.find("local").expect("引ける");
    }

    /// **P1-2 の再現。** Scheduler が選んだ瞬間と枠を取る瞬間のあいだに
    /// 実行先の状態は変わりうる。枠取りが `Ready` を求めないと、
    /// 準備中 / 抜けかけ / 壊れた機械へ仕事を載せてしまう。
    #[test]
    fn 選んだ後に準備中へ落ちたら枠を配らない() {
        for (state, 名) in [
            (TargetLifecycle::Provisioning, "provisioning"),
            (TargetLifecycle::Draining, "draining"),
            (TargetLifecycle::Failed, "failed"),
            (TargetLifecycle::Unknown, "unknown"),
        ] {
            let _home = home_guard(&format!("claim-not-ready-{名}"));
            let id = put_managed("w1");
            // Scheduler が見たとき (= いま手元にある写し) は Ready
            let snapshot = store::load_targets().expect("読める");
            assert_eq!(snapshot[0].lifecycle, TargetLifecycle::Ready);

            // …その後で状態が変わる (探りの失敗 / 抜け始め / 作り直し)
            store::with_targets(|l| l[0].lifecycle = state).expect("書ける");

            let e = match SlotGuard::claim(&id) {
                Ok(_) => panic!("{名} の実行先へ枠を配ってしまった"),
                Err(e) => e,
            };
            assert!(matches!(e, CloudError::NoCapacity(_)), "{名}: {e:?}");
            assert!(format!("{e}").contains(state.id()), "{名}: {e}");
            // 枠は 1 つも増えていない
            let after = store::load_targets().expect("読める");
            assert_eq!(after[0].capacity.active_jobs, 0, "{名}");
        }
    }

    /// **結果が分からない失敗では、安易に Ready へ戻さない。**
    #[test]
    fn 削除の結果が不明なら削除中のまま留め置く() {
        let _home = home_guard("destroy-unknown");
        let id = put_managed("w1");
        // GET は成功、DELETE は 5xx が続く (再試行の上限まで)
        let mut script = vec![get_ok(id.as_str())];
        for _ in 0..8 {
            script.push(HttpResponse {
                status: 503,
                body: String::new(),
            });
        }
        let reg = registry_with(Arc::new(FakeHttpClient::new(script)));
        let e = reg.destroy("w1").expect_err("失敗する");
        assert!(format!("{e}").contains("503"), "{e}");

        let after = store::load_targets().expect("読める");
        assert_eq!(after.len(), 1, "台帳から消してしまっている");
        assert_eq!(
            after[0].lifecycle,
            TargetLifecycle::Destroying,
            "Ready へ戻してしまっている"
        );
        assert!(after[0].note.contains("不明"), "{}", after[0].note);
        // 新しい仕事は載らない
        assert!(matches!(
            claim_slot(&id),
            Err(CloudError::NoCapacity(_))
        ));
    }

    /// **届いていないと確実に分かる失敗なら、元の状態へ戻す。**
    #[test]
    fn 届いていないと分かる失敗なら元へ戻す() {
        let _home = home_guard("destroy-untried");
        let id = put_managed("w1");
        // 認証で断られた = DELETE は送られていない
        let reg = registry_with(Arc::new(FakeHttpClient::new(vec![HttpResponse {
            status: 401,
            body: r#"{"error":{"message":"unable to authenticate"}}"#.into(),
        }])));
        let e = reg.destroy("w1").expect_err("失敗する");
        assert!(matches!(e, CloudError::Auth(_)), "{e:?}");

        let after = store::load_targets().expect("読める");
        assert_eq!(after[0].lifecycle, TargetLifecycle::Ready, "戻していない");
        assert!(after[0].note.contains("削除は行われませんでした"), "{}", after[0].note);
        // 元どおり枠を取れる
        SlotGuard::claim(&id).expect("取れる");
    }

    /// **回復経路**: Provider の状態を確かめ直せば、また使えるようになる。
    #[test]
    fn 確かめ直せば削除中から復帰できる() {
        let _home = home_guard("destroy-recover");
        let id = put_managed("w1");
        store::with_targets(|l| {
            l[0].lifecycle = TargetLifecycle::Destroying;
            l[0].note = "削除の結果が不明です".into();
        })
        .expect("書ける");
        assert!(claim_slot(&id).is_err(), "削除中なのに取れてしまう");

        // `zai cloud target probe` が通る = 機械は生きていた
        let probe = ProbeResult {
            reachable: true,
            latency_ms: 3,
            capabilities: Default::default(),
            shell: "/bin/sh".into(),
            kernel: "test".into(),
            error: String::new(),
        };
        store::with_targets(|l| {
            let fixed = apply_probe(&l[0], &probe);
            l[0] = fixed;
        })
        .expect("書ける");

        assert_eq!(
            store::load_targets().expect("読める")[0].lifecycle,
            TargetLifecycle::Ready
        );
        SlotGuard::claim(&id).expect("また取れる");

        // 台帳から外す道も残っている (機械には触らない)
        let reg = registry_with(Arc::new(FakeHttpClient::new(vec![])));
        reg.remove_target("w1").expect("外せる");
    }

    /// Scheduler は削除中の実行先を選ばない (要件 4)。
    #[test]
    fn schedulerは削除中の実行先を選ばない() {
        use crate::features::cloud_execution::scheduler;
        let mut t = target("w1", TargetOpts::default());
        t.lifecycle = TargetLifecycle::Destroying;
        let req = crate::features::cloud_execution::model::ExecutionRequirements::default();
        assert_eq!(
            scheduler::evaluate(&req, &t),
            Err(scheduler::Reject::Destroying),
            "「未確認」と混ぜている (次に打つ手が違う)"
        );
        assert_eq!(scheduler::select_target(&req, &[t]), None);
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
