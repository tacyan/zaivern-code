//! テスト専用のヘルパ群。`#[cfg(test)]` でのみコンパイルされる。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

/// std::env::temp_dir() 配下に一意なディレクトリを自作する（HOME 非依存）。
///
/// `prefix` はモジュールごとの名前空間（例: `"zaivern-session-test"`）、
/// `tag` はテストごとの識別子。生成されるディレクトリ名は
/// `{prefix}-{tag}-{pid}-{nanos}-{counter}` となる。
///
/// カウンタは全モジュールで共有される。これは一意性を弱めない
/// （むしろモジュール間での値の重複が起きなくなる）。
pub fn unique_temp_dir(prefix: &str, tag: &str) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    let dir = std::env::temp_dir().join(format!(
        "{}-{}-{}-{}-{}",
        prefix,
        tag,
        std::process::id(),
        nanos,
        COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("create unique temp dir");
    sweep_stale_dirs();
    dir
}

/// 置き去りになった古いテスト用ディレクトリを掃く。**1 時間に 1 回だけ**。
///
/// 多くのテストは後始末をしないので、`$TMPDIR` に `zaivern-*` が積み上がる
/// (実測: 3441 個 / 251MB)。それ自体は無駄なだけだが、**並列実行の速度に効く**。
/// `worktree_base` はリポジトリの隣を worktree の置き場にするため、テストが
/// 一時ディレクトリ直下にリポジトリを作ると worktree が共有の `$TMPDIR` 直下へ
/// 生まれ、エントリ数が膨れた共有ディレクトリで `git worktree add` を並列に
/// 撃つとディレクトリロックで取り合いになる。
///
/// ## 「1 プロセス 1 回」ではなく「1 時間に 1 回」である理由
///
/// nextest は**テスト 1 件につき 1 プロセス**を起こす (約 2900 プロセス)。
/// 「1 プロセス 1 回」にすると全プロセスが `$TMPDIR` を読み切ることになり、
/// しかも走行中はテスト自身がエントリを増やすので **O(n²)** になる。
/// 実測でこれが全体実行を数百秒へ膨らませ、git 系テストを slow-timeout へ
/// 追い込んだ (掃除のつもりが渋滞の原因だった)。
/// スタンプファイルの mtime を見て間引けば、各プロセスの負担は `stat` 1 回で済む。
///
/// **安全側の作り**:
/// * 消すのは `$TMPDIR` 直下の `zaivern-` で始まるディレクトリだけ
/// * **2 時間以上更新が無いものだけ** — 並走している別のテストプロセスの
///   作業ディレクトリを巻き込まないため
/// * スタンプの作成に失敗したら**掃除しない** (競合で二重に走らせない)
/// * 失敗は全部黙って無視する (掃除でテストを落とさない)
fn sweep_stale_dirs() {
    const STALE: std::time::Duration = std::time::Duration::from_secs(2 * 60 * 60);
    const EVERY: std::time::Duration = std::time::Duration::from_secs(60 * 60);
    let tmp = std::env::temp_dir();
    let stamp = tmp.join(".zaivern-sweep-stamp");
    let now = std::time::SystemTime::now();
    // まだ新しいスタンプがあるなら何もしない (ここが全プロセスの通り道)。
    if let Ok(age) = stamp
        .metadata()
        .and_then(|m| m.modified())
        .and_then(|t| now.duration_since(t).map_err(std::io::Error::other))
    {
        if age < EVERY {
            return;
        }
    }
    // 先にスタンプを更新して、同時に走った他プロセスを弾く。
    if std::fs::write(&stamp, b"").is_err() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(&tmp) else {
        return;
    };
    for e in entries.flatten() {
        let name = e.file_name();
        let Some(name) = name.to_str() else { continue };
        if !name.starts_with("zaivern-") {
            continue;
        }
        let stale = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok())
            .is_some_and(|age| age > STALE);
        if stale && e.path().is_dir() {
            let _ = std::fs::remove_dir_all(e.path());
        }
    }
}

// ─────────── 実バイナリ (`target/<profile>/zai`) を使うテストの関所 ───────────

