//! 🕸 メッシュ — エージェント / エディタ / 短命な `zai hook` を
//! **Erlang のプロセスとして同じ土俵に載せる**通信層。
//!
//! ## なぜ要るのか (この製品の心臓)
//!
//! 「エージェントを何体並列で走らせても、マージ衝突が 0 になる」を成立させる
//! ためには、**誰が何を触っているかを全員が同じ 1 つの事実で見ている**必要が
//! ある。ところが実際の登場人物は
//!
//!   * GUI (`zai` の描画プロセス) — 落ちていることもある
//!   * エージェント CLI (claude / codex / …) — 何体でも増える
//!   * ベンダーが起こす短命な `zai hook` — **数十 ms で消える**
//!
//! と寿命も所属もバラバラで、しかも**判定するのは GUI ではなくフック**である。
//! つまり「GUI が持っているメモリ上の状態」は共有の事実になり得ない。
//! そこで Erlang のプロセスモデル (Pid / メールボックス / link / monitor /
//! 監視ツリー) を**ファイルシステムの上に**そのまま作る。
//!
//! ## 採った Erlang のセマンティクス
//!
//! | 採用 | ここでの形 |
//! |---|---|
//! | Pid は「再起動したら別物」 | [`Pid::incarnation`] にプロセス開始時刻を入れる |
//! | 同じ送信者 → 同じ受信者は**送信順**に届く | 送信者ごとの連番 + [`order_envelopes`] |
//! | 全体順序は保証しない | 送信者が違えば順序は未定義。テストでもそう固定する |
//! | 受信は破壊的 / 選択受信 | [`Mesh::recv`] / [`Mesh::recv_match`] |
//! | `register/2` は 1 つの pid にしか付かない | [`Mesh::register`] は **fail-closed** |
//! | monitor → `DOWN` が届く | [`Msg::Down`] |
//! | link は双方向・異常終了だけ伝播 | [`Mesh::link`] / [`REASON_NORMAL`] |
//! | `trap_exit` があれば死なずに受け取る | [`SpawnOpts::trap_exit`] |
//! | 監視ツリーは `one_for_one` | [`Mesh::reap`] |
//! | "let it crash" | 死んだ pid の担当は [`Mesh::reap`] が**自動で解放**する |
//!
//! ## 意図的に採らなかったセマンティクス (やらないことの明記)
//!
//! * **`one_for_all` / `rest_for_one`**。1 体が落ちたら道連れに全部落とす戦略は、
//!   人が回している作業ツリーを巻き添えにするので持たない。再起動戦略は
//!   `one_for_one` 相当 (落ちた 1 体の後始末だけ) の 1 つだけ。
//! * **死んだ pid への送信を黙って捨てる**。Erlang の `!` は無反応だが、ここは
//!   CLI から撃つので「宛先が居ない」は終了コードで返す ([`MeshError::NoProc`])。
//!   黙って消えるのがいちばんデバッグしづらい。
//! * **link で相手の OS プロセスを自動的に kill する**。Erlang の VM は自分が
//!   起こしたプロセスしか殺せない。ここの相手は**他人が起こした OS プロセス**な
//!   ので、自動 kill は事故になる。代わりに
//!   - 自分が [`cli_main`] の `spawn -- <cmd>` で**起こした子**だけは
//!     [`crate::procx::kill_tree`] でツリーごと落とす (所有しているので安全)
//!   - 自分が起こしていない相手には `exit` 標識を置き、相手自身に降りてもらう
//!     ([`Mesh::exit_signal`])
//! * **行域の重なり判定**。`spec` は文字列のまま運ぶ (`src/a.rs#L10-40`)。
//!   重なりの調停は `region.rs` / `lease.rs` の仕事で、ここは
//!   **完全一致キーの相互排他**と**運搬**だけを持つ。
//!
//! ## 設計上の約束 (CLAUDE.md との対応)
//!
//! * **アイドル時のコストはゼロ**: レジストリのディレクトリが無ければ
//!   `stat` 1 回で戻る ([`Mesh::enabled`])。常時ポーリングしない。監視間隔は
//!   [`backoff`] で 2 秒 → 30 秒へ適応的に後退する。
//! * **UI スレッドを絶対にブロックしない**: GUI の入口は裏スレッド + チャネルで、
//!   描画は「いま手元にある値」を返す (古くてよい)。
//! * **パスを 1 文字もハードコードしない**: 置き場は
//!   `crate::config::zaivern_dir()` (= `dirs` 由来) から導く。テストは
//!   [`Mesh::open_at`] に一時ディレクトリを渡すので実 `~/.zaivern` に触れない。
//! * **決定性**: 出力に出る並びは全部 `BTreeMap` / `BTreeSet` / ソート済み `Vec`。
//!
//! ## 統合担当への申し送り
//!
//! CLI (`zai mesh …`) の入口は [`cli_main`] として公開してある。
//! `src/cli.rs` は共有ファイルなので**こちらでは配線していない** —
//! サブコマンドの分岐へ次の 1 行を足すと繋がる:
//!
//! ```ignore
//! "mesh" => return Some(crate::features::mesh::cli_main(&args[1..])),
//! ```
//!
//! 打鍵は要求しない (`keybinds.rs` を触らせないため)。パレットの
//! 「🕸 メッシュ」で開ける。

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::i18n::{tr, trf};

// ═══════════════════════════════════════════════════════════════════════════
//  0. 定数 — 数字は全部ここに集める
// ═══════════════════════════════════════════════════════════════════════════

/// 1 つのメールボックスに積める上限。超えたら**古い方を捨てて**
/// ギャップ標識を残す (設計原則 2: 隠れている処理は欠落ありでよいが、
/// 決してブロックさせない)。
///
/// 256 の根拠: 1 通は 200〜400 バイト程度なので、満杯でも 100KB 前後。
/// GUI が数分止まっていても取りこぼさず、放置されたメールボックスが
/// ディスクを食い潰すこともない大きさ。
pub const MAILBOX_MAX: usize = 256;

/// これを超えて心拍が来ていなければ「疑わしい」。**まだ刈らない。**
const STALE: Duration = Duration::from_secs(60);

/// これを超えたら、OS が「その PID は生きている」と言っていても死んだ扱いにする。
///
/// **時間だけに頼らないための線引き**: 一次情報はあくまで OS の生存確認で、
/// この時間は **PID 再利用**への保険にすぎない。重い処理をしているエージェントは
/// 心拍を専用スレッドから打つので 30 分止まることはない。逆に PID が再利用され
/// ていると「生きている別人」を握った担当が永久に解放されないので、そこだけを
/// この上限が拾う。
const HARD_STALE: Duration = Duration::from_secs(30 * 60);

/// 心拍と監視の最短間隔 (忙しいとき)。
const BEAT_MIN: Duration = Duration::from_secs(2);
/// 心拍と監視の最長間隔 (何も起きていないとき)。
const BEAT_MAX: Duration = Duration::from_secs(30);

/// [`Msg::Custom`] のうち、**メッセージを捨てた**ことを伝える種別。
///
/// 専用の variant を足さないのは、プロトコル (下の [`Msg`]) を
/// 「エージェント同士が交わす語彙」だけに保つため。ギャップはメッシュ自身が
/// 出す運用上の通知なので `Custom` 側に置く。
pub const GAP_KIND: &str = "mesh.gap";

/// 正常終了。**この理由では link は伝播しない** (Erlang と同じ)。
pub const REASON_NORMAL: &str = "normal";
/// プロセスが消えていた (OS が知らない PID になった)。
pub const REASON_NOPROC: &str = "noproc";
/// 心拍が止まったまま [`HARD_STALE`] を超えた (PID 再利用の疑い)。
pub const REASON_STALE: &str = "stale";

/// 自分の [`Pid`] を子プロセスへ渡す環境変数。
///
/// `zai mesh spawn -- <cmd>` はこれを立てて子を起こすので、子の中で走る
/// `zai mesh send …` は `--from` を書かなくてよい。
pub const PID_ENV: &str = "ZAIVERN_MESH_PID";

// ═══════════════════════════════════════════════════════════════════════════
//  1. Pid — 「再起動したら別物」
// ═══════════════════════════════════════════════════════════════════════════

/// メッシュ上のプロセス識別子。表記は Erlang 風に `<node.incarnation.serial>`。
///
/// * `node` — **マシン識別 + リポジトリスコープ**。linked worktree は
///   `.git` ファイルの `gitdir:` を辿って**元のリポジトリ**へ寄せる
///   ([`crate::lease::roots_of`] を再利用)。寄せないと worktree ごとに
///   メッシュが割れて、並列エージェントを見る意味が消える。
/// * `incarnation` — **プロセス開始時刻 (epoch ミリ秒)**。ここが要で、
///   OS が PID を再利用しても「前の住人」と「今の住人」が別物になる。
///   このリポジトリは「終了済みセッションへ kill を撃たない」を絶対ルールに
///   しているので、同一性の判定を PID 単体に持たせてはいけない。
/// * `serial` — 1 つの OS プロセスの中での採番。Erlang と同じく、**1 つの
///   OS プロセスが複数のメッシュプロセスを持てる**ようにするためにある
///   (GUI が「エディタ本体」と「見張り」を別 pid として登録する等)。
///
/// **OS の PID はここに入れない。** PID は `Proc::os_pid` として登録レコード側に
/// 置く。同一性 (Pid) と生存確認の手段 (os_pid) を混ぜないための分離で、
/// 混ぜると「PID が再利用された瞬間に別人が自分になる」。
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Pid {
    pub node: String,
    pub incarnation: u64,
    pub serial: u64,
}

impl fmt::Display for Pid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<{}.{}.{}>", self.node, self.incarnation, self.serial)
    }
}

impl Pid {
    /// `<node.incarnation.serial>` を読み戻す。形が違えば `None` (推測しない)。
    ///
    /// `node` は [`sanitize`] を通っていて `.` を含まないが、**後ろから**
    /// 2 つ切るので、将来 node の綴りが変わっても壊れない。
    pub fn parse(s: &str) -> Option<Pid> {
        let inner = s.trim();
        let inner = inner.strip_prefix('<')?.strip_suffix('>')?;
        let mut it = inner.rsplitn(3, '.');
        let serial = it.next()?.parse().ok()?;
        let incarnation = it.next()?.parse().ok()?;
        let node = it.next()?;
        if node.is_empty() {
            return None;
        }
        Some(Pid {
            node: node.to_string(),
            incarnation,
            serial,
        })
    }

    /// ファイル名に使う短い鍵。
    ///
    /// **Pid をそのままファイル名にしない理由**は Windows の `MAX_PATH` (260)。
    /// `node` はホスト名 + スコープハッシュで 40 字前後になり、受信箱は
    /// `mbox/<宛先>/<送信元>.<連番>.msg` と 2 つ並べるので、素で書くと
    /// ホームディレクトリが少し深いだけで上限に触れる。**Pid の実体は
    /// JSON 本文に入っている**ので、鍵は 16 桁で足りる。
    pub fn fkey(&self) -> String {
        short_hash(&self.to_string())
    }
}

/// メッシュ自身が出すメッセージの送り主 (`incarnation == 0`)。
///
/// Erlang の `init` に相当する予約席。[`Mesh::spawn`] は現在時刻を
/// incarnation に使うので、0 が実プロセスへ割り当たることはない。
fn system_pid(node: &str) -> Pid {
    Pid {
        node: node.to_string(),
        incarnation: 0,
        serial: 0,
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  2. メッセージ (プロトコル)
// ═══════════════════════════════════════════════════════════════════════════

/// エージェント同士がやり取りする語彙。
///
/// `spec` は **文字列のまま**運ぶ (`"src/a.rs#L10-40"`)。書式は
/// `region.rs` の `parse` / `render` と同じだが、**コンパイル時の依存は作らない** —
/// 行域の実装と通信層は別のブランチが同時に育てているので、片方の型変更で
/// もう片方が止まらないようにするため。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// 「これから触る」の事前告知。衝突を**起こす前に**見せるための一報。
    Announce {
        intent: String,
        paths: Vec<String>,
    },
    /// 行域の確保要求。
    Claim {
        spec: String,
    },
    /// 確保できた。
    Granted {
        spec: String,
    },
    /// 確保できなかった。`holder` は今の持ち主、`hint` は次の一手。
    Denied {
        spec: String,
        holder: String,
        hint: String,
    },
    /// 手放した。
    Release {
        spec: String,
    },
    /// 引き継ぎ (持ち主を `to` へ渡す意思表示)。
    Yield {
        spec: String,
        to: Pid,
    },
    /// 「landed したので view を追従して」。
    Sync {
        path: String,
        base: String,
        note: String,
    },
    /// 監視していた相手が落ちた。**メッシュだけが出す** (手で送れない)。
    Down {
        pid: Pid,
        reason: String,
    },
    Ping,
    Pong,
    /// 拡張用。運用上の通知 ([`GAP_KIND`]) もここに乗る。
    Custom {
        kind: String,
        body: String,
    },
}

/// 受信箱に入る 1 通。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub from: Pid,
    pub to: Pid,
    /// **送信者ごと・宛先ごと**の連番。順序保証の根拠はこれ。
    pub seq: u64,
    /// 送信時刻 (epoch ミリ秒)。表示と、送信者をまたぐ整列に使う。
    pub ts_ms: u64,
    pub msg: Msg,
}

impl Envelope {
    /// メッセージを捨てたことを伝えるギャップ標識か。
    pub fn is_gap(&self) -> bool {
        matches!(&self.msg, Msg::Custom { kind, .. } if kind == GAP_KIND)
    }

    /// 一覧に出す 1 行 (種別だけの短い形)。
    pub fn kind(&self) -> &'static str {
        match self.msg {
            Msg::Announce { .. } => "announce",
            Msg::Claim { .. } => "claim",
            Msg::Granted { .. } => "granted",
            Msg::Denied { .. } => "denied",
            Msg::Release { .. } => "release",
            Msg::Yield { .. } => "yield",
            Msg::Sync { .. } => "sync",
            Msg::Down { .. } => "down",
            Msg::Ping => "ping",
            Msg::Pong => "pong",
            Msg::Custom { .. } => "custom",
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  3. エラー
// ═══════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MeshError {
    /// 呼び出し側の誤り (引数が変・未登録の pid で操作した等)。
    Invalid(String),
    /// 入出力。
    Io(String),
    /// **fail-closed**: 既に他人のもの。先勝ちにも後勝ちにもしない。
    Taken { what: String, holder: Pid },
    /// そんなプロセスは居ない。
    NoProc(Pid),
}

impl fmt::Display for MeshError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MeshError::Invalid(s) => write!(f, "{s}"),
            MeshError::Io(s) => write!(f, "{s}"),
            MeshError::Taken { what, holder } => write!(
                f,
                "{}",
                trf(
                    "{w} は既に {h} のものです",
                    &[("w", what.clone()), ("h", holder.to_string())]
                )
            ),
            MeshError::NoProc(p) => write!(
                f,
                "{}",
                trf("{p} はメッシュに居ません", &[("p", p.to_string())])
            ),
        }
    }
}

fn io_err(e: std::io::Error) -> MeshError {
    MeshError::Io(e.to_string())
}

// ═══════════════════════════════════════════════════════════════════════════
//  4. 低レベルのファイル操作 (アトミック)
// ═══════════════════════════════════════════════════════════════════════════

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 一意な一時ファイル名。**Windows の `rename` は既存ファイルを置換するが、
/// 一時ファイル名が他プロセスと衝突すると `create` 側で取り合いになる**ので、
/// pid + ナノ秒 + プロセス内カウンタの 3 点で一意にする。
fn tmp_name() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        ".tmp-{}-{}-{}",
        std::process::id(),
        nanos,
        N.fetch_add(1, Ordering::SeqCst)
    )
}

/// tmp へ書いて `rename` で置く。**半分書けた内容を読ませない。**
fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(tmp_name());
    std::fs::write(&tmp, data)?;
    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// **アトミックな確保**。既にあれば `Ok(false)` (奪わない = fail-closed)。
///
/// `create_new` は「作成」と「排他」が 1 つの syscall なので、N 個の
/// プロセスが同時に来ても勝者は必ず 1 つになる。中身の書き込みは作成の直後に
/// 行うため、ごく短い間だけ**空ファイル**が見える。読み手はそれを
/// 「持ち主不明だが埋まっている」として扱う ([`read_json_retry`])。
/// 空を「空き」と読むと fail-closed が崩れるので、ここが一番の勘所。
fn create_new_json<T: Serialize>(path: &Path, v: &T) -> std::io::Result<bool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut f = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(e) => return Err(e),
    };
    let json = serde_json::to_vec(v).unwrap_or_else(|_| b"{}".to_vec());
    f.write_all(&json)?;
    Ok(true)
}

