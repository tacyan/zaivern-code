//! Windows ファイアウォールの受信許可 — 📱 スマホリモートが繋がるための前提。
//!
//! # なぜ必要か
//!
//! スマホリモートは `0.0.0.0:8899` で待ち受ける「受信サーバ」である。
//! macOS / Linux は初回に許可ダイアログが出る (あるいは既定で通る) が、
//! **Windows は既定の受信動作がブロック**で、規則が無い限り LAN からの
//! 接続は握られたまま落ちる。つまり PC 側では
//!
//! - `netstat` にはちゃんと LISTEN が出る
//! - `127.0.0.1` からは開ける
//! - QR の URL も正しい
//!
//! のに、**スマホからだけ何も起きない**。原因が画面のどこにも出ないので、
//! 「Windows ではスマホから操作できない」という結論になっていた。
//!
//! そこで:
//!
//! 1. 📱 ウィンドウを開いたときに受信許可の有無を調べる ([`FirewallUi::ensure_checked`])
//! 2. 無ければ理由を明示し、ボタン 1 つで受信規則を作る (UAC の確認は出る)
//! 3. 取り消しも同じ画面から ([`FirewallUi::revoke`])
//!
//! # 実装メモ
//!
//! - 追加クレートは使わない。`netsh` ではなく PowerShell の `*-NetFirewallRule`
//!   を使う: netsh の出力はコンソールのコードページで返り、規則名や
//!   「規則が見つかりません」の文字列照合が環境依存になる。cmdlet ならオブジェクトを
//!   自分の書式 (`ZVFW …` の 1 行) に落とせるので、日本語 Windows でも同じに読める。
//! - 規則の作成には管理者権限が必要なので、**そこだけ** `Start-Process -Verb RunAs`
//!   で昇格する (すでに管理者なら昇格せずそのまま実行する)。
//!   昇格する側のスクリプトは `%LOCALAPPDATA%\Zaivern\` に置く —
//!   ユーザー専用の ACL が掛かる場所で、他ユーザーに差し替えられない。
//! - 規則は「この実行ファイル + TCP 8899-8919」に絞る。ポート範囲は
//!   [`crate::remote`] のフォールバック範囲と一致させること。
//! - プロファイルは Private/Domain を基本に、**いま繋いでいるネットワークが
//!   パブリック扱いのときだけ** Public を足す (家の Wi-Fi が「パブリック」に
//!   分類されているのはごく普通で、Private だけの規則では直らない)。
//!   その事実は UI にそのまま出して、取り消しボタンも必ず添える。

#[cfg(windows)]
use std::path::PathBuf;
use std::sync::mpsc;

/// 作成する受信規則の表示名。
pub const RULE_NAME: &str = "Zaivern Code (Mobile Remote)";

/// 規則の説明文 (Windows のファイアウォール画面に出る)。
const RULE_DESC: &str = "Zaivern Code phone remote (LAN only, token required)";

/// 許可するポート範囲。`remote::RemoteServer::start` の探索範囲と揃える。
pub const PORT_FROM: u16 = 8899;
pub const PORT_TO: u16 = 8919;

/// 受信許可の状態。`allowed` が false の間、スマホからは繋がらない。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// この実行ファイル宛の受信許可規則があるか。
    pub allowed: bool,
    /// この実行ファイルを名指しで拒否する受信規則があるか。
    /// Windows では拒否が許可より優先されるので、残っていると許可しても通らない。
    pub blocked: bool,
    /// 許可規則が有効なプロファイル (表示用、例 "Private, Public")。
    pub profiles: String,
    /// いま接続しているネットワークの種別 ("Domain" | "Private" | "Public")。
    pub categories: Vec<String>,
}

impl Report {
    /// いま繋いでいるネットワークがパブリック扱いか。
    pub fn on_public_network(&self) -> bool {
        self.categories.iter().any(|c| c == "Public")
    }
}

// ───────────────────────── スクリプト生成 (純関数) ─────────────────────────

