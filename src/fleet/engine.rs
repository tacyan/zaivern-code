//! Fleet の**時間依存状態** — レーンのヒステリシス・出力の勢い・進捗の裏取り。
//!
//! ## なぜここへ移したか
//!
//! これらはもともと `kanban::KanbanState.tracks` に置かれていた。ところが
//! `KanbanState::update_tracks` は `kanban::draw` からしか呼ばれず、
//! `kanban_ui` は `center == CenterView::Kanban` のフレームでしか走らない。
//! つまり:
//!
//! * **看板を閉じている間、ヒステリシスも `Flow` の裏取りも 1 ミリ秒も進まない**
//! * 看板 → デッキ → 看板 と切り替えると [`Track`] が作り直され、
//!   「停滞・異常」の継続確認 (`TROUBLE_HOLD_MS`) が**リセットされる**
//! * デッキは**別の** `tracks` を持ち、しかもラダー無しの判定で回していた
//!
//! 時間の関数である状態を、寿命の短い所有者 (ビュー) の中に置いたのが原因で、
//! 設計原則 1「ユーザーが失って困る状態は UI の破棄を生き延びる場所に置く」の
//! 適用漏れだった。ここは `crate::fleet::store::FleetStore` が所有し、
//! **ビューは読むだけ**になる。
//!
//! ## 判定そのものは 1 行も書き換えていない
//!
//! ラダー ([`classify_stream`])・確信度の床 ([`Read::lane`])・進捗の裏取り
//! ([`trouble_confirmed`])・ヒステリシス ([`LaneTracker`]) は
//! **既存の実装をそのまま呼ぶ / そのまま移設した**。新しい判定アルゴリズムは
//! 1 つも作っていない。壊れていたのは「誰がそれを呼ぶか」だけだった。

use crate::kanban::{
    bucket_series, classify_stream, has_new_content, norm_tail, tail_delta, Activity, Column, Flow,
    Read, Source, FLOW_WINDOW_MS, LAND_HIGHLIGHT_MS, PULSE_BUCKETS, PULSE_WINDOW_MS,
};

use super::model::Observation;

/// 「新しい出力があった」と見なす直近の窓 (LIVE 表示・サンプリング速度の判断)。
const NOISY_WINDOW_MS: u64 = 3_000;

// ---------------------------------------------------------------------------
// レーン移動ポリシー (デバウンス) — kanban.rs から**そのまま**移設
// ---------------------------------------------------------------------------

/// 1 枚のカードのレーン位置を、ちらつかせずに動かす状態機械。
///
/// - 判定が現在のレーンと同じなら何もしない (候補は取り下げ)
/// - 違うレーンの判定が [`Column::hold_ms`] 以上続いたら初めて移動
/// - 承認待ち・完了は `hold_ms == 0` なので即座に動く
///   (「停滞・異常」だけは `TROUBLE_HOLD_MS` 続くことを求める)
///
/// 8 レーンでは 思考↔編集↔実行↔検証 の往復がそのままレーンをまたぐので、
/// この機械がいちばん効く場所になる。
/// 判定が 1 サンプル揺れただけでカードが飛ぶのを**構造的に不可能**にする。
#[derive(Clone, Copy, Debug)]
pub struct LaneTracker {
    lane: Column,
    /// 候補レーンと、その候補が続き始めた時刻
    pending: Option<(Column, u64)>,
    /// 現在のレーンへ着地した時刻 (ハイライト用)
    landed_ms: u64,
}

impl LaneTracker {
    pub fn new(lane: Column, now_ms: u64) -> Self {
        Self {
            lane,
            pending: None,
            landed_ms: now_ms,
        }
    }

    pub fn lane(&self) -> Column {
        self.lane
    }

    /// 現在のレーンへ着地した時刻。
    pub fn landed_ms(&self) -> u64 {
        self.landed_ms
    }

    /// 着地ハイライトの強さ (1.0 → 0.0)。0.0 なら描かなくてよい。
    pub fn land_glow(&self, now_ms: u64) -> f32 {
        land_glow(self.landed_ms, now_ms)
    }

    /// 望ましいレーン `want` を与えて 1 ステップ進める。移動したら true。
    pub fn step(&mut self, want: Column, now_ms: u64) -> bool {
        if want == self.lane {
            self.pending = None;
            return false;
        }
        let hold = want.hold_ms();
        match self.pending {
            Some((c, since)) if c == want => {
                if now_ms.saturating_sub(since) >= hold {
                    self.lane = want;
                    self.landed_ms = now_ms;
                    self.pending = None;
                    return true;
                }
            }
            _ => {
                if hold == 0 {
                    self.lane = want;
                    self.landed_ms = now_ms;
                    self.pending = None;
                    return true;
                }
                self.pending = Some((want, now_ms));
            }
        }
        false
    }
}

