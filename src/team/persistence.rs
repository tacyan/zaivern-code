//! Team Run の永続化 — **DB を足さない。JSON を原子的に置き換えるだけ。**
//!
//! ## 置き場所
//!
//! ```text
//! ~/.zaivern/team/<ワークスペースキー>/
//!   schema.json     ← 版だけを持つ 1 ファイル
//!   goal.json
//!   tasks.json
//!   agents.json
//!   run.json
//!   events.jsonl    ← 追記専用
//! ```
//!
//! **ワークスペースキーは [`crate::history::workspace_key`] から取る。**
//! CLAUDE.md の絶対ルール: 16 桁キーを自前で作らない (rustc を上げた日に
//! 利用者から見て全部消える)。
//!
//! 仕様書はワークスペース直下の `.zaivern/team/` を挙げていたが、この
//! リポジトリの既存の置き場は `~/.zaivern/<キー>/` に統一されている
//! (`.gitignore` もその前提で書かれている)。**利用者のリポジトリへ
//! 実行時生成物を書き足さない**ほうが約束を守れるので、こちらへ寄せた。
//!
//! ## 壊れていたら黙って初期化しない
//!
//! 初期化してしまうと「昨日の実行が消えた」が静かに起きる。読めない
//! ファイルは `<名前>.corrupt-<epoch>` へ**退避**してから、読めなかった
//! ことを [`LoadOutcome`] で返す。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::model::{Decision, TeamAgent, TeamEvent, TeamGoal, TeamGroup, TeamTask};

/// 保存形式の版。**上げたら移行を書く。**
///
/// * 1 — 初版
/// * 2 — Effect を「発行済み (Dispatched)」と「成功 (Completed)」の 2 段で
///   持つようにした。旧 `done_effects` は「成功済み」として引き取る
///   ([`RunDoc::migrate`])。
pub const SCHEMA_VERSION: u32 = 2;

/// events.jsonl の行数上限 (超えたら古い行から落とす)。
pub const EVENT_LOG_MAX_LINES: usize = 5_000;
/// 1 ファイルのバイト上限。これを超えたものは読まない (壊れているとみなす)。
pub const FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// 状態の置き場 = `<根>/team/<ワークスペースキー>/`。
///
/// **根を引数で受け取る** (`lease::store_path_in` と同じ流儀)。既定の根を
/// 決めるのは [`default_home`] 1 か所だけで、**テストは自分の一時
/// ディレクトリを渡す** — 素で `~/.zaivern` を指す入口を残すと、テストが
/// 利用者の (あるいは同時に走っている別インスタンスの) 台帳の隣に
/// ファイルを作る。`ZAIVERN_HOME` を差し替える手もあるが、**環境変数は
/// 並列に走る他のテストへ漏れる**ので採らない。
pub fn team_dir_in(root: &Path, workspace: &Path) -> PathBuf {
    root.join("team")
        .join(crate::history::workspace_key(workspace))
}

/// 既定の根 (`~/.zaivern`)。**ここが唯一の既定の決め所。**
pub fn default_home() -> PathBuf {
    crate::config::zaivern_dir()
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct SchemaDoc {
    version: u32,
}

/// Effect がどこまで進んだか。
///
/// **発行しただけで「済んだ」ことにしない。** 発行と実行の間でプロセスが
/// 落ちると、済んだ扱いのまま二度と実行されない Effect が残る
/// (エージェントが永久に起動されない、指示が届かない、検証が走らない)。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectState {
    /// 実行側へ渡した。**まだ成功していない。**
    Dispatched,
    /// 実行側が成功を返した。
    Completed,
}

/// Effect 1 件の進み具合。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EffectRecord {
    pub key: String,
    pub state: EffectState,
    /// 記録した時刻 (Unix 秒)。回収の判断に使う。
    pub at: u64,
}

fn default_validation_timeout() -> u64 {
    super::launch::VALIDATION_TIMEOUT_SECS
}

