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

// ── ワークスペースキー ──────────────────────────────────────
//
// このキーは履歴だけのものではない。リース台帳 (`lease::ledger_path`)・
// 競合ゼロの予約 (`czero`)・ローカル履歴 (`local_history`)・mesh のスコープが
// **同じ関数**から場所を決めている。つまりキーが 1 ビットでも変われば、
// 利用者から見て「台帳もセッションもログも全部消えた」ことになる。
// したがってこのキーに求められる性質は 1 つだけ:
//
//   **同じフォルダなら、いつ・どの版のバイナリで計算しても同じ 16 桁になる。**

/// 大文字小文字を畳むか。**既定のファイルシステムが大小を区別しない OS だけ** true。
///
/// macOS (APFS / HFS+ の既定) と Windows (NTFS の既定) では `MyRepo` と `myrepo` が
/// **同じフォルダ**なので、畳まないと同じフォルダに 2 つの台帳ができてしまう
/// (= 排他が効かず、2 体のエージェントが同じ行を同時に書ける)。
/// Linux の ext4 / xfs は大小を区別するので、畳んではいけない
/// (別々のフォルダが 1 つの台帳を共有して、いもしない相手に断られる)。
///
/// 畳むのは **ASCII の範囲だけ**。`str::to_lowercase` は Unicode の写像表を引くが、
/// **その表は rustc に同梱される Unicode の版で変わる** — つまり
/// `DefaultHasher` を捨てた理由と同じ「版に依存する」問題を連れ戻す。
/// ASCII の A-Z → a-z は未来永劫変わらない。
const FOLD_CASE: bool = cfg!(any(target_os = "windows", target_os = "macos"));

/// FNV-1a 64bit の初期値 (offset basis)。
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a 64bit の乗数 (prime)。
const FNV_PRIME: u64 = 0x100_0000_01b3;

/// FNV-1a 64bit。**仕様として値が固定された**ハッシュ。
///
/// `DefaultHasher` を使わない理由: std のドキュメントが
/// 「アルゴリズムと出力は Rust のリリース間で変わり得る」と明言している。
/// このキーはディスク上のディレクトリ名なので、rustc を上げた瞬間に
/// 全ワークスペースの台帳・履歴・ログが**行方不明になる** (消えたことにも気付けない)。
///
/// FNV-1a を選んだのは (1) 依存を増やさず 5 行で書ける (2) 公開されたテストベクタで
/// 実装の正しさを外部から検証できる (3) 既に `spec.rs` が同じ理由で採っている、の 3 点。
/// 暗号用途ではない — 求めているのは秘匿性ではなく**再現性**である。
pub(crate) fn fnv1a64(bytes: &[u8]) -> u64 {
    fnv1a64_seeded(FNV_OFFSET, bytes)
}

