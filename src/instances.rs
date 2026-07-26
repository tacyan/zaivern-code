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
/// Windows: tasklist の CSV 出力に該当 PID の行があるか。
#[cfg_attr(windows, allow(dead_code))] // Windows の entry_alive は tasklist_image を直接使う
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
        tasklist_image(pid).is_some()
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
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
        let Some(image) = tasklist_image(e.pid) else {
            return false;
        };
        let expect = expected_image_name(e);
        if expect.is_empty() {
            return true;
        }
        image.eq_ignore_ascii_case(&expect)
    }
    #[cfg(not(any(unix, windows)))]
    {
        false
    }
}

/// 照合に使う実行ファイル名。登録された exe の basename を第一候補、
/// 無ければ自分自身の exe 名。どちらも取れなければ空 (= 照合をスキップ)。
#[cfg(any(windows, target_os = "linux"))]
fn expected_image_name(e: &InstanceEntry) -> String {
    let base = Path::new(&e.exe)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
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
/// 該当行が無い (「タスクはありません」等) なら None = 死んでいる。
#[cfg(windows)]
fn tasklist_image(pid: u32) -> Option<String> {
    let out = crate::procx::hidden_command("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let want = pid.to_string();
    for line in text.lines() {
        let line = line.trim();
        // CSV 行は `"image","pid","session",...` の形。INFO 行は " で始まらない。
        if !line.starts_with('"') {
            continue;
        }
        let mut cols = line.trim_start_matches('"').split("\",\"");
        let image = cols.next()?.to_string();
        if cols.next().map(|p| p.trim_matches('"') == want) == Some(true) {
            return Some(image);
        }
    }
    None
}

// ───────────────────────── プロセス名 ─────────────────────────

/// ps/top で見つけやすいプロセス名を設定する。
///
/// Linux のみ prctl(PR_SET_NAME) で comm を "zaivern-code" にする
/// (comm は 15 文字上限、12 文字なので収まる)。macOS / Windows では
/// 実行ファイル名がそのままアクティビティモニタ / タスクマネージャーに
/// 出るため何もしない (レジストリ + `zai status` が検知経路)。
pub fn set_process_name() {
    #[cfg(target_os = "linux")]
    {
        if let Ok(name) = std::ffi::CString::new("zaivern-code") {
            unsafe {
                libc::prctl(libc::PR_SET_NAME, name.as_ptr());
            }
        }
    }
}

// ───────────────────────── 表示 (純粋関数) ─────────────────────────

/// 稼働時間を日本語で人間向けに (例: "42秒" / "5分3秒" / "2時間30分" / "3日2時間")。
pub fn humanize_uptime(secs: u64) -> String {
    let (d, h, m, s) = (secs / 86_400, (secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    if d > 0 {
        format!("{d}日{h}時間")
    } else if h > 0 {
        format!("{h}時間{m}分")
    } else if m > 0 {
        format!("{m}分{s}秒")
    } else {
        format!("{s}秒")
    }
}

/// `zai status` 用のテーブル文字列を作る。副作用なし (テーブルテスト用)。
pub fn render_table(entries: &[InstanceEntry], now_epoch: u64) -> String {
    if entries.is_empty() {
        return "実行中の Zaivern Code はありません。".to_string();
    }
    let mut rows: Vec<[String; 4]> =
        vec![["PID", "バージョン", "稼働時間", "ワークスペース"].map(String::from)];
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
        assert_eq!(entries.len(), 1, "自分のエントリだけが生き残る: {entries:?}");
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
        let mut child = std::process::Command::new("true").spawn().expect("spawn true");
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
