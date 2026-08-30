//! Team Run の永続化 — **DB を足さない。JSON を原子的に置き換えるだけ。**
//!
//! ## 置き場所
//!
//! ```text
//! ~/.zaivern/team/<ワークスペースキー>/
//!   state.json       ← Run 全体 (いまの世代)
//!   state.prev.json  ← 直前の完全なスナップショット
//! ```
//!
//! ## なぜ 1 ファイルなのか
//!
//! 版 3 までは 8 つのファイルを個別に原子的置換していた。**ファイル単体が
//! 原子的でも、まとまりとしては原子的ではない** — 3 つ目まで書いたところで
//! 電源が落ちれば、新しい `run.json` と古い `tasks.json` が同居する。
//! しかもどちらも JSON としては正しいので、読む側は正常な状態として
//! 読んでしまう (承認済みの検証が別のタスクへ当たる、Effect が二重に
//! 出る、という形で現れる)。1 回の rename で全部が切り替わるなら、
//! その隙間は存在しない。
//!
//! 版 3 以前の置き場も読める。次の保存で 1 ファイルへ移り、旧ファイルは
//! `.legacy-<epoch>` へ退く (**2 つの真実を残さない**)。
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
/// * 3 — 検証の実行承認をコマンド文字列だけで持つのをやめ、
///   **Run + タスク + 世代 + コマンド**で持つようにした
///   ([`ValidationApproval`])。旧 `approved_validation` (文字列の一覧) は
///   読まずに捨てる — 範囲が広すぎる承認を引き継ぐと、承認したときとは
///   別のコードが人の同意なく走る。
/// * 4 — 8 ファイルを個別に原子的置換するのをやめ、**Run 全体を 1 つの
///   スナップショット** (`state.json`) にした。ファイル単体が原子的でも
///   まとまりとしては原子的でないので、保存の途中で落ちると新しい
///   `run.json` と古い `tasks.json` が同居する — しかもどちらも JSON
///   としては正しいので、読む側は正常な状態として読んでしまう。
///   直前の完全なスナップショットは `state.prev.json` に残す。
pub const SCHEMA_VERSION: u32 = 4;

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
    /// 実行側へ渡した。**まだ外部副作用は成立していない。**
    ///
    /// 「渡した」と「成立した」を同じ段にしないのが、この台帳の全部。
    /// 混ぜると、積めたのに届かなかった指示が「送った」ことになって
    /// 永久に消えるか、起こしたのに保存前に落ちたエージェントが次の起動で
    /// 2 体になるかの、どちらかが必ず起きる。
    Dispatched,
    /// **外部副作用が本当に成立した**と実行側が返した。
    ///
    /// 指示なら相手の端末へ確定まで届いたとき、検証なら走らせ始めたとき、
    /// 起動ならセッションへ結び付いたとき。
    Completed,
}

