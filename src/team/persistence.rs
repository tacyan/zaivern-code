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
/// * 5 — 元 workspace と Run 専用 git worktree の対応を保存する。
///   エージェント起動・検証・changeset 計測は後者、状態の置き場は前者を使う。
/// * 6 — Run専用worktreeを作った基準commitを保存し、detached HEAD上で
///   commitされた成果物も削除確認とchangesetの対象にする。
pub const SCHEMA_VERSION: u32 = 6;

/// events.jsonl の行数上限 (超えたら古い行から落とす)。
pub const EVENT_LOG_MAX_LINES: usize = 5_000;
/// 1 ファイルのバイト上限。これを超えたものは読まない (壊れているとみなす)。
pub const FILE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 墓標列挙の上限。外部入力で起動時メモリとI/Oを無制限に増やさない。
pub const CLOSED_RUN_MAX: usize = 256;

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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RunDoc {
    pub version: u32,
    /// 実行 ID。**同じ Run を二重に開始しない**ための鍵。
    pub run_id: String,
    /// ユーザーが Team を開始した元 workspace。旧版互換のため
    /// フィールド名は維持する。
    pub workspace: String,
    /// Run 専用 worktree と実行 workspace。`None` は旧保存形式。
    #[serde(default)]
    pub run_workspace: Option<super::run_workspace::RunWorkspace>,
    pub spec_source: String,
    pub agent_count: usize,
    /// **どのエージェントで動かすか** (プリセット名の一覧)。空なら「おまかせ」。
    ///
    /// | 選び方 | 動き |
    /// |---|---|
    /// | 空 | この PC に入っている CLI を、役割ごとに配る |
    /// | 1 つ | 全員がそれで動く |
    /// | 複数 | **選んだものの中だけ**で、役割ごとに配る |
    ///
    /// 1 つも複数も同じ仕組みで動く — 選ばれたものだけを候補にして
    /// [`super::roles::preset_for_role`] を通すだけ (分岐を 2 つ作らない)。
    ///
    /// Run に持たせるのは、再起動をまたいでも同じ顔ぶれで立て直すため
    /// (設定を後から変えても、走っている Run の編成は変わらない)。
    #[serde(default)]
    pub agent_presets: Vec<String>,
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
    /// 受理済み報告のbounded台帳。削除失敗と再起動が重なっても二重反映しない。
    #[serde(default)]
    pub seen_blocks: Vec<SeenBlockRecord>,
    /// **人が実行を承認した検証 (1 回ぶん)。**
    ///
    /// 文字列だけで持ってはいけない ([`ValidationApproval`] の doc を参照)。
    #[serde(default)]
    pub validation_approvals: Vec<ValidationApproval>,
    /// 検証 1 本あたりの時間切れ (秒)。テストは短い値を注入する。
    #[serde(default = "default_validation_timeout")]
    pub validation_timeout_secs: u64,
    /// **この Run にだけ効く安全側の設定** ([`super::model::RunGuardrails`])。
    ///
    /// 既存のグローバル設定は 1 バイトも書き換えない。ここに持つのは
    /// 「この Run では、それに加えてどこまで締めるか」だけ。
    /// `serde(default)` なので、この欄が無い旧 Run もそのまま読める。
    #[serde(default)]
    pub guardrails: super::model::RunGuardrails,
    /// 版 1 の「処理済み Effect」。読むためだけに残す
    /// ([`RunDoc::migrate`] が `effects` へ移す)。**新しく書かない。**
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub done_effects: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeenBlockRecord {
    pub agent_id: String,
    pub kind: String,
    pub task_id: Option<u64>,
    pub digest: (u64, u64),
    pub len: u32,
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
#[derive(Clone, Debug, PartialEq)]
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
#[derive(Clone, Debug, PartialEq)]
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
        return Err(SaveError::Io(e));
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
pub(super) fn rename_retrying(from: &Path, to: &Path) -> Result<(), String> {
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
    Err(last.unwrap_or_else(|| "置き換えに失敗しました".to_string()))
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
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
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
        if !super::outbox::valid_run_id(&self.run.run_id) {
            return Err(format!("run_id が安全な名前ではありません: {:?}", self.run.run_id));
        }
        if self.run_id != self.run.run_id {
            return Err(format!(
                "スナップショットの run_id ({}) が Run ({}) と一致しません",
                self.run_id, self.run.run_id
            ));
        }
        if self.workspace != self.run.workspace {
            return Err(format!(
                "スナップショットの workspace ({}) が Run ({}) と一致しません",
                self.workspace, self.run.workspace
            ));
        }
        if let Some(run_workspace) = &self.run.run_workspace {
            if run_workspace.source_workspace != self.run.workspace {
                return Err("Run の元 workspace と worktree 対応が一致しません".into());
            }
        }
        if let Some(t) = self.tasks.iter().find(|t| t.goal_id != self.goal.id) {
            return Err(format!(
                "タスク #{} の goal ({}) が goal.json ({}) と一致しません",
                t.id, t.goal_id.0, self.goal.id.0
            ));
        }
        if self.tasks.iter().any(|t| t.id == u64::MAX) {
            return Err("最大値のtask_idは次のIDを安全に採番できません".into());
        }
        if self.events.iter().any(|e| e.id == u64::MAX) {
            return Err("最大値のevent_idは次のIDを安全に採番できません".into());
        }
        if self.run.seen_blocks.len() > super::runtime::SEEN_BLOCKS_CAP {
            return Err(format!(
                "受理済み報告台帳が上限{}件を超えています",
                super::runtime::SEEN_BLOCKS_CAP
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
    if let Some(parent) = dir.parent() {
        ensure_plain_dir_created(parent)?;
    }
    ensure_plain_dir_created(dir)?;
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
        rename_retrying(&cur, &dir.join(PREV_FILE)).map_err(SaveError::Io)?;
    }
    fault(SavePhase::PrevRetired)?;

    if let Err(e) = rename_retrying(&tmp, &cur) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io(e));
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
        static REMOVE_UNDER: std::cell::RefCell<Option<std::path::PathBuf>> =
            const { std::cell::RefCell::new(None) };
    }

    /// この段で `save` を失敗させる。
    pub fn fail_at(phase: SavePhase) {
        AT.with(|c| c.set(Some(phase)));
    }

    /// **このパス (とその下) の削除を失敗させる** ([`super::remove_dir_checked`])。
    ///
    /// 権限エラーの再現は OS 依存 (root で走る CI や Windows では通ってしまう)
    /// なので、削除の口を 1 つに絞ってそこで決定的に失敗させる。
    pub fn fail_remove_under(path: &std::path::Path) {
        REMOVE_UNDER.with(|c| *c.borrow_mut() = Some(path.to_path_buf()));
    }

    /// 差し替えを外す。
    pub fn clear() {
        AT.with(|c| c.set(None));
        REMOVE_UNDER.with(|c| *c.borrow_mut() = None);
    }

    pub(super) fn should_fail(phase: SavePhase) -> bool {
        AT.with(|c| c.get()) == Some(phase)
    }

    pub(super) fn should_fail_remove(dir: &std::path::Path) -> bool {
        REMOVE_UNDER.with(|c| c.borrow().as_ref().is_some_and(|p| dir.starts_with(p)))
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

// ── 閉じた Run の墓標 ─────────────────────────────────────────────────
//
// Run を閉じるとき、保存 (`runs/<run_id>/`) の削除は失敗しうる (Windows の
// delete pending、権限、同期ツールが握っている、など)。削除だけに頼ると、
// 失敗した Run は**次の起動で `restore_run` に拾われて勝手に復活する**。
// そこで「閉じた」という事実を**消す前に**別の場所へ原子的に書き、復元は
// それを先に見る。後始末が全部済んだら墓標も片付ける。
//
// 置き場は `<state_dir>/closed/<run_id>`。名前は [`super::outbox::safe_child`]
// と同じ関門を通す (空・`..`・区切り文字入りの `run_id` は、書く側も消す側も
// 通さない)。中身が壊れていても**存在すれば閉じた扱い** (安全側)。

/// 閉じた Run の墓標を置くフォルダ名。
pub const CLOSED_DIR: &str = "closed";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosePolicy {
    /// 未保存変更が無い場合だけ削除する。旧墓標もこの扱い。
    #[default]
    CleanOnly,
    /// ユーザーが未保存成果物の破棄を明示承認した。
    Discard,
    /// Run管理だけを閉じ、worktreeと成果物を残す。
    Keep,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClosePhase {
    /// セッション／validationの停止完了をまだ確認していない。
    Stopping,
    /// 停止完了を確認済みで、policyに従うcleanupを再試行できる。
    #[default]
    Cleanup,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloseRecord {
    pub run_id: String,
    pub closed_at: u64,
    #[serde(default)]
    pub policy: ClosePolicy,
    #[serde(default)]
    pub phase: ClosePhase,
    /// UI表示専用。削除対象は必ず`run_workspace::expected_root()`から再計算する。
    #[serde(default)]
    pub artifact_path: String,
}

/// Run ごとの保存フォルダ (`<state_dir>/runs/<run_id>/`)。
///
/// `run_id` が名前として安全でなければ `None` — 保存も削除もしない。
/// 保存された JSON の `run_id` は利用者のファイルから来るので、そのまま
/// `join` すると `runs/` の外へ書き出せてしまう。
pub fn run_dir_in(state_dir: &Path, run_id: &str) -> Option<PathBuf> {
    super::outbox::safe_child(&state_dir.join(super::panel::RUNS_DIR), run_id)
}

/// 墓標のパス (`run_id` が安全なときだけ)。
fn closed_marker(state_dir: &Path, run_id: &str) -> Option<PathBuf> {
    super::outbox::safe_child(&state_dir.join(CLOSED_DIR), run_id)
}

/// **閉じたと記す** (削除の前に呼ぶ)。書き方は保存と同じ「一時ファイルへ
/// fsync → rename」なので、途中で落ちても半端な墓標は残らない。
pub fn mark_closed(state_dir: &Path, run_id: &str) -> Result<(), SaveError> {
    write_close_record(
        state_dir,
        &CloseRecord {
            run_id: run_id.to_string(),
            closed_at: super::model::now_secs(),
            policy: ClosePolicy::CleanOnly,
            phase: ClosePhase::Cleanup,
            artifact_path: String::new(),
        },
    )
}

pub fn mark_close_state(
    state_dir: &Path,
    run_id: &str,
    policy: ClosePolicy,
    phase: ClosePhase,
    artifact_path: &str,
) -> Result<(), SaveError> {
    write_close_record(
        state_dir,
        &CloseRecord {
            run_id: run_id.to_string(),
            closed_at: super::model::now_secs(),
            policy,
            phase,
            artifact_path: artifact_path.to_string(),
        },
    )
}

fn write_close_record(state_dir: &Path, record: &CloseRecord) -> Result<(), SaveError> {
    let run_id = record.run_id.as_str();
    let marker = closed_marker(state_dir, run_id).ok_or_else(|| {
        SaveError::Io(format!("run_id {run_id:?} は保存の名前にできません"))
    })?;
    let dir = state_dir.join(CLOSED_DIR);
    if let Some(parent) = state_dir.parent() {
        ensure_plain_dir_created(parent)?;
    }
    ensure_plain_dir_created(state_dir)?;
    ensure_plain_dir_created(&dir)?;
    let body = serde_json::to_string(record).map_err(|e| SaveError::Serialize(e.to_string()))?;
    let tmp = tmp_path(&dir, run_id);
    write_synced(&tmp, &body)?;
    if let Err(e) = rename_retrying(&tmp, &marker) {
        let _ = std::fs::remove_file(&tmp);
        return Err(SaveError::Io(e));
    }
    sync_dir(&dir);
    Ok(())
}

/// 墓標の内容を読む。壊れている場合は`None`だが、[`is_closed`]はtrueのまま。
/// 呼び出し側は内容不明を自動削除してはいけない。
pub fn close_record(state_dir: &Path, run_id: &str) -> Option<CloseRecord> {
    let marker = closed_marker(state_dir, run_id)?;
    let raw = read_capped(&marker)?;
    let record: CloseRecord = serde_json::from_str(&raw).ok()?;
    (record.run_id == run_id).then_some(record)
}

/// 根の控え (`state.json`) が持っている `run_id` (読めなければ `None`)。
///
/// 根の控えは「いちばん古い 1 本」の写しなので、その Run を閉じたら一緒に
/// 片付けないと復元経路が拾い直す。
pub fn root_run_id(state_dir: &Path) -> Option<String> {
    read_doc(&state_dir.join(STATE_FILE))
        .or_else(|| read_doc(&state_dir.join(PREV_FILE)))
        .map(|d| d.run.run_id)
}

/// 閉じたと記されているか。**中身は見ない** — 壊れていても、有れば閉じた扱い。
pub fn is_closed(state_dir: &Path, run_id: &str) -> bool {
    closed_marker(state_dir, run_id).is_some_and(|p| p.exists())
}

/// 墓標を片付ける (後始末が済んだあと)。無ければ成功。
pub fn unmark_closed(state_dir: &Path, run_id: &str) -> Result<(), SaveError> {
    let Some(marker) = closed_marker(state_dir, run_id) else {
        return Ok(());
    };
    match std::fs::remove_file(&marker) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SaveError::Io(e.to_string())),
    }
}

/// 墓標のある `run_id` の一覧 (名前順)。書きかけの一時ファイル (`.` 始まり) は
/// [`super::outbox::valid_run_id`] が弾くので混ざらない。
pub fn closed_run_ids(state_dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(state_dir.join(CLOSED_DIR)) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter_map(|e| e.file_name().to_str().map(str::to_string))
        .filter(|n| super::outbox::valid_run_id(n))
        .take(CLOSED_RUN_MAX)
        .collect();
    ids.sort();
    ids
}

/// **フォルダごと消す。無いのは成功。** それ以外の失敗は理由を返す。
///
/// 削除は `let _ =` で握り潰さない — 失敗を黙ると、消えなかった保存が次の
/// 起動で復活する。テストは [`fault_inject::fail_remove_under`] で失敗を
/// 決定的に起こせる (権限エラーの再現は OS 依存なので使わない)。
pub fn remove_dir_checked(dir: &Path) -> Result<(), String> {
    #[cfg(test)]
    if fault_inject::should_fail_remove(dir) {
        return Err("(テスト) 削除に失敗".to_string());
    }
    if let Some(parent) = dir.parent() {
        ensure_plain_dir(parent).map_err(|e| e.detail())?;
    }
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(format!("削除対象が通常のフォルダではありません: {}", dir.display()));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e.to_string()),
        Ok(_) => {}
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

fn ensure_plain_dir(dir: &Path) -> Result<(), SaveError> {
    match std::fs::symlink_metadata(dir) {
        Ok(meta) if meta.file_type().is_symlink() => Err(SaveError::Io(format!(
            "保存フォルダにsymlinkは使えません: {}",
            dir.display()
        ))),
        Ok(meta) if !meta.is_dir() => Err(SaveError::Io(format!(
            "保存先がフォルダではありません: {}",
            dir.display()
        ))),
        Ok(_) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(SaveError::Io(e.to_string())),
    }
}

/// symlinkを辿らず、通常ディレクトリであることを確認して1段だけ作る。
/// 呼び出し側は親から順に渡すため、`create_dir_all` で検査前の親を横断しない。
pub(super) fn ensure_plain_dir_created(dir: &Path) -> Result<(), SaveError> {
    match std::fs::symlink_metadata(dir) {
        Ok(_) => ensure_plain_dir(dir),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = dir.parent().ok_or_else(|| {
                SaveError::Io(format!("保存フォルダの親を決められません: {}", dir.display()))
            })?;
            // 足りない親だけを同じ関門で先に作る。最初に存在する
            // 親で止まるため、OS側の `/var` 等の配置まで利用を禁止しない。
            ensure_plain_dir_created(parent)?;
            match std::fs::create_dir(dir) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(SaveError::Io(e.to_string())),
            }
            ensure_plain_dir(dir)
        }
        Err(e) => Err(SaveError::Io(e.to_string())),
    }
}

/// **復元できる保存が 1 つでもあるか** (起動時の「復元しますか」の根拠)。
///
/// [`has_run`] は根の `state.json` の有無しか見ないので、閉じた Run の
/// 控えが根に残っているだけで「ある」と言ってしまう。墓標のある Run と、
/// 名前が安全でない Run は数えない (復元しないものを案内しない)。
pub fn has_restorable_run(state_dir: &Path) -> bool {
    if has_run(state_dir) {
        // 根の控えは「いちばん古い 1 本」。その Run が閉じられていなければ復元できる。
        match read_doc(&state_dir.join(STATE_FILE))
            .or_else(|| read_doc(&state_dir.join(PREV_FILE)))
        {
            Some(doc) => {
                let id = doc.run.run_id.as_str();
                if super::outbox::valid_run_id(id) && !is_closed(state_dir, id) {
                    return true;
                }
            }
            // 読めない (旧形式・壊れている) — 復元経路が退避と案内をする。
            None => return true,
        }
    }
    let Ok(rd) = std::fs::read_dir(state_dir.join(super::panel::RUNS_DIR)) else {
        return false;
    };
    rd.filter_map(|e| e.ok()).any(|e| {
        let name = e.file_name();
        let Some(id) = name.to_str() else {
            return false;
        };
        super::outbox::valid_run_id(id) && !is_closed(state_dir, id) && has_run(&e.path())
    })
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
                run_workspace: None,
                spec_source: "SPEC.md".into(),
                agent_count: 4,
                agent_presets: Vec::new(),
                max_attempts: 3,
                review_required: true,
                paused: false,
                stopped: false,
                started_at: 100,
                updated_at: 100,
                validation_approvals: Vec::new(),
                validation_timeout_secs: default_validation_timeout(),
                guardrails: Default::default(),
                effects: vec![EffectRecord {
                    key: "start:1".into(),
                    state: EffectState::Completed,
                    at: 100,
                }],
                seen_blocks: Vec::new(),
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
    fn run_workspaceの基準commitを保存復元し旧schemaの欠落は空で読む() {
        let dir = tmp("run-workspace-base");
        let mut state = saved();
        state.run.workspace = "source".into();
        state.run.run_workspace = Some(super::super::run_workspace::RunWorkspace {
            source_workspace: "source".into(),
            repository_root: "repo".into(),
            worktree_root: "worktree".into(),
            execution_workspace: "execution".into(),
            base_commit: "a".repeat(40),
        });
        save(&dir, &state).unwrap();
        let loaded = match load(&dir) {
            LoadOutcome::Loaded(saved) => *saved,
            other => panic!("保存した基準commitを復元できない: {other:?}"),
        };
        assert_eq!(
            loaded.run.run_workspace.as_ref().unwrap().base_commit,
            "a".repeat(40)
        );

        let path = dir.join(STATE_FILE);
        let mut raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        raw["version"] = serde_json::Value::from(5);
        raw["run"]["version"] = serde_json::Value::from(5);
        raw["run"]["run_workspace"]
            .as_object_mut()
            .unwrap()
            .remove("base_commit");
        std::fs::write(&path, serde_json::to_vec(&raw).unwrap()).unwrap();
        let legacy = match load(&dir) {
            LoadOutcome::Loaded(saved) => *saved,
            other => panic!("旧schemaを安全に読めない: {other:?}"),
        };
        assert!(
            legacy
                .run
                .run_workspace
                .as_ref()
                .unwrap()
                .base_commit
                .is_empty(),
            "欠落した基準commitを現在値で捏造した"
        );
        std::fs::remove_dir_all(&dir).ok();
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
    fn 別runの外側メタデータを混ぜたスナップショットは拒否する() {
        let dir = tmp("cross-run-state");
        save(&dir, &saved()).unwrap();
        let path = dir.join(STATE_FILE);
        let mut doc = read_doc(&path).expect("読める");
        doc.run_id = "run-other".into();
        std::fs::write(&path, serde_json::to_string(&doc).unwrap()).unwrap();
        match load(&dir) {
            LoadOutcome::Corrupt { reason, .. } => {
                assert!(reason.contains("run_id"), "理由が分からない: {reason}");
            }
            other => panic!("別 Run の混在を受理した: {other:?}"),
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

    #[cfg(unix)]
    #[test]
    fn 保存先の親がsymlinkなら外へ書かない() {
        let state = tmp("save-parent-symlink");
        let outside = tmp("save-parent-symlink-outside");
        std::os::unix::fs::symlink(&outside, state.join(super::super::panel::RUNS_DIR)).unwrap();
        let run = state.join(super::super::panel::RUNS_DIR).join("run-a");

        let err = save(&run, &saved()).expect_err("symlinkの親を通って保存した");
        assert!(err.detail().contains("symlink"), "{}", err.detail());
        assert!(
            std::fs::read_dir(&outside).unwrap().next().is_none(),
            "外部のフォルダへ状態を書いた"
        );
        std::fs::remove_dir_all(&state).ok();
        std::fs::remove_dir_all(&outside).ok();
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
        assert!(!e.is_empty(), "理由が空");
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

    /// **墓標は原子的に置かれ、書きかけが残らない。**
    ///
    /// 保存と同じ「一時ファイルへ fsync → rename」を通るので、途中で落ちても
    /// 半端な墓標は生まれない。走査 ([`closed_run_ids`]) は `.` 始まりの
    /// 一時ファイルを拾わない。
    #[test]
    fn 墓標は原子的に置かれ一時ファイルを残さない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-closed", "atomic");
        mark_closed(&dir, "run-1").expect("置ける");
        assert!(is_closed(&dir, "run-1"));
        let pen = dir.join(CLOSED_DIR);
        let names: Vec<String> = std::fs::read_dir(&pen)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["run-1".to_string()], "一時ファイルが残っている");
        assert_eq!(closed_run_ids(&dir), vec!["run-1".to_string()]);
        // 二度置いても増えない (置き換え)。
        mark_closed(&dir, "run-1").expect("置き直せる");
        assert_eq!(closed_run_ids(&dir), vec!["run-1".to_string()]);
        // 片付けは冪等 (無いものを消すのは成功)。
        unmark_closed(&dir, "run-1").expect("消せる");
        assert!(!is_closed(&dir, "run-1"));
        unmark_closed(&dir, "run-1").expect("無くても成功");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **壊れた墓標でも「閉じた」と読む** (安全側)。中身は見ない。
    #[test]
    fn 壊れた墓標は閉じた扱いになる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-closed", "corrupt");
        let pen = dir.join(CLOSED_DIR);
        std::fs::create_dir_all(&pen).unwrap();
        std::fs::write(pen.join("run-9"), b"\xff\xfe not json at all").unwrap();
        assert!(is_closed(&dir, "run-9"), "読めない墓標を無視した");
        assert_eq!(closed_run_ids(&dir), vec!["run-9".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **保存の名前にできない `run_id` は、書くのも読むのも消すのも断る。**
    /// 断らないと `runs/` や `closed/` の外を触ることになる。
    #[test]
    fn 危険なrun_idは墓標にも保存先にもならない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-closed", "unsafe-id");
        for bad in ["", ".", "..", "../x", "/abs", "a/b", "a\\b", "C:x", ".hidden"] {
            assert!(mark_closed(&dir, bad).is_err(), "{bad:?} の墓標を書いた");
            assert!(!is_closed(&dir, bad), "{bad:?} を閉じた扱いにした");
            assert_eq!(run_dir_in(&dir, bad), None, "{bad:?} の保存先を作った");
            // 消すほうも黙って成功にする (触る先が無いので害は無い)。
            assert!(unmark_closed(&dir, bad).is_ok());
        }
        let ok = run_dir_in(&dir, "run-1").expect("正しい ID");
        assert_eq!(ok.parent(), Some(dir.join(super::super::panel::RUNS_DIR).as_path()));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 削除の口は 1 つ。**無いのは成功、それ以外の失敗は理由を返す。**
    #[test]
    fn 削除は無いのを成功にし失敗は理由を返す() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-closed", "remove");
        let sub = dir.join("gone");
        assert!(remove_dir_checked(&sub).is_ok(), "無いものの削除を失敗にした");
        std::fs::create_dir_all(sub.join("deep")).unwrap();
        std::fs::write(sub.join("deep/x.json"), "{}").unwrap();
        fault_inject::fail_remove_under(&sub);
        let err = remove_dir_checked(&sub).expect_err("仕込んだ失敗が返らない");
        assert!(!err.trim().is_empty(), "理由が空");
        assert!(sub.exists(), "失敗したのに消えている");
        fault_inject::clear();
        assert!(remove_dir_checked(&sub).is_ok());
        assert!(!sub.exists());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **復元できるものがあるかを、墓標まで見て答える。**
    #[test]
    fn 復元できる保存の有無は墓標まで見る() {
        let dir = crate::test_util::unique_temp_dir("zaivern-team-closed", "restorable");
        assert!(!has_restorable_run(&dir), "何も無いのに有ると答えた");
        let mut s = saved();
        s.run.run_id = "run-a".into();
        save(&run_dir_in(&dir, "run-a").unwrap(), &s).expect("保存");
        assert!(has_restorable_run(&dir), "保存があるのに無いと答えた");
        mark_closed(&dir, "run-a").expect("墓標");
        assert!(!has_restorable_run(&dir), "閉じた Run を案内した");
        // 根の控えも同じ扱い。
        unmark_closed(&dir, "run-a").unwrap();
        std::fs::remove_dir_all(dir.join(super::super::panel::RUNS_DIR)).unwrap();
        save(&dir, &s).expect("根へ保存");
        assert_eq!(root_run_id(&dir).as_deref(), Some("run-a"));
        assert!(has_restorable_run(&dir), "根の控えを見落とした");
        mark_closed(&dir, "run-a").expect("墓標");
        assert!(!has_restorable_run(&dir), "閉じた Run の根の控えを案内した");
        std::fs::remove_dir_all(&dir).ok();
    }
}
