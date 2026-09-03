//! **報告置き場 (outbox) の取り決め。** 書く側 (指示文 = `prompt.rs`) と
//! 読む側 (`panel.rs`) が**同じ 1 か所**を使う。ここが 2 か所になると、
//! 指示文が教えた名前を読む側が受け付けない、という食い違いが黙って起きる。
//!
//! ## なぜファイルか
//!
//! 画面から読むのをやめてここから読む。Claude Code v2 は報告を改行ではなく
//! カーソル移動で描くので、画面のグリッドでは行が潰れて**構造的に**
//! 取りこぼす (実測)。置き場を `ZAIVERN_HOME` の下に置くのは、ワークスペースへ
//! 置くと `changeset` が「担当外を変更した」と測って報告ごと却下されるため。
//!
//! ## 取り決め (書きかけを読まないために)
//!
//! 1. エージェントは `<agent-id>-<一意な値>.json.tmp` へ JSON **全体**を書き切る
//! 2. 書き終えてから**同じフォルダの中で** `.tmp` を外した名前へ改名する。
//!    同じファイルシステム上の rename は原子的なので、読む側が「半分だけ
//!    書かれたファイル」を見ることは無い
//! 3. 読む側は `.json` だけを見る ([`list_reports`])。`.json.tmp` も
//!    `rejected/` の中身も見ない
//! 4. **読んで・解析して・配送できたときだけ消す。** 先に消すと、読めなかった
//!    報告は二度と戻らない (以前は読む前に消していた)
//! 5. 読めない (書きかけ・壊れた) ファイルは残して次の tick で読み直す。
//!    [`MAX_ATTEMPTS`] 回で諦めて `rejected/` へ隔離し、理由を Run の記録へ残す
//!    ([`Ledger`] が回数を数える)
//! 6. 担当の照合は**ファイル名と本文の両方**で行う ([`judge`])。ファイル名の
//!    境界は `-` なので `agent-1` は `agent-10-…` に当たらない
//!    ([`stem_matches`])。本文の `agent_id` がファイル名の担当と食い違う
//!    報告は配送しない
//!
//! ## 置き場は Run ごと
//!
//! `<state_dir>/outbox/<run_id>/`。**同じ ID の担当は毎 Run に居る**
//! (`team-lead` など) ので、Run をまたいで 1 つの表に混ぜると後の Run の
//! セッションが前の Run のものを上書きし、報告が別の Run へ流れる。
//! 読む側は Run ごとに独立した表で配送先を引く。
//!
//! Run を閉じるときは置き場ごと消す ([`run_dir`] が「消してよい場所」を
//! 構造で保証する — `run_id` が空・`..`・区切り文字入りなら置き場そのものを
//! 作らない)。

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::model::AgentId;
use super::result_parser as rp;

/// 置き場の親フォルダ名 (`<state_dir>/outbox/<run_id>/`)。
pub const DIR_NAME: &str = "outbox";

/// 正式な報告ファイルの拡張子。
pub const FINAL_EXT: &str = "json";

/// 書きかけの一時ファイルの末尾。`.json` ではないので読む側は見ない。
pub const TMP_SUFFIX: &str = ".json.tmp";

/// 取り込めなかった報告を移す先 (置き場の中の 1 段下)。
pub const REJECTED_DIR: &str = "rejected";

/// 読めないファイルを読み直す回数の上限。
///
/// 走査は [`super::panel::SCAN_INTERVAL`] (400ms) ごとなので、20 回 ≒ 8 秒。
/// 取り決めどおり rename で公開されたファイルは 1 回で読めるので、ここまで
/// 読めないのは「直接 `.json` へ書いている途中」か「壊れている」のどちらか。
/// 上限が無いと壊れた 1 個が永久に残り、毎 tick 同じ失敗を出す。
pub const MAX_ATTEMPTS: u32 = 20;

/// `run_id` の長さの上限 (置き場の名前にするので、ファイル名の制限より内側)。
const RUN_ID_MAX_LEN: usize = 128;

/// 正式な報告ファイルの名前 (`<agent-id>-<一意な値>.json`)。
pub fn final_name(agent_id: &str, unique: &str) -> String {
    format!("{agent_id}-{unique}.{FINAL_EXT}")
}

/// 一時ファイルの名前 (`<agent-id>-<一意な値>.json.tmp`)。
pub fn tmp_name(agent_id: &str, unique: &str) -> String {
    format!("{agent_id}-{unique}{TMP_SUFFIX}")
}