use std::path::Path;
use std::time::SystemTime;

/// 隣の `zai` を使ってよいかの判定。**`Ok` 以外は必ず理由を持つ。**
#[derive(Debug, PartialEq, Eq)]
pub enum ZaiVerdict {
    /// 使ってよい。
    Usable,
    /// 実行ファイルが無い。
    Missing,
    /// `--version` が動かない / 版が違う。
    WrongVersion(String),
    /// 版は合っているが、**ソースより古い**。中身が別物である。
    Stale(String),
    /// 古さを測れなかった (ソースツリーが隣に無い)。使うが黙らない。
    Unmeasurable(String),
}

impl ZaiVerdict {
    /// 使ってよいか。`Unmeasurable` は使ってよい (測れないだけで矛盾は無い)。
    pub fn usable(&self) -> bool {
        matches!(self, ZaiVerdict::Usable | ZaiVerdict::Unmeasurable(_))
    }
}

/// **判定そのもの。** 事実だけを受け取り、I/O をしない (だから表で固定できる)。
///
/// * `bin_mtime` — 実行ファイルの更新時刻。`None` なら実行ファイルが無い
/// * `version_line` — `zai --version` の出力。`None` なら起動できなかった
/// * `want_ver` — `CARGO_PKG_VERSION`
/// * `newest_src` — ソース側の最新更新時刻と、その持ち主のファイル名。
///   `None` ならソースツリーが見つからず**測れない**
///
/// ## なぜ版だけでは足りないか (実際に起きた事故)
///
/// `cargo test --bin zai --no-run` も `cargo test` も **bin を作らない**ので、
/// `target/<profile>/zai` は前の実行の残骸のまま残る。**版は上がっていない
/// のに中身だけ古い**という状態が普通に起こる。実際に `guard` の実フック
/// 試験が「はみ出したのに通った」で赤くなり、原因はソースが 06:00 なのに
/// 実行ファイルが 02:40 のビルドだったことだった。**`--version` は両方
/// 同じ文字列**だったので版の照合では 1 つも捕まえられていない。
pub fn judge_zai(
    bin_mtime: Option<SystemTime>,
    version_line: Option<&str>,
    want_ver: &str,
    newest_src: Option<(String, SystemTime)>,
) -> ZaiVerdict {
    let Some(bin_mtime) = bin_mtime else {
        return ZaiVerdict::Missing;
    };
    match version_line {
        None => return ZaiVerdict::WrongVersion("--version が動きません".into()),
        Some(v) if !v.contains(want_ver) => {
            return ZaiVerdict::WrongVersion(format!("{} != {}", v.trim(), want_ver));
        }
        Some(_) => {}
    }
    let Some((who, src_mtime)) = newest_src else {
        return ZaiVerdict::Unmeasurable("ソースツリーが隣に無いので古さを測れません".into());
    };
    if src_mtime > bin_mtime {
        return ZaiVerdict::Stale(format!(
            "{who} の方が新しい (ソース {} / バイナリ {})",
            stamp(src_mtime),
            stamp(bin_mtime)
        ));
    }
    ZaiVerdict::Usable
}

/// 時刻を「epoch からの秒」で出す。**人間向けの整形をしない。**
/// ロケール・タイムゾーンに依存する書式は環境ごとに文面が変わり、
/// 出力を突き合わせる回帰テストが書けなくなる。
fn stamp(t: SystemTime) -> String {
    match t.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(d) => format!("{}s", d.as_secs()),
        Err(_) => "epoch 以前".into(),
    }
}

