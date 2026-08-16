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
//! # 「許可したのに繋がらない」
//!
//! 規則が 1 本あることと、その規則が **いまのネットワークで効いていること** は別物で、
//! 以下は規則があっても受信が落ちる。どれも画面には「許可済み ✅」と出ていたので、
//! 利用者からは *許可しても直らないバグ* に見える。[`Report::problems`] で
//! 全部拾って、原因ごとに直し方を出す:
//!
//! - **プロファイル不一致** — 規則は Domain/Private で作ったのに、いま繋いでいる
//!   Wi-Fi が「パブリック」に分類されている。Windows は規則を*適用しない*。
//!   自宅の Wi-Fi でもルータや接続順で普通に起きるうえ、一度許可した後に
//!   別の Wi-Fi へ移るだけでも再発する (規則は残るので画面は許可済みのまま)。
//! - **「すべての受信接続をブロックする」** — Windows セキュリティのこのチェックが
//!   入っていると、**許可規則ごと無視される**。何本規則を作っても直らない。
//! - **拒否規則** — 既出。Windows の警告ダイアログで「キャンセル」を押すと作られる。
//! - **別のファイアウォール製品** — ノートン / ESET / マカフィー等を入れていると、
//!   受信を落としているのは **Windows ではなくそちら**。Windows 側の規則を
//!   何本作っても、その製品で許可しない限りスマホからは繋がらない。
//!   ここは Windows の規則をいくら読んでも分からないので、
//!   `root/SecurityCenter2` に登録された製品名を出して、そちらへ誘導する
//!   ([`Report::other_fw`])。
//!
//! 逆にファイアウォール自体が切られているときは何も落とされないので、警告も出さない。
//!
//! # 「届いているのかどうかも分からない」
//!
//! 規則を読んで分かるのは *建前* だけで、**実際にパケットが来たか**は別物である
//! (ルータのクライアント分離、スマホが別セグメント/モバイル回線、といった
//! ファイアウォールの外の理由でも同じ「真っ白」になる)。
//! そこで [`crate::remote::Reach`] が accept した相手アドレスを記録し、
//! 📱 の画面へ「まだ届いていない / いつ・どこから届いた」を出す。
//! これで *Windows で止まっているのか、そもそも来ていないのか* を切り分けられる。
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

use crate::i18n::tr;
// `trf` が要るのは Windows 専用の経路だけ。素の `use` にすると
// 非 Windows の `cargo check --bin zai` で unused import になる。
#[cfg(any(windows, test))]
use crate::i18n::trf;

/// 作成する受信規則の表示名。
pub const RULE_NAME: &str = "Zaivern Code (Mobile Remote)";

/// 規則の説明文 (Windows のファイアウォール画面に出る)。
#[cfg(any(windows, test))]
const RULE_DESC: &str = "Zaivern Code phone remote (LAN only, token required)";

/// 許可するポート範囲。`remote::RemoteServer::start` の探索範囲と揃える。
pub const PORT_FROM: u16 = 8899;
pub const PORT_TO: u16 = 8919;

/// 受信許可の状態。[`Report::problems`] が空でない間、スマホからは繋がらない。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// この実行ファイル宛の受信許可規則があるか。
    pub allowed: bool,
    /// この実行ファイルを名指しで拒否する受信規則があるか。
    /// Windows では拒否が許可より優先されるので、残っていると許可しても通らない。
    pub blocked: bool,
    /// 許可規則が有効なプロファイル (表示用、例 "Private, Public")。
    pub profiles: String,
    /// 同上 (判定用に分解したもの、例 ["Domain", "Private"])。
    /// `Any` は「全プロファイル」— Windows は 3 つ揃うとこう畳む。
    pub rule_profiles: Vec<String>,
    /// いま接続しているネットワークの種別 ("Domain" | "Private" | "Public")。
    pub categories: Vec<String>,
    /// いま接続しているネットワークの数 (種別を取れなかったときは 0)。
    pub networks: u32,
    /// そのうちファイアウォールが有効なものの数。
    pub enforcing: u32,
    /// そのうち「すべての受信接続をブロックする」が入っているものの数。
    pub strict: u32,
    /// Windows とは別に動いているファイアウォール製品の名前 (例 ["ノートン 360"])。
    /// ここが空でない限り、Windows 側で許可してもそちらが落としていれば繋がらない。
    pub other_fw: Vec<String>,
}

/// 「スマホから繋がらない」の原因。1 つとは限らないので [`Report::problems`] は列で返す。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// 「すべての受信接続をブロックする」が有効 = 許可規則ごと無視される。
    /// これが立っている間は規則を何本作っても直らないので、先頭に出す。
    StrictInbound,
    /// この実行ファイルを名指しで拒否する規則が残っている (許可より優先される)。
    DenyRule,
    /// 受信許可の規則が無い。
    NoRule,
    /// 規則はあるが、いま繋いでいるネットワークの種別に適用されない。
    ProfileMismatch,
    /// Windows とは別のファイアウォール製品が動いている。
    /// **Windows 側の許可では直らない**ので、規則の問題より後ろに出す。
    OtherFirewall,
}

impl Problem {
    /// 画面と CLI に出す 1 行の見出し。
    ///
    /// **ここでは訳さない。** `&'static str` のまま返し、画面へ出す側が
    /// `tr()` を通す (`app/remote_api.rs` が既にその形で呼んでいる)。
    pub fn headline(&self) -> &'static str {
        match self {
            Problem::StrictInbound => {
                "⚠ Windows が「すべての受信接続をブロックする」設定になっています"
            }
            Problem::DenyRule => "⚠ この実行ファイルを拒否する規則が残っています",
            Problem::NoRule => "⚠ Windows のファイアウォールが受信をブロックしています",
            Problem::ProfileMismatch => {
                "⚠ 許可規則が、いま繋いでいるネットワークに適用されていません"
            }
            Problem::OtherFirewall => "⚠ Windows とは別のファイアウォールが動いています",
        }
    }

    /// なぜ繋がらないのか / どう直すのか。
    pub fn detail(&self) -> &'static str {
        match self {
            // これが本命の「許可しても直らない」。規則の話をしても解決しないので、
            // 規則ではなく設定を外す方へ誘導する。
            Problem::StrictInbound => {
                "この設定が入っている間、受信の許可規則は\u{3000}すべて無視されます\u{3000}—\n\
                 規則を作り直しても症状は変わりません。下のボタン (または Windows セキュリティ →\n\
                 ファイアウォールとネットワーク保護 → 使用中のネットワーク) で解除してください。"
            }
            Problem::DenyRule => {
                "拒否は許可より優先されるため、許可規則があっても落とされます\n\
                 (Windows の警告ダイアログで「キャンセル」を押すと作られます)。\n\
                 「受信を許可する」で削除してから作り直します。"
            }
            Problem::NoRule => {
                "PC は待ち受けていますが、スマホからの接続は Windows 側で\n\
                 落とされます (QR を読んでも真っ白 / 繋がらない)。"
            }
            // 一度許可した後に別の Wi-Fi へ移るだけで再発する。規則は残るので
            // 画面は「許可済み」に見えていた = 利用者には「許可しても直らない」。
            Problem::ProfileMismatch => {
                "規則は残っていますが、いま繋いでいるネットワークの種別が\n\
                 規則のプロファイルと違うため、Windows は規則を適用しません\n\
                 (別の Wi-Fi へ移ったときに起きます)。\n\
                 「受信を許可する」で、いまのネットワークに合わせて作り直せます。"
            }
            // 本命の「Windows で許可したのに繋がらない」。受信を落としているのが
            // Windows ではないので、こちらのボタンでは絶対に直らない。
            // 製品名は Report 側が持っているので、UI/CLI がこの文の前に出す。
            Problem::OtherFirewall => {
                "受信を落としているのが Windows とは限りません。\n\
                 この製品の側でも zai.exe の受信を許可してください\n\
                 (設定 → ファイアウォール → プログラム制御 などにあります)。\n\
                 Windows 側の「受信を許可する」では直りません。"
            }
        }
    }
}