/// PowerShell の単一引用符文字列用エスケープ (`'` → `''`)。
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// 状態を調べるスクリプト。標準出力の最後に `ZVFW …` の 1 行を出す。
///
/// 管理者権限は要らない (規則の *参照* は一般ユーザーでもできる)。
/// 値に空白を含めない形に整形してから出すこと — 読む側は空白区切りで割る。
pub fn check_script(exe: &str) -> String {
    let exe = ps_quote(exe);
    let name = ps_quote(RULE_NAME);
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n\
         $exe = '{exe}'\n\
         $ours = @(Get-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue |\n\
         \x20 Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Allow' -and $_.Enabled -eq 'True' }})\n\
         $mine = @($ours | Where-Object {{\n\
         \x20 @($_ | Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |\n\
         \x20   Where-Object {{ $_.Program -and $_.Program -ieq $exe }}).Count -gt 0 }})\n\
         $blocked = @(Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |\n\
         \x20 Where-Object {{ $_.Program -and $_.Program -ieq $exe }} |\n\
         \x20 Get-NetFirewallRule -ErrorAction SilentlyContinue |\n\
         \x20 Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' -and $_.Enabled -eq 'True' }})\n\
         $cats = @(Get-NetConnectionProfile -ErrorAction SilentlyContinue |\n\
         \x20 ForEach-Object {{ [string]$_.NetworkCategory }} | Sort-Object -Unique)\n\
         $prof = (@($mine | ForEach-Object {{ ([string]$_.Profile) -replace '[\\s]', '' }}) -join '/')\n\
         Write-Output \"ZVFW allow=$($mine.Count) block=$($blocked.Count) profiles=$prof cats=$($cats -join ',')\"\n"
    )
}

/// 受信規則を作るスクリプト (**管理者で実行する側**)。
///
/// - 同じ実行ファイル向けの古い規則は消してから作り直す (何度押しても増えない)。
///   ただし**別の場所にある zai.exe 向けの規則は残す** — インストーラは
///   `%LOCALAPPDATA%\Zaivern\bin` と `~\.cargo\bin` の両方を更新するので、
///   どちらから起動しても許可が生きているようにする (消すと交互に許可し直す羽目になる)。
///   実体が消えたパスを指す規則だけは掃除する。
/// - この実行ファイルを名指しで拒否している受信規則も消す
///   (Windows の警告ダイアログで「キャンセル」を押すと作られる。
///   拒否は許可より優先されるので、残っていると許可しても通らない)
pub fn allow_script(exe: &str, profiles: &str) -> String {
    let exe_q = ps_quote(exe);
    let name = ps_quote(RULE_NAME);
    let desc = ps_quote(RULE_DESC);
    let profiles = sanitize_profiles(profiles);
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $exe = '{exe_q}'\n\
         Get-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue | ForEach-Object {{\n\
         \x20 $prog = ($_ | Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue).Program\n\
         \x20 if ((-not $prog) -or ($prog -ieq $exe) -or (-not (Test-Path -LiteralPath $prog))) {{\n\
         \x20   $_ | Remove-NetFirewallRule -ErrorAction SilentlyContinue }} }}\n\
         Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue |\n\
         \x20 Where-Object {{ $_.Program -and $_.Program -ieq $exe }} |\n\
         \x20 Get-NetFirewallRule -ErrorAction SilentlyContinue |\n\
         \x20 Where-Object {{ $_.Direction -eq 'Inbound' -and $_.Action -eq 'Block' }} |\n\
         \x20 Remove-NetFirewallRule -ErrorAction SilentlyContinue\n\
         New-NetFirewallRule -DisplayName '{name}' -Description '{desc}' \
         -Direction Inbound -Action Allow -Protocol TCP \
         -LocalPort '{PORT_FROM}-{PORT_TO}' -Program $exe -Profile {profiles} -Enabled True | Out-Null\n\
         exit 0\n"
    )
}

/// 受信規則を消すスクリプト (**管理者で実行する側**)。
pub fn revoke_script() -> String {
    let name = ps_quote(RULE_NAME);
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n\
         Get-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue |\n\
         \x20 Remove-NetFirewallRule -ErrorAction SilentlyContinue\n\
         exit 0\n"
    )
}

