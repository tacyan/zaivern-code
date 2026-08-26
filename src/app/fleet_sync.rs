use super::*;

impl ZaivernApp {
    /// **Fleet の観測を 1 ティック取り込む。**
    ///
    /// ここが Fleet 状態の**唯一の書き込み口**である。看板・デッキ・Cockpit・
    /// サイドバー・スマホ一覧は、この結果 (`self.fleet.snapshot()`) を読むだけ。
    ///
    /// ## どのフレームでも走る
    ///
    /// 従来この判定は `kanban::KanbanState::update_tracks` にあり、
    /// **看板を開いているフレームしか進まなかった**。看板 → デッキ → 看板 と
    /// 切り替えるだけで「停滞・異常」の継続確認 (`TROUBLE_HOLD_MS`) が
    /// リセットされ、看板を閉じている間はヒステリシスも `Flow` の裏取りも
    /// 1 ミリ秒も進まなかった。ここは `frame_update` から**無条件に**呼ばれる。
    ///
    /// ## PTY を二重に読まない
    ///
    /// 画面末尾を読むのは [`crate::fleet::FleetStore::sample_due`] が真を返した
    /// ティックだけ (動いている間 ~6.7Hz / 静かなら 1Hz)。看板が持っていた
    /// 間引きをそのまま移したので、**看板を開いていたときの費用を超えない**。
    /// 読まないティックは `tail_lines: None` で渡し、追跡側が前回サンプルを
    /// 使い回すので判定は落ちない。
    pub(super) fn fleet_tick(&mut self) {
        let now_ms = self.supervisor.elapsed_ms();
        let fresh = self.fleet.sample_due(now_ms);
        let mut obs: Vec<fleet::Observation> = Vec::with_capacity(self.agents.sessions.len());
        for s in &self.agents.sessions {
            obs.push(fleet::Observation {
                id: s.id,
                kind: fleet::model::AgentKindOpt::pty(),
                title: s.title.clone(),
                icon: if s.icon.is_empty() {
                    "👾".to_string()
                } else {
                    s.icon.clone()
                },
                running: s.running(),
                attention: s.attention,
                rate_limited: s.rate_limited.clone(),
                sup: self.supervisor.state_of(s.id),
                // 状態ラダー上位 3 段 (構造化プロトコル / フック / シェル統合)。
                // **スマホもここから同じ値を読む** — 以前は `column_for` 経由で
                // ラダーを 1 度も見ていなかったので、PC と食い違っていた。
                ladder: self.supervisor.ladder_of(s.id),
                tail_lines: fresh.then(|| s.screen_tail_lines(10, 180)),
                uptime_ms: s.started.elapsed().as_millis() as u64,
            });
        }
        // ACP セッションも同じ 1 本の集計に載せる (`Total Agents` を正しくする)。
        // 接続 0 本なら空 `Vec` なので、使っていない人の費用はゼロ。
        if !self.acp.is_empty() {
            obs.extend(self.acp.fleet_observations(std::time::Instant::now()));
        }
        self.fleet.update(&obs, now_ms);
    }
}
