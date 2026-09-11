param([string]$Installer = (Join-Path $PSScriptRoot '../install.ps1'))
$ErrorActionPreference = 'Stop'
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $Installer, [ref]$null, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
# 定義だけ読み込み、実ユーザーへのインストールやネットワーク通信は行わない。
foreach ($name in @('Test-Checksum', 'Invoke-UpdateFileStep', 'Copy-Binary', 'Test-SourceToolchain', 'Install-FromSource')) {
    $definition = $ast.Find({ param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $true)
    Invoke-Expression $definition.Extent.Text
}
function Assert($ok, $message) { if (-not $ok) { throw $message } }
function Say($msg) {}
function Warn($msg) { Write-Host "[test diagnostic] $msg" }
$script:attempts = 0
$result = Invoke-UpdateFileStep {
    $script:attempts++
    if ($script:attempts -lt 3) { throw [IO.IOException]::new('sharing violation', -2147024864) }
    'recovered'
} 'retry test'
Assert ($result -eq 'recovered' -and $script:attempts -eq 3) 'sharing violation must recover'
$script:attempts = 0
$denied = $false
try {
    Invoke-UpdateFileStep { $script:attempts++; throw [UnauthorizedAccessException]::new('denied') } 'denied test'
} catch {
    $cause = $_.Exception
    while ($cause.InnerException) { $cause = $cause.InnerException }
    $denied = $cause -is [UnauthorizedAccessException]
}
Assert ($denied -and $script:attempts -eq 1) 'permission errors must not be swallowed or retried'
$script:attempts = 0
$timedOut = $false
try {
    Invoke-UpdateFileStep { $script:attempts++; throw [IO.IOException]::new('sharing violation', -2147024864) } 'timeout test' 50
} catch { $timedOut = $true }
Assert ($timedOut -and $script:attempts -ge 2) 'permanent sharing violation must time out'
# 実行中 exe の差し替えを実 Windows のファイルロックで検証する。
# 自分で作成した一時ディレクトリと子プロセスだけを使う。
$copyRoot = Join-Path ([IO.Path]::GetTempPath()) ('zai-copy-' + [Guid]::NewGuid().ToString('N'))
$child = $null
try {
    New-Item -ItemType Directory -Path $copyRoot | Out-Null
    $runningExe = Join-Path $copyRoot 'zai.exe'
    $replacement = Join-Path $copyRoot 'replacement.exe'
    $fixtureSource = Join-Path $copyRoot 'fixture.rs'
    [IO.File]::WriteAllText($fixtureSource, 'fn main() { println!("ready"); let mut line = String::new(); std::io::stdin().read_line(&mut line).unwrap(); }')
    & rustc --crate-name zai_copy_fixture $fixtureSource -o $runningExe
    Assert ($LASTEXITCODE -eq 0) 'copy fixture must compile'
    [IO.File]::WriteAllText($replacement, 'replacement fixture')
    $start = New-Object Diagnostics.ProcessStartInfo
    $start.FileName = $runningExe
    $start.UseShellExecute = $false
    $start.CreateNoWindow = $true
    $start.RedirectStandardInput = $true
    $start.RedirectStandardOutput = $true
    $child = [Diagnostics.Process]::Start($start)
    $ready = $child.StandardOutput.ReadLineAsync()
    Assert ($ready.Wait(10000)) 'copy fixture must start within 10 seconds'
    Assert ($ready.Result.Trim() -eq 'ready') 'copy fixture must be running'
    Assert (-not $child.HasExited) 'original process must be alive before replacement'
    Assert (Copy-Binary $replacement $runningExe) 'running executable must be replaced'
    Assert ((Get-FileHash $runningExe).Hash -eq (Get-FileHash $replacement).Hash) 'installed bytes must match'
    Assert (-not $child.HasExited) 'replacement must not terminate the original process'
} finally {
    if ($child -and -not $child.HasExited) {
        $child.StandardInput.WriteLine('done')
        $child.StandardInput.Close()
        if (-not $child.WaitForExit(10000)) { throw 'owned copy fixture did not exit; retained for diagnosis' }
    }
    if ($child) { $child.Dispose() }
    $resolvedCopyRoot = [IO.Path]::GetFullPath($copyRoot)
    Assert ($resolvedCopyRoot.StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()), [StringComparison]::OrdinalIgnoreCase)) 'cleanup must stay in temp'
    try { Remove-Item -LiteralPath $resolvedCopyRoot -Recurse -Force } catch {
        Write-Warning "Test fixture retained at ${resolvedCopyRoot}: $_"
    }
}
# コピー失敗・改名後の配置失敗では元の実行ファイルを保持する。
$copyRoot = Join-Path ([IO.Path]::GetTempPath()) ('zai-copy-failure-' + [Guid]::NewGuid().ToString('N'))
try {
    New-Item -ItemType Directory -Path $copyRoot | Out-Null
    $dst = Join-Path $copyRoot 'zai.exe'
    $src = Join-Path $copyRoot 'new.exe'
    [IO.File]::WriteAllText($dst, 'original')
    [IO.File]::WriteAllText($src, 'replacement')
    Assert (-not (Copy-Binary (Join-Path $copyRoot 'missing.exe') $dst)) 'missing source must fail'
    Assert ([IO.File]::ReadAllText($dst) -eq 'original') 'copy failure must retain original'
    function Move-Item { throw 'injected placement failure' }
    try {
        Assert (-not (Copy-Binary $src $dst)) 'placement failure must fail'
        Assert ([IO.File]::ReadAllText($dst) -eq 'original') 'placement failure must restore original'
    } finally { Remove-Item Function:Move-Item }
    Assert (Copy-Binary $src $dst) 'unlocked executable must update'
    Assert ([IO.File]::ReadAllText($dst) -eq 'replacement') 'successful update must install replacement'
} finally {
    $resolvedCopyRoot = [IO.Path]::GetFullPath($copyRoot)
    Assert ($resolvedCopyRoot.StartsWith([IO.Path]::GetFullPath([IO.Path]::GetTempPath()), [StringComparison]::OrdinalIgnoreCase)) 'cleanup must stay in temp'
    Remove-Item -LiteralPath $resolvedCopyRoot -Recurse -Force
}
# レスポンスの型だけを模擬し、SHA-256 は実ファイルから計算する。
function Invoke-WebRequest($uri, [switch]$UseBasicParsing) {
    if ($script:downloadFails) { throw 'simulated download failure' }
    return [pscustomobject]@{ Content = $script:checksumContent }
}
$fixture = Join-Path ([IO.Path]::GetTempPath()) ('zai-checksum-' + [Guid]::NewGuid().ToString('N'))
$checksumCases = 0
try {
    [IO.File]::WriteAllText($fixture, 'installer checksum regression fixture')
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $fixture).Hash.ToLower()
    $name = 'zai-v0.24.1-windows-x86_64.zip'
    $script:downloadFails = $false
    foreach ($bytes in @($false, $true)) {
        foreach ($newline in @("`n", "`r`n")) {
            foreach ($case in @(
                @{Body="$hash  other.zip${newline}$hash  $name$newline"; Expected=$true},
                @{Body="$('0' * 64)  $name$newline"; Expected=$false},
                @{Body="$hash  other.zip$newline"; Expected=$false},
                @{Body="invalid-hash  $name$newline"; Expected=$false}
            )) {
                $script:checksumContent = $case.Body
                if ($bytes) { $script:checksumContent = [Text.Encoding]::UTF8.GetBytes($case.Body) }
                Assert ((Test-Checksum $fixture $name 'https://example.invalid/checksums.txt') -eq $case.Expected) "checksum case: bytes=$bytes, expected=$($case.Expected)"
                $checksumCases++
            }
        }
    }
    $script:downloadFails = $true
    Assert ((Test-Checksum $fixture $name 'https://example.invalid/checksums.txt') -eq $false) 'download failure must reject checksum'
    $checksumCases++
} finally {
    Remove-Item -LiteralPath $fixture -Force -ErrorAction SilentlyContinue
    Remove-Item Function:Invoke-WebRequest
}
function rustc {
    $global:LASTEXITCODE = $script:rustExit
    if ($args -contains '--version') { 'rustc 1.88.0 (test)'; return }
    $script:probeDir = Split-Path $args[-1]
    if ($script:rustExit -eq 0) { [IO.File]::WriteAllText($args[-1], 'mock executable') }
}
foreach ($code in @(0, 1)) {
    $script:rustExit = $code
    Assert ((Test-SourceToolchain) -eq ($code -eq 0)) 'link failure must be detected'
    Assert (-not (Test-Path $script:probeDir)) 'probe directory must be cleaned'
}
function Test-SourceToolchain { return $script:ready }
function cargo { $script:cargoCalls++; $script:cargoArgs = $args; $global:LASTEXITCODE = $script:buildExit }
function Test-Path { return $true } # 更新前の zai.exe が存在していても失敗は失敗。
function Sync-Stale {}
function Add-UserPath {}
function Register-App {}
function Register-Firewall {}
function Show-Done { $script:doneCalls++ }
function Copy-Binary($src, $dst) { $script:copiedFrom = $src; $script:copiedTo = $dst; return $script:copyOk }
function New-Item {} # 配置先指定のテストも実ユーザーのパスへ書き込まない。
$cargoBin = [IO.Path]::GetTempPath()
$requiredMinor = 88
$repoUrl = 'https://example.invalid/test'
$script:rustExit = 0
foreach ($case in @(
    @{Ready=$false; Exit=0; Builds=0; Done=0},
    @{Ready=$true; Exit=1; Builds=1; Done=0},
    @{Ready=$true; Exit=0; Builds=1; Done=1}
)) {
    $script:ready = $case.Ready; $script:buildExit = $case.Exit
    $script:cargoCalls = 0; $script:doneCalls = 0
    Assert ((Install-FromSource) -eq ($case.Done -eq 1)) 'source install result'
    Assert ($script:cargoCalls -eq $case.Builds) 'cargo must wait for linker readiness'
    Assert ($script:doneCalls -eq $case.Done) 'failed update must not report success'
}
$previousInstallDir = $env:ZAI_INSTALL_DIR
try {
    $installDir = Join-Path ([IO.Path]::GetTempPath()) "custom ' 日本語"
    $env:ZAI_INSTALL_DIR = $installDir
    foreach ($copyOk in @($false, $true)) {
        $script:ready = $true; $script:buildExit = 0
        $script:copyOk = $copyOk; $script:copiedTo = $null; $script:doneCalls = 0
        Assert ((Install-FromSource) -eq $copyOk) 'custom destination copy failure must fail installation'
        Assert ($script:copiedTo -eq (Join-Path $installDir 'zai.exe')) 'source update must honor the selected destination'
        Assert ($script:doneCalls -eq [int]$copyOk) 'custom update must only report success after copying'
    }
} finally { $env:ZAI_INSTALL_DIR = $previousInstallDir }
$previousUpdateOnly = $env:ZAI_UPDATE_ONLY
try {
    $env:ZAI_UPDATE_ONLY = '1'
    foreach ($destination in @($cargoBin, (Join-Path ([IO.Path]::GetTempPath()) "custom ' 日本語"))) {
        $installDir = $destination
        foreach ($copyOk in @($false, $true)) {
            $script:ready = $true; $script:buildExit = 0; $script:copyOk = $copyOk; $script:doneCalls = 0
            Assert ((Install-FromSource) -eq $copyOk) 'isolated source update must report copy failure'
            $rootIndex = [Array]::IndexOf($script:cargoArgs, '--root')
            Assert ($rootIndex -ge 0) 'source update must install into a temporary cargo root'
            $buildRoot = $script:cargoArgs[$rootIndex + 1]
            Assert ($buildRoot -ne $cargoBin -and $buildRoot -ne $installDir) 'cargo must not install directly into a user destination'
            Assert ($script:copiedFrom -eq (Join-Path $buildRoot 'bin/zai.exe')) 'copy must use the isolated build output'
            Assert ($script:copiedTo -eq (Join-Path $installDir 'zai.exe')) 'copy must target only the selected installation'
            Assert ($script:doneCalls -eq [int]$copyOk) 'failed source copy must not report success'
        }
    }
} finally { $env:ZAI_UPDATE_ONLY = $previousUpdateOnly }
# 実際のエントリーポイントを、配布物取得失敗・明示指定・検証失敗で実行する。
$entry = $ast.EndBlock.Statements[-1].Extent.Text
function Install-Prebuilt {
    $script:prebuiltCalls++
    $script:zaiGiveUp = $script:checksumFailed
    return $script:installOk
}
function Install-FromSource { $script:sourceCalls++; return $script:installOk }
$previousSource = $env:ZAI_FROM_SOURCE
try {
    foreach ($case in @(
        @{Source=''; Checksum=$false; Prebuilt=1; Builds=0},
        @{Source='1'; Checksum=$false; Prebuilt=0; Builds=1},
        @{Source=''; Checksum=$true; Prebuilt=1; Builds=0},
        @{Source=''; Checksum=$false; Prebuilt=1; Builds=0; Ok=$true},
        @{Source='1'; Checksum=$false; Prebuilt=0; Builds=1; Ok=$true}
    )) {
        $env:ZAI_FROM_SOURCE = $case.Source
        $script:checksumFailed = $case.Checksum
        $script:zaiGiveUp = $false
        $script:prebuiltCalls = 0; $script:sourceCalls = 0
        $zaiPrevEap = $ErrorActionPreference
        $script:installOk = $case.Ok -eq $true
        $thrown = $false
        try { Invoke-Expression $entry } catch { $thrown = $true }
        Assert ($thrown -eq (-not $script:installOk)) 'failed install must throw for zai update to return nonzero'
        Assert ($ErrorActionPreference -eq $zaiPrevEap) 'caller preference must be restored'
        Assert ($script:prebuiltCalls -eq $case.Prebuilt) 'prebuilt selection'
        Assert ($script:sourceCalls -eq $case.Builds) 'source builds must be explicit'
    }
} finally { $env:ZAI_FROM_SOURCE = $previousSource }
Write-Host "PASS: syntax, copy/rollback cases, $checksumCases checksum cases and 16 mocked installer scenarios (release download not exercised)"
Write-Host 'PASS: running executable replacement, sharing retries and timeout'