/// 管理者スクリプトを起動する側のスクリプト。
/// すでに管理者ならそのまま実行し、そうでなければ UAC で昇格して待つ。
/// 終了コードは中のスクリプトのものをそのまま返す。
pub fn elevate_script(script_path: &str) -> String {
    let p = ps_quote(script_path);
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $id = [Security.Principal.WindowsIdentity]::GetCurrent()\n\
         $admin = (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(\n\
         \x20 [Security.Principal.WindowsBuiltInRole]::Administrator)\n\
         if ($admin) {{ & powershell -NoProfile -ExecutionPolicy Bypass -File '{p}'; exit $LASTEXITCODE }}\n\
         $proc = Start-Process -FilePath 'powershell' -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File','{p}')\n\
         exit $proc.ExitCode\n"
    )
}

/// `-Profile` に渡す値を安全な形に整える (英字とカンマのみ)。
/// 想定外の入力が来ても PowerShell 側へ余計な語を混ぜない。
fn sanitize_profiles(profiles: &str) -> String {
    let out: Vec<String> = profiles
        .split(',')
        .map(|p| p.trim())
        .filter(|p| matches!(*p, "Domain" | "Private" | "Public" | "Any"))
        .map(|p| p.to_string())
        .collect();
    if out.is_empty() {
        "Domain,Private".to_string()
    } else {
        out.join(",")
    }
}

/// 作る規則のプロファイル。基本は Domain+Private、
/// **いま繋いでいるネットワークがパブリック扱いのときだけ** Public も足す。
/// (家の Wi-Fi が「パブリック」に分類されているのは珍しくなく、
///  そこで Private だけの規則を作っても症状は変わらない)
pub fn profiles_for(categories: &[String]) -> String {
    let mut v = vec!["Domain".to_string(), "Private".to_string()];
    if categories.iter().any(|c| c == "Public") {
        v.push("Public".to_string());
    }
    v.join(",")
}

/// 手作業でやりたい人向けの netsh 版 (管理者のコマンドプロンプト / PowerShell)。
pub fn manual_command(exe: &str, profiles: &str) -> String {
    let prof = sanitize_profiles(profiles).to_lowercase();
    format!(
        "netsh advfirewall firewall add rule name=\"{RULE_NAME}\" dir=in action=allow \
         protocol=TCP localport={PORT_FROM}-{PORT_TO} program=\"{exe}\" profile={prof} enable=yes"
    )
}

