//! エージェント別のセッション履歴 (JSONL 追記ログ)
//!
//! 「前に使ったエージェントの会話を、次に立ち上げ直したときに再開する」ための保存層。
//!
//! ## なぜベンダーの保存物では足りないか
//!
//! [`crate::session_picker`] が読むのは**ベンダー側が残したファイル**
//! (Claude の `~/.claude/projects/**.jsonl`、Codex の `rollout-*.jsonl`、
//! Antigravity のローカル SQLite) だけなので、保存物を持たないエージェントは
//! 一覧に一切出ず再開もできない。カタログには 30 種以上あるのに、実際に
//! 「続きから」を押せるのはごく一部という状態だった。
//!
//! こちらはアプリ自身が起動時点で持っている情報 (起動コマンド全文・cwd・
//! PTY 生ログのパス) を書き残すので、**エージェントの種類に依存せず**
//! 一覧と再開ができる。ベンダー ID が判るときは [`Entry::vendor_id`] に
//! 入れておき、ベンダー側の再開機能へ橋渡しできるようにしてある。
//!
//! ## 置き場と形式
//!
//! `~/.zaivern/history/<agent_bin>/<workspace_key>.jsonl`
//!
//! * エージェント別にディレクトリを分けるので、片方が壊れても他方は無傷。
//! * 1 行 1 レコードの JSONL で**追記のみ**。既存の TOML セッションファイル
//!   ([`crate::session`]) とは別系統にしてある — あちらは「今のウィンドウの状態」を
//!   丸ごと書き換える器で、追記ログとは寿命も更新頻度も違うため。
//! * 壊れた 1 行で全履歴を失わないこと (= 行単位のフェイルソフト) をこのモジュールの
//!   最優先の性質とする。JSON パースに失敗した行は黙って飛ばす。
//!
//! ## テスト可能性のための構造
//!
//! 公開 API は `~/.zaivern` を指す薄いラッパーで、実体は履歴ルートを引数で受け取る
//! `*_in()` 系の内部関数にある。テストは `*_in()` を一時ディレクトリに向けて叩くので、
//! **実ユーザーの `~/.zaivern` には決して触れない**。

// UI 配線 (履歴パネル / 再開ボタン) は別担当の作業で、まだ呼び出し側が無い。
// 配線が入った時点でこの allow を外すこと — 外し忘れると
// 「作ったのに繋いでいない」の検出器 (dead_code) が効かなくなる。
#![allow(dead_code)]

use crate::config::zaivern_dir;
use serde::{Deserialize, Serialize};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::Write as _;
use std::path::{Path, PathBuf};

/// `brief` に残す最大文字数 (バイトではなく char 数)。
/// 一覧の 1 行に出す前提なので、これ以上は持っていても表示に使えない。
const BRIEF_MAX_CHARS: usize = 200;

/// パス構成要素 (エージェント名) の最大文字数。
/// 長いファイル名は OS ごとに上限が違う (Windows の MAX_PATH が最も厳しい) ため、
/// どの環境でも安全側に倒れる長さで切る。
const COMPONENT_MAX_CHARS: usize = 64;

/// 履歴 1 件 = 「あるエージェントを 1 回起動したこと」。
///
/// 全フィールドに既定値が入る (`#[serde(default)]` は構造体単位で書くと全フィールドに効く)。
/// これは**前方互換のため**で、将来フィールドを増やしても古い行が読めなくならないし、
/// 逆に新しい版が書いた行を古い版が読んでも未知フィールドを無視するだけで済む。
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    /// アプリ内のセッション ID (`crate::session` の採番と同じもの)。
    pub id: u64,
    /// 実行ファイル名 (`claude` / `codex` / `gemini` …)。ディレクトリ名にもなる。
    pub agent_bin: String,
    /// カタログのプリセット名 (表示用)。
    pub preset_name: String,
    /// タブに出していたタイトル。
    pub title: String,
    /// タブのアイコン (絵文字 1 文字想定、空可)。
    pub icon: String,
    /// 起動コマンド全文。**再開時にそのまま実行できる形**で持つ。
    pub command: String,
    /// 起動時の作業フォルダ (絶対パス)。
    pub cwd: String,
    /// PTY 生ログのパス ([`crate::session::term_log_path`])。空可。
    pub log_file: String,
    /// 開始時刻 (Unix 秒)。
    pub started: i64,
    /// 終了時刻 (Unix 秒)。`0` は「まだ開いている / 不明」。
    pub ended: i64,
    /// 最初のユーザー指示の要約 ([`brief_of`] で作る)。空可。
    pub brief: String,
    /// ベンダー側のセッション ID が判っていれば入れる (Claude の UUID 等)。空可。
    /// 空なら [`Self::command`] での再開にフォールバックする。
    pub vendor_id: String,
}