/// `src_root` の下で**いちばん新しい** `*.rs` と、`Cargo.toml` / `build.rs` を見る。
///
/// **これ単独では `touch` 1 回で破れる。** 中身の違うバイナリを一度 `Stale` と
/// 弾いても、`touch` すれば mtime が新しくなって受け入れてしまう (端から端まで
/// 再現した)。そこで [`zai_gate_at`] は、この mtime 判定に
/// [`judge_with_stamp`] の**内容ハッシュの記憶**を重ねる。詳細は
/// `docs/bench-honesty.md`。
pub fn newest_source_change(src_root: &Path) -> Option<(String, SystemTime)> {
    let mut best: Option<(String, SystemTime)> = None;
    let mut consider = |p: &Path| {
        let Ok(t) = p.metadata().and_then(|m| m.modified()) else {
            return;
        };
        let name = p
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        if best.as_ref().is_none_or(|(_, b)| t > *b) {
            best = Some((name, t));
        }
    };
    let mut stack = vec![src_root.to_path_buf()];
    let mut seen_any = false;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                seen_any = true;
                consider(&p);
            }
        }
    }
    if !seen_any {
        return None;
    }
    if let Some(root) = src_root.parent() {
        for f in ["Cargo.toml", "build.rs"] {
            consider(&root.join(f));
        }
    }
    best
}

/// 前回の判定の記憶。**`touch` では動かない実行ファイルの同一性**と、
/// そのときのソースの**内容**ハッシュを組にして覚える。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrcStamp {
    /// 実行ファイルの同一性 (unix: dev+inode+サイズ / windows: 作成時刻+サイズ)。
    pub bin: String,
    /// ソースの内容ハッシュ。
    pub src: u64,
    /// そのとき使ってよいと判断したか。
    pub usable: bool,
}

/// `touch` で動かない実行ファイルの同一性。**両 OS を実装する。**
///
/// unix は inode。`touch` は mtime しか動かさないが、`cargo build` は
/// `target/<profile>/deps/zai-<hash>` を張り直すので**別の inode**になる。
/// Windows に inode は無いが作成時刻がある (`LastWriteTime` を書き換えても動かない)。
fn bin_identity(bin: &Path) -> Option<String> {
    let m = bin.metadata().ok()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(format!("unix:{}:{}:{}", m.dev(), m.ino(), m.len()))
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        Some(format!("win:{}:{}", m.creation_time(), m.file_size()))
    }
    #[cfg(not(any(unix, windows)))]
    {
        Some(format!("other:{}", m.len()))
    }
}

/// ソースの**内容**の指紋。`(ハッシュ, ファイル数, バイト数)`。
///
/// **暗号学的ハッシュではない** (std だけで済ませるため)。ここで防ぎたいのは
/// 「うっかり古いバイナリを測る」であって、衝突を細工する攻撃者ではない。
/// 相対パスも混ぜるので、名前の入れ替えも別物として出る。
pub fn source_digest(src_root: &Path) -> Option<(u64, usize, u64)> {
    const SEED: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01B3;
    fn mix(h: &mut u64, chunk: u64) {
        *h = (*h ^ chunk).wrapping_mul(PRIME).rotate_left(29);
    }
    fn feed(h: &mut u64, bytes: &[u8]) {
        let (chunks, rest) = bytes.as_chunks::<8>();
        for c in chunks {
            mix(h, u64::from_le_bytes(*c));
        }
        let mut w = [0u8; 8];
        w[..rest.len()].copy_from_slice(rest);
        mix(h, u64::from_le_bytes(w));
    }

    let mut files: Vec<PathBuf> = Vec::new();
    let mut stack = vec![src_root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
                files.push(p);
            }
        }
    }
    if files.is_empty() {
        return None;
    }
    if let Some(root) = src_root.parent() {
        for f in ["Cargo.toml", "build.rs"] {
            let p = root.join(f);
            if p.is_file() {
                files.push(p);
            }
        }
    }
    files.sort();
    let mut h = SEED;
    let mut bytes = 0u64;
    for f in &files {
        // **絶対パスを混ぜない。** 混ぜると worktree ごとに別の指紋になり、
        // 「同じ内容なのに毎回作り直す」記憶になってしまう
        let rel = f
            .strip_prefix(src_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| PathBuf::from(f.file_name().unwrap_or_default()));
        feed(&mut h, rel.to_string_lossy().as_bytes());
        let Ok(data) = std::fs::read(f) else {
            continue;
        };
        mix(&mut h, data.len() as u64);
        feed(&mut h, &data);
        bytes += data.len() as u64;
    }
    Some((h, files.len(), bytes))
}