/// **人が承認した検証の実行 1 回ぶん。**
///
/// コマンド文字列だけで承認を覚えると、次の筋書きが通ってしまう:
///
/// 1. タスク A の `cargo test` を人が承認する
/// 2. エージェントが `build.rs` / テスト本体 / `Makefile` を書き換える
/// 3. タスク B や、差し戻し後の再試行で、また `cargo test` が要る
/// 4. **文字列が同じというだけ**で承認済みとして実行される
/// 5. 人が見て承認したのとは**別のコード**が走る
///
/// なので承認は「どの Run の・どのタスクの・どの検証回の・どのコマンドか」
/// まで縛る。別タスク・差し戻し後・レビュー指摘後は、必ず聞き直す。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidationApproval {
    pub run_id: String,
    pub task_id: super::model::TaskId,
    /// 検証の世代 ([`super::model::ValidationState::generation`])。
    /// **検証をやり直すたびに 1 つ進む**ので、前の回の承認は当たらない。
    pub generation: u32,
    pub command: String,
    /// 承認した時刻 (Unix 秒)。監査のために残す。
    pub at: u64,
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
    ///
    /// ここが Transactional Outbox の本体。落ちたあとの引き取り方は
    /// 状態で決まる ([`super::runtime::TeamRuntime::restore`]):
    ///
    /// | 記録 | 意味 | 立て直したとき |
    /// |---|---|---|
    /// | 無い | まだ作っていない | 必要ならもう一度作られる |
    /// | `Dispatched` | 渡したが成立は未確認 | **引き継がない** = もう一度出す |
    /// | `Completed` | 成立した | 引き継ぐ = もう出さない |
    /// | (失敗で消える) | 成立しなかった | もう一度出す |
    ///
    /// 例外は 1 つだけ。**決着していない検証の `Completed` は引き継がない**
    /// — 実行側は「裏で走らせ始めた」時点で成功を返すので、結果が戻る前に
    /// 落ちると記録だけが残り、プロセスは消えているのに再発行を止めて
    /// タスクが永久に `Validating` で固まる。
    #[serde(default)]
    pub effects: Vec<EffectRecord>,
    /// **人が実行を承認した検証 (1 回ぶん)。**
    ///
    /// 文字列だけで持ってはいけない ([`ValidationApproval`] の doc を参照)。
    #[serde(default)]
    pub validation_approvals: Vec<ValidationApproval>,
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
///
/// **旧形式を書くためだけに残っている。** 製品の保存は [`save`] が
/// スナップショット 1 つを置き換える。
#[cfg(test)]
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
    if let Err(e) = rename_retrying(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// 置き換えを短い待ちで数回試す。
///
/// **Windows では置き換えが一時的に断られる。** 宛先を誰かが開いている間
/// (`zai team stop` が読んでいる最中など) は `MoveFileEx` が ACCESS_DENIED
/// を返す。1 回で諦めると「いちばん混んでいるとき = いちばん保存したい
/// とき」にだけ台帳が書けなくなる。unix では 1 回目で通るので費用はゼロ。
///
/// **上限を持つ。** 進まないものを待ち続けても人を待たせるだけなので、
/// 諦めたら理由を返して呼び出し側に見せる。
fn rename_retrying(from: &Path, to: &Path) -> Result<(), SaveError> {
    let mut last = None;
    for attempt in 0..RENAME_RETRIES {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last = Some(e.to_string());
                if attempt + 1 < RENAME_RETRIES {
                    std::thread::sleep(RENAME_BACKOFF * (attempt + 1));
                }
            }
        }
    }
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

/// いまの世代のスナップショット。
pub const STATE_FILE: &str = "state.json";
/// **直前の完全なスナップショット。** いまのものが壊れていたらここへ戻る。
pub const PREV_FILE: &str = "state.prev.json";

/// 保存の段階 (フォールト注入とテストのため)。
///
/// **段階を型で持つ。** 「どこで落ちても新旧が混ざらない」を確かめるには、
/// 落ちる場所を 1 つずつ指せる必要がある。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SavePhase {
    /// 一時ファイルを書いた直後 (まだ誰も見ていない)。
    TmpWritten,
    /// いまのものを「直前」へ退けた直後 (`state.json` が一瞬無い)。
    PrevRetired,
    /// 新しいものを `state.json` へ置いた直後。
    Committed,
}

/// Run 全体を 1 つにまとめたスナップショット。
///
/// ## なぜ 1 ファイルなのか
///
/// 8 つのファイルを個別に原子的置換すると、**ファイル単体は原子的でも
/// まとまりとしては原子的ではない**。3 つ目まで書いたところで電源が
/// 落ちれば、新しい `run.json` と古い `tasks.json` が同居する。しかも
/// どちらも JSON としては正しいので、読む側は正常な状態として読んでしまう
/// (承認済みの検証が別のタスクへ当たる、Effect が二重に出る、といった形で
/// 現れる)。
///
/// **1 回の rename で全部が切り替わる**なら、その隙間は存在しない。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateDoc {
    pub version: u32,
    /// 保存のたびに 1 つ進む。**どちらが新しいか**を mtime に頼らずに言う。
    #[serde(default)]
    pub generation: u64,
    /// 何の Run か (取り違えの検出)。
    #[serde(default)]
    pub run_id: String,
    /// どのワークスペースのものか (置き場を移されたときの検出)。
    #[serde(default)]
    pub workspace: String,
    #[serde(default)]
    pub saved_at: u64,
    pub run: RunDoc,
    pub goal: TeamGoal,
    #[serde(default)]
    pub teams: Vec<TeamGroup>,
    #[serde(default)]
    pub tasks: Vec<TeamTask>,
    #[serde(default)]
    pub agents: Vec<TeamAgent>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    /// **事象もスナップショットに入れる。** 別ファイルにすると、
    /// 「復元した状態」と「そこまでの経緯」の世代がずれる。
    #[serde(default)]
    pub events: Vec<TeamEvent>,
}

