//! ワークスペースセッション永続化
//!
//! アプリ再起動時に「開いていたタブ」「アクティブタブ」「サイドバー/パネルの開閉」を
//! 復元するための保存層。ワークスペース絶対パスごとに
//! `~/.zaivern/sessions/<ハッシュhex>.toml` へ保存する。
#![allow(dead_code)]

use std::path::{Path, PathBuf};

/// 1ワークスペース分のセッション情報。
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct SessionData {
    /// 開いていたファイルの絶対パス（存在しないパスもそのまま保存してよい）
    pub open_files: Vec<String>,
    /// アクティブタブの index
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active: Option<usize>,
    pub sidebar_open: bool,
    pub panel_open: bool,
    /// サイドバーのタブ ("files"|"agents"|"plugins"|"git")。
    /// 旧バージョンのセッションファイルには無いので空文字なら既定タブ扱い。
    pub sidebar_tab: String,
    /// ワークスペースのルート一覧(絶対パス)。再起動時に全フォルダを復元する。
    /// 旧形式(単一ルート)のファイルでは空 — その場合は起動時のルートを使う。
    pub roots: Vec<String>,
    /// エディタの分割レイアウト ([`crate::editor_split::EditorPanesRec::to_line`])。
    ///
    /// 端末分割 ([`AgentSessionRec::split`]) と**同じ流儀**: プレーンな文字列 1 本。
    /// TOML はテーブル / 配列を単純値より後ろにしか置けないので、ここへ構造体や
    /// `Vec` を足してはいけない。空 = 分割なし (1 ペインで開く)。
    /// リーフはバッファ ID ではなく**ファイルの絶対パス**で指す (再起動で ID は変わる)。
    pub editor_split: String,
    /// **ピン留めされていたタブ**のファイル絶対パス (ピン順)。
    ///
    /// ペイン ID ではなくパスで持つ理由は [`Self::editor_split`] と同じ
    /// (バッファ ID は再起動で必ず変わる)。復元時は、そのファイルを開いている
    /// 全ペインでピン留めし直す。旧形式のファイルには無いので空 = ピン留めなし。
    /// TOML の制約でテーブル配列 (`agents`) より**前**に置くこと。
    pub pinned_files: Vec<String>,
    /// 変更レビューで「レビュー済み」の印を付けたファイル (リポジトリ相対パス)。
    ///
    /// VS Code の "Mark as viewed" 相当。**印が消えるとレビューは有限で
    /// なくなる** (毎回ゼロから読み直しになる) ので、セッションを跨いで残す。
    /// 相対パスで持つのは、同じリポジトリを別の場所へ clone しても効くため。
    /// TOML の制約でテーブル配列 (`agents`) より**前**に置くこと。
    pub reviewed_files: Vec<String>,
    /// 走らせていたエージェントタブの記録 (チャット履歴のフォルダ別保存)。
    /// フォルダを開き直したときに、タブ + 前回スクロールバックを復元し、
    /// 対応 CLI (claude / codex) は会話を再開する。旧ファイルには無いので空。
    /// TOML の制約 (テーブル配列は単純値の後) があるため必ず最後のフィールドに置く。
    pub agents: Vec<AgentSessionRec>,
}

/// 復元用に保存するエージェントセッション 1 本分の記録。
///
/// env は保存しない — プリセットの env にはシークレットが入り得るので、
/// 復元時に `preset_name` で現在の設定から引き直す (config.rs 側が真実源)。
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct AgentSessionRec {
    /// 起動元プリセット名 (復元時に env を再構成するための照合キー)。
    pub preset_name: String,
    /// タブのタイトル ("Claude Code #2" など)。
    pub title: String,
    pub icon: String,
    /// 実際に起動したコマンド (承認モード適用済み)。復元時はこれに再開指定を足す。
    pub command: String,
    /// 起動ディレクトリ (絶対パス)。消えていたら現在の作業フォルダで代替する。
    pub cwd: String,
    /// 生ログの書き出し先 (絶対パス)。復元後も同じファイルへ追記する。
    pub log_file: String,
    /// このタイルの端末分割レイアウト ([`crate::terminal::SplitLayoutRec::to_line`])。
    ///
    /// **必ずプレーンな文字列 1 本**にしておく: TOML はテーブル / 配列を
    /// 単純値より後ろにしか置けないため、ここへ構造体や `Vec` を足すと
    /// 既存フィールドの並び順に依存した壊れ方をする。空 = 分割なし。
    /// リーフはセッション ID ではなく**生ログのパス**で指す (再起動で ID は変わる)。
    pub split: String,
    /// worktree 隔離で起動していた場合の**本体リポジトリ** (絶対パス)。空 = 隔離なし。
    ///
    /// worktree のフォルダそのものは `cwd` に入っているので、ここには
    /// 「どのリポジトリの worktree か」だけを持つ (破棄時に `git -C <repo>
    /// worktree remove` を撃つ相手)。復元時はこの 2 本が揃っているときだけ
    /// 隔離エージェントとして扱い、**前回と同じ worktree へ戻す**。
    pub worktree_repo: String,
    /// worktree 隔離で起動していた場合のブランチ名 (`agent/<slug>-<n>`)。空 = 隔離なし。
    pub worktree_branch: String,
}

/// `~/.zaivern/sessions/<ルート集合キー>.toml` から読む。無ければ None。
pub fn load(roots: &[PathBuf]) -> Option<SessionData> {
    adopt_keys_for(roots);
    let mut data = load_from(&sessions_dir(), roots)?;
    repoint_log_files(&mut data, roots);
    Some(data)
}

/// 同パスへ書く（ディレクトリは自動作成、失敗は無視）。
pub fn save(roots: &[PathBuf], data: &SessionData) {
    adopt_keys_for(roots);
    save_to(&sessions_dir(), roots, data);
}

/// 旧キーで置かれた保存物を、このルート集合の新しい名前へ引き取る。
///
/// **実 `~/.zaivern` を触るのはこの関数だけ** (テストは `*_in()` / `*_from()` 系を
/// 一時ディレクトリへ向けるので通らない)。引き取りは 2 系統ある:
///
/// * **単一パスのキー** — `term_logs/` `bookmarks/` `lease/` などが使う。
///   主ルートについて [`crate::history::adopt_legacy_keys`] が面倒を見る
/// * **ルート集合のキー** — `sessions/` `hotexit/` が使う。旧値は
///   [`crate::history::legacy_roots_key`]
///
/// どちらも組ごとに 1 プロセス 1 回。
fn adopt_keys_for(roots: &[PathBuf]) {
    if let Some(primary) = roots.first() {
        crate::history::adopt_legacy_keys(primary);
    }
    crate::history::adopt_keys(
        &[crate::history::legacy_roots_key(roots)],
        &crate::history::workspace_set_key(roots),
    );
}

/// 保存されていた生ログのパスを、いまの置き場へ張り替える。
///
/// `log_file` は**絶対パス**なので、`term_logs/<キー>/` の名前が変わると宙に浮く
/// (引き取りでディレクトリごと改名されるため)。ファイル名は変わらないので、
/// **元のパスが消えていて、同じ名前が現在の置き場にあるときだけ**差し替える。
/// 存在するパスには触らない — 利用者が別の場所を指していたら、それが真実源。
fn repoint_log_files(data: &mut SessionData, roots: &[PathBuf]) {
    let Some(primary) = roots.first() else {
        return;
    };
    repoint_log_files_in(data, &term_log_dir(primary));
}

/// 内部: 置き場を直接受ける版（テストで差し替え可能）。
fn repoint_log_files_in(data: &mut SessionData, dir: &Path) {
    for a in &mut data.agents {
        if a.log_file.is_empty() {
            continue;
        }
        let old = Path::new(&a.log_file);
        if old.exists() {
            continue;
        }
        let Some(name) = old.file_name() else {
            continue;
        };
        let cand = dir.join(name);
        if cand.exists() {
            a.log_file = cand.to_string_lossy().into_owned();
        }
    }
}