/// 実行そのものの記録。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunDoc {
    pub version: u32,
    /// 実行 ID。**同じ Run を二重に開始しない**ための鍵。
    pub run_id: String,
    pub workspace: String,
    pub spec_source: String,
    pub agent_count: usize,
    pub max_attempts: u8,
    pub review_required: bool,
    pub paused: bool,
    pub stopped: bool,
    pub started_at: u64,
    pub updated_at: u64,
    /// Effect の進み具合 (版 2 以降)。**発行 = 完了ではない。**
    #[serde(default)]
    pub effects: Vec<EffectRecord>,
    /// **人が実行を承認した検証コマンド。**
    ///
    /// `cargo test` などはリポジトリ内の任意コードを実行しうるので、
    /// 承認したものだけを走らせる ([`super::graph::ValidationRisk`])。
    /// Run 単位で持つ — 同じコマンドを試行のたびに聞き直すと、承認が
    /// 「はい」を押すだけの儀式になり、実際には読まれなくなる。
    #[serde(default)]
    pub approved_validation: Vec<String>,
    /// 検証 1 本あたりの時間切れ (秒)。テストは短い値を注入する。
    #[serde(default = "default_validation_timeout")]
    pub validation_timeout_secs: u64,
    /// 版 1 の「処理済み Effect」。読むためだけに残す
    /// ([`RunDoc::migrate`] が `effects` へ移す)。**新しく書かない。**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done_effects: Vec<String>,
}

impl RunDoc {
    /// 旧版で保存された `done_effects` を `effects` へ引き取る。
    ///
    /// 旧版は「発行 = 完了」だったので、**成功済みとして扱うしかない**
    /// (どれが未実行だったかは記録に残っていない)。ここで失われるのは
    /// 「旧版でクラッシュした直前の 1 件」だけで、それ以降は新しい 2 段の
    /// 記録が守る。
    pub fn migrate(&mut self) {
        if self.done_effects.is_empty() {
            return;
        }
        let at = super::model::now_secs();
        for k in std::mem::take(&mut self.done_effects) {
            if !self.effects.iter().any(|e| e.key == k) {
                self.effects.push(EffectRecord {
                    key: k,
                    state: EffectState::Completed,
                    at,
                });
            }
        }
    }
}

/// 保存する状態のまとまり。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Saved {
    pub run: RunDoc,
    pub goal: TeamGoal,
    pub teams: Vec<TeamGroup>,
    pub tasks: Vec<TeamTask>,
    pub agents: Vec<TeamAgent>,
    pub decisions: Vec<Decision>,
    pub events: Vec<TeamEvent>,
}

/// 読み込みの結末。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LoadOutcome {
    /// 何も保存されていない。
    Empty,
    /// 読めた。
    Loaded(Box<Saved>),
    /// 壊れていた。退避したファイル名と理由を返す。
    Corrupt {
        backed_up: Vec<String>,
        reason: String,
    },
    /// 版が新しすぎる (このビルドでは読めない)。**消さない。**
    Newer { found: u32 },
}

/// 書き込みに失敗した理由。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveError {
    Io(String),
    Serialize(String),
}

impl SaveError {
    pub fn detail(&self) -> String {
        match self {
            SaveError::Io(e) => format!("Team の状態を保存できません: {e}"),
            SaveError::Serialize(e) => format!("Team の状態を JSON にできません: {e}"),
        }
    }
}

/// 一時ファイルへ書いて rename する (原子的置き換え)。
///
/// 途中で失敗しても**元のファイルはそのまま**残る。
fn write_atomic(path: &Path, body: &str) -> Result<(), SaveError> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).map_err(|e| SaveError::Io(e.to_string()))?;
    // 一時名はプロセス ID とナノ秒で衝突しないようにする (同じフォルダに
    // 複数インスタンスが書きうる)。
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = dir.join(format!(
        ".{}.{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("team"),
        std::process::id(),
        stamp
    ));
    std::fs::write(&tmp, body).map_err(|e| SaveError::Io(e.to_string()))?;
    // **Windows では置き換えが一時的に断られる。**
    //
    // 宛先を誰かが開いている間 (`zai team stop` が読んでいる最中など) は
    // `MoveFileEx` が ACCESS_DENIED を返す。1 回で諦めると「いちばん混んで
    // いるとき = いちばん保存したいとき」にだけ台帳が書けなくなるので、
    // 短い待ちで数回だけ試す。unix では 1 回目で通るので費用はゼロ。
    //
    // **上限を持つ。** 進まないものを待ち続けても人を待たせるだけなので、
    // 諦めたら理由を返して呼び出し側に見せる。
    let mut last = None;
    for attempt in 0..RENAME_RETRIES {
        match std::fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e.to_string());
                if attempt + 1 < RENAME_RETRIES {
                    std::thread::sleep(RENAME_BACKOFF * (attempt + 1));
                }
            }
        }
    }
    let _ = std::fs::remove_file(&tmp);
    Err(SaveError::Io(
        last.unwrap_or_else(|| "置き換えに失敗しました".to_string()),
    ))
}