/// JSON を読む。**空ファイルは「まだ書き終わっていない」**とみなして少し待つ。
fn read_json_retry<T: for<'a> Deserialize<'a>>(path: &Path) -> Option<T> {
    for i in 0..4 {
        match std::fs::read(path) {
            Ok(b) if !b.is_empty() => {
                if let Ok(v) = serde_json::from_slice::<T>(&b) {
                    return Some(v);
                }
            }
            Ok(_) => {}
            Err(_) => return None,
        }
        if i < 3 {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    None
}

/// 決定的な 16 桁ハッシュ。`history::workspace_key` と同じ流儀
/// (`DefaultHasher` は固定鍵の SipHash なので、プロセスをまたいでも同じ値)。
fn short_hash(s: &str) -> String {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    format!("{:016x}", h.finish())
}

/// ファイル名 / node 名に使える形へ落とす。`.` を落とすのが要点
/// (`<node.incarnation.serial>` の区切りと混ざらないため)。
fn sanitize(s: &str, max: usize) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else {
            out.push('_');
        }
        if out.len() >= max {
            break;
        }
    }
    if out.is_empty() {
        out.push('x');
    }
    out
}

/// マシン識別。**両 OS を実装する** (片方だけだともう片方で node が潰れる)。
fn machine_id() -> String {
    #[cfg(unix)]
    {
        // **`[i8; 256]` と書かない。** `libc::c_char` は x86_64 Linux / macOS では
        // `i8` だが、**aarch64 Linux と musl では `u8`** なので、決め打ちすると
        // そのターゲットだけコンパイルが通らない (手元の macOS では一生気付けない)。
        let mut buf = [0 as libc::c_char; 256];
        // SAFETY: buf は 256 要素あり、長さもそのまま渡している。
        let ok = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) } == 0;
        if ok {
            let bytes: Vec<u8> = buf
                .iter()
                .take_while(|c| **c != 0)
                .map(|c| *c as u8)
                .collect();
            let s = String::from_utf8_lossy(&bytes).to_string();
            if !s.trim().is_empty() {
                // ホスト名の `.` (`mac.local`) は区切りと混ざるので sanitize で潰れる
                return sanitize(&s, 24);
            }
        }
    }
    #[cfg(windows)]
    {
        if let Ok(s) = std::env::var("COMPUTERNAME") {
            if !s.trim().is_empty() {
                return sanitize(&s, 24);
            }
        }
    }
    sanitize("local", 24)
}

// ═══════════════════════════════════════════════════════════════════════════
//  5. 登録レコード
// ═══════════════════════════════════════════════════════════════════════════

/// [`Mesh::spawn`] に渡す条件。
#[derive(Clone, Debug)]
pub struct SpawnOpts {
    /// 役割 (`"agent"` / `"editor"` / `"hook"` など)。表示と絞り込みだけに使う。
    pub role: String,
    /// 人が読む名札。空でよい。
    pub label: String,
    /// 生存確認に使う **OS の PID**。自分を載せるなら `std::process::id()`。
    pub os_pid: u32,
    /// Erlang の `trap_exit`。`true` なら link 相手の異常終了で**死なず**、
    /// [`Msg::Down`] を受け取るだけになる。監視役はこれを立てる。
    pub trap_exit: bool,
}

impl Default for SpawnOpts {
    fn default() -> Self {
        SpawnOpts {
            role: "proc".into(),
            label: String::new(),
            os_pid: std::process::id(),
            trap_exit: false,
        }
    }
}

/// 登録レコード (`procs/<fkey>.json`)。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proc {
    pub pid: Pid,
    pub os_pid: u32,
    pub role: String,
    pub label: String,
    pub trap_exit: bool,
    pub started_ms: u64,
}

/// 生死。**3 値**なのが要点で、「疑わしい」を「死んだ」に丸めない。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Liveness {
    /// OS も心拍も生きている。
    Alive,
    /// OS は生きているが心拍が古い。**刈らない** (重い処理中かもしれない)。
    Suspect,
    /// OS がその PID を知らない、または心拍が [`HARD_STALE`] を超えた。
    Dead,
}

/// 生死の判定規則。**引数だけで決まる純粋関数**なので、どの OS からでも
/// テーブルテストで固定できる (`instances::osrule` と同じ流儀)。
///
/// 一次情報は OS の生存確認 (`os_alive`)。時間は PID 再利用への保険にすぎない。
pub fn liveness_of(os_alive: bool, beat_age: Duration) -> Liveness {
    if !os_alive {
        return Liveness::Dead;
    }
    if beat_age >= HARD_STALE {
        return Liveness::Dead;
    }
    if beat_age >= STALE {
        return Liveness::Suspect;
    }
    Liveness::Alive
}

/// 一覧に出す 1 行。
#[derive(Clone, Debug, Serialize)]
pub struct ProcInfo {
    #[serde(flatten)]
    pub proc: Proc,
    /// 最後の心拍 (epoch ミリ秒)。
    pub beat_ms: u64,
    pub liveness: Liveness,
    /// link 伝播で置かれた「降りてくれ」の理由。無ければ `None`。
    pub exit_signal: Option<String>,
    /// 受信箱に溜まっている数。
    pub mailbox: usize,
}

/// 担当 1 件 (`claims/<hash>.json`)。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClaimInfo {
    /// `region.rs` と同じ書式の文字列 (`src/a.rs#L10-40`)。
    pub spec: String,
    /// `spec` の `#` より前。**行域を解釈せず**にファイル単位でまとめるためだけの値。
    pub path: String,
    pub pid: Pid,
    pub ms: u64,
}

/// 名前登録 1 件 (`names/<hash>.json`)。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NameInfo {
    pub name: String,
    pub pid: Pid,
}

/// monitor / link の 1 本 (`mon/<対象>/<見張り>.json`)。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct MonEntry {
    watcher: Pid,
    target: Pid,
    /// `true` なら link (双方向・異常終了を伝播)、`false` なら monitor。
    link: bool,
}

/// [`Mesh::reap`] の結果。**冪等**なので、2 回目は全部空になる。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct ReapReport {
    /// 刈った pid (文字列表記・昇順)。
    pub dead: Vec<String>,
    /// 配った [`Msg::Down`] の数。
    pub downs: usize,
    /// 自動で解放した担当の spec (昇順)。
    pub released: Vec<String>,
    /// 外した名前登録 (昇順)。
    pub unnamed: Vec<String>,
    /// 疑わしいが**刈っていない** pid (昇順)。報告だけする。
    pub suspect: Vec<String>,
}