/// [`check_script`] の出力を読む。`ZVFW …` 行が無ければ `None`。
pub fn parse_report(out: &str) -> Option<Report> {
    let line = out.lines().rev().find(|l| l.trim_start().starts_with("ZVFW "))?;
    let mut r = Report::default();
    for tok in line.split_whitespace().skip(1) {
        let Some((k, v)) = tok.split_once('=') else {
            continue;
        };
        match k {
            "allow" => r.allowed = v.parse::<u32>().unwrap_or(0) > 0,
            "block" => r.blocked = v.parse::<u32>().unwrap_or(0) > 0,
            // 表示用: "Private/Public" → "Private, Public"
            "profiles" => {
                r.profiles = v
                    .split('/')
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            // NetworkCategory の DomainAuthenticated は規則側の Domain と同じ意味
            "cats" => {
                r.categories = v
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(|s| {
                        if s == "DomainAuthenticated" {
                            "Domain".to_string()
                        } else {
                            s.to_string()
                        }
                    })
                    .collect()
            }
            _ => {}
        }
    }
    Some(r)
}

// ───────────────────────── 実行 (Windows のみ) ─────────────────────────

/// この OS でファイアウォールの面倒を見る必要があるか。
pub fn applicable() -> bool {
    cfg!(windows)
}

/// 規則に書く実行ファイルのパス (verbatim 接頭辞は外す)。
#[cfg(windows)]
fn exe_path() -> Result<String, String> {
    let exe = std::env::current_exe()
        .map_err(|e| format!("自分の実行ファイルの場所を特定できません: {e}"))?;
    Ok(crate::pathx::canonical(&exe).to_string_lossy().to_string())
}

/// 昇格して実行するスクリプトの置き場所 (ユーザー専用 ACL の下)。
#[cfg(windows)]
fn script_path(name: &str) -> Result<PathBuf, String> {
    let dir = dirs::data_local_dir()
        .ok_or("LOCALAPPDATA が見つかりません")?
        .join("Zaivern");
    std::fs::create_dir_all(&dir).map_err(|e| format!("{} を作成できません: {e}", dir.display()))?;
    Ok(dir.join(name))
}

/// PowerShell を隠しウィンドウで実行し、標準出力を返す。
#[cfg(windows)]
fn run_ps(script: &str) -> Result<(bool, String, String), String> {
    let out = crate::procx::hidden_command("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("{}{script}", crate::textenc::PS_UTF8_PRELUDE),
        ])
        .output()
        .map_err(|e| format!("powershell を実行できません: {e}"))?;
    // PowerShell はコンソールのコードページ (日本語 Windows なら CP932) で
    // 書いてくるので、UTF-8 として読むと日本語のエラーが化ける。
    // 化けるだけでなく「キャンセル」の照合まで外れるため textenc へ通す。
    Ok((
        out.status.success(),
        crate::textenc::decode_output(&out.stdout),
        crate::textenc::decode_output(&out.stderr),
    ))
}

/// 状態を調べる (同期・数百 ms かかるのでスレッドから呼ぶ)。
#[cfg(windows)]
fn check_now() -> Result<Report, String> {
    let exe = exe_path()?;
    let (_ok, stdout, stderr) = run_ps(&check_script(&exe))?;
    parse_report(&stdout).ok_or_else(|| {
        let hint = stderr.trim();
        if hint.is_empty() {
            "ファイアウォールの状態を取得できませんでした".to_string()
        } else {
            format!("ファイアウォールの状態を取得できませんでした: {hint}")
        }
    })
}

/// 管理者スクリプトをファイルへ書いてから昇格実行する。
#[cfg(windows)]
fn run_elevated(file: &str, script: &str) -> Result<(), String> {
    let path = script_path(file)?;
    // **BOM 付き UTF-8 で書く。** Windows PowerShell 5.1 は BOM の無い .ps1 を
    // ANSI (日本語 Windows なら CP932) として読むため、実行ファイルのパスに
    // 日本語が入っている環境 (`C:\Users\たろう\…`) では規則のパスが壊れ、
    // 「許可したのにスマホから繋がらない」が再発する。
    std::fs::write(&path, crate::textenc::ps_script_bytes(script))
        .map_err(|e| format!("{} を書けません: {e}", path.display()))?;
    let outer = elevate_script(&path.to_string_lossy());
    let res = run_ps(&outer);
    // 平文のスクリプトを残さない (失敗しても消す)
    let _ = std::fs::remove_file(&path);
    let (ok, _stdout, stderr) = res?;
    if ok {
        return Ok(());
    }
    let err = stderr.trim();
    // UAC で「いいえ」を押した場合 (Start-Process が例外を投げる)
    if err.contains("canceled") || err.contains("cancelled") || err.contains("キャンセル") {
        return Err("管理者の確認がキャンセルされました".to_string());
    }
    Err(if err.is_empty() {
        "ファイアウォール設定の変更に失敗しました".to_string()
    } else {
        format!("ファイアウォール設定の変更に失敗しました: {err}")
    })
}

/// 受信を許可する (UAC の確認が出る)。成功したら許可後の状態を返す。
#[cfg(windows)]
fn allow_now(profiles: &str) -> Result<Report, String> {
    let exe = exe_path()?;
    run_elevated("firewall-allow.ps1", &allow_script(&exe, profiles))?;
    check_now()
}

/// 受信許可を取り消す (UAC の確認が出る)。
#[cfg(windows)]
fn revoke_now() -> Result<Report, String> {
    run_elevated("firewall-revoke.ps1", &revoke_script())?;
    check_now()
}