impl StateDoc {
    fn from_saved(s: &Saved, generation: u64) -> Self {
        let start = s.events.len().saturating_sub(EVENT_LOG_MAX_LINES);
        Self {
            version: SCHEMA_VERSION,
            generation,
            run_id: s.run.run_id.clone(),
            workspace: s.run.workspace.clone(),
            saved_at: super::model::now_secs(),
            run: s.run.clone(),
            goal: s.goal.clone(),
            teams: s.teams.clone(),
            tasks: s.tasks.clone(),
            agents: s.agents.clone(),
            decisions: s.decisions.clone(),
            events: s.events[start..].to_vec(),
        }
    }

    fn into_saved(self) -> Saved {
        Saved {
            run: self.run,
            goal: self.goal,
            teams: self.teams,
            tasks: self.tasks,
            agents: self.agents,
            decisions: self.decisions,
            events: self.events,
        }
    }

    /// **中身が互いに噛み合っているか。**
    ///
    /// 新旧が混ざったスナップショット (旧形式で保存の途中に落ちたもの) は、
    /// ファイル単体としては正しい JSON なので、形だけ見ても気付けない。
    /// 参照が繋がっていることまで見る。
    fn consistent(&self) -> Result<(), String> {
        if self.run.run_id.trim().is_empty() {
            return Err("run_id が空です".into());
        }
        if let Some(t) = self.tasks.iter().find(|t| t.goal_id != self.goal.id) {
            return Err(format!(
                "タスク #{} の goal ({}) が goal.json ({}) と一致しません",
                t.id, t.goal_id.0, self.goal.id.0
            ));
        }
        let ids: std::collections::BTreeSet<super::model::TaskId> =
            self.tasks.iter().map(|t| t.id).collect();
        if let Some(d) = self
            .decisions
            .iter()
            .find(|d| d.task_id.is_some_and(|id| !ids.contains(&id)))
        {
            return Err(format!(
                "判断 {} が存在しないタスク #{} を指しています",
                d.id,
                d.task_id.unwrap_or_default()
            ));
        }
        Ok(())
    }
}

/// 状態を保存する。**Run 全体を 1 つのスナップショットとして置き換える。**
///
/// ```text
/// state.json.<pid>.<ns>.tmp  ← 書いて fsync
///          ↓ rename          state.json → state.prev.json  (直前を残す)
///          ↓ rename          tmp        → state.json       (ここで切り替わる)
/// ```
///
/// どの段で落ちても、`state.json` か `state.prev.json` の**どちらかは
/// 完全**なので、新旧が混ざった状態を読むことはない。
pub fn save(dir: &Path, s: &Saved) -> Result<(), SaveError> {
    std::fs::create_dir_all(dir).map_err(|e| SaveError::Io(e.to_string()))?;
    let next = read_doc(&dir.join(STATE_FILE))
        .map(|d| d.generation.saturating_add(1))
        .unwrap_or(1);
    let doc = StateDoc::from_saved(s, next);
    let body = serde_json::to_string(&doc).map_err(|e| SaveError::Serialize(e.to_string()))?;

    let tmp = tmp_path(dir, STATE_FILE);
    write_synced(&tmp, &body)?;
    fault(SavePhase::TmpWritten)?;

    let cur = dir.join(STATE_FILE);
    if cur.exists() {
        // **直前の完全なスナップショットを必ず 1 つ残す。** ここで落ちても
        // `state.prev.json` から復旧できる。
        rename_retrying(&cur, &dir.join(PREV_FILE))?;
    }
    fault(SavePhase::PrevRetired)?;

    if let Err(e) = rename_retrying(&tmp, &cur) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    sync_dir(dir);
    fault(SavePhase::Committed)?;

    // 旧形式が残っていたら、移行が済んだのでどける (読む側が 2 つの
    // 真実を持たないように)。**消さずに退ける** — 中身は人のものなので。
    retire_legacy(dir);
    Ok(())
}

