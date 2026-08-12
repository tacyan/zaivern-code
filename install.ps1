# Zaivern Code ワンライナーインストーラ (Windows)
#   irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
#
# やること:
#   1. GitHub Releases のビルド済み zai.exe を取得
#   2. リリースの checksums.txt と SHA-256 を突き合わせてから
#      %LOCALAPPDATA%\Zaivern\bin へ配置
#      (**検証できなければ展開も実行もせずに中止する = fail-closed**)
#   3. ビルド済みが取得できない場合はソースからビルド
#      (Rust が無ければ rustup ごと非対話でセットアップ)
#
# 2回目以降の実行は「更新」として動作する:
#   最新版を取得して上書きし、PATH 上の別の場所(~\.cargo\bin 等)に残った
#   古い zai.exe も同じバイナリで揃える(古い方が先に見つかって起動するのを防ぐ)
#
# 環境変数:
#   ZAI_INSTALL_DIR    ビルド済みバイナリの配置先 (既定: %LOCALAPPDATA%\Zaivern\bin)
#   ZAI_FROM_SOURCE=1  常にソースビルドする

$repo = "tacyan/zaivern-code"
$repoUrl = "https://github.com/$repo"
$requiredMinor = 88
$installDir = if ($env:ZAI_INSTALL_DIR) { $env:ZAI_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA "Zaivern\bin" }
$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"

function Say($msg) { Write-Host "[zaivern-code] $msg" -ForegroundColor Cyan }
function Warn($msg) { Write-Host "[zaivern-code] $msg" -ForegroundColor Yellow }

# --- 配布物の検証 (fail-closed) ----------------------------------------------
# 取得した zip は **展開する前に** リリースの checksums.txt と突き合わせる。
# Expand-Archive がファイルを書き出した後で確かめても手遅れ。
#
# 検証できない理由が何であれ (取得失敗・行が無い・不一致) $false を返す。
# 呼び出し側は zip を消し、ソースビルドへも降りずに終わる —
# 「確かめられなかったので、とりあえず入れた」は検証していないのと同じ。
function Test-Checksum($file, $baseName, $sumsUrl) {
    try {
        # -UseBasicParsing: Windows PowerShell 5.1 で IE エンジンに依存しない
        $body = (Invoke-WebRequest $sumsUrl -UseBasicParsing).Content
    } catch {
        Warn "checksums.txt を取得できませんでした: $_"
        return $false
    }
    # release.yml が書く標準形 "<64桁hex><SP><SP><ファイル名>" だけを受け付ける。
    $want = $null
    foreach ($line in ($body -split "`n")) {
        if ($line -match '^([0-9a-fA-F]{64})  (\S+)\s*$' -and $matches[2] -eq $baseName) {
            $want = $matches[1].ToLower()
            break
        }
    }
    if (-not $want) { Warn "checksums.txt に $baseName の行がありません"; return $false }
    $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $file).Hash.ToLower()
    if ($want -ne $got) {
        Warn "   期待値: $want"
        Warn "   実際  : $got"
        return $false
    }
    Say "✅ SHA-256 一致: $baseName"
    return $true
}