/// 置き換えを試す回数。
const RENAME_RETRIES: u32 = 4;
/// 試行の間隔 (回数に比例して伸ばす)。
const RENAME_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

fn read_capped(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > FILE_MAX_BYTES {
        return None;
    }
    std::fs::read_to_string(path).ok()
}

/// 壊れたファイルを退避する。戻り値は退避先のファイル名。
fn quarantine(path: &Path) -> Option<String> {
    if !path.exists() {
        return None;
    }
    let stamp = super::model::now_secs();
    let name = format!(
        "{}.corrupt-{stamp}",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("file")
    );
    let dest = path.with_file_name(&name);
    std::fs::rename(path, &dest).ok().map(|_| name)
}

/// 状態を保存する。**1 ファイルずつ原子的に置き換える。**
pub fn save(dir: &Path, s: &Saved) -> Result<(), SaveError> {
    std::fs::create_dir_all(dir).map_err(|e| SaveError::Io(e.to_string()))?;
    let ser = |v: &dyn erased::Ser| v.to_json().map_err(SaveError::Serialize);
    write_atomic(
        &dir.join("schema.json"),
        &ser(&SchemaDoc {
            version: SCHEMA_VERSION,
        })?,
    )?;
    write_atomic(&dir.join("run.json"), &ser(&s.run)?)?;
    write_atomic(&dir.join("goal.json"), &ser(&s.goal)?)?;
    write_atomic(&dir.join("teams.json"), &ser(&s.teams)?)?;
    write_atomic(&dir.join("tasks.json"), &ser(&s.tasks)?)?;
    write_atomic(&dir.join("agents.json"), &ser(&s.agents)?)?;
    write_atomic(&dir.join("decisions.json"), &ser(&s.decisions)?)?;
    save_events(dir, &s.events)?;
    Ok(())
}

/// イベントログは**追記専用の JSONL**。上限を超えたら古い行から落とす。
pub fn save_events(dir: &Path, events: &[TeamEvent]) -> Result<(), SaveError> {
    let start = events.len().saturating_sub(EVENT_LOG_MAX_LINES);
    let mut body = String::new();
    for e in &events[start..] {
        let line = serde_json::to_string(e).map_err(|e| SaveError::Serialize(e.to_string()))?;
        body.push_str(&line);
        body.push('\n');
    }
    write_atomic(&dir.join("events.jsonl"), &body)
}

/// 状態を読む。
pub fn load(dir: &Path) -> LoadOutcome {
    if !dir.exists() {
        return LoadOutcome::Empty;
    }
    let schema_path = dir.join("schema.json");
    if !schema_path.exists() {
        return LoadOutcome::Empty;
    }
    let Some(raw) = read_capped(&schema_path) else {
        let b = quarantine(&schema_path).into_iter().collect();
        return LoadOutcome::Corrupt {
            backed_up: b,
            reason: "schema.json を読めません".into(),
        };
    };
    let schema: SchemaDoc = match serde_json::from_str(&raw) {
        Ok(v) => v,
        Err(e) => {
            let b = quarantine(&schema_path).into_iter().collect();
            return LoadOutcome::Corrupt {
                backed_up: b,
                reason: format!("schema.json が壊れています: {e}"),
            };
        }
    };
    if schema.version > SCHEMA_VERSION {
        // **消さない。** 新しい版の Zaivern が書いたものかもしれない。
        return LoadOutcome::Newer {
            found: schema.version,
        };
    }

    macro_rules! read_json {
        ($name:literal, $ty:ty) => {{
            let p = dir.join($name);
            let Some(raw) = read_capped(&p) else {
                let b = quarantine(&p).into_iter().collect();
                return LoadOutcome::Corrupt {
                    backed_up: b,
                    reason: concat!($name, " を読めません").into(),
                };
            };
            match serde_json::from_str::<$ty>(&raw) {
                Ok(v) => v,
                Err(e) => {
                    let b = quarantine(&p).into_iter().collect();
                    return LoadOutcome::Corrupt {
                        backed_up: b,
                        reason: format!("{} が壊れています: {e}", $name),
                    };
                }
            }
        }};
    }

    let mut run = read_json!("run.json", RunDoc);
    // 旧版で保存されたものをここで引き取る (読めたものは必ず今の形にする)。
    run.migrate();
    let goal = read_json!("goal.json", TeamGoal);
    let teams = read_json!("teams.json", Vec<TeamGroup>);
    let tasks = read_json!("tasks.json", Vec<TeamTask>);
    let agents = read_json!("agents.json", Vec<TeamAgent>);
    let decisions = read_json!("decisions.json", Vec<Decision>);

    // イベントログは 1 行ずつ。**壊れた行は捨てるが、捨てたことを黙らない**
    // — ただしファイル全体を壊れ扱いにはしない (追記専用なので末尾が
    // 切れているのはあり得る)。
    let mut events = Vec::new();
    if let Some(raw) = read_capped(&dir.join("events.jsonl")) {
        for line in raw.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(ev) = serde_json::from_str::<TeamEvent>(line) {
                events.push(ev);
            }
        }
    }

    LoadOutcome::Loaded(Box::new(Saved {
        run,
        goal,
        teams,
        tasks,
        agents,
        decisions,
        events,
    }))
}