/// **置き場の名前にしてよい `run_id` か。**
///
/// 消す側 ([`run_dir`]) が `<state_dir>/outbox/` の 1 段下しか触らないための
/// 関門。空・`.` 始まり (`.` / `..` / 隠しファイル)・区切り文字や `:`
/// (Windows のドライブ) を含むものは通さない。`new_run_id` が作る
/// `run-<秒>-<pid>-<番号>` は必ず通る。
pub fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= RUN_ID_MAX_LEN
        && !run_id.starts_with('.')
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// `<base>/<name>` — ただし `name` が 1 段の安全な名前のときだけ。
///
/// 戻りが `Some` なら、その親は必ず `base` で、末尾の 1 段は `name` そのもの
/// (join が別の場所へ跳ぶ余地を、名前の検査と結果の検査の両方で塞ぐ)。
pub fn safe_child(base: &Path, name: &str) -> Option<PathBuf> {
    if !valid_run_id(name) {
        return None;
    }
    let dir = base.join(name);
    let anchored = dir.parent() == Some(base)
        && dir.file_name().and_then(|n| n.to_str()) == Some(name)
        && dir.starts_with(base);
    anchored.then_some(dir)
}

/// この Run の置き場 (`<state_dir>/outbox/<run_id>/`)。
///
/// `run_id` が置き場の名前として安全でなければ `None` — 置き場を**作らない**
/// (画面から読む経路だけになる) し、閉じるときも**消さない**。
pub fn run_dir(state_dir: &Path, run_id: &str) -> Option<PathBuf> {
    safe_child(&state_dir.join(DIR_NAME), run_id)
}

/// ファイル名 (拡張子なし) が担当 `id` のものか。**境界を見る。**
///
/// `id` そのもの、または `id` の直後に `-` が来るものだけ。
/// `agent-1` は `agent-10-x` に当たらない (直後が `0`)。
pub fn stem_matches(stem: &str, id: &str) -> bool {
    !id.is_empty()
        && (stem == id
            || stem
                .strip_prefix(id)
                .is_some_and(|rest| rest.starts_with('-')))
}

/// ファイル名から担当の候補を全部引く (`a` と `a-b` のように、境界を
/// 見ても 2 つ当たることはある。決めるのは [`judge`] で、本文と突き合わせる)。
pub fn candidates(stem: &str, ids: &[AgentId]) -> Vec<AgentId> {
    ids.iter()
        .filter(|id| stem_matches(stem, id.as_str()))
        .cloned()
        .collect()
}

/// 1 つの報告ファイルをどう扱うか。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// この担当の報告として配送してよい。
    Deliver(AgentId),
    /// いまは取り込めない (書きかけ・読めない)。残して次の tick で読み直す。
    Retry(String),
    /// 取り込まない。隔離して理由を残す。`agent` は記録の宛先 (分かれば)。
    Reject { agent: Option<AgentId>, why: String },
}

/// **ファイル名と本文の両方で担当を決める。** 純関数。
///
/// * JSON として完結していなければ [`Verdict::Retry`] (書きかけは必ずここで
///   止まる — 途中で切れた JSON が正しい JSON になることは無い)
/// * 本文に `agent_id` が無い / ファイル名がどの担当にも当たらない /
///   本文の `agent_id` がファイル名の担当と食い違う → [`Verdict::Reject`]
/// * それ以外 → [`Verdict::Deliver`]
///
/// 読み方は Runtime と同じ [`rp::parse_lenient`] (綴りの手直しまで同じ)。
/// ここで読めたものは Runtime でも読める。
pub fn judge(stem: &str, body: &str, ids: &[AgentId]) -> Verdict {
    if body.len() > rp::BLOCK_MAX_BYTES {
        // 囲みに入れて渡しても `extract_blocks` が黙って落とす大きさ。
        // 配送して消すと「届いたのに何も起きない」になるので、ここで断る。
        return Verdict::Reject {
            agent: None,
            why: format!(
                "報告が大きすぎます ({} バイト。上限 {} バイト)",
                body.len(),
                rp::BLOCK_MAX_BYTES
            ),
        };
    }
    let value: serde_json::Value = match rp::parse_lenient(body) {
        Ok(v) => v,
        Err(e) => return Verdict::Retry(format!("JSON として読めません: {e}")),
    };
    let claimed = value
        .get("agent_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(claimed) = claimed else {
        return Verdict::Reject {
            agent: None,
            why: "本文に agent_id がありません".to_string(),
        };
    };
    let cands = candidates(stem, ids);
    if cands.is_empty() {
        return Verdict::Reject {
            agent: ids.iter().find(|id| id.as_str() == claimed).cloned(),
            why: format!("ファイル名 `{stem}` はこの Run のどの担当にも一致しません"),
        };
    }
    match cands.iter().find(|c| c.as_str() == claimed) {
        Some(agent) => Verdict::Deliver(agent.clone()),
        None => Verdict::Reject {
            agent: ids.iter().find(|id| id.as_str() == claimed).cloned(),
            why: format!(
                "ファイル名の担当 ({}) と本文の agent_id ({claimed}) が一致しません",
                cands
                    .iter()
                    .map(|c| c.as_str())
                    .collect::<Vec<_>>()
                    .join(" / ")
            ),
        },
    }
}