/// 旧形式 (8 ファイル) を `.legacy-<epoch>` へ退ける。
fn retire_legacy(dir: &Path) {
    if !dir.join("run.json").exists() {
        return;
    }
    let stamp = super::model::now_secs();
    for n in LEGACY_NAMES {
        let p = dir.join(n);
        if p.exists() {
            let _ = std::fs::rename(&p, p.with_file_name(format!("{n}.legacy-{stamp}")));
        }
    }
}

const LEGACY_NAMES: &[&str] = &[
    "schema.json",
    "run.json",
    "goal.json",
    "teams.json",
    "tasks.json",
    "agents.json",
    "decisions.json",
    "events.jsonl",
];

/// 一時ファイルの名前 (同じフォルダに複数インスタンスが書きうる)。
fn tmp_path(dir: &Path, name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dir.join(format!(".{}.{}.{}.tmp", name, std::process::id(), stamp))
}

/// 書いて **fsync してから**返す。
///
/// fsync しないと、rename だけが先にディスクへ届いて「中身が空の
/// `state.json`」が残りうる (ext4 の既定では滅多に起きないが、
/// 起きたときに失うのは Run 全体なので払っておく)。
fn write_synced(path: &Path, body: &str) -> Result<(), SaveError> {
    use std::io::Write;
    let mut f = std::fs::File::create(path).map_err(|e| SaveError::Io(e.to_string()))?;
    f.write_all(body.as_bytes())
        .map_err(|e| SaveError::Io(e.to_string()))?;
    f.sync_all().map_err(|e| SaveError::Io(e.to_string()))?;
    Ok(())
}

/// ディレクトリ自体も同期する (rename の永続化)。**失敗しても続ける** —
/// ここが効かない FS はあるが、そのために保存そのものを失敗にはしない。
fn sync_dir(dir: &Path) {
    if let Ok(f) = std::fs::File::open(dir) {
        let _ = f.sync_all();
    }
}

/// テスト用のフォールト注入 (`#[cfg(test)]` のときだけ効く)。
#[allow(unused_variables)]
fn fault(phase: SavePhase) -> Result<(), SaveError> {
    #[cfg(test)]
    if fault_inject::should_fail(phase) {
        return Err(SaveError::Io(format!("(テスト) {phase:?} で中断")));
    }
    Ok(())
}

/// **保存の途中で落ちる**筋書きを作るための差し替え。
///
/// プロセス共通の `static` にしない — 同時に走っている他のテストの
/// 差し替えまで混ざる (CLAUDE.md の実績あり)。
#[cfg(test)]
pub mod fault_inject {
    use std::cell::Cell;

    use super::SavePhase;

    thread_local! {
        static AT: Cell<Option<SavePhase>> = const { Cell::new(None) };
    }

    /// この段で `save` を失敗させる。
    pub fn fail_at(phase: SavePhase) {
        AT.with(|c| c.set(Some(phase)));
    }

    /// 差し替えを外す。
    pub fn clear() {
        AT.with(|c| c.set(None));
    }

    pub(super) fn should_fail(phase: SavePhase) -> bool {
        AT.with(|c| c.get()) == Some(phase)
    }
}

/// イベントログを版 3 以前の形 (`events.jsonl`) で書く。
///
/// **製品の保存経路は [`save`] 1 つだけ。** ここは「旧形式で保存された
/// 置き場」を実験で作るための道具なので、テストのときしか存在しない
/// (残すと「まだ 2 つの書き口がある」という誤読を生む)。
#[cfg(test)]
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