/// **`touch` を効かなくする判断そのもの。** I/O をしないので表で固定できる。
///
/// 返りは `(判定, 記録し直すか)`。
///
/// * 実行ファイルが前回と同一で、ソースの内容も同じ
///   → **前回の判定をそのまま繰り返す**。`touch` で mtime が動いても変わらない
/// * 実行ファイルが前回と同一なのにソースの内容が変わった
///   → 建て直していない以上、中身は追いついていないので**無条件に古い**
/// * それ以外 (初めて見る / 建て直された) → mtime の判定を採り、記録する
///
/// **正直に書く弱点**: 記録が 1 つも無い状態での初回だけは mtime しか
/// 手掛かりが無い。ここを閉じるにはビルド時にソースのハッシュをバイナリへ
/// 焼き込む (`build.rs` + `cli.rs`) しかない。
pub fn judge_with_stamp(
    prev: Option<&SrcStamp>,
    bin_id: &str,
    src: u64,
    by_mtime: ZaiVerdict,
) -> (ZaiVerdict, bool) {
    match prev {
        Some(p) if p.bin == bin_id && p.src == src => {
            if p.usable {
                (ZaiVerdict::Usable, false)
            } else {
                (
                    ZaiVerdict::Stale(
                        "同じ実行ファイル・同じソース内容で前回も古いと判定済み \
                         (mtime を触っても変わりません)"
                            .into(),
                    ),
                    false,
                )
            }
        }
        Some(p) if p.bin == bin_id => (
            ZaiVerdict::Stale(
                "実行ファイルは前回と同一なのにソースの内容が変わりました \
                 (建て直していないので中身が追いついていません)"
                    .into(),
            ),
            true,
        ),
        _ => (by_mtime, true),
    }
}

/// スタンプの置き場。実行ファイルの隣 (`<bin>.zai-srcstamp`)。
fn stamp_path(bin: &Path) -> PathBuf {
    let mut p = bin.as_os_str().to_owned();
    p.push(".zai-srcstamp");
    PathBuf::from(p)
}

fn read_stamp(path: &Path) -> Option<SrcStamp> {
    let text = std::fs::read_to_string(path).ok()?;
    // 改行は正規化してから見る (CRLF のチェックアウトで壊れないため)
    let line = text.replace("\r\n", "\n");
    let mut it = line.trim().split('\t');
    let bin = it.next()?.to_string();
    let src = it.next()?.parse().ok()?;
    let usable = it.next()? == "usable";
    Some(SrcStamp { bin, src, usable })
}

/// 書けなくても黙って諦める (関所を強くするための記録であって、
/// 書けないこと自体はテストを落とす理由にならない)。並走する別プロセスと
/// 混ざらないよう、一時名へ書いてから `rename` する。
fn write_stamp(path: &Path, s: &SrcStamp) {
    let mut tmp = path.as_os_str().to_owned();
    tmp.push(format!(".{}.tmp", std::process::id()));
    let tmp = PathBuf::from(tmp);
    let body = format!(
        "{}\t{}\t{}\n",
        s.bin,
        s.src,
        if s.usable { "usable" } else { "stale" }
    );
    if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// **関所そのもの。** `bin` を使ってよいかを、版・古さ・**内容の記憶**で判定する。
///
/// I/O はここに閉じ込め、判断は [`judge_zai`] と [`judge_with_stamp`] に委ねる。
pub fn zai_gate_at(bin: &Path, src_root: &Path, want_ver: &str) -> ZaiVerdict {
    let bin_mtime = bin
        .is_file()
        .then(|| bin.metadata().and_then(|m| m.modified()).ok())
        .flatten();
    if bin_mtime.is_none() {
        return ZaiVerdict::Missing;
    }
    let version_line = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned());
    let by_mtime = judge_zai(
        bin_mtime,
        version_line.as_deref(),
        want_ver,
        newest_source_change(src_root),
    );
    // 版が違う / 実行ファイルが無い / そもそも測れない、は記憶の出番ではない
    if !matches!(by_mtime, ZaiVerdict::Usable | ZaiVerdict::Stale(_)) {
        return by_mtime;
    }
    let (Some(bin_id), Some((src, _, _))) = (bin_identity(bin), source_digest(src_root)) else {
        return by_mtime;
    };
    let path = stamp_path(bin);
    let (verdict, record) = judge_with_stamp(read_stamp(&path).as_ref(), &bin_id, src, by_mtime);
    if record {
        write_stamp(
            &path,
            &SrcStamp {
                bin: bin_id,
                src,
                usable: verdict == ZaiVerdict::Usable,
            },
        );
    }
    verdict
}