impl ReapReport {
    /// 何かしたか。`false` なら 2 回目以降の空回り。
    pub fn is_empty(&self) -> bool {
        self.dead.is_empty()
            && self.downs == 0
            && self.released.is_empty()
            && self.unnamed.is_empty()
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  6. Mesh — レジストリ本体
// ═══════════════════════════════════════════════════════════════════════════

/// 1 つのリポジトリスコープに対応するメッシュ。
///
/// 置き場は `~/.zaivern/mesh/<scope-hash>/`。ディレクトリ構成:
///
/// ```text
/// procs/<fkey>.json   登録レコード
/// procs/<fkey>.beat   心拍 (中身は epoch ミリ秒のテキスト)
/// procs/<fkey>.exit   link 伝播で置かれた「降りてくれ」の理由
/// names/<hash>.json   名前登録 (create_new で確保 = fail-closed)
/// claims/<hash>.json  担当 (create_new で確保 = fail-closed)
/// mbox/<fkey>/<送信元fkey>.<連番12桁>.msg   受信箱
/// mbox/<fkey>/.gap    捨てた件数 (ギャップ標識)
/// mon/<対象fkey>/<見張りfkey>.json          monitor / link
/// ```
///
/// **心拍を mtime ではなく中身に持つ理由**: mtime はファイルシステムの粒度
/// (FAT なら 2 秒) と時計に引きずられ、しかもテストから「1 時間前」を作るのが
/// 面倒になる。epoch ミリ秒をテキストで置けば、どの OS でも同じ意味になり、
/// 停滞のテストが実時間を待たずに書ける。読めないときだけ mtime へ落ちる。
#[derive(Clone, Debug)]
pub struct Mesh {
    root: PathBuf,
    node: String,
}

impl Mesh {
    /// 実運用の入口。`start` から**元のリポジトリ**を割り出してスコープを決める。
    ///
    /// linked worktree は `.git` ファイルの `gitdir:` を辿って元のリポジトリへ
    /// 寄せる ([`crate::lease::roots_of`] の再実装をしない)。寄せないと
    /// worktree ごとにメッシュが割れ、並列エージェントを見る意味が消える。
    pub fn open_for(start: &Path) -> Mesh {
        let roots = crate::lease::roots_of(start);
        let scope = crate::history::workspace_key(&roots.key);
        let root = crate::config::zaivern_dir().join("mesh").join(&scope);
        // **構築経路は 1 本にする** (テストと本番で組み立てがずれないため)。
        Mesh::open_at(
            root,
            &format!("{}-{}", machine_id(), &scope[..8.min(scope.len())]),
        )
    }

    /// 置き場と node を明示して開く。テストはここに一時ディレクトリを渡すので
    /// **実 `~/.zaivern` に触れない**。将来 SSH / リモートを足すときも、
    /// 差はこの 1 箇所 (置き場の解決) に閉じる (設計原則 5)。
    pub fn open_at(root: PathBuf, node: &str) -> Mesh {
        Mesh {
            root,
            node: sanitize(node, 40),
        }
    }

    /// この node 名 (`Pid::node` に入る値)。
    pub fn node(&self) -> &str {
        &self.node
    }

    /// **未導入のリポジトリのコストはゼロ** — `stat` 1 回で戻る。
    pub fn enabled(&self) -> bool {
        self.root.join("procs").is_dir()
    }

    fn procs_dir(&self) -> PathBuf {
        self.root.join("procs")
    }
    fn names_dir(&self) -> PathBuf {
        self.root.join("names")
    }
    fn claims_dir(&self) -> PathBuf {
        self.root.join("claims")
    }
    fn mbox_dir(&self, pid: &Pid) -> PathBuf {
        self.root.join("mbox").join(pid.fkey())
    }
    fn mon_dir(&self, target: &Pid) -> PathBuf {
        self.root.join("mon").join(target.fkey())
    }
    fn proc_path(&self, pid: &Pid) -> PathBuf {
        self.procs_dir().join(format!("{}.json", pid.fkey()))
    }
    fn beat_path(&self, pid: &Pid) -> PathBuf {
        self.procs_dir().join(format!("{}.beat", pid.fkey()))
    }
    fn exit_path(&self, pid: &Pid) -> PathBuf {
        self.procs_dir().join(format!("{}.exit", pid.fkey()))
    }

    // ── 6.1 プロセス登録 ───────────────────────────────────────────────

    /// メッシュに載る。**Erlang の `spawn` と違い、新しい OS プロセスは起こさない** —
    /// 既にある OS プロセス (自分、または自分が起こした子) を載せるだけ。
    ///
    /// `incarnation` は現在時刻なので、同じ OS プロセスから 2 回呼んでも
    /// `serial` で別 pid になる (Erlang と同じく 1 OS プロセス多 pid)。
    pub fn spawn(&self, opts: SpawnOpts) -> Result<Proc, MeshError> {
        static SERIAL: AtomicU64 = AtomicU64::new(0);
        std::fs::create_dir_all(self.procs_dir()).map_err(io_err)?;
        let now = now_ms();
        let pid = Pid {
            node: self.node.clone(),
            incarnation: now,
            serial: SERIAL.fetch_add(1, Ordering::SeqCst),
        };
        let p = Proc {
            pid: pid.clone(),
            os_pid: opts.os_pid,
            role: sanitize(&opts.role, 24),
            label: opts.label,
            trap_exit: opts.trap_exit,
            started_ms: now,
        };
        // 受信箱は登録と同時に作る。**送信側が「宛先が居ない」を
        // ディレクトリの有無だけで判定できる**ようにするため。
        std::fs::create_dir_all(self.mbox_dir(&pid)).map_err(io_err)?;
        write_atomic(
            &self.proc_path(&pid),
            &serde_json::to_vec(&p).unwrap_or_default(),
        )
        .map_err(io_err)?;
        self.beat(&pid);
        Ok(p)
    }

    /// 心拍を打つ。**失敗しても黙る** (心拍が飛んだだけで動作を止めない)。
    pub fn beat(&self, pid: &Pid) {
        let _ = write_atomic(&self.beat_path(pid), now_ms().to_string().as_bytes());
    }

    /// 登録レコードを引く。
    pub fn lookup(&self, pid: &Pid) -> Option<Proc> {
        read_json_retry::<Proc>(&self.proc_path(pid))
    }

    /// link 伝播で置かれた「降りてくれ」の理由。**自分で見に来る**のが約束で、
    /// メッシュが他人の OS プロセスを勝手に殺すことはない。
    pub fn exit_signal(&self, pid: &Pid) -> Option<String> {
        std::fs::read_to_string(self.exit_path(pid))
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }

    /// 自発的な終了 (Erlang の normal exit)。
    /// 監視者へ [`Msg::Down`] を配り、担当と名前を返し、痕跡を消す。
    pub fn exit(&self, pid: &Pid, reason: &str) -> Result<ReapReport, MeshError> {
        if !self.enabled() {
            return Ok(ReapReport::default());
        }
        let procs = self.scan_procs();
        let Some(info) = procs.get(&pid.fkey()) else {
            return Err(MeshError::NoProc(pid.clone()));
        };
        let mut rep = ReapReport::default();
        let dead: BTreeSet<String> = BTreeSet::new();
        self.retire(&info.proc.clone(), reason, &procs, &dead, &mut rep);
        Ok(rep)
    }

    /// 全プロセス (fkey → 情報)。**`BTreeMap` なので並びは決定的**。
    fn scan_procs(&self) -> BTreeMap<String, ProcInfo> {
        let mut out = BTreeMap::new();
        let Ok(entries) = std::fs::read_dir(self.procs_dir()) else {
            return out;
        };
        let now = now_ms();
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Some(p) = read_json_retry::<Proc>(&path) else {
                continue;
            };
            let beat_ms = self.read_beat(&p.pid).unwrap_or(p.started_ms);
            let age = Duration::from_millis(now.saturating_sub(beat_ms));
            let liveness = liveness_of(crate::instances::pid_alive(p.os_pid), age);
            let mailbox = self.mailbox_len(&p.pid);
            out.insert(
                p.pid.fkey(),
                ProcInfo {
                    beat_ms,
                    liveness,
                    exit_signal: self.exit_signal(&p.pid),
                    mailbox,
                    proc: p,
                },
            );
        }
        out
    }

    fn read_beat(&self, pid: &Pid) -> Option<u64> {
        let path = self.beat_path(pid);
        if let Ok(s) = std::fs::read_to_string(&path) {
            if let Ok(v) = s.trim().parse::<u64>() {
                return Some(v);
            }
        }
        // 中身が読めないときだけ mtime へ落ちる (壊れた心拍で全員を殺さない)。
        let m = std::fs::metadata(&path).ok()?.modified().ok()?;
        Some(m.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
    }

    /// 一覧。**Pid 順に並べる**ので、出力は毎回同じ。
    pub fn list(&self) -> Vec<ProcInfo> {
        if !self.enabled() {
            return Vec::new();
        }
        let mut v: Vec<ProcInfo> = self.scan_procs().into_values().collect();
        v.sort_by(|a, b| a.proc.pid.cmp(&b.proc.pid));
        v
    }

    // ── 6.2 名前登録 (Erlang の global) ─────────────────────────────────

    fn name_path(&self, name: &str) -> PathBuf {
        self.names_dir().join(format!("{}.json", short_hash(name)))
    }

    /// 名前を 1 つの pid へ結ぶ。**既に他人のものなら奪わない**
    /// ([`MeshError::Taken`])。同じ pid で 2 回呼ぶのは成功 (冪等)。
    pub fn register(&self, name: &str, pid: &Pid) -> Result<(), MeshError> {
        let name = name.trim();
        if name.is_empty() {
            return Err(MeshError::Invalid(tr("名前が空です")));
        }
        if self.lookup(pid).is_none() {
            return Err(MeshError::NoProc(pid.clone()));
        }
        let path = self.name_path(name);
        let rec = NameInfo {
            name: name.to_string(),
            pid: pid.clone(),
        };
        if create_new_json(&path, &rec).map_err(io_err)? {
            return Ok(());
        }
        match read_json_retry::<NameInfo>(&path) {
            Some(cur) if cur.pid == *pid => Ok(()),
            Some(cur) => Err(MeshError::Taken {
                what: name.to_string(),
                holder: cur.pid,
            }),
            // 中身が読めない = 誰かが確保した直後。**空きとして扱わない。**
            None => Err(MeshError::Taken {
                what: name.to_string(),
                holder: system_pid(&self.node),
            }),
        }
    }

    /// 名前 → pid。
    pub fn whereis(&self, name: &str) -> Option<Pid> {
        read_json_retry::<NameInfo>(&self.name_path(name.trim())).map(|n| n.pid)
    }

    /// 名前を外す。**自分の名前しか外せない。**
    pub fn unregister(&self, name: &str, pid: &Pid) -> Result<(), MeshError> {
        let path = self.name_path(name.trim());
        match read_json_retry::<NameInfo>(&path) {
            None => Ok(()),
            Some(cur) if cur.pid == *pid => std::fs::remove_file(&path).map_err(io_err),
            Some(cur) => Err(MeshError::Taken {
                what: name.trim().to_string(),
                holder: cur.pid,
            }),
        }
    }

    /// 登録されている名前の一覧 (名前順)。
    pub fn names(&self) -> Vec<NameInfo> {
        let mut v = read_all_json::<NameInfo>(&self.names_dir());
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    // ── 6.3 担当 (claims) ──────────────────────────────────────────────

    fn claim_path(&self, spec: &str) -> PathBuf {
        self.claims_dir().join(format!("{}.json", short_hash(spec)))
    }

    /// `spec` を確保する。**完全一致キーの相互排他だけ**を持ち、行域の重なりは
    /// 見ない (それは `region.rs` の仕事 — ここに二重実装しない)。
    pub fn claim(&self, spec: &str, pid: &Pid) -> Result<(), MeshError> {
        let spec = spec.trim();
        if spec.is_empty() {
            return Err(MeshError::Invalid(tr("spec が空です")));
        }
        if self.lookup(pid).is_none() {
            return Err(MeshError::NoProc(pid.clone()));
        }
        let path = self.claim_path(spec);
        let rec = ClaimInfo {
            spec: spec.to_string(),
            path: spec.split('#').next().unwrap_or(spec).to_string(),
            pid: pid.clone(),
            ms: now_ms(),
        };
        if create_new_json(&path, &rec).map_err(io_err)? {
            return Ok(());
        }
        match read_json_retry::<ClaimInfo>(&path) {
            Some(cur) if cur.pid == *pid => Ok(()),
            Some(cur) => Err(MeshError::Taken {
                what: spec.to_string(),
                holder: cur.pid,
            }),
            None => Err(MeshError::Taken {
                what: spec.to_string(),
                holder: system_pid(&self.node),
            }),
        }
    }

    /// 担当を返す。**自分の担当しか返せない。**
    pub fn release(&self, spec: &str, pid: &Pid) -> Result<(), MeshError> {
        let path = self.claim_path(spec.trim());
        match read_json_retry::<ClaimInfo>(&path) {
            None => Ok(()),
            Some(cur) if cur.pid == *pid => std::fs::remove_file(&path).map_err(io_err),
            Some(cur) => Err(MeshError::Taken {
                what: spec.trim().to_string(),
                holder: cur.pid,
            }),
        }
    }

    /// 担当の一覧 (spec 順)。
    pub fn claims(&self) -> Vec<ClaimInfo> {
        let mut v = read_all_json::<ClaimInfo>(&self.claims_dir());
        v.sort_by(|a, b| a.spec.cmp(&b.spec));
        v
    }

    /// 同じファイルを触っている他の担当 (`Denied` の `hint` を作るため)。
    /// **行域は解釈しない** — `#` の前だけを見る。
    pub fn same_path_holders(&self, spec: &str) -> Vec<ClaimInfo> {
        let path = spec.split('#').next().unwrap_or(spec);
        let mut v: Vec<ClaimInfo> = self
            .claims()
            .into_iter()
            .filter(|c| c.path == path && c.spec != spec)
            .collect();
        v.sort_by(|a, b| a.spec.cmp(&b.spec));
        v
    }

    // ── 6.4 メールボックス ─────────────────────────────────────────────

    fn mailbox_len(&self, pid: &Pid) -> usize {
        msg_files(&self.mbox_dir(pid)).len()
    }

    /// 送る。**tmp → rename** なので、半分書けたメッセージは読まれない。
    ///
    /// 宛先が登録されていなければ [`MeshError::NoProc`]。
    /// (Erlang は死んだ pid への `!` を黙って捨てるが、ここは CLI の
    ///  終了コードで返したい — 黙って消えるのが一番デバッグしづらい。)
    pub fn send(&self, to: &Pid, from: &Pid, msg: Msg) -> Result<u64, MeshError> {
        if matches!(msg, Msg::Down { .. }) {
            return Err(MeshError::Invalid(tr(
                "Down はメッシュだけが出せます (手で送れません)",
            )));
        }
        if !self.mbox_dir(to).is_dir() {
            return Err(MeshError::NoProc(to.clone()));
        }
        self.deliver(to, from, msg)
    }

    /// 実配送。**`Down` の配布 (メッシュ自身) もここを通る。**
    fn deliver(&self, to: &Pid, from: &Pid, msg: Msg) -> Result<u64, MeshError> {
        let dir = self.mbox_dir(to);
        std::fs::create_dir_all(&dir).map_err(io_err)?;
        let prefix = from.fkey();
        // 連番は「今ある最大 + 1」。`create_new` が弾いたら取り直す。
        // 送信者ごとに独立した番号なので、**同じ送信者から同じ受信者への
        // 順序**だけが保証される (Erlang と同じ線引き)。
        let mut seq = next_seq(&dir, &prefix);
        for _ in 0..64 {
            let path = dir.join(format!("{prefix}.{seq:012}.msg"));
            let env = Envelope {
                from: from.clone(),
                to: to.clone(),
                seq,
                ts_ms: now_ms(),
                msg: msg.clone(),
            };
            match create_new_json(&path, &env) {
                Ok(true) => {
                    self.trim_mailbox(&dir);
                    return Ok(seq);
                }
                Ok(false) => seq = next_seq(&dir, &prefix).max(seq + 1),
                Err(e) => return Err(io_err(e)),
            }
        }
        Err(MeshError::Io(tr("受信箱の連番を確保できませんでした")))
    }

    /// 上限を超えたぶんを**古い方から**捨て、捨てた件数をギャップ標識へ足す。
    ///
    /// 生産者 (送信側) を絶対に待たせないための仕掛け。設計原則 2 の
    /// 「隠れている処理は欠落ありでよいが、決してブロックさせない」そのもの。
    fn trim_mailbox(&self, dir: &Path) {
        let mut files = msg_files(dir);
        if files.len() <= MAILBOX_MAX {
            return;
        }
        // 古い順 = mtime 昇順。同じ秒に並んだら名前で決める (決定的にするため)。
        files.sort_by(|a, b| {
            let ka = std::fs::metadata(a).and_then(|m| m.modified()).ok();
            let kb = std::fs::metadata(b).and_then(|m| m.modified()).ok();
            ka.cmp(&kb).then_with(|| a.cmp(b))
        });
        let excess = files.len() - MAILBOX_MAX;
        let mut dropped = 0u64;
        for f in files.iter().take(excess) {
            if std::fs::remove_file(f).is_ok() {
                dropped += 1;
            }
        }
        if dropped > 0 {
            self.bump_gap(dir, dropped);
        }
    }

    fn gap_path(dir: &Path) -> PathBuf {
        dir.join(".gap")
    }

    fn bump_gap(&self, dir: &Path, n: u64) {
        let cur: u64 = std::fs::read_to_string(Mesh::gap_path(dir))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0);
        let _ = write_atomic(&Mesh::gap_path(dir), (cur + n).to_string().as_bytes());
    }

    /// 受け取る (**破壊的** — 読んだら消える)。
    pub fn recv(&self, me: &Pid) -> Vec<Envelope> {
        self.recv_match(me, &|_| true)
    }

    /// 選択受信。**述語に当たったものだけ**取り出し、残りは受信箱に残る。
    ///
    /// Erlang の `receive … end` と同じ性質で、これがあると
    /// 「`Granted` が来るまで他のメッセージは触らない」が素直に書ける。
    pub fn recv_match(&self, me: &Pid, pred: &dyn Fn(&Msg) -> bool) -> Vec<Envelope> {
        if !self.enabled() {
            return Vec::new();
        }
        let dir = self.mbox_dir(me);
        let mut out = Vec::new();
        let mut corrupt = 0u64;
        for f in msg_files(&dir) {
            match read_json_retry::<Envelope>(&f) {
                Some(env) => {
                    if pred(&env.msg) && std::fs::remove_file(&f).is_ok() {
                        out.push(env);
                    }
                }
                None => {
                    // 壊れた 1 通で受信箱全体を詰まらせない。捨てて数える。
                    if std::fs::remove_file(&f).is_ok() {
                        corrupt += 1;
                    }
                }
            }
        }
        if corrupt > 0 {
            self.bump_gap(&dir, corrupt);
        }
        let mut out = order_envelopes(out);
        // ギャップ標識は**先頭**へ。「ここから先は欠けている」を最初に見せる。
        if let Some(gap) = self.take_gap(&dir, me, pred) {
            out.insert(0, gap);
        }
        out
    }

    fn take_gap(&self, dir: &Path, me: &Pid, pred: &dyn Fn(&Msg) -> bool) -> Option<Envelope> {
        let path = Mesh::gap_path(dir);
        let n: u64 = std::fs::read_to_string(&path).ok()?.trim().parse().ok()?;
        if n == 0 {
            return None;
        }
        let msg = Msg::Custom {
            kind: GAP_KIND.to_string(),
            body: trf("{n} 通を捨てました", &[("n", n.to_string())]),
        };
        if !pred(&msg) {
            return None;
        }
        std::fs::remove_file(&path).ok()?;
        Some(Envelope {
            from: system_pid(&self.node),
            to: me.clone(),
            seq: 0,
            ts_ms: now_ms(),
            msg,
        })
    }

    // ── 6.5 monitor / link ────────────────────────────────────────────

    fn mon_path(&self, target: &Pid, watcher: &Pid) -> PathBuf {
        self.mon_dir(target)
            .join(format!("{}.json", watcher.fkey()))
    }

    /// `target` が死んだら `watcher` へ [`Msg::Down`] を届ける (片方向)。
    pub fn monitor(&self, watcher: &Pid, target: &Pid) -> Result<(), MeshError> {
        self.put_mon(watcher, target, false)
    }

    /// monitor / link を外す (片方向ぶん)。
    pub fn demonitor(&self, watcher: &Pid, target: &Pid) -> Result<(), MeshError> {
        let path = self.mon_path(target, watcher);
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(io_err(e)),
        }
    }

    /// **双方向**の結び付き。どちらが異常終了しても、もう片方へ伝播する。
    /// 相手が `trap_exit` なら死なずに [`Msg::Down`] を受け取るだけになる。
    pub fn link(&self, a: &Pid, b: &Pid) -> Result<(), MeshError> {
        self.put_mon(a, b, true)?;
        self.put_mon(b, a, true)
    }

    /// 双方向の結び付きを解く。
    pub fn unlink(&self, a: &Pid, b: &Pid) -> Result<(), MeshError> {
        self.demonitor(a, b)?;
        self.demonitor(b, a)
    }

    fn put_mon(&self, watcher: &Pid, target: &Pid, link: bool) -> Result<(), MeshError> {
        if self.lookup(watcher).is_none() {
            return Err(MeshError::NoProc(watcher.clone()));
        }
        if self.lookup(target).is_none() {
            // Erlang は「既に死んでいる相手」を monitor すると即 DOWN が来る。
            // 同じ振る舞いにする (無言で無視すると監視が張れたと誤解する)。
            self.deliver(
                watcher,
                &system_pid(&self.node),
                Msg::Down {
                    pid: target.clone(),
                    reason: REASON_NOPROC.to_string(),
                },
            )?;
            return Ok(());
        }
        let rec = MonEntry {
            watcher: watcher.clone(),
            target: target.clone(),
            link,
        };
        write_atomic(
            &self.mon_path(target, watcher),
            &serde_json::to_vec(&rec).unwrap_or_default(),
        )
        .map_err(io_err)
    }

    // ── 6.6 監視ツリー (reap) ─────────────────────────────────────────

    /// 死んだ pid を刈り、`Down` を配り、**担当を自動で解放し**、名前を外す。
    ///
    /// これが "let it crash" の実体で、エージェントが落ちても人が掃除しなくて
    /// よくなる。**冪等**: 2 回走らせても 2 回目は何も起きない
    /// ([`ReapReport::is_empty`] が `true`)。
    ///
    /// 再起動戦略は Erlang の `one_for_one` 相当だけ — 落ちた 1 体の後始末を
    /// するが、**道連れに他を落とすことはしない** (`one_for_all` は持たない)。
    pub fn reap(&self) -> ReapReport {
        let mut rep = ReapReport::default();
        if !self.enabled() {
            return rep;
        }
        let procs = self.scan_procs();
        let dead: BTreeSet<String> = procs
            .iter()
            .filter(|(_, i)| i.liveness == Liveness::Dead)
            .map(|(k, _)| k.clone())
            .collect();
        for k in &dead {
            let Some(info) = procs.get(k) else { continue };
            let reason = if crate::instances::pid_alive(info.proc.os_pid) {
                REASON_STALE
            } else {
                REASON_NOPROC
            };
            self.retire(&info.proc, reason, &procs, &dead, &mut rep);
        }
        // 見張りが死んでいる監視の線を掃く (残すと次回以降ずっと空振りする)。
        self.prune_monitors(&dead);
        // 登録レコードの無い担当 / 名前も孤児として返す。**2 回目には
        // 残っていないので冪等性は崩れない。**
        self.prune_orphans(&procs, &mut rep);
        for info in procs.values() {
            if info.liveness == Liveness::Suspect {
                rep.suspect.push(info.proc.pid.to_string());
            }
        }
        rep.dead.sort();
        rep.released.sort();
        rep.unnamed.sort();
        rep.suspect.sort();
        rep
    }

    /// 1 体ぶんの後始末。[`Mesh::exit`] (自発) と [`Mesh::reap`] (他律) が共有する。
    fn retire(
        &self,
        victim: &Proc,
        reason: &str,
        procs: &BTreeMap<String, ProcInfo>,
        dead: &BTreeSet<String>,
        rep: &mut ReapReport,
    ) {
        let pid = &victim.pid;
        // (1) 監視していた相手へ Down を配る
        if let Ok(entries) = std::fs::read_dir(self.mon_dir(pid)) {
            let mut mons: Vec<MonEntry> = entries
                .flatten()
                .filter_map(|e| read_json_retry::<MonEntry>(&e.path()))
                .collect();
            mons.sort_by(|a, b| a.watcher.cmp(&b.watcher));
            for m in mons {
                if dead.contains(&m.watcher.fkey()) {
                    continue; // 死人へは配らない
                }
                if self
                    .deliver(
                        &m.watcher,
                        &system_pid(&self.node),
                        Msg::Down {
                            pid: pid.clone(),
                            reason: reason.to_string(),
                        },
                    )
                    .is_ok()
                {
                    rep.downs += 1;
                }
                // link の伝播は**異常終了のときだけ** (Erlang と同じ)。
                // trap_exit を立てている監視役は死なない。
                let traps = procs
                    .get(&m.watcher.fkey())
                    .map(|i| i.proc.trap_exit)
                    .unwrap_or(false);
                if m.link && reason != REASON_NORMAL && !traps {
                    // **OS プロセスは殺さない。** 標識を置いて自分で降りてもらう。
                    let _ = create_new_json(&self.exit_path(&m.watcher), &reason.to_string());
                    let _ = write_atomic(&self.exit_path(&m.watcher), reason.as_bytes());
                }
            }
        }
        let _ = std::fs::remove_dir_all(self.mon_dir(pid));

        // (2) 担当を自動で解放する — ここが "let it crash" の正体
        for c in self.claims() {
            if c.pid == *pid && std::fs::remove_file(self.claim_path(&c.spec)).is_ok() {
                rep.released.push(c.spec);
            }
        }
        // (3) 名前登録を外す
        for n in self.names() {
            if n.pid == *pid && std::fs::remove_file(self.name_path(&n.name)).is_ok() {
                rep.unnamed.push(n.name);
            }
        }
        // (4) 受信箱と登録レコードを消す
        let _ = std::fs::remove_dir_all(self.mbox_dir(pid));
        let _ = std::fs::remove_file(self.proc_path(pid));
        let _ = std::fs::remove_file(self.beat_path(pid));
        let _ = std::fs::remove_file(self.exit_path(pid));
        rep.dead.push(pid.to_string());
    }

    fn prune_monitors(&self, dead: &BTreeSet<String>) {
        let Ok(dirs) = std::fs::read_dir(self.root.join("mon")) else {
            return;
        };
        for d in dirs.flatten() {
            let Ok(files) = std::fs::read_dir(d.path()) else {
                continue;
            };
            let mut left = 0usize;
            for f in files.flatten() {
                let stem = f
                    .path()
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                if dead.contains(&stem) {
                    let _ = std::fs::remove_file(f.path());
                } else {
                    left += 1;
                }
            }
            if left == 0 {
                let _ = std::fs::remove_dir(d.path());
            }
        }
    }

    fn prune_orphans(&self, procs: &BTreeMap<String, ProcInfo>, rep: &mut ReapReport) {
        for c in self.claims() {
            if !procs.contains_key(&c.pid.fkey())
                && std::fs::remove_file(self.claim_path(&c.spec)).is_ok()
            {
                rep.released.push(c.spec);
            }
        }
        for n in self.names() {
            if !procs.contains_key(&n.pid.fkey())
                && std::fs::remove_file(self.name_path(&n.name)).is_ok()
            {
                rep.unnamed.push(n.name);
            }
        }
    }
}

/// ディレクトリ直下の `.msg` を集める (一時ファイルとギャップ標識は除く)。
fn msg_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut v: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("msg"))
        .collect();
    v.sort();
    v
}