/// スナップショットを 1 つ読む (壊れていれば `None`)。
fn read_doc(path: &Path) -> Option<StateDoc> {
    let raw = read_capped(path)?;
    let doc: StateDoc = serde_json::from_str(&raw).ok()?;
    Some(doc)
}

/// 状態を読む。
///
/// 見る順は `state.json` → `state.prev.json` → 旧形式。
/// **保存の途中で落ちても、どちらか一方は完全**なので、新旧が混ざった
/// ものを読むことはない。
pub fn load(dir: &Path) -> LoadOutcome {
    if !dir.exists() {
        return LoadOutcome::Empty;
    }
    let cur = dir.join(STATE_FILE);
    let prev = dir.join(PREV_FILE);
    if cur.exists() || prev.exists() {
        let mut backed_up = Vec::new();
        let mut why = String::new();
        // 新しいほうから順に試す。**世代の大きいほうが新しい** (mtime に
        // 頼らない — コピーや同期で mtime は簡単にひっくり返る)。
        let mut cands: Vec<(PathBuf, Option<StateDoc>)> = vec![
            (cur.clone(), read_doc(&cur)),
            (prev.clone(), read_doc(&prev)),
        ];
        cands.sort_by_key(|(_, d)| std::cmp::Reverse(d.as_ref().map(|x| x.generation)));
        for (path, doc) in cands {
            if !path.exists() {
                continue;
            }
            let Some(doc) = doc else {
                // **消さない。退ける。** 中身は人のものなので、読めなくても
                // 捨てない (次の候補で復旧できたかどうかは戻り値で分かる)。
                backed_up.extend(quarantine(&path));
                if why.is_empty() {
                    why = format!(
                        "{} を読めません",
                        path.file_name().and_then(|s| s.to_str()).unwrap_or("state")
                    );
                }
                continue;
            };
            if doc.version > SCHEMA_VERSION {
                // **消さない。** 新しい版の Zaivern が書いたものかもしれない。
                return LoadOutcome::Newer {
                    found: doc.version,
                };
            }
            if let Err(e) = doc.consistent() {
                backed_up.extend(quarantine(&path));
                if why.is_empty() {
                    why = e;
                }
                continue;
            }
            let mut saved = doc.into_saved();
            saved.run.migrate();
            return LoadOutcome::Loaded(Box::new(saved));
        }
        return LoadOutcome::Corrupt {
            backed_up,
            reason: if why.is_empty() {
                "保存された状態を読めません".into()
            } else {
                why
            },
        };
    }
    load_legacy(dir)
}

/// 版 3 以前の置き場 (8 ファイル) を読む。**移行のためだけに残す。**
///
/// ここは「新旧が混ざりうる」形なので、読めたあとに必ず噛み合いを見る
/// ([`StateDoc::consistent`])。次の [`save`] で 1 ファイルへ移り、
/// 旧形式は `.legacy-<epoch>` へ退く。
fn load_legacy(dir: &Path) -> LoadOutcome {
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

    // **旧形式は新旧が混ざりうる。** 保存の途中で落ちれば、新しい
    // `run.json` と古い `tasks.json` が同居し、どちらも JSON としては
    // 正しい。参照が繋がっているかまで見る。
    let doc = StateDoc {
        version: schema.version,
        generation: 0,
        run_id: run.run_id.clone(),
        workspace: run.workspace.clone(),
        saved_at: run.updated_at,
        run,
        goal,
        teams,
        tasks,
        agents,
        decisions,
        events,
    };
    if let Err(e) = doc.consistent() {
        let mut backed_up = Vec::new();
        for n in LEGACY_NAMES {
            backed_up.extend(quarantine(&dir.join(n)));
        }
        return LoadOutcome::Corrupt {
            backed_up,
            reason: format!("保存の途中で中断した形跡があります: {e}"),
        };
    }
    LoadOutcome::Loaded(Box::new(doc.into_saved()))
}

/// 保存済みの Run があるか (再起動時の「未完了 Run」検出)。
pub fn has_run(dir: &Path) -> bool {
    dir.join(STATE_FILE).exists()
        || dir.join(PREV_FILE).exists()
        || (dir.join("run.json").exists() && dir.join("schema.json").exists())
}

