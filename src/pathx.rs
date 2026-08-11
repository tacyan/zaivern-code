//! パス正規化の共通ヘルパー。
//!
//! Windows の [`Path::canonicalize`] は `\\?\C:\...` (verbatim 形式) を返す。
//! アプリ内で持ち回るだけなら無害だが、**子プロセスの作業ディレクトリ**に
//! 渡すと壊れる: `cmd.exe` は verbatim / UNC のカレントディレクトリを受け付けず、
//!
//! ```text
//! '\\?\C:\Users\me\proj'
//! CMD.EXE was started with the above path as the current directory.
//! UNC paths are not supported.  Defaulting to Windows directory.
//! ```
//!
//! と言って `C:\Windows` へ落ちる。つまり `zai .` で開いたフォルダで動くはずの
//! エージェントが `C:\Windows` で起動してしまう (端末は `cmd.exe /C <command>`
//! 経由で起動するため、この経路をすべて通る)。
//!
//! そこでアプリが保持するパスは最初から素の形 (`C:\...`) に揃える。
//! canonicalize の目的 (シンボリックリンク差と `..` の吸収) は接頭辞を外しても
//! 失われない。macOS / Linux では canonicalize と同じ挙動になる。

use std::path::{Component, Path, PathBuf};

/// Windows の canonicalize が付ける `\\?\` 接頭辞を外した素のパス。
/// 接頭辞が無ければそのまま返す (macOS / Linux では常に素通し)。
///
/// - `\\?\C:\a` → `C:\a`
/// - `\\?\UNC\srv\share\a` → `\\srv\share\a` (ネットワークパスの素の形)
/// - `\\?\Volume{…}\a` → そのまま (ドライブ文字が無く、外すと解決できなくなる)
pub fn plain(p: PathBuf) -> PathBuf {
    match plain_str(&p.to_string_lossy()) {
        Some(s) => PathBuf::from(s),
        None => p,
    }
}