# ユーザー PATH へ追加 (未登録の場合のみ)。現在のセッションにも反映する。
function Add-UserPath($dir) {
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if (($userPath -split ";") -notcontains $dir) {
        [Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
        Warn "PATH に $dir を追加しました (新しいターミナルから有効)"
    }
    if (($env:Path -split ";") -notcontains $dir) { $env:Path = "$env:Path;$dir" }
}

# zai.exe を配置する。起動中の exe は上書きできないので、
# その場合は実行中のファイルを .old へ改名してから置き換える
# (Windows は実行中の exe を削除できないが改名はできる)。
function Copy-Binary($src, $dst) {
    try {
        Copy-Item $src $dst -Force
        # 前回の置き換えで残った .old を掃除する (まだ使用中なら失敗するので無視)
        Remove-Item "$dst.old" -Force -ErrorAction SilentlyContinue
        return $true
    } catch {
        if (-not (Test-Path $dst)) { throw }
        $old = "$dst.old"
        try {
            Remove-Item $old -Force -ErrorAction SilentlyContinue  # 前回の残骸 (使用中なら残る)
            Move-Item $dst $old -Force
            Copy-Item $src $dst -Force
            Warn "起動中の $dst を置き換えました (次回起動から新しい版になります)"
            return $true
        } catch {
            Warn "$dst を更新できませんでした: $_"
            return $false
        }
    }
}

# 既知のインストール先に残った古い zai.exe を新バイナリで揃える
# (PATH 順によっては古い方が起動してしまい「更新されない」ように見えるため)
function Sync-Stale($newBin, $skipDir) {
    foreach ($d in @((Join-Path $env:LOCALAPPDATA "Zaivern\bin"), $cargoBin)) {
        if ($d -eq $skipDir) { continue }
        $old = Join-Path $d "zai.exe"
        if (Test-Path $old) {
            Say "旧バイナリを更新します: $old"
            $null = Copy-Binary $newBin $old
        }
    }
}

# OS のアプリ一覧 (スタートメニュー) へ登録。失敗しても続行。
# zai.exe は GUI サブシステムなので Start-Process -Wait で完了を待つ
function Register-App($exe) {
    try {
        Start-Process -FilePath $exe -ArgumentList "app", "install" -Wait -WindowStyle Hidden
        Say "スタートメニューに「Zaivern Code」を登録しました (解除: zai app uninstall)"
    } catch {
        Warn "スタートメニュー登録をスキップしました: $_"
    }
}

$fwRule = "Zaivern Code (Mobile Remote)"

# 📱 スマホリモートの受信許可。
# Windows は既定で受信をブロックするため、規則が無いとスマホから接続できない
# (PC 側は待ち受けていて QR も正しいのに、スマホからだけ何も起きない)。
# 規則の作成には管理者権限が必要なので:
#   - すでに管理者で実行しているならこの場で作る (UAC は出ない)
#   - そうでなければ勝手に昇格せず、案内だけ出す
#     (アプリの 📱 画面のボタン、または `zai firewall allow` から作れる)
function Register-Firewall($exe) {
    try {
        if (Get-NetFirewallRule -DisplayName $fwRule -ErrorAction SilentlyContinue) {
            Say "📱 スマホリモートの受信は許可済みです (取り消し: zai firewall revoke)"
            return
        }
        $id = [Security.Principal.WindowsIdentity]::GetCurrent()
        $admin = (New-Object Security.Principal.WindowsPrincipal $id).IsInRole(
            [Security.Principal.WindowsBuiltInRole]::Administrator)
        if (-not $admin) {
            Warn "📱 スマホリモートには受信許可が必要です (Windows は既定で受信をブロックします)"
            Warn "   許可の仕方: アプリの 📱 画面のボタン、または管理者で zai firewall allow"
            return
        }
        Start-Process -FilePath $exe -ArgumentList "firewall", "allow" -Wait -WindowStyle Hidden
        if (Get-NetFirewallRule -DisplayName $fwRule -ErrorAction SilentlyContinue) {
            Say "📱 スマホリモートの受信を許可しました (TCP 8899-8919 / 取り消し: zai firewall revoke)"
        } else {
            Warn "📱 受信許可を作成できませんでした — アプリの 📱 画面のボタンから許可してください"
        }
    } catch {
        Warn "ファイアウォールの確認をスキップしました: $_"
    }
}

function Show-Done($verb, $exe, $tag) {
    Write-Host ""
    Write-Host "[zaivern-code] ✅ ${verb}完了: $exe $tag" -ForegroundColor Green
    Write-Host "[zaivern-code]    起動: プロジェクトのフォルダで zai . (または zai [ワークスペースのパス])"
    Write-Host "[zaivern-code]    スタートメニューの「Zaivern Code」からも起動できます (解除: zai app uninstall)"
}

# --- ビルド済みバイナリのインストール ----------------------------------------
function Install-Prebuilt {
    if ([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture -ne "X64") {
        Warn "ビルド済みバイナリは x86_64 のみです (現在: $([System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture))"
        return $false
    }
    Say "最新リリースを確認しています..."
    $tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
    if (-not $tag) { return $false }

    $name = "zai-$tag-windows-x86_64"
    $url = "$repoUrl/releases/download/$tag/$name.zip"
    $zip = Join-Path $env:TEMP "$name.zip"
    $extract = Join-Path $env:TEMP "zai-extract"
    $sumsUrl = "$repoUrl/releases/download/$tag/checksums.txt"
    Say "ダウンロード: $url"
    $ProgressPreference = "SilentlyContinue"  # Invoke-WebRequest の進捗バーは遅いので切る
    Invoke-WebRequest $url -OutFile $zip

    # ここから先は fail-closed。展開する前に必ず突き合わせる。
    # 戻り値は「最後の出力」で判定する (Say/Warn は Write-Host なので
    # パイプラインには乗らないが、この形なら混ざっても壊れない)。
    Say "チェックサムを確認します: $sumsUrl"
    if (@(Test-Checksum $zip "$name.zip" $sumsUrl)[-1] -ne $true) {
        Remove-Item $zip -Force -ErrorAction SilentlyContinue
        Write-Host "[zaivern-code] ⛔ 配布物を検証できなかったため中止しました。" -ForegroundColor Red
        Write-Host "[zaivern-code]    ダウンロードしたものは展開も実行もしていません。" -ForegroundColor Red
        Write-Host '[zaivern-code]    ソースから入れる場合: $env:ZAI_FROM_SOURCE = "1" を設定して再実行してください。'
        $script:zaiGiveUp = $true   # 検証に失敗した以上、黙ってソースビルドへ降りない
        return $false
    }

    if (Test-Path $extract) { Remove-Item $extract -Recurse -Force }
    Expand-Archive $zip -DestinationPath $extract -Force

    $new = Join-Path $extract "$name\zai.exe"
    $exe = Join-Path $installDir "zai.exe"
    $verb = if (Test-Path $exe) { "更新" } else { "インストール" }
    New-Item -ItemType Directory -Force -Path $installDir | Out-Null
    if (-not (Copy-Binary $new $exe)) {
        Warn "Zaivern Code を終了してから、もう一度実行してください。"
        Remove-Item $zip, $extract -Recurse -Force -ErrorAction SilentlyContinue
        $script:zaiGiveUp = $true   # ダウンロードは成功しているのでソースビルドはしない
        return $false
    }
    Sync-Stale $new $installDir
    Remove-Item $zip, $extract -Recurse -Force -ErrorAction SilentlyContinue

    Add-UserPath $installDir
    Register-App $exe
    Register-Firewall $exe
    Show-Done $verb $exe "($tag)"
    return $true
}

# --- ソースビルド (フォールバック) -------------------------------------------
function Install-FromSource {
    Say "ソースからビルド・インストールします..."

    # 1. Rust ツールチェーンの確認 (rustup 導入直後で PATH が未反映のケースも拾う)
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        if (Test-Path (Join-Path $cargoBin "cargo.exe")) { $env:Path = "$env:Path;$cargoBin" }
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Say "Rust (cargo) が見つかりません。rustup をインストールします..."
        $init = Join-Path $env:TEMP "rustup-init.exe"
        Invoke-WebRequest "https://win.rustup.rs/x86_64" -OutFile $init
        & $init -y --default-toolchain stable | Out-Host
        Remove-Item $init -Force -ErrorAction SilentlyContinue
        $env:Path = "$env:Path;$cargoBin"
    }
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
        Warn "cargo を用意できませんでした。https://rustup.rs から Rust を入れて再実行してください。"
        return $false
    }

    # 2. rustc 1.88+ の確認
    $ver = (rustc --version) -replace '^rustc 1\.(\d+).*', '$1'
    if (($ver -notmatch '^\d+$') -or ([int]$ver -lt $requiredMinor)) {
        Say "rustc 1.$requiredMinor+ が必要です(現在: $(rustc --version))。stable を更新します..."
        rustup update stable | Out-Host
    }

    # 3. C++ ビルドツール (link.exe) のヒント
    if (-not (Get-Command link.exe -ErrorAction SilentlyContinue)) {
        Say "ヒント: ビルドに失敗する場合は Visual Studio Build Tools が必要です:"
        Say "  winget install --id Microsoft.VisualStudio.2022.BuildTools --override `"--quiet --add Microsoft.VisualStudio.Workload.VCTools`""
    }

    # 4. GitHub から直接ビルド & インストール
    #    --force: 同一バージョンがインストール済みでも再ビルドして上書き(=再実行で更新)
    Say "GitHub からビルド・インストールします(初回は数分かかります)..."
    cargo install --git $repoUrl --locked --force zaivern-code | Out-Host
    $exe = Join-Path $cargoBin "zai.exe"
    if (-not (Test-Path $exe)) {
        Warn "ビルドに失敗しました。上のエラーを確認してください。"
        return $false
    }

    Sync-Stale $exe $cargoBin
    Add-UserPath $cargoBin
    Register-App $exe
    Register-Firewall $exe
    Show-Done "インストール" $exe ""
    return $true
}

# --- 実行 ---------------------------------------------------------------------
# irm | iex で読み込まれるため、呼び出し元のセッションを壊さないよう
# ErrorActionPreference は退避して必ず戻す (exit も使わない)。
$zaiPrevEap = $ErrorActionPreference
$ErrorActionPreference = "Stop"
$script:zaiGiveUp = $false
try {
    $ok = $false
    if ($env:ZAI_FROM_SOURCE -ne "1") {
        # 関数の戻り値は「最後の出力」で判定する (途中の出力が混ざっても壊れないように)
        try { $ok = @(Install-Prebuilt)[-1] -eq $true } catch { Warn "ビルド済みバイナリを取得できませんでした: $_" }
    }
    if (-not $ok -and -not $script:zaiGiveUp) { $null = Install-FromSource }
} finally {
    $ErrorActionPreference = $zaiPrevEap
}
