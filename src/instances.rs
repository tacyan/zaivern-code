//! 実行中インスタンスのレジストリ — 「アプリとして起動しているか」を
//! どの OS でも検知するための仕組み。
//!
//! 起動時に `~/.zaivern/instances/<pid>.json` を書き、終了時 (panic の
//! 巻き戻し含む) に [`RegistryGuard`] の Drop で消す。クラッシュで残った
//! ファイルはスキャン ([`scan_and_prune`]) のたびに生存確認して掃除する。
//! `zai status` / `zai status --json` がこのレジストリを読んで一覧を出す
//! (終了コード 0 = 実行中あり / 1 = なし。スクリプトや CI から使える)。
//!
//! OS ごとの検知セマンティクス:
//! - **Linux**: `/proc/<pid>/stat` の starttime (起動からの clock tick) を
//!   シグネチャとして保存し、読み直して一致するかで PID 再利用を排除する。
//!   さらに ps/top で見つけやすいよう prctl(PR_SET_NAME) で comm を
//!   "zaivern-code" にする (comm は 15 文字上限、12 文字なので収まる)。
//! - **macOS**: kill(pid, 0) の生存確認に加え、「登録時刻が OS 起動時刻
//!   (sysctl kern.boottime) より前のエントリは本人ではあり得ない」という
//!   ヒューリスティックで PID 再利用を弾く — いま生きているプロセスは必ず
//!   OS 起動後に始まっているため。アクティビティモニタには実行ファイル名が
//!   そのまま出るので、表示名の細工はしない。
//! - **Windows**: `tasklist /FI "PID eq <pid>" /FO CSV /NH` (procx の隠し
//!   コンソール経由) でイメージ名を取り、登録された実行ファイル名と照合する。
//!   タスクマネージャーには zai.exe がそのまま出る。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::zaivern_dir;
use crate::i18n::{tr, trf};

/// レジストリの置き場所: `~/.zaivern/instances/`。
pub fn instances_dir() -> PathBuf {
    zaivern_dir().join("instances")
}

// ───────────────────────── エントリ ─────────────────────────

/// レジストリの 1 エントリ = 実行中 (だったかもしれない) インスタンス 1 つ。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceEntry {
    pub pid: u32,
    /// プロセス開始シグネチャ (PID 再利用対策)。
    /// Linux では /proc/<pid>/stat の starttime。他 OS は 0 (別手段で検証)。
    #[serde(default)]
    pub start_signature: u64,
    /// 実行ファイルのフルパス。取得できなければ空 (fail-soft)。
    #[serde(default)]
    pub exe: String,
    pub version: String,
    /// マルチルートワークスペースのルート一覧。
    #[serde(default)]
    pub workspace_roots: Vec<String>,
    /// 起動時刻 (UNIX epoch 秒)。
    pub launched_epoch: u64,
}

impl InstanceEntry {
    /// 現在のプロセスを指すエントリを作る。
    pub fn current(roots: &[PathBuf]) -> Self {
        Self {
            pid: std::process::id(),
            start_signature: own_start_signature(),
            exe: std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            workspace_roots: roots.iter().map(|r| r.display().to_string()).collect(),
            launched_epoch: now_epoch(),
        }
    }
}

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

// ───────────────────────── 登録と後始末 ─────────────────────────

/// 自分のレジストリファイルを Drop で消すガード。
/// main が握っておけば、正常終了でも panic の巻き戻しでも後始末が走る。
pub struct RegistryGuard {
    path: PathBuf,
}

impl Drop for RegistryGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// 既定の場所 (`~/.zaivern/instances`) へ現在のプロセスを登録する。
pub fn register_current(roots: &[PathBuf]) -> Option<RegistryGuard> {
    register_in(&instances_dir(), roots)
}

/// `dir` へ現在のプロセスを登録する (テストからはディレクトリ注入で使う)。
/// 書き込みに失敗しても None を返すだけ — アプリの起動は止めない。
pub fn register_in(dir: &Path, roots: &[PathBuf]) -> Option<RegistryGuard> {
    std::fs::create_dir_all(dir).ok()?;
    // 先にクラッシュ残骸を掃除しておく (スキャン = 生存確認付き)。
    let _ = scan_and_prune(dir);
    let entry = InstanceEntry::current(roots);
    let path = dir.join(format!("{}.json", entry.pid));
    let json = serde_json::to_string_pretty(&entry).ok()?;
    // 同時起動した別インスタンスのスキャンに書きかけを読ませない (tmp → rename)。
    let tmp = dir.join(format!("{}.json.tmp", entry.pid));
    std::fs::write(&tmp, json).ok()?;
    std::fs::rename(&tmp, &path).ok()?;
    Some(RegistryGuard { path })
}