/// `<prefix>.<連番>.msg` の最大値 + 1。無ければ 0。
fn next_seq(dir: &Path, prefix: &str) -> u64 {
    let mut max: Option<u64> = None;
    for p in msg_files(dir) {
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Some(rest) = stem.strip_prefix(prefix) else {
            continue;
        };
        let Some(num) = rest.strip_prefix('.') else {
            continue;
        };
        if let Ok(n) = num.parse::<u64>() {
            max = Some(max.map_or(n, |m: u64| m.max(n)));
        }
    }
    max.map_or(0, |m| m + 1)
}

fn read_all_json<T: for<'a> Deserialize<'a>>(dir: &Path) -> Vec<T> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    paths.sort();
    paths
        .iter()
        .filter_map(|p| read_json_retry::<T>(p))
        .collect()
}

/// 受信箱から取り出した束を、**Erlang と同じ順序保証**で並べる。
///
/// * 保証する: **同じ送信者から同じ受信者へのメッセージは送信順**
/// * 保証しない: 送信者をまたいだ全体順序
///
/// 実装の勘所は「時計が戻っても FIFO を壊さない」こと。素朴に `ts_ms` で
/// 並べると、NTP の巻き戻しやマシンをまたいだ時計のずれで**同じ送信者の
/// 2 通が入れ替わる**。そこで送信者ごとに `ts` を単調化 (直前の値より
/// 小さければ直前の値へ持ち上げる) してから並べる。同値は `(送信者, 連番)`
/// で割るので、送信者内の順序は連番そのものになる。
fn order_envelopes(mut v: Vec<Envelope>) -> Vec<Envelope> {
    // 1. まず送信者ごとに連番昇順
    v.sort_by(|a, b| a.from.cmp(&b.from).then_with(|| a.seq.cmp(&b.seq)));
    // 2. 送信者ごとに ts を単調化
    let mut keyed: Vec<(u64, Pid, u64, Envelope)> = Vec::with_capacity(v.len());
    let mut last: Option<(Pid, u64)> = None;
    for e in v {
        let t = match &last {
            Some((p, prev)) if *p == e.from => e.ts_ms.max(*prev),
            _ => e.ts_ms,
        };
        last = Some((e.from.clone(), t));
        keyed.push((t, e.from.clone(), e.seq, e));
    }
    // 3. (単調化した ts, 送信者, 連番) で全体を並べる
    keyed.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then_with(|| a.1.cmp(&b.1))
            .then_with(|| a.2.cmp(&b.2))
    });
    keyed.into_iter().map(|(_, _, _, e)| e).collect()
}

/// 監視間隔の適応的な後退。**アイドル時のコストをゼロに近づける**ための要。
///
/// 何も起きなかった周回が続くほど間隔を伸ばす (2 秒 → 30 秒)。
/// 固定 TTL のままだと、誰も居ないリポジトリでも 2 秒ごとに `read_dir` が
/// 走り続ける (`git::scan_interval` と同じ考え方)。
pub fn backoff(idle_rounds: u32) -> Duration {
    let mult = 1u32 << idle_rounds.min(4); // 1,2,4,8,16
    let d = BEAT_MIN.saturating_mul(mult);
    if d > BEAT_MAX {
        BEAT_MAX
    } else {
        d
    }
}

/// 環境変数から自分の [`Pid`] を拾う (`zai mesh spawn -- <cmd>` の子が使う)。
pub fn self_pid_from_env() -> Option<Pid> {
    std::env::var(PID_ENV).ok().and_then(|s| Pid::parse(&s))
}

// ═══════════════════════════════════════════════════════════════════════════
//  7. GUI — 裏スレッド + チャネル。**UI スレッドは 1 ミリ秒も待たない**
// ═══════════════════════════════════════════════════════════════════════════

/// 1 回ぶんの走査結果。
#[derive(Clone, Debug, Default)]
struct Snapshot {
    /// この GUI 自身の Pid (参加していれば)。
    me: Option<Pid>,
    node: String,
    procs: Vec<ProcInfo>,
    claims: Vec<ClaimInfo>,
    names: Vec<NameInfo>,
    /// 自分の受信箱から取り出した末尾ぶん。**読んだら消える**ので、
    /// 表示できる件数だけ手元に残す (画面外の消費者のために生産者を止めない)。
    inbox: Vec<Envelope>,
    /// 走れなかった / 何も無い理由。
    note: Option<String>,
    reaped: Option<ReapReport>,
    cost: Duration,
}

/// パネルから裏スレッドへ渡す 1 回ぶんの依頼。
///
/// **UI スレッドはレジストリに 1 度も触らない。** 押されたボタンはここへ
/// 積むだけで、実際の入出力は全部 `zv-mesh-scan` スレッドが行う。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum Act {
    #[default]
    None,
    /// このエディタをメッシュへ載せる。
    Join,
    /// 抜ける (正常終了 = link を伝播しない)。
    Leave,
    /// 死んだ pid を刈る。
    Reap,
    /// 相手を監視する (落ちたら `Down` が自分の受信箱へ届く)。
    Monitor(Pid),
    /// 生存確認を撃つ。
    Ping(Pid),
}

#[derive(Default)]
struct PanelState {
    open: bool,
    root: PathBuf,
    /// 参加していれば自分の Pid。**UI スレッドはこれを表示に使うだけ。**
    me: Option<Pid>,
    snap: Snapshot,
    pending: Option<std::sync::mpsc::Receiver<Snapshot>>,
    last_scan: Option<std::time::Instant>,
    idle_rounds: u32,
    queued: Act,
}

fn state() -> &'static std::sync::Mutex<PanelState> {
    static S: std::sync::OnceLock<std::sync::Mutex<PanelState>> = std::sync::OnceLock::new();
    S.get_or_init(|| std::sync::Mutex::new(PanelState::default()))
}

/// GUI が開いているワークスペース。取れなければカレントディレクトリ。
/// **パスは 1 つもハードコードしない。**
fn gui_workspace_root() -> PathBuf {
    let me = std::process::id();
    crate::instances::scan_and_prune(&crate::instances::instances_dir())
        .into_iter()
        .find(|e| e.pid == me)
        .and_then(|e| e.workspace_roots.first().map(PathBuf::from))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
}

/// パレットからの入口。開閉を切り替える。
///
/// **開いただけでは参加しない。** レジストリを作るのは明示的な「参加」だけで、
/// 使っていないリポジトリのコストをゼロに保つ (設計原則 3)。
pub fn toggle_panel() {
    let Ok(mut st) = state().lock() else { return };
    if st.open {
        st.open = false;
        return;
    }
    st.open = true;
    st.root = gui_workspace_root();
    st.last_scan = None;
    st.idle_rounds = 0;
}

/// パレットからの入口 (掃除)。開いて、次の走査で `reap` を回す。
pub fn open_and_reap() {
    let Ok(mut st) = state().lock() else { return };
    st.open = true;
    st.root = gui_workspace_root();
    st.last_scan = None;
    st.idle_rounds = 0;
    // 参加していなくても掃除はできる (落ちた他人の担当を解放するだけなので)。
    st.queued = Act::Reap;
}

/// このエディタがメッシュに載るときの名札。
///
/// ワークスペースのフォルダ名を使う (ユーザー名やフルパスを出さない)。
fn editor_label(root: &Path) -> String {
    root.file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("workspace")
        .to_string()
}

/// 裏で 1 回走査する。**UI スレッドからは絶対に呼ばない。**
///
/// ここが「エディタ自身も Erlang のプロセスである」の実体で、
/// 参加していれば毎周回で心拍を打ち、自分の受信箱を空にする。
fn scan(root: PathBuf, mut me: Option<Pid>, act: Act) -> Snapshot {
    let t0 = std::time::Instant::now();
    let mesh = Mesh::open_for(&root);
    let node = mesh.node().to_string();
    let mut note: Option<String> = None;

    // 参加は明示的な操作でだけ行う (ここで初めてレジストリが生える)。
    if act == Act::Join && me.is_none() {
        match mesh.spawn(SpawnOpts {
            role: "editor".into(),
            label: editor_label(&root),
            os_pid: std::process::id(),
            // エディタは**監視役**なので死なない。link 相手が落ちても
            // `Down` を受け取るだけで、自分は降りない (Erlang の trap_exit)。
            trap_exit: true,
        }) {
            Ok(p) => {
                // 名前は「先に居た方」が持つ。取れなくても参加は成立する。
                let _ = mesh.register("editor", &p.pid);
                me = Some(p.pid);
            }
            Err(e) => note = Some(e.to_string()),
        }
    }

    if !mesh.enabled() {
        return Snapshot {
            node,
            note: Some(tr(
                "まだ誰も参加していません (「参加」を押すか zai mesh spawn で載ります)",
            )),
            cost: t0.elapsed(),
            ..Default::default()
        };
    }

    // 参加中の操作。**どれも裏スレッドの中**。
    if let Some(my) = me.clone() {
        match &act {
            Act::Leave => {
                let _ = mesh.exit(&my, REASON_NORMAL);
                return Snapshot {
                    node,
                    note: Some(tr("メッシュから抜けました")),
                    cost: t0.elapsed(),
                    ..Default::default()
                };
            }
            Act::Monitor(t) => {
                if let Err(e) = mesh.monitor(&my, t) {
                    note = Some(e.to_string());
                }
            }
            Act::Ping(t) => {
                if let Err(e) = mesh.send(t, &my, Msg::Ping) {
                    note = Some(e.to_string());
                }
            }
            _ => {}
        }
        mesh.beat(&my);
    }

    let reaped = (act == Act::Reap).then(|| mesh.reap());
    // 自分宛ての `Ping` には `Pong` を返す。これで `zai mesh ping --wait` が
    // GUI に対して本当に成立する (Erlang の応答ループの最小形)。
    let mut inbox = Vec::new();
    if let Some(my) = me.clone() {
        for env in mesh.recv(&my) {
            if matches!(env.msg, Msg::Ping) {
                let _ = mesh.send(&env.from, &my, Msg::Pong);
            }
            inbox.push(env);
        }
        // 末尾だけ残す (上限つき。全部抱えると GUI が受信箱の写しになる)。
        if inbox.len() > INBOX_SHOWN {
            inbox.drain(..inbox.len() - INBOX_SHOWN);
        }
    }
    let procs = mesh.list();
    if note.is_none() && procs.is_empty() {
        note = Some(tr("参加しているプロセスがありません"));
    }
    Snapshot {
        me,
        node,
        procs,
        claims: mesh.claims(),
        names: mesh.names(),
        inbox,
        note,
        reaped,
        cost: t0.elapsed(),
    }
}

/// 画面に残す受信の件数。**受信は破壊的**なので、これを超えたぶんは
/// 表示から落ちる (捨てるのは表示だけで、処理済みという意味では正しい)。
const INBOX_SHOWN: usize = 12;

fn spawn_scan(
    root: PathBuf,
    me: Option<Pid>,
    act: Act,
) -> Option<std::sync::mpsc::Receiver<Snapshot>> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("zv-mesh-scan".into())
        .spawn(move || {
            let _ = tx.send(scan(root, me, act));
        })
        .ok()
        .map(|_| rx)
}

fn poll(st: &mut PanelState, ctx: &egui::Context) {
    use std::sync::mpsc::TryRecvError;
    if let Some(rx) = &st.pending {
        match rx.try_recv() {
            Ok(s) => {
                // 前回と中身が変わっていなければ「暇」と数えて間隔を伸ばす。
                let same = s.procs.len() == st.snap.procs.len()
                    && s.claims.len() == st.snap.claims.len()
                    && s.inbox.is_empty()
                    && s.reaped.is_none();
                st.idle_rounds = if same { st.idle_rounds + 1 } else { 0 };
                st.me = s.me.clone();
                st.snap = s;
                st.last_scan = Some(std::time::Instant::now());
                st.pending = None;
            }
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => st.pending = None,
        }
    }
    if st.pending.is_none() {
        let wait = backoff(st.idle_rounds);
        let queued = st.queued != Act::None;
        let due = st.last_scan.is_none_or(|t| t.elapsed() >= wait) || queued;
        if due {
            let act = std::mem::take(&mut st.queued);
            st.pending = spawn_scan(st.root.clone(), st.me.clone(), act);
            if st.pending.is_none() {
                st.last_scan = Some(std::time::Instant::now());
            }
        }
    }
    // 開いている間だけ、結果を拾うために軽く回す。**閉じていれば 1 命令も走らない。**
    ctx.request_repaint_after(Duration::from_millis(400));
}

/// 一覧の列幅。**純粋関数**なのでテーブルテストで固定できる
/// (CLAUDE.md「レイアウト判断は純粋関数に切り出す」)。
///
/// 返すのは `[pid, 役割, 状態, 受信箱]` の 4 列。狭いときは pid 列から削り、
/// **合計は必ず可用幅に収まる** (どの幅でも見切れない)。
fn columns(avail: f32) -> [f32; 4] {
    const MIN: [f32; 4] = [90.0, 60.0, 52.0, 44.0];
    const WANT: [f32; 4] = [260.0, 150.0, 80.0, 60.0];
    let gaps = 3.0 * 8.0;
    let usable = (avail - gaps).max(0.0);
    let min_sum: f32 = MIN.iter().sum();
    if usable <= min_sum {
        // 収まらない: 最小幅を比例縮小する (負の幅を作らない)
        let k = if min_sum > 0.0 { usable / min_sum } else { 0.0 };
        return [MIN[0] * k, MIN[1] * k, MIN[2] * k, MIN[3] * k];
    }
    let want_sum: f32 = WANT.iter().sum();
    if usable >= want_sum {
        // 余りは pid 列 (いちばん長い文字列) へ寄せる
        return [WANT[0] + (usable - want_sum), WANT[1], WANT[2], WANT[3]];
    }
    // 最小と希望の間: 余裕を希望の比で配る
    let slack = usable - min_sum;
    let span: f32 = WANT.iter().zip(MIN.iter()).map(|(w, m)| w - m).sum();
    let k = if span > 0.0 { slack / span } else { 0.0 };
    [
        MIN[0] + (WANT[0] - MIN[0]) * k,
        MIN[1] + (WANT[1] - MIN[1]) * k,
        MIN[2] + (WANT[2] - MIN[2]) * k,
        MIN[3] + (WANT[3] - MIN[3]) * k,
    ]
}

fn liveness_label(l: Liveness) -> &'static str {
    match l {
        Liveness::Alive => "🟢 生存",
        Liveness::Suspect => "🟡 疑わしい",
        Liveness::Dead => "🔴 死亡",
    }
}

/// 幅に収まるよう省略する (全文はホバーで出す)。
fn ellipsize(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let head: String = s.chars().take(max_chars.saturating_sub(1)).collect();
    format!("{head}…")
}

/// 毎フレームのオーバーレイ。**閉じているフレームは 1 ピクセルも触らない。**
pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {
    let _ = app; // 状態はモジュール側に持つので app の中身へは触らない
    let Ok(mut st) = state().lock() else { return };
    if !st.open {
        // **参加したまま閉じられたら、最後に 1 回だけ裏で退出する。**
        // 心拍は開いている間しか打たないので、登録だけ残すと
        // 「エディタは生きているのに 30 分後に死亡として刈られる」という
        // 嘘の状態が生まれる (HARD_STALE の保険がここで裏目に出る)。
        // 長生きする参加者が要るときは `zai mesh spawn` を使う。
        if st.me.is_some() {
            let me = st.me.take();
            let root = st.root.clone();
            // 受け取り口は捨てる。スレッドは最後まで走る (送信に失敗するだけ)。
            let _ = spawn_scan(root, me, Act::Leave);
            st.snap = Snapshot::default();
        }
        // 以後のフレームは 1 命令も走らない (アイドル時のコストはゼロ)。
        return;
    }
    poll(&mut st, ctx);
    let mut open = true;
    let mut act = Act::None;
    egui::Window::new(tr("🕸 メッシュ — 並列エージェントの通信"))
        .collapsible(false)
        .resizable(true)
        .default_width(660.0)
        .default_height(400.0)
        .open(&mut open)
        .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
        .show(ctx, |ui| {
            act = body(ui, &st);
        });
    if !open {
        st.open = false;
    }
    // **押されたボタンは積むだけ。** 実際の入出力は裏スレッドが行う。
    if act != Act::None {
        st.queued = act;
        st.last_scan = None;
    }
}