/// [`plain`] の文字列版。変換が不要な入力には `None` を返す。
///
/// 文字列処理だけを切り出してあるのは、OS を問わずテストできるようにするため
/// (Windows 以外でも `\\?\` 付きの入力を与えて挙動を確かめられる)。
fn plain_str(s: &str) -> Option<String> {
    let rest = s.strip_prefix(r"\\?\")?;
    if let Some(unc) = rest.strip_prefix(r"UNC\") {
        return Some(format!(r"\\{unc}"));
    }
    // `C:` で始まるものだけ素に戻す。`Volume{GUID}` 形式は接頭辞込みでしか
    // 解決できないので触らない。
    let b = rest.as_bytes();
    let has_drive = b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':';
    has_drive.then(|| rest.to_string())
}

/// canonicalize してから [`plain`] を当てる。
/// 解決できないパス (存在しない等) は入力のまま返す。
pub fn canonical(p: &Path) -> PathBuf {
    match p.canonicalize() {
        Ok(c) => plain(c),
        Err(_) => p.to_path_buf(),
    }
}

/// 子プロセス (PTY / 外部コマンド) の作業ディレクトリとして安全なパス。
///
/// verbatim 接頭辞を外し、ディレクトリとして実在することまで確かめる。
/// 実在しなければホーム → 一時ディレクトリへ落とす: 存在しない cwd は spawn
/// 自体の失敗になり「エージェントが起動しない」形で表に出るため、
/// 起動できる場所へ寄せてから渡す。
pub fn launch_dir(p: &Path) -> PathBuf {
    let plain_p = plain(p.to_path_buf());
    if plain_p.is_dir() {
        return plain_p;
    }
    if let Some(home) = dirs::home_dir().map(plain).filter(|h| h.is_dir()) {
        return home;
    }
    std::env::temp_dir()
}

// ═══════════════════════════════════════════════════════════════════════════
//  シンボリックリンクの解決 (ゲートの抜け道を塞ぐための最小の道具)
// ═══════════════════════════════════════════════════════════════════════════

/// リンクを解くのに許す段数。これを超えたら**輪になっている**と見なす。
/// POSIX の `SYMLOOP_MAX` は 8 以上と決まっているだけなので、実装の上限
/// (Linux 40 / macOS 32) に合わせて 40 を採る。
const MAX_HOPS: u32 = 40;

/// [`LinkResolver::resolve`] の答え。**3 通りを区別する**のが肝で、
/// 呼ぶ側によって「判らない」を倒す向きが逆になる:
///
/// * ゲート (書き込みを止める側) は [`Resolved::Unknown`] を**止める**へ倒す
/// * 照合 (担当が当たるかを見る側) は [`Resolved::Unknown`] を**字句の答えのまま**にする
///   (実体が判らないからといって、字句で当たっていた担当を落としてはいけない)
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolved {
    /// 実体もルートの中。値はルート相対の正規形 ([`crate::lease::normalize_path`])。
    Inside(String),
    /// 実体がルートの**外**を指している (リポジトリ内のリンク越しの脱出)。
    Outside,
    /// リンクが輪になっている / 読めない。**判定できない**。
    Unknown,
}

/// ルート相対のパスを、シンボリックリンクを 1 段ずつ解いて実体へ寄せる。
///
/// ## なぜ字句正規化だけでは足りないか
///
/// [`crate::lease::normalize_path`] は `.` を捨て `..` を畳み、`\` を `/` へ
/// 寄せ、大小を畳む — **字句だけ**の正規化である。同じ実体を指す 2 つの綴り
/// (`lib/app.rs` と `src/app.rs`、`lib -> src`) は字句では別物のままなので、
/// 担当表 (台帳) をリンク越しの綴りで書いた人と、実体の綴りで書いた人が
/// **同じ行を同時に持てる**。
///
/// ## なぜ実体解決だけでも足りないか
///
/// `canonicalize` は**その瞬間**の実体しか答えない。リンクは判定とコミットの
/// 間に張り替えられる (TOCTOU) し、まだ存在しないファイルは解決できない。
/// 実体の答えだけを採ると、リンクを張り替えるだけで担当から外れてしまう。
///
/// ## 採った方針: **字句 ∪ 実体** (どちらかで当たれば当たり)
///
/// 照合側は字句の綴りを捨てずに、実体の綴りを**足す**。減らさないので
/// TOCTOU で緩む方向へは動かない (張り替えても字句の答えは残る)。
/// 実体が判らないとき ([`Resolved::Unknown`]) も字句の答えが残る。
///
/// ## 速さ
///
/// ディレクトリの解決を憶える。`src/` 配下に 200 個の担当があっても
/// `src` を探るのは 1 回で済む (葉だけが担当ごとに 1 回)。
/// **`canonicalize` を使わない** — 途中に実在しない要素があっても答えを返す
/// 必要があり、また [`crate::guard`] の `canon_key` の呼び出し回数を
/// 固定しているテストを鈍らせないため。
pub struct LinkResolver {
    root: PathBuf,
    /// 憶えたディレクトリ: (ルート相対の字句の綴り, 解決後の絶対パス)。
    /// 台帳の規模でしか増えないので線形走査で足りる (`HashMap` を持つと
    /// 反復順が非決定になり、判定の決定性を説明しづらくなる)。
    dirs: Vec<(String, PathBuf)>,
}

impl LinkResolver {
    /// `root` (作業ツリーの頂点) を基準にする解決器。
    pub fn new(root: &Path) -> Self {
        Self {
            root: canonical(root),
            dirs: Vec::new(),
        }
    }

    /// `rel` (ルート相対) の実体を、ルート相対の正規形で返す。
    ///
    /// 末尾の `/**` (「配下ぜんぶ」の印) は実体を持たないので落としてから解く。
    pub fn resolve(&mut self, rel: &str) -> Resolved {
        let norm = crate::lease::normalize_path(rel);
        let body = norm.strip_suffix("/**").unwrap_or(&norm);
        let segs: Vec<&str> = body.split('/').filter(|s| !s.is_empty()).collect();
        let mut acc = self.root.clone();
        let mut hops = 0u32;
        let last = segs.len().saturating_sub(1);
        for (i, seg) in segs.iter().enumerate() {
            // ディレクトリの段だけ憶える (葉は担当ごとに違うので憶えても当たらない)。
            let memo_key = (i < last).then(|| segs[..=i].join("/"));
            if let Some(k) = &memo_key {
                if let Some((_, p)) = self.dirs.iter().find(|(kk, _)| kk == k) {
                    acc = p.clone();
                    continue;
                }
            }
            match step(&acc, seg, &mut hops) {
                Some(next) => acc = next,
                // 輪 / 読めない。**ここで打ち切って `Unknown`** — 途中まで
                // 解けた形を返すと、解けていない部分を「解けた」と誤読させる。
                None => return Resolved::Unknown,
            }
            if let Some(k) = memo_key {
                self.dirs.push((k, acc.clone()));
            }
        }
        // **最後にだけ**中外を決める。途中でルートの外へ出ても、その先の
        // リンクで戻ってくることがある (`a -> ../out`, `out/back -> ../repo/src`)。
        // 途中で打ち切ると、戻ってくる場合の担当を落とす = 緩む方向。
        //
        // **比べる前に両側を同じ形へ寄せる。** `self.root` は `canonical` を
        // 通してあるが `acc` はリンクの行き先を**書かれたまま**積んだ形なので、
        // Windows では `\\?\` の有無・8.3 短縮名・ドライブ文字の大小が食い違い、
        // 中に居るのに `Outside` になる (CI の windows-latest だけで実際に落ちた:
        // `left: Outside / right: Inside("src/app.rs")`)。
        let acc = settle(&acc);
        match acc.strip_prefix(&self.root) {
            Ok(r) => Resolved::Inside(crate::lease::normalize_path(&r.to_string_lossy())),
            Err(_) => Resolved::Outside,
        }
    }
}

/// `base/seg` を 1 段だけ進める (`base` は解決済み)。
/// 輪 / 読めないときだけ `None`。
///
/// **リンクの行き先は「1 つの葉」ではなく「並んだ要素」として扱う。**
/// 行き先の途中に別のリンクが居ることがある (macOS の `/var` →
/// `private/var` がまさにこれ) ので、葉だけを見て済ませると
/// **外へ出たまま戻ってこられず、実体が中にあるのに「外」と答える**。
/// 実際にこれで「外へ出てから戻るリンク」の検査が落ちた。
/// 比較できる形へ寄せる。**存在しないパスでも必ず答えを返す。**
///
/// [`canonical`] は解決に失敗すると入力をそのまま返すので、まだ作られていない
/// ファイルは寄らないままになる。そこで**実在する最深の祖先まで戻って解決し、
/// 残りを継ぎ足す**。これが無いと「フォルダを作った瞬間に判定が変わる」形になる。
fn settle(p: &Path) -> PathBuf {
    let direct = canonical(p);
    if direct != p {
        return direct;
    }
    // 祖先を辿って、解決できたところから積み直す。
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    while let Some(parent) = cur.parent().map(Path::to_path_buf) {
        let Some(name) = cur.file_name().map(|s| s.to_os_string()) else {
            break;
        };
        tail.push(name);
        let base = canonical(&parent);
        if base != parent {
            let mut out = base;
            for seg in tail.iter().rev() {
                out.push(seg);
            }
            return out;
        }
        if parent.as_os_str().is_empty() {
            break;
        }
        cur = parent;
    }
    plain(p.to_path_buf())
}

fn step(base: &Path, seg: &str, hops: &mut u32) -> Option<PathBuf> {
    let mut acc = base.to_path_buf();
    let mut queue: std::collections::VecDeque<std::ffi::OsString> =
        std::collections::VecDeque::new();
    queue.push_back(std::ffi::OsString::from(seg));
    while let Some(c) = queue.pop_front() {
        if c == std::ffi::OsStr::new(".") {
            continue;
        }
        if c == std::ffi::OsStr::new("..") {
            acc.pop();
            continue;
        }
        let next = acc.join(&c);
        probe();
        // 実在しなければリンクではあり得ない (まだ作られていないファイルも通す)。
        let Ok(meta) = std::fs::symlink_metadata(&next) else {
            acc = next;
            continue;
        };
        if !meta.is_symlink() {
            acc = next;
            continue;
        }
        *hops += 1;
        if *hops > MAX_HOPS {
            return None;
        }
        let target = std::fs::read_link(&next).ok()?;
        // 絶対リンクはファイルシステムの根から積み直す。
        if target.is_absolute() {
            acc = PathBuf::new();
        }
        for x in target
            .components()
            .map(|x| x.as_os_str().to_os_string())
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
        {
            queue.push_front(x);
        }
    }
    Some(acc)
}

/// `.` を捨て `..` を畳むだけの正規化 (**実体を見ない**)。
/// リンクの行き先に `..` が入っていても、そこだけは字句で畳んでよい —
/// 既に 1 段解いた後なので、畳んだ結果をもう一度 [`step`] が実体で確かめる。
///
/// [`canonical`] と違って**実在しないパスでも答えを返す**うえ、リンクを
/// 追わない。「リポジトリの中を指す綴りか」を先に字句で決めてから実体を
/// 解く、という順番のために要る (実体から入ると、リンクで外へ出た時点で
/// 「関知しない」に落ちて**ゲートを素通りする**)。
pub fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

// 実体を探るシステムコールを何回撃ったか。**テストだけが読む。**
//
// `guard` の `canon_key` と同じ理由でスレッドローカルにする (プロセス共通に
// すると同時に走る他のテストの呼び出しが混ざる)。実時間で線を引くと
// Docker の仮想ファイルシステムで嘘の赤が出るので、**回数**で固定する。
#[cfg(test)]
thread_local! {
    static LINK_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn probe() {
    LINK_PROBES.with(|c| c.set(c.get() + 1));
}

#[cfg(not(test))]
fn probe() {}

/// これまでの探り回数を読んで 0 に戻す。**テスト専用。**
#[cfg(test)]
pub(crate) fn link_probes_take() -> usize {
    LINK_PROBES.with(|c| {
        let v = c.get();
        c.set(0);
        v
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_strips_verbatim_drive_prefix() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\C:\Users\me\proj")),
            PathBuf::from(r"C:\Users\me\proj")
        );
        // 小文字ドライブ / ドライブ直下も同じ
        assert_eq!(plain(PathBuf::from(r"\\?\d:\")), PathBuf::from(r"d:\"));
    }

    #[test]
    fn plain_converts_verbatim_unc_to_plain_unc() {
        assert_eq!(
            plain(PathBuf::from(r"\\?\UNC\srv\share\proj")),
            PathBuf::from(r"\\srv\share\proj")
        );
    }

    #[test]
    fn plain_leaves_untouchable_paths_alone() {
        // ドライブ文字を持たない verbatim パスは外すと解決できなくなる
        let vol = PathBuf::from(r"\\?\Volume{9f8a}\proj");
        assert_eq!(plain(vol.clone()), vol);
        // 接頭辞が無いパス (Windows / POSIX どちらも) は素通し
        for p in [r"C:\Users\me", r"\\srv\share", "/home/me/proj", "rel/ative"] {
            assert_eq!(plain(PathBuf::from(p)), PathBuf::from(p), "{p}");
        }
    }

    #[test]
    fn canonical_never_returns_a_verbatim_path() {
        let dir = crate::test_util::unique_temp_dir("zaivern-pathx-test", "canon");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let c = canonical(&dir);
        assert!(
            !c.to_string_lossy().starts_with(r"\\?\"),
            "canonical は素のパスを返す: {}",
            c.display()
        );
        assert!(c.is_dir(), "指しているものは変わらない: {}", c.display());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn canonical_keeps_unresolvable_input_as_is() {
        let ghost = PathBuf::from("no/such/dir-for-zaivern-pathx");
        assert_eq!(canonical(&ghost), ghost);
    }

    #[test]
    fn launch_dir_returns_an_existing_directory() {
        let dir = crate::test_util::unique_temp_dir("zaivern-pathx-test", "launch");
        std::fs::create_dir_all(&dir).expect("mkdir");
        assert_eq!(launch_dir(&dir), dir, "実在するディレクトリはそのまま");

        // 消えたフォルダを cwd にしようとしても、起動できる場所へ落ちる
        let ghost = dir.join("gone");
        let fallback = launch_dir(&ghost);
        assert!(fallback.is_dir(), "{} は実在すべき", fallback.display());
        assert_ne!(fallback, ghost);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ───────────────────── シンボリックリンクの解決 ─────────────────────

    /// リンクを 1 本作る。作れない環境 (Windows で開発者モードが無い等) は `false`。
    /// **両方の OS を実装する** — `cfg` を片側だけ書くと、その OS では
    /// 一度もコンパイルされないまま「動くはず」になる。
    fn link(target: &Path, at: &Path, dir: bool) -> bool {
        #[cfg(unix)]
        {
            let _ = dir;
            std::os::unix::fs::symlink(target, at).is_ok()
        }
        #[cfg(windows)]
        {
            if dir {
                std::os::windows::fs::symlink_dir(target, at).is_ok()
            } else {
                std::os::windows::fs::symlink_file(target, at).is_ok()
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let (_, _, _) = (target, at, dir);
            false
        }
    }

    fn tree(tag: &str) -> PathBuf {
        let d = crate::test_util::unique_temp_dir("zaivern-pathx-test", tag);
        std::fs::create_dir_all(d.join("src")).expect("mkdir");
        std::fs::write(d.join("src/app.rs"), "x\n").expect("write");
        d
    }

    #[test]
    fn 中を指すリンク越しの綴りは実体の綴りへ寄る() {
        let root = tree("inside");
        if !link(Path::new("src"), &root.join("lib"), true) {
            std::fs::remove_dir_all(&root).ok();
            return; // リンクを作れない環境では検査しない
        }
        let mut r = LinkResolver::new(&root);
        assert_eq!(
            r.resolve("lib/app.rs"),
            Resolved::Inside(crate::lease::normalize_path("src/app.rs")),
            "リンク越しの綴りが実体へ寄る"
        );
        // 実体の綴りはそのまま
        assert_eq!(
            r.resolve("src/app.rs"),
            Resolved::Inside(crate::lease::normalize_path("src/app.rs"))
        );
        // まだ存在しないファイルでも、途中のリンクは解ける
        assert_eq!(
            r.resolve("lib/new.rs"),
            Resolved::Inside(crate::lease::normalize_path("src/new.rs"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 外を指すリンクは外だと答える() {
        let root = tree("outside");
        let away = crate::test_util::unique_temp_dir("zaivern-pathx-test", "outside-away");
        std::fs::create_dir_all(&away).expect("mkdir");
        if !link(&away, &root.join("out"), true) {
            std::fs::remove_dir_all(&root).ok();
            std::fs::remove_dir_all(&away).ok();
            return;
        }
        let mut r = LinkResolver::new(&root);
        assert_eq!(r.resolve("out/x.rs"), Resolved::Outside);
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&away).ok();
    }

    /// 途中で外へ出ても、その先のリンクで戻ってくることがある。
    /// **途中で打ち切ると担当を落とす** (= 判定が緩む) ので、最後にだけ決める。
    #[test]
    fn 外へ出てから戻るリンクは中だと答える() {
        let root = tree("roundtrip");
        let away = crate::test_util::unique_temp_dir("zaivern-pathx-test", "roundtrip-away");
        std::fs::create_dir_all(&away).expect("mkdir");
        if !link(&away, &root.join("out"), true)
            || !link(&root.join("src"), &away.join("back"), true)
        {
            std::fs::remove_dir_all(&root).ok();
            std::fs::remove_dir_all(&away).ok();
            return;
        }
        let mut r = LinkResolver::new(&root);
        assert_eq!(
            r.resolve("out/back/app.rs"),
            Resolved::Inside(crate::lease::normalize_path("src/app.rs"))
        );
        std::fs::remove_dir_all(&root).ok();
        std::fs::remove_dir_all(&away).ok();
    }

    #[test]
    fn 輪になったリンクは判らないと答える() {
        let root = tree("loop");
        if !link(Path::new("b"), &root.join("a"), false)
            || !link(Path::new("a"), &root.join("b"), false)
        {
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        let mut r = LinkResolver::new(&root);
        assert_eq!(r.resolve("a"), Resolved::Unknown, "輪は判らないと答える");
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn 字句の畳み込みは実体の解決より先に効く() {
        let root = tree("lexical");
        let mut r = LinkResolver::new(&root);
        let want = Resolved::Inside(crate::lease::normalize_path("src/app.rs"));
        for spec in [
            "src/app.rs",
            "./src/app.rs",
            "src/./app.rs",
            "src/sub/../app.rs",
        ] {
            assert_eq!(r.resolve(spec), want, "{spec}");
        }
        // 先頭を越える `..` は落ちる (スコープ相対なので外は関知しない)
        assert_eq!(r.resolve("../src/app.rs"), want);
        // 末尾の `/**` (配下ぜんぶ) は実体を持たないので外してから解く
        assert_eq!(
            r.resolve("src/"),
            Resolved::Inside(crate::lease::normalize_path("src"))
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 大小非区別の OS では綴りの大小でリンクを迂回できない。
    /// **OS で分岐する既定値は、テストも OS 条件を明示する。**
    #[test]
    fn 大小の違いで別物にならない() {
        let root = tree("case");
        let mut r = LinkResolver::new(&root);
        let got = r.resolve("SRC/APP.RS");
        if cfg!(any(windows, target_os = "macos")) {
            assert_eq!(
                got,
                Resolved::Inside(crate::lease::normalize_path("src/app.rs")),
                "大小非区別の OS では同じ実体"
            );
        } else {
            assert_eq!(
                got,
                Resolved::Inside("SRC/APP.RS".to_string()),
                "大小区別の OS では別のパス"
            );
        }
        std::fs::remove_dir_all(&root).ok();
    }

    /// Windows の verbatim / UNC 綴りでルートを渡してもゲートを迂回できない。
    #[cfg(windows)]
    #[test]
    fn verbatim接頭辞のルートでも同じ答えになる() {
        let root = tree("verbatim");
        let verbatim = PathBuf::from(format!(r"\\?\{}", canonical(&root).display()));
        let mut a = LinkResolver::new(&root);
        let mut b = LinkResolver::new(&verbatim);
        assert_eq!(a.resolve("src/app.rs"), b.resolve("src/app.rs"));
        // Windows の区切りでも同じ
        assert_eq!(a.resolve(r"src\app.rs"), b.resolve("src/app.rs"));
        std::fs::remove_dir_all(&root).ok();
    }

    /// **ディレクトリの解決を憶えること。** 台帳に同じフォルダ配下の担当が
    /// 200 個並んでも、探る回数は「フォルダ 1 回 + 葉 1 個につき 1 回」で
    /// 収まる (パス × 担当で増やさない)。
    /// **実時間ではなく回数で固定する** — 仮想ファイルシステムでは時間が嘘をつく。
    #[test]
    fn ディレクトリの解決を憶えるので探る回数が積で増えない() {
        let root = tree("memo");
        let mut r = LinkResolver::new(&root);
        let n = 200;
        let _ = link_probes_take();
        for i in 0..n {
            let _ = r.resolve(&format!("src/f{i}.rs"));
        }
        let probes = link_probes_take();
        eprintln!("同じフォルダ配下 {n} 件: 探り {probes} 回");
        // フォルダ 1 回 + 葉 n 回 + 余裕。憶えていなければ 2n を超える。
        assert!(
            probes <= n + 4,
            "ディレクトリの解決を憶えていない: {probes} 回 (上限 {})",
            n + 4
        );
        std::fs::remove_dir_all(&root).ok();
    }

    /// 本命の回帰: PTY へ渡す cwd に `\\?\` が残っていると cmd.exe が
    /// `C:\Windows` へ落ちるので、実在チェックの前に必ず素へ戻す。
    #[cfg(windows)]
    #[test]
    fn launch_dir_strips_verbatim_prefix_from_real_dir() {
        let dir = crate::test_util::unique_temp_dir("zaivern-pathx-test", "verbatim");
        std::fs::create_dir_all(&dir).expect("mkdir");
        let verbatim = PathBuf::from(format!(r"\\?\{}", plain(canonical(&dir)).display()));
        let got = launch_dir(&verbatim);
        assert!(
            !got.to_string_lossy().starts_with(r"\\?\"),
            "{}",
            got.display()
        );
        assert!(got.is_dir());
        std::fs::remove_dir_all(&dir).ok();
    }
}