/// レジストリを走査し、生きているエントリだけを返す。
/// 死んだ PID・壊れたファイル (クラッシュ残骸) はその場で削除する。
pub fn scan_and_prune(dir: &Path) -> Vec<InstanceEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else {
        return out;
    };
    for ent in rd.flatten() {
        let path = ent.path();
        // *.json だけを対象にする (無関係なファイルは消さない)。
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let parsed = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<InstanceEntry>(&raw).ok());
        match parsed {
            Some(e) if entry_alive(&e) => out.push(e),
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    out.sort_by_key(|e| (e.launched_epoch, e.pid));
    out
}

// ───────────────────────── 生存確認 (OS 別) ─────────────────────────

/// PID が生きているか (シグナルを送らない存在確認のみ)。
///
/// unix: `kill(pid, 0)`。EPERM は「存在するが他人のプロセス」なので生存扱い。
/// Windows: `OpenProcess` + `WaitForSingleObject(0)`。**プロセスを起こさない。**
///
/// **Windows で `tasklist` を起こしてはいけない理由。** この関数は
/// [`crate::lease`] の `active()` → `prune()` から呼ばれ、その呼び出しは
/// `with_store` が**台帳のロックを握ったまま**行う。プロセス起動は
/// syscall の数千倍 (Windows の CreateProcess は数十 ms 級) なので、
/// リース 1 件でもロック待ちの上限 (`lease::LOCK_WAIT_MS`) を
/// 1 回で食い潰し得る。ロックを取れなかった側は fail-open するため、
/// **いちばん混んでいるとき (= いちばん衝突しやすいとき) にだけ
/// リースが効かなくなる**という最悪の壊れ方になる。
pub fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        osrule::alive_from(win::probe(pid))
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

// ─────────────── Windows の判定規則 (どの OS からでも検査できる形) ───────────────

/// OS 依存の判定 (Windows の Win32 戻り値・パス区切り) を、**引数だけ**から
/// 決める純粋関数群。
///
/// `keybinds::canonical_mods_on(m, mac)` と同じ流儀 — OS 依存の規則を引数へ
/// 追い出して、**macOS / Linux のホストからでもテーブルテストで固定できる**
/// ようにする。`#[cfg(windows)]` の中に規則を埋めると、開発機では 1 行も
/// 実行されないまま「コンパイルは通る」状態で寝てしまう。
///
/// 非 Windows では呼び出し元がテストだけになるので dead_code を許可する。
/// **これは「繋いでいない」のではなく、繋ぐ先が別 OS にしか無いという意味。**
mod osrule {
    #![cfg_attr(not(windows), allow(dead_code))]

    /// `GetLastError`: 権限が足りないだけ = **プロセスは存在する** (unix の EPERM 相当)。
    pub const ERROR_ACCESS_DENIED: u32 = 5;
    /// `GetLastError`: そんな PID は居ない (`OpenProcess` の主な失敗理由)。
    pub const ERROR_INVALID_PARAMETER: u32 = 87;
    /// `WaitForSingleObject`: シグナル済み = プロセスは**終了している**。
    pub const WAIT_OBJECT_0: u32 = 0;
    /// `WaitForSingleObject`: 時間切れ = まだ動いている。
    pub const WAIT_TIMEOUT: u32 = 0x0000_0102;

    /// `pid_alive` が Win32 から受け取る観測結果。
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Probe {
        /// `OpenProcess` に成功し、`WaitForSingleObject(h, 0)` がこの値を返した。
        Waited(u32),
        /// `OpenProcess` が失敗し、`GetLastError` がこの値を返した。
        OpenFailed(u32),
    }

    /// 観測結果 → 生死。
    ///
    /// **`GetExitCodeProcess` を使わない理由**: 実行中を表す `STILL_ACTIVE` は
    /// 259 で、「終了コード 259 で終わったプロセス」と区別が付かない。
    /// `WaitForSingleObject` はカーネルオブジェクトのシグナル状態を見るので
    /// この曖昧さが無い。
    pub fn alive_from(p: Probe) -> bool {
        match p {
            // シグナル済み = プロセスオブジェクトが終了している。
            Probe::Waited(WAIT_OBJECT_0) => false,
            // 時間切れ = 未シグナル = 実行中。
            Probe::Waited(WAIT_TIMEOUT) => true,
            // 想定外の戻り (WAIT_FAILED 等) は「開けた以上ハンドルは実在する」
            // ので生存側へ倒す (fail-safe: リースを早すぎるタイミングで奪わない)。
            Probe::Waited(_) => true,
            // 開けなかった理由で分ける。ACCESS_DENIED は「居るが触れない」。
            Probe::OpenFailed(ERROR_ACCESS_DENIED) => true,
            Probe::OpenFailed(ERROR_INVALID_PARAMETER) => false,
            // 理由不明の失敗は死亡扱い (存在の証拠が無い)。
            Probe::OpenFailed(_) => false,
        }
    }