/// 保存済みの Run があるか (再起動時の「未完了 Run」検出)。
pub fn has_run(dir: &Path) -> bool {
    dir.join("run.json").exists() && dir.join("schema.json").exists()
}

/// 消す対象の一覧 (`zai team reset --dry-run` が出す)。
pub fn reset_targets(dir: &Path) -> Vec<PathBuf> {
    const NAMES: &[&str] = &[
        "schema.json",
        "run.json",
        "goal.json",
        "teams.json",
        "tasks.json",
        "agents.json",
        "decisions.json",
        "events.jsonl",
    ];
    let mut out: Vec<PathBuf> = NAMES
        .iter()
        .map(|n| dir.join(n))
        .filter(|p| p.exists())
        .collect();
    // 退避済みの壊れファイルも掃除対象に含める (自分が作ったものだけ)。
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut extra: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(|n| n.contains(".corrupt-"))
            })
            .collect();
        extra.sort();
        out.extend(extra);
    }
    out
}

/// 実際に消す。**呼び出し側が明示確認を取ってから呼ぶこと。**
pub fn reset(dir: &Path) -> Result<usize, SaveError> {
    let targets = reset_targets(dir);
    let mut n = 0;
    for p in targets {
        std::fs::remove_file(&p).map_err(|e| SaveError::Io(e.to_string()))?;
        n += 1;
    }
    Ok(n)
}