// ── パス導出 ────────────────────────────────────────────────

/// 履歴ルート: `~/.zaivern/history/`。
///
/// 直書きせず [`crate::config::zaivern_dir`] から導く (home が取れない環境でも
/// `./.zaivern` に落ちるだけで動く)。
fn history_root() -> PathBuf {
    zaivern_dir().join("history")
}

/// このエージェントの履歴ファイルが入るディレクトリ。
///
/// `cwd` は現在のレイアウトでは使わない (ワークスペースはファイル名側で分けている) が、
/// **他の公開 API と引数の並びを揃える**ために受け取る。呼び出し側は常に
/// 「エージェント + 作業フォルダ」の組で考えることになり、将来レイアウトを
/// `history/<agent>/<key>/` へ変えても呼び出し側の変更が要らない。
pub fn record_dir(agent_bin: &str, _cwd: &Path) -> PathBuf {
    record_dir_in(&history_root(), agent_bin)
}

/// このエージェント × この作業フォルダの履歴ファイル (JSONL) のパス。
pub fn record_path(agent_bin: &str, cwd: &Path) -> PathBuf {
    record_path_in(&history_root(), agent_bin, cwd)
}

fn record_dir_in(root: &Path, agent_bin: &str) -> PathBuf {
    root.join(sanitize_component(agent_bin))
}

fn record_path_in(root: &Path, agent_bin: &str, cwd: &Path) -> PathBuf {
    record_dir_in(root, agent_bin).join(format!("{}.jsonl", workspace_key(cwd)))
}

// ── 純関数 ──────────────────────────────────────────────────