/// 正式な報告ファイルだけを並べる (`.json`・通常ファイル・名前順)。
///
/// `.json.tmp` (拡張子が `tmp`) と `rejected/` (ディレクトリ) は
/// 構造的に外れる。
pub fn list_reports(dir: &Path) -> Vec<PathBuf> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == FINAL_EXT))
        .collect();
    files.sort();
    files
}

/// 隔離の結果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Disposal {
    /// `rejected/` へ移した。
    Moved(PathBuf),
    /// 移せなかったので消した (残すと毎 tick 同じ失敗を出す)。
    Deleted,
}

/// 取り込めなかった報告を `rejected/` へ移す。移せなければ消す。
///
/// 移す先は同じ置き場の 1 段下なので、[`list_reports`] には二度と現れない。
/// 人が中身を見て「何を書いたのか」を追えるように、消すより移すを先に試す。
pub fn quarantine(file: &Path) -> std::io::Result<Disposal> {
    let bad = |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, what.to_string());
    let dir = file.parent().ok_or_else(|| bad("親フォルダが無い"))?;
    let name = file.file_name().ok_or_else(|| bad("ファイル名が無い"))?;
    let pen = dir.join(REJECTED_DIR);
    let moved = std::fs::create_dir_all(&pen).and_then(|()| {
        let dest = pen.join(name);
        std::fs::rename(file, &dest).map(|()| dest)
    });
    match moved {
        Ok(dest) => Ok(Disposal::Moved(dest)),
        Err(_) => std::fs::remove_file(file).map(|()| Disposal::Deleted),
    }
}

/// 1 ファイルの覚え書き。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct Entry {
    /// 読み直した回数。
    attempts: u32,
    /// 理由を記録へ出したか (同じファイルで毎 tick 出さない)。
    announced: bool,
}

/// **読めなかった報告の台帳** (保存しない — 再起動したら数え直せばよい)。
///
/// 置き場ごとに持たず、パスで引く。閉じた Run のぶんは [`Ledger::prune_missing`]
/// が「もう無いファイル」として落とす。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    entries: HashMap<PathBuf, Entry>,
}

impl Ledger {
    /// 読み直しを 1 回数え、通算を返す。
    pub fn bump(&mut self, file: &Path) -> u32 {
        let e = self.entries.entry(file.to_path_buf()).or_default();
        e.attempts += 1;
        e.attempts
    }

    /// このファイルについて**初めて**理由を出すときだけ `true`。
    pub fn announce_once(&mut self, file: &Path) -> bool {
        let e = self.entries.entry(file.to_path_buf()).or_default();
        let first = !e.announced;
        e.announced = true;
        first
    }

    /// 片付いたファイルを忘れる。
    pub fn forget(&mut self, file: &Path) {
        self.entries.remove(file);
    }

    /// もう無いファイルの覚え書きを落とす (消えた・隔離した・Run を閉じた)。
    pub fn prune_missing(&mut self) {
        self.entries.retain(|p, _| p.exists());
    }