/// テストバイナリの隣に居る**本物の `zai`**。使えないなら理由を出して `None`。
///
/// 単体テストに `CARGO_BIN_EXE_*` は無い (統合テストだけの仕組み) ので、
/// `target/<profile>/deps/<test>-<hash>` の 2 つ上から拾う。
///
/// **黙って赤にしない・黙って緑にしない。** 使えないときは `[skip]` 行を
/// stderr へ出し、直し方 (`cargo build --bin zai`) まで書く。
/// `purpose` は「何の試験を飛ばしたか」を書くための短い名前。
pub fn real_zai(purpose: &str) -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let bin = exe
        .parent()?
        .parent()?
        .join(if cfg!(windows) { "zai.exe" } else { "zai" });
    // ソースツリーは cargo が渡す manifest から導く (直書きしない)。
    let src_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let verdict = zai_gate_at(&bin, &src_root, env!("CARGO_PKG_VERSION"));
    match &verdict {
        ZaiVerdict::Usable => Some(bin),
        ZaiVerdict::Unmeasurable(why) => {
            eprintln!("[warn] {purpose}: {why} ({})", bin.display());
            Some(bin)
        }
        ZaiVerdict::Missing => {
            eprintln!(
                "[skip] {purpose}: 隣に zai が居ません ({})。\
                 `cargo build --bin zai` を先に走らせること",
                bin.display()
            );
            None
        }
        ZaiVerdict::WrongVersion(why) => {
            eprintln!(
                "[skip] {purpose}: 隣の zai が版違いです ({why})。\
                 `cargo build --bin zai` を先に走らせること"
            );
            None
        }
        ZaiVerdict::Stale(why) => {
            eprintln!(
                "[skip] {purpose}: 隣の zai がソースより古いです ({why})。\
                 版は合っているので `--version` の照合では捕まりません。\
                 `cargo build --bin zai` を先に走らせること"
            );
            None
        }
    }
}

#[cfg(test)]
mod zai_gate_tests {
    use super::*;
    use std::time::Duration;