/// 本体。押された操作を返す。
fn body(ui: &mut egui::Ui, st: &PanelState) -> Act {
    let mut act = Act::None;
    let dim = ui.visuals().weak_text_color();

    ui.horizontal_wrapped(|ui| {
        ui.label(
            egui::RichText::new(trf(
                "{n} プロセス / 担当 {c} 件",
                &[
                    ("n", st.snap.procs.len().to_string()),
                    ("c", st.snap.claims.len().to_string()),
                ],
            ))
            .strong(),
        );
        match &st.me {
            Some(me) => {
                let s = me.to_string();
                ui.label(egui::RichText::new(ellipsize(&s, 22)).color(dim))
                    .on_hover_text(&s);
                if ui
                    .button(tr("退出"))
                    .on_hover_text(tr("正常終了として抜けます (link は伝播しません)"))
                    .clicked()
                {
                    act = Act::Leave;
                }
            }
            None => {
                if ui
                    .button(tr("参加"))
                    .on_hover_text(tr(
                        "このエディタを Erlang 風のプロセスとしてメッシュへ載せます",
                    ))
                    .clicked()
                {
                    act = Act::Join;
                }
            }
        }
        if ui
            .button(tr("🧹 掃除"))
            .on_hover_text(tr(
                "落ちたプロセスを刈り、その担当を自動で解放します (冪等)",
            ))
            .clicked()
        {
            act = Act::Reap;
        }
        if st.snap.cost > Duration::ZERO {
            ui.label(
                egui::RichText::new(trf(
                    "{ms} ms",
                    &[("ms", st.snap.cost.as_millis().to_string())],
                ))
                .color(dim)
                .small(),
            );
        }
    });

    if let Some(r) = &st.snap.reaped {
        if !r.is_empty() {
            ui.label(trf(
                "掃除: {d} 体を刈り、担当 {c} 件を解放しました",
                &[
                    ("d", r.dead.len().to_string()),
                    ("c", r.released.len().to_string()),
                ],
            ));
        }
    }

    // **空白は作らない**: 中身が無いときは領域の中央に 1 枚だけ出す。
    if st.snap.procs.is_empty() {
        let note = st
            .snap
            .note
            .clone()
            .unwrap_or_else(|| tr("参加しているプロセスがありません"));
        ui.centered_and_justified(|ui| {
            ui.label(egui::RichText::new(note).color(dim));
        });
        return act;
    }

    ui.separator();
    let w = columns(ui.available_width());
    let joined = st.me.is_some();
    egui::ScrollArea::vertical()
        .auto_shrink([false, false])
        .show(ui, |ui| {
            for p in &st.snap.procs {
                let is_me = st.me.as_ref() == Some(&p.proc.pid);
                ui.horizontal(|ui| {
                    let pid = p.proc.pid.to_string();
                    ui.allocate_ui_with_layout(
                        egui::vec2(w[0], 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            let text = ellipsize(&pid, (w[0] / 7.0) as usize);
                            let text = if is_me {
                                egui::RichText::new(text).strong()
                            } else {
                                egui::RichText::new(text)
                            };
                            ui.label(text).on_hover_text(&pid);
                        },
                    );
                    let who = if p.proc.label.is_empty() {
                        p.proc.role.clone()
                    } else {
                        format!("{} · {}", p.proc.role, p.proc.label)
                    };
                    ui.allocate_ui_with_layout(
                        egui::vec2(w[1], 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.label(ellipsize(&who, (w[1] / 7.0) as usize))
                                .on_hover_text(&who);
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(w[2], 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.label(tr(liveness_label(p.liveness)));
                        },
                    );
                    ui.allocate_ui_with_layout(
                        egui::vec2(w[3], 18.0),
                        egui::Layout::left_to_right(egui::Align::Center),
                        |ui| {
                            ui.label(format!("✉ {}", p.mailbox));
                        },
                    );
                    // 参加していないと送りようがないので、そのときは出さない
                    // (押せないボタンを並べない)。
                    if joined && !is_me {
                        if ui
                            .small_button(tr("監視"))
                            .on_hover_text(tr("落ちたら Down がこちらへ届きます"))
                            .clicked()
                        {
                            act = Act::Monitor(p.proc.pid.clone());
                        }
                        if ui.small_button(tr("ping")).clicked() {
                            act = Act::Ping(p.proc.pid.clone());
                        }
                    }
                });
            }
            if !st.snap.inbox.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new(tr("受信")).strong());
                for e in &st.snap.inbox {
                    // ギャップ標識は「ここから先は欠けている」の合図なので目立たせる。
                    let line = if e.is_gap() {
                        format!("⚠ {}", body_of(&e.msg))
                    } else {
                        format!("{} {} {}", e.from, e.kind(), body_of(&e.msg))
                    };
                    ui.label(ellipsize(&line, (ui.available_width() / 7.0) as usize))
                        .on_hover_text(&line);
                }
            }
            if !st.snap.claims.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new(tr("担当")).strong());
                for c in &st.snap.claims {
                    let line = format!("{}  ←  {}", c.spec, c.pid);
                    ui.label(ellipsize(&line, (ui.available_width() / 7.0) as usize))
                        .on_hover_text(&line);
                }
            }
            if !st.snap.names.is_empty() {
                ui.separator();
                ui.label(egui::RichText::new(tr("名前")).strong());
                for n in &st.snap.names {
                    let line = format!("{}  →  {}", n.name, n.pid);
                    ui.label(ellipsize(&line, (ui.available_width() / 7.0) as usize))
                        .on_hover_text(&line);
                }
            }
            // node (= マシン + リポジトリスコープ) は最後に小さく。
            // **どのリポジトリのメッシュを見ているか**が分からないと、
            // worktree を行き来したときに取り違える。
            if !st.snap.node.is_empty() {
                ui.separator();
                ui.label(
                    egui::RichText::new(trf("node: {n}", &[("n", st.snap.node.clone())]))
                        .color(dim)
                        .small(),
                );
            }
        });
    act
}

/// メッセージの中身を 1 行に潰す (一覧用)。
fn body_of(m: &Msg) -> String {
    match m {
        Msg::Announce { intent, paths } => format!("{intent} [{}]", paths.join(" ")),
        Msg::Claim { spec } | Msg::Granted { spec } | Msg::Release { spec } => spec.clone(),
        Msg::Denied { spec, holder, hint } => format!("{spec} ← {holder} {hint}"),
        Msg::Yield { spec, to } => format!("{spec} → {to}"),
        Msg::Sync { path, base, note } => format!("{path}@{base} {note}"),
        Msg::Down { pid, reason } => format!("{pid} {reason}"),
        Msg::Ping => "ping".into(),
        Msg::Pong => "pong".into(),
        Msg::Custom { kind, body } => format!("{kind} {body}"),
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  8. CLI — `zai mesh …`
// ═══════════════════════════════════════════════════════════════════════════

/// 終了コード。**意味を固定する** (スクリプトから分岐できるようにするため)。
///
/// | 値 | 意味 |
/// |---|---|
/// | 0 | 成功 |
/// | 1 | 実行時の失敗 (入出力・宛先が居ない・未登録) |
/// | 2 | 引数の誤り (usage を出す) |
/// | 3 | **fail-closed** — 名前 / 担当が既に他人のもの |
/// | 4 | 見つからない (`whereis` が未登録 / `ping` の相手が死亡) |
pub const EXIT_OK: i32 = 0;
pub const EXIT_FAIL: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_TAKEN: i32 = 3;
pub const EXIT_NOTFOUND: i32 = 4;

fn usage() -> String {
    tr(concat!(
        "zai mesh <サブコマンド>\n",
        "\n",
        "  spawn   [--role R] [--label L] [--trap-exit] [--link PID] [-- CMD…]\n",
        "          いまのプロセス (または起こした子) をメッシュに載せて Pid を出す\n",
        "  list    [--json]                     参加中のプロセス一覧\n",
        "  register <名前> [--pid PID] [--off]  名前を 1 つの Pid へ結ぶ (先勝ちにしない)\n",
        "  whereis <名前>                       名前 → Pid\n",
        "  claim   <spec> [--pid PID]           行域を確保する (先勝ちにしない)\n",
        "  release <spec> [--pid PID]           行域を返す (自分のものだけ)\n",
        "  send    <宛先PID> <種別> [引数…] [--from PID]\n",
        "          種別: announce|claim|granted|denied|release|yield|sync|ping|pong|custom\n",
        "  recv    [--pid PID] [--kind K] [--json]   受信 (読んだら消える)\n",
        "  monitor <対象PID> [--from PID] [--off]    死んだら Down を受け取る\n",
        "  link    <PID-A> <PID-B> [--off]          双方向。異常終了を伝播する\n",
        "  reap    [--json]                     死んだ Pid を刈り、担当を自動解放する\n",
        "  ping    <宛先PID> [--from PID] [--wait MS]\n",
        "\n",
        "終了コード: 0=成功 1=失敗 2=引数の誤り 3=既に他人のもの 4=見つからない\n",
    ))
}

/// `zai mesh <sub>` の入口。`src/cli.rs` の dispatch から呼ばれる想定。
///
/// ## この `allow` は「未完成」の印である (消し方も書いておく)
///
/// `src/cli.rs` は**並列ブランチが取り合う共有ファイル**なので、この
/// ブランチでは 1 バイトも触っていない。結果として `cli_main` とその配下
/// (`Pid::parse` / `claim` / `release` / `link` / `whereis` / `parse_msg` …) は
/// 非テストビルドから到達できず、`dead_code` が全部鳴る。
///
/// `src/cli.rs` の dispatch から `zai mesh …` として呼ばれる
/// (統合時に直列で配線済み。`allow(dead_code)` はその時点で外した)。
/// GUI 側の面 (参加 / 監視 / ping / 掃除 / 一覧) はパレットから到達する。
pub fn cli_main(argv: &[String]) -> i32 {
    let Some(sub) = argv.first().map(String::as_str) else {
        print!("{}", usage());
        return EXIT_USAGE;
    };
    let rest = &argv[1..];
    match sub {
        "help" | "--help" | "-h" => {
            print!("{}", usage());
            EXIT_OK
        }
        "spawn" => cli_spawn(rest),
        "list" => cli_list(rest),
        "register" => cli_register(rest),
        "whereis" => cli_whereis(rest),
        "claim" => cli_claim(rest),
        "release" => cli_release(rest),
        "send" => cli_send(rest),
        "recv" => cli_recv(rest),
        "monitor" => cli_monitor(rest),
        "link" => cli_link(rest),
        "reap" => cli_reap(rest),
        "ping" => cli_ping(rest),
        other => {
            eprintln!(
                "{}",
                trf(
                    "zai mesh: 知らないサブコマンド {s}",
                    &[("s", other.to_string())]
                )
            );
            print!("{}", usage());
            EXIT_USAGE
        }
    }
}

/// エラー → 終了コード。**fail-closed だけを 3 に分ける**のが要点で、
/// 呼び出し側 (エージェント) が「順番待ちすればよい」と「壊れている」を
/// 区別できるようにする。
fn code_of(e: &MeshError) -> i32 {
    match e {
        MeshError::Taken { .. } => EXIT_TAKEN,
        MeshError::NoProc(_) => EXIT_FAIL,
        MeshError::Invalid(_) => EXIT_USAGE,
        MeshError::Io(_) => EXIT_FAIL,
    }
}

fn fail(e: MeshError) -> i32 {
    eprintln!("{e}");
    code_of(&e)
}

/// 旗を切り出す。`--k v` と `--k` (真偽) の両方。位置引数は順番どおり残る。
fn split_flags(args: &[String], valued: &[&str]) -> (Vec<String>, BTreeMap<String, String>) {
    let mut pos = Vec::new();
    let mut flags = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            if valued.contains(&name) {
                if let Some(v) = args.get(i + 1) {
                    flags.insert(name.to_string(), v.clone());
                    i += 2;
                    continue;
                }
            }
            flags.insert(name.to_string(), String::new());
            i += 1;
            continue;
        }
        pos.push(a.clone());
        i += 1;
    }
    (pos, flags)
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn mesh_here() -> Mesh {
    Mesh::open_for(&cwd())
}

/// `--pid` か `$ZAIVERN_MESH_PID` から自分の Pid を決める。
fn me_from(flags: &BTreeMap<String, String>, key: &str) -> Result<Pid, MeshError> {
    if let Some(s) = flags.get(key) {
        return Pid::parse(s).ok_or_else(|| {
            MeshError::Invalid(trf("Pid の形が違います: {s}", &[("s", s.clone())]))
        });
    }
    self_pid_from_env().ok_or_else(|| {
        MeshError::Invalid(trf(
            "自分の Pid が分かりません (--{k} か ${e} を指定してください)",
            &[("k", key.to_string()), ("e", PID_ENV.to_string())],
        ))
    })
}

fn cli_spawn(args: &[String]) -> i32 {
    // `--` の後ろは子プロセスのコマンド。
    let (head, cmd) = match args.iter().position(|a| a == "--") {
        Some(i) => (&args[..i], args[i + 1..].to_vec()),
        None => (args, Vec::new()),
    };
    let (_pos, flags) = split_flags(head, &["role", "label", "link"]);
    let mesh = mesh_here();
    let opts = SpawnOpts {
        role: flags.get("role").cloned().unwrap_or_else(|| "proc".into()),
        label: flags.get("label").cloned().unwrap_or_default(),
        os_pid: std::process::id(),
        trap_exit: flags.contains_key("trap-exit"),
    };
    if cmd.is_empty() {
        let p = match mesh.spawn(opts) {
            Ok(p) => p,
            Err(e) => return fail(e),
        };
        if let Some(l) = flags.get("link") {
            match Pid::parse(l) {
                Some(other) => {
                    if let Err(e) = mesh.link(&p.pid, &other) {
                        return fail(e);
                    }
                }
                None => return fail(MeshError::Invalid(tr("--link の Pid の形が違います"))),
            }
        }
        println!("{}", p.pid);
        return EXIT_OK;
    }
    cli_spawn_child(&mesh, opts, flags.get("link").cloned(), &cmd)
}

/// `-- CMD…` 付きの `spawn`。**自分が起こした子だけ**は本物の link 伝播
/// (異常終了でツリーごと kill) を持てる — 所有しているので安全。
fn cli_spawn_child(mesh: &Mesh, mut opts: SpawnOpts, link: Option<String>, cmd: &[String]) -> i32 {
    let mut c = crate::procx::hidden_command(&cmd[0]);
    c.args(&cmd[1..]);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // 独立したプロセスグループで起こす。**孫まで一度に落とす**ための前提
        // (直接の子だけを kill すると、シェルが exec せずに起こした孫が
        //  パイプを握ったまま残る)。
        c.process_group(0);
    }
    let mut child = match c.spawn() {
        Ok(ch) => ch,
        Err(e) => return fail(MeshError::Io(e.to_string())),
    };
    opts.os_pid = child.id();
    let p = match mesh.spawn(opts) {
        Ok(p) => p,
        Err(e) => {
            let _ = child.kill();
            return fail(e);
        }
    };
    if let Some(l) = &link {
        if let Some(other) = Pid::parse(l) {
            let _ = mesh.link(&p.pid, &other);
        }
    }
    // 子には自分の Pid を渡す (子の中の `zai mesh send` が --from 無しで動く)。
    // 既に起動してしまっているので、環境変数は次回以降のための表示に留める。
    println!("{}", p.pid);

    let mut idle = 0u32;
    let status = loop {
        mesh.beat(&p.pid);
        // link 相手が落ちていれば Down が来る。**自分が起こした子なので
        // ここでは本当にツリーごと落とす** (Erlang の exit 伝播に相当)。
        let downs = mesh.recv_match(&p.pid, &|m| matches!(m, Msg::Down { .. }));
        let fatal = downs
            .iter()
            .any(|e| matches!(&e.msg, Msg::Down { reason, .. } if reason != REASON_NORMAL));
        if fatal && !p.trap_exit {
            crate::procx::kill_tree(child.id());
            let _ = child.wait();
            let _ = mesh.exit(&p.pid, "killed_by_link");
            eprintln!("{}", tr("link 相手が異常終了したので子を落としました"));
            return EXIT_FAIL;
        }
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Ok(None) => {}
            Err(e) => {
                let _ = mesh.exit(&p.pid, REASON_NOPROC);
                return fail(MeshError::Io(e.to_string()));
            }
        }
        idle = idle.saturating_add(1);
        std::thread::sleep(backoff(idle / 4).min(BEAT_MIN));
    };
    let reason = if status.success() {
        REASON_NORMAL.to_string()
    } else {
        format!("exit_{}", status.code().unwrap_or(-1))
    };
    let _ = mesh.exit(&p.pid, &reason);
    status.code().unwrap_or(EXIT_FAIL)
}