/// `zai firewall <status|allow|revoke>`。インストーラや自動化から使う。
/// 管理者で実行していれば UAC は出ない (install.ps1 の昇格経路と噛み合う)。
pub fn run(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    #[cfg(not(windows))]
    {
        let _ = sub;
        println!("この OS ではファイアウォールの設定は要りません (受信は既定で通ります)。");
        return 0;
    }
    #[cfg(windows)]
    {
        let result: Result<String, String> = match sub {
            "status" | "" => check_now().map(|r| {
                if r.allowed {
                    format!(
                        "✅ 受信を許可済み: {RULE_NAME} (TCP {PORT_FROM}-{PORT_TO} / {})",
                        if r.profiles.is_empty() {
                            "-".into()
                        } else {
                            r.profiles.clone()
                        }
                    )
                } else {
                    format!(
                        "⚠ 受信が許可されていません — スマホからは繋がりません。\n\u{3000}\
                         `zai firewall allow` で許可できます{}",
                        if r.blocked {
                            " (拒否規則も残っています)"
                        } else {
                            ""
                        }
                    )
                }
            }),
            "allow" => {
                let cats = check_now().map(|r| r.categories).unwrap_or_default();
                allow_now(&profiles_for(&cats)).map(|r| {
                    format!(
                        "✅ 受信を許可しました: TCP {PORT_FROM}-{PORT_TO} / {}",
                        if r.profiles.is_empty() {
                            profiles_for(&cats)
                        } else {
                            r.profiles.clone()
                        }
                    )
                })
            }
            "revoke" | "remove" | "uninstall" => {
                revoke_now().map(|_| format!("🗑 受信許可を取り消しました: {RULE_NAME}"))
            }
            other => Err(format!(
                "不明な firewall サブコマンドです: {other} (status / allow / revoke)"
            )),
        };
        match result {
            Ok(msg) => {
                println!("{msg}");
                0
            }
            Err(msg) => {
                eprintln!("{msg}");
                1
            }
        }
    }
}

// ───────────────────────── UI 用の状態機械 ─────────────────────────

/// 何をしている途中か。
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Busy {
    Check,
    Allow,
    Revoke,
}

/// 📱 ウィンドウが持つ状態。UI スレッドを止めないため、
/// PowerShell 実行はすべて別スレッドへ出してチャネルで受け取る
/// (UAC の確認はユーザーが応答するまで返ってこない)。
#[derive(Default)]
pub struct FirewallUi {
    report: Option<Report>,
    rx: Option<mpsc::Receiver<Result<Report, String>>>,
    busy: Option<Busy>,
    started: bool,
    /// 直前の操作のエラー (成功時は None)。
    pub error: Option<String>,
    /// 直前の操作が成功したときのメッセージ (トースト用に取り出す)。
    pub done: Option<String>,
}

impl FirewallUi {
    /// 初回だけ状態を調べる (📱 ウィンドウを開いたときに呼ぶ)。
    pub fn ensure_checked(&mut self) {
        if self.started || !applicable() {
            return;
        }
        self.started = true;
        self.spawn(Busy::Check);
    }

    /// もう一度調べ直す。
    pub fn recheck(&mut self) {
        self.spawn(Busy::Check);
    }

    /// 受信を許可する (UAC の確認が出る)。
    pub fn allow(&mut self) {
        self.spawn(Busy::Allow);
    }

    /// 受信許可を取り消す (UAC の確認が出る)。
    pub fn revoke(&mut self) {
        self.spawn(Busy::Revoke);
    }

    fn spawn(&mut self, what: Busy) {
        if self.busy.is_some() || !applicable() {
            return;
        }
        self.busy = Some(what);
        self.error = None;
        #[cfg(windows)]
        {
            let profiles = profiles_for(
                &self
                    .report
                    .as_ref()
                    .map(|r| r.categories.clone())
                    .unwrap_or_default(),
            );
            let (tx, rx) = mpsc::channel();
            self.rx = Some(rx);
            let _ = std::thread::Builder::new()
                .name("zv-firewall".into())
                .spawn(move || {
                    let r = match what {
                        Busy::Check => check_now(),
                        Busy::Allow => allow_now(&profiles),
                        Busy::Revoke => revoke_now(),
                    };
                    let _ = tx.send(r);
                });
        }
    }