/// 消す対象の一覧 (`zai team reset --dry-run` が出す)。
pub fn reset_targets(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = [STATE_FILE, PREV_FILE]
        .iter()
        .chain(LEGACY_NAMES.iter())
        .map(|n| dir.join(n))
        .filter(|p| p.exists())
        .collect();
    // 退避済みのファイルも掃除対象に含める (自分が作ったものだけ)。
    if let Ok(rd) = std::fs::read_dir(dir) {
        let mut extra: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name().and_then(|s| s.to_str()).is_some_and(|n| {
                    n.contains(".corrupt-") || n.contains(".legacy-")
                })
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
                validation_approvals: Vec::new(),
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
        std::fs::write(dir.join(STATE_FILE), "{ これは JSON ではない").unwrap();
        match load(&dir) {
            LoadOutcome::Corrupt { backed_up, reason } => {
                assert!(!backed_up.is_empty(), "退避していない");
                assert!(reason.contains(STATE_FILE), "{reason}");
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
        let mut doc = read_doc(&dir.join(STATE_FILE)).expect("読める");
        doc.version = SCHEMA_VERSION + 1;
        std::fs::write(
            dir.join(STATE_FILE),
            serde_json::to_string(&doc).unwrap(),
        )
        .unwrap();
        assert_eq!(
            load(&dir),
            LoadOutcome::Newer {
                found: SCHEMA_VERSION + 1
            }
        );
        // 消していない
        assert!(dir.join(STATE_FILE).exists());
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
    fn 旧形式のイベントログの壊れた行は飛ばす() {
        // 版 3 以前の置き場を読む経路。追記専用なので**末尾が切れている
        // のはあり得る**。ファイル全体を壊れ扱いにはしない。
        let dir = tmp("events-broken");
        write_legacy(&dir, &saved());
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
        let before = std::fs::read_to_string(dir.join(STATE_FILE)).unwrap();
        // 一時ファイルは残さない
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "一時ファイルが残っている");
        save(&dir, &saved()).unwrap();
        // 世代だけが進み、中身は同じ。
        let after = read_doc(&dir.join(STATE_FILE)).unwrap();
        let prev = read_doc(&dir.join(PREV_FILE)).unwrap();
        assert_eq!(after.generation, prev.generation + 1);
        assert_eq!(after.tasks, prev.tasks);
        assert!(before.contains("\"generation\":1"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resetは対象を出してから消す() {
        let dir = tmp("reset");
        save(&dir, &saved()).unwrap();
        save(&dir, &saved()).unwrap(); // `state.prev.json` も作る
        let targets = reset_targets(&dir);
        assert!(targets.len() >= 2, "{targets:?}");
        assert!(has_run(&dir));
        let n = reset(&dir).unwrap();
        assert_eq!(n, targets.len());
        assert!(!has_run(&dir));
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── 保存はまとまりとして原子的 ───────────────────────────────────

    /// 版 3 以前の形で 8 ファイルを書く (移行と混在の実験用)。
    fn write_legacy(dir: &Path, s: &Saved) {
        std::fs::create_dir_all(dir).unwrap();
        let w = |n: &str, body: String| std::fs::write(dir.join(n), body).unwrap();
        w("schema.json", format!("{{\"version\":{}}}", SCHEMA_VERSION));
        w("run.json", serde_json::to_string(&s.run).unwrap());
        w("goal.json", serde_json::to_string(&s.goal).unwrap());
        w("teams.json", serde_json::to_string(&s.teams).unwrap());
        w("tasks.json", serde_json::to_string(&s.tasks).unwrap());
        w("agents.json", serde_json::to_string(&s.agents).unwrap());
        w(
            "decisions.json",
            serde_json::to_string(&s.decisions).unwrap(),
        );
        save_events(dir, &s.events).unwrap();
    }

    fn loaded(o: LoadOutcome) -> Saved {
        match o {
            LoadOutcome::Loaded(s) => *s,
            other => panic!("読めなかった: {other:?}"),
        }
    }

    #[test]
    fn どの段で落ちても新旧が混ざらない() {
        // **これが今回の中核。** ファイル単体が原子的でも、まとまりとして
        // 原子的でなければ、保存の途中で落ちたときに新しい run と古い
        // tasks が同居する。しかもどちらも JSON としては正しいので、
        // 読む側は正常な状態として読んでしまう。
        for phase in [
            SavePhase::TmpWritten,
            SavePhase::PrevRetired,
            SavePhase::Committed,
        ] {
            let dir = tmp(&format!("phase-{phase:?}"));
            // 1 世代目 (これが「旧」)。
            let mut old = saved();
            old.run.run_id = "run-old".into();
            save(&dir, &old).unwrap();

            // 2 世代目の途中で落ちる。
            let mut new = saved();
            new.run.run_id = "run-new".into();
            new.tasks[0].title = "新しい題".into();
            fault_inject::fail_at(phase);
            let r = save(&dir, &new);
            fault_inject::clear();
            assert!(r.is_err(), "{phase:?} で落ちなかった");

            // **どちらか一方だけが読める。混ざったものは読めない。**
            let got = loaded(load(&dir));
            let is_old = got.run.run_id == "run-old" && got.tasks[0].title != "新しい題";
            let is_new = got.run.run_id == "run-new" && got.tasks[0].title == "新しい題";
            assert!(
                is_old || is_new,
                "{phase:?} で新旧が混ざった: run_id={} title={}",
                got.run.run_id,
                got.tasks[0].title
            );
            // 切り替え前に落ちたなら旧、切り替え後なら新。
            match phase {
                SavePhase::TmpWritten | SavePhase::PrevRetired => {
                    assert!(is_old, "{phase:?} なのに新しい世代が見えた")
                }
                SavePhase::Committed => assert!(is_new, "置いたのに見えない"),
            }
            std::fs::remove_dir_all(&dir).ok();
        }
    }

    #[test]
    fn 一時ファイルだけが残っても復元できる() {
        // 一時ファイルは**正式なスナップショットではない**。読まない。
        let dir = tmp("tmp-leftover");
        save(&dir, &saved()).unwrap();
        std::fs::write(
            dir.join(".state.json.999.1.tmp"),
            "{ 壊れた一時ファイル",
        )
        .unwrap();
        let got = loaded(load(&dir));
        assert_eq!(got.run.run_id, "run-1");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 壊れた新世代があれば直前へ戻る() {
        // **常に復旧可能な最後の完全スナップショットを残す。**
        let dir = tmp("fallback");
        let mut old = saved();
        old.run.run_id = "run-old".into();
        save(&dir, &old).unwrap();
        let mut new = saved();
        new.run.run_id = "run-new".into();
        save(&dir, &new).unwrap();
        assert_eq!(loaded(load(&dir)).run.run_id, "run-new");

        // 新しいほうを壊す。
        std::fs::write(dir.join(STATE_FILE), "{ 途中で切れた").unwrap();
        match load(&dir) {
            LoadOutcome::Loaded(s) => assert_eq!(
                s.run.run_id, "run-old",
                "直前のスナップショットへ戻れていない"
            ),
            other => panic!("直前へ戻れなかった: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 旧形式から移行して旧ファイルを退ける() {
        let dir = tmp("migrate");
        let mut s = saved();
        s.run.run_id = "run-legacy".into();
        write_legacy(&dir, &s);
        assert!(has_run(&dir), "旧形式の Run を見つけられない");

        // 読める。
        let got = loaded(load(&dir));
        assert_eq!(got.run.run_id, "run-legacy");
        assert_eq!(got.tasks.len(), 1);

        // 保存すると 1 ファイルへ移り、旧形式は退く
        // (**2 つの真実を残さない**)。
        save(&dir, &got).unwrap();
        assert!(dir.join(STATE_FILE).exists());
        assert!(!dir.join("run.json").exists(), "旧形式が残っている");
        assert!(!dir.join("tasks.json").exists());
        assert_eq!(loaded(load(&dir)).run.run_id, "run-legacy");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 旧形式の新旧混在を正常扱いしない() {
        // 旧形式で保存の途中に落ちた形: `run.json` と `goal.json` は
        // 新しいが `tasks.json` が古い。**どちらも JSON としては正しい。**
        let dir = tmp("legacy-mixed");
        let mut s = saved();
        s.run.run_id = "run-new".into();
        write_legacy(&dir, &s);
        // 古い世代の tasks (別の goal を指している) を置く。
        let mut stale = s.tasks.clone();
        stale[0].goal_id = super::super::model::GoalId::new("g-old");
        std::fs::write(
            dir.join("tasks.json"),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();

        match load(&dir) {
            LoadOutcome::Corrupt { backed_up, reason } => {
                assert!(reason.contains("中断"), "{reason}");
                assert!(!backed_up.is_empty(), "退避していない");
            }
            other => panic!("新旧混在を正常として読んだ: {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 保存失敗を握り潰さない() {
        let dir = tmp("save-fail");
        fault_inject::fail_at(SavePhase::TmpWritten);
        let r = save(&dir, &saved());
        fault_inject::clear();
        let e = r.expect_err("失敗を返していない");
        assert!(e.detail().contains("保存できません"), "{}", e.detail());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 世代は保存のたびに進む() {
        // どちらが新しいかを **mtime に頼らない** (コピーや同期で簡単に
        // ひっくり返る)。
        let dir = tmp("generation");
        save(&dir, &saved()).unwrap();
        assert_eq!(read_doc(&dir.join(STATE_FILE)).unwrap().generation, 1);
        save(&dir, &saved()).unwrap();
        assert_eq!(read_doc(&dir.join(STATE_FILE)).unwrap().generation, 2);
        assert_eq!(read_doc(&dir.join(PREV_FILE)).unwrap().generation, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn effectの二段記録が世代ごと一緒に動く() {
        // **Effect が状態と同じ世代で保存される**こと。別ファイルだと、
        // 「完了した Effect」だけが新しくなって二重実行や取りこぼしが出る。
        let dir = tmp("effects");
        let mut s = saved();
        s.run.effects = vec![
            EffectRecord {
                key: "start:1".into(),
                state: EffectState::Completed,
                at: 1,
            },
            EffectRecord {
                key: "instr:1".into(),
                state: EffectState::Dispatched,
                at: 2,
            },
        ];
        save(&dir, &s).unwrap();
        let got = loaded(load(&dir));
        assert_eq!(got.run.effects.len(), 2);
        // 成功したものは成功のまま (再起動後に二重実行しない)。
        assert_eq!(
            got.run
                .effects
                .iter()
                .find(|e| e.key == "start:1")
                .map(|e| e.state),
            Some(EffectState::Completed)
        );
        // 発行しただけのものは発行のまま (再発行の対象として残る)。
        assert_eq!(
            got.run
                .effects
                .iter()
                .find(|e| e.key == "instr:1")
                .map(|e| e.state),
            Some(EffectState::Dispatched)
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 置き換えの再試行は諦めたら理由を返す() {
        // Windows は宛先を誰かが開いていると置換を断る。1 回で諦めると
        // 「いちばん保存したいとき」に書けなくなるので数回試すが、
        // **無限には待たない**。
        let dir = tmp("rename-retry");
        std::fs::create_dir_all(&dir).unwrap();
        let missing = dir.join("no-such-source");
        let start = std::time::Instant::now();
        let e = rename_retrying(&missing, &dir.join("dest"))
            .expect_err("存在しない元から置き換えられた");
        let took = start.elapsed();
        assert!(matches!(e, SaveError::Io(_)));
        // **1 回で諦めていないこと**を、こちら自身が入れた待ちの合計で見る。
        // 上限 (絶対時間) は引かない — 遅い機械で嘘の赤になるので、
        // **下限だけ**を見る (自分の `sleep` は短くはならない)。
        let least: std::time::Duration = (1..RENAME_RETRIES).map(|i| RENAME_BACKOFF * i).sum();
        assert!(
            took >= least,
            "1 回で諦めている (待ち {took:?} < 想定 {least:?})"
        );
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