/// 既定の保存先ディレクトリ: `~/.zaivern/sessions`
/// (`zai session list` からも走査するので pub)
pub fn sessions_dir() -> PathBuf {
    crate::config::zaivern_dir().join("sessions")
}

/// 内部: 既定ディレクトリ配下のセッションファイルパス。
fn session_file(roots: &[PathBuf]) -> PathBuf {
    session_file_in(&sessions_dir(), roots)
}

/// 内部: 指定ディレクトリ配下のセッションファイルパス（テストで差し替え可能）。
///
/// キーは [`crate::history::workspace_set_key`] — **このモジュールでは計算しない**。
/// 層ごとに別々の写像を持つと、同じフォルダに層の数だけ台帳ができて、
/// どれか 1 つだけが静かにデータを失う。
fn session_file_in(dir: &Path, roots: &[PathBuf]) -> PathBuf {
    dir.join(format!("{}.toml", crate::history::workspace_set_key(roots)))
}

/// ターミナル生ログの置き場: `~/.zaivern/term_logs/<ワークスペースハッシュ>/`。
///
/// ワークスペース単位で分けるので、同じプロジェクトを開き直せば前回のログが
/// そのまま並ぶ (スクロールバック永続化)。
pub fn term_log_dir(workspace: &Path) -> PathBuf {
    // 旧キー (`Path` を DefaultHasher へ流した値) で置かれたログを先に引き取る。
    // ここは実 `~/.zaivern` を触るので、テストは `term_log_dir_in` を使うこと。
    crate::history::adopt_legacy_keys(workspace);
    term_log_dir_in(&crate::config::zaivern_dir(), workspace)
}

/// 内部: `zdir` 配下のログ置き場（テストで差し替え可能・ファイルシステムを触らない）。
fn term_log_dir_in(zdir: &Path, workspace: &Path) -> PathBuf {
    zdir.join("term_logs")
        .join(crate::history::workspace_key(workspace))
}

/// セッション 1 本分のログファイルパス。ファイル名にタイトルを含めて
/// 一覧で見分けられるようにする (パスに使えない文字は `_` へ)。
pub fn term_log_path(workspace: &Path, session_id: u64, title: &str) -> PathBuf {
    let safe: String = title
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect();
    term_log_dir(workspace).join(format!("{safe}-{session_id}.log"))
}

/// セッション復元時にスクロールバックへ流し込む前回ログの上限バイト数。
/// vt100 のスクロールバックは 5000 行なので、これで十分に埋まる
/// (ログ全量 (最大 8MB) を流すと起動が重くなるだけで見える行は増えない)。
pub const REPLAY_TAIL_CAP: usize = 1024 * 1024;

/// 前回ログの末尾を読み出す (復元時のスクロールバック再生用)。
///
/// `open_term_log` と同じく `.log.old` (ローテート退避分) → `.log` の順で繋ぎ、
/// 後ろ `cap` バイトへ切り詰める。切り口がエスケープ列や UTF-8 の途中に
/// かからないよう、次の改行まで進めてから切る。ログが無ければ空。
pub fn read_term_log_tail(path: &Path, cap: usize) -> Vec<u8> {
    let mut raw = std::fs::read(path.with_extension("log.old")).unwrap_or_default();
    if let Ok(cur) = std::fs::read(path) {
        raw.extend_from_slice(&cur);
    }
    if raw.len() > cap {
        let from = raw.len() - cap;
        let cut = raw[from..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|i| from + i + 1)
            .unwrap_or(from);
        raw.drain(..cut);
    }
    raw
}

/// 古いターミナルログの掃除。新しい方から `keep` 本を残して削除する
/// (`.old` ローテート分は本体と対で消す)。起動時に一度呼ぶ想定。
pub fn prune_term_logs(workspace: &Path, keep: usize) {
    prune_term_logs_in(&term_log_dir(workspace), keep)
}

/// 内部: ログ置き場を直接受ける版（テスト用）。
fn prune_term_logs_in(dir: &Path, keep: usize) {
    let mut logs = list_term_logs_in(dir);
    for p in logs.split_off(keep.min(logs.len())) {
        let _ = std::fs::remove_file(&p);
        let _ = std::fs::remove_file(p.with_extension("log.old"));
    }
}

/// ワークスペースの既存ログ一覧 (新しい順)。「📜 前回ログ」メニューの素材。
pub fn list_term_logs(workspace: &Path) -> Vec<PathBuf> {
    list_term_logs_in(&term_log_dir(workspace))
}

/// 内部: ログ置き場を直接受ける版（テスト用）。
fn list_term_logs_in(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut logs: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().map(|x| x == "log").unwrap_or(false))
        .filter_map(|e| {
            let t = e.metadata().ok()?.modified().ok()?;
            Some((t, e.path()))
        })
        .collect();
    logs.sort_by_key(|b| std::cmp::Reverse(b.0));
    logs.into_iter().map(|(_, p)| p).collect()
}

/// 内部: 指定ディレクトリから読む（テスト用に保存先を差し替え可能）。
///
/// マルチルートキーで見つからず、かつルートが 1 件だけの場合は、
/// 旧形式（単一ワークスペースパスのハッシュ）のファイルへフォールバックする。
/// これで v0.1.3 以前のセッションもそのまま復元できる。
fn load_from(dir: &Path, roots: &[PathBuf]) -> Option<SessionData> {
    let read = |p: PathBuf| -> Option<SessionData> {
        toml::from_str(&std::fs::read_to_string(p).ok()?).ok()
    };
    if let Some(d) = read(session_file_in(dir, roots)) {
        return Some(d);
    }
    if roots.len() == 1 {
        // v0.1.3 以前は単一ワークスペースのキーで置いていた。旧キーの引き取りで
        // その名前は `history::workspace_key` へ改名済みなので、こちらで探す。
        return read(dir.join(format!("{}.toml", crate::history::workspace_key(&roots[0]))));
    }
    None
}

/// 内部: 指定ディレクトリへ書く（dirは自動作成、失敗は無視）。
fn save_to(dir: &Path, roots: &[PathBuf], data: &SessionData) {
    let Ok(text) = toml::to_string_pretty(data) else {
        return;
    };
    let _ = std::fs::create_dir_all(dir);
    let _ = std::fs::write(session_file_in(dir, roots), text);
}

// ═════════════════════════════════════════════════════════════════════════
//  Hot Exit — 未保存の本文を退避し、再起動後に復元する
// ═════════════════════════════════════════════════════════════════════════
//
// 置き場: `~/.zaivern/hotexit/<ルート集合ハッシュ>/`
//   - `index.toml` … 退避 1 件ずつのメタ情報 (パス / タイトル / ハッシュ)
//   - `<バッファIDのhex>.txt` … 本文そのもの
//
// **本文を索引へ入れない**のは、変更のあったバッファだけを書き直すため。
// 1 文字打つたびに全バッファを直列化していては、大きなファイルで毎打鍵ごとに
// 数 MB の I/O が飛ぶ (設計原則 3: アイドル時のコストはゼロ)。
//
// 書き込みは必ず `<名前>.tmp` → `rename` の二段。途中で電源が落ちても
// 半端な本文が本番のファイル名に残らない。それでも壊れたものを掴んだときの
// ために、索引はバイト長とハッシュを持ち、合わなければその 1 件だけを捨てる
// (起動そのものは必ず成功させる)。