/// 着地ハイライトの強さ (1.0 → 0.0)。**描画側もこの 1 本を使う**
/// (`AgentView.landed_ms` から計算するので、`LaneTracker` を持ち回らない)。
pub fn land_glow(landed_ms: u64, now_ms: u64) -> f32 {
    let age = now_ms.saturating_sub(landed_ms);
    if age >= LAND_HIGHLIGHT_MS {
        return 0.0;
    }
    1.0 - age as f32 / LAND_HIGHLIGHT_MS as f32
}

// ---------------------------------------------------------------------------
// 1 体分の追跡状態 — kanban.rs から**そのまま**移設
// ---------------------------------------------------------------------------

/// 1 セッションぶんの追跡状態 (レーン位置・アクティビティ・出力の勢い)。
#[derive(Clone, Debug)]
pub struct Track {
    lane: LaneTracker,
    /// いまのアクティビティと、それが始まった時刻 (経過タイマー)
    activity: Activity,
    since_ms: u64,
    source: Source,
    detail: String,
    /// 段位不足で採らなかった見張りの異常判定 (カードに ⚠ で出す)
    suspicion: Option<&'static str>,
    /// 直近に触ったファイル / 走らせたコマンド (分かった時点で更新)
    last_file: String,
    last_cmd: String,
    /// 最後にサンプルした画面末尾 (カードの一言・ホバープレビューの元)
    tail: Vec<String>,
    /// 最後にサンプルした画面末尾の**正規化表現** ([`norm_tail`])。
    /// 「新しい中身が出たか」の判定はこちらで行う (スピナーに騙されない)。
    norm: Vec<String>,
    /// 最後に**意味のある進捗**があった時刻
    progress_ms: Option<u64>,
    /// このカードを最初に観測した時刻 (窓が埋まったかの判断に使う)
    born_ms: u64,
    /// 出力の勢い `(時刻, 新規文字数)`
    pulse: Vec<(u64, u64)>,
}

impl Track {
    fn new(read: &Read, now_ms: u64) -> Self {
        Self {
            lane: LaneTracker::new(read.lane(), now_ms),
            activity: read.activity,
            since_ms: now_ms,
            source: read.source,
            detail: read.detail.clone(),
            suspicion: read.suspicion,
            last_file: String::new(),
            last_cmd: String::new(),
            tail: Vec::new(),
            norm: Vec::new(),
            progress_ms: None,
            born_ms: now_ms,
            pulse: Vec::new(),
        }
    }

    /// **生の出力ストリームの事実** — 見張りの異常判定の裏取りに使う ([`Flow`])。
    ///
    /// - 直近 `FLOW_WINDOW_MS` に新しい中身が出た → `Live`
    /// - 同じ時間ぶん観測していて 1 行も増えていない → `Silent`
    /// - まだ観測が足りない → `Unknown` (判断材料にしない = 従来通りの扱い)
    pub fn flow(&self, now_ms: u64) -> Flow {
        if self
            .progress_ms
            .is_some_and(|p| now_ms.saturating_sub(p) <= FLOW_WINDOW_MS)
        {
            return Flow::Live;
        }
        if now_ms.saturating_sub(self.born_ms) >= FLOW_WINDOW_MS && !self.norm.is_empty() {
            return Flow::Silent;
        }
        Flow::Unknown
    }

    /// 直近 30 秒の出力の勢い (古い → 新しい)。
    pub fn pulse_series(&self, now_ms: u64) -> Vec<f32> {
        bucket_series(&self.pulse, now_ms, PULSE_WINDOW_MS, PULSE_BUCKETS)
    }

    /// 直近 3 秒に新しい出力があったか (LIVE 表示・サンプリング速度の判断)。
    pub fn recently_noisy(&self, now_ms: u64) -> bool {
        self.pulse
            .iter()
            .any(|(t, v)| *v > 0 && now_ms.saturating_sub(*t) <= NOISY_WINDOW_MS)
    }

    /// 表示レーン (デバウンス済み)。
    pub fn lane(&self) -> Column {
        self.lane.lane()
    }

    /// 現在のレーンへ着地した時刻。
    pub fn landed_ms(&self) -> u64 {
        self.lane.landed_ms()
    }