fn cli_list(args: &[String]) -> i32 {
    let (_pos, flags) = split_flags(args, &[]);
    let mesh = mesh_here();
    let v = mesh.list();
    if flags.contains_key("json") {
        println!("{}", serde_json::to_string_pretty(&v).unwrap_or_default());
        return EXIT_OK;
    }
    if v.is_empty() {
        println!("{}", tr("参加しているプロセスがありません"));
        return EXIT_OK;
    }
    for p in &v {
        println!(
            "{}  {}  {}  ✉{}{}",
            p.proc.pid,
            p.proc.role,
            tr(liveness_label(p.liveness)),
            p.mailbox,
            p.exit_signal
                .as_ref()
                .map(|r| format!("  exit={r}"))
                .unwrap_or_default()
        );
    }
    EXIT_OK
}

fn cli_register(args: &[String]) -> i32 {
    let (pos, flags) = split_flags(args, &["pid"]);
    let Some(name) = pos.first() else {
        eprintln!("{}", tr("名前を指定してください"));
        return EXIT_USAGE;
    };
    let me = match me_from(&flags, "pid") {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let mesh = mesh_here();
    // `--off` で外す。**自分の名前しか外せない** (他人の名前を奪えない)。
    if flags.contains_key("off") {
        return match mesh.unregister(name, &me) {
            Ok(()) => EXIT_OK,
            Err(e) => fail(e),
        };
    }
    match mesh.register(name, &me) {
        Ok(()) => {
            println!("{name} = {me}");
            EXIT_OK
        }
        Err(e) => fail(e),
    }
}

fn cli_whereis(args: &[String]) -> i32 {
    let (pos, _flags) = split_flags(args, &[]);
    let Some(name) = pos.first() else {
        eprintln!("{}", tr("名前を指定してください"));
        return EXIT_USAGE;
    };
    match mesh_here().whereis(name) {
        Some(p) => {
            println!("{p}");
            EXIT_OK
        }
        None => {
            eprintln!(
                "{}",
                trf("{n} は登録されていません", &[("n", name.clone())])
            );
            EXIT_NOTFOUND
        }
    }
}

/// `zai mesh claim <spec>` — **fail-closed**。取れなければ終了コード 3 と、
/// 同じファイルを触っている他の担当 (`Denied` の `hint` 相当) を出す。
fn cli_claim(args: &[String]) -> i32 {
    let (pos, flags) = split_flags(args, &["pid"]);
    let Some(spec) = pos.first() else {
        eprintln!("{}", tr("spec を指定してください (例: src/a.rs#L10-40)"));
        return EXIT_USAGE;
    };
    let me = match me_from(&flags, "pid") {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let mesh = mesh_here();
    match mesh.claim(spec, &me) {
        Ok(()) => {
            println!("{spec}");
            EXIT_OK
        }
        Err(e @ MeshError::Taken { .. }) => {
            eprintln!("{e}");
            // **次の一手を出す**のが要点。「取れなかった」だけでは
            // エージェントは同じ spec を叩き続ける。
            let others = mesh.same_path_holders(spec);
            if !others.is_empty() {
                eprintln!(
                    "{}",
                    trf(
                        "同じファイルの別の行域を {n} 人が持っています: {s}",
                        &[
                            ("n", others.len().to_string()),
                            (
                                "s",
                                others
                                    .iter()
                                    .map(|c| c.spec.clone())
                                    .collect::<Vec<_>>()
                                    .join(" ")
                            ),
                        ]
                    )
                );
            }
            EXIT_TAKEN
        }
        Err(e) => fail(e),
    }
}

/// `zai mesh release <spec>` — **自分の担当しか返せない**。
fn cli_release(args: &[String]) -> i32 {
    let (pos, flags) = split_flags(args, &["pid"]);
    let Some(spec) = pos.first() else {
        eprintln!("{}", tr("spec を指定してください"));
        return EXIT_USAGE;
    };
    let me = match me_from(&flags, "pid") {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    match mesh_here().release(spec, &me) {
        Ok(()) => EXIT_OK,
        Err(e) => fail(e),
    }
}

/// 位置引数 → [`Msg`]。**純粋関数**なのでテーブルテストで固定できる。
fn parse_msg(pos: &[String]) -> Result<Msg, String> {
    let kind = pos.first().map(String::as_str).unwrap_or("");
    let a = |i: usize| pos.get(i).cloned().unwrap_or_default();
    let need = |i: usize, what: &str| -> Result<String, String> {
        pos.get(i)
            .cloned()
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                trf(
                    "{k} には {w} が要ります",
                    &[("k", kind.to_string()), ("w", what.to_string())],
                )
            })
    };
    match kind {
        "announce" => Ok(Msg::Announce {
            intent: need(1, tr("意図").as_str())?,
            paths: pos.iter().skip(2).cloned().collect(),
        }),
        "claim" => Ok(Msg::Claim {
            spec: need(1, tr("spec").as_str())?,
        }),
        "granted" => Ok(Msg::Granted {
            spec: need(1, tr("spec").as_str())?,
        }),
        "release" => Ok(Msg::Release {
            spec: need(1, tr("spec").as_str())?,
        }),
        "denied" => Ok(Msg::Denied {
            spec: need(1, tr("spec").as_str())?,
            holder: need(2, tr("持ち主").as_str())?,
            hint: a(3),
        }),
        "yield" => {
            let spec = need(1, tr("spec").as_str())?;
            let to = Pid::parse(&need(2, tr("引き継ぎ先 Pid").as_str())?)
                .ok_or_else(|| tr("引き継ぎ先の Pid の形が違います"))?;
            Ok(Msg::Yield { spec, to })
        }
        "sync" => Ok(Msg::Sync {
            path: need(1, tr("パス").as_str())?,
            base: need(2, tr("base").as_str())?,
            note: a(3),
        }),
        "ping" => Ok(Msg::Ping),
        "pong" => Ok(Msg::Pong),
        "custom" => Ok(Msg::Custom {
            kind: need(1, tr("種別").as_str())?,
            body: a(2),
        }),
        "down" => Err(tr("down はメッシュだけが出せます (手で送れません)")),
        "" => Err(tr("種別を指定してください")),
        other => Err(trf("知らない種別: {s}", &[("s", other.to_string())])),
    }
}

fn cli_send(args: &[String]) -> i32 {
    let (pos, flags) = split_flags(args, &["from"]);
    let Some(to_s) = pos.first() else {
        eprintln!("{}", tr("宛先 Pid を指定してください"));
        return EXIT_USAGE;
    };
    let Some(to) = Pid::parse(to_s) else {
        eprintln!("{}", tr("宛先 Pid の形が違います"));
        return EXIT_USAGE;
    };
    let msg = match parse_msg(&pos[1..]) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("{e}");
            return EXIT_USAGE;
        }
    };
    let from = match me_from(&flags, "from") {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    match mesh_here().send(&to, &from, msg) {
        Ok(seq) => {
            println!("{seq}");
            EXIT_OK
        }
        Err(e) => fail(e),
    }
}

fn cli_recv(args: &[String]) -> i32 {
    let (_pos, flags) = split_flags(args, &["pid", "kind"]);
    let me = match me_from(&flags, "pid") {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let mesh = mesh_here();
    let want = flags.get("kind").cloned();
    let envs = match &want {
        Some(k) => {
            let k = k.clone();
            mesh.recv_match(&me, &move |m| {
                let e = Envelope {
                    from: Pid::default(),
                    to: Pid::default(),
                    seq: 0,
                    ts_ms: 0,
                    msg: m.clone(),
                };
                e.kind() == k
            })
        }
        None => mesh.recv(&me),
    };
    if flags.contains_key("json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&envs).unwrap_or_default()
        );
        return EXIT_OK;
    }
    if envs.is_empty() {
        println!("{}", tr("受信箱は空です"));
        return EXIT_OK;
    }
    for e in &envs {
        println!(
            "{} {} {}",
            e.from,
            e.kind(),
            serde_json::to_string(&e.msg).unwrap_or_default()
        );
    }
    EXIT_OK
}