/// `serde::Serialize` を型消去して `save` の中の重複を減らすための小道具。
mod erased {
    pub trait Ser {
        fn to_json(&self) -> Result<String, String>;
    }
    impl<T: serde::Serialize> Ser for T {
        fn to_json(&self) -> Result<String, String> {
            serde_json::to_string_pretty(self).map_err(|e| e.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::testkit::{goal as mkgoal, task};
    use super::*;

    fn saved() -> Saved {
        Saved {
            run: RunDoc {
                version: SCHEMA_VERSION,
                run_id: "run-1".into(),
                workspace: "ws".into(),
                spec_source: "SPEC.md".into(),
                agent_count: 4,
                max_attempts: 3,
                review_required: true,
                paused: false,
                stopped: false,
                started_at: 100,
                updated_at: 100,
                approved_validation: Vec::new(),
                validation_timeout_secs: default_validation_timeout(),
                effects: vec![EffectRecord {
                    key: "start:1".into(),
                    state: EffectState::Completed,
                    at: 100,
                }],
                done_effects: Vec::new(),
            },
            goal: mkgoal(),
            teams: super::super::plan_schema::default_lanes(),
            tasks: vec![task(1, "a", &[])],
            agents: Vec::new(),
            decisions: Vec::new(),
            events: Vec::new(),
        }
    }

    fn tmp(name: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-team-persist", name)
    }

    #[test]
    fn 保存して読み戻せる() {
        let dir = tmp("roundtrip");
        let s = saved();
        save(&dir, &s).expect("保存できるべき");
        match load(&dir) {
            LoadOutcome::Loaded(got) => {
                assert_eq!(got.run, s.run);
                assert_eq!(got.tasks, s.tasks);
                assert_eq!(got.goal, s.goal);
            }
            other => panic!("読めなかった: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 旧版のdone_effectsを成功済みとして引き取る() {
        let mut run = saved().run;
        run.effects.clear();
        run.done_effects = vec!["start:agent-1".into(), "instr:1".into()];
        run.migrate();
        assert!(run.done_effects.is_empty(), "旧欄を残したままにしない");
        assert_eq!(run.effects.len(), 2);
        assert!(run.effects.iter().all(|e| e.state == EffectState::Completed));
        // 2 度呼んでも増えない
        run.migrate();
        assert_eq!(run.effects.len(), 2);
    }

    #[test]
    fn 何も無ければemptyを返す() {
        let dir = tmp("empty");
        assert_eq!(load(&dir), LoadOutcome::Empty);
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(load(&dir), LoadOutcome::Empty);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 壊れたファイルを黙って初期化しない() {
        let dir = tmp("corrupt");
        save(&dir, &saved()).unwrap();
        std::fs::write(dir.join("tasks.json"), "{ これは JSON ではない").unwrap();
        match load(&dir) {
            LoadOutcome::Corrupt { backed_up, reason } => {
                assert!(!backed_up.is_empty(), "退避していない");
                assert!(reason.contains("tasks.json"), "{reason}");
                // 退避先が実在する = 中身を捨てていない
                assert!(dir.join(&backed_up[0]).exists());
            }
            other => panic!("壊れを検出できなかった: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 新しい版は読まずに残す() {
        let dir = tmp("newer");
        save(&dir, &saved()).unwrap();
        std::fs::write(
            dir.join("schema.json"),
            format!("{{\"version\":{}}}", SCHEMA_VERSION + 1),
        )
        .unwrap();
        assert_eq!(
            load(&dir),
            LoadOutcome::Newer {
                found: SCHEMA_VERSION + 1
            }
        );
        // 消していない
        assert!(dir.join("tasks.json").exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn イベントログは上限を超えない() {
        let dir = tmp("events");
        let events: Vec<TeamEvent> = (0..EVENT_LOG_MAX_LINES + 100)
            .map(|i| TeamEvent {
                id: i as u64,
                at: 1,
                kind: super::super::model::TeamEventKind::TaskReady,
                actor: None,
                target: None,
                task_id: None,
                summary: format!("e{i}"),
            })
            .collect();
        save_events(&dir, &events).unwrap();
        let raw = std::fs::read_to_string(dir.join("events.jsonl")).unwrap();
        assert_eq!(raw.lines().count(), EVENT_LOG_MAX_LINES);
        // 残るのは新しい方
        assert!(raw.contains(&format!("e{}", EVENT_LOG_MAX_LINES + 99)));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn イベントログの壊れた行は飛ばす() {
        let dir = tmp("events-broken");
        save(&dir, &saved()).unwrap();
        let good = serde_json::to_string(&TeamEvent {
            id: 1,
            at: 2,
            kind: super::super::model::TeamEventKind::RunStarted,
            actor: None,
            target: None,
            task_id: None,
            summary: "ok".into(),
        })
        .unwrap();
        std::fs::write(
            dir.join("events.jsonl"),
            format!("{good}\nこれは壊れた行\n{good}\n"),
        )
        .unwrap();
        match load(&dir) {
            LoadOutcome::Loaded(s) => assert_eq!(s.events.len(), 2),
            other => panic!("{other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 原子的置き換えは途中の失敗で元を壊さない() {
        let dir = tmp("atomic");
        save(&dir, &saved()).unwrap();
        let before = std::fs::read_to_string(dir.join("tasks.json")).unwrap();
        // 一時ファイルは残さない
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "一時ファイルが残っている");
        save(&dir, &saved()).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("tasks.json")).unwrap(),
            before
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resetは対象を出してから消す() {
        let dir = tmp("reset");
        save(&dir, &saved()).unwrap();
        let targets = reset_targets(&dir);
        assert!(targets.len() >= 8, "{targets:?}");
        assert!(has_run(&dir));
        let n = reset(&dir).unwrap();
        assert_eq!(n, targets.len());
        assert!(!has_run(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 置き場はワークスペースキーから決まる() {
        // **`team_dir` は実 `~/.zaivern` を指すので、ここでは触らずに
        // パスの導出だけを見る** (ディレクトリを 1 つも作らない)。
        let root = Path::new("/nonexistent-root-for-path-derivation");
        let a = team_dir_in(root, Path::new("/tmp/ws-a"));
        let b = team_dir_in(root, Path::new("/tmp/ws-b"));
        assert_ne!(a, b);
        assert!(a.ends_with(crate::history::workspace_key(Path::new("/tmp/ws-a"))));
        // 同じワークスペースなら何度呼んでも同じ
        assert_eq!(a, team_dir_in(root, Path::new("/tmp/ws-a")));
        // 既定の根も同じ導出を通る (根が違うだけ)
        assert!(team_dir_in(&default_home(), Path::new("/tmp/ws-a"))
            .ends_with(crate::history::workspace_key(Path::new("/tmp/ws-a"))));
    }
}