    /// 毎フレーム呼ぶ。結果が届いていれば取り込む。
    /// 取り込んだら true (再描画のきっかけに使える)。
    pub fn poll(&mut self) -> bool {
        let Some(rx) = &self.rx else { return false };
        match rx.try_recv() {
            Ok(Ok(report)) => {
                let what = self.busy.take();
                self.rx = None;
                self.done = match what {
                    Some(Busy::Allow) if report.allowed => {
                        Some("🛡 ファイアウォールで受信を許可しました".to_string())
                    }
                    Some(Busy::Revoke) if !report.allowed => {
                        Some("🛡 受信許可を取り消しました".to_string())
                    }
                    _ => None,
                };
                self.report = Some(report);
                true
            }
            Ok(Err(e)) => {
                self.busy = None;
                self.rx = None;
                self.error = Some(e);
                true
            }
            Err(mpsc::TryRecvError::Empty) => false,
            Err(mpsc::TryRecvError::Disconnected) => {
                self.busy = None;
                self.rx = None;
                true
            }
        }
    }

    pub fn busy(&self) -> Option<Busy> {
        self.busy
    }

    pub fn report(&self) -> Option<&Report> {
        self.report.as_ref()
    }

    /// スマホから繋がらない状態か (= 警告を出すべきか)。
    pub fn needs_allow(&self) -> bool {
        applicable() && matches!(&self.report, Some(r) if !r.allowed || r.blocked)
    }

    /// 手作業用の netsh コマンド (コピーさせる)。
    pub fn manual(&self) -> String {
        let exe = std::env::current_exe()
            .map(|p| crate::pathx::canonical(&p).to_string_lossy().to_string())
            .unwrap_or_else(|_| "zai.exe".into());
        let cats = self
            .report
            .as_ref()
            .map(|r| r.categories.clone())
            .unwrap_or_default();
        manual_command(&exe, &profiles_for(&cats))
    }
}