/// 任意の文字列を、どの OS でも安全なパス構成要素へ落とす。
///
/// 禁止文字の**ブロックリストではなく許可リスト**にしてあるのは、OS ごとの
/// 禁止文字表 (Windows の `< > : " / \ | ? *` と制御文字、macOS の `:`、
/// unix の `/`) を追いかけ続けたくないから。英数字・`-`・`_`・`.` だけを通せば
/// 3 OS すべてで確実に通る。
///
/// さらに次の落とし穴を潰してある:
/// * 空 / `.` / `..` → `_` (パス外へ抜ける構成要素を作らない)
/// * 先頭の `.` → `_` (unix で隠しディレクトリになり、一覧から消える)
/// * 末尾の `.` → 除去 (Windows は末尾ドットを勝手に落とすので名前が一致しなくなる)
/// * Windows の予約デバイス名 (`CON` / `NUL` / `COM1` …) → 先頭に `_`
pub fn sanitize_component(name: &str) -> String {
    let mut out: String = name
        .chars()
        .take(COMPONENT_MAX_CHARS)
        .map(|ch| {
            if ch.is_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    // 末尾ドットは Windows が黙って落とすので、こちらで先に落として名前を一致させる。
    while out.ends_with('.') {
        out.pop();
    }
    if out.starts_with('.') {
        out.replace_range(..1, "_");
    }
    if out.is_empty() {
        return "_".to_string();
    }
    if is_windows_reserved(&out) {
        out.insert(0, '_');
    }
    out
}

/// Windows の予約デバイス名か (拡張子を除いた語幹で判定、大文字小文字を問わない)。
/// これらの名前のファイルは Windows では作れないので、避ける必要がある。
fn is_windows_reserved(name: &str) -> bool {
    let stem = name.split('.').next().unwrap_or(name).to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL") {
        return true;
    }
    let is_numbered = |prefix: &str| {
        stem.strip_prefix(prefix)
            .is_some_and(|n| n.len() == 1 && matches!(n.as_bytes()[0], b'1'..=b'9'))
    };
    is_numbered("COM") || is_numbered("LPT")
}

/// 作業フォルダ → 16 桁 hex のワークスペースキー。
///
/// 作り方は [`crate::session`] のワークスペースハッシュに合わせてある
/// (canonicalize してから [`DefaultHasher`])。canonicalize するのは
/// シンボリックリンク経由や `./` 付きで開いた同じフォルダを同じキーへ寄せるため。
/// **失敗しても落ちない** — 存在しない / 権限が無いパスでは元のパスをそのまま
/// 使う。履歴の分類が分かれるだけで、動作を止める理由にはならない。
pub fn workspace_key(cwd: &Path) -> String {
    let resolved = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = DefaultHasher::new();
    resolved.to_string_lossy().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 最初のユーザー指示 → 一覧の 1 行に出せる要約。
///
/// 改行・タブ・**制御文字 (ANSI エスケープの残骸を含む)** を空白へ潰してから
/// 連続空白を 1 個に畳む。生のプロンプトをそのまま入れると JSONL の 1 行が
/// 巨大になり、一覧の描画でも折り返しで崩れるため。
/// 切り詰めたときは末尾に `…` を付けて「続きがある」ことを示す。
pub fn brief_of(text: &str) -> String {
    let flat: String = text
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    let words: Vec<&str> = flat.split_whitespace().collect();
    let joined = words.join(" ");
    let mut out: String = joined.chars().take(BRIEF_MAX_CHARS).collect();
    if joined.chars().count() > BRIEF_MAX_CHARS {
        out.push('…');
    }
    out
}

/// 現在時刻 (Unix 秒)。時計が epoch 以前でも落とさず 0 を返す。
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── 読み書き ────────────────────────────────────────────────

/// 履歴を 1 件追記する。保存先は `entry` の `agent_bin` / `cwd` から決まる。
pub fn append(entry: &Entry) -> std::io::Result<()> {
    append_in(&history_root(), entry)
}

fn append_in(root: &Path, entry: &Entry) -> std::io::Result<()> {
    let path = record_path_in(root, &entry.agent_bin, Path::new(&entry.cwd));
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other("履歴ファイルの親ディレクトリが無い"));
    };
    std::fs::create_dir_all(dir)?;
    let mut line = serde_json::to_string(entry).map_err(std::io::Error::other)?;
    line.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    f.write_all(line.as_bytes())
}

/// 該当 ID の行の `ended` を埋めて書き戻す (セッション終了時に呼ぶ想定)。
pub fn update_end(agent_bin: &str, cwd: &Path, id: u64, ended: i64) -> std::io::Result<()> {
    update_end_in(&history_root(), agent_bin, cwd, id, ended)
}

fn update_end_in(
    root: &Path,
    agent_bin: &str,
    cwd: &Path,
    id: u64,
    ended: i64,
) -> std::io::Result<()> {
    let path = record_path_in(root, agent_bin, cwd);
    let Some(text) = read_text(&path) else {
        // まだ 1 件も無い = 更新対象が無いだけ。エラーにはしない。
        return Ok(());
    };
    let mut hit = false;
    let mut out = String::with_capacity(text.len());
    for line in text.lines() {
        if line.trim().is_empty() {
            continue;
        }
        // 対象行だけ差し替え、**それ以外は元の文字列のまま**書き戻す。
        // 一度デコードして再エンコードすると、将来版が足したフィールドを
        // 古い版が黙って削ってしまうため (前方互換を壊さない)。
        match serde_json::from_str::<Entry>(line) {
            Ok(mut e) if e.id == id && !hit => {
                e.ended = ended;
                match serde_json::to_string(&e) {
                    Ok(s) => {
                        out.push_str(&s);
                        hit = true;
                    }
                    Err(_) => out.push_str(line),
                }
            }
            _ => out.push_str(line),
        }
        out.push('\n');
    }
    if !hit {
        // 書き換える必要が無いなら触らない (無駄な rename でファイルを揺らさない)。
        return Ok(());
    }
    write_atomic(&path, &out)
}

/// このエージェント × この作業フォルダの履歴を**新しい順**で返す。
pub fn list(agent_bin: &str, cwd: &Path) -> Vec<Entry> {
    list_in(&history_root(), agent_bin, cwd)
}