fn cli_monitor(args: &[String]) -> i32 {
    let (pos, flags) = split_flags(args, &["from"]);
    let Some(t) = pos.first().and_then(|s| Pid::parse(s)) else {
        eprintln!("{}", tr("対象 Pid を指定してください"));
        return EXIT_USAGE;
    };
    let me = match me_from(&flags, "from") {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let mesh = mesh_here();
    let r = if flags.contains_key("off") {
        mesh.demonitor(&me, &t)
    } else {
        mesh.monitor(&me, &t)
    };
    match r {
        Ok(()) => EXIT_OK,
        Err(e) => fail(e),
    }
}

fn cli_link(args: &[String]) -> i32 {
    let (pos, flags) = split_flags(args, &[]);
    let (Some(a), Some(b)) = (
        pos.first().and_then(|s| Pid::parse(s)),
        pos.get(1).and_then(|s| Pid::parse(s)),
    ) else {
        eprintln!("{}", tr("Pid を 2 つ指定してください"));
        return EXIT_USAGE;
    };
    let mesh = mesh_here();
    let r = if flags.contains_key("off") {
        mesh.unlink(&a, &b)
    } else {
        mesh.link(&a, &b)
    };
    match r {
        Ok(()) => EXIT_OK,
        Err(e) => fail(e),
    }
}

fn cli_reap(args: &[String]) -> i32 {
    let (_pos, flags) = split_flags(args, &[]);
    let rep = mesh_here().reap();
    if flags.contains_key("json") {
        println!("{}", serde_json::to_string_pretty(&rep).unwrap_or_default());
        return EXIT_OK;
    }
    println!(
        "{}",
        trf(
            "刈った {d} / Down {n} / 解放 {c} / 名前 {m} / 疑わしい {s}",
            &[
                ("d", rep.dead.len().to_string()),
                ("n", rep.downs.to_string()),
                ("c", rep.released.len().to_string()),
                ("m", rep.unnamed.len().to_string()),
                ("s", rep.suspect.len().to_string()),
            ]
        )
    );
    EXIT_OK
}

fn cli_ping(args: &[String]) -> i32 {
    let (pos, flags) = split_flags(args, &["from", "wait"]);
    let Some(to) = pos.first().and_then(|s| Pid::parse(s)) else {
        eprintln!("{}", tr("宛先 Pid を指定してください"));
        return EXIT_USAGE;
    };
    let me = match me_from(&flags, "from") {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    let mesh = mesh_here();
    if let Err(e) = mesh.send(&to, &me, Msg::Ping) {
        return fail(e);
    }
    let wait_ms: u64 = flags.get("wait").and_then(|s| s.parse().ok()).unwrap_or(0);
    if wait_ms == 0 {
        // 待たない既定。**相手が応答ループを回しているとは限らない**ので、
        // レジストリ上の生死だけを見て返す。
        let alive = mesh
            .list()
            .into_iter()
            .find(|p| p.proc.pid == to)
            .map(|p| p.liveness);
        return match alive {
            Some(Liveness::Alive) | Some(Liveness::Suspect) => EXIT_OK,
            Some(Liveness::Dead) | None => EXIT_NOTFOUND,
        };
    }
    let deadline = std::time::Instant::now() + Duration::from_millis(wait_ms);
    while std::time::Instant::now() < deadline {
        let got = mesh.recv_match(&me, &|m| matches!(m, Msg::Pong));
        if !got.is_empty() {
            println!("{}", tr("pong"));
            return EXIT_OK;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    eprintln!("{}", tr("pong が来ませんでした"));
    EXIT_NOTFOUND
}

// ═══════════════════════════════════════════════════════════════════════════
//  9. 機能レジストリへの登録
// ═══════════════════════════════════════════════════════════════════════════

/// パレットからの到達経路。打鍵は要求しない (`keybinds.rs` は共有面なので、
/// **欲しい打鍵は報告に書いて統合担当が直列に入れる**)。
pub const FEATURE: crate::feature::Feature = crate::feature::Feature {
    module: "mesh",
    entries: &[
        crate::feature::Entry {
            icon: "🕸",
            label: "メッシュ — 並列エージェントの通信を見る",
            id: "mesh.open",
        },
        crate::feature::Entry {
            icon: "🧹",
            label: "メッシュを掃除する (落ちた担当を自動解放)",
            id: "mesh.reap",
        },
    ],
    dispatch: |_app, _ctx, id| match id {
        "mesh.open" => {
            toggle_panel();
            true
        }
        "mesh.reap" => {
            open_and_reap();
            true
        }
        _ => false,
    },
    // 中央ビューに属さないオーバーレイなので毎フレームここから描く。
    // **閉じているときは先頭で即 return する**ので、アイドル時のコストはゼロ。
    draw: Some(draw),
    ..crate::feature::Feature::DEFAULT
};

// ═══════════════════════════════════════════════════════════════════════════
//  10. テスト
// ═══════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    /// 「絶対に生きていない」PID。
    ///
    /// **`u32::MAX - 1` を使ってはいけない。** unix の `pid_t` は `i32` なので
    /// `4294967294 as i32 == -2` になり、`kill(-2, 0)` が
    /// **プロセスグループ 2 への問い合わせ**に化ける。Linux には pgid 2
    /// (kthreadd) が常に居るため「生きている」と返り、macOS では返らない —
    /// つまり **macOS で緑・Linux で赤**という、いちばん見つけにくい形で壊れる。
    /// `i32` の正の範囲に収め、かつどの OS の pid 上限より大きい値にする
    /// (Linux の `pid_max` 上限は 2^22 = 4194304、macOS は 99999、
    ///  Windows の PID は 4 の倍数なのでこの値は取り得ない)。
    const DEAD_PID: u32 = 0x7FFF_FFFE;

    fn mesh_for(tag: &str) -> (Mesh, PathBuf) {
        let dir = unique_temp_dir("zaivern-mesh", tag);
        (Mesh::open_at(dir.clone(), "testnode"), dir)
    }

    fn spawn_alive(m: &Mesh, role: &str) -> Pid {
        m.spawn(SpawnOpts {
            role: role.into(),
            os_pid: std::process::id(),
            ..Default::default()
        })
        .expect("spawn")
        .pid
    }

    fn spawn_dead(m: &Mesh, role: &str) -> Pid {
        m.spawn(SpawnOpts {
            role: role.into(),
            os_pid: DEAD_PID,
            ..Default::default()
        })
        .expect("spawn")
        .pid
    }

    // ── Pid ────────────────────────────────────────────────────────────

    #[test]
    fn pid_の文字列表記は往復する() {
        let p = Pid {
            node: "mymac-0123abcd".into(),
            incarnation: 1_754_790_000_123,
            serial: 7,
        };
        assert_eq!(p.to_string(), "<mymac-0123abcd.1754790000123.7>");
        assert_eq!(Pid::parse(&p.to_string()), Some(p.clone()));
        // 形が違えば推測しない
        assert_eq!(Pid::parse("mymac.1.2"), None, "< > が無い");
        assert_eq!(Pid::parse("<mymac.1>"), None, "要素が足りない");
        assert_eq!(Pid::parse("<mymac.x.2>"), None, "数字でない");
        assert_eq!(Pid::parse("<.1.2>"), None, "node が空");
    }

    #[test]
    fn incarnation_が違えば別人になる() {
        let a = Pid {
            node: "n".into(),
            incarnation: 100,
            serial: 0,
        };
        let b = Pid {
            node: "n".into(),
            incarnation: 101,
            serial: 0,
        };
        assert_ne!(a, b, "開始時刻が違えば別プロセス");
        assert_ne!(a.fkey(), b.fkey(), "受信箱も別になる");
    }

    #[test]
    fn node_は区切り記号を含まない() {
        // ホスト名の `.` を残すと `<node.inc.serial>` の解釈が壊れる
        assert_eq!(sanitize("My-Mac.local", 24), "my-mac_local");
        assert!(!machine_id().contains('.'));
    }

    // ── 生死の判定 (純粋関数・OS 非依存) ──────────────────────────────

    #[test]
    fn 生死は_os_を一次情報にして時間は保険にする() {
        use Liveness::*;
        // OS が知らなければ即死
        assert_eq!(liveness_of(false, Duration::ZERO), Dead);
        // 生きていて心拍も新しい
        assert_eq!(liveness_of(true, Duration::from_secs(1)), Alive);
        // 心拍が古いだけでは**殺さない** (重い処理中かもしれない)
        assert_eq!(liveness_of(true, STALE), Suspect);
        assert_eq!(
            liveness_of(true, HARD_STALE - Duration::from_secs(1)),
            Suspect
        );
        // PID 再利用の保険としてだけ、十分長い停滞を死とみなす
        assert_eq!(liveness_of(true, HARD_STALE), Dead);
    }

    #[test]
    fn 監視間隔は暇なほど伸びて上限で止まる() {
        assert_eq!(backoff(0), BEAT_MIN);
        assert_eq!(backoff(1), BEAT_MIN * 2);
        assert!(backoff(3) <= BEAT_MAX);
        assert_eq!(backoff(9), BEAT_MAX, "上限を超えない");
    }

    // ── 未導入のコストはゼロ ──────────────────────────────────────────

    #[test]
    fn 未導入のリポジトリでは何もせずに戻る() {
        let dir = unique_temp_dir("zaivern-mesh", "cold");
        let m = Mesh::open_at(dir.join("nothing-here"), "n");
        assert!(!m.enabled());
        assert!(m.list().is_empty());
        assert!(m.reap().is_empty());
        let p = Pid {
            node: "n".into(),
            incarnation: 1,
            serial: 0,
        };
        assert!(m.recv(&p).is_empty());
    }

    // ── メールボックス ────────────────────────────────────────────────

    #[test]
    fn 送ったものが届き読んだら消える() {
        let (m, _d) = mesh_for("deliver");
        let a = spawn_alive(&m, "agent");
        let b = spawn_alive(&m, "editor");
        m.send(
            &b,
            &a,
            Msg::Claim {
                spec: "src/a.rs#L10-40".into(),
            },
        )
        .expect("send");
        let got = m.recv(&b);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].from, a);
        assert_eq!(
            got[0].msg,
            Msg::Claim {
                spec: "src/a.rs#L10-40".into()
            }
        );
        assert!(m.recv(&b).is_empty(), "受信は破壊的");
    }

    #[test]
    fn 宛先が居なければ黙って捨てずにエラーにする() {
        let (m, _d) = mesh_for("noproc");
        let a = spawn_alive(&m, "agent");
        let ghost = Pid {
            node: m.node().into(),
            incarnation: 1,
            serial: 999,
        };
        assert_eq!(m.send(&ghost, &a, Msg::Ping), Err(MeshError::NoProc(ghost)));
    }

    #[test]
    fn down_は手で送れない() {
        let (m, _d) = mesh_for("nodown");
        let a = spawn_alive(&m, "a");
        let b = spawn_alive(&m, "b");
        let r = m.send(
            &b,
            &a,
            Msg::Down {
                pid: a.clone(),
                reason: "fake".into(),
            },
        );
        assert!(matches!(r, Err(MeshError::Invalid(_))));
    }

    #[test]
    fn 選択受信は当たったものだけ取り出す() {
        let (m, _d) = mesh_for("selective");
        let a = spawn_alive(&m, "a");
        let b = spawn_alive(&m, "b");
        m.send(&b, &a, Msg::Ping).unwrap();
        m.send(
            &b,
            &a,
            Msg::Granted {
                spec: "src/a.rs#L1-2".into(),
            },
        )
        .unwrap();
        m.send(&b, &a, Msg::Pong).unwrap();
        let got = m.recv_match(&b, &|msg| matches!(msg, Msg::Granted { .. }));
        assert_eq!(got.len(), 1, "当たった 1 通だけ");
        let rest = m.recv(&b);
        assert_eq!(rest.len(), 2, "残りは受信箱に残っている");
        assert_eq!(rest[0].msg, Msg::Ping, "残りの順序も送信順のまま");
        assert_eq!(rest[1].msg, Msg::Pong);
    }

    // ── 順序保証 ──────────────────────────────────────────────────────

    fn env_of(from: &Pid, seq: u64, ts: u64, body: &str) -> Envelope {
        Envelope {
            from: from.clone(),
            to: Pid::default(),
            seq,
            ts_ms: ts,
            msg: Msg::Custom {
                kind: "t".into(),
                body: body.into(),
            },
        }
    }

    #[test]
    fn 同じ送信者からは送信順に並ぶ_時計が戻っても() {
        let a = Pid {
            node: "n".into(),
            incarnation: 1,
            serial: 0,
        };
        // ts が途中で**戻っている**束 (NTP の巻き戻し)。素朴に ts で並べると入れ替わる。
        let v = vec![
            env_of(&a, 2, 50, "3rd"),
            env_of(&a, 0, 100, "1st"),
            env_of(&a, 1, 90, "2nd"),
        ];
        let out = order_envelopes(v);
        let bodies: Vec<String> = out
            .iter()
            .map(|e| match &e.msg {
                Msg::Custom { body, .. } => body.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(bodies, vec!["1st", "2nd", "3rd"], "送信者内は連番順");
    }

    #[test]
    fn 送信者をまたぐ順序は保証しないが決定的である() {
        let a = Pid {
            node: "n".into(),
            incarnation: 1,
            serial: 0,
        };
        let b = Pid {
            node: "n".into(),
            incarnation: 1,
            serial: 1,
        };
        let v = vec![env_of(&b, 0, 10, "b0"), env_of(&a, 0, 10, "a0")];
        let out1 = order_envelopes(v.clone());
        let out2 = order_envelopes(v);
        assert_eq!(out1, out2, "同じ入力なら同じ並び (決定的)");
    }

    #[test]
    fn 送信者ごとの_fifo_が実ファイル越しでも守られる() {
        let (m, _d) = mesh_for("fifo");
        let a = spawn_alive(&m, "a");
        let b = spawn_alive(&m, "b");
        let recv = spawn_alive(&m, "recv");
        for i in 0..20u64 {
            m.send(
                &recv,
                &a,
                Msg::Custom {
                    kind: "a".into(),
                    body: i.to_string(),
                },
            )
            .unwrap();
            m.send(
                &recv,
                &b,
                Msg::Custom {
                    kind: "b".into(),
                    body: i.to_string(),
                },
            )
            .unwrap();
        }
        let got = m.recv(&recv);
        assert_eq!(got.len(), 40);
        for who in [&a, &b] {
            let seqs: Vec<u64> = got
                .iter()
                .filter(|e| e.from == *who)
                .map(|e| e.seq)
                .collect();
            let mut sorted = seqs.clone();
            sorted.sort_unstable();
            assert_eq!(seqs, sorted, "{who} からの順序が入れ替わっている");
            assert_eq!(seqs, (0..20).collect::<Vec<u64>>());
        }
    }

    #[test]
    fn 複数スレッドから同じ受信者へ送っても送信者ごとの順序は保たれる() {
        let (m, dir) = mesh_for("fifo-threads");
        let recv = spawn_alive(&m, "recv");
        let senders: Vec<Pid> = (0..4).map(|_| spawn_alive(&m, "tx")).collect();
        let mut hs = Vec::new();
        for s in senders.clone() {
            let dir = dir.clone();
            let recv = recv.clone();
            hs.push(std::thread::spawn(move || {
                // **各スレッドが独立した Mesh を持つ** (共有メモリを使わない証明)
                let m = Mesh::open_at(dir, "testnode");
                for i in 0..25u64 {
                    m.send(
                        &recv,
                        &s,
                        Msg::Custom {
                            kind: "n".into(),
                            body: i.to_string(),
                        },
                    )
                    .expect("send");
                }
            }));
        }
        for h in hs {
            h.join().expect("スレッド");
        }
        let got = m.recv(&recv);
        assert_eq!(got.len(), 100);
        for s in &senders {
            let bodies: Vec<String> = got
                .iter()
                .filter(|e| e.from == *s)
                .map(|e| match &e.msg {
                    Msg::Custom { body, .. } => body.clone(),
                    _ => String::new(),
                })
                .collect();
            let want: Vec<String> = (0..25u64).map(|i| i.to_string()).collect();
            assert_eq!(bodies, want, "{s} の FIFO が崩れている");
        }
    }

    // ── 別プロセスから本当に届く ──────────────────────────────────────

    /// **メールボックスがプロセス内状態を 1 バイトも持たない**ことの証明。
    ///
    /// 配送は「一時ファイルへ書く → `rename` で入れる」だけなので、
    /// その 2 手を**別の OS プロセス** (`sh` / `cmd`) にやらせても、こちら側の
    /// [`Mesh::recv`] が同じように拾えなければならない。
    ///
    /// 自分自身のテストバイナリを再実行しないのは、libtest の再入で
    /// 子プロセスツリーが積み上がるのを避けるため (CI の Linux ランナーを
    /// 殺さないという CLAUDE.md の約束)。**PTY も使わない。**
    #[test]
    fn 別のプロセスが入れたメッセージも受け取れる() {
        let (m, _d) = mesh_for("xproc");
        let a = spawn_alive(&m, "outsider");
        let b = spawn_alive(&m, "me");
        let env = Envelope {
            from: a.clone(),
            to: b.clone(),
            seq: 0,
            ts_ms: now_ms(),
            msg: Msg::Sync {
                path: "src/a.rs".into(),
                base: "abc123".into(),
                note: "landed".into(),
            },
        };
        let json = serde_json::to_string(&env).expect("json");
        let dir = m.mbox_dir(&b);
        let stage = dir.join("outside.json");
        std::fs::write(&stage, &json).expect("下ごしらえ");
        let dest = dir.join(format!("{}.{:012}.msg", a.fkey(), 0));

        // 別プロセスに rename させる。**どちらの OS にもある道具だけを使う。**
        //
        // **シェルへ 1 本の文字列を渡さない。** Rust の `Command` は引数ごとに
        // Windows の規則で引用するので、`cmd /C "move /Y \"a\" \"b\""` のように
        // コマンド全体を 1 引数へ押し込むと、cmd 側の再解析とずれて失敗する
        // (CI の windows-latest が実際にここで落ちた)。引数は**分けて**渡し、
        // unix でもシェルを挟まず `mv` を直に起こす (引用の問題が消える)。
        let run =
            |prog: &str, args: &[&str]| crate::procx::hidden_command(prog).args(args).output().ok();
        let src = stage.to_string_lossy().to_string();
        let dst = dest.to_string_lossy().to_string();
        let out = if cfg!(windows) {
            // `move` は cmd の組み込みなので cmd 経由が要る。
            let first = run("cmd", &["/C", "move", "/Y", &src, &dst]);
            match first {
                Some(o) if o.status.success() => Some(o),
                // 環境によっては cmd が使えないことがある。PowerShell へ落ちる。
                other => run(
                    "powershell",
                    &[
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        "Move-Item",
                        "-LiteralPath",
                        &src,
                        "-Destination",
                        &dst,
                        "-Force",
                    ],
                )
                .or(other),
            }
        } else {
            run("mv", &[&src, &dst])
        };
        let out = out.expect("子プロセスが起動できる");
        assert!(
            out.status.success(),
            "子プロセスが失敗した: {} / {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(!stage.exists(), "別プロセスが rename したはず");

        let got = m.recv(&b);
        assert_eq!(got.len(), 1, "別プロセスが入れた 1 通を受け取れる");
        assert_eq!(got[0].from, a);
        assert!(matches!(&got[0].msg, Msg::Sync { path, .. } if path == "src/a.rs"));
    }

    // ── 溢れとギャップ標識 ────────────────────────────────────────────

    #[test]
    fn 受信箱が溢れたら古い方を捨ててギャップ標識を残す() {
        let (m, _d) = mesh_for("overflow");
        let a = spawn_alive(&m, "a");
        let b = spawn_alive(&m, "b");
        let extra = 3usize;
        for i in 0..(MAILBOX_MAX + extra) {
            m.send(
                &b,
                &a,
                Msg::Custom {
                    kind: "n".into(),
                    body: i.to_string(),
                },
            )
            .expect("send");
        }
        let got = m.recv(&b);
        assert!(got[0].is_gap(), "先頭にギャップ標識が来る");
        assert_eq!(
            got.len(),
            MAILBOX_MAX + 1,
            "上限ぶん + ギャップ 1 通 (生産者は止めない)"
        );
        // 捨てられたのは古い方 (0,1,2)
        let bodies: Vec<String> = got
            .iter()
            .skip(1)
            .map(|e| match &e.msg {
                Msg::Custom { body, .. } => body.clone(),
                _ => String::new(),
            })
            .collect();
        assert!(!bodies.contains(&"0".to_string()), "いちばん古いのが消える");
        assert!(bodies.contains(&(MAILBOX_MAX + extra - 1).to_string()));
        assert!(
            m.recv(&b).is_empty(),
            "ギャップ標識も読んだら消える (二重報告しない)"
        );
    }

    // ── 名前登録 (fail-closed) ────────────────────────────────────────

    #[test]
    fn 名前は一つの_pid_にしか付かない() {
        let (m, _d) = mesh_for("name");
        let a = spawn_alive(&m, "a");
        let b = spawn_alive(&m, "b");
        assert_eq!(m.register("leader", &a), Ok(()));
        assert_eq!(m.register("leader", &a), Ok(()), "同じ pid なら冪等");
        match m.register("leader", &b) {
            Err(MeshError::Taken { holder, .. }) => assert_eq!(holder, a),
            other => panic!("奪えてしまった: {other:?}"),
        }
        assert_eq!(m.whereis("leader"), Some(a.clone()));
        // 他人の名前は外せない
        assert!(matches!(
            m.unregister("leader", &b),
            Err(MeshError::Taken { .. })
        ));
        assert_eq!(m.unregister("leader", &a), Ok(()));
        assert_eq!(m.whereis("leader"), None);
    }

    #[test]
    fn 同時に名前を取りに行っても勝者は一つ() {
        let (m, dir) = mesh_for("name-race");
        let pids: Vec<Pid> = (0..8).map(|_| spawn_alive(&m, "racer")).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut hs = Vec::new();
        for p in pids.clone() {
            let dir = dir.clone();
            let tx = tx.clone();
            hs.push(std::thread::spawn(move || {
                let m = Mesh::open_at(dir, "testnode");
                let _ = tx.send(m.register("leader", &p).is_ok());
            }));
        }
        drop(tx);
        for h in hs {
            h.join().expect("スレッド");
        }
        let wins = rx.iter().filter(|ok| *ok).count();
        assert_eq!(wins, 1, "勝者は必ず 1 つ (先勝ちにも後勝ちにもしない)");
        assert!(pids.contains(&m.whereis("leader").expect("誰かが持っている")));
    }

    // ── 担当 (claims) ────────────────────────────────────────────────

    #[test]
    fn 同時に同じ_spec_を取りに行っても勝者は一つ() {
        let (m, dir) = mesh_for("claim-race");
        let pids: Vec<Pid> = (0..8).map(|_| spawn_alive(&m, "racer")).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let mut hs = Vec::new();
        for p in pids.clone() {
            let dir = dir.clone();
            let tx = tx.clone();
            hs.push(std::thread::spawn(move || {
                let m = Mesh::open_at(dir, "testnode");
                let _ = tx.send(m.claim("src/app.rs#L10-40", &p).is_ok());
            }));
        }
        drop(tx);
        for h in hs {
            h.join().expect("スレッド");
        }
        assert_eq!(rx.iter().filter(|ok| *ok).count(), 1, "勝者は必ず 1 つ");
        assert_eq!(m.claims().len(), 1);
    }

    #[test]
    fn 担当は完全一致キーで排他し重なりは見ない() {
        let (m, _d) = mesh_for("claim-exact");
        let a = spawn_alive(&m, "a");
        let b = spawn_alive(&m, "b");
        m.claim("src/a.rs#L10-40", &a).expect("確保");
        // **重なっていても別キーなら通る** — 重なり判定は region.rs の仕事
        m.claim("src/a.rs#L20-30", &b).expect("別キーは通る");
        assert!(matches!(
            m.claim("src/a.rs#L10-40", &b),
            Err(MeshError::Taken { .. })
        ));
        // 同じファイルの他の担当は hint 用に引ける (行域は解釈しない)
        let others = m.same_path_holders("src/a.rs#L10-40");
        assert_eq!(others.len(), 1);
        assert_eq!(others[0].spec, "src/a.rs#L20-30");
    }

    #[test]
    fn 未登録の_pid_は担当も名前も取れない() {
        let (m, _d) = mesh_for("unregistered");
        let ghost = Pid {
            node: m.node().into(),
            incarnation: 1,
            serial: 0,
        };
        assert!(matches!(
            m.claim("src/a.rs#L1-2", &ghost),
            Err(MeshError::NoProc(_))
        ));
        assert!(matches!(m.register("x", &ghost), Err(MeshError::NoProc(_))));
    }

    // ── monitor / link / DOWN ────────────────────────────────────────

    #[test]
    fn 死んだ_pid_の担当が自動で解放され_reap_は冪等() {
        let (m, _d) = mesh_for("reap");
        let watcher = spawn_alive(&m, "watcher");
        let victim = spawn_dead(&m, "victim");
        m.claim("src/a.rs#L1-9", &victim).expect("確保");
        m.register("worker", &victim).expect("名前");
        m.monitor(&watcher, &victim).expect("監視");

        let rep = m.reap();
        assert_eq!(rep.dead, vec![victim.to_string()]);
        assert_eq!(rep.released, vec!["src/a.rs#L1-9".to_string()]);
        assert_eq!(rep.unnamed, vec!["worker".to_string()]);
        assert_eq!(rep.downs, 1);
        assert!(m.claims().is_empty(), "担当が自動で解放されている");
        assert_eq!(m.whereis("worker"), None);

        let downs = m.recv(&watcher);
        assert_eq!(downs.len(), 1);
        assert!(matches!(&downs[0].msg, Msg::Down { pid, reason }
                if *pid == victim && reason == REASON_NOPROC));

        // **冪等**: 2 回目は何も起きない
        let again = m.reap();
        assert!(again.is_empty(), "2 回目で何かが起きた: {again:?}");
        assert!(m.recv(&watcher).is_empty(), "Down が二重に配られていない");
    }

    #[test]
    fn 死んだ後に同じ_os_pid_で再登録しても前の受信箱を引き継がない() {
        let (m, _d) = mesh_for("incarnation");
        let watcher = spawn_alive(&m, "watcher");
        let old = spawn_dead(&m, "agent");
        m.claim("src/a.rs#L1-9", &old).expect("確保");
        m.monitor(&watcher, &old).expect("監視");
        m.reap();
        assert!(m.claims().is_empty());

        // 「同じ席」に別人が座る = incarnation だけが違う pid
        let newer = Pid {
            node: old.node.clone(),
            incarnation: old.incarnation + 1,
            serial: old.serial,
        };
        assert_ne!(old, newer);
        // 前任者宛ての Down は新任者の受信箱に**入らない**
        assert!(
            m.recv(&newer).is_empty(),
            "PID 再利用で他人の Down を拾った"
        );
        // 新任者は同じ spec を取り直せる
        let re = spawn_alive(&m, "agent");
        m.claim("src/a.rs#L1-9", &re).expect("再取得できる");
    }

    #[test]
    fn link_は異常終了だけを伝播し_trap_exit_なら死なない() {
        let (m, _d) = mesh_for("link");
        let victim = spawn_dead(&m, "victim");
        let plain = spawn_alive(&m, "plain");
        let trapper = m
            .spawn(SpawnOpts {
                role: "sup".into(),
                os_pid: std::process::id(),
                trap_exit: true,
                ..Default::default()
            })
            .expect("spawn")
            .pid;
        m.link(&victim, &plain).expect("link");
        m.link(&victim, &trapper).expect("link");

        m.reap();
        // どちらにも Down は届く
        assert_eq!(m.recv(&plain).len(), 1);
        assert_eq!(m.recv(&trapper).len(), 1);
        // 伝播 (降りてくれ標識) は trap_exit を立てていない側にだけ付く
        assert_eq!(m.exit_signal(&plain).as_deref(), Some(REASON_NOPROC));
        assert_eq!(m.exit_signal(&trapper), None, "trap_exit は死なない");
    }

    #[test]
    fn 正常終了は_link_を伝播しない() {
        let (m, _d) = mesh_for("normal-exit");
        let a = spawn_alive(&m, "a");
        let b = spawn_alive(&m, "b");
        m.link(&a, &b).expect("link");
        let rep = m.exit(&a, REASON_NORMAL).expect("exit");
        assert_eq!(rep.downs, 1, "Down は届く");
        assert_eq!(
            m.exit_signal(&b),
            None,
            "normal では伝播しない (Erlang と同じ)"
        );
        assert!(m.lookup(&a).is_none(), "登録は消えている");
    }

    #[test]
    fn 既に死んでいる相手を監視すると即座に_down_が来る() {
        let (m, _d) = mesh_for("mon-dead");
        let watcher = spawn_alive(&m, "w");
        let ghost = Pid {
            node: m.node().into(),
            incarnation: 1,
            serial: 42,
        };
        m.monitor(&watcher, &ghost).expect("監視");
        let got = m.recv(&watcher);
        assert_eq!(got.len(), 1);
        assert!(matches!(&got[0].msg, Msg::Down { reason, .. } if reason == REASON_NOPROC));
    }

    #[test]
    fn 疑わしいだけの_pid_は刈らない() {
        let (m, _d) = mesh_for("suspect");
        let p = spawn_alive(&m, "busy");
        m.claim("src/a.rs#L1-2", &p).expect("確保");
        // 心拍を 5 分前へ倒す (実時間を待たずに停滞を作れるのが
        // 「心拍を中身に持つ」設計の利点)
        let old = now_ms() - 5 * 60 * 1000;
        std::fs::write(m.beat_path(&p), old.to_string()).expect("心拍");
        let rep = m.reap();
        assert!(rep.dead.is_empty(), "OS が生きていると言う限り刈らない");
        assert_eq!(rep.suspect, vec![p.to_string()]);
        assert_eq!(m.claims().len(), 1, "担当は残る (重い処理中を殺さない)");
    }

    #[test]
    fn 心拍が_hard_stale_を超えたら_pid_再利用として刈る() {
        let (m, _d) = mesh_for("hard-stale");
        let p = spawn_alive(&m, "zombie");
        m.claim("src/a.rs#L1-2", &p).expect("確保");
        let old = now_ms() - (HARD_STALE.as_millis() as u64 + 1000);
        std::fs::write(m.beat_path(&p), old.to_string()).expect("心拍");
        let rep = m.reap();
        assert_eq!(rep.dead, vec![p.to_string()]);
        assert_eq!(rep.released, vec!["src/a.rs#L1-2".to_string()]);
    }

    // ── レイアウト (純粋関数) ────────────────────────────────────────

    #[test]
    fn 列幅はどの可用幅でも収まり負にならない() {
        for avail in [120.0f32, 300.0, 480.0, 900.0, 1200.0, 2400.0] {
            let w = columns(avail);
            let sum: f32 = w.iter().sum::<f32>() + 3.0 * 8.0;
            assert!(
                sum <= avail + 0.01,
                "avail={avail} で合計 {sum} がはみ出した"
            );
            assert!(w.iter().all(|x| *x >= 0.0), "avail={avail} で負の幅");
        }
        // 十分広ければ余りは pid 列へ寄る
        let wide = columns(2400.0);
        assert!(wide[0] > wide[1] + wide[2] + wide[3]);
    }

    #[test]
    fn 長い文字列は省略してホバーに逃がす() {
        assert_eq!(ellipsize("abcdef", 10), "abcdef");
        assert_eq!(ellipsize("abcdef", 4), "abc…");
        // マルチバイトでも境界で割らない
        assert_eq!(ellipsize("あいうえお", 3), "あい…");
    }

    // ── CLI ──────────────────────────────────────────────────────────

    #[test]
    fn cli_は引数の誤りを終了コード_2_で綺麗に返す() {
        assert_eq!(cli_main(&[]), EXIT_USAGE, "サブコマンド無し");
        assert_eq!(
            cli_main(&["nosuchsub".to_string()]),
            EXIT_USAGE,
            "知らないサブコマンド"
        );
        assert_eq!(cli_main(&["help".to_string()]), EXIT_OK);
        assert_eq!(cli_main(&["whereis".to_string()]), EXIT_USAGE, "名前が無い");
        assert_eq!(cli_main(&["send".to_string()]), EXIT_USAGE, "宛先が無い");
        assert_eq!(
            cli_main(&["send".to_string(), "not-a-pid".to_string()]),
            EXIT_USAGE,
            "Pid の形が違う"
        );
        assert_eq!(
            cli_main(&["link".to_string(), "<n.1.0>".to_string()]),
            EXIT_USAGE,
            "Pid が 1 つしかない"
        );
        assert_eq!(
            cli_main(&["monitor".to_string(), "x".to_string()]),
            EXIT_USAGE
        );
        assert_eq!(cli_main(&["claim".to_string()]), EXIT_USAGE, "spec が無い");
        assert_eq!(
            cli_main(&["release".to_string()]),
            EXIT_USAGE,
            "spec が無い"
        );
    }

    #[test]
    fn cli_の使い方には終了コードの意味が書いてある() {
        let u = usage();
        for needle in [
            "0=成功",
            "2=引数の誤り",
            "3=既に他人のもの",
            "4=見つからない",
        ] {
            assert!(u.contains(needle), "usage に {needle} が無い");
        }
        // 課題で要求されたサブコマンドが全部載っていること
        for sub in [
            "spawn", "list", "whereis", "register", "send", "recv", "monitor", "link", "reap",
            "ping", "claim", "release",
        ] {
            assert!(u.contains(sub), "usage に {sub} が無い");
        }
    }

    #[test]
    fn メッセージの引数解釈は形が違えば断る() {
        let s = |v: &[&str]| -> Vec<String> { v.iter().map(|x| (*x).to_string()).collect() };
        assert_eq!(parse_msg(&s(&["ping"])), Ok(Msg::Ping));
        assert_eq!(
            parse_msg(&s(&["claim", "src/a.rs#L1-2"])),
            Ok(Msg::Claim {
                spec: "src/a.rs#L1-2".into()
            })
        );
        assert_eq!(
            parse_msg(&s(&["announce", "整形", "src/a.rs", "src/b.rs"])),
            Ok(Msg::Announce {
                intent: "整形".into(),
                paths: vec!["src/a.rs".into(), "src/b.rs".into()],
            })
        );
        assert_eq!(
            parse_msg(&s(&["yield", "src/a.rs#L1-2", "<n.1.0>"])),
            Ok(Msg::Yield {
                spec: "src/a.rs#L1-2".into(),
                to: Pid {
                    node: "n".into(),
                    incarnation: 1,
                    serial: 0
                },
            })
        );
        assert!(parse_msg(&s(&["claim"])).is_err(), "spec が無い");
        assert!(
            parse_msg(&s(&["down", "x"])).is_err(),
            "down は手で送れない"
        );
        assert!(parse_msg(&s(&["nope"])).is_err());
        assert!(parse_msg(&[]).is_err());
    }

    #[test]
    fn 旗の切り出しは値付きと真偽を取り違えない() {
        let args: Vec<String> = ["a", "--role", "agent", "--trap-exit", "b"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let (pos, flags) = split_flags(&args, &["role"]);
        assert_eq!(pos, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(flags.get("role").map(String::as_str), Some("agent"));
        assert_eq!(flags.get("trap-exit").map(String::as_str), Some(""));
    }

    #[test]
    fn 終了コードは_fail_closed_だけを分ける() {
        let p = Pid::default();
        assert_eq!(
            code_of(&MeshError::Taken {
                what: "x".into(),
                holder: p.clone()
            }),
            EXIT_TAKEN
        );
        assert_eq!(code_of(&MeshError::NoProc(p)), EXIT_FAIL);
        assert_eq!(code_of(&MeshError::Io("x".into())), EXIT_FAIL);
        assert_eq!(code_of(&MeshError::Invalid("x".into())), EXIT_USAGE);
    }

    // ── レジストリ登録 ────────────────────────────────────────────────

    #[test]
    fn 機能登録は接頭辞と到達経路を持つ() {
        assert_eq!(FEATURE.module, "mesh");
        assert!(!FEATURE.entries.is_empty(), "パレットからの到達経路が要る");
        for e in FEATURE.entries {
            assert!(e.id.starts_with("mesh."), "ID の接頭辞: {}", e.id);
            assert!(!e.label.trim().is_empty());
            assert!(!e.icon.trim().is_empty());
        }
        assert!(FEATURE.draw.is_some(), "オーバーレイを描く");
    }

    #[test]
    fn 生存確認に負の_pid_を渡していない() {
        // unix の `kill(-pgid, …)` はプロセス**グループ**に効く。
        // 生存確認のつもりで `i32` の範囲を超えた値を渡すと、無関係な
        // グループを問い合わせて「生きている」と誤判定する
        // (CLAUDE.md「kill に負の PID を渡すときは -- を付ける」と同じ根)。
        assert!(
            DEAD_PID <= i32::MAX as u32,
            "負の pid_t になる値を使っている"
        );
        assert!(
            !crate::instances::pid_alive(DEAD_PID),
            "この PID は死んでいる"
        );
    }

    // ── GUI が Erlang のプロセスとして載る側 ─────────────────────────

    #[test]
    fn 名札はフォルダ名だけで個人情報を含まない() {
        let dir = unique_temp_dir("zaivern-mesh", "label");
        let ws = dir.join("my-repo");
        std::fs::create_dir_all(&ws).expect("作れる");
        assert_eq!(editor_label(&ws), "my-repo");
        // 取れない場所でも落ちない
        assert_eq!(editor_label(Path::new("/")), "workspace");
    }

    #[test]
    fn 押された操作は既定で空であり_pid_を持ち回れる() {
        let p = Pid {
            node: "n".into(),
            incarnation: 1,
            serial: 0,
        };
        assert_eq!(Act::default(), Act::None);
        assert_ne!(Act::Monitor(p.clone()), Act::Ping(p));
    }

    #[test]
    fn 一覧の一行化は全てのメッセージ種別を潰せる() {
        let p = Pid {
            node: "n".into(),
            incarnation: 1,
            serial: 0,
        };
        // **新しい variant を足したらここが必ずコンパイルエラーになる**ので、
        // 「画面に出ないメッセージ」が無言で増えることがない。
        let all = [
            Msg::Announce {
                intent: "整形".into(),
                paths: vec!["a".into()],
            },
            Msg::Claim { spec: "s".into() },
            Msg::Granted { spec: "s".into() },
            Msg::Release { spec: "s".into() },
            Msg::Denied {
                spec: "s".into(),
                holder: "h".into(),
                hint: "i".into(),
            },
            Msg::Yield {
                spec: "s".into(),
                to: p.clone(),
            },
            Msg::Sync {
                path: "a".into(),
                base: "b".into(),
                note: "c".into(),
            },
            Msg::Down {
                pid: p,
                reason: REASON_NORMAL.into(),
            },
            Msg::Ping,
            Msg::Pong,
            Msg::Custom {
                kind: GAP_KIND.into(),
                body: "3 通".into(),
            },
        ];
        for m in &all {
            assert!(!body_of(m).trim().is_empty(), "空行になる種別: {m:?}");
        }
    }

    #[test]
    fn 参加していないうちはレジストリを作らない() {
        // 「開いただけ」でディレクトリが生えると、使っていないリポジトリに
        // コストが発生する (設計原則 3)。参加は明示的な操作だけ。
        let (m, _d) = mesh_for("no-autojoin");
        assert!(!m.enabled(), "spawn するまでレジストリは無い");
        let src = include_str!("mesh.rs").replace("\r\n", "\n");
        let f = src
            .split("pub fn toggle_panel() {")
            .nth(1)
            .expect("toggle_panel が見つからない");
        let f = f.split("\n}\n").next().expect("本体の終端");
        assert!(
            !f.contains("spawn") && !f.contains("Mesh::"),
            "パネルを開いただけで参加している"
        );
    }

    /// **UI スレッドで待たない**ことの構造検査 (`git.rs` の番人と同じ手)。
    /// `draw` / `body` / `poll` から重い走査を直接呼んでいないこと。
    #[test]
    fn 描画から同期の走査を撃つ経路が残っていない() {
        let src = include_str!("mesh.rs").replace("\r\n", "\n");
        let sig = "pub fn draw(app: &mut crate::app::ZaivernApp, ctx: &egui::Context) {";
        let body = src.split(sig).nth(1).expect("draw が見つからない");
        let body = body.split("\n}\n").next().expect("本体の終端");
        for bad in [
            "Mesh::open_for",
            "Mesh::open_at",
            ".reap()",
            ".list()",
            ".recv(",
            ".send(",
            ".beat(",
        ] {
            assert!(
                !body.contains(bad),
                "draw が {bad} を直接呼んでいる (UI スレッドが止まる)"
            );
        }
        // レジストリへ触る唯一の経路が `spawn_scan` であること。
        // 逃がし口を 1 本に絞っておくと、後から同期の穴が開かない。
        assert!(
            body.contains("spawn_scan("),
            "draw がレジストリへ触る経路を裏スレッドへ逃がしていない"
        );
        // 本体 (ボタンを並べる側) も同じ。押された操作は**積むだけ**にする。
        let inner = src
            .split("fn body(ui: &mut egui::Ui, st: &PanelState) -> Act {")
            .nth(1)
            .expect("body が見つからない");
        let inner = inner.split("\n}\n").next().expect("本体の終端");
        for bad in [
            "Mesh::open_for",
            "Mesh::open_at",
            ".reap()",
            ".recv(",
            ".send(",
        ] {
            assert!(
                !inner.contains(bad),
                "body が {bad} を直接呼んでいる (UI スレッドが止まる)"
            );
        }
        assert!(
            src.contains("std::thread::Builder::new()"),
            "走査を裏スレッドへ逃がしていない"
        );
    }
}