// ───────────────────────── テスト ─────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_report_reads_the_summary_line() {
        let out = "何か余計な行\nZVFW allow=1 block=0 profiles=Private/Public cats=Public\n";
        let r = parse_report(out).expect("ZVFW 行を読む");
        assert!(r.allowed);
        assert!(!r.blocked);
        assert_eq!(r.profiles, "Private, Public");
        assert_eq!(r.categories, vec!["Public".to_string()]);
        assert!(r.on_public_network());
    }

    #[test]
    fn parse_report_handles_missing_rule_and_empty_fields() {
        let r = parse_report("ZVFW allow=0 block=2 profiles= cats=").expect("読める");
        assert!(!r.allowed, "規則が無ければ allowed=false");
        assert!(r.blocked, "拒否規則の存在は拾う");
        assert_eq!(r.profiles, "");
        assert!(r.categories.is_empty());
        assert!(!r.on_public_network());
    }

    #[test]
    fn parse_report_maps_domain_authenticated_to_domain() {
        let r = parse_report("ZVFW allow=1 block=0 profiles=Domain cats=DomainAuthenticated,Private")
            .expect("読める");
        assert_eq!(r.categories, vec!["Domain".to_string(), "Private".to_string()]);
    }

    #[test]
    fn parse_report_without_marker_is_none() {
        assert!(parse_report("").is_none());
        assert!(parse_report("Get-NetFirewallRule : 用語を認識できません").is_none());
    }

    #[test]
    fn profiles_add_public_only_on_a_public_network() {
        assert_eq!(profiles_for(&[]), "Domain,Private");
        assert_eq!(profiles_for(&["Private".into()]), "Domain,Private");
        assert_eq!(
            profiles_for(&["Public".into()]),
            "Domain,Private,Public",
            "パブリック扱いの Wi-Fi では Public を足さないと直らない"
        );
    }

    #[test]
    fn sanitize_profiles_drops_anything_unexpected() {
        assert_eq!(sanitize_profiles("Private,Public"), "Private,Public");
        assert_eq!(sanitize_profiles("Private; rm -rf"), "Domain,Private");
        assert_eq!(sanitize_profiles(""), "Domain,Private");
    }

    #[test]
    fn allow_script_targets_this_exe_and_the_port_range() {
        let s = allow_script(r"C:\Users\o'brien\zai.exe", "Domain,Private,Public");
        assert!(s.contains(r"$exe = 'C:\Users\o''brien\zai.exe'"), "' は '' に畳む");
        assert!(s.contains("-LocalPort '8899-8919'"));
        assert!(s.contains("-Profile Domain,Private,Public"));
        assert!(s.contains("-Direction Inbound"));
        assert!(s.contains("-Action Allow"));
        assert!(s.contains(RULE_NAME));
        // 拒否規則の掃除 (これを忘れると許可しても通らない)
        assert!(s.contains("$_.Action -eq 'Block'"));
        assert!(s.contains("Remove-NetFirewallRule"));
        // 別の場所の zai.exe 向け規則は残す (実体があるかで判断する)
        assert!(s.contains("Test-Path -LiteralPath $prog"));
    }

    /// 許可するポート範囲は、実際に待ち受ける範囲と一致していなければならない。
    /// ずれた分だけ「PC は待っているのに Windows が落とす」— 症状は元のバグと同じで、
    /// しかも片方だけ直すと再発するので、コード側で縛っておく。
    #[test]
    fn port_range_matches_the_remote_server() {
        assert_eq!(PORT_FROM, crate::remote::PORT_FROM);
        assert_eq!(PORT_TO, crate::remote::PORT_TO);
        const { assert!(PORT_FROM <= PORT_TO) };
    }

    #[test]
    fn check_script_prints_the_marker_and_quotes_the_exe() {
        let s = check_script(r"C:\a'b\zai.exe");
        assert!(s.contains(r"$exe = 'C:\a''b\zai.exe'"));
        assert!(s.contains("ZVFW allow="));
        assert!(s.contains("Get-NetConnectionProfile"), "ネットワーク種別も見る");
        // 空白入りの値を出すと読めなくなる (Profile は "Private, Public" と出る)
        assert!(s.contains(r"-replace '[\s]', ''"));
    }

    #[test]
    fn elevate_script_runs_directly_when_already_admin() {
        let s = elevate_script(r"C:\tmp\a.ps1");
        assert!(s.contains("IsInRole"));
        assert!(s.contains("if ($admin)"), "管理者なら UAC を出さない");
        assert!(s.contains("-Verb RunAs"));
        assert!(s.contains("-Wait"), "終わるまで待って結果を見る");
        assert!(s.contains(r"-File','C:\tmp\a.ps1'"));
    }

    #[test]
    fn manual_command_is_a_valid_looking_netsh_line() {
        let c = manual_command(r"C:\bin\zai.exe", "Domain,Private");
        assert!(c.starts_with("netsh advfirewall firewall add rule "));
        assert!(c.contains(r#"program="C:\bin\zai.exe""#));
        assert!(c.contains("localport=8899-8919"));
        assert!(c.contains("profile=domain,private"));
    }

    #[test]
    fn revoke_script_only_removes_our_rule() {
        let s = revoke_script();
        assert!(s.contains(RULE_NAME));
        assert!(s.contains("Remove-NetFirewallRule"));
        assert!(!s.contains("New-NetFirewallRule"));
    }

    /// Windows 以外では何も要らない = UI に警告も出さない。
    #[test]
    fn non_windows_never_asks_for_anything() {
        let mut ui = FirewallUi::default();
        ui.ensure_checked();
        if !applicable() {
            assert!(ui.report().is_none());
            assert!(!ui.needs_allow());
            assert!(ui.busy().is_none());
        }
    }
}