    /// `tasklist /FO CSV /NH` の出力から該当 PID のイメージ名を取り出す。
    ///
    /// 出力例 (1 行): `"zai.exe","4242","Console","1","62,912 K"`
    /// 該当なしのときは `情報: …` / `INFO: …` の 1 行が来る (ロケール依存なので
    /// **文面では判定しない**。`"` で始まる CSV 行だけを見る)。
    pub fn tasklist_image_of(text: &str, pid: u32) -> Option<String> {
        let want = pid.to_string();
        for line in text.lines() {
            // CRLF の \r は lines() が落とさない列がある (末尾列) ため trim する。
            let line = line.trim();
            if !line.starts_with('"') {
                continue;
            }
            let mut cols = line.trim_start_matches('"').split("\",\"");
            let Some(image) = cols.next() else { continue };
            if cols.next().map(|p| p.trim_matches('"')) == Some(want.as_str()) {
                return Some(image.trim_matches('"').to_string());
            }
        }
        None
    }

    /// パス文字列の basename を、**区切り規則を引数で受けて**取り出す。
    ///
    /// `std::path::Path::file_name` はホスト OS の規則しか知らないので、
    /// macOS から `C:\\x\\zai.exe` を渡すと `\\` が区切りに見えず
    /// **文字列まるごと**が返る。ここを引数化しておかないと、Windows の
    /// 規則は Windows でしか確かめられない。
    pub fn basename_on(path: &str, win_sep: bool) -> String {
        let mut cut = path.rfind('/').map(|i| i + 1).unwrap_or(0);
        if win_sep {
            if let Some(i) = path.rfind('\\') {
                cut = cut.max(i + 1);
            }
        }
        path[cut..].to_string()
    }

    /// イメージ名の照合 (Windows のファイル名は大小非区別)。
    /// `expect` が空 = 照合材料が無いので「合っている」とみなす (fail-open)。
    pub fn image_matches(image: &str, expect: &str) -> bool {
        expect.is_empty() || image.eq_ignore_ascii_case(expect)
    }
}

/// kernel32 の直叩き。`windows-sys` の feature を増やさずに済むよう、
/// `textenc.rs` と同じ流儀で必要な 5 つだけ宣言する
/// (`Win32_System_Threading` を Cargo.toml へ足すと依存の解決面が広がる)。
#[cfg(windows)]
mod win {
    use std::ffi::c_void;

    /// 終了コード・イメージ名の照会に足りる最小権限 (Vista 以降)。
    /// `PROCESS_QUERY_INFORMATION` と違い、権限の低いプロセスからでも通る。
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    /// `WaitForSingleObject` に要る権限。
    const SYNCHRONIZE: u32 = 0x0010_0000;

    #[link(name = "kernel32")]
    extern "system" {
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut c_void;
        fn WaitForSingleObject(handle: *mut c_void, millis: u32) -> u32;
        fn CloseHandle(handle: *mut c_void) -> i32;
        fn GetLastError() -> u32;
        fn QueryFullProcessImageNameW(
            handle: *mut c_void,
            flags: u32,
            buf: *mut u16,
            size: *mut u32,
        ) -> i32;
    }

    /// PID を 1 回だけ観測する。**プロセスは起こさない** (syscall 3 回)。
    pub fn probe(pid: u32) -> super::osrule::Probe {
        let h = unsafe { OpenProcess(SYNCHRONIZE | PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return super::osrule::Probe::OpenFailed(unsafe { GetLastError() });
        }
        let rc = unsafe { WaitForSingleObject(h, 0) };
        unsafe { CloseHandle(h) };
        super::osrule::Probe::Waited(rc)
    }

    /// PID のイメージ名 (basename)。取れなければ None。
    /// 長さは固定で決め打たず、`MAX_PATH` から始めて足りなければ倍にする
    /// (長いパス有効時は 32767 まで伸びうる)。
    pub fn image_name(pid: u32) -> Option<String> {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h.is_null() {
            return None;
        }
        let mut cap: usize = 260;
        let mut out = None;
        while cap <= 32_768 {
            let mut buf = vec![0u16; cap];
            let mut len = cap as u32;
            let ok = unsafe { QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut len) };
            if ok != 0 {
                let full = String::from_utf16_lossy(&buf[..len as usize]);
                out = std::path::Path::new(&full)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string());
                break;
            }
            cap *= 2;
        }
        unsafe { CloseHandle(h) };
        out
    }
}