/// 退避 1 件ぶんの索引レコード。**本文はここに入れない** (別ファイル)。
///
/// ハッシュを 16 進文字列で持つのは TOML の整数が i64 までしか無いため
/// (`u64` のまま書くと `hash > i64::MAX` の回で直列化が失敗し、
/// 「たまに退避されない」という再現しにくい壊れ方をする)。
#[derive(Default, Clone, PartialEq, Debug, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HotExitEntry {
    /// 元ファイルの絶対パス。**空 = 名前のないバッファ** (untitled)。
    pub path: String,
    /// タブのタイトル (名前のないバッファを復元するときの表示名)。
    pub title: String,
    /// 本文を書いたファイル名 (退避ディレクトリからの相対。区切りを含まない)。
    pub body: String,
    /// 本文のバイト長。部分書き込みの検出に使う。
    pub len: u64,
    /// 本文のハッシュ (16 進)。部分書き込み・外からの書き換えの検出に使う。
    pub hash: String,
    /// 退避した時点で分かっていた**ディスク側**本文のハッシュ (16 進)。
    /// `disk_existed == false` のときは意味を持たない。
    pub disk_hash: String,
    /// 退避した時点でディスクにファイルが在ったか。
    pub disk_existed: bool,
}

/// 退避ディレクトリの索引ファイルの中身。
#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HotExitIndex {
    pub entries: Vec<HotExitEntry>,
}

/// 索引ファイルの名前 (本文ファイルと衝突しないよう拡張子を分けてある)。
const HOTEXIT_INDEX: &str = "index.toml";

/// 退避を取るための入力 1 件。
///
/// `editor::Buffer` をそのまま受け取らないのは、退避層を UI から切り離して
/// テストできるようにするため (この層は egui も Buffer も知らない)。
#[derive(Clone, Copy)]
pub struct HotExitSnapshot<'a> {
    /// バッファ ID。本文ファイル名の元になる (セッション内で一意)。
    pub id: u64,
    /// 元ファイル。名前のないバッファは `None`。
    pub path: Option<&'a Path>,
    pub title: &'a str,
    /// 未保存の本文。
    pub text: &'a str,
    /// **最後にディスクと合わせた時点**の本文ハッシュ
    /// (`editor::Buffer::saved_hash`)。復元時に「ディスクが外から
    /// 変わっていないか」を判定する基準になる。
    pub saved_hash: u64,
}

/// [`HotExitStore::sync`] の結果。呼び出し側が UI へ出すために使う。
#[derive(Default, Clone, PartialEq, Debug)]
pub struct HotExitReport {
    /// 実際に本文を書き出した件数 (変化が無ければ 0)。
    pub wrote: usize,
    /// 退避を消した件数 (保存された / 閉じられた)。
    pub removed: usize,
    /// **上限を超えたので退避しなかった**バッファのタイトル。
    /// 黙って落とすと「戻ると思っていたのに消えた」になるので必ず伝える。
    pub skipped: Vec<String>,
}

impl HotExitReport {
    /// 何も起きなかったか (トーストを出すかの判定に使う)。
    pub fn is_noop(&self) -> bool {
        self.wrote == 0 && self.removed == 0 && self.skipped.is_empty()
    }
}

/// 退避した本文と、いまのディスクの食い違い方。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DiskState {
    /// 名前のないバッファ — 対応するディスク上のファイルが無い。
    Untitled,
    /// 退避した時点と同じ。そのまま戻して良い。
    Same,
    /// **外から書き換えられている**。黙って上書きしてはいけない。
    Changed,
    /// 退避時は在ったのに、いまは無い。
    Missing,
    /// 在るが読めない (権限など)。
    Unreadable,
}

impl DiskState {
    /// ユーザーに選ばせる必要があるか (= 黙って戻してはいけないか)。
    pub fn needs_choice(&self) -> bool {
        matches!(self, Self::Changed | Self::Missing | Self::Unreadable)
    }

    /// 競合の理由を 1 行で (UI とテストで同じ文言を使う)。
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Untitled | Self::Same => "",
            Self::Changed => "ディスク側が外から書き換えられています",
            Self::Missing => "ディスク側のファイルが消えています",
            Self::Unreadable => "ディスク側のファイルを読めません",
        }
    }
}

/// 復元 1 件ぶん。
#[derive(Clone, Debug)]
pub struct RestoredBuffer {
    /// 元ファイル。名前のないバッファは `None`。
    pub path: Option<PathBuf>,
    pub title: String,
    /// 退避しておいた未保存の本文。
    pub text: String,
    pub disk: DiskState,
    /// いまのディスクの本文 (読めたときだけ)。差分表示と
    /// 「ディスクの方を採る」に使う。
    pub disk_text: Option<String>,
}

/// 退避帳。**変更のあったバッファだけ**を書き出す。
///
/// 呼び出し側 (app.rs) がスロットリングを持ち、この構造体は
/// 「前回と同じ本文なら書かない」という重複排除だけを引き受ける。
pub struct HotExitStore {
    dir: PathBuf,
    /// バッファ ID → 最後に書いた本文ハッシュ。
    seen: std::collections::HashMap<u64, u64>,
    /// 最後に書いた索引。変化が無ければ索引も書き直さない。
    index: Vec<HotExitEntry>,
    /// 1 バッファあたりの退避上限 (バイト)。0 なら 1 件も退避しない。
    max_bytes: usize,
}

impl HotExitStore {
    /// `dir` は [`hotexit_dir_for`] で作る (テストは一時ディレクトリを渡す)。
    ///
    /// **前回の索引をそのまま引き継ぐ。** 引き継がないと、前回の本文
    /// ファイルがどこにも記録されていない状態で最初の `sync` が走り、
    /// 掃除の対象から外れて退避ディレクトリにゴミが溜まり続ける
    /// (バッファ ID は再起動で必ず変わるため名前も一致しない)。
    pub fn new(dir: PathBuf, max_bytes: usize) -> Self {
        let index = std::fs::read_to_string(dir.join(HOTEXIT_INDEX))
            .ok()
            .and_then(|raw| toml::from_str::<HotExitIndex>(&raw).ok())
            .map(|i| i.entries)
            .unwrap_or_default();
        Self {
            dir,
            seen: std::collections::HashMap::new(),
            index,
            max_bytes,
        }
    }