    /// **観測 1 件で 1 ステップ進める。**
    ///
    /// 判定の優先順位は [`classify_stream`] が持ち、確信度の床は
    /// [`Read::lane`] が持つ。ここがやるのは
    /// 「材料を渡す → 結果を時間方向へ畳む」だけである。
    fn step(&mut self, obs: &Observation, now_ms: u64) -> Read {
        // 生の出力ストリームの裏取り。**前回までの観測**で決めるので、
        // このティックの画面を反映する前に取る (従来と同じ順序)。
        let flow = self.flow(now_ms);
        // 画面が来ていないティックは、前回サンプルした画面で判定する
        // (生死・承認・レート制限といった構造化信号は毎ティック最新)。
        let tail: &[String] = match obs.tail_lines.as_deref() {
            Some(t) => t,
            None => &self.tail,
        };
        let read = classify_stream(
            obs.running,
            obs.attention,
            obs.rate_limited.is_some(),
            obs.ladder.as_ref(),
            obs.sup,
            tail,
            flow,
        );

        if let Some(fresh) = obs.tail_lines.as_deref() {
            let delta = tail_delta(&self.tail, fresh);
            self.pulse.push((now_ms, delta));
            let from = now_ms.saturating_sub(PULSE_WINDOW_MS);
            self.pulse.retain(|(t, _)| *t >= from);
            self.tail = fresh.to_vec();
            // **意味のある進捗**の時刻を更新する (スピナー/カウンタは潰す)。
            let norm = norm_tail(fresh);
            if has_new_content(&self.norm, &norm) {
                self.progress_ms = Some(now_ms);
            }
            self.norm = norm;
        }

        if self.activity != read.activity {
            self.activity = read.activity;
            self.since_ms = now_ms;
        }
        self.source = read.source;
        self.detail = read.detail.clone();
        self.suspicion = read.suspicion;
        match read.activity {
            Activity::Editing if !read.detail.is_empty() => {
                self.last_file = read.detail.clone();
            }
            Activity::Running | Activity::Verifying if !read.detail.is_empty() => {
                self.last_cmd = read.detail.clone();
            }
            _ => {}
        }

        // **確信度の床を通したレーン**へ寄せる
        // (画面推定だけで承認待ち/停滞・異常/完了にしない)。
        self.lane.step(read.lane(), now_ms);
        read
    }
}

/// 追跡表を 1 ステップ進める内部関数。[`super::store::FleetStore`] だけが呼ぶ。
///
/// 戻り値は「このティックで初めて現れた ID」— 呼び出し側 (UI) が
/// 「新しく起動した 1 体を選ぶ」判断に使う。
pub(super) fn step_tracks(
    tracks: &mut std::collections::HashMap<u64, Track>,
    obs: &[Observation],
    now_ms: u64,
) -> (Vec<super::model::AgentView>, StepStats) {
    let mut views = Vec::with_capacity(obs.len());
    let mut stats = StepStats::default();
    stats.first_fill = tracks.is_empty();

    for o in obs {
        if !tracks.contains_key(&o.id) {
            stats.arrived.push(o.id);
        }
        // 初回は「材料を全部渡した判定」で作る (空の Read で作らない)。
        let seed = classify_stream(
            o.running,
            o.attention,
            o.rate_limited.is_some(),
            o.ladder.as_ref(),
            o.sup,
            o.tail_lines.as_deref().unwrap_or(&[]),
            Flow::Unknown,
        );
        let track = tracks
            .entry(o.id)
            .or_insert_with(|| Track::new(&seed, now_ms));
        let read = track.step(o, now_ms);

        if o.running {
            stats.any_running = true;
        }
        if read.activity.is_busy() || track.recently_noisy(now_ms) {
            stats.busy = true;
        }
        if track.lane.land_glow(now_ms) > 0.0 {
            stats.animating = true;
        }

        views.push(super::model::AgentView {
            id: o.id,
            kind: o.kind.get(),
            title: o.title.clone(),
            icon: o.icon.clone(),
            lane: track.lane(),
            activity: track.activity,
            source: track.source,
            detail: track.detail.clone(),
            suspicion: track.suspicion,
            flow: track.flow(now_ms),
            running: o.running,
            attention: o.attention,
            rate_limited: o.rate_limited.clone(),
            since_ms: track.since_ms,
            landed_ms: track.landed_ms(),
            uptime_ms: o.uptime_ms,
            tail: track.tail.clone(),
            last_file: track.last_file.clone(),
            last_cmd: track.last_cmd.clone(),
            pulse: track.pulse_series(now_ms),
        });
    }

    // 消えたセッションの追跡は捨てる (無限に太らせない)。
    tracks.retain(|id, _| obs.iter().any(|o| o.id == *id));
    (views, stats)
}

/// [`step_tracks`] が持ち帰る、追跡表の外側の事実。
#[derive(Debug, Default)]
pub(super) struct StepStats {
    pub busy: bool,
    pub any_running: bool,
    pub animating: bool,
    /// このティックで初めて現れた ID。
    pub arrived: Vec<u64>,
    /// 追跡表がこのティックまで空だった (= 初回の総取り込み)。
    pub first_fill: bool,
}