    fn at(secs: u64) -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(secs)
    }

    /// 判定の表。**「版が同じで中身が古い」が捕まることがこの表の主眼**。
    #[test]
    fn 判定は版と古さの両方を見る() {
        let ver = "0.14.0";
        let line = Some("Zaivern Code 0.14.0");
        let cases: Vec<(&str, ZaiVerdict, bool)> = vec![
            (
                "実行ファイルが無い",
                judge_zai(None, line, ver, Some(("a.rs".into(), at(100)))),
                false,
            ),
            (
                "--version が動かない",
                judge_zai(Some(at(200)), None, ver, Some(("a.rs".into(), at(100)))),
                false,
            ),
            (
                "版が違う",
                judge_zai(
                    Some(at(200)),
                    Some("Zaivern Code 0.12.0"),
                    ver,
                    Some(("a.rs".into(), at(100))),
                ),
                false,
            ),
            (
                "版は同じだがソースの方が新しい (これが事故の形)",
                judge_zai(Some(at(100)), line, ver, Some(("a.rs".into(), at(200)))),
                false,
            ),
            (
                "同時刻はまだ新しい側と見なす (再ビルドは必ず後に置かれる)",
                judge_zai(Some(at(200)), line, ver, Some(("a.rs".into(), at(200)))),
                true,
            ),
            (
                "バイナリの方が新しい",
                judge_zai(Some(at(300)), line, ver, Some(("a.rs".into(), at(200)))),
                true,
            ),
            (
                "ソースが無いので測れない (使うが黙らない)",
                judge_zai(Some(at(300)), line, ver, None),
                true,
            ),
        ];
        for (name, got, want_usable) in cases {
            assert_eq!(got.usable(), want_usable, "{name}: {got:?}");
            if !want_usable {
                assert_ne!(got, ZaiVerdict::Usable, "{name}");
            }
        }
        // 理由の無い拒否を作らない (`[skip]` 行が空になると原因が追えない)。
        let stale = judge_zai(Some(at(100)), line, ver, Some(("guard.rs".into(), at(200))));
        match stale {
            ZaiVerdict::Stale(why) => {
                assert!(
                    why.contains("guard.rs"),
                    "誰が新しいのかを名指しする: {why}"
                );
                assert!(
                    why.contains("100s") && why.contains("200s"),
                    "両方の時刻: {why}"
                );
            }
            other => panic!("Stale を期待した: {other:?}"),
        }
    }

    #[test]
    fn 実行ファイルが無ければmissing() {
        let dir = unique_temp_dir("zaivern-zaigate-test", "missing");
        std::fs::create_dir_all(dir.join("src")).expect("src");
        std::fs::write(dir.join("src/a.rs"), b"// x").expect("a.rs");
        let v = zai_gate_at(&dir.join("target/debug/zai"), &dir.join("src"), "0.14.0");
        assert_eq!(v, ZaiVerdict::Missing);
        assert!(!v.usable());
    }

    #[test]
    fn ソースが無ければ測れないと言う() {
        let dir = unique_temp_dir("zaivern-zaigate-test", "nosrc");
        assert_eq!(newest_source_change(&dir.join("src")), None);
    }

    /// **関所が「版は同じで中身が古い」を実際に捕まえる実演。**
    ///
    /// 替え玉の `zai` は正しい版を名乗る。それでもソースの方が新しければ
    /// 断られること、逆なら通ることを、mtime を明示的に置いて確かめる
    /// (`sleep` を挟まない — 待ち時間で試験の結論を変えないため)。
    ///
    /// 実行ビットのある替え玉を置くので unix だけ。判断そのもの
    /// ([`judge_zai`]) は上の表で全 OS で固定している。
    #[cfg(unix)]
    #[test]
    fn 関所は版が同じでもソースより古いバイナリを断る() {
        use std::os::unix::fs::PermissionsExt;
        let dir = unique_temp_dir("zaivern-zaigate-test", "stale");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::create_dir_all(dir.join("target/debug")).expect("target");
        std::fs::write(src.join("a.rs"), b"// x").expect("a.rs");
        let bin = dir.join("target/debug/zai");
        std::fs::write(&bin, "#!/bin/sh\necho \"Zaivern Code 0.14.0\"\n").expect("bin");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");

        let set = |p: &Path, secs: u64| {
            let f = std::fs::File::options().write(true).open(p).expect("open");
            f.set_times(std::fs::FileTimes::new().set_modified(at(secs)))
                .expect("set_times");
        };

        // (1) バイナリの方が古い → 断る
        set(&src.join("a.rs"), 2_000_000_000);
        set(&bin, 1_000_000_000);
        match zai_gate_at(&bin, &src, "0.14.0") {
            ZaiVerdict::Stale(why) => assert!(why.contains("a.rs"), "{why}"),
            other => panic!("Stale を期待した: {other:?}"),
        }

        // (2) **`touch` しただけでは生き返らない。** mtime は新しくなるが
        // 実行ファイルは同一・ソースの内容も同じなので、前回の「古い」が残る。
        // ここが以前は `Usable` になっていた (= 関所が `touch` 1 回で破れた)
        set(&bin, 2_000_000_100);
        match zai_gate_at(&bin, &src, "0.14.0") {
            ZaiVerdict::Stale(why) => assert!(why.contains("前回も古い"), "{why}"),
            other => panic!("touch では生き返らないはず: {other:?}"),
        }

        // (3) 本当に建て直した (中身もサイズも変わる) → 通る
        std::fs::remove_file(&bin).expect("rm");
        std::fs::write(&bin, "#!/bin/sh\nexec echo \"Zaivern Code 0.14.0\"\n").expect("bin");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        set(&bin, 2_000_000_100);
        assert_eq!(zai_gate_at(&bin, &src, "0.14.0"), ZaiVerdict::Usable);

        // (4) 建て直さずにソースの**中身**を変えたら、mtime を戻しても断る
        std::fs::write(src.join("a.rs"), b"// x2 changed").expect("a.rs");
        set(&src.join("a.rs"), 1_000_000_000);
        match zai_gate_at(&bin, &src, "0.14.0") {
            ZaiVerdict::Stale(why) => assert!(why.contains("内容が変わり"), "{why}"),
            other => panic!("内容が変われば断るはず: {other:?}"),
        }

        // (5) 版が違えば、たとえ新しくても断る
        std::fs::remove_file(&bin).expect("rm");
        std::fs::write(&bin, "#!/bin/sh\necho \"Zaivern Code 0.12.0\"\n").expect("bin");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        set(&bin, 2_000_000_100);
        assert!(matches!(
            zai_gate_at(&bin, &src, "0.14.0"),
            ZaiVerdict::WrongVersion(_)
        ));
    }

    /// [`judge_with_stamp`] の表。**`touch` の効かなさをここで固定する。**
    #[test]
    fn 記憶は同じバイナリと同じソース内容なら前回の判定を繰り返す() {
        let stale = || ZaiVerdict::Stale("mtime".into());
        let s = |bin: &str, src: u64, usable: bool| SrcStamp {
            bin: bin.into(),
            src,
            usable,
        };

        // 記録が無ければ mtime の判定をそのまま採り、記録する
        let (v, rec) = judge_with_stamp(None, "b1", 7, ZaiVerdict::Usable);
        assert_eq!((v, rec), (ZaiVerdict::Usable, true));

        // 同じバイナリ・同じソース内容 → 前回どおり (mtime を無視する)
        let prev = s("b1", 7, false);
        let (v, rec) = judge_with_stamp(Some(&prev), "b1", 7, ZaiVerdict::Usable);
        assert!(matches!(v, ZaiVerdict::Stale(_)), "touch で生き返った");
        assert!(!rec, "記録は書き換えない");

        let prev = s("b1", 7, true);
        let (v, rec) = judge_with_stamp(Some(&prev), "b1", 7, stale());
        assert_eq!((v, rec), (ZaiVerdict::Usable, false));

        // 同じバイナリのままソースの内容が変わった → 無条件に古い
        let prev = s("b1", 7, true);
        let (v, rec) = judge_with_stamp(Some(&prev), "b1", 8, ZaiVerdict::Usable);
        assert!(matches!(v, ZaiVerdict::Stale(_)));
        assert!(rec);

        // 建て直された (同一性が変わった) → mtime の判定に戻る
        let prev = s("b1", 7, false);
        let (v, rec) = judge_with_stamp(Some(&prev), "b2", 8, ZaiVerdict::Usable);
        assert_eq!((v, rec), (ZaiVerdict::Usable, true));
    }

    /// 内容ハッシュは**中身**を見る (mtime を触っても動かない)。
    #[test]
    fn ソースの指紋は中身が変わったときだけ動く() {
        let dir = unique_temp_dir("zaivern-zaigate-test", "digest");
        let src = dir.join("src");
        std::fs::create_dir_all(&src).expect("src");
        std::fs::write(src.join("a.rs"), b"// x").expect("a.rs");
        let (h1, n, _) = source_digest(&src).expect("digest");
        assert_eq!(n, 1);

        // mtime だけ動かしても指紋は同じ
        let f = std::fs::File::options()
            .write(true)
            .open(src.join("a.rs"))
            .expect("open");
        f.set_times(std::fs::FileTimes::new().set_modified(at(2_000_000_000)))
            .expect("set_times");
        assert_eq!(source_digest(&src).expect("digest").0, h1);

        // 中身を変えれば動く
        std::fs::write(src.join("a.rs"), b"// y").expect("a.rs");
        assert_ne!(source_digest(&src).expect("digest").0, h1);

        // ファイルが増えても動く
        std::fs::write(src.join("b.rs"), b"// x").expect("b.rs");
        let (h3, n3, _) = source_digest(&src).expect("digest");
        assert_eq!(n3, 2);
        assert_ne!(h3, h1);
        assert!(source_digest(&dir.join("no-such-dir")).is_none());
    }

    /// **全ベンチの `zai` 決定ブロックが 1 バイトも違わないこと** (事故3の番人)。
    ///
    /// 探索順が `conflict-zero-bench.sh` は release→debug、`coedit-bench.sh` は
    /// debug→release で**逆だった**ため、release 0.12.0 / debug 0.14.0 という
    /// 環境で**同一セッションが別のバイナリを測って**その数字を並べていた。
    /// 「揃えた」を口約束で終わらせないために、ここで機械が照合する。
    #[test]
    fn 全ベンチのzai決定ブロックは1バイトも違わない() {
        const BEGIN: &str = "# @zai-honesty-begin";
        const END: &str = "# @zai-honesty-end";
        let tools = Path::new(env!("CARGO_MANIFEST_DIR")).join("tools");
        let names = [
            "conflict-zero-bench.sh",
            "coedit-bench.sh",
            "conflict-bench.sh",
            "union-bench.sh",
            "anyrepo-prove.sh",
            "xplat-bench.sh",
        ];
        let mut first: Option<(&str, String)> = None;
        for n in names {
            let p = tools.join(n);
            // **改行を正規化する。** Windows のチェックアウトは CRLF なので、
            // 素の比較は「全部違う」か「全部同じ」かを OS で切り替えてしまう。
            let text = std::fs::read_to_string(&p)
                .unwrap_or_else(|e| panic!("{} が読めない: {e}", p.display()))
                .replace("\r\n", "\n");
            let a = text
                .find(BEGIN)
                .unwrap_or_else(|| panic!("{n} に {BEGIN} が無い (共通ブロックが外された)"));
            let b = text
                .find(END)
                .unwrap_or_else(|| panic!("{n} に {END} が無い"));
            assert!(a < b, "{n}: 開始と終了が逆");
            let body = text[a..b + END.len()].to_string();
            match &first {
                None => first = Some((n, body)),
                Some((who, want)) => assert_eq!(
                    &body, want,
                    "{n} の zai 決定ブロックが {who} と違う。\
                     **別のバイナリを測る事故 (探索順の食い違い) がここから戻る。**\
                     直すときは全ベンチへ同じ内容を反映すること"
                ),
            }
        }
        let (_, body) = first.expect("1 本もベンチが無い");
        // 中身が空の「揃っているように見えるだけ」を弾く。
        for must in ["zai_pick", "newer_sources", "zai_fresh", "zai_identity"] {
            assert!(body.contains(must), "共通ブロックに {must} が無い");
        }
        assert!(
            body.find("release/zai").unwrap() < body.find("debug/zai").unwrap(),
            "探索順は release → debug で統一する"
        );
    }

    /// 関所を実物へ当てる。**落とさない** (隣に zai が居るかは実行の仕方次第)。
    /// 通ったと言うからには本当に動くことだけを確かめる。
    #[test]
    fn 実物の隣のzaiを関所に通す() {
        match real_zai("test_util の自己点検") {
            Some(p) => {
                let out = std::process::Command::new(&p)
                    .arg("--version")
                    .output()
                    .expect("関所を通ったのに起動できない");
                let text = String::from_utf8_lossy(&out.stdout).into_owned();
                assert!(
                    text.contains(env!("CARGO_PKG_VERSION")),
                    "関所を通ったのに版が違う: {text}"
                );
                eprintln!("[ok] 隣の zai は使える: {} ({})", p.display(), text.trim());
            }
            None => eprintln!("[info] 隣の zai は使えない (理由は上の [skip] 行)"),
        }
    }
}