    /// 覚えているファイル (テストが「片付いたら忘れる」を見る)。
    #[cfg(test)]
    pub fn tracked(&self) -> std::collections::HashSet<PathBuf> {
        self.entries.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(list: &[&str]) -> Vec<AgentId> {
        list.iter().map(|s| AgentId::new(*s)).collect()
    }

    fn report(agent: &str) -> String {
        format!(
            "{{\"task_id\": 1, \"agent_id\": \"{agent}\", \"status\": \"completed\", \
             \"summary\": \"x\", \"changed_files\": [], \"validation\": [], \"blockers\": []}}"
        )
    }

    /// **`agent-1` / `agent-10` / `agent-100` を同時に置いても取り違えない。**
    ///
    /// 以前は `stem.starts_with(id)` だったので `agent-10-report` が `agent-1`
    /// にも当たり、しかも HashMap の走査順で結果が変わっていた。
    #[test]
    fn 担当idの前方一致は境界で切る() {
        let all = ids(&["agent-1", "agent-10", "agent-100"]);
        let table: &[(&str, &[&str])] = &[
            ("agent-1", &["agent-1"]),
            ("agent-10", &["agent-10"]),
            ("agent-100", &["agent-100"]),
            ("agent-1-report", &["agent-1"]),
            ("agent-10-report", &["agent-10"]),
            ("agent-100-1712345678", &["agent-100"]),
            // 境界が `-` でないものは誰にも当たらない
            ("agent-1x", &[]),
            ("agent-1_report", &[]),
            ("agent-", &[]),
            ("agent", &[]),
            ("", &[]),
        ];
        for (stem, want) in table {
            let got: Vec<String> = candidates(stem, &all).into_iter().map(|a| a.0).collect();
            let want: Vec<String> = want.iter().map(|s| s.to_string()).collect();
            assert_eq!(got, want, "stem={stem:?}");
        }
        // 走査順に依らない: 逆順で渡しても同じ 1 つに決まる
        let rev: Vec<AgentId> = all.iter().rev().cloned().collect();
        for stem in ["agent-1-r", "agent-10-r", "agent-100-r"] {
            assert_eq!(candidates(stem, &all), candidates(stem, &rev), "stem={stem}");
        }
    }

    /// **本文の `agent_id` とファイル名の担当を突き合わせる。**
    #[test]
    fn 本文とファイル名の担当が食い違う報告は配送しない() {
        let all = ids(&["agent-1", "agent-10", "agent-100"]);
        // 一致 → 配送
        assert_eq!(
            judge("agent-10-abc", &report("agent-10"), &all),
            Verdict::Deliver(AgentId::new("agent-10"))
        );
        assert_eq!(
            judge("agent-1", &report("agent-1"), &all),
            Verdict::Deliver(AgentId::new("agent-1"))
        );
        // ファイル名は agent-10、本文は agent-1 → 却下 (どちらへも配らない)
        match judge("agent-10-abc", &report("agent-1"), &all) {
            Verdict::Reject { agent, why } => {
                assert_eq!(agent, Some(AgentId::new("agent-1")), "記録の宛先");
                assert!(why.contains("agent-10") && why.contains("agent-1"), "{why}");
            }
            other => panic!("配送してしまった: {other:?}"),
        }
        // ファイル名が誰にも当たらない → 却下
        assert!(matches!(
            judge("stranger-1", &report("agent-1"), &all),
            Verdict::Reject { .. }
        ));
        // 本文に agent_id が無い → 却下
        assert!(matches!(
            judge("agent-1-x", "{\"task_id\": 1}", &all),
            Verdict::Reject { .. }
        ));
    }

    /// **`a` と `a-b` のように境界を見ても 2 つ当たるときは、本文で決める。**
    #[test]
    fn 候補が二つあるときは本文のagent_idで決める() {
        let all = ids(&["a", "a-b"]);
        assert_eq!(candidates("a-b-x", &all).len(), 2, "前提: 2 つ当たる");
        assert_eq!(
            judge("a-b-x", &report("a-b"), &all),
            Verdict::Deliver(AgentId::new("a-b"))
        );
        assert_eq!(
            judge("a-b-x", &report("a"), &all),
            Verdict::Deliver(AgentId::new("a"))
        );
        assert!(matches!(
            judge("a-b-x", &report("c"), &all),
            Verdict::Reject { .. }
        ));
    }

    /// **書きかけは Retry、大きすぎは Reject。** 途中で切れた JSON が
    /// 正しい JSON になることは無いので、書きかけは必ずここで止まる。
    #[test]
    fn 書きかけのjsonは読み直しに回す() {
        let all = ids(&["agent-1"]);
        let full = report("agent-1");
        for cut in [0, 1, 10, full.len() / 2, full.len() - 1] {
            let partial = &full[..cut];
            assert!(
                matches!(judge("agent-1-x", partial, &all), Verdict::Retry(_)),
                "cut={cut} で Retry にならない: {partial:?}"
            );
        }
        assert!(matches!(
            judge("agent-1-x", &full, &all),
            Verdict::Deliver(_)
        ));
        let huge = format!(
            "{{\"agent_id\":\"agent-1\",\"summary\":\"{}\"}}",
            "x".repeat(rp::BLOCK_MAX_BYTES)
        );
        assert!(matches!(
            judge("agent-1-x", &huge, &all),
            Verdict::Reject { .. }
        ));
    }

    /// **消してよい場所は `<state_dir>/outbox/<run_id>` の 1 段下だけ。**
    #[test]
    fn 置き場の名前にできないrun_idは断る() {
        let root = std::env::temp_dir().join("zv-outbox-safety");
        let base = root.join(DIR_NAME);
        for bad in [
            "",
            ".",
            "..",
            ".hidden",
            "a/b",
            "a\\b",
            "../x",
            "/abs",
            "C:x",
            "run 1",
            "run\u{0}1",
            "日本語",
        ] {
            assert!(!valid_run_id(bad), "{bad:?} を通した");
            assert_eq!(run_dir(&root, bad), None, "{bad:?} で置き場を作った");
        }
        let long = "r".repeat(RUN_ID_MAX_LEN + 1);
        assert_eq!(run_dir(&root, &long), None, "長すぎる run_id を通した");
        let longest = "x".repeat(RUN_ID_MAX_LEN);
        for good in ["run-1756000000-123-0", "abc", "a.b-c_d", longest.as_str()] {
            let dir = run_dir(&root, good).unwrap_or_else(|| panic!("{good:?} を断った"));
            assert_eq!(dir.parent(), Some(base.as_path()), "{good:?} の親が置き場でない");
            assert_eq!(dir.file_name().and_then(|n| n.to_str()), Some(good));
        }
        // `new_run_id` が作るものは必ず通る
        assert!(valid_run_id(&super::super::runtime::new_run_id()));
    }

    /// **`.json` だけを読む。** `.json.tmp` と `rejected/` の中は見ない。
    #[test]
    fn 一時ファイルと隔離先は一覧に出ない() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "list");
        std::fs::write(dir.join(tmp_name("agent-1", "1")), "{").unwrap();
        std::fs::write(dir.join(final_name("agent-1", "2")), "{}").unwrap();
        std::fs::write(dir.join("agent-1.json"), "{}").unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(dir.join(REJECTED_DIR)).unwrap();
        std::fs::write(dir.join(REJECTED_DIR).join(final_name("agent-1", "3")), "{}").unwrap();
        // 拡張子だけ `.json` のディレクトリも報告ではない
        std::fs::create_dir_all(dir.join("dir.json")).unwrap();
        let names: Vec<String> = list_reports(&dir)
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["agent-1-2.json".to_string(), "agent-1.json".to_string()]);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 隔離は同じ置き場の 1 段下へ。移した後は一覧から消える。
    #[test]
    fn 隔離したファイルは一覧から消える() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "quarantine");
        let f = dir.join(final_name("agent-1", "9"));
        std::fs::write(&f, "{broken").unwrap();
        assert_eq!(list_reports(&dir).len(), 1);
        let dest = match quarantine(&f).expect("隔離できる") {
            Disposal::Moved(d) => d,
            Disposal::Deleted => panic!("移せる場所なのに消した"),
        };
        assert_eq!(dest.parent(), Some(dir.join(REJECTED_DIR).as_path()));
        assert!(dest.exists() && !f.exists());
        assert!(list_reports(&dir).is_empty(), "隔離したものがまだ一覧に出る");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 台帳は回数を数え消えたファイルを忘れる() {
        let dir = crate::test_util::unique_temp_dir("zaivern-outbox", "ledger");
        let f = dir.join("agent-1-1.json");
        std::fs::write(&f, "{").unwrap();
        let mut l = Ledger::default();
        assert_eq!(l.bump(&f), 1);
        assert_eq!(l.bump(&f), 2);
        assert!(l.announce_once(&f), "初回は出す");
        assert!(!l.announce_once(&f), "2 回目は出さない");
        l.prune_missing();
        assert_eq!(l.tracked().len(), 1, "まだ有るファイルを忘れた");
        std::fs::remove_file(&f).unwrap();
        l.prune_missing();
        assert!(l.tracked().is_empty(), "消えたファイルを覚えたまま");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 指示文が教える名前を、読む側がそのまま受け付ける (取り決めが 1 か所)。
    #[test]
    fn 指示文の名前は読む側の照合を通る() {
        let all = ids(&["impl-1", "impl-10"]);
        let fin = final_name("impl-1", "1712345678");
        let stem = Path::new(&fin)
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap();
        assert_eq!(candidates(stem, &all), ids(&["impl-1"]));
        let tmp = tmp_name("impl-1", "1712345678");
        assert!(tmp.ends_with(TMP_SUFFIX));
        assert_eq!(
            Path::new(&tmp).extension().and_then(|x| x.to_str()),
            Some("tmp"),
            "一時ファイルの拡張子が json になっている (読まれてしまう)"
        );
        assert_eq!(&tmp[..tmp.len() - 4], &fin, "`.tmp` を外すと正式な名前になる");
    }
}