impl Report {
    /// いま繋いでいるネットワークがパブリック扱いか。
    pub fn on_public_network(&self) -> bool {
        self.categories.iter().any(|c| c == "Public")
    }

    /// Windows がいまのネットワークで受信を検査しているか。
    /// 状態を取れなかったときは「検査している」と見なす (安全側 = 案内を出す)。
    pub fn enforcing_now(&self) -> bool {
        self.networks == 0 || self.enforcing > 0
    }

    /// 許可規則が、**いま繋いでいるネットワークの種別に適用されるか**。
    ///
    /// ここが本体。規則が 1 本あることと効いていることは別で、
    /// Private で作った規則はパブリック扱いの Wi-Fi では素通りされる
    /// (画面は「許可済み」のまま、スマホからだけ繋がらない)。
    /// 判定材料が無いとき (種別もプロファイルも読めない) は騒がない。
    pub fn covers_current_network(&self) -> bool {
        if !self.allowed {
            return false;
        }
        if self.categories.is_empty() || self.rule_profiles.is_empty() {
            return true;
        }
        if self
            .rule_profiles
            .iter()
            .any(|p| p.eq_ignore_ascii_case("Any"))
        {
            return true;
        }
        // 有線と無線で種別が違うことがある。スマホがどちら側かは分からないので、
        // **どれか 1 つでも外れていれば** 不一致として扱う。
        self.categories
            .iter()
            .all(|c| self.rule_profiles.iter().any(|p| p.eq_ignore_ascii_case(c)))
    }

    /// Windows とは別のファイアウォール製品が動いているか。
    pub fn has_other_firewall(&self) -> bool {
        !self.other_fw.is_empty()
    }

    /// その製品名 (表示用、例 "ノートン 360")。複数なら並べる。
    pub fn other_firewall_label(&self) -> String {
        self.other_fw.join(" / ")
    }

    /// スマホから繋がらない原因を、直すべき順に並べて返す。空なら繋がるはず。
    pub fn problems(&self) -> Vec<Problem> {
        let mut v = Vec::new();
        // Windows が受信を検査しているときだけ、Windows 側の原因を並べる。
        // 切られていれば Windows は何も落とさないので、規則の話は嘘になる。
        if self.enforcing_now() {
            if self.strict > 0 {
                v.push(Problem::StrictInbound);
            }
            if self.blocked {
                v.push(Problem::DenyRule);
            }
            if !self.allowed {
                v.push(Problem::NoRule);
            } else if !self.covers_current_network() {
                v.push(Problem::ProfileMismatch);
            }
        }
        // 別製品は最後 — Windows 側は 1 クリックで直せるので先に片付けさせる。
        // **Windows が無効でもここは出す**: 「ファイアウォールが無効だから
        // 問題なし」と言い切ると、実際に落としている製品を見逃して
        // 「許可したのに繋がらない」に戻る。
        if self.has_other_firewall() {
            v.push(Problem::OtherFirewall);
        }
        v
    }

    /// 受信規則を作り直せば直る原因があるか (= 「許可する」ボタンを出すか)。
    pub fn fixable_by_allow(&self) -> bool {
        self.problems().iter().any(|p| {
            matches!(
                p,
                Problem::DenyRule | Problem::NoRule | Problem::ProfileMismatch
            )
        })
    }