fn list_in(root: &Path, agent_bin: &str, cwd: &Path) -> Vec<Entry> {
    let mut v = read_entries(&record_path_in(root, agent_bin, cwd));
    sort_newest_first(&mut v);
    v
}

/// `history/` 配下の**全エージェント**の履歴を集めて新しい順で返す。
pub fn list_all(cwd: &Path) -> Vec<Entry> {
    list_all_in(&history_root(), cwd)
}

fn list_all_in(root: &Path, cwd: &Path) -> Vec<Entry> {
    let mut all = Vec::new();
    // ディレクトリ名 = サニタイズ済みのエージェント名。read_dir で発見する方式なら、
    // カタログに無いエージェント (ユーザーが追加した独自コマンド) の履歴も拾える。
    if let Ok(entries) = std::fs::read_dir(root) {
        for e in entries.flatten() {
            if !e.path().is_dir() {
                continue;
            }
            let file = e.path().join(format!("{}.jsonl", workspace_key(cwd)));
            all.extend(read_entries(&file));
        }
    }
    sort_newest_first(&mut all);
    all
}

/// 新しい順に `keep` 件だけ残して、それより古い行を捨てる。
///
/// 追記のみのログなので、放っておけば無制限に伸びる。一覧に出せる件数には
/// 上限があるのだから、ファイルにも上限を持たせる。
pub fn prune(agent_bin: &str, cwd: &Path, keep: usize) -> std::io::Result<()> {
    prune_in(&history_root(), agent_bin, cwd, keep)
}

fn prune_in(root: &Path, agent_bin: &str, cwd: &Path, keep: usize) -> std::io::Result<()> {
    let path = record_path_in(root, agent_bin, cwd);
    let Some(text) = read_text(&path) else {
        return Ok(());
    };
    // (元の行, 読めたレコード) の組。壊れた行はここで落ちる = prune が掃除も兼ねる。
    let parsed: Vec<(&str, Entry)> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok().map(|e| (l, e)))
        .collect();
    if parsed.len() <= keep && parsed.len() == text.lines().filter(|l| !l.trim().is_empty()).count()
    {
        // 件数も内容も削るものが無い。書き戻さない。
        return Ok(());
    }
    // 新しい順に並べて上位 `keep` 件の位置を選び、**ファイル上の順序 (古い順) で**書き戻す。
    // 追記ログとしての「後ろほど新しい」性質を壊さないため。
    let mut order: Vec<usize> = (0..parsed.len()).collect();
    order.sort_by(|&a, &b| newest_first(&parsed[a].1, &parsed[b].1));
    order.truncate(keep);
    order.sort_unstable();
    let mut out = String::new();
    for i in order {
        out.push_str(parsed[i].0);
        out.push('\n');
    }
    write_atomic(&path, &out)
}

// ── 下請け ──────────────────────────────────────────────────