/// 途中状態から続きを混ぜる FNV-1a。`fnv1a64` は初期値から始めた版。
fn fnv1a64_seeded(seed: u64, bytes: &[u8]) -> u64 {
    let mut h = seed;
    for b in bytes {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

/// 少しずつ食わせる FNV-1a 64bit。**一度に読めない大きさ**のために置いてある。
///
/// `fnv1a64` と**同じ値を出す** — 実装を 2 つ持たないよう、1 塊ぶんの混ぜ方は
/// [`fnv1a64_seeded`] を共有する (ずれると「全体で取った指紋」と
/// 「分割して取った指紋」が食い違い、同じ内容が変更に見える)。
/// 同値であることは `history::tests::分割して食わせても一括と同じ値になる` が固定する。
#[derive(Clone, Copy, Debug)]
pub(crate) struct Fnv1a64 {
    h: u64,
}

impl Default for Fnv1a64 {
    fn default() -> Self {
        Self { h: FNV_OFFSET }
    }
}

impl Fnv1a64 {
    /// 続きを混ぜる。
    pub(crate) fn update(&mut self, bytes: &[u8]) {
        self.h = fnv1a64_seeded(self.h, bytes);
    }

    /// いまの値。
    pub(crate) fn finish(self) -> u64 {
        self.h
    }
}

/// 作業フォルダ → 16 桁 hex のワークスペースキー。
///
/// 正規化の規則は [`normalized_workspace`] に、置き換えの経緯と移行は
/// `docs/workspace-key.md` に書いてある。
pub fn workspace_key(cwd: &Path) -> String {
    let (text, raw) = normalized_workspace(cwd);
    key_of_normalized(&text, raw.as_deref())
}

/// 正規化済み文字列 (と、必要なら生バイト列) → 16 桁 hex。
///
/// ファイルシステムに触らない純関数として切り出してあるのは、
/// **入力 → 出力の対応をテストで固定する**ため。どの OS のどのマシンでも
/// 同じ値になることを、実在するフォルダ無しで確かめられる。
fn key_of_normalized(text: &str, raw: Option<&[u8]>) -> String {
    format!("{:016x}", fnv1a64(&normalized_bytes(text, raw)))
}

/// 正規化済み文字列 (と、あれば生バイト) → ハッシュへ流す 1 本のバイト列。
///
/// 区切りの 0 バイトを挟むのは、文字列側と生バイト側の境界をずらした別の入力が
/// 同じ列にならないようにするため。
///
/// **集合版 ([`workspace_set_key`]) もこの列を要素として使う。**
/// 「1 つのワークスペースをどうバイト列にするか」の決定はここ 1 箇所しかない。
fn normalized_bytes(text: &str, raw: Option<&[u8]>) -> Vec<u8> {
    let mut v = text.as_bytes().to_vec();
    if let Some(raw) = raw {
        v.push(0);
        v.extend_from_slice(raw);
    }
    v
}

/// ルート**集合** → 16 桁 hex。マルチルートのセッション / Hot Exit の置き場を決める。
///
/// 単一の [`workspace_key`] と同じ正規化・同じ FNV-1a を通すが、**畳み方が違うので
/// 1 要素の集合と単一キーは別の値になる**。これは意図した設計で、両者は別のものに
/// 名前を付けている (「このフォルダの台帳」と「このルート集合のセッション」) 。
/// わざと一致させると、`sessions/` の中で旧形式 (単一パス) のファイルと
/// 新形式 (集合) のファイルが同じ名前を取り合う。
///
/// 満たすべき性質は 2 つ:
///
/// 1. **順序に依らない。** 並べ替えてから畳むので `[A, B]` と `[B, A]` は同じ。
///    重複も畳む (`[A, A]` は `[A]`)。
/// 2. **要素に何が入っていても境界が一意に決まる。** 区切り文字で繋ぐと、
///    その文字を含むパス名 (unix では改行すら合法) で別の集合が同じ列になる。
///    そこで**長さを 10 進で前置する** (netstring と同じ) 。長さの後ろの `:` まで
///    含めて数えれば、どこで要素が切れるかが構造的に決まるので衝突しない。
pub(crate) fn workspace_set_key(roots: &[PathBuf]) -> String {
    let mut items: Vec<Vec<u8>> = roots
        .iter()
        .map(|p| {
            let (text, raw) = normalized_workspace(p);
            normalized_bytes(&text, raw.as_deref())
        })
        .collect();
    items.sort();
    items.dedup();
    let mut h = FNV_OFFSET;
    for it in &items {
        h = fnv1a64_seeded(h, it.len().to_string().as_bytes());
        h = fnv1a64_seeded(h, b":");
        h = fnv1a64_seeded(h, it);
    }
    format!("{h:016x}")
}

/// キーを取る前にパスへ当てる正規化。**規則はこの 5 つだけ**:
///
/// 1. **シンボリックリンクと `..` を解決する** ([`crate::pathx::canonical`])。
///    Windows の `\\?\` 接頭辞もここで落ちる — 付いたり付かなかったりする接頭辞を
///    そのままハッシュすると、**フォルダが実在するかどうかでキーが変わる**。
/// 2. **相対パスは現在の作業ディレクトリで絶対化する**。`zai .` と `zai /path/to/x` が
///    別のキーになってはいけない。1 が成功していれば既に絶対なので、これが効くのは
///    解決できなかったとき (存在しない / 権限が無い) だけ。
/// 3. **区切りを `/` に揃え、`.`・空・末尾の区切りを畳む** ([`lexical_clean`])。
///    `\` を写すのは **Windows だけ** — unix では `\` は普通のファイル名文字なので、
///    触ると別々のフォルダが同じキーになる。
/// 4. **大小を畳む** — ただし [`FOLD_CASE`] が真の OS でだけ。
/// 5. **Unicode で表せないパス名は生バイトも混ぜる**。`to_string_lossy` は
///    不正なバイトを U+FFFD へ潰すので、**別々のフォルダが同じ文字列になり得る**
///    (Linux では latin-1 のファイル名が普通に存在する)。潰れたときだけ生バイトを
///    足して取り違えを防ぐ。返り値の 2 番目が `Some` になるのはその場合だけ。
fn normalized_workspace(cwd: &Path) -> (String, Option<Vec<u8>>) {
    let resolved = resolve_workspace(cwd);
    let lossy = resolved.to_str().is_none();
    let mut text = lexical_clean(&resolved.to_string_lossy());
    if FOLD_CASE {
        text.make_ascii_lowercase();
    }
    (text, lossy.then(|| raw_path_bytes(&resolved)))
}

/// シンボリックリンクを解いた実体のパス。**存在しないフォルダでも諦めない**。
///
/// 段は 3 つ。上から順に試して、成功したところで止まる:
///
/// 1. そのまま `canonicalize`。ふつうはここで終わる。
/// 2. 字面で `..` などを畳んでからもう一度 `canonicalize`。
///    `<実在するフォルダ>/no-such/..` のような書き方を救う。
/// 3. **実在する最も深い祖先まで戻って解決し、残りを継ぎ足す。**
///
/// 3 段目が要るのは、まだ作っていないフォルダを指したときに
/// **シンボリックリンクの差だけが吸収されない**ため。macOS では
/// `$TMPDIR` が `/var/…` (実体は `/private/var/…`) なので必ず踏み、
/// 「フォルダを作った瞬間にキーが変わる = 台帳が別物になる」形で出る。
///
/// 字面を畳む段は **UTF-8 として読めるときだけ**通す。読めないパス名を
/// `to_string_lossy` で往復させると U+FFFD へ潰れて別のフォルダと混ざる。
fn resolve_workspace(cwd: &Path) -> PathBuf {
    if let Ok(c) = cwd.canonicalize() {
        return crate::pathx::plain(c);
    }
    let abs = absolutize(cwd.to_path_buf());
    let lex = match abs.to_str() {
        Some(s) => PathBuf::from(lexical_clean(s)),
        None => abs,
    };
    if let Ok(c) = lex.canonicalize() {
        return crate::pathx::plain(c);
    }
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur: PathBuf = lex.clone();
    while let (Some(parent), Some(name)) = (cur.parent(), cur.file_name()) {
        tail.push(name.to_os_string());
        let parent = parent.to_path_buf();
        if let Ok(c) = parent.canonicalize() {
            let mut out = crate::pathx::plain(c);
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        cur = parent;
    }
    lex
}

/// 相対パスを現在の作業ディレクトリで絶対化する。取れなければそのまま返す。
fn absolutize(p: PathBuf) -> PathBuf {
    if p.is_absolute() {
        return p;
    }
    match std::env::current_dir() {
        Ok(d) => d.join(p),
        Err(_) => p,
    }
}

/// 区切りを `/` に揃え、`.` / `..` / 連続する区切り / 末尾の区切りを畳む。
///
/// `canonicalize` が成功していればここは素通りに近いが、失敗したときの
/// 入力 (`./repo/` や `repo//sub`) を同じ形へ寄せるのが役目。
/// UNC (`//server/share`) の先頭 2 本だけは Windows で意味があるので残す。
fn lexical_clean(raw: &str) -> String {
    let unified = if cfg!(windows) {
        raw.replace('\\', "/")
    } else {
        raw.to_string()
    };
    let lead = if cfg!(windows) && unified.starts_with("//") {
        "//"
    } else if unified.starts_with('/') {
        "/"
    } else {
        ""
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in unified.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if matches!(parts.last(), Some(&last) if last != "..") {
                    parts.pop();
                } else if lead.is_empty() {
                    // 絶対パスの根より上へは行けないので捨てる。
                    // 相対のままなら情報を落とさずに残す。
                    parts.push("..");
                }
            }
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if joined.is_empty() {
        return lead.to_string();
    }
    format!("{lead}{joined}")
}

/// パス名の生バイト列。OS の持ち方をそのまま取り出すので情報を落とさない。
fn raw_path_bytes(p: &Path) -> Vec<u8> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        p.as_os_str().as_bytes().to_vec()
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        p.as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect()
    }
    #[cfg(not(any(unix, windows)))]
    {
        p.to_string_lossy().into_owned().into_bytes()
    }
}

/// 旧キー (v0.14.0 まで)。`canonicalize` したパス文字列を [`DefaultHasher`] で
/// 叩いた 16 桁 hex。**新しく書くことは無い** — 既存のデータを引き取るためだけに残す。
///
/// この関数が過去の値を再現できるのは「データを書いた版と同じ `DefaultHasher` を
/// 持つ rustc でビルドされている間」だけである。std は版をまたぐ安定性を保証して
/// いないので、**移行を先送りするほど引き取れる保証が薄くなる**。
/// だから [`adopt_keys_in`] による引き取りを今入れてある。
pub(crate) fn legacy_workspace_key(cwd: &Path) -> String {
    let resolved = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = DefaultHasher::new();
    resolved.to_string_lossy().hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 旧キーその 2 (`session.rs` の `workspace_hash` / `marks.rs` の私的複製)。
///
/// **`legacy_workspace_key` と混同しないこと。** あちらは `canonicalize` した
/// パスの**文字列**を、こちらは **`Path` そのもの**を `DefaultHasher` へ流す。
/// `Path: Hash` は構成要素ごとに書き込むので**同じフォルダでも別の値**になる
/// (実測: `7d04257970e725eb` と `be6ef641440bbada`)。
/// `~/.zaivern/term_logs/<key>/` と `~/.zaivern/bookmarks/<key>.toml` がこの値だった。
pub(crate) fn legacy_path_key(cwd: &Path) -> String {
    let resolved = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = DefaultHasher::new();
    resolved.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 旧キーその 3 (`session.rs` の `roots_hash`)。ルート集合版。
///
/// `canonicalize` → 文字列化 → ソート → 重複除去 した `Vec<String>` を
/// `DefaultHasher` へ流していた。`~/.zaivern/sessions/<key>.toml` と
/// `~/.zaivern/hotexit/<key>/` がこの値。**新しく書くことは無い。**
pub(crate) fn legacy_roots_key(roots: &[PathBuf]) -> String {
    let mut keys: Vec<String> = roots
        .iter()
        .map(|p| {
            p.canonicalize()
                .unwrap_or_else(|_| p.to_path_buf())
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    keys.sort();
    keys.dedup();
    let mut hasher = DefaultHasher::new();
    keys.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// あるフォルダについて過去に使われていたキーの全部。
///
/// **層ごとに旧キーが違った**ので、引き取りは 1 本のリストで受ける。
/// ここに足せば、キーで場所を決める全ての層がまとめて移行される。
pub(crate) fn legacy_keys_of(cwd: &Path) -> Vec<String> {
    vec![legacy_workspace_key(cwd), legacy_path_key(cwd)]
}

// ── 旧キーの引き取り ────────────────────────────────────────
//
// **なぜ「旧キーも読む」ではなく「改名する」のか。**
// 読み側で両対応にするには、キーを使う側 (`lease` / `czero` / `local_history` /
// `history`) がそれぞれ「新の場所を見て、無ければ旧の場所を見る」を書く必要がある。
// 置き場の形も違う (`<key>.json` / `<key>/` / `<agent>/<key>.jsonl`) ので、
// **同じ分岐が 4 箇所に増え、1 つ書き忘れた場所だけが静かにデータを失う**。
// しかも旧キーは `DefaultHasher` 由来なので、読み側の分岐を残す限り
// 「rustc を上げたら読めなくなる」性質を永久に抱え込む。
//
// 改名なら 1 回で終わり、以後どの層も新しいキーだけを見ればよい。
// 名前を突き合わせるだけなので**置き場の形を知らずに済み**、将来
// キーで場所を決める層が増えても自動的に面倒を見る。

/// `~/.zaivern` 配下を何段まで潜って旧キーを探すか。
/// 実際の最深は `history/<agent>/<key>.jsonl` の 3 段なので 3 で足りる。
const LEGACY_SCAN_DEPTH: usize = 3;

/// 走査するエントリ数の上限。壊れた / 巨大なディレクトリで起動が止まらないための保険。
const LEGACY_SCAN_MAX_ENTRIES: usize = 4096;

/// 16 桁の hex か (= 誰かのワークスペースキーらしい名前か)。
fn looks_like_key(name: &str) -> bool {
    name.len() == 16 && name.bytes().all(|b| b.is_ascii_hexdigit())
}

/// 旧キーで置かれた台帳・履歴・生ログ・印を、新キーの名前へ引き取る。
///
/// 層ごとに旧キーが違ったので (`str` を叩いた版 / `Path` を叩いた版 /
/// ルート集合版) **旧キーの集まり**で受け、まとめて 1 回の走査で引き取る。
/// 走査は名前しか見ないので、置き場の形 (`<key>.json` / `<key>/` /
/// `<agent>/<key>.jsonl`) を知らずに済み、**将来キーで場所を決める層が
/// 増えても自動的に面倒を見る**。
///
/// `zdir` を引数で受けるのは、テストが実 `~/.zaivern` に一切触らずに
/// 検証できるようにするため (このモジュールの `*_in()` 系と同じ流儀)。
///
/// **安全側の作り**:
/// * 新しい名前が既にあれば**何もしない**。古い方も消さずに残す
///   (利用者のデータを黙って捨てない。人が見て判断できる状態で置いておく)
/// * `rename` は原子的。複数インスタンスが同時に走っても、負けた側は
///   「元が無い」/「先が在る」で失敗するだけで、壊れた中間状態にならない
/// * **Windows の delete pending / ACCESS_DENIED (os error 5) は異常ではない** —
///   誰かがそのファイルを開いている間は改名できないので、黙って諦めて
///   次の起動でやり直す。ここで騒ぐと「いちばん使っているワークスペースだけ
///   移行できない」形で表に出る
/// * 旧キー = 新キー (理論上ありえないが) は先に落とす
///
/// 戻り値は引き取った先のパス。テストと、将来ログに出したいときのため。
pub(crate) fn adopt_keys_in(zdir: &Path, olds: &[String], new: &str) -> Vec<PathBuf> {
    let olds: Vec<&str> = olds
        .iter()
        .map(String::as_str)
        .filter(|o| *o != new)
        .collect();
    let mut moved = Vec::new();
    if olds.is_empty() {
        return moved;
    }
    let mut budget = LEGACY_SCAN_MAX_ENTRIES;
    adopt_scan(zdir, LEGACY_SCAN_DEPTH, &olds, new, &mut moved, &mut budget);
    moved
}

fn adopt_scan(
    dir: &Path,
    depth: usize,
    olds: &[&str],
    new: &str,
    moved: &mut Vec<PathBuf>,
    budget: &mut usize,
) {
    if depth == 0 {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let path = e.path();
        let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
            continue;
        };
        // `<key>` と `<key>.<拡張子>` の両方を拾う。拡張子は層ごとに違う
        // (`.json` / `.jsonl` / `.toml`) ので、こちらでは知らないまま持ち越す。
        let (stem, ext) = match name.split_once('.') {
            Some((s, x)) => (s, Some(x)),
            None => (name, None),
        };
        if olds.contains(&stem) {
            let dest = dir.join(match ext {
                Some(x) => format!("{new}.{x}"),
                None => new.to_string(),
            });
            // 新しい側が既にあるなら、そちらが本物。旧は触らず残す。
            if !dest.exists() && std::fs::rename(&path, &dest).is_ok() {
                moved.push(dest);
            }
            continue;
        }
        // 別のワークスペースの入れ物には降りない (中身は全部そのワークスペースのもの
        // なので、旧キーの名前は構造上あり得ない)。term_logs のように中が
        // 数千件になる置き場を読み切らずに済む。
        if looks_like_key(stem) {
            continue;
        }
        if path.is_dir() {
            adopt_scan(&path, depth - 1, olds, new, moved, budget);
        }
    }
}

/// 実 `~/.zaivern` に対する引き取り。**1 プロセス 1 ワークスペース 1 回**。
///
/// 履歴の読み書き入口 ([`append`] / [`list_all`]) から呼ぶ。ワークスペースを
/// 開いた最初の 1 回だけディレクトリを 7 つほど読み、以降は集合の照会で終わる。
/// [`workspace_key`] 自身に入れないのは、あれが**キーを組み立てるたびに**
/// (リース台帳の 1 操作ごとに) 呼ばれる純関数だからで、そこにファイル操作を
/// 隠すとテストが実 `~/.zaivern` を読むようにもなる。
pub(crate) fn adopt_legacy_keys(cwd: &Path) {
    adopt_keys(&legacy_keys_of(cwd), &workspace_key(cwd));
}

/// 実 `~/.zaivern` に対する [`adopt_keys_in`]。**プロセス内で組ごとに 1 回だけ**走る。
///
/// 旧キーの組と新キーで覚えるので、単一パスの引き取りとルート集合の引き取りが
/// 互いを打ち消さない。呼ぶのは**実ディレクトリを触る入口だけ** — テストは
/// `*_in()` 系を一時ディレクトリへ向けるので、ここを通らない。
pub(crate) fn adopt_keys(olds: &[String], new: &str) {
    static DONE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    let tag = format!("{}>{new}", olds.join(","));
    let done = DONE.get_or_init(Default::default);
    {
        // 毒された Mutex でも中身は壊れていない (入れるのは String だけ) ので、
        // そのまま使う。ここで panic すると履歴が読めなくなるほうが害が大きい。
        let mut set = done.lock().unwrap_or_else(|e| e.into_inner());
        if !set.insert(tag) {
            return;
        }
    }
    adopt_keys_in(&zaivern_dir(), olds, new);
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
    // 書く前に旧キーの置き土産を引き取る。順序が逆だと、引き取る前に
    // 新しいファイルを作ってしまい「新しい側が既にある」で移行が諦める。
    adopt_legacy_keys(Path::new(&entry.cwd));
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

/// [`finish`] の実体 (履歴ルートを引数で受ける)。
fn finish_in(
    root: &Path,
    agent_bin: &str,
    cwd: &Path,
    id: u64,
    ended: i64,
    brief: &str,
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
                // 要約が空なら既存の値を残す (締めのたびに消さない)。
                if !brief.is_empty() {
                    e.brief = brief_of(brief);
                }
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
/// **セッションを閉じるときの締め**: 終了時刻と要約をまとめて書き戻す。
///
/// `update_end` と分けているのは、要約 (最初のユーザー指示) が
/// **起動時点では存在しない**ため。`Session::last_prompt` が埋まるのは
/// 最初の指示を送った後なので、締めのタイミングで初めて書ける。
/// 要約が空なら既存の値を残す (上書きで消さない)。
pub fn finish(
    agent_bin: &str,
    cwd: &Path,
    id: u64,
    ended: i64,
    brief: &str,
) -> std::io::Result<()> {
    finish_in(&history_root(), agent_bin, cwd, id, ended, brief)
}

/// `history/` 配下の**全エージェント**の履歴を集めて新しい順で返す。
pub fn list_all(cwd: &Path) -> Vec<Entry> {
    // 一覧は「前回の続き」を出す画面が呼ぶ。ここで引き取らないと、旧キーの
    // 履歴を持つ利用者には**空の一覧**が出る (= 消えたように見える)。
    adopt_legacy_keys(cwd);
    list_all_in(&history_root(), cwd)
}

/// このエージェント × この作業フォルダの履歴を**新しい順**で返す。
///
/// 公開はしない — 一覧は [`list_all`] (全エージェント分をマージした版) だけを
/// 使うので、公開すると「作ったのに繋いでいない」API が増える。
/// 本番経路からは呼ばないので `cfg(test)` に閉じる。
#[cfg(test)]
fn list_in(root: &Path, agent_bin: &str, cwd: &Path) -> Vec<Entry> {
    let mut v = read_entries(&record_path_in(root, agent_bin, cwd));
    sort_newest_first(&mut v);
    v
}

/// 終了時刻だけを書き戻す ([`finish_in`] の要約なし版)。
/// 本番経路は要約も一緒に書く `finish` を通るので、こちらはテスト専用。
#[cfg(test)]
fn update_end_in(
    root: &Path,
    agent_bin: &str,
    cwd: &Path,
    id: u64,
    ended: i64,
) -> std::io::Result<()> {
    finish_in(root, agent_bin, cwd, id, ended, "")
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
        let root = history_root();
        assert!(root.starts_with(crate::config::zaivern_dir()));
        let dir = record_dir_in(&root, "claude");
        assert!(dir.starts_with(crate::config::zaivern_dir().join("history")));
        assert_eq!(dir.file_name().and_then(|s| s.to_str()), Some("claude"));
        let file = record_path_in(&root, "claude", &cwd);
        assert_eq!(file.parent(), Some(dir.as_path()));
        assert_eq!(
            file.extension().and_then(|s| s.to_str()),
            Some("jsonl"),
            "JSONL であること"
        );
    }

    // ── ワークスペースキーの安定性 ──────────────────────────

    #[test]
    fn fnv1a64は公開テストベクタと一致する() {
        // FNV の作者が公開している既知の値。**自前実装が本物の FNV-1a か**を
        // リポジトリの外の基準で確かめられるのが、この選択の利点。
        assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn 分割して食わせても一括と同じ値になる() {
        // 8MB 超のファイルは一度に読めないので分割して混ぜる。**分割の仕方で
        // 値が変わったら**、同じ内容のファイルが「変わった」と見える。
        let body: Vec<u8> = (0u32..5000).map(|i| (i % 251) as u8).collect();
        for chunk in [1usize, 2, 7, 64, 4096, 8192] {
            let mut inc = Fnv1a64::default();
            for part in body.chunks(chunk) {
                inc.update(part);
            }
            assert_eq!(
                inc.finish(),
                fnv1a64(&body),
                "{chunk} バイトずつ食わせた値が一括と違う"
            );
        }
        // 空でも初期値と一致すること (0 バイトのファイルが特別扱いにならない)。
        assert_eq!(Fnv1a64::default().finish(), fnv1a64(b""));
    }

    #[test]
    fn 正規化済みパスからキーへの写像を値ごと固定する() {
        // **この表が動いたら、利用者の台帳・履歴・ログが行方不明になる。**
        // 値は Python の独立実装で突き合わせて起こした。
        let table: &[(&str, &str)] = &[
            ("/home/u/proj", "143becae573131b5"),
            ("/", "af63a24c860189fe"),
            ("c:/users/u/proj", "e1f5d8f194c658cf"),
        ];
        for (input, want) in table {
            assert_eq!(&key_of_normalized(input, None), want, "入力 {input}");
        }
        // 生バイトを混ぜる側 (Unicode で表せないパス) も固定する。
        assert_eq!(
            key_of_normalized("/tmp/ws/\u{fffd}", Some(b"/tmp/ws/\xff")),
            "ba0cf83a33d8fe77"
        );
    }

    #[test]
    fn 字面の正規化は書き方の揺れを畳む() {
        let table: &[(&str, &str)] = &[
            ("/a/b", "/a/b"),
            ("/a/b/", "/a/b"),
            ("/a//b///", "/a/b"),
            ("/a/./b", "/a/b"),
            ("/a/b/..", "/a"),
            ("/a/b/../../c", "/c"),
            ("/..", "/"),
            ("/", "/"),
            ("", ""),
            ("a/b/", "a/b"),
            ("../a", "../a"),
        ];
        for (input, want) in table {
            assert_eq!(&lexical_clean(input), want, "入力 {input}");
        }
    }

    #[test]
    fn 同じフォルダは書き方が違っても同じキーになる() {
        let base = root("same-folder");
        let ws = base.join("ws");
        std::fs::create_dir_all(ws.join("sub")).expect("create ws/sub");
        let want = workspace_key(&ws);
        // 末尾の区切り / `.` / `..` / 実在しない中間要素、どれでも同じ。
        for variant in [
            ws.join(""),
            ws.join("."),
            ws.join("sub").join(".."),
            ws.join("no-such-dir").join(".."),
        ] {
            assert_eq!(workspace_key(&variant), want, "{}", variant.display());
        }
    }

    #[cfg(unix)]
    #[test]
    fn シンボリックリンク越しでも同じキーになる() {
        let base = root("symlink");
        let ws = base.join("ws");
        std::fs::create_dir_all(&ws).expect("create ws");
        let link = base.join("link");
        std::os::unix::fs::symlink(&ws, &link).expect("symlink");
        assert_eq!(workspace_key(&link), workspace_key(&ws));
    }

    #[test]
    fn 大小の畳み方は既定ファイルシステムに合わせる() {
        let base = root("case");
        // **実在させない**。実在すると canonicalize が OS 側で大小を直して
        // しまい、こちらの規則を検査したことにならない。
        let upper = base.join("MixedCase").join("Repo");
        let lower = base.join("mixedcase").join("repo");
        let (text, _) = normalized_workspace(&upper);
        if cfg!(any(target_os = "windows", target_os = "macos")) {
            assert!(text.ends_with("mixedcase/repo"), "畳んだ後: {text}");
            assert_eq!(
                workspace_key(&upper),
                workspace_key(&lower),
                "大小を区別しない FS では同じフォルダなので同じ台帳を使う"
            );
        } else {
            assert!(text.ends_with("MixedCase/Repo"), "畳まない: {text}");
            assert_ne!(
                workspace_key(&upper),
                workspace_key(&lower),
                "大小を区別する FS では別のフォルダなので別の台帳"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unicodeで表せないパス名でも取り違えない() {
        use std::os::unix::ffi::OsStringExt;
        let base = root("nonutf8");
        let bytes = |tail: u8| {
            let mut v = base.clone().into_os_string().into_vec();
            v.push(b'/');
            v.push(tail);
            PathBuf::from(std::ffi::OsString::from_vec(v))
        };
        // to_string_lossy はどちらも U+FFFD へ潰すので、文字列だけでは同じになる。
        let a = bytes(0xff);
        let b = bytes(0xfe);
        assert_eq!(a.to_string_lossy(), b.to_string_lossy());
        assert_ne!(workspace_key(&a), workspace_key(&b));
    }

    #[test]
    fn 新しいキーは_defaulthasher_を使っていない() {
        // Windows のチェックアウトは CRLF なので必ず正規化してから探す。
        let src = include_str!("history.rs").replace("\r\n", "\n");
        // 探す語を実行時に組み立てる。ソースへ直に書くと**このテスト自身が
        // 検出対象になり**、数が合わなくなる (実際にそれで落ちた)。
        let needle = format!("{}::new()", "DefaultHasher");
        assert_eq!(
            src.matches(&needle).count(),
            3,
            "{needle} は旧キー 3 種類 (str 版 / Path 版 / ルート集合版) の再現だけ"
        );
        let legacy = src
            .find("fn legacy_workspace_key")
            .expect("旧キー関数が残っている");
        assert!(
            src.match_indices(&needle).all(|(at, _)| at > legacy),
            "DefaultHasher が新しいキー側へ戻っている"
        );
    }

    /// **ワークスペースのキーを計算しているのはこのモジュールだけ。**
    ///
    /// 寄せる前は同じフォルダが 2 つの名前を持っていた (`history` の
    /// `7d04257970e725eb` と `session` / `marks` の `be6ef641440bbada`) 。
    /// 層ごとに別の写像があると**片方の層だけが静かにデータを失う**ので、
    /// 「他所で計算していない」ことを構造で固定する。
    ///
    /// 見張る形は 2 つ:
    ///
    /// 1. `format!("{{:016x}}", <ハッシュ>.finish())` — 自前ハッシュから 16 桁キーを作る形
    /// 2. `canonicalize()` の結果をそのままハッシュへ流す形 — つまり「パス → キー」
    #[test]
    fn ワークスペースキーを計算するのはこのモジュールだけ() {
        // 探す語は実行時に組み立てる。ソースへ直に書くと**このテスト自身が
        // 検出対象になる** (`新しいキーは_defaulthasher_を使っていない` で実際に踏んだ)。
        let hex16 = format!("{}{}", "{:016x}\"", ", ");
        let finish = format!("{}()", "finish");
        let canon = format!("{}()", "canonicalize");
        let fnv = format!("{}{}", "fnv1a", "64");
        let mut checked = 0usize;
        // パスはビルド時のクレート位置から起こす (どのマシンでも動く)。
        let mut stack = vec![std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
        while let Some(dir) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension().and_then(|s| s.to_str()) != Some("rs") {
                    continue;
                }
                // ここ (キーの本家) と、その旧キー再現だけは対象外。
                if path.file_name().and_then(|s| s.to_str()) == Some("history.rs") {
                    continue;
                }
                let Ok(raw) = std::fs::read_to_string(&path) else {
                    continue;
                };
                // Windows のチェックアウトは CRLF なので必ず正規化する。
                let text = raw.replace("\r\n", "\n");
                checked += 1;
                for (at, _) in text.match_indices(&hex16) {
                    // **文字数で切る。** バイト位置で切ると日本語コメントの
                    // 途中に落ちて panic する (実際に踏んだ)。
                    let tail: String = text[at..].chars().take(80).collect();
                    assert!(
                        !tail.contains(&finish),
                        "{}: 自前ハッシュから 16 桁キーを作っている。\n                         ワークスペースのキーなら crate::history::workspace_key を、\n                         集合なら workspace_set_key を通すこと。",
                        path.display()
                    );
                }
                for (at, _) in text.match_indices(&canon) {
                    // 囲っている関数の終わり (列 0 の `}`) まで、無ければ 800 字。
                    let end = text[at..].find("\n}").map_or(text.len(), |d| at + d);
                    let win: String = text[at..end].chars().take(800).collect();
                    assert!(
                        !win.contains(&finish) && !win.contains(&fnv),
                        "{}: canonicalize したパスを直接ハッシュしている (= 独自のワークスペースキー)。\n                         crate::history::workspace_key を通すこと。",
                        path.display()
                    );
                }
            }
        }
        assert!(checked > 50, "走査できたのが {checked} 個しかない");
    }

    // ── 旧キーの引き取り ────────────────────────────────────

    /// 単一パスの旧キー (`str` 版 / `Path` 版) をまとめて引き取る。
    fn adopt_legacy_keys_in(zdir: &Path, cwd: &Path) -> Vec<PathBuf> {
        adopt_keys_in(zdir, &legacy_keys_of(cwd), &workspace_key(cwd))
    }

    /// 旧キーで置かれた 3 つの形 (ファイル / 拡張子なしのディレクトリ / 2 段下) を作る。
    fn legacy_layout(zdir: &Path, old: &str) {
        std::fs::create_dir_all(zdir.join("lease")).expect("lease");
        std::fs::write(zdir.join("lease").join(format!("{old}.json")), "{}").expect("ledger");
        std::fs::create_dir_all(zdir.join("history").join("claude")).expect("history");
        std::fs::write(
            zdir.join("history")
                .join("claude")
                .join(format!("{old}.jsonl")),
            "{}\n",
        )
        .expect("jsonl");
        std::fs::create_dir_all(zdir.join("czero").join(old)).expect("czero");
        std::fs::write(zdir.join("czero").join(old).join("r.json"), "{}").expect("reservation");
    }

    #[test]
    fn 旧キーの台帳と履歴と予約を新キーへ引き取る() {
        let zdir = root("adopt-zdir");
        let ws = root("adopt-ws");
        let old = legacy_workspace_key(&ws);
        let new = workspace_key(&ws);
        assert_ne!(old, new, "置き換えたのだから値は変わっている");
        legacy_layout(&zdir, &old);

        let moved = adopt_legacy_keys_in(&zdir, &ws);
        assert_eq!(moved.len(), 3, "3 つとも引き取る: {moved:?}");
        assert!(zdir.join("lease").join(format!("{new}.json")).is_file());
        assert!(zdir
            .join("history")
            .join("claude")
            .join(format!("{new}.jsonl"))
            .is_file());
        assert!(zdir.join("czero").join(&new).join("r.json").is_file());
        // 旧い名前は残っていない (中身ごと移した)。
        assert!(!zdir.join("lease").join(format!("{old}.json")).exists());
    }

    #[test]
    fn 引き取りは何度走らせても同じ結果になる() {
        let zdir = root("adopt-idempotent");
        let ws = root("adopt-idempotent-ws");
        legacy_layout(&zdir, &legacy_workspace_key(&ws));
        assert_eq!(adopt_legacy_keys_in(&zdir, &ws).len(), 3);
        assert_eq!(
            adopt_legacy_keys_in(&zdir, &ws).len(),
            0,
            "2 回目は動かすものが無い"
        );
        // 同時に走った別インスタンスが先に済ませても壊れないこと (= 上と同じ状態)。
        let new = workspace_key(&ws);
        assert!(zdir.join("czero").join(&new).join("r.json").is_file());
    }

    #[test]
    fn 新しい側が既にあるなら旧いデータを消さない() {
        let zdir = root("adopt-keep");
        let ws = root("adopt-keep-ws");
        let old = legacy_workspace_key(&ws);
        let new = workspace_key(&ws);
        std::fs::create_dir_all(zdir.join("lease")).expect("lease");
        std::fs::write(zdir.join("lease").join(format!("{old}.json")), "old").expect("old");
        std::fs::write(zdir.join("lease").join(format!("{new}.json")), "new").expect("new");

        assert_eq!(adopt_legacy_keys_in(&zdir, &ws).len(), 0, "上書きしない");
        assert_eq!(
            std::fs::read_to_string(zdir.join("lease").join(format!("{new}.json"))).unwrap(),
            "new",
            "新しい側は無傷"
        );
        assert_eq!(
            std::fs::read_to_string(zdir.join("lease").join(format!("{old}.json"))).unwrap(),
            "old",
            "旧い側も黙って消さない"
        );
    }

    #[test]
    fn 別ワークスペースの入れ物と深すぎる場所へは踏み込まない() {
        let zdir = root("adopt-bounds");
        let ws = root("adopt-bounds-ws");
        let old = legacy_workspace_key(&ws);
        // 別ワークスペースのディレクトリの中 (中身は全部その持ち主のもの)。
        let other = "0123456789abcdef";
        assert!(looks_like_key(other));
        std::fs::create_dir_all(zdir.join("czero").join(other)).expect("other");
        let inside = zdir.join("czero").join(other).join(format!("{old}.json"));
        std::fs::write(&inside, "{}").expect("inside");
        // 深さ 4 (この置き場の形では存在しない深さ)。
        let deep = zdir.join("a").join("b").join("c");
        std::fs::create_dir_all(&deep).expect("deep");
        let deep_file = deep.join(format!("{old}.json"));
        std::fs::write(&deep_file, "{}").expect("deep file");

        assert_eq!(adopt_legacy_keys_in(&zdir, &ws).len(), 0);
        assert!(inside.is_file(), "他人の入れ物には降りない");
        assert!(deep_file.is_file(), "深さの上限で止まる");
    }

    // ── 層をまたぐ引き取り (寄せた分) ──────────────────────

    /// `session.rs` / `marks.rs` が使っていた旧キー (`Path` を叩いた値) も
    /// **同じ 1 回の走査で**引き取る。生ログは利用者の実データが入っているので、
    /// ここが抜けると「スクロールバックが全部消えた」として表に出る。
    #[test]
    fn 生ログと印の旧キーも同じ走査で引き取る() {
        let zdir = root("adopt-path-zdir");
        let ws = root("adopt-path-ws");
        let old = legacy_path_key(&ws);
        let new = workspace_key(&ws);
        assert_ne!(old, legacy_workspace_key(&ws), "Path 版と str 版は別の値");
        assert_ne!(old, new);

        // term_logs/<旧キー>/<ログ>  と  bookmarks/<旧キー>.toml
        std::fs::create_dir_all(zdir.join("term_logs").join(&old)).expect("term_logs");
        std::fs::write(
            zdir.join("term_logs").join(&old).join("Claude-1.log"),
            "前回の画面\n",
        )
        .expect("log");
        std::fs::create_dir_all(zdir.join("bookmarks")).expect("bookmarks");
        std::fs::write(
            zdir.join("bookmarks").join(format!("{old}.toml")),
            "version = 1\n",
        )
        .expect("marks");

        let moved = adopt_legacy_keys_in(&zdir, &ws);
        assert_eq!(moved.len(), 2, "生ログと印の両方: {moved:?}");
        assert_eq!(
            std::fs::read_to_string(zdir.join("term_logs").join(&new).join("Claude-1.log"))
                .expect("引き取った生ログが読める"),
            "前回の画面\n"
        );
        assert!(zdir.join("bookmarks").join(format!("{new}.toml")).is_file());
    }

    /// ルート集合のキー (セッション / Hot Exit) も同じ流儀で引き取れる。
    #[test]
    fn ルート集合の旧キーも引き取れる() {
        let zdir = root("adopt-roots-zdir");
        let a = root("adopt-roots-a");
        let b = root("adopt-roots-b");
        let roots = vec![a, b];
        let old = legacy_roots_key(&roots);
        let new = workspace_set_key(&roots);
        assert_ne!(old, new);

        std::fs::create_dir_all(zdir.join("sessions")).expect("sessions");
        std::fs::write(
            zdir.join("sessions").join(format!("{old}.toml")),
            "active = 0\n",
        )
        .expect("session");
        std::fs::create_dir_all(zdir.join("hotexit").join(&old)).expect("hotexit");
        std::fs::write(zdir.join("hotexit").join(&old).join("index.toml"), "").expect("index");

        let moved = adopt_keys_in(&zdir, &[old.clone()], &new);
        assert_eq!(moved.len(), 2, "セッションと退避の両方: {moved:?}");
        assert_eq!(
            std::fs::read_to_string(zdir.join("sessions").join(format!("{new}.toml")))
                .expect("引き取ったセッションが読める"),
            "active = 0\n"
        );
        assert!(zdir.join("hotexit").join(&new).join("index.toml").is_file());
    }

    // ── ルート集合のキー ────────────────────────────────────

    #[test]
    fn 集合のキーは順序と重複に依らない() {
        let a = root("set-a");
        let b = root("set-b");
        let c = root("set-c");
        let ab = workspace_set_key(&[a.clone(), b.clone()]);
        assert_eq!(ab, workspace_set_key(&[b.clone(), a.clone()]), "順序非依存");
        assert_eq!(
            ab,
            workspace_set_key(&[a.clone(), b.clone(), a.clone()]),
            "重複は畳む"
        );
        assert_ne!(ab, workspace_set_key(&[a.clone(), b.clone(), c]));
        assert_ne!(ab, workspace_set_key(std::slice::from_ref(&a)));
        assert_eq!(ab.len(), 16);
        assert!(ab.chars().all(|c| c.is_ascii_hexdigit()));
        // 空集合でも決定的な値を返す (呼ぶ側が panic しない)。
        assert_eq!(workspace_set_key(&[]).len(), 16);
    }

    /// **区切り文字で繋いでいたら衝突する組**で、実際に衝突しないことを見る。
    ///
    /// 素朴に `/` や改行で繋ぐと、要素の切れ目が要素の中身と区別できず
    /// **別の集合が同じキーになる**。長さを前置しているので起こらない。
    #[test]
    fn 集合のキーは要素の切れ目を取り違えない() {
        let base = root("set-sep");
        // 繋いだ文字列は同じでも、集合としては別物。
        let left = vec![base.join("a").join("b"), base.join("c")];
        let right = vec![base.join("a"), base.join("b").join("c")];
        assert_ne!(workspace_set_key(&left), workspace_set_key(&right));
        // パス名そのものに区切りが入っていても取り違えない (unix では改行も合法)。
        let one = vec![base.join("x\ny"), base.join("z")];
        let two = vec![base.join("x"), base.join("y\nz")];
        assert_ne!(workspace_set_key(&one), workspace_set_key(&two));
    }
}