    /// 上限を差し替える (設定の再読み込みで変わりうる)。
    pub fn set_max_bytes(&mut self, max_bytes: usize) {
        self.max_bytes = max_bytes;
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 未保存バッファの一覧を受け取り、差分だけをディスクへ反映する。
    ///
    /// - 前回と同じ本文のバッファは**触らない** (I/O ゼロ)
    /// - 一覧から消えたバッファ (保存された / 閉じた) の退避は削除する
    /// - 上限を超えるバッファは退避せず、タイトルを報告に載せる
    pub fn sync(&mut self, snaps: &[HotExitSnapshot]) -> HotExitReport {
        let mut report = HotExitReport::default();
        let mut next_index: Vec<HotExitEntry> = Vec::with_capacity(snaps.len());
        let mut next_seen: std::collections::HashMap<u64, u64> =
            std::collections::HashMap::with_capacity(snaps.len());
        let mut pending: Vec<(PathBuf, &str)> = Vec::new();

        for s in snaps {
            if s.text.len() > self.max_bytes {
                // 上限超過。退避しない代わりに必ず伝える (無音で落とさない)
                report.skipped.push(s.title.to_string());
                continue;
            }
            let body = body_name(s.id);
            let hash = crate::editor::hash_str(s.text);
            if self.seen.get(&s.id) != Some(&hash) {
                pending.push((self.dir.join(&body), s.text));
            }
            next_seen.insert(s.id, hash);
            next_index.push(HotExitEntry {
                path: s
                    .path
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                title: s.title.to_string(),
                body,
                len: s.text.len() as u64,
                hash: format!("{hash:016x}"),
                disk_hash: format!("{:016x}", s.saved_hash),
                disk_existed: s.path.map(|p| p.is_file()).unwrap_or(false),
            });
        }

        let index_changed = next_index != self.index;
        if pending.is_empty() && !index_changed {
            return report; // 何も変わっていない = ディスクへ触らない
        }
        if std::fs::create_dir_all(&self.dir).is_err() {
            return report;
        }
        for (path, text) in &pending {
            if write_atomic(path, text.as_bytes()) {
                report.wrote += 1;
            }
        }
        // 一覧から消えたバッファの本文を捨てる (ゴミを残さない)
        let keep: std::collections::HashSet<&str> =
            next_index.iter().map(|e| e.body.as_str()).collect();
        for old in &self.index {
            if !keep.contains(old.body.as_str())
                && std::fs::remove_file(self.dir.join(&old.body)).is_ok()
            {
                report.removed += 1;
            }
        }
        if index_changed {
            match toml::to_string_pretty(&HotExitIndex {
                entries: next_index.clone(),
            }) {
                Ok(text) if !next_index.is_empty() => {
                    write_atomic(&self.dir.join(HOTEXIT_INDEX), text.as_bytes());
                }
                // 未保存が 1 つも無くなったら索引ごと消す (ゴミを残さない)
                _ => {
                    let _ = std::fs::remove_file(self.dir.join(HOTEXIT_INDEX));
                }
            }
        }
        self.index = next_index;
        self.seen = next_seen;
        report
    }

    /// 退避を丸ごと捨てる (ユーザーが「復元しない」を選んだとき)。
    pub fn clear(&mut self) {
        self.index.clear();
        self.seen.clear();
        discard_hotexit(&self.dir);
    }
}

/// バッファ ID から本文ファイル名を作る。**区切り文字を含まない**ので
/// 索引を手で書き換えられても退避ディレクトリの外を指せない。
fn body_name(id: u64) -> String {
    format!("{id:016x}.txt")
}

/// `<path>.tmp` へ書いてから rename する。成功したら true。
///
/// 直接書くと、途中で落ちたときに本番のファイル名が半端な本文のまま残る。
fn write_atomic(path: &Path, bytes: &[u8]) -> bool {
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, bytes).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    if std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return false;
    }
    true
}

/// Hot Exit の置き場: `~/.zaivern/hotexit/`。
pub fn hotexit_dir() -> PathBuf {
    crate::config::zaivern_dir().join("hotexit")
}

/// ワークスペース (ルート集合) ごとの退避ディレクトリ。
/// キーはセッションファイルと同じルート集合ハッシュなので、同じフォルダを
/// 開き直せば必ず同じ場所に着く。
pub fn hotexit_dir_for(roots: &[PathBuf]) -> PathBuf {
    adopt_keys_for(roots);
    hotexit_dir_for_in(&hotexit_dir(), roots)
}

/// 内部: 退避ルートを直接受ける版（テストで差し替え可能・実 `~/.zaivern` を触らない）。
fn hotexit_dir_for_in(dir: &Path, roots: &[PathBuf]) -> PathBuf {
    dir.join(crate::history::workspace_set_key(roots))
}