    /// いま繋いでいるネットワークの種別 (表示用の日本語、例 "プライベート, パブリック")。
    /// 読めなかったときは空文字。
    pub fn network_label(&self) -> String {
        self.categories
            .iter()
            // 左辺 ("Public" 等) は PowerShell が返す**識別子**なので訳さない
            // (照合に使う値であって画面の文字列ではない)。右辺だけが画面に出る。
            .map(|c| match c.as_str() {
                "Public" => tr("パブリック"),
                "Private" => tr("プライベート"),
                "Domain" => tr("ドメイン"),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

// ───────────────────────── スクリプト生成 (純関数) ─────────────────────────

/// PowerShell の単一引用符文字列用エスケープ (`'` → `''`)。
// Windows 専用 — 呼び出し元は全て #[cfg(windows)] 側にある。
// テストは全 OS で走るので `test` も通す (ここを外すと macOS/Linux で dead_code になる)。
#[cfg(any(windows, test))]
fn ps_quote(s: &str) -> String {
    s.replace('\'', "''")
}

/// 状態を調べるスクリプト。標準出力の最後に `ZVFW …` の 1 行を出す。
///
/// 管理者権限は要らない (規則の *参照* は一般ユーザーでもできる)。
/// 値に空白を含めない形に整形してから出すこと — 読む側は空白区切りで割る。
///
/// 規則の有無だけでなく **いま繋いでいるネットワークのプロファイル設定** も見る:
/// `net`(接続数) / `on`(ファイアウォールが有効な数) / `strict`(「すべての受信接続を
/// ブロックする」が入っている数)。これが無いと「許可済みなのに繋がらない」の
/// 原因が画面のどこにも出ない。
#[cfg(any(windows, test))]
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
         $names = @($cats | ForEach-Object {{\n\
         \x20 if ($_ -eq 'DomainAuthenticated') {{ 'Domain' }} else {{ $_ }} }} | Sort-Object -Unique)\n\
         $live = @(Get-NetFirewallProfile -ErrorAction SilentlyContinue |\n\
         \x20 Where-Object {{ $names -contains [string]$_.Name }})\n\
         $on = @($live | Where-Object {{ ([string]$_.Enabled) -eq 'True' }})\n\
         $strict = @($on | Where-Object {{ ([string]$_.AllowInboundRules) -eq 'False' }})\n\
         $prof = (@($mine | ForEach-Object {{ ([string]$_.Profile) -replace '[\\s]', '' }}) -join '/')\n\
         $other = @(Get-CimInstance -Namespace root/SecurityCenter2 -ClassName FirewallProduct \
         -ErrorAction SilentlyContinue |\n\
         \x20 Where-Object {{ (((([int]$_.productState) -shr 8)) -band 0xFF) -ne 0 }} |\n\
         \x20 ForEach-Object {{ ([string]$_.displayName).Trim() }} |\n\
         \x20 Where-Object {{ $_ -and ($_ -notmatch 'Windows') }} | Sort-Object -Unique)\n\
         foreach ($o in $other) {{ Write-Output \"ZVFWX $o\" }}\n\
         Write-Output \"ZVFW allow=$($mine.Count) block=$($blocked.Count) profiles=$prof cats=$($cats -join ',')\
         \x20net=$($live.Count) on=$($on.Count) strict=$($strict.Count) other=$($other.Count)\"\n"
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
#[cfg(any(windows, test))]
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
#[cfg(any(windows, test))]
pub fn revoke_script() -> String {
    let name = ps_quote(RULE_NAME);
    format!(
        "$ErrorActionPreference = 'SilentlyContinue'\n\
         Get-NetFirewallRule -DisplayName '{name}' -ErrorAction SilentlyContinue |\n\
         \x20 Remove-NetFirewallRule -ErrorAction SilentlyContinue\n\
         exit 0\n"
    )
}

/// 「すべての受信接続をブロックする」を解除するスクリプト (**管理者で実行する側**)。
///
/// このチェックが入っている間は **許可規則ごと無視される** ので、
/// 規則を作り直しても症状は変わらない。触るのは
/// **いま繋いでいるネットワークのプロファイルだけ** — 使っていない
/// プロファイル (例: 外出先の Public) の設定まで緩めない。
#[cfg(any(windows, test))]
pub fn unblock_script() -> String {
    "$ErrorActionPreference = 'Stop'\n\
     $names = @(Get-NetConnectionProfile -ErrorAction SilentlyContinue | ForEach-Object {\n\
     \x20 if ([string]$_.NetworkCategory -eq 'DomainAuthenticated') { 'Domain' }\n\
     \x20 else { [string]$_.NetworkCategory } } | Sort-Object -Unique)\n\
     if (-not $names) { throw '接続中のネットワークが見つかりません' }\n\
     foreach ($n in $names) { Set-NetFirewallProfile -Name $n -AllowInboundRules True }\n\
     exit 0\n"
        .to_string()
}

/// 管理者スクリプトを起動する側のスクリプト。
/// すでに管理者ならそのまま実行し、そうでなければ UAC で昇格して待つ。
/// 終了コードは中のスクリプトのものをそのまま返す。
///
/// UAC で「いいえ」を押すと `Start-Process` が例外を投げる。その文言は
/// 言語ごとに違う (日本語なら「操作は…取り消されました」) ので、
/// **文字列照合ではなく終了コード** [`CANCELLED`] で伝える。
#[cfg(any(windows, test))]
pub fn elevate_script(script_path: &str) -> String {
    let p = ps_quote(script_path);
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $id = [Security.Principal.WindowsIdentity]::GetCurrent()\n\
         $admin = (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(\n\
         \x20 [Security.Principal.WindowsBuiltInRole]::Administrator)\n\
         if ($admin) {{ & powershell -NoProfile -ExecutionPolicy Bypass -File '{p}'; exit $LASTEXITCODE }}\n\
         try {{\n\
         \x20 $proc = Start-Process -FilePath 'powershell' -Verb RunAs -Wait -PassThru -WindowStyle Hidden \
         -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File','{p}')\n\
         }} catch {{ exit {CANCELLED} }}\n\
         exit $proc.ExitCode\n"
    )
}

/// UAC の確認を断ったときの終了コード (Windows の `ERROR_CANCELLED`)。
#[cfg(any(windows, test))]
pub const CANCELLED: i32 = 1223;

/// 管理者スクリプトを `try/catch` で包み、失敗した理由をログへ落とす。
///
/// 昇格した側の標準エラーはこちらへ返ってこない。包まずに実行すると
/// 「ファイアウォール設定の変更に失敗しました」しか出せず、
/// 直しようが無くなる (元のバグと同じ「原因が画面に出ない」形)。
#[cfg(any(windows, test))]
pub fn with_error_log(script: &str, log_path: &str) -> String {
    let log = ps_quote(log_path);
    format!(
        "$ZvLog = '{log}'\n\
         try {{\n{script}}} catch {{\n\
         \x20 try {{ ($_ | Out-String) | Set-Content -LiteralPath $ZvLog -Encoding UTF8 }} catch {{}}\n\
         \x20 exit 1\n\
         }}\n\
         exit 0\n"
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

/// 作る規則のプロファイル — **3 つすべて**。
///
/// 以前は「いま繋いでいる種別」に合わせて絞っていたが、それだと
/// *許可した直後は繋がるのに、しばらくすると繋がらなくなる* が定期的に再発する:
///
/// - Windows は Wi-Fi をゲートウェイに届かないと「識別されていないネットワーク」
///   = **パブリック**に再分類する。同じ Wi-Fi でも再接続のたびに変わりうる。
/// - 別の Wi-Fi へ移れば当然変わる。
///
/// 規則は残るので画面は「許可済み」のまま、スマホからだけ落ちる — 利用者から見れば
/// 「許可しても直らないバグ」そのものだった。規則自体は
/// **この実行ファイル + TCP 8899-8919** に絞ってあり、接続にはトークンが要るので、
/// 3 つに広げても開くのは「Zaivern が動いている間の、この 1 本の口」だけである。
pub fn allow_profiles() -> &'static str {
    "Domain,Private,Public"
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
#[cfg(any(windows, test))]
pub fn parse_report(out: &str) -> Option<Report> {
    let line = out
        .lines()
        .rev()
        .find(|l| l.trim_start().starts_with("ZVFW "))?;
    let mut r = Report::default();
    // 製品名は空白を含む ("ノートン 360") ので、空白区切りの要約行には載せられない。
    // 1 製品 1 行の `ZVFWX <名前>` で受け取る。
    for l in out.lines() {
        if let Some(name) = l.trim().strip_prefix("ZVFWX ") {
            let name = name.trim();
            if !name.is_empty() && !r.other_fw.iter().any(|n| n == name) {
                r.other_fw.push(name.to_string());
            }
        }
    }
    let mut other = 0u32;
    for tok in line.split_whitespace().skip(1) {
        let Some((k, v)) = tok.split_once('=') else {
            continue;
        };
        match k {
            "allow" => r.allowed = v.parse::<u32>().unwrap_or(0) > 0,
            "block" => r.blocked = v.parse::<u32>().unwrap_or(0) > 0,
            "net" => r.networks = v.parse().unwrap_or(0),
            "on" => r.enforcing = v.parse().unwrap_or(0),
            "strict" => r.strict = v.parse().unwrap_or(0),
            "other" => other = v.parse().unwrap_or(0),
            // 規則が複数あれば "/" 区切り、1 本の中の複数プロファイルは "," 区切り。
            // 表示用: "Private/Public" → "Private, Public"
            "profiles" => {
                r.rule_profiles = v
                    .split(['/', ','])
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string())
                    .collect();
                r.profiles = r.rule_profiles.join(", ");
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
    // 名前が読めなくても「別の製品が居る」ことは伝える (黙ると元のバグに戻る)。
    if r.other_fw.is_empty() && other > 0 {
        r.other_fw.push(tr("別のファイアウォール製品"));
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
    let exe = std::env::current_exe().map_err(|e| {
        trf(
            "自分の実行ファイルの場所を特定できません: {e}",
            &[("e", e.to_string())],
        )
    })?;
    Ok(crate::pathx::canonical(&exe).to_string_lossy().to_string())
}

/// 昇格して実行するスクリプトの置き場所 (ユーザー専用 ACL の下)。
#[cfg(windows)]
fn script_path(name: &str) -> Result<PathBuf, String> {
    let dir = dirs::data_local_dir()
        .ok_or_else(|| tr("LOCALAPPDATA が見つかりません"))?
        .join("Zaivern");
    std::fs::create_dir_all(&dir).map_err(|e| {
        crate::i18n::fill_positional(
            &trf("{} を作成できません: {e}", &[("e", e.to_string())]),
            &[dir.display().to_string()],
        )
    })?;
    Ok(dir.join(name))
}

/// PowerShell の実行結果。**終了コードまで持つ**のが要点 —
/// UAC のキャンセルは [`CANCELLED`] で返ってきて標準エラーには何も出ない。
#[cfg(windows)]
struct PsOut {
    ok: bool,
    code: i32,
    stdout: String,
    stderr: String,
}

/// PowerShell を隠しウィンドウで実行し、標準出力を返す。
#[cfg(windows)]
fn run_ps(script: &str) -> Result<PsOut, String> {
    let out = crate::procx::hidden_command("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            &format!("{}{script}", crate::textenc::PS_UTF8_PRELUDE),
        ])
        .output()
        .map_err(|e| trf("powershell を実行できません: {e}", &[("e", e.to_string())]))?;
    // PowerShell はコンソールのコードページ (日本語 Windows なら CP932) で
    // 書いてくるので、UTF-8 として読むと日本語のエラーが化ける。
    // 化けるだけでなく「キャンセル」の照合まで外れるため textenc へ通す。
    Ok(PsOut {
        ok: out.status.success(),
        code: out.status.code().unwrap_or(-1),
        stdout: crate::textenc::decode_output(&out.stdout),
        stderr: crate::textenc::decode_output(&out.stderr),
    })
}

/// 状態を調べる (同期・数百 ms かかるのでスレッドから呼ぶ)。
#[cfg(windows)]
fn check_now() -> Result<Report, String> {
    let exe = exe_path()?;
    let out = run_ps(&check_script(&exe))?;
    parse_report(&out.stdout).ok_or_else(|| {
        let hint = out.stderr.trim();
        if hint.is_empty() {
            tr("ファイアウォールの状態を取得できませんでした")
        } else {
            trf(
                "ファイアウォールの状態を取得できませんでした: {hint}",
                &[("hint", hint.to_string())],
            )
        }
    })
}

/// 管理者スクリプトをファイルへ書いてから昇格実行する。
///
/// 昇格した側は別プロセスなので、その標準エラーはこちらへ返ってこない。
/// 失敗の理由は [`with_error_log`] でログへ落とさせ、ここで読み直して返す
/// (これが無いと「変更に失敗しました」だけが出て、直しようが無くなる)。
#[cfg(windows)]
fn run_elevated(file: &str, script: &str) -> Result<(), String> {
    let path = script_path(file)?;
    let log = script_path("firewall-error.log")?;
    // 前回の失敗を今回の理由として読まないよう、必ず消してから走らせる。
    let _ = std::fs::remove_file(&log);
    // **BOM 付き UTF-8 で書く。** Windows PowerShell 5.1 は BOM の無い .ps1 を
    // ANSI (日本語 Windows なら CP932) として読むため、実行ファイルのパスに
    // 日本語が入っている環境 (`C:\Users\たろう\…`) では規則のパスが壊れ、
    // 「許可したのにスマホから繋がらない」が再発する。
    let wrapped = with_error_log(script, &log.to_string_lossy());
    std::fs::write(&path, crate::textenc::ps_script_bytes(&wrapped)).map_err(|e| {
        crate::i18n::fill_positional(
            &trf("{} を書けません: {e}", &[("e", e.to_string())]),
            &[path.display().to_string()],
        )
    })?;
    let outer = elevate_script(&path.to_string_lossy());
    let res = run_ps(&outer);
    // 平文のスクリプトを残さない (失敗しても消す)
    let _ = std::fs::remove_file(&path);
    let out = res?;
    let reason = std::fs::read(&log)
        .map(|b| crate::textenc::decode_output(&b).trim().to_string())
        .unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    if out.ok {
        return Ok(());
    }
    // UAC で「いいえ」を押した場合。文言は言語ごとに違うので終了コードで見る
    // (念のため昔の文字列照合も残す — 古い Windows は例外を投げずに返す)。
    // **照合side の "キャンセル" は tr() へ通さない** — Windows が返す文言
    // そのものであって、こちらが画面へ出す文字列ではない。UI 言語を韓国語に
    // したら日本語 Windows のメッセージと照合できなくなる。
    let err = out.stderr.trim();
    if out.code == CANCELLED
        || err.contains("canceled")
        || err.contains("cancelled")
        || err.contains("キャンセル")
    {
        return Err(tr("管理者の確認がキャンセルされました"));
    }
    // 昇格側のログ → こちら側の標準エラー、の順に理由を探す。
    let why = if !reason.is_empty() {
        reason.lines().next().unwrap_or(&reason).to_string()
    } else {
        err.lines().next().unwrap_or("").to_string()
    };
    Err(if why.is_empty() {
        crate::i18n::fill_positional(
            &tr("ファイアウォール設定の変更に失敗しました (終了コード {})"),
            &[out.code.to_string()],
        )
    } else {
        trf(
            "ファイアウォール設定の変更に失敗しました: {why}",
            &[("why", why)],
        )
    })
}

/// 受信を許可する (UAC の確認が出る)。成功したら許可後の状態を返す。
///
/// プロファイルは [`allow_profiles`] の 3 つ固定 — いま繋いでいる種別に
/// 合わせて絞ると、Windows がネットワークを再分類しただけで規則が効かなくなる。
#[cfg(windows)]
fn allow_now() -> Result<Report, String> {
    let exe = exe_path()?;
    run_elevated("firewall-allow.ps1", &allow_script(&exe, allow_profiles()))?;
    check_now()
}

/// 受信許可を取り消す (UAC の確認が出る)。
#[cfg(windows)]
fn revoke_now() -> Result<Report, String> {
    run_elevated("firewall-revoke.ps1", &revoke_script())?;
    check_now()
}

/// 「すべての受信接続をブロックする」を解除する (UAC の確認が出る)。
/// これが入っている間は許可規則ごと無視されるので、規則より先に外させる。
#[cfg(windows)]
fn unblock_now() -> Result<Report, String> {
    run_elevated("firewall-unblock.ps1", &unblock_script())?;
    check_now()
}

/// `zai firewall status` の出力 (純関数 — 原因ごとに直し方まで出す)。
///
/// 規則の有無だけを見て「✅ 許可済み」と出すのが元のバグだった。
/// 規則があっても [`Report::problems`] が空でなければスマホからは繋がらないので、
/// そちらを主役にする。
#[cfg(any(windows, test))]
pub fn status_text(r: Report) -> String {
    let problems = r.problems();
    if problems.is_empty() {
        let mut msg = if r.allowed {
            crate::i18n::fill_positional(
                &trf(
                    "✅ 受信を許可済み: {RULE_NAME} (TCP {PORT_FROM}-{PORT_TO} / {})",
                    &[
                        ("RULE_NAME", RULE_NAME.to_string()),
                        ("PORT_FROM", PORT_FROM.to_string()),
                        ("PORT_TO", PORT_TO.to_string()),
                    ],
                ),
                &[if r.profiles.is_empty() {
                    "-".to_string()
                } else {
                    r.profiles.clone()
                }],
            )
        } else {
            // 規則は無いが Windows も検査していない (ファイアウォールが切られている)。
            tr("✅ 受信はブロックされていません (ファイアウォールが無効です)")
        };
        let net = r.network_label();
        if !net.is_empty() {
            msg.push_str(&trf("\n\u{3000}いまのネットワーク: {net}", &[("net", net)]));
        }
        return msg;
    }
    let mut out = Vec::new();
    for p in &problems {
        out.push(tr(p.headline()));
        // 別製品は名指しで出す。「別のファイアウォール」とだけ言われても
        // どこを開けば良いのか分からない。
        if *p == Problem::OtherFirewall {
            out.push(format!("\u{3000}{}", r.other_firewall_label()));
        }
        out.push(
            tr(p.detail())
                .lines()
                .map(|l| format!("\u{3000}{}", l.trim_start()))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    let net = r.network_label();
    if !net.is_empty() {
        let suffix = if r.profiles.is_empty() {
            String::new()
        } else {
            crate::i18n::fill_positional(&tr(" / 規則のプロファイル: {}"), &[r.profiles.clone()])
        };
        out.push(crate::i18n::fill_positional(
            &trf("\u{3000}いまのネットワーク: {net}{}", &[("net", net)]),
            &[suffix],
        ));
    }
    if problems.contains(&Problem::StrictInbound) {
        out.push(tr("\u{3000}→ `zai firewall unblock` (管理者の確認あり)"));
    }
    if r.fixable_by_allow() {
        out.push(tr("\u{3000}→ `zai firewall allow` (管理者の確認あり)"));
    }
    out.join("\n")
}

/// `zai firewall <status|allow|revoke|unblock>`。インストーラや自動化から使う。
/// 管理者で実行していれば UAC は出ない (install.ps1 の昇格経路と噛み合う)。
pub fn run(args: &[String]) -> i32 {
    let sub = args.first().map(|s| s.as_str()).unwrap_or("status");
    #[cfg(not(windows))]
    {
        let _ = sub;
        println!(
            "{}",
            tr("この OS ではファイアウォールの設定は要りません (受信は既定で通ります)。")
        );
        return 0;
    }
    #[cfg(windows)]
    {
        let result: Result<String, String> = match sub {
            "status" | "" => check_now().map(status_text),
            "allow" => allow_now().map(|r| {
                let mut msg = crate::i18n::fill_positional(
                    &trf(
                        "✅ 受信を許可しました: TCP {PORT_FROM}-{PORT_TO} / {}",
                        &[
                            ("PORT_FROM", PORT_FROM.to_string()),
                            ("PORT_TO", PORT_TO.to_string()),
                        ],
                    ),
                    &[if r.profiles.is_empty() {
                        "-".to_string()
                    } else {
                        r.profiles.clone()
                    }],
                );
                // 規則を作っても残る原因 (「すべての受信接続をブロックする」) は
                // ここで言っておく。黙っていると「許可したのに繋がらない」に戻る。
                for p in r.problems() {
                    msg.push_str(&format!("\n{}", tr(p.headline())));
                }
                if r.problems().contains(&Problem::StrictInbound) {
                    msg.push_str(&tr("\n\u{3000}`zai firewall unblock` で解除できます"));
                }
                msg
            }),
            "revoke" | "remove" | "uninstall" => revoke_now().map(|_| {
                trf(
                    "🗑 受信許可を取り消しました: {RULE_NAME}",
                    &[("RULE_NAME", RULE_NAME.to_string())],
                )
            }),
            "unblock" => unblock_now().map(|r| {
                crate::i18n::fill_positional(
                    &tr("✅ 「すべての受信接続をブロックする」を解除しました\n{}"),
                    &[status_text(r)],
                )
            }),
            other => Err(trf(
                "不明な firewall サブコマンドです: {other} (status / allow / revoke / unblock)",
                &[("other", other.to_string())],
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
    /// 「すべての受信接続をブロックする」の解除。
    Unblock,
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

    /// 「すべての受信接続をブロックする」を解除する (UAC の確認が出る)。
    pub fn unblock(&mut self) {
        self.spawn(Busy::Unblock);
    }

    fn spawn(&mut self, what: Busy) {
        if self.busy.is_some() || !applicable() {
            return;
        }
        self.busy = Some(what);
        self.error = None;
        #[cfg(windows)]
        {
            let (tx, rx) = mpsc::channel();
            self.rx = Some(rx);
            let _ = std::thread::Builder::new()
                .name("zv-firewall".into())
                .spawn(move || {
                    let r = match what {
                        Busy::Check => check_now(),
                        // プロファイルは allow_now が「いまの」ネットワークから
                        // 決める (画面の状態は古いことがある)。
                        Busy::Allow => allow_now(),
                        Busy::Revoke => revoke_now(),
                        Busy::Unblock => unblock_now(),
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
                // 「許可しました」は **本当に繋がる状態になったときだけ** 出す。
                // 規則を作れても「すべての受信接続をブロックする」が残っていれば
                // スマホからは繋がらない — そこで成功トーストを出すと、元のバグ
                // (画面は許可済み・実際は繋がらない) をそのまま再現してしまう。
                self.done = match what {
                    Some(Busy::Allow) if report.problems().is_empty() => {
                        Some(tr("🛡 ファイアウォールで受信を許可しました"))
                    }
                    Some(Busy::Unblock) if report.strict == 0 => {
                        Some(tr("🛡 「すべての受信接続をブロックする」を解除しました"))
                    }
                    Some(Busy::Revoke) if !report.allowed => Some(tr("🛡 受信許可を取り消しました")),
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
    /// 規則の有無ではなく [`Report::problems`] で見る — 規則があっても
    /// いまのネットワークに適用されていなければ繋がらない。
    pub fn needs_allow(&self) -> bool {
        !self.problems().is_empty()
    }

    /// スマホから繋がらない原因 (直すべき順)。空なら繋がるはず。
    pub fn problems(&self) -> Vec<Problem> {
        if !applicable() {
            return Vec::new();
        }
        self.report
            .as_ref()
            .map(|r| r.problems())
            .unwrap_or_default()
    }

    /// 手作業用の netsh コマンド (コピーさせる)。
    pub fn manual(&self) -> String {
        manual_command(&self.exe(), allow_profiles())
    }

    /// この実行ファイルのパス。**別のファイアウォール製品へ登録するときに使う** —
    /// ノートン等の「プログラム制御」は exe を指定させるので、そのまま貼れる形で渡す。
    pub fn exe(&self) -> String {
        std::env::current_exe()
            .map(|p| crate::pathx::canonical(&p).to_string_lossy().to_string())
            .unwrap_or_else(|_| "zai.exe".into())
    }

    /// Windows とは別に動いているファイアウォール製品の名前 (無ければ空)。
    pub fn other_firewall(&self) -> String {
        self.report
            .as_ref()
            .map(|r| r.other_firewall_label())
            .unwrap_or_default()
    }
}

// ───────────────────────── テスト ─────────────────────────

#[cfg(test)]
mod tests {
    /// 位置プレースホルダの埋め方を表で固定する (`desktop.rs` と対の複製)。
    /// 訳文は外部ファイルから来るので、`{}` の数が原文と食い違いうる。
    #[test]
    fn 位置プレースホルダは順に埋まり数が食い違っても壊れない() {
        let cases: &[(&str, &[&str], &str)] = &[
            ("{} を書けません: x", &["/a/b"], "/a/b を書けません: x"),
            ("Cannot write {}", &["/a", "余り"], "Cannot write /a"),
            ("{} と {}", &["/a"], "/a と {}"),
            ("穴なし", &["/a"], "穴なし"),
            ("{} / {}", &["a{}b", "後"], "a{}b / 後"),
        ];
        for (tpl, args, want) in cases {
            let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
            assert_eq!(
                &crate::i18n::fill_positional(tpl, &owned),
                want,
                "template={tpl:?}"
            );
        }
    }

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
        let r =
            parse_report("ZVFW allow=1 block=0 profiles=Domain cats=DomainAuthenticated,Private")
                .expect("読める");
        assert_eq!(
            r.categories,
            vec!["Domain".to_string(), "Private".to_string()]
        );
    }

    #[test]
    fn parse_report_reads_the_profile_state() {
        let r = parse_report(
            "ZVFW allow=1 block=0 profiles=Domain,Private cats=Private net=1 on=1 strict=1",
        )
        .expect("読める");
        assert_eq!(
            r.rule_profiles,
            vec!["Domain".to_string(), "Private".to_string()]
        );
        assert_eq!((r.networks, r.enforcing, r.strict), (1, 1, 1));
    }

    /// 規則が複数あると `profiles` は "/" で連なる。判定は両方の中身で行う。
    #[test]
    fn parse_report_splits_profiles_on_both_separators() {
        let r = parse_report("ZVFW allow=2 block=0 profiles=Domain,Private/Public cats=Public")
            .expect("読める");
        assert_eq!(
            r.rule_profiles,
            vec![
                "Domain".to_string(),
                "Private".to_string(),
                "Public".to_string()
            ]
        );
        assert!(
            r.covers_current_network(),
            "Public を含む規則があるなら適用される"
        );
    }

    // ── 「許可したのに繋がらない」の原因判定 ──

    /// 元のバグの核心。Private の規則はパブリック扱いの Wi-Fi では適用されないので、
    /// 「許可済み ✅」と出しながらスマホからは繋がらない。ここで拾えないと
    /// 画面のどこにも原因が出ず、利用者は「許可しても直らない」と詰まる。
    #[test]
    fn a_private_rule_does_not_cover_a_public_network() {
        let r = parse_report(
            "ZVFW allow=1 block=0 profiles=Domain,Private cats=Public net=1 on=1 strict=0",
        )
        .expect("読める");
        assert!(r.allowed, "規則自体はある");
        assert!(!r.covers_current_network());
        assert_eq!(r.problems(), vec![Problem::ProfileMismatch]);
        assert!(r.fixable_by_allow(), "作り直せば直る");
        // 作り直す規則は 3 つとも含む (再分類されてもまた落ちないように)
        assert_eq!(allow_profiles(), "Domain,Private,Public");
    }

    #[test]
    fn a_matching_rule_has_no_problems() {
        let r = parse_report(
            "ZVFW allow=1 block=0 profiles=Domain,Private cats=Private net=1 on=1 strict=0",
        )
        .expect("読める");
        assert!(r.covers_current_network());
        assert!(r.problems().is_empty(), "繋がるはずの状態で警告を出さない");
        assert!(!r.fixable_by_allow());
    }

    /// `Any` は「全プロファイル」— Windows は 3 つ揃うとこう畳む。
    #[test]
    fn an_any_profile_rule_covers_every_network() {
        let r = parse_report("ZVFW allow=1 block=0 profiles=Any cats=Public net=1 on=1 strict=0")
            .expect("読める");
        assert!(r.covers_current_network());
        assert!(r.problems().is_empty());
    }

    /// 「すべての受信接続をブロックする」は許可規則ごと無視する。
    /// 規則を作り直しても直らないので、必ず先頭に出す。
    #[test]
    fn strict_inbound_comes_first_and_is_not_fixed_by_allowing() {
        let r = parse_report(
            "ZVFW allow=1 block=0 profiles=Domain,Private cats=Private net=1 on=1 strict=1",
        )
        .expect("読める");
        assert_eq!(r.problems(), vec![Problem::StrictInbound]);
        assert!(!r.fixable_by_allow(), "許可ボタンでは直らない");
    }

    #[test]
    fn strict_inbound_is_listed_before_a_missing_rule() {
        let r = parse_report("ZVFW allow=0 block=0 profiles= cats=Private net=1 on=1 strict=1")
            .expect("読める");
        assert_eq!(r.problems(), vec![Problem::StrictInbound, Problem::NoRule]);
        assert!(r.fixable_by_allow(), "規則が無い分は許可で直る");
    }

    #[test]
    fn a_deny_rule_is_reported_even_with_an_allow_rule() {
        let r = parse_report(
            "ZVFW allow=1 block=1 profiles=Domain,Private cats=Private net=1 on=1 strict=0",
        )
        .expect("読める");
        assert_eq!(r.problems(), vec![Problem::DenyRule]);
        assert!(r.fixable_by_allow(), "許可時に拒否規則を消すので直る");
    }

    /// ファイアウォールが切られていれば Windows は何も落とさない。
    /// 規則が無くても繋がるので、ここで警告を出すのは嘘になる。
    #[test]
    fn a_disabled_firewall_drops_nothing() {
        let r = parse_report("ZVFW allow=0 block=0 profiles= cats=Private net=1 on=0 strict=0")
            .expect("読める");
        assert!(!r.enforcing_now());
        assert!(r.problems().is_empty());
    }

    /// 状態が読めないときは安全側 = 案内を出す (黙って繋がらないのが最悪)。
    #[test]
    fn an_unreadable_profile_state_still_warns_about_a_missing_rule() {
        let r = parse_report("ZVFW allow=0 block=0 profiles= cats=").expect("読める");
        assert!(r.enforcing_now(), "取れないときは検査されている前提");
        assert_eq!(r.problems(), vec![Problem::NoRule]);
    }

    /// 判定材料が無いとき (種別が読めない) に不一致だと騒がない。
    #[test]
    fn unknown_categories_do_not_cause_a_false_mismatch() {
        let r = parse_report("ZVFW allow=1 block=0 profiles=Domain,Private cats=").expect("読める");
        assert!(r.covers_current_network());
        assert!(r.problems().is_empty());
    }

    // ── 別のファイアウォール製品 (ノートン等) ──

    /// 本命の回帰。Windows 側は完璧に許可済みでも、受信を落としているのが
    /// ノートン等なら **スマホからは真っ白のまま**。ここで黙ると画面には
    /// 「✅ 許可済み」しか出ず、利用者は Windows の設定を延々いじる羽目になる。
    #[test]
    fn a_third_party_firewall_is_reported_even_when_windows_is_fully_allowed() {
        let r = parse_report(
            "ZVFWX ノートン 360\n\
             ZVFW allow=1 block=0 profiles=Any cats=Public net=1 on=1 strict=0 other=1",
        )
        .expect("読める");
        assert!(r.allowed, "Windows 側は許可済み");
        assert!(r.covers_current_network(), "プロファイルも合っている");
        assert_eq!(r.other_fw, vec!["ノートン 360".to_string()]);
        assert_eq!(r.problems(), vec![Problem::OtherFirewall]);
        assert!(!r.fixable_by_allow(), "Windows 側の許可では直らない");
    }

    /// Windows 側にも問題があるときは、1 クリックで直せる Windows を先に出す。
    #[test]
    fn windows_causes_come_before_the_other_product() {
        let r = parse_report(
            "ZVFWX ESET Security\n\
             ZVFW allow=0 block=0 profiles= cats=Private net=1 on=1 strict=0 other=1",
        )
        .expect("読める");
        assert_eq!(r.problems(), vec![Problem::NoRule, Problem::OtherFirewall]);
    }

    /// 「Windows のファイアウォールが無効 = 何も落とされない」は、別製品が
    /// 動いているときは嘘になる。ここを黙ると元のバグ (原因が画面に出ない) に戻る。
    #[test]
    fn a_disabled_windows_firewall_is_not_an_all_clear_when_another_product_runs() {
        let off = parse_report("ZVFW allow=0 block=0 profiles= cats=Private net=1 on=0 strict=0")
            .expect("読める");
        assert!(off.problems().is_empty(), "本当に何も無ければ黙る");

        let r = parse_report(
            "ZVFWX マカフィー\n\
             ZVFW allow=0 block=0 profiles= cats=Private net=1 on=0 strict=0 other=1",
        )
        .expect("読める");
        assert!(!r.enforcing_now(), "Windows は検査していない");
        assert_eq!(
            r.problems(),
            vec![Problem::OtherFirewall],
            "それでも黙らない"
        );
        assert!(
            !status_text(r).contains("✅"),
            "繋がらないのに ✅ を出さない"
        );
    }

    /// 名前が読めなくても「別の製品が居る」ことだけは伝える。
    #[test]
    fn an_unnamed_product_still_raises_the_cause() {
        let r = parse_report(
            "ZVFW allow=1 block=0 profiles=Any cats=Private net=1 on=1 strict=0 other=2",
        )
        .expect("読める");
        assert!(r.has_other_firewall());
        assert_eq!(r.problems(), vec![Problem::OtherFirewall]);
        assert!(!r.other_firewall_label().is_empty(), "空欄を出さない");
    }

    #[test]
    fn no_other_product_means_no_extra_noise() {
        let r = parse_report(
            "ZVFW allow=1 block=0 profiles=Any cats=Private net=1 on=1 strict=0 other=0",
        )
        .expect("読める");
        assert!(!r.has_other_firewall());
        assert!(r.problems().is_empty(), "居ないのに警告を出さない");
    }

    /// 製品名は空白を含む ("ノートン 360") ので要約行には載せられない。
    /// 別行で受け取り、要約行の解析を壊さないこと。
    #[test]
    fn product_lines_do_not_disturb_the_summary_line() {
        let r = parse_report(
            "ZVFWX ノートン 360\n\
             ZVFWX ESET Endpoint Security\n\
             ZVFW allow=1 block=0 profiles=Domain,Private cats=Private net=1 on=1 strict=0 other=2",
        )
        .expect("読める");
        assert_eq!(r.profiles, "Domain, Private", "要約行はそのまま読める");
        assert_eq!(r.categories, vec!["Private".to_string()]);
        assert_eq!(
            r.other_firewall_label(),
            "ノートン 360 / ESET Endpoint Security"
        );
    }

    /// 状態を調べる側が製品を見に行っていること。
    #[test]
    fn check_script_looks_for_other_firewall_products() {
        let s = check_script(r"C:\bin\zai.exe");
        assert!(s.contains("root/SecurityCenter2"));
        assert!(s.contains("FirewallProduct"));
        assert!(s.contains("ZVFWX"), "名前は別行で返す");
        assert!(s.contains("other=$"), "件数も要約行に載せる");
        // Windows 自身を「別製品」と数えない
        assert!(s.contains("-notmatch 'Windows'"));
    }

    #[test]
    fn every_problem_explains_itself_and_how_to_fix_it() {
        for p in [
            Problem::StrictInbound,
            Problem::DenyRule,
            Problem::NoRule,
            Problem::ProfileMismatch,
            Problem::OtherFirewall,
        ] {
            assert!(!p.headline().is_empty());
            assert!(p.detail().len() > 20, "直し方まで書く: {p:?}");
        }
    }

    #[test]
    fn network_label_is_japanese() {
        let r =
            parse_report("ZVFW allow=1 block=0 profiles=Any cats=Private,Public").expect("読める");
        assert_eq!(r.network_label(), "プライベート, パブリック");
        assert_eq!(Report::default().network_label(), "");
    }

    // ── status_text (CLI) ──

    #[test]
    fn status_text_names_the_cause_and_the_fix() {
        let r = parse_report(
            "ZVFW allow=1 block=0 profiles=Domain,Private cats=Public net=1 on=1 strict=0",
        )
        .expect("読める");
        let s = status_text(r);
        assert!(s.contains(Problem::ProfileMismatch.headline()));
        assert!(s.contains("zai firewall allow"), "直し方を出す");
        assert!(s.contains("パブリック"), "いまのネットワーク種別を出す");
        assert!(!s.contains("✅"), "繋がらないのに ✅ を出さない");
    }

    #[test]
    fn status_text_sends_strict_inbound_to_unblock() {
        let s = status_text(
            parse_report("ZVFW allow=1 block=0 profiles=Any cats=Private net=1 on=1 strict=1")
                .expect("読める"),
        );
        assert!(s.contains("zai firewall unblock"));
        assert!(
            !s.contains("zai firewall allow"),
            "許可では直らないので勧めない"
        );
    }

    #[test]
    fn status_text_says_ok_when_nothing_is_wrong() {
        let s = status_text(
            parse_report(
                "ZVFW allow=1 block=0 profiles=Domain,Private cats=Private net=1 on=1 strict=0",
            )
            .expect("読める"),
        );
        assert!(s.starts_with("✅"));
        assert!(s.contains(RULE_NAME));
    }

    #[test]
    fn parse_report_without_marker_is_none() {
        assert!(parse_report("").is_none());
        assert!(parse_report("Get-NetFirewallRule : 用語を認識できません").is_none());
    }

    /// 規則は 3 プロファイルすべてで作る。種別に合わせて絞ると、Windows が
    /// 同じ Wi-Fi を「識別されていないネットワーク」= パブリックへ再分類した
    /// だけで規則が効かなくなり、「許可したのにまた繋がらない」が再発する。
    #[test]
    fn the_rule_covers_every_profile_so_it_survives_reclassification() {
        assert_eq!(allow_profiles(), "Domain,Private,Public");
        // 実際に作る規則がその 3 つを持つこと (ここがずれると再発する)
        assert!(allow_script(r"C:\bin\zai.exe", allow_profiles())
            .contains("-Profile Domain,Private,Public"));
        // どの種別のネットワークでも適用される = 二度と不一致にならない
        for cats in [
            vec!["Public"],
            vec!["Private"],
            vec!["Domain"],
            vec!["Private", "Public"],
        ] {
            let r = Report {
                allowed: true,
                rule_profiles: vec!["Domain".into(), "Private".into(), "Public".into()],
                categories: cats.iter().map(|c| c.to_string()).collect(),
                networks: 1,
                enforcing: 1,
                ..Report::default()
            };
            assert!(r.covers_current_network(), "{cats:?} で適用されない");
            assert!(r.problems().is_empty(), "{cats:?} で警告が出る");
        }
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
        assert!(
            s.contains(r"$exe = 'C:\Users\o''brien\zai.exe'"),
            "' は '' に畳む"
        );
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
        assert!(
            s.contains("Get-NetConnectionProfile"),
            "ネットワーク種別も見る"
        );
        // 空白入りの値を出すと読めなくなる (Profile は "Private, Public" と出る)
        assert!(s.contains(r"-replace '[\s]', ''"));
    }

    /// 規則の有無だけでは「許可済みなのに繋がらない」を説明できない。
    /// いま繋いでいるネットワークのプロファイル設定まで持ち帰ること。
    #[test]
    fn check_script_also_reads_the_live_profile_state() {
        let s = check_script(r"C:\bin\zai.exe");
        assert!(s.contains("Get-NetFirewallProfile"));
        assert!(s.contains("AllowInboundRules"), "受信の全ブロックを見る");
        assert!(s.contains("net=$"), "接続数");
        assert!(s.contains("on=$"), "ファイアウォールが有効な数");
        assert!(s.contains("strict=$"), "全ブロックの数");
        // 規則側は Domain、接続側は DomainAuthenticated と綴りが違う
        assert!(s.contains("DomainAuthenticated"));
    }

    /// 触るのは **いま繋いでいるネットワークのプロファイルだけ**。
    /// 使っていない Public まで緩めない。
    #[test]
    fn unblock_script_only_touches_the_current_profiles() {
        let s = unblock_script();
        assert!(s.contains("Get-NetConnectionProfile"));
        assert!(s.contains("Set-NetFirewallProfile -Name $n -AllowInboundRules True"));
        // 対象は接続中の種別だけ。`-All` や 3 つ並べた指定で一括適用しない
        assert!(s.contains("foreach ($n in $names)"));
        assert!(!s.contains("-All "), "全プロファイルへ一括適用しない");
        assert!(!s.contains("Domain,Private,Public"));
        assert!(!s.contains("New-NetFirewallRule"), "規則は作らない");
        assert!(
            !s.contains("Set-NetFirewallProfile -Enabled"),
            "ファイアウォールは切らない"
        );
    }

    /// UAC を断ったときの合図は終了コード。文言照合は言語で外れる。
    #[test]
    fn elevate_script_reports_a_cancelled_uac_by_exit_code() {
        let s = elevate_script(r"C:\tmp\a.ps1");
        assert!(s.contains("try {"));
        assert!(s.contains(&format!("exit {CANCELLED}")));
        assert_eq!(CANCELLED, 1223, "Windows の ERROR_CANCELLED");
    }

    /// 昇格側の標準エラーは返ってこない。理由はログへ落とさせる。
    #[test]
    fn with_error_log_wraps_the_script_and_records_why_it_failed() {
        let s = with_error_log("New-NetFirewallRule ...\n", r"C:\tmp\o'brien\err.log");
        assert!(
            s.contains(r"$ZvLog = 'C:\tmp\o''brien\err.log'"),
            "' は '' に畳む"
        );
        assert!(
            s.contains("New-NetFirewallRule ..."),
            "中身はそのまま走らせる"
        );
        assert!(s.contains("try {"));
        assert!(s.contains("} catch {"));
        assert!(s.contains("Set-Content -LiteralPath $ZvLog"));
        assert!(s.contains("exit 1"), "失敗は 0 以外で返す");
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