/// エントリが「いまも同じプロセスとして」生きているか。
/// 単なる PID 生存ではなく、PID 再利用をモジュールドキュメントの
/// OS 別セマンティクスで排除する。
fn entry_alive(e: &InstanceEntry) -> bool {
    if e.pid == 0 {
        return false;
    }
    #[cfg(target_os = "linux")]
    {
        if !Path::new(&format!("/proc/{}", e.pid)).exists() {
            return false;
        }
        if e.start_signature != 0 {
            // starttime が一致 = 同一プロセス。不一致 = PID 再利用。
            return proc_starttime(e.pid) == Some(e.start_signature);
        }
        // シグネチャ無し (他 OS で書かれた等): 読めるなら exe 名で代替検証。
        if let Ok(link) = std::fs::read_link(format!("/proc/{}/exe", e.pid)) {
            let expect = expected_image_name(e);
            if expect.is_empty() {
                return true;
            }
            let name = link
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            // 再ビルド直後は " (deleted)" が付くことがある
            return name.trim_end_matches(" (deleted)") == expect;
        }
        pid_alive(e.pid)
    }
    #[cfg(all(unix, not(target_os = "linux")))]
    {
        if !pid_alive(e.pid) {
            return false;
        }
        // PID 再利用ヒューリスティック: いま生きているプロセスは必ず OS 起動後に
        // 始まっている。登録時刻が起動時刻より前 (時計の揺れに 5 秒の余裕) なら、
        // 記録した本人はもういない — 生きて見えるのは PID を引き継いだ別物。
        if let Some(boot) = boot_epoch() {
            if e.launched_epoch + 5 < boot {
                return false;
            }
        }
        true
    }
    #[cfg(windows)]
    {
        // 生存確認は syscall だけで済ませる (tasklist を起こさない)。
        if !pid_alive(e.pid) {
            return false;
        }
        let expect = expected_image_name(e);
        if expect.is_empty() {
            return true;
        }
        // イメージ名は QueryFullProcessImageNameW が第一候補。取れないとき
        // (権限が足りない等) だけ tasklist へ落ちる — ここへ来るのは起動時と
        // `zai status` だけで、リースのロック保持中ではない。
        match win::image_name(e.pid).or_else(|| tasklist_image(e.pid)) {
            Some(image) => osrule::image_matches(&image, &expect),
            // 名前が取れない = 照合できないだけ。生きているのは確認済みなので残す。
            None => true,
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// 照合に使う実行ファイル名。登録された exe の basename を第一候補、
/// 無ければ自分自身の exe 名。どちらも取れなければ空 (= 照合をスキップ)。
// macOS からでも規則を検査できるよう、どの OS でもコンパイルする
// (呼び出し元が居るのは Windows / Linux だけ)。
#[cfg_attr(not(any(windows, target_os = "linux")), allow(dead_code))]
fn expected_image_name(e: &InstanceEntry) -> String {
    // 区切り規則は cfg!(windows) を**引数として**渡す (Path::file_name だと
    // ホスト OS の規則しか使えず、macOS から Windows の規則を検査できない)。
    let base = osrule::basename_on(&e.exe, cfg!(windows));
    if !base.is_empty() {
        return base;
    }
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().to_string()))
        .unwrap_or_default()
}

/// Linux: /proc/<pid>/stat の starttime (第22フィールド)。
/// comm は括弧つきで空白を含み得るため、最後の ')' より後ろを読む。
#[cfg(target_os = "linux")]
fn proc_starttime(pid: u32) -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let rest = stat.rsplit_once(')')?.1;
    // ')' の後は state(3) から始まる → starttime(22) は 0 起点で 19 番目
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// 自分自身の開始シグネチャ。Linux 以外は 0 (別手段で検証するため)。
fn own_start_signature() -> u64 {
    #[cfg(target_os = "linux")]
    {
        proc_starttime(std::process::id()).unwrap_or(0)
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}

/// macOS: OS の起動時刻 (epoch 秒)。取れなければ None (検証をスキップ)。
#[cfg(all(unix, not(target_os = "linux")))]
fn boot_epoch() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let name = std::ffi::CString::new("kern.boottime").ok()?;
        let mut tv = libc::timeval {
            tv_sec: 0,
            tv_usec: 0,
        };
        let mut len = std::mem::size_of::<libc::timeval>() as libc::size_t;
        let rc = unsafe {
            libc::sysctlbyname(
                name.as_ptr(),
                &mut tv as *mut libc::timeval as *mut libc::c_void,
                &mut len,
                std::ptr::null_mut(),
                0,
            )
        };
        if rc == 0 && tv.tv_sec > 0 {
            Some(tv.tv_sec as u64)
        } else {
            None
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Windows: tasklist の CSV 出力から該当 PID のイメージ名を取り出す。
/// **QueryFullProcessImageNameW が取れなかったときの保険**でしかない
/// (プロセス起動を払うので、生存確認の臨界路からは外してある)。
/// 解析そのものは [`osrule::tasklist_image_of`] にあり、どの OS でもテストできる。
#[cfg(windows)]
fn tasklist_image(pid: u32) -> Option<String> {
    let out = crate::procx::hidden_command("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    osrule::tasklist_image_of(&String::from_utf8_lossy(&out.stdout), pid)
}

// ───────────────────────── プロセス名 ─────────────────────────

/// 起動直後のプロセス整形 — どの OS でも「Zaivern」で見つかる状態にする。
///
/// - **Linux**: prctl(PR_SET_NAME) で comm を "zaivern-code" にする
///   (comm は 15 文字上限、12 文字なので収まる)。`ps` / `top` / `pgrep zaivern`。
/// - **macOS**: プロセス名は実行ファイルの basename で決まるので、
///   `zai app install` が作る `.app` の実体を `Contents/MacOS/Zaivern` に
///   している ([`crate::desktop::MACOS_EXEC_NAME`])。ここではその `.app`
///   起動時 (cwd=`/`) の作業ディレクトリだけ直す — 旧ランチャースクリプトの
///   `cd "$HOME"` の代替。ターミナルの `zai` は一切影響を受けない。
/// - **Windows**: `zai.exe` のまま出るが、build.rs が埋める版情報リソースで
///   タスクマネージャーの「説明」列に "Zaivern Code" が出る。
pub fn set_process_name() {
    #[cfg(target_os = "linux")]
    {
        if let Ok(name) = std::ffi::CString::new("zaivern-code") {
            unsafe {
                libc::prctl(libc::PR_SET_NAME, name.as_ptr());
            }
        }
    }
    crate::desktop::normalize_app_launch_cwd();
}

// ───────────────────────── 表示 (純粋関数) ─────────────────────────

/// 稼働時間を人間向けに (例: "42秒" / "5分3秒" / "2時間30分" / "3日2時間")。
///
/// 単位語まで選択中の言語で出す。`format!` ではなく [`trf`] を通すのは、
/// 言語によって単位の綴りも並びも変わるため (en は `{d}d {h}h`)。
/// テンプレートの綴りは**辞書の鍵そのもの**なので 1 文字も変えないこと。
pub fn humanize_uptime(secs: u64) -> String {
    let (d, h, m, s) = (
        secs / 86_400,
        (secs / 3600) % 24,
        (secs / 60) % 60,
        secs % 60,
    );
    if d > 0 {
        trf(
            "{d}日{h}時間",
            &[("d", d.to_string()), ("h", h.to_string())],
        )
    } else if h > 0 {
        trf(
            "{h}時間{m}分",
            &[("h", h.to_string()), ("m", m.to_string())],
        )
    } else if m > 0 {
        trf("{m}分{s}秒", &[("m", m.to_string()), ("s", s.to_string())])
    } else {
        trf("{s}秒", &[("s", s.to_string())])
    }
}

/// `zai status` 用のテーブル文字列を作る。副作用なし (テーブルテスト用)。
pub fn render_table(entries: &[InstanceEntry], now_epoch: u64) -> String {
    if entries.is_empty() {
        return tr("実行中の Zaivern Code はありません。");
    }
    // "PID" は訳さない — どの言語でも PID と書く頭字語で、辞書にも項目が無い。
    let mut rows: Vec<[String; 4]> = vec![[
        "PID".to_string(),
        tr("バージョン"),
        tr("稼働時間"),
        tr("ワークスペース"),
    ]];
    for e in entries {
        rows.push([
            e.pid.to_string(),
            e.version.clone(),
            humanize_uptime(now_epoch.saturating_sub(e.launched_epoch)),
            e.workspace_roots.join(", "),
        ]);
    }
    // 表示幅 (雑に 非ASCII=全角2 とみなす) で桁を揃える
    fn width(s: &str) -> usize {
        s.chars().map(|c| if c.is_ascii() { 1 } else { 2 }).sum()
    }
    let mut w = [0usize; 4];
    for r in &rows {
        for i in 0..4 {
            w[i] = w[i].max(width(&r[i]));
        }
    }
    let mut out = String::new();
    for r in &rows {
        for i in 0..4 {
            out.push_str(&r[i]);
            if i < 3 {
                out.extend(std::iter::repeat(' ').take(w[i] - width(&r[i]) + 2));
            }
        }
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// `zai status --json` 用。エントリ配列をそのまま JSON にする (機械可読)。
pub fn render_json(entries: &[InstanceEntry]) -> String {
    serde_json::to_string(entries).unwrap_or_else(|_| "[]".to_string())
}

/// `zai status --pid-only` 用。PID だけを 1 行 1 つで返す。
///
/// `zai status --pid-only | xargs kill` のようにテーブルを解析させずに
/// パイプへ流すための形式。空 (実行中なし) なら**空文字列**を返し、
/// 呼び出し側は何も print せず終了コード 1 にする — `xargs` に空行を
/// 渡さないため (空行を渡すと kill が引数無しでエラーになる)。

#[allow(dead_code)]
pub fn render_pids(entries: &[InstanceEntry]) -> String {
    entries
        .iter()
        .map(|e| e.pid.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

// ───────────────────────── テスト ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::unique_temp_dir;

    fn stale_entry(pid: u32) -> InstanceEntry {
        InstanceEntry {
            pid,
            start_signature: 0,
            exe: String::new(),
            version: "0.0.0".into(),
            workspace_roots: vec!["/gone".into()],
            launched_epoch: 0,
        }
    }

    // ── レジストリの往復と掃除 ──

    #[test]
    fn registry_roundtrip_prunes_stale_and_keeps_own() {
        let dir = unique_temp_dir("zaivern-instances-test", "roundtrip");
        // クラッシュ残骸を模す: まず生きていない巨大 PID (Linux の pid_max 超)
        let stale = stale_entry(99_999_999);
        std::fs::write(
            dir.join("99999999.json"),
            serde_json::to_string(&stale).unwrap(),
        )
        .unwrap();
        // 壊れた JSON も残骸として掃除される
        std::fs::write(dir.join("777.json"), "not json at all").unwrap();
        // *.json 以外は消さない
        std::fs::write(dir.join("README.txt"), "keep me").unwrap();

        let guard = register_in(&dir, &[PathBuf::from("/ws")]).expect("register");
        let entries = scan_and_prune(&dir);
        assert_eq!(
            entries.len(),
            1,
            "自分のエントリだけが生き残る: {entries:?}"
        );
        assert_eq!(entries[0].pid, std::process::id());
        assert_eq!(entries[0].version, env!("CARGO_PKG_VERSION"));
        assert_eq!(entries[0].workspace_roots, vec!["/ws".to_string()]);
        assert!(entries[0].launched_epoch > 0);
        assert!(!dir.join("99999999.json").exists(), "死んだ PID は削除");
        assert!(!dir.join("777.json").exists(), "壊れた JSON は削除");
        assert!(dir.join("README.txt").exists(), "無関係ファイルは残す");

        // Drop ガードで自分のファイルも消える (panic 巻き戻しの後始末と同経路)
        let own = dir.join(format!("{}.json", std::process::id()));
        assert!(own.exists());
        drop(guard);
        assert!(!own.exists(), "Drop で自分のエントリが消える");
    }

    #[test]
    fn exited_child_pid_is_pruned() {
        let dir = unique_temp_dir("zaivern-instances-test", "exited-child");
        // 実際に起動して終了した子の PID = 「死んでいる実在した PID」
        #[cfg(windows)]
        let mut child = crate::procx::hidden_command("cmd")
            .args(["/C", "exit 0"])
            .spawn()
            .expect("spawn cmd");
        #[cfg(unix)]
        let mut child = std::process::Command::new("true")
            .spawn()
            .expect("spawn true");
        let pid = child.id();
        child.wait().expect("wait child");

        std::fs::write(
            dir.join(format!("{pid}.json")),
            serde_json::to_string(&stale_entry(pid)).unwrap(),
        )
        .unwrap();
        let entries = scan_and_prune(&dir);
        assert!(entries.is_empty(), "終了済みの子のエントリは掃除される");
        assert!(!dir.join(format!("{pid}.json")).exists());
    }

    // ── 生存確認 ──

    #[test]
    fn liveness_tracks_a_real_child() {
        // OS 標準のスリーパーで「確かに生きている子」を作る
        // (terminal.rs のクロスプラットフォームスリーパーと同じ流儀)。
        #[cfg(windows)]
        let mut child = crate::procx::hidden_command("ping")
            .args(["-n", "30", "127.0.0.1"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .expect("spawn ping");
        #[cfg(unix)]
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert!(pid_alive(pid), "起動直後の子は生きている");
        child.kill().ok();
        child.wait().expect("wait child"); // wait でゾンビを回収してから判定する
        assert!(!pid_alive(pid), "回収済みの子は死んでいる");
    }

    #[test]
    fn pid_zero_and_own_pid() {
        assert!(!pid_alive(0));
        assert!(pid_alive(std::process::id()), "自プロセスは生きている");
    }

    #[test]
    fn own_entry_passes_signature_check() {
        let e = InstanceEntry::current(&[PathBuf::from("/ws")]);
        assert!(entry_alive(&e), "自分自身のエントリは生存判定を通る");
    }

    // ── OS 依存の規則 (macOS / Linux のホストからでも固定する) ──
    //
    // `#[cfg(windows)]` の中に規則を埋めると開発機では 1 行も実行されない。
    // keybinds::canonical_mods_on と同じく引数化してあるので、ここは
    // **どの OS で走らせても同じ結果**になる。

    #[test]
    fn windows生存判定はwin32の戻り値だけで決まる() {
        use osrule::{
            Probe, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
        };
        for (probe, want, why) in [
            (Probe::Waited(WAIT_TIMEOUT), true, "未シグナル = 実行中"),
            (
                Probe::Waited(WAIT_OBJECT_0),
                false,
                "シグナル済み = 終了している",
            ),
            // WAIT_FAILED。開けた以上ハンドルは実在するので生存側へ倒す。
            (
                Probe::Waited(0xFFFF_FFFF),
                true,
                "想定外の戻りは奪わない側へ",
            ),
            (
                Probe::OpenFailed(ERROR_ACCESS_DENIED),
                true,
                "権限が無いだけ (unix の EPERM 相当) = 居る",
            ),
            (
                Probe::OpenFailed(ERROR_INVALID_PARAMETER),
                false,
                "そんな PID は居ない",
            ),
            (Probe::OpenFailed(0), false, "理由不明の失敗は死亡扱い"),
        ] {
            assert_eq!(osrule::alive_from(probe), want, "{why}: {probe:?}");
        }
    }

    #[test]
    fn tasklistのcsv解析はロケールに依存しない() {
        // 実際の `tasklist /FO CSV /NH` の出力 (CRLF・カンマ入りメモリ列)。
        let csv = "\"zai.exe\",\"4242\",\"Console\",\"1\",\"62,912 K\"\r\n";
        assert_eq!(
            osrule::tasklist_image_of(csv, 4242).as_deref(),
            Some("zai.exe")
        );
        assert_eq!(
            osrule::tasklist_image_of(csv, 4243),
            None,
            "PID 違いは拾わない"
        );

        // 該当なしの 1 行。**文面はロケールで変わる**ので、どちらでも None。
        for info in [
            "INFO: No tasks are running which match the specified criteria.\r\n",
            "情報: 指定の検索条件に一致するタスクは実行されていません。\r\n",
            "",
        ] {
            assert_eq!(osrule::tasklist_image_of(info, 4242), None, "{info:?}");
        }

        // 複数行から正しい 1 行を選ぶ。
        let many = "\"a.exe\",\"1\",\"Services\",\"0\",\"1 K\"\r\n\
                    \"zai.exe\",\"22\",\"Console\",\"1\",\"2 K\"\r\n";
        assert_eq!(
            osrule::tasklist_image_of(many, 22).as_deref(),
            Some("zai.exe")
        );
    }

    #[test]
    fn イメージ名の照合は大小非区別かつ空なら通す() {
        assert!(osrule::image_matches("ZAI.EXE", "zai.exe"));
        assert!(osrule::image_matches("zai.exe", "Zai.Exe"));
        assert!(!osrule::image_matches("cmd.exe", "zai.exe"));
        assert!(
            osrule::image_matches("なんでも", ""),
            "照合材料が無ければ通す"
        );
    }

    #[test]
    fn basenameはosの区切り規則を引数で受ける() {
        // (入力, win_sep, 期待)
        for (raw, win_sep, want) in [
            (r"C:\Program Files\Zaivern\zai.exe", true, "zai.exe"),
            // unix 規則では `\` は**ただの文字**。ここを Path::file_name に
            // 任せると、macOS のテストが Windows の規則を検査できない。
            (
                r"C:\Program Files\Zaivern\zai.exe",
                false,
                r"C:\Program Files\Zaivern\zai.exe",
            ),
            ("/usr/local/bin/zai", false, "zai"),
            ("/usr/local/bin/zai", true, "zai"),
            // Windows は `/` も区切りとして受け付ける (混在も実在する)。
            (r"C:/src\zai.exe", true, "zai.exe"),
            (r"C:\src/zai.exe", true, "zai.exe"),
            ("zai.exe", true, "zai.exe"),
            ("", true, ""),
            // 末尾が区切り = basename 無し。呼び出し側は空を「照合しない」と扱う。
            ("/a/b/", false, ""),
        ] {
            assert_eq!(
                osrule::basename_on(raw, win_sep),
                want,
                "raw={raw:?} win_sep={win_sep}"
            );
        }
    }

    #[test]
    fn expected_image_nameは登録済みexeのbasenameを優先する() {
        let mk = |exe: &str| InstanceEntry {
            pid: 1,
            start_signature: 0,
            exe: exe.into(),
            version: "1.0.0".into(),
            workspace_roots: vec![],
            launched_epoch: 1,
        };
        // ホスト OS の規則で畳まれる (Windows では `\` も区切り)。
        assert_eq!(expected_image_name(&mk("/opt/zaivern/zai")), "zai");
        assert_eq!(
            expected_image_name(&mk(r"C:\Program Files\Zaivern\zai.exe")),
            if cfg!(windows) {
                "zai.exe".to_string()
            } else {
                r"C:\Program Files\Zaivern\zai.exe".to_string()
            },
            "win_sep={} — 規則そのものは basename_on のテーブルが固定する",
            cfg!(windows)
        );
        // exe が空なら自分自身の名前へ落ちる (照合を諦めない)。
        assert!(
            !expected_image_name(&mk("")).is_empty(),
            "空の exe は current_exe の名前で代替する"
        );
    }

    // ── 表示 (純粋関数) のテーブルテスト ──

    #[test]
    fn humanize_uptime_buckets() {
        for (secs, want) in [
            (0, "0秒"),
            (42, "42秒"),
            (60, "1分0秒"),
            (303, "5分3秒"),
            (3600, "1時間0分"),
            (3661, "1時間1分"),
            (9000, "2時間30分"),
            (86_400, "1日0時間"),
            (93_600 + 7200, "1日4時間"),
        ] {
            assert_eq!(humanize_uptime(secs), want, "secs={secs}");
        }
    }

    #[test]
    fn render_table_lists_all_columns() {
        let e = InstanceEntry {
            pid: 4242,
            start_signature: 0,
            exe: "/opt/zai".into(),
            version: "9.9.9".into(),
            workspace_roots: vec!["/a".into(), "/b".into()],
            launched_epoch: 1000,
        };
        let t = render_table(std::slice::from_ref(&e), 1000 + 3661);
        assert!(t.contains("PID"), "ヘッダ行がある: {t}");
        assert!(t.contains("4242"));
        assert!(t.contains("9.9.9"));
        assert!(t.contains("1時間1分"));
        assert!(t.contains("/a, /b"));
    }

    #[test]
    fn render_table_empty_is_japanese_message() {
        let t = render_table(&[], 0);
        assert!(t.contains("実行中の Zaivern Code はありません"));
    }

    #[test]
    fn render_json_roundtrips() {
        let entries = vec![
            InstanceEntry {
                pid: 1,
                start_signature: 7,
                exe: "/x/zai".into(),
                version: "1.0.0".into(),
                workspace_roots: vec!["/w".into()],
                launched_epoch: 123,
            },
            InstanceEntry::current(&[PathBuf::from("/ws")]),
        ];
        let json = render_json(&entries);
        let back: Vec<InstanceEntry> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, entries);
    }

    #[test]
    fn render_json_empty_is_array() {
        assert_eq!(render_json(&[]), "[]");
    }

    #[test]
    fn render_pids_is_one_per_line() {
        let mk = |pid| InstanceEntry {
            pid,
            start_signature: 0,
            exe: String::new(),
            version: "1.0.0".into(),
            workspace_roots: vec![],
            launched_epoch: 1,
        };
        assert_eq!(render_pids(&[mk(11)]), "11");
        assert_eq!(render_pids(&[mk(11), mk(2222), mk(3)]), "11\n2222\n3");
        // 末尾に改行を足さない = 呼び出し側の println! でちょうど 1 行 1 PID。
        assert!(!render_pids(&[mk(11)]).ends_with('\n'));
    }

    #[test]
    fn render_pids_empty_is_empty_string() {
        // 空行を出すと `xargs kill` が引数無しで走ってしまうので、空は空のまま。
        assert_eq!(render_pids(&[]), "");
    }

    #[test]
    fn set_process_name_does_not_crash() {
        // Linux では comm が変わり、他 OS では no-op。どこでも安全に呼べること。
        set_process_name();
        #[cfg(target_os = "linux")]
        {
            // prctl は「呼び出したスレッド」の comm を変える。テストスレッドから
            // 呼ぶので thread-self を読む (main から呼べばプロセス名になる)。
            let comm = std::fs::read_to_string("/proc/thread-self/comm").unwrap_or_default();
            assert_eq!(comm.trim(), "zaivern-code");
        }
    }
}