/// ファイル全体を文字列で読む。存在しなければ `None`。
///
/// `read_to_string` ではなくバイト読み + lossy 変換にしているのは、書き込み途中で
/// 落ちた行が不正な UTF-8 になっていても**残りの履歴まで巻き添えにしない**ため。
fn read_text(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// JSONL を読んでレコード列にする。**壊れた行は黙って飛ばす**。
fn read_entries(path: &Path) -> Vec<Entry> {
    let Some(text) = read_text(path) else {
        return Vec::new();
    };
    text.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        .collect()
}

/// 並び順: 開始が新しい順。同時刻なら ID の大きい方 (= 後から作った方) を先に。
fn newest_first(a: &Entry, b: &Entry) -> std::cmp::Ordering {
    b.started.cmp(&a.started).then(b.id.cmp(&a.id))
}

fn sort_newest_first(v: &mut [Entry]) {
    v.sort_by(newest_first);
}

/// 同一ディレクトリの一時ファイルへ書いてから rename する。
///
/// 途中でプロセスが落ちても、履歴ファイルが**書きかけの状態で残らない**ようにする。
/// rename は同一ファイルシステム内なので原子的で、Windows でも既存ファイルを置換する。
fn write_atomic(path: &Path, body: &str) -> std::io::Result<()> {
    let Some(dir) = path.parent() else {
        return Err(std::io::Error::other("履歴ファイルの親ディレクトリが無い"));
    };
    std::fs::create_dir_all(dir)?;
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let stem = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "history".to_string());
    // 一時ファイル名に PID と時刻を混ぜるのは、複数インスタンスが同時に
    // 書き戻しても互いの一時ファイルを踏まないようにするため。
    let tmp = dir.join(format!("{stem}.tmp-{}-{nanos}", std::process::id()));
    std::fs::write(&tmp, body)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の履歴ルート。実 `~/.zaivern` には**絶対に触らない**ため、
    /// 常に `$TMPDIR` 配下の一意なディレクトリを使う。
    fn root(tag: &str) -> PathBuf {
        crate::test_util::unique_temp_dir("zaivern-history-test", tag)
    }

    fn entry(id: u64, agent: &str, cwd: &Path, started: i64) -> Entry {
        Entry {
            id,
            agent_bin: agent.to_string(),
            preset_name: format!("{agent} preset"),
            title: format!("{agent} #{id}"),
            icon: "🤖".to_string(),
            command: format!("{agent} --resume"),
            cwd: cwd.to_string_lossy().into_owned(),
            started,
            ..Default::default()
        }
    }

    #[test]
    fn 追記した履歴を新しい順に読み出せる() {
        let root = root("roundtrip");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        for (id, started) in [(1u64, 100i64), (2, 300), (3, 200)] {
            append_in(&root, &entry(id, "claude", &cwd, started)).expect("append");
        }
        let got = list_in(&root, "claude", &cwd);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![2, 3, 1],
            "started の降順で並ぶこと"
        );
        assert_eq!(got[0].command, "claude --resume");
        assert_eq!(got[0].cwd, cwd.to_string_lossy());
    }

    #[test]
    fn 壊れた行があっても残りの履歴は読める() {
        let root = root("broken");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        append_in(&root, &entry(1, "codex", &cwd, 100)).expect("append");
        // 書き込み途中で落ちたような半端な行を挟む。
        let path = record_path_in(&root, "codex", &cwd);
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        f.write_all(b"{\"id\": 999, broken\n\n")
            .expect("write junk");
        drop(f);
        append_in(&root, &entry(2, "codex", &cwd, 200)).expect("append");

        let got = list_in(&root, "codex", &cwd);
        assert_eq!(
            got.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![2, 1],
            "壊れた 1 行で全履歴が消えてはいけない"
        );
    }

    #[test]
    fn ended_を後から埋められる() {
        let root = root("ended");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        append_in(&root, &entry(7, "gemini", &cwd, 100)).expect("append");
        append_in(&root, &entry(8, "gemini", &cwd, 200)).expect("append");
        assert!(list_in(&root, "gemini", &cwd).iter().all(|e| e.ended == 0));

        update_end_in(&root, "gemini", &cwd, 7, 555).expect("update");
        let got = list_in(&root, "gemini", &cwd);
        let seven = got.iter().find(|e| e.id == 7).expect("id 7");
        assert_eq!(seven.ended, 555);
        let eight = got.iter().find(|e| e.id == 8).expect("id 8");
        assert_eq!(eight.ended, 0, "他の行を巻き込まないこと");
        assert_eq!(got.len(), 2, "行数が変わらないこと");
    }

    #[test]
    fn 存在しない_id_の更新でも壊れない() {
        let root = root("ended-miss");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        append_in(&root, &entry(1, "aider", &cwd, 100)).expect("append");
        update_end_in(&root, "aider", &cwd, 42, 999).expect("update");
        update_end_in(&root, "aider", &cwd, 42, 999).expect("ファイルが無くてもエラーにしない");
        assert_eq!(list_in(&root, "aider", &cwd).len(), 1);
        // そもそもファイルが無いエージェントでも Ok。
        update_end_in(&root, "unknown-agent", &cwd, 1, 1).expect("no file");
    }

    #[test]
    fn prune_で古い履歴が減る() {
        let root = root("prune");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        for id in 1..=10u64 {
            append_in(&root, &entry(id, "droid", &cwd, id as i64 * 10)).expect("append");
        }
        assert_eq!(list_in(&root, "droid", &cwd).len(), 10);
        prune_in(&root, "droid", &cwd, 3).expect("prune");
        let got = list_in(&root, "droid", &cwd);
        assert_eq!(
            got.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![10, 9, 8],
            "新しい方から keep 件だけ残ること"
        );
        // 追記を続けても壊れない (ファイル末尾に改行が残っていること)。
        append_in(&root, &entry(11, "droid", &cwd, 110)).expect("append");
        assert_eq!(list_in(&root, "droid", &cwd).len(), 4);
        // keep が件数以上なら何も減らない。
        prune_in(&root, "droid", &cwd, 100).expect("prune");
        assert_eq!(list_in(&root, "droid", &cwd).len(), 4);
    }

    #[test]
    fn エージェントごとにディレクトリが分かれる() {
        let root = root("split");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        append_in(&root, &entry(1, "claude", &cwd, 100)).expect("append");
        append_in(&root, &entry(2, "codex", &cwd, 200)).expect("append");

        assert_eq!(list_in(&root, "claude", &cwd).len(), 1);
        assert_eq!(list_in(&root, "codex", &cwd).len(), 1);
        assert!(
            record_path_in(&root, "claude", &cwd).is_file(),
            "claude 側のファイルができていること"
        );
        assert_ne!(
            record_dir_in(&root, "claude"),
            record_dir_in(&root, "codex"),
            "ディレクトリが分かれていること"
        );

        // list_all は全エージェントぶんを新しい順にマージする。
        let all = list_all_in(&root, &cwd);
        assert_eq!(all.iter().map(|e| e.id).collect::<Vec<_>>(), vec![2, 1]);
        assert_eq!(all[0].agent_bin, "codex");
    }

    #[test]
    fn list_all_は別ワークスペースの履歴を混ぜない() {
        let root = root("ws-isolate");
        let a = root.join("ws-a");
        let b = root.join("ws-b");
        std::fs::create_dir_all(&a).expect("create a");
        std::fs::create_dir_all(&b).expect("create b");
        append_in(&root, &entry(1, "claude", &a, 100)).expect("append");
        append_in(&root, &entry(2, "claude", &b, 200)).expect("append");
        assert_eq!(list_all_in(&root, &a).len(), 1);
        assert_eq!(list_all_in(&root, &b).len(), 1);
        assert_eq!(list_all_in(&root, &a)[0].id, 1);
    }

    #[test]
    fn 履歴が無いときは空の一覧を返す() {
        let root = root("empty");
        let cwd = root.join("ws");
        assert!(list_in(&root, "claude", &cwd).is_empty());
        assert!(list_all_in(&root, &cwd).is_empty());
        prune_in(&root, "claude", &cwd, 5).expect("prune on missing file");
    }

    #[test]
    fn ファイル名に使えない文字は落ちる() {
        // Windows の禁止文字とパス区切りが全部 `_` になること。
        assert_eq!(
            sanitize_component("a<b>c:d\"e/f\\g|h?i*j"),
            "a_b_c_d_e_f_g_h_i_j"
        );
        assert_eq!(sanitize_component("claude"), "claude");
        assert_eq!(sanitize_component("claude-code_1.2"), "claude-code_1.2");
        // パス外へ抜ける構成要素を作らない。
        assert_eq!(sanitize_component(".."), "_");
        assert_eq!(sanitize_component("."), "_");
        assert_eq!(sanitize_component(""), "_");
        assert_eq!(sanitize_component("   "), "___");
        // 先頭ドットは隠しディレクトリになるので潰す / 末尾ドットは Windows が落とす。
        assert_eq!(sanitize_component(".hidden"), "_hidden");
        assert_eq!(sanitize_component("agent."), "agent");
        // Windows の予約デバイス名。
        assert_eq!(sanitize_component("con"), "_con");
        assert_eq!(sanitize_component("COM1"), "_COM1");
        assert_eq!(sanitize_component("nul.txt"), "_nul.txt");
        assert_eq!(
            sanitize_component("console"),
            "console",
            "前方一致では予約扱いしない"
        );
        // 制御文字と改行も落ちる。
        assert_eq!(sanitize_component("a\nb\tc\0d"), "a_b_c_d");
        // 長すぎる名前は切る。
        assert_eq!(
            sanitize_component(&"x".repeat(500)).chars().count(),
            COMPONENT_MAX_CHARS
        );
        // サニタイズ結果がそのままディレクトリ名になる。
        let root = root("sanitize");
        assert_eq!(
            record_dir_in(&root, "my/agent"),
            root.join("my_agent"),
            "区切り文字が入ってもディレクトリが 1 段だけであること"
        );
    }

    #[test]
    fn 危険な名前でも履歴ルートの外へ書き込まない() {
        let root = root("escape");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        let mut e = entry(1, "../../evil", &cwd, 100);
        e.agent_bin = "../../evil".to_string();
        append_in(&root, &e).expect("append");
        let path = record_path_in(&root, "../../evil", &cwd);
        assert!(
            path.starts_with(&root),
            "履歴ルート配下に収まること: {path:?}"
        );
        assert_eq!(path.components().count(), root.components().count() + 2);
    }

    #[test]
    fn 同じ_cwd_は同じキー_違う_cwd_は違うキー() {
        let root = root("key");
        let a = root.join("proj-a");
        let b = root.join("proj-b");
        std::fs::create_dir_all(&a).expect("create a");
        std::fs::create_dir_all(&b).expect("create b");

        assert_eq!(workspace_key(&a), workspace_key(&a));
        assert_ne!(workspace_key(&a), workspace_key(&b));
        assert_eq!(workspace_key(&a).len(), 16, "16 桁 hex であること");
        assert!(workspace_key(&a).chars().all(|c| c.is_ascii_hexdigit()));
        // `./` を挟んだ同じフォルダは canonicalize で同じキーへ寄る。
        assert_eq!(workspace_key(&a), workspace_key(&a.join(".")));
        // 存在しないパスでも panic しない (canonicalize 失敗のフォールバック)。
        let missing = root.join("no-such-dir");
        assert_eq!(workspace_key(&missing).len(), 16);
    }

    #[test]
    fn brief_は空白を畳んで長さで切る() {
        assert_eq!(brief_of("  hello\n\tworld  "), "hello world");
        assert_eq!(brief_of(""), "");
        assert_eq!(brief_of("   \n  "), "");
        // 制御文字 (ANSI エスケープの残骸) も空白になる。
        assert_eq!(brief_of("a\u{1b}[31mb"), "a [31mb");
        let long = "あ".repeat(500);
        let cut = brief_of(&long);
        assert_eq!(
            cut.chars().count(),
            BRIEF_MAX_CHARS + 1,
            "省略記号 1 文字ぶん増える"
        );
        assert!(cut.ends_with('…'));
        // ちょうど上限なら省略記号を付けない。
        let exact = "b".repeat(BRIEF_MAX_CHARS);
        assert_eq!(brief_of(&exact), exact);
    }

    #[test]
    fn 未知フィールドがある行を読んでも落ちない() {
        // 将来版が足したフィールドを古い版が読む場合 (前方互換)。
        let root = root("forward-compat");
        let cwd = root.join("ws");
        std::fs::create_dir_all(&cwd).expect("create ws");
        let dir = record_dir_in(&root, "opencode");
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = record_path_in(&root, "opencode", &cwd);
        std::fs::write(
            &path,
            "{\"id\":5,\"agent_bin\":\"opencode\",\"started\":10,\"future_field\":\"x\"}\n\
             {\"id\":6}\n",
        )
        .expect("write");
        let got = list_in(&root, "opencode", &cwd);
        assert_eq!(
            got.len(),
            2,
            "未知フィールドも欠けたフィールドも許容すること"
        );
        let five = got.iter().find(|e| e.id == 5).expect("id 5");
        assert_eq!(five.agent_bin, "opencode");
        let six = got.iter().find(|e| e.id == 6).expect("id 6");
        assert_eq!(six.title, "", "欠けたフィールドは既定値");
    }

    #[test]
    fn 公開ラッパーのパスは_zaivern_dir_配下を指す() {
        // 実ファイルには触らず、パスの組み立てだけを確認する。
        let cwd = std::env::temp_dir();
        let dir = record_dir("claude", &cwd);
        assert!(dir.starts_with(crate::config::zaivern_dir().join("history")));
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("claude"));
        let file = record_path("claude", &cwd);
        assert_eq!(file.parent(), Some(dir.as_path()));
        assert_eq!(
            file.extension().and_then(|s| s.to_str()),
            Some("jsonl"),
            "JSONL であること"
        );
    }
}
