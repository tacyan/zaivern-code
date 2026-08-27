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

use super::model::{
    CloudError, ExecutionTarget, JobId, ProbeResult, SlotHolder, TargetId, TargetLifecycle,
};
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
        // **前回落ちた仕事の枠をここで返す** (P1-1)。台帳を 2 つ読むだけなので
        // 安い。失敗しても組み立ては続ける — 後始末ができないことと、
        // 実行先を一覧できないことは別の話。
        let _ = reconcile_active_jobs();
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
    ///
    /// ## 走っている仕事があれば断る (P2-2)
    ///
    /// 外した後も [`SlotGuard`] は枠を返そうとするが、実行先そのものが
    /// 台帳に無いので**返す先が無い**。仕事が終わったことも枠が空いたことも
    /// どこにも残らず、記録だけが「走っている実行先 X」を指したまま孤児になる。
    ///
    /// [`Registry::destroy`] と同じ考え方で、**引くのも数えるのも同じロックの
    /// 中で**行う。外で `find` してから消すと、そのあいだに枠を取られる。
    pub fn remove_target(&self, name_or_id: &str) -> Result<ExecutionTarget, CloudError> {
        store::with_targets(|list| {
            // 手元の実行先は台帳の行としては「枠の数」でしかないので、
            // 名前で引く前に断る (行が在っても外す対象ではない)
            if name_or_id == "local" {
                return Err(CloudError::config("手元の機械は一覧から外せません"));
            }
            let hits: Vec<usize> = list
                .iter()
                .enumerate()
                .filter(|(_, t)| {
                    t.transport != super::model::TransportKind::Local
                        && (t.name == name_or_id || t.id.as_str() == name_or_id)
                })
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
            let t = &list[hits[0]];
            if t.capacity.active_jobs() > 0 {
                return Err(CloudError::config(format!(
                    "{} ではまだ {} 本の仕事が走っています。終了後に一覧から外してください",
                    t.name,
                    t.capacity.active_jobs()
                )));
            }
            Ok(list.remove(hits[0]))
        })?
    }

    /// 実行先を確かめて、分かったことを台帳へ書き戻す。
    ///
    /// **ここだけが [`TargetLifecycle::Ready`] を付ける。**
    pub fn probe(&self, name_or_id: &str) -> Result<(ExecutionTarget, ProbeResult), CloudError> {
        let target = self.find(name_or_id)?;
        let tr = transport::for_target(&target, self.ssh_timeout);
        self.probe_with(&target, tr.as_ref())
    }

    /// [`Registry::probe`] の本体。**Transport を差し替えられる**ので、
    /// 「確かめているあいだに削除予約が入る」順序を試験が決定的に作れる。
    pub(crate) fn probe_with(
        &self,
        target: &ExecutionTarget,
        tr: &dyn ExecutionTransport,
    ) -> Result<(ExecutionTarget, ProbeResult), CloudError> {
        let target = target.clone();
        let probe = tr.probe(&target)?;
        let updated = apply_probe(&target, &probe);
        // 手元の機械は台帳に載っていないので書き戻さない
        if updated.transport != super::model::TransportKind::Local {
            // **読んだときから状態が変わっていたら書かない** (削除予約を消さない)
            if write_back_if_current(&updated, target.generation)? == WriteBack::Stale {
                return Err(CloudError::config(format!(
                    "{} の状態が確認中に変わりました。もう一度 probe してください",
                    target.name
                )));
            }
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
                        // **削除中のものには触らない。** Provider の一覧は
                        // 問い合わせた瞬間の写しで、そのあいだに入った削除予約より
                        // 古い。ここで上書きすると予約が消える
                        // (`destroy` は自分で結果を書き戻すので、待てばよい)。
                        if slot.lifecycle == TargetLifecycle::Destroying {
                            continue;
                        }
                        // 接続情報と能力は Provider が正しい。枠の使用中は手元が正しい
                        let holders = std::mem::take(&mut slot.capacity.holders);
                        let lifecycle = slot.lifecycle;
                        let generation = slot.generation;
                        *slot = t.clone();
                        slot.capacity.holders = holders;
                        slot.generation = generation;
                        // 一度 Ready と分かっているものを降格させない。
                        // **ただし Provider が「消えかけ / 消えた」と言うなら
                        // そちらを採る** — 手元の記憶より、向こうの現状が正しい。
                        if lifecycle == TargetLifecycle::Ready
                            && !t.lifecycle.blocks_new_jobs()
                        {
                            slot.lifecycle = TargetLifecycle::Ready;
                        }
                        bump(slot);
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
        let (target, reserved_at) = self.reserve_destroy(name_or_id)?;

        // 2) **ロックの外で** Provider を呼ぶ (数秒かかりうる)
        let outcome = self
            .provider(target.provider.as_str())
            .and_then(|p| p.destroy(&target));

        // 3) 結果で台帳を確定させる
        match outcome {
            Ok(()) => {
                // **自分の予約のときだけ消す。** 予約してから応答が返るまでに
                // 別の操作が入っていたら、その新しい状態を上書きしない。
                let removed = store::with_targets(|list| {
                    match list.iter().position(|x| x.id == target.id) {
                        Some(i) if list[i].generation == reserved_at => {
                            list.remove(i);
                            true
                        }
                        _ => false,
                    }
                })?;
                if !removed {
                    return Err(CloudError::config(format!(
                        "{} は削除しましたが、そのあいだに台帳の状態が変わっていました。\n                         zai cloud target list で確かめてください",
                        target.name
                    )));
                }
                Ok(target)
            }
            Err(e) if certainly_untried(&e) => {
                self.release_destroy(&target, reserved_at, &e)?;
                Err(e)
            }
            Err(e) => {
                self.hold_destroying(&target, reserved_at, &e)?;
                Err(e)
            }
        }
    }

    /// 台帳ロックの中で「消してよいか」を確かめ、[`TargetLifecycle::Destroying`]
    /// へ移す。**名前の解決もこの中で行う** — 外で引いた写しを使うと、
    /// 引いてから予約するまでのあいだに状態が変わりうる。
    fn reserve_destroy(&self, name_or_id: &str) -> Result<(ExecutionTarget, u64), CloudError> {
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
            if t.capacity.active_jobs() > 0 {
                return Err(CloudError::config(format!(
                    "{name_or_id} ではまだ {} 本の仕事が走っています",
                    t.capacity.active_jobs()
                )));
            }
            let before = t.clone();
            t.lifecycle = TargetLifecycle::Destroying;
            t.note = "削除中 (Provider へ問い合わせています)".to_string();
            // **この予約の世代を返す。** 結果を書き戻すときに照合して、
            // 古い削除処理の応答が新しい操作を上書きしないようにする。
            bump(t);
            Ok((before, t.generation))
        })?
    }

    /// 削除が**始まっていない**と分かったので、元の状態へ戻す。
    fn release_destroy(
        &self,
        before: &ExecutionTarget,
        reserved_at: u64,
        why: &CloudError,
    ) -> Result<(), CloudError> {
        store::with_targets(|list| {
            if let Some(t) = list.iter_mut().find(|t| t.id == before.id) {
                // **自分が付けた予約だけを外す。** 世代で見るので、
                // 別の操作が入った後の状態を上書きしない
                // (状態だけを見ると、他人が入れた新しい予約まで外してしまう)。
                if t.generation == reserved_at && t.lifecycle == TargetLifecycle::Destroying {
                    t.lifecycle = before.lifecycle;
                    t.note = crate::features::cloud_execution::redact::redact(&format!(
                        "削除は行われませんでした: {why}"
                    ));
                    bump(t);
                }
            }
        })
    }

    /// 削除が**届いたか分からない**ので、削除中のまま留め置く。
    fn hold_destroying(
        &self,
        before: &ExecutionTarget,
        reserved_at: u64,
        why: &CloudError,
    ) -> Result<(), CloudError> {
        store::with_targets(|list| {
            if let Some(t) = list.iter_mut().find(|t| t.id == before.id) {
                // 自分の予約のままなら「結果不明」と書く。別の操作が
                // 入っていたら触らない (新しいほうが正しい)
                if t.generation != reserved_at {
                    return;
                }
                t.lifecycle = TargetLifecycle::Destroying;
                t.note = crate::features::cloud_execution::redact::redact(&format!(
                    "削除の結果が不明です: {why} / probe で確かめ直すまで新しい仕事は載せません"
                ));
                bump(t);
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
                // **待っているあいだに消されたなら、Ready にはしない。**
                if write_back_if_current(&updated, target.generation)? == WriteBack::Stale {
                    return Err(CloudError::config(format!(
                        "{} の状態が待機中に変わりました (削除された可能性があります)",
                        target.name
                    )));
                }
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

// ─────────── 古い写しで状態を上書きしない (世代の照合) ───────────

/// 状態を書き換えたことを記録する。**枠の増減では増やさない**
/// (増やすと、仕事が 1 本載るたびに探りの書き戻しが落ちる)。
pub fn bump(t: &mut ExecutionTarget) {
    t.generation = t.generation.wrapping_add(1);
}

/// 書き戻しを断った理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteBack {
    /// 書けた。
    Applied,
    /// 読んでからのあいだに別の誰かが状態を変えた (自分の写しはもう古い)。
    Stale,
    /// もう台帳に居ない。
    Gone,
}

/// **読んだときから状態が変わっていなければ**書き戻す。
///
/// ## なぜ要るのか (この版で直した壊れ方)
///
/// `probe` は「読む → ネットワークで確かめる (数秒) → 書き戻す」の形。
/// そのあいだに別のプロセスが `destroy` を予約すると、
///
/// ```text
/// A: probe が古い写しを読む (ready)
///                       B: destroying を書いて Provider へ DELETE を送る
/// A: probe が成功し、古い写しから作った ready を書き戻す ← 予約が消える
///                       新しい仕事が枠を取り、その VM ごと消える
/// ```
///
/// ロックを数秒握り続けるのは答えではない (そのあいだ他が全部止まる)。
/// **読んだときの世代を覚えておき、書く瞬間にロックの中で照合する。**
///
/// 枠 (`capacity.holders`) は書き戻しの対象にしない — あれは「いま何本
/// 走っているか」で、探りが持ち帰るものではない。
fn write_back_if_current(
    updated: &ExecutionTarget,
    seen_generation: u64,
) -> Result<WriteBack, CloudError> {
    store::with_targets(|list| {
        let Some(slot) = list.iter_mut().find(|t| t.id == updated.id) else {
            return WriteBack::Gone;
        };
        if slot.generation != seen_generation {
            return WriteBack::Stale;
        }
        // **削除中を上書きしない。** 世代が同じでも (= 読んだときすでに
        // destroying だった場合)、SSH に入れたというだけで Ready へ戻さない。
        // 戻してよいと言えるのは Provider へ確かめ直したときだけ。
        if slot.lifecycle == TargetLifecycle::Destroying
            && updated.lifecycle != TargetLifecycle::Destroying
        {
            return WriteBack::Stale;
        }
        let holders = std::mem::take(&mut slot.capacity.holders);
        let generation = slot.generation;
        *slot = updated.clone();
        slot.capacity.holders = holders;
        slot.generation = generation;
        bump(slot);
        WriteBack::Applied
    })
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
pub fn claim_slot(id: &TargetId, job: &JobId) -> Result<(), CloudError> {
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
        if !t.capacity.hold(holder(job)) {
            return Err(CloudError::no_capacity(format!(
                "{} はすでに {} 本を実行中です (max_jobs = {})",
                t.name,
                t.capacity.active_jobs(),
                t.capacity.max_jobs
            )));
        }
        Ok(())
    })?
}

/// いま自分が握るときの印。**PID を一緒に置く**ので、落ちた後の後始末が
/// `targets.json` だけで完結する ([`reconcile_active_jobs`])。
fn holder(job: &JobId) -> SlotHolder {
    SlotHolder {
        job: job.clone(),
        pid: std::process::id(),
        since_unix: store::now_unix(),
    }
}

// ─────────────── 落ちた仕事の後始末 ───────────────

/// 後始末の結果 (`doctor` と試験が読む)。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Reconciled {
    /// 持ち主が居なくなっていた枠の数。
    pub stale_jobs: usize,
    /// 返した枠の数 (= `stale_jobs`。分けて持つのは読む側のため)。
    pub freed_slots: usize,
}

/// **落ちた仕事が握ったままの枠を返す。**
///
/// ## なぜ要るのか
///
/// 枠を返すのは [`SlotGuard`] の Drop だが、`kill -9` / OOM Killer /
/// 電源断 / OS 再起動では Drop が呼ばれない。すると台帳には「走っている 1 本」が
/// 残り、**実際には 0 件なのに `active_jobs == max_jobs`** になって、
/// その実行先へ二度と仕事を載せられなくなる。
///
/// ## 1 つのファイル・1 つのロックで完結させる (この版で直した壊れ方)
///
/// 前の版は「`jobs.json` を Failed にする」→「`targets.json` から 1 引く」の
/// **2 段**だった。あいだで止まると仕事だけが完了扱いになり、次回は
/// `is_final()` で除外されて**枠が永久に返らない**。順序を逆にすると、
/// 今度は同じ枠を二重に返しうる。**2 つのファイルにまたがる限り、どちらかが
/// 必ず壊れる。**
///
/// いまは枠を*数*ではなく**持ち主の集合** ([`SlotHolder`]) で持つので、
/// 判定に要るもの (仕事 ID と PID) が `targets.json` の中に揃っている。
/// 後始末は**そのファイルのロックの中だけ**で終わり、
///
/// * 途中で止まっても、次の実行が同じ判定をやり直すだけ
/// * 何度実行しても結果が変わらない (同じ id を 2 度外しても no-op)
/// * `jobs.json` の履歴上限で記録が押し出されても、枠は迷子にならない
///
/// `jobs.json` 側の記録を `failed` にするのは**履歴の見た目を合わせるだけ**で、
/// 枠の正しさはそれに依存しない (失敗しても枠はもう返っている)。
///
/// ## 残る穴 (正直に)
///
/// 判定は**手元の PID の生存**だけ。だから
///
/// * PID が再利用されていると、その回は枠が返らない (再利用した側が終われば返る)
/// * **手元のプロセスが死んでも、リモートで走っているコマンドは死なない。**
///   枠を返すのは「手元がもう見ていない」という意味であって、
///   「向こうが止まった」という意味ではない。SSH が切れれば向こうも
///   終わるのが普通だが、`nohup` などで切り離されていれば残る。
///   確実に止めたいなら `zai cloud shell` で入って確かめること
pub fn reconcile_active_jobs() -> Result<Reconciled, CloudError> {
    // **実行先の台帳のロックの中だけ**で判定して外す (ここが要)。
    let dropped: Vec<JobId> = store::with_targets(|list| {
        let mut dropped = Vec::new();
        for t in list.iter_mut() {
            t.capacity.holders.retain(|h| {
                if crate::instances::pid_alive(h.pid) {
                    return true;
                }
                dropped.push(h.job.clone());
                false
            });
        }
        dropped
    })?;

    if dropped.is_empty() {
        return Ok(Reconciled::default());
    }

    // 履歴の見た目を合わせる。**ここが失敗しても枠はもう返っている**ので、
    // 後始末そのものは成功として返す (次に走らせても二重には返らない)。
    let _ = store::with_jobs(|jobs| {
        for j in jobs.iter_mut() {
            if j.state.is_final() || !dropped.contains(&j.id) {
                continue;
            }
            j.state = super::model::ExecutionJobState::Failed;
            j.message = "実行していたプロセスが終了したため、この仕事の枠を返しました".to_string();
            if j.ended_unix == 0 {
                j.ended_unix = store::now_unix();
            }
        }
    });

    Ok(Reconciled {
        stale_jobs: dropped.len(),
        freed_slots: dropped.len(),
    })
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
        local.capacity.holders = s.capacity.holders.clone();
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
/// 台帳の行が持つ意味は `capacity.holders` **だけ**。上限も能力も
/// 毎回設定から組み直す ([`Registry::targets`]) ので、古い値が残ることはない。
pub fn claim_local_slot(max_jobs: u16, job: &JobId) -> Result<(), CloudError> {
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
        if !t.capacity.hold(holder(job)) {
            return Err(CloudError::no_capacity(format!(
                "手元ではすでに {} 本を実行中です (default_max_jobs = {})",
                t.capacity.active_jobs(),
                t.capacity.max_jobs
            )));
        }
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

/// 枠を返す。**何度呼んでも同じ** (持ち主の id で外すので冪等)。
pub fn release_slot(id: &TargetId, job: &JobId) -> Result<(), CloudError> {
    store::with_targets(|list| {
        if let Some(t) = list.iter_mut().find(|t| &t.id == id) {
            t.capacity.release(job);
        }
    })
}

/// 枠の増減を、途中で失敗しても必ず返す形で包む。
pub struct SlotGuard {
    id: TargetId,
    job: JobId,
    active: bool,
}

impl SlotGuard {
    /// 取れたら番人を返す。取れなければ [`CloudError::NoCapacity`]。
    ///
    /// **どの仕事が握るかを名前で渡す。** 数ではなく持ち主で持つので、
    /// 返すのも後始末も同じ id で冪等に行える。
    pub fn claim(id: &TargetId, job: &JobId) -> Result<Self, CloudError> {
        claim_slot(id, job)?;
        Ok(Self {
            id: id.clone(),
            job: job.clone(),
            active: true,
        })
    }

    /// 手元の枠を取る。取れなければ [`CloudError::NoCapacity`]。
    pub fn claim_local(max_jobs: u16, job: &JobId) -> Result<Self, CloudError> {
        claim_local_slot(max_jobs, job)?;
        Ok(Self {
            id: ExecutionTarget::local(max_jobs).id,
            job: job.clone(),
            active: true,
        })
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if self.active {
            // **失敗しても黙って落とす。** ここで panic すると、
            // 仕事の失敗が「後始末の失敗」に化けて原因が見えなくなる。
            let _ = release_slot(&self.id, &self.job);
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
    use crate::features::cloud_execution::model::{Capabilities, ExecutionJobState, OsFamily};
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

    /// 試験用の仕事 ID。**枠は持ち主の id で持つ**ので、1 本ごとに別の名前が要る。
    fn jid(n: &str) -> JobId {
        JobId::new(format!("j-test-{n}"))
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
        let g1 = SlotGuard::claim(&t.id, &jid("1")).expect("1 本目");
        let g2 = SlotGuard::claim(&t.id, &jid("2")).expect("2 本目");
        let e = match SlotGuard::claim(&t.id, &jid("3")) {
            Ok(_) => panic!("3 本目を通してしまった"),
            Err(e) => e,
        };
        assert!(matches!(e, CloudError::NoCapacity(_)), "{e:?}");
        assert_eq!(e.exit_code(), 4, "空きが無いときの終了コード");

        // 返せばまた取れる
        drop(g1);
        let _g3 = SlotGuard::claim(&t.id, &jid("4")).expect("返した分は取れる");
        drop(g2);
        // 台帳の数と実際の番人の数が合っている
        let after = reg.find("dev-01").expect("引ける");
        assert_eq!(after.capacity.active_jobs(), 1);
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
            .map(|i| {
                let id = id.clone();
                let dir = dir.clone();
                std::thread::spawn(move || {
                    // 置き場の差し替えはスレッドごとなので、子でも指し直す
                    store::set_test_dir(Some(dir));
                    // **1 本ごとに別の仕事 ID** (同じ id は 2 度数えないため)
                    claim_slot(&id, &jid(&format!("race-{i}"))).is_ok()
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
            reg.find("dev-01").expect("引ける").capacity.active_jobs(),
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
        let mut t = target(
            "dev-01",
            TargetOpts {
                active_jobs: 2,
                ..TargetOpts::default()
            },
        );
        t.capacity = crate::features::cloud_execution::model::TargetCapacity::busy(4, 2);
        let probe = ProbeResult {
            reachable: true,
            ..ProbeResult::default()
        };
        // apply_probe 自身は capacity を触らない
        assert_eq!(apply_probe(&t, &probe).capacity.active_jobs(), 2);
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
            slot.capacity
                .hold(crate::features::cloud_execution::model::SlotHolder {
                    job: jid("busy"),
                    pid: std::process::id(),
                    since_unix: 0,
                });
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

        let _slot = SlotGuard::claim(&id, &jid("6")).expect("枠を取れる");
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
        let e = claim_slot(&id, &jid("7")).expect_err("断る");
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

        let held = SlotGuard::claim(&id, &jid("8")).expect("枠を取れる");
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
        let e = match SlotGuard::claim(&stale.id, &jid("9")) {
            Ok(_) => panic!("古い写しで削除中の実行先へ枠を配ってしまった"),
            Err(e) => e,
        };
        assert!(matches!(e, CloudError::NoCapacity(_)), "{e:?}");
        assert_eq!(e.exit_code(), 4);
        let _ = id;
    }

    // ───── 走っている仕事がある実行先は外せない (P2-2) ─────

    /// **P2-2 の再現。** 外した後も [`SlotGuard`] は枠を返そうとするが、
    /// 返す先が無い。仕事の記録だけが孤児になり、台帳と食い違う。
    #[test]
    fn remove_target_rejects_active_jobs() {
        let _home = home_guard("remove-active");
        let reg = registry();
        let t = add(&reg, "dev-01", 4);
        mark_ready(&t.id);
        let _g1 = SlotGuard::claim(&t.id, &jid("10")).expect("1 本目");
        let _g2 = SlotGuard::claim(&t.id, &jid("11")).expect("2 本目");

        let e = reg.remove_target("dev-01").expect_err("断る");
        let text = format!("{e}");
        assert!(
            text.contains("2 本"),
            "何本走っているか言っていない: {text}"
        );
        assert!(text.contains("dev-01"), "{text}");
        // 台帳から消えていない
        assert!(reg.find("dev-01").is_ok(), "断ったのに消してしまった");
    }

    #[test]
    fn remove_target_succeeds_when_idle() {
        let _home = home_guard("remove-idle");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        mark_ready(&t.id);
        // 1 本走らせて、終わらせる
        {
            let _g = SlotGuard::claim(&t.id, &jid("12")).expect("取れる");
        }
        let removed = reg.remove_target("dev-01").expect("空いていれば外せる");
        assert_eq!(removed.id, t.id);
        assert!(reg.find("dev-01").is_err(), "消えていない");
    }

    /// **引くのと数えるのが同じロックの中**なので、「空いていると読んでから
    /// 消すまでの隙間」が無い。どちらが先に通っても、通ったほうだけが成る。
    #[test]
    fn remove_target_and_claim_slot_do_not_race() {
        let _home = home_guard("remove-race");
        let reg = registry();
        let t = add(&reg, "dev-01", 1);
        mark_ready(&t.id);

        let id = t.id.clone();
        let dir = store::cloud_dir();
        let (claimed, removed) = std::thread::scope(|sc| {
            let a = {
                let id = id.clone();
                let dir = dir.clone();
                sc.spawn(move || {
                    store::set_test_dir(Some(dir));
                    claim_slot(&id, &jid("13")).is_ok()
                })
            };
            let b = {
                let dir = dir.clone();
                sc.spawn(move || {
                    store::set_test_dir(Some(dir));
                    registry().remove_target("dev-01").is_ok()
                })
            };
            (a.join().expect("終わる"), b.join().expect("終わる"))
        });

        let left = store::load_targets().expect("読める");
        if removed {
            // 外れたなら、枠を取れていてはいけない
            assert!(!claimed, "外したのに枠も配った (どちらも成った)");
            assert!(left.iter().all(|x| x.id != id), "外れていない");
        } else {
            // 外れなかったなら、実行先は残っている
            assert!(left.iter().any(|x| x.id == id), "断ったのに消えている");
        }
    }

    // ───── 確かめているあいだの削除予約を消さない ─────

    /// **P1 の再現。** `probe` は「読む → ネットワークで確かめる → 書き戻す」
    /// の形なので、そのあいだに入った削除予約を古い写しで消しうる。
    /// 消えると新しい仕事が枠を取り、その VM ごと消える。
    ///
    /// 順序は門 (channel) で決定的に作る — 眠りの長さには頼らない。
    #[test]
    fn probeの書き戻しが削除予約を消さない() {
        use crate::features::cloud_execution::test_support::GatedTransport;

        let _home = home_guard("probe-vs-destroy");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        mark_ready(&t.id);
        store::with_targets(|l| l[0].managed = true).expect("書ける");
        let target = reg.find("dev-01").expect("引ける");

        let (tr, gate) = GatedTransport::new(ProbeResult {
            reachable: true,
            ..ProbeResult::default()
        });

        let dir = store::cloud_dir();
        let id = t.id.clone();
        let outcome = std::thread::scope(|sc| {
            // 1) probe が古い写しを読み、確かめに入る (門で止まる)
            let prober = {
                let dir = dir.clone();
                let target = target.clone();
                sc.spawn(move || {
                    store::set_test_dir(Some(dir));
                    let reg = registry();
                    reg.probe_with(&target, &tr).map(|(t, _)| t.lifecycle)
                })
            };
            gate.wait_entered();

            // 2) そのあいだに削除を予約する (Provider は呼ばず、台帳だけ)
            store::with_targets(|l| {
                let slot = l.iter_mut().find(|x| x.id == id).expect("居る");
                slot.lifecycle = TargetLifecycle::Destroying;
                slot.note = "削除中".into();
                bump(slot);
            })
            .expect("書ける");

            // 3) probe を完了させる
            gate.release();
            prober.join().expect("終わる")
        });

        // 書き戻しは断られる (古い写しなので)
        assert!(
            outcome.is_err(),
            "古い写しで書き戻してしまった: {outcome:?}"
        );

        // 台帳は削除中のまま
        let after = store::load_targets().expect("読める");
        assert_eq!(
            after[0].lifecycle,
            TargetLifecycle::Destroying,
            "削除予約が消えた"
        );
        // 新しい仕事は載らない
        assert!(
            matches!(
                claim_slot(&t.id, &jid("after-destroy")),
                Err(CloudError::NoCapacity(_))
            ),
            "消えかけの実行先へ枠を配った"
        );
    }

    /// 削除中の実行先は、**SSH に入れたというだけでは Ready へ戻さない。**
    /// (「結果が不明で復旧待ち」からの復帰は `probe` が Provider を
    /// 確かめ直す経路でしか起こしてはいけない。)
    #[test]
    fn 削除中はsshに入れてもreadyへ戻さない() {
        use crate::features::cloud_execution::test_support::GatedTransport;

        let _home = home_guard("probe-destroying-no-ready");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        store::with_targets(|l| {
            l[0].lifecycle = TargetLifecycle::Destroying;
            l[0].note = "削除の結果が不明です".into();
        })
        .expect("書ける");
        let target = reg.find("dev-01").expect("引ける");
        assert_eq!(target.lifecycle, TargetLifecycle::Destroying);

        // 読んだときすでに destroying (= 世代は変わらない) でも戻さない
        let (tr, gate) = GatedTransport::new(ProbeResult {
            reachable: true,
            ..ProbeResult::default()
        });
        gate.release(); // 止めずに通す
        let out = reg.probe_with(&target, &tr);
        assert!(out.is_err(), "削除中を Ready へ戻した: {out:?}");
        assert_eq!(
            store::load_targets().expect("読める")[0].lifecycle,
            TargetLifecycle::Destroying
        );
        let _ = t;
    }

    /// `wait_ready` も同じ (待っているあいだに消されたら Ready にしない)。
    /// 世代の照合が 1 か所 (`write_back_if_current`) に在ることを、
    /// 書き戻しの入口ごとに確かめる。
    #[test]
    fn 古い世代の書き戻しはどの入口でも断られる() {
        let _home = home_guard("writeback-generation");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        mark_ready(&t.id);
        let seen = reg.find("dev-01").expect("引ける");

        // 別の誰かが状態を進める
        store::with_targets(|l| {
            let slot = l.iter_mut().find(|x| x.id == t.id).expect("居る");
            slot.lifecycle = TargetLifecycle::Draining;
            bump(slot);
        })
        .expect("書ける");

        let mut updated = seen.clone();
        updated.lifecycle = TargetLifecycle::Ready;
        assert_eq!(
            write_back_if_current(&updated, seen.generation).expect("書ける"),
            WriteBack::Stale,
            "古い世代で書き戻せてしまった"
        );
        assert_eq!(
            store::load_targets().expect("読める")[0].lifecycle,
            TargetLifecycle::Draining
        );

        // いまの世代なら書ける
        let now = store::load_targets().expect("読める")[0].generation;
        assert_eq!(
            write_back_if_current(&updated, now).expect("書ける"),
            WriteBack::Applied
        );
        assert_eq!(
            store::load_targets().expect("読める")[0].lifecycle,
            TargetLifecycle::Ready
        );
    }

    /// **古い削除処理の応答が、新しい操作の状態を上書きしない。**
    #[test]
    fn 古い削除の応答は新しい状態を上書きしない() {
        let _home = home_guard("destroy-generation");
        let id = put_managed("w1");

        // 削除を予約した「つもり」の世代を覚える
        let reserved_at = store::load_targets().expect("読める")[0].generation;

        // そのあいだに別の操作が状態を進めた
        store::with_targets(|l| {
            l[0].lifecycle = TargetLifecycle::Draining;
            l[0].note = "別の操作".into();
            bump(l[0..1].iter_mut().next().expect("居る"));
        })
        .expect("書ける");

        // 古い削除処理が「消えた」と言って戻ってきても、消さない
        let removed = store::with_targets(|list| match list.iter().position(|x| x.id == id) {
            Some(i) if list[i].generation == reserved_at => {
                list.remove(i);
                true
            }
            _ => false,
        })
        .expect("書ける");
        assert!(!removed, "古い応答で消してしまった");
        assert_eq!(
            store::load_targets().expect("読める")[0].lifecycle,
            TargetLifecycle::Draining,
            "新しい状態が失われた"
        );
    }

    // ───── 落ちた仕事の後始末 ─────

    /// 仕事の記録を 1 件置く (履歴の見た目を見るテスト用)。
    fn put_job(id: &str, target: &TargetId, state: ExecutionJobState, owner_pid: u32) {
        store::upsert_job(&crate::features::cloud_execution::model::ExecutionJob {
            id: JobId::new(id),
            target: target.clone(),
            state,
            command: "true".into(),
            workspace: None,
            result_ref: String::new(),
            result_oid: String::new(),
            owner_pid,
            started_unix: 1,
            ended_unix: 0,
            exit_code: None,
            message: String::new(),
        })
        .expect("書ける");
    }

    /// **もう居ない PID。** 自分自身から離れた大きな値を使い、
    /// 生きていないことを確かめてから使う (偶然生きていたら試験が嘘になる)。
    fn dead_pid() -> u32 {
        for pid in [4_000_001u32, 4_000_003, 4_000_007, 4_000_011] {
            if !crate::instances::pid_alive(pid) {
                return pid;
            }
        }
        panic!("死んでいる PID を選べない");
    }

    /// 落ちたプロセスが握ったままの枠を、台帳へ直に置く。
    fn put_stale_holder(id: &TargetId, job: &str) {
        store::with_targets(|list| {
            let t = list.iter_mut().find(|t| &t.id == id).expect("居る");
            t.capacity.holders.push(SlotHolder {
                job: JobId::new(job),
                pid: dead_pid(),
                since_unix: 1,
            });
        })
        .expect("書ける");
    }

    /// **P1 の再現。** `kill -9` / OOM / 電源断では [`SlotGuard`] の Drop が
    /// 呼ばれないので、台帳に枠が残ったまま実行中の仕事は 0 件になる。
    #[test]
    fn stale_running_job_does_not_permanently_consume_slot() {
        let _home = home_guard("reconcile-stale");
        let reg = registry();
        let t = add(&reg, "dev-01", 1);
        mark_ready(&t.id);

        // 落ちたプロセスが残した状態: 枠は埋まったまま、記録は running のまま
        put_stale_holder(&t.id, "j-dead");
        put_job("j-dead", &t.id, ExecutionJobState::Running, dead_pid());
        assert!(
            claim_slot(&t.id, &jid("stale-a")).is_err(),
            "後始末の前から枠が空いていては、再現になっていない"
        );

        let r = reconcile_active_jobs().expect("後始末できる");
        assert_eq!(r.stale_jobs, 1, "{r:?}");
        assert_eq!(r.freed_slots, 1, "{r:?}");

        assert_eq!(
            reg.find("dev-01").expect("引ける").capacity.active_jobs(),
            0
        );
        SlotGuard::claim(&t.id, &jid("stale-b")).expect("また取れる");
        let jobs = store::load_jobs().expect("読める");
        assert_eq!(jobs[0].state, ExecutionJobState::Failed, "履歴が合っていない");
    }

    /// **生きている仕事の枠は奪わない。**
    #[test]
    fn reconciliation_preserves_live_jobs() {
        let _home = home_guard("reconcile-live");
        let reg = registry();
        let t = add(&reg, "dev-01", 4);
        mark_ready(&t.id);

        // 生きている 1 本 (このプロセスが持ち主) と、落ちた 1 本
        std::mem::forget(SlotGuard::claim(&t.id, &jid("live")).expect("1 本目"));
        put_stale_holder(&t.id, "j-dead");
        assert_eq!(
            reg.find("dev-01").expect("引ける").capacity.active_jobs(),
            2
        );

        let r = reconcile_active_jobs().expect("後始末できる");
        assert_eq!(r.stale_jobs, 1, "生きている側まで片付けた: {r:?}");
        assert_eq!(
            reg.find("dev-01").expect("引ける").capacity.active_jobs(),
            1,
            "生きている 1 本の枠まで返してしまった"
        );
        // 生きている持ち主はそのまま残っている
        let held = reg.find("dev-01").expect("引ける").capacity.holders;
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].job, jid("live"));
    }

    /// **何度実行しても結果が変わらない** (冪等)。
    ///
    /// 前の版は 2 つのファイルにまたがっていたので、途中で止まると
    /// 枠が永久に返らない / 二重に返る、のどちらかになった。
    #[test]
    fn reconciliation_is_idempotent() {
        let _home = home_guard("reconcile-idempotent");
        let reg = registry();
        let t = add(&reg, "dev-01", 3);
        mark_ready(&t.id);
        std::mem::forget(SlotGuard::claim(&t.id, &jid("alive")).expect("生きている 1 本"));
        put_stale_holder(&t.id, "j-dead-1");
        put_stale_holder(&t.id, "j-dead-2");

        let first = reconcile_active_jobs().expect("1 回目");
        assert_eq!(first.freed_slots, 2, "{first:?}");
        let after = reg.find("dev-01").expect("引ける").capacity.active_jobs();
        assert_eq!(after, 1);

        for _ in 0..3 {
            let again = reconcile_active_jobs().expect("何度でも呼べる");
            assert_eq!(again.freed_slots, 0, "二重に返した: {again:?}");
            assert_eq!(
                reg.find("dev-01").expect("引ける").capacity.active_jobs(),
                1,
                "生きている枠を奪った"
            );
        }
    }

    /// **履歴の記録が失われても、枠は迷子にならない。**
    ///
    /// `jobs.json` は上限 ([`store::MAX_JOBS_KEPT`]) で古いものから押し出される。
    /// 判定を履歴に頼っていると、押し出された仕事の枠が永久に残る。
    #[test]
    fn reconciliation_survives_lost_history() {
        let _home = home_guard("reconcile-no-history");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        mark_ready(&t.id);
        put_stale_holder(&t.id, "j-forgotten");
        // 履歴には 1 件も無い (押し出された後を模す)
        assert!(store::load_jobs().expect("読める").is_empty());

        let r = reconcile_active_jobs().expect("後始末できる");
        assert_eq!(r.freed_slots, 1, "履歴が無いと枠を返せない: {r:?}");
        assert_eq!(
            reg.find("dev-01").expect("引ける").capacity.active_jobs(),
            0
        );
    }

    /// **履歴の書き込みに失敗しても、枠はもう返っている。**
    ///
    /// 枠の正しさが履歴に依存していないことを、`jobs.json` を書けない形
    /// (ディレクトリで塞ぐ) にして確かめる。
    #[test]
    fn reconciliation_frees_slot_even_if_history_write_fails() {
        let _home = home_guard("reconcile-history-fails");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        mark_ready(&t.id);
        put_stale_holder(&t.id, "j-dead");

        // 記録の置き場を塞ぐ (書き込みが必ず失敗する)
        let p = store::jobs_path();
        let _ = std::fs::remove_file(&p);
        std::fs::create_dir(&p).expect("塞げる");

        let r = reconcile_active_jobs().expect("枠は返せる");
        assert_eq!(r.freed_slots, 1, "{r:?}");
        assert_eq!(
            reg.find("dev-01").expect("引ける").capacity.active_jobs(),
            0
        );
    }

    /// **後始末と新しい枠取りが競合しても、上限を超えず生きている枠を失わない。**
    #[test]
    fn reconciliation_does_not_exceed_max_jobs() {
        let _home = home_guard("reconcile-capacity");
        let reg = registry();
        let t = add(&reg, "dev-01", 2);
        mark_ready(&t.id);
        put_stale_holder(&t.id, "j-dead");

        let id = t.id.clone();
        let dir = store::cloud_dir();
        // 後始末と、新しい枠取りを同時に走らせる
        let (freed, claimed) = std::thread::scope(|sc| {
            let a = {
                let dir = dir.clone();
                sc.spawn(move || {
                    store::set_test_dir(Some(dir));
                    reconcile_active_jobs().map(|r| r.freed_slots).unwrap_or(0)
                })
            };
            let b = {
                let dir = dir.clone();
                let id = id.clone();
                sc.spawn(move || {
                    store::set_test_dir(Some(dir));
                    claim_slot(&id, &JobId::new("j-newcomer")).is_ok()
                })
            };
            (a.join().expect("終わる"), b.join().expect("終わる"))
        });

        let cap = reg.find("dev-01").expect("引ける").capacity;
        assert!(
            cap.active_jobs() <= cap.max_jobs,
            "上限を超えた: {cap:?} (freed={freed} claimed={claimed})"
        );
        if claimed {
            assert!(
                cap.holders.iter().any(|h| h.job.as_str() == "j-newcomer"),
                "取れたはずの枠が消えている: {cap:?}"
            );
        }
        // 落ちた側は、どちらの順序でも最後には返る
        reconcile_active_jobs().expect("もう一度");
        let cap = reg.find("dev-01").expect("引ける").capacity;
        assert!(
            !cap.holders.iter().any(|h| h.job.as_str() == "j-dead"),
            "落ちた枠が残っている: {cap:?}"
        );
    }

    /// **手元の行は台帳にも在るが、一覧には 1 行しか出ない。**
    #[test]
    fn 手元の実行先が二重に出ない() {
        let _home = home_guard("registry-local-once");
        let reg = registry();
        // 手元で 1 本走った後の台帳を模す
        claim_local_slot(2, &jid("26")).expect("取れる");
        let all = reg.targets().expect("数えられる");
        assert_eq!(
            all.iter()
                .filter(|t| t.transport
                    == crate::features::cloud_execution::model::TransportKind::Local)
                .count(),
            1,
            "local が二重に出た: {all:#?}"
        );
        // 使用中の数は引き継ぎ、上限は設定から組み直す
        assert_eq!(all[0].capacity.active_jobs(), 1);
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

            let e = match SlotGuard::claim(&id, &jid("21")) {
                Ok(_) => panic!("{名} の実行先へ枠を配ってしまった"),
                Err(e) => e,
            };
            assert!(matches!(e, CloudError::NoCapacity(_)), "{名}: {e:?}");
            assert!(format!("{e}").contains(state.id()), "{名}: {e}");
            // 枠は 1 つも増えていない
            let after = store::load_targets().expect("読める");
            assert_eq!(after[0].capacity.active_jobs(), 0, "{名}");
        }
    }

    /// **走っている `launch --run` の最中は VM を消せない。**
    ///
    /// 枠を取るのが `run_attached` でも `run` でも同じ道なので、
    /// 実行中は `destroy` が Provider を 1 度も呼ばずに断る。
    #[test]
    fn その場実行の最中はvmを消さない() {
        let _home = home_guard("destroy-vs-attached");
        let id = put_managed("w1");
        let http = Arc::new(FakeHttpClient::new(vec![]));
        let calls = http.calls();
        let reg = registry_with(http.clone());
        let target = store::load_targets().expect("読める")[0].clone();

        let (started_tx, started_rx) = std::sync::mpsc::channel::<()>();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel::<()>();
        let dir = store::cloud_dir();

        std::thread::scope(|sc| {
            let runner = {
                let dir = dir.clone();
                let target = target.clone();
                sc.spawn(move || {
                    store::set_test_dir(Some(dir));
                    super::super::runner::run_attached(&target, "agent --serve", || {
                        // 走り始めたことを知らせ、外から終わらせてもらうまで待つ
                        let _ = started_tx.send(());
                        let _ = finish_rx.recv_timeout(Duration::from_secs(30));
                        // 「正常終了した」ことにする
                        crate::procx::hidden_command(if cfg!(windows) { "cmd" } else { "true" })
                            .args(if cfg!(windows) {
                                vec!["/C", "exit", "0"]
                            } else {
                                vec![]
                            })
                            .status()
                            .map_err(|e| CloudError::io(format!("{e}")))
                    })
                })
            };
            started_rx
                .recv_timeout(Duration::from_secs(30))
                .expect("走り始める");

            // 走っている最中は消せない
            let e = reg.destroy("w1").expect_err("断る");
            assert!(format!("{e}").contains("1 本"), "{e}");
            assert!(
                calls.lock().expect("読める").is_empty(),
                "Provider へ DELETE を送った"
            );
            // 予約もされていない
            assert_eq!(
                store::load_targets().expect("読める")[0].lifecycle,
                TargetLifecycle::Ready
            );

            let _ = finish_tx.send(());
            runner.join().expect("終わる").expect("走り終える");
        });

        // 終われば枠は返る
        assert_eq!(
            store::load_targets().expect("読める")[0]
                .capacity
                .active_jobs(),
            0
        );
        let _ = id;
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
            claim_slot(&id, &jid("22")),
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
        SlotGuard::claim(&id, &jid("23")).expect("取れる");
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
        assert!(
            claim_slot(&id, &jid("24")).is_err(),
            "削除中なのに取れてしまう"
        );

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
        SlotGuard::claim(&id, &jid("25")).expect("また取れる");

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