/// 退避ディレクトリを丸ごと消す (失敗は無視)。
pub fn discard_hotexit(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// 退避を読み出す。**何が壊れていても panic しない** — 読めなかった 1 件は
/// 捨てて、残りだけを返す。索引ごと壊れていれば空を返す (起動は必ず成功する)。
pub fn load_hotexit(dir: &Path) -> Vec<RestoredBuffer> {
    let Ok(raw) = std::fs::read_to_string(dir.join(HOTEXIT_INDEX)) else {
        return Vec::new();
    };
    let Ok(index) = toml::from_str::<HotExitIndex>(&raw) else {
        return Vec::new();
    };
    let mut out = Vec::with_capacity(index.entries.len());
    for e in index.entries {
        // 本文ファイル名は「1 要素の普通の名前」だけを許す。
        // 索引を手で書き換えて退避ディレクトリの外を読ませない。
        let mut comps = Path::new(&e.body).components();
        let ok_name =
            matches!(comps.next(), Some(std::path::Component::Normal(_))) && comps.next().is_none();
        if !ok_name {
            continue;
        }
        let Ok(bytes) = std::fs::read(dir.join(&e.body)) else {
            continue; // 本文が消えている / 読めない
        };
        if bytes.len() as u64 != e.len {
            continue; // 部分書き込み
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue; // 不正な UTF-8 (外から壊された)
        };
        if format!("{:016x}", crate::editor::hash_str(&text)) != e.hash {
            continue; // 中身が索引と食い違う
        }
        let (path, disk, disk_text) = if e.path.is_empty() {
            (None, DiskState::Untitled, None)
        } else {
            let p = PathBuf::from(&e.path);
            let (state, dt) = disk_state(&p, &e);
            (Some(p), state, dt)
        };
        out.push(RestoredBuffer {
            path,
            title: e.title,
            text,
            disk,
            disk_text,
        });
    }
    out
}

/// いまのディスクの中身を、退避時に見えていたものと突き合わせる。
fn disk_state(path: &Path, e: &HotExitEntry) -> (DiskState, Option<String>) {
    if !path.exists() {
        // 退避時から在り続けていた前提が崩れているときだけ「消えた」と言う。
        // 元々無かった (一度も保存していない名前付き) なら競合ではない。
        return if e.disk_existed {
            (DiskState::Missing, None)
        } else {
            (DiskState::Same, None)
        };
    }
    let Ok(bytes) = std::fs::read(path) else {
        return (DiskState::Unreadable, None);
    };
    // 開いたときと同じ復号経路を通す (CP932 のファイルを「読めない」にしない)
    let (text, _enc) = crate::textenc::decode_bytes(&bytes);
    let same = format!("{:016x}", crate::editor::hash_str(&text)) == e.disk_hash;
    let state = if same {
        DiskState::Same
    } else {
        DiskState::Changed
    };
    (state, Some(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    // ─────────────────────────────────────────────────────────────────
    // Hot Exit — 退避と復元
    // ─────────────────────────────────────────────────────────────────

    /// 退避ディレクトリを一時領域に作る (実 `~/.zaivern` には絶対に触らない)。
    fn hotexit_tmp(tag: &str) -> PathBuf {
        unique_temp_dir("zaivern-hotexit", tag)
    }

    /// 上限は「そこそこ大きい既定」。上限そのものを見るテストだけ別に渡す。
    const BIG: usize = 1 << 20;

    fn snap<'a>(
        id: u64,
        path: Option<&'a Path>,
        title: &'a str,
        text: &'a str,
    ) -> HotExitSnapshot<'a> {
        HotExitSnapshot {
            id,
            path,
            title,
            text,
            saved_hash: crate::editor::hash_str(""),
        }
    }

    #[test]
    fn 退避と復元が複数バッファで往復する() {
        let dir = hotexit_tmp("roundtrip");
        let f1 = dir.join("a.rs");
        let f2 = dir.join("b.txt");
        std::fs::write(&f1, "").unwrap();
        std::fs::write(&f2, "").unwrap();
        let store_dir = dir.join("store");
        let mut store = HotExitStore::new(store_dir.clone(), BIG);

        let rep = store.sync(&[
            snap(1, Some(&f1), "a.rs", "fn main() {}\n"),
            snap(2, Some(&f2), "b.txt", "ふつうの日本語\n"),
            // 名前のないバッファも退避する
            snap(3, None, "untitled-1", "まだ名前がない本文"),
        ]);
        assert_eq!(rep.wrote, 3);
        assert!(rep.skipped.is_empty());

        let got = load_hotexit(&store_dir);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].text, "fn main() {}\n");
        assert_eq!(got[1].text, "ふつうの日本語\n");
        assert_eq!(got[2].text, "まだ名前がない本文");
        assert_eq!(got[2].path, None, "名前のないバッファにパスが生えた");
        assert_eq!(got[2].title, "untitled-1");
        assert_eq!(got[2].disk, DiskState::Untitled);
        assert_eq!(got[0].disk, DiskState::Same);
        assert_eq!(got[1].disk, DiskState::Same);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 退避はcjkと絵文字とcrlfと空バッファをそのまま戻す() {
        let dir = hotexit_tmp("cjk");
        let store_dir = dir.join("store");
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        // 濁点付き・結合絵文字・全角スペース・CRLF・空
        let cjk = "日本語のテスト\u{3000}ハングル 한국어 中文\n";
        let emoji = "👨‍👩‍👧‍👦 家族と🇯🇵 と ✅\n";
        let crlf = "one\r\ntwo\r\n\r\nfour\r\n";
        store.sync(&[
            snap(1, None, "cjk", cjk),
            snap(2, None, "emoji", emoji),
            snap(3, None, "crlf", crlf),
            snap(4, None, "empty", ""),
        ]);
        let got = load_hotexit(&store_dir);
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].text, cjk);
        assert_eq!(got[1].text, emoji);
        assert_eq!(got[2].text, crlf, "CRLF が LF へ潰れている");
        assert_eq!(got[3].text, "");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 上限を超えたバッファは退避せず名前を報告する() {
        let dir = hotexit_tmp("limit");
        let store_dir = dir.join("store");
        // 上限は設定から来る値。ここでは 32 バイトに絞る
        let mut store = HotExitStore::new(store_dir.clone(), 32);
        let big = "あ".repeat(1000); // 3000 バイト
        let rep = store.sync(&[
            snap(1, None, "小さい", "ok"),
            snap(2, None, "巨大.log", &big),
        ]);
        assert_eq!(rep.skipped, vec!["巨大.log".to_string()]);
        assert_eq!(rep.wrote, 1);
        let got = load_hotexit(&store_dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "ok");
        // 上限を上げれば退避されるようになる (設定から取っている証拠)
        store.set_max_bytes(BIG);
        let rep = store.sync(&[
            snap(1, None, "小さい", "ok"),
            snap(2, None, "巨大.log", &big),
        ]);
        assert!(rep.skipped.is_empty());
        assert_eq!(load_hotexit(&store_dir).len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 変わっていないバッファは書き直さない() {
        let dir = hotexit_tmp("throttle");
        let store_dir = dir.join("store");
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        assert_eq!(store.sync(&[snap(1, None, "a", "hello")]).wrote, 1);
        // 同じ本文をもう一度渡しても I/O は起きない (アイドル時のコストはゼロ)
        let rep = store.sync(&[snap(1, None, "a", "hello")]);
        assert!(rep.is_noop(), "変化が無いのに書いている: {rep:?}");
        // 1 文字でも変われば書く
        assert_eq!(store.sync(&[snap(1, None, "a", "hello!")]).wrote, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 保存して閉じたバッファの退避は消える() {
        let dir = hotexit_tmp("cleanup");
        let store_dir = dir.join("store");
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        store.sync(&[snap(1, None, "a", "aaa"), snap(2, None, "b", "bbb")]);
        assert_eq!(load_hotexit(&store_dir).len(), 2);
        // b を保存した = 未保存一覧から外れる
        let rep = store.sync(&[snap(1, None, "a", "aaa")]);
        assert_eq!(rep.removed, 1);
        let got = load_hotexit(&store_dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "aaa");
        // 全部保存したら索引ごと消えてゴミが残らない
        let rep = store.sync(&[]);
        assert_eq!(rep.removed, 1);
        assert!(load_hotexit(&store_dir).is_empty());
        assert!(
            !store_dir.join("index.toml").exists(),
            "未保存が無いのに索引が残っている"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 壊れた退避を読んでもパニックせず起動できる() {
        let dir = hotexit_tmp("broken");
        let store_dir = dir.join("store");
        std::fs::create_dir_all(&store_dir).unwrap();

        // 1. 索引そのものが無い
        assert!(load_hotexit(&store_dir).is_empty());
        // 2. 索引が TOML として壊れている
        std::fs::write(store_dir.join("index.toml"), "これは TOML ではない [[[").unwrap();
        assert!(load_hotexit(&store_dir).is_empty());
        // 3. 索引は読めるが本文ファイルが無い / 部分書き込み / 不正な UTF-8 /
        //    存在しないパスを指す / 退避ディレクトリの外を指す
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        let ghost = dir.join("消えたファイル.rs");
        store.sync(&[
            snap(1, None, "本文が消える", "あああ"),
            snap(2, None, "部分書き込み", "いいい"),
            snap(3, None, "不正なUTF8", "ううう"),
            snap(4, Some(&ghost), "存在しないパス", "えええ"),
            snap(5, None, "生き残り", "おおお"),
        ]);
        std::fs::remove_file(store_dir.join(format!("{:016x}.txt", 1u64))).unwrap();
        std::fs::write(store_dir.join(format!("{:016x}.txt", 2u64)), "い").unwrap();
        std::fs::write(
            store_dir.join(format!("{:016x}.txt", 3u64)),
            [0xff, 0xfe, 0xff],
        )
        .unwrap();

        let got = load_hotexit(&store_dir);
        // 壊れた 3 件は捨て、残り 2 件は戻る
        assert_eq!(got.len(), 2, "{got:#?}");
        assert_eq!(got[0].title, "存在しないパス");
        assert_eq!(got[0].text, "えええ");
        assert_eq!(got[1].text, "おおお");

        // 4. 索引が退避ディレクトリの外を指していても読みに行かない
        let outside = dir.join("秘密.txt");
        std::fs::write(&outside, "見えてはいけない").unwrap();
        let evil = format!(
            "[[entries]]\npath = \"\"\ntitle = \"外\"\nbody = \"../秘密.txt\"\nlen = {}\nhash = \"{:016x}\"\ndisk_hash = \"0000000000000000\"\ndisk_existed = false\n",
            "見えてはいけない".len(),
            crate::editor::hash_str("見えてはいけない")
        );
        std::fs::write(store_dir.join("index.toml"), evil).unwrap();
        assert!(
            load_hotexit(&store_dir).is_empty(),
            "退避ディレクトリの外を読んでいる"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ディスク側が変わっていたら検出する() {
        let dir = hotexit_tmp("conflict");
        let store_dir = dir.join("store");
        let same = dir.join("そのまま.rs");
        let changed = dir.join("書き換えられた.rs");
        let gone = dir.join("消される.rs");
        for (p, t) in [
            (&same, "元の中身\n"),
            (&changed, "元の中身\n"),
            (&gone, "元の中身\n"),
        ] {
            std::fs::write(p, t).unwrap();
        }
        let base = crate::editor::hash_str("元の中身\n");
        fn mk<'a>(id: u64, p: &'a Path, t: &'a str, base: u64) -> HotExitSnapshot<'a> {
            HotExitSnapshot {
                id,
                path: Some(p),
                title: "t",
                text: t,
                saved_hash: base,
            }
        }
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        store.sync(&[
            mk(1, &same, "編集した中身\n", base),
            mk(2, &changed, "編集した中身\n", base),
            mk(3, &gone, "編集した中身\n", base),
        ]);

        // 退避のあと、外からディスクを動かす
        std::fs::write(&changed, "外から書き換えた\n").unwrap();
        std::fs::remove_file(&gone).unwrap();

        let got = load_hotexit(&store_dir);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].disk, DiskState::Same);
        assert!(!got[0].disk.needs_choice());
        assert_eq!(got[1].disk, DiskState::Changed);
        assert_eq!(got[1].disk_text.as_deref(), Some("外から書き換えた\n"));
        assert!(got[1].disk.needs_choice(), "黙って戻してはいけない");
        assert_eq!(got[2].disk, DiskState::Missing);
        assert!(got[2].disk.needs_choice());
        // 未保存の本文はどの場合でも失われない
        assert!(got.iter().all(|r| r.text == "編集した中身\n"));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 一度も保存していない名前付きファイルは競合にしない() {
        let dir = hotexit_tmp("newfile");
        let store_dir = dir.join("store");
        // 「名前を付けたがまだ保存していない」= ディスクに実体が無い
        let never = dir.join("まだ無い.rs");
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        store.sync(&[snap(1, Some(&never), "まだ無い.rs", "書きかけ")]);
        let got = load_hotexit(&store_dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].disk, DiskState::Same);
        assert!(!got[0].disk.needs_choice());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 退避先はワークスペースごとに分かれ順序に依存しない() {
        let a = unique_temp_dir("zaivern-hotexit", "ws-a");
        let b = unique_temp_dir("zaivern-hotexit", "ws-b");
        let store = unique_temp_dir("zaivern-hotexit", "store");
        let ab = hotexit_dir_for_in(&store, &[a.clone(), b.clone()]);
        let ba = hotexit_dir_for_in(&store, &[b.clone(), a.clone()]);
        assert_eq!(ab, ba, "ルートの並び順で退避先が変わっている");
        assert_ne!(ab, hotexit_dir_for_in(&store, &[a.clone()]));
        // 置き場は必ず ~/.zaivern/hotexit 配下 (パスは dirs から導出する)
        assert!(hotexit_dir().starts_with(crate::config::zaivern_dir()));
        std::fs::remove_dir_all(&store).ok();
        std::fs::remove_dir_all(&a).ok();
        std::fs::remove_dir_all(&b).ok();
    }

    #[test]
    fn 再起動しても前回の退避ファイルが残り続けない() {
        let dir = hotexit_tmp("relaunch");
        let store_dir = dir.join("store");
        // 1 回目の起動: 未保存のまま落ちる
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        store.sync(&[snap(7, None, "a", "aaa"), snap(9, None, "b", "bbb")]);
        drop(store);

        // 2 回目の起動: 本文は読めるが、バッファ ID は必ず変わる
        let got = load_hotexit(&store_dir);
        assert_eq!(got.len(), 2);
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        store.sync(&[snap(1, None, "a", "aaa"), snap(2, None, "b", "bbb")]);

        // 前回の ID で書かれた本文が掃除されていること (ゴミが溜まらない)
        let files: Vec<String> = std::fs::read_dir(&store_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(files.len(), 3, "余分なファイルが残っている: {files:?}");
        assert!(files.iter().any(|f| f == "index.toml"));
        assert!(!files
            .iter()
            .any(|f| f.starts_with(&format!("{:016x}", 7u64))));
        assert_eq!(load_hotexit(&store_dir).len(), 2);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn 退避を捨てるとディレクトリごと消える() {
        let dir = hotexit_tmp("discard");
        let store_dir = dir.join("store");
        let mut store = HotExitStore::new(store_dir.clone(), BIG);
        store.sync(&[snap(1, None, "a", "aaa")]);
        assert!(store_dir.exists());
        store.clear();
        assert!(!store_dir.exists());
        assert!(load_hotexit(&store_dir).is_empty());
        // 捨てたあとにまた退避できる (状態が壊れていない)
        assert_eq!(store.sync(&[snap(1, None, "a", "aaa")]).wrote, 1);
        assert_eq!(load_hotexit(&store_dir).len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_save_then_load() {
        let dir = unique_temp_dir("zaivern-session-test", "roundtrip");
        let workspace = dir.join("my-workspace");
        let data = SessionData {
            open_files: vec![
                "/Users/alice/project/src/main.rs".into(),
                "/Users/alice/project/README.md".into(),
                "/does/not/exist.rs".into(), // 存在しないパスもそのまま保存される
            ],
            active: Some(1),
            sidebar_open: true,
            panel_open: false,
            roots: Vec::new(),
            ..Default::default()
        };

        save_to(&dir, std::slice::from_ref(&workspace), &data);
        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("session should load");

        assert_eq!(loaded.open_files, data.open_files);
        assert_eq!(loaded.active, Some(1));
        assert!(loaded.sidebar_open);
        assert!(!loaded.panel_open);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_with_active_none() {
        let dir = unique_temp_dir("zaivern-session-test", "none-active");
        let workspace = dir.join("ws-no-active");
        let data = SessionData {
            open_files: vec![],
            active: None,
            sidebar_open: false,
            panel_open: true,
            roots: Vec::new(),
            ..Default::default()
        };

        save_to(&dir, std::slice::from_ref(&workspace), &data);
        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("session should load");

        assert!(loaded.open_files.is_empty());
        assert_eq!(loaded.active, None);
        assert!(!loaded.sidebar_open);
        assert!(loaded.panel_open);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn roundtrip_with_japanese_paths() {
        let dir = unique_temp_dir("zaivern-session-test", "japanese");
        // ワークスペース自体も日本語パス（実在させて canonicalize 経路も通す）
        let workspace = dir.join("日本語ワークスペース").join("プロジェクト");
        std::fs::create_dir_all(&workspace).expect("create japanese workspace dir");
        let data = SessionData {
            open_files: vec![
                workspace.join("メモ帳.txt").to_string_lossy().into_owned(),
                workspace
                    .join("設計/仕様書.md")
                    .to_string_lossy()
                    .into_owned(),
            ],
            active: Some(0),
            sidebar_open: true,
            panel_open: true,
            roots: Vec::new(),
            ..Default::default()
        };

        save_to(&dir, std::slice::from_ref(&workspace), &data);
        let loaded = load_from(&dir, std::slice::from_ref(&workspace))
            .expect("japanese session should load");

        assert_eq!(loaded.open_files, data.open_files);
        assert_eq!(loaded.active, Some(0));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_missing_session_returns_none() {
        let dir = unique_temp_dir("zaivern-session-test", "missing");
        let workspace = dir.join("never-saved-workspace");

        assert!(load_from(&dir, std::slice::from_ref(&workspace)).is_none());
        // 保存先ディレクトリ自体が無い場合も None
        let ghost_dir = dir.join("no-such-dir");
        assert!(load_from(&ghost_dir, std::slice::from_ref(&workspace)).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn sidebar_tab_roundtrips_and_old_file_without_it_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "sidebar-tab");
        let workspace = dir.join("ws-tab");

        let data = SessionData {
            sidebar_tab: "git".into(),
            ..Default::default()
        };
        let roots = std::slice::from_ref(&workspace);
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.sidebar_tab, "git");

        // 旧バージョンのセッション (sidebar_tab フィールド無し) も読めること
        let old = "open_files = []\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert_eq!(loaded.sidebar_tab, "");
        assert!(loaded.sidebar_open);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn agents_roundtrip_and_old_file_without_agents_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "agents");
        let workspace = dir.join("ws-agents");
        let roots = std::slice::from_ref(&workspace);

        let data = SessionData {
            open_files: vec!["/p/main.rs".into()],
            agents: vec![
                AgentSessionRec {
                    preset_name: "Claude Code".into(),
                    title: "Claude Code".into(),
                    icon: "👾".into(),
                    command: "claude --dangerously-skip-permissions".into(),
                    cwd: "/p".into(),
                    log_file: "/logs/Claude_Code-1.log".into(),
                    // 分割レイアウト (リーフ = 生ログのパス)。
                    split: String::new(),
                    worktree_repo: String::new(),
                    worktree_branch: String::new(),
                },
                AgentSessionRec {
                    preset_name: "Codex".into(),
                    title: "Codex #2".into(),
                    icon: "💡".into(),
                    command: "codex".into(),
                    cwd: "/p/サブ".into(),
                    log_file: "/logs/Codex__2-2.log".into(),
                    split: String::new(),
                    worktree_repo: String::new(),
                    worktree_branch: String::new(),
                },
            ],
            ..Default::default()
        };
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.agents.len(), 2);
        assert_eq!(loaded.agents[0].preset_name, "Claude Code");
        assert_eq!(
            loaded.agents[0].command,
            "claude --dangerously-skip-permissions"
        );
        assert_eq!(loaded.agents[1].title, "Codex #2");
        assert_eq!(loaded.agents[1].cwd, "/p/サブ");
        assert_eq!(loaded.agents[1].log_file, "/logs/Codex__2-2.log");
        // open_files などの既存フィールドと共存できる (テーブル配列は最後)
        assert_eq!(loaded.open_files, vec!["/p/main.rs"]);

        // 旧バージョンのセッション ([[agents]] 無し) も読めること
        let old = "open_files = [\"/a.rs\"]\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert!(loaded.agents.is_empty(), "旧ファイルでは空の agents になる");
        assert_eq!(loaded.open_files, vec!["/a.rs"]);

        std::fs::remove_dir_all(&dir).ok();
    }

    /// worktree 隔離で起動したエージェントの記録が、保存 → 復元で往復すること。
    /// ここが落ちると「再起動したら自分の worktree に戻れない」= 隔離が壊れる。
    #[test]
    fn worktree隔離の記録が保存と復元で保たれる() {
        let dir = unique_temp_dir("zaivern-session-test", "agent-worktree");
        let roots = &[dir.join("ws")];
        // 日本語・空白入りのパスでも壊れないこと (TOML の文字列としてそのまま往復する)
        let wt_dir = "/親 フォルダ/repo-agent-claude-code-1";
        let data = SessionData {
            agents: vec![
                AgentSessionRec {
                    preset_name: "Claude Code".into(),
                    title: "Claude Code".into(),
                    command: "claude".into(),
                    cwd: wt_dir.into(),
                    worktree_repo: "/親 フォルダ/repo".into(),
                    worktree_branch: "agent/claude-code-1".into(),
                    ..Default::default()
                },
                // 隔離していないエージェントは空文字のまま (= 通常起動)
                AgentSessionRec {
                    preset_name: "Codex".into(),
                    cwd: "/親 フォルダ/repo".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.agents.len(), 2);
        assert_eq!(loaded.agents[0].cwd, wt_dir, "worktree の cwd へ戻る");
        assert_eq!(loaded.agents[0].worktree_repo, "/親 フォルダ/repo");
        assert_eq!(loaded.agents[0].worktree_branch, "agent/claude-code-1");
        assert!(loaded.agents[1].worktree_branch.is_empty(), "隔離なしは空");
        assert!(loaded.agents[1].worktree_repo.is_empty(), "隔離なしは空");

        // この欄を持たない旧ファイルでも読める (= 空文字 = 隔離なし扱い)
        let old = "open_files = []\n[[agents]]\npreset_name = \"Claude Code\"\ncwd = \"/p\"\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert_eq!(loaded.agents.len(), 1);
        assert!(loaded.agents[0].worktree_branch.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    /// エディタ分割の 1 行が `[[agents]]` (テーブル配列) と共存でき、
    /// この欄を持たない旧セッションも読めること。
    #[test]
    fn editor_split_line_roundtrips_and_old_file_without_it_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "editor-split");
        let roots = &[dir.join("ws")];
        let line = format!(
            "1{gs}0{fs}p0{fs}H:0.5{rs}L:p0{rs}L:p1{gs}p0{fs}0{fs}/p/a.rs{gs}p1{fs}0{fs}/p/b.rs",
            gs = '\u{1d}',
            fs = '\u{1f}',
            rs = '\u{1e}',
        );
        let data = SessionData {
            open_files: vec!["/p/a.rs".into(), "/p/b.rs".into()],
            editor_split: line.clone(),
            agents: vec![AgentSessionRec {
                preset_name: "Claude Code".into(),
                title: "Claude Code #1".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.editor_split, line, "分割の 1 行がそのまま戻る");
        assert_eq!(loaded.agents.len(), 1, "テーブル配列と共存できる");

        // この欄を持たない旧ファイルは空文字 (= 分割なし) になる
        let old = "open_files = [\"/a.rs\"]\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert_eq!(loaded.editor_split, "");

        std::fs::remove_dir_all(&dir).ok();
    }

    /// ピン留めは**再起動をまたいで残る**。旧セッションファイル (この欄が無い)
    /// でも空 = ピン留めなしとして読める。
    #[test]
    fn ピン留めは往復し古いセッションでも読める() {
        let dir = unique_temp_dir("zaivern-session-test", "pinned");
        let roots = &[dir.join("ws")];
        // パスはハードコードせず一時ディレクトリから組む (どの OS でも通る)
        let a = dir.join("a.rs").to_string_lossy().into_owned();
        let b = dir.join("b.rs").to_string_lossy().into_owned();
        let data = SessionData {
            open_files: vec![a.clone(), b.clone()],
            pinned_files: vec![b.clone()],
            agents: vec![AgentSessionRec {
                preset_name: "Claude Code".into(),
                ..Default::default()
            }],
            ..Default::default()
        };
        save_to(&dir, roots, &data);
        let loaded = load_from(&dir, roots).expect("session should load");
        assert_eq!(loaded.pinned_files, vec![b], "ピン留めがそのまま戻る");
        assert_eq!(loaded.open_files.len(), 2);
        assert_eq!(loaded.agents.len(), 1, "テーブル配列と共存できる");

        // この欄を持たない旧ファイルは空 (= ピン留めなし)
        let old = "open_files = [\"/a.rs\"]\nsidebar_open = true\npanel_open = false\n";
        std::fs::write(session_file_in(&dir, roots), old).expect("write old session");
        let loaded = load_from(&dir, roots).expect("old session should still load");
        assert!(loaded.pinned_files.is_empty());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_term_log_tail_concats_old_then_current() {
        let dir = unique_temp_dir("zaivern-termlog-test", "tail-concat");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("a-1.log");
        std::fs::write(log.with_extension("log.old"), b"old-part\n").unwrap();
        std::fs::write(&log, b"new-part\n").unwrap();
        // .old が先、本体が後 (時系列順)
        assert_eq!(read_term_log_tail(&log, 1024), b"old-part\nnew-part\n");
        // ログが無ければ空 (復元側はこれで resume を諦める)
        assert!(read_term_log_tail(&dir.join("ghost.log"), 1024).is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_term_log_tail_caps_at_line_boundary() {
        let dir = unique_temp_dir("zaivern-termlog-test", "tail-cap");
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("b-1.log");
        std::fs::write(&log, b"line-one\nline-two\nline-three\n").unwrap();
        // cap 位置が行の途中なら、次の改行まで捨ててから返す
        let tail = read_term_log_tail(&log, 14); // "-two\nline-three\n" の途中
        assert_eq!(tail, b"line-three\n");
        // cap がファイルより大きければ全量そのまま
        let all = read_term_log_tail(&log, 4096);
        assert_eq!(all, b"line-one\nline-two\nline-three\n");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// **置き場の名前は `history` の写像そのもの**。ここで別に計算していたら、
    /// 同じフォルダに層の数だけ台帳ができて片方だけが静かに消える。
    #[test]
    fn 置き場のキーは_history_の写像そのもの() {
        let base = unique_temp_dir("zaivern-key-check", "map");
        let a = base.join("a");
        let store = base.join("store");

        // 生ログは単一パスのキー
        assert_eq!(
            term_log_dir_in(&store, &a),
            store
                .join("term_logs")
                .join(crate::history::workspace_key(&a)),
        );
        // セッションと Hot Exit はルート集合のキー
        let roots = [a.clone()];
        assert_eq!(
            session_file_in(&store, &roots),
            store.join(format!(
                "{}.toml",
                crate::history::workspace_set_key(&roots)
            )),
        );
        assert_eq!(
            hotexit_dir_for_in(&store, &roots),
            store.join(crate::history::workspace_set_key(&roots)),
        );
        // 別のフォルダは別の置き場
        assert_ne!(
            term_log_dir_in(&store, &a),
            term_log_dir_in(&store, &base.join("b"))
        );
        std::fs::remove_dir_all(&base).ok();
    }

    /// 引き取りで `term_logs/<キー>/` の名前が変わっても、**前回ログを見失わない**。
    /// 保存されている `log_file` は絶対パスなので、ここを繋ぎ直さないと
    /// 「再起動したらスクロールバックが消えた」として表に出る。
    #[test]
    fn 引き取りで置き場が変わっても前回ログを見失わない() {
        let base = unique_temp_dir("zaivern-repoint", "logs");
        let now = base.join("term_logs").join("fedcba9876543210");
        let gone = base.join("term_logs").join("0123456789abcdef");
        std::fs::create_dir_all(&now).expect("mkdir");
        std::fs::write(now.join("Claude-1.log"), "前回の画面\n").expect("log");

        let rec = |p: &Path| SessionData {
            agents: vec![AgentSessionRec {
                log_file: p.to_string_lossy().into_owned(),
                ..Default::default()
            }],
            ..Default::default()
        };

        // 旧い置き場を指していたら、同じ名前のログがある今の置き場へ繋ぎ直す
        let mut moved = rec(&gone.join("Claude-1.log"));
        repoint_log_files_in(&mut moved, &now);
        assert_eq!(
            PathBuf::from(&moved.agents[0].log_file),
            now.join("Claude-1.log")
        );

        // 実在するパスには触らない (利用者が別の場所を指していたらそれが真実源)
        let mut kept = rec(&now.join("Claude-1.log"));
        let before = kept.agents[0].log_file.clone();
        repoint_log_files_in(&mut kept, &base.join("term_logs").join("nowhere"));
        assert_eq!(kept.agents[0].log_file, before);

        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn roots_hash_is_order_independent() {
        let base = unique_temp_dir("zaivern-roots", "order");
        let store = base.join("store");
        let (a, b, c) = (base.join("a"), base.join("b"), base.join("c"));

        let file = |roots: &[PathBuf]| session_file_in(&store, roots);
        let ab = file(&[a.clone(), b.clone()]);
        assert_eq!(
            ab,
            file(&[b.clone(), a.clone()]),
            "ルート集合が同じなら順序が違っても同じセッション"
        );
        // 重複は畳まれる
        assert_eq!(file(&[a.clone(), b.clone(), a.clone()]), ab);
        // 集合が違えば別キー
        assert_ne!(file(&[a.clone(), b, c]), ab);
        assert_ne!(file(std::slice::from_ref(&a)), ab);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn multi_root_session_roundtrip_ignores_order() {
        let dir = unique_temp_dir("zaivern-session-test", "multi");
        let a = dir.join("proj-a");
        let b = dir.join("proj-b");
        std::fs::create_dir_all(&a).expect("mkdir a");
        std::fs::create_dir_all(&b).expect("mkdir b");

        let data = SessionData {
            open_files: vec![
                a.join("src/main.rs").to_string_lossy().into_owned(),
                b.join("index.js").to_string_lossy().into_owned(),
            ],
            active: Some(1),
            sidebar_open: true,
            panel_open: true,
            roots: vec![
                a.to_string_lossy().into_owned(),
                b.to_string_lossy().into_owned(),
            ],
            ..Default::default()
        };

        save_to(&dir, &[a.clone(), b.clone()], &data);
        // 順序を入れ替えても同じセッションが読める
        let loaded = load_from(&dir, &[b.clone(), a.clone()]).expect("session should load");
        assert_eq!(loaded.open_files, data.open_files);
        assert_eq!(loaded.roots.len(), 2, "ルート一覧そのものも永続化される");

        // 片方だけのワークスペースは別セッション扱い
        assert!(load_from(&dir, std::slice::from_ref(&a)).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn legacy_single_root_session_still_loads() {
        let dir = unique_temp_dir("zaivern-session-test", "legacy");
        let workspace = dir.join("old-ws");
        std::fs::create_dir_all(&workspace).expect("mkdir ws");

        // v0.1.3 以前の形式で書かれたファイルを手で置く
        let legacy = SessionData {
            open_files: vec!["/old/a.rs".into()],
            active: Some(0),
            sidebar_open: true,
            panel_open: false,
            roots: Vec::new(),
            ..Default::default()
        };
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(
            dir.join(format!(
                "{}.toml",
                crate::history::workspace_key(&workspace)
            )),
            toml::to_string_pretty(&legacy).expect("serialize"),
        )
        .expect("write legacy session");

        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("legacy session should load");
        assert_eq!(loaded.open_files, vec!["/old/a.rs"]);
        assert!(loaded.roots.is_empty(), "旧形式に roots は無い");

        // 複数ルートになると旧形式へはフォールバックしない（別ワークスペース扱い）
        assert!(load_from(&dir, &[workspace, dir.join("other")]).is_none());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_overwrites_existing_session() {
        let dir = unique_temp_dir("zaivern-session-test", "overwrite");
        let workspace = dir.join("ws");

        let first = SessionData {
            open_files: vec!["/old.rs".into()],
            active: Some(0),
            sidebar_open: false,
            panel_open: false,
            roots: Vec::new(),
            ..Default::default()
        };
        save_to(&dir, std::slice::from_ref(&workspace), &first);

        let second = SessionData {
            open_files: vec!["/new.rs".into(), "/new2.rs".into()],
            active: Some(1),
            sidebar_open: true,
            panel_open: true,
            roots: Vec::new(),
            ..Default::default()
        };
        save_to(&dir, std::slice::from_ref(&workspace), &second);

        let loaded =
            load_from(&dir, std::slice::from_ref(&workspace)).expect("session should load");
        assert_eq!(loaded.open_files, vec!["/new.rs", "/new2.rs"]);
        assert_eq!(loaded.active, Some(1));

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── ターミナルログのパスと掃除 ──────────────────────────────────

    #[test]
    fn term_log_path_sanitizes_title() {
        let p = term_log_path(Path::new("/tmp"), 7, "Claude Code #2 (全自動)/危険");
        let name = p.file_name().unwrap().to_string_lossy().into_owned();
        // パス区切りや空白は _ に置き換わり、id で一意になる
        assert!(name.ends_with("-7.log"), "{name}");
        assert!(!name.contains('/') && !name.contains(' '), "{name}");
    }

    #[test]
    fn prune_term_logs_keeps_newest() {
        use std::time::Duration;
        let ws = crate::test_util::unique_temp_dir("zaivern-termlog-test", "prune");
        let dir = term_log_dir_in(&ws.join("zdir"), &ws);
        std::fs::create_dir_all(&dir).unwrap();
        // mtime 差を付けて 4 本作る
        for i in 0..4u64 {
            let p = dir.join(format!("t-{i}.log"));
            std::fs::write(&p, format!("log {i}")).unwrap();
            let t = std::time::SystemTime::now() - Duration::from_secs((4 - i) * 10);
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_modified(t).unwrap();
        }
        prune_term_logs_in(&dir, 2);
        let left = list_term_logs_in(&dir);
        assert_eq!(left.len(), 2, "新しい 2 本だけ残る");
        let names: Vec<String> = left
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert!(names.contains(&"t-3.log".to_string()), "{names:?}");
        assert!(names.contains(&"t-2.log".to_string()), "{names:?}");
        std::fs::remove_dir_all(&dir).ok();
        std::fs::remove_dir_all(&ws).ok();
    }
}
