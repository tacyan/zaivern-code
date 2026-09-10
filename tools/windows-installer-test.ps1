param([string]$Installer = (Join-Path $PSScriptRoot '../install.ps1'))
$ErrorActionPreference = 'Stop'
$errors = $null
$ast = [System.Management.Automation.Language.Parser]::ParseFile(
    $Installer, [ref]$null, [ref]$errors)
if ($errors.Count) { throw ($errors | Out-String) }
# 定義だけ読み込み、実ユーザーへのインストールやネットワーク通信は行わない。
foreach ($name in @('Test-Checksum', 'Test-SourceToolchain', 'Install-FromSource')) {
    $definition = $ast.Find({ param($node)
        $node -is [System.Management.Automation.Language.FunctionDefinitionAst] -and $node.Name -eq $name
    }, $true)
    Invoke-Expression $definition.Extent.Text
}
function Assert($ok, $message) { if (-not $ok) { throw $message } }
function Say($msg) {}
function Warn($msg) {}
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
function cargo { $script:cargoCalls++; $global:LASTEXITCODE = $script:buildExit }
function Test-Path { return $true } # 更新前の zai.exe が存在していても失敗は失敗。
function Sync-Stale {}
function Add-UserPath {}
function Register-App {}
function Register-Firewall {}
function Show-Done { $script:doneCalls++ }
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
# 実際のエントリーポイントを、配布物取得失敗・明示指定・検証失敗で実行する。
$entry = $ast.EndBlock.Statements[-1].Extent.Text
function Install-Prebuilt {
    $script:prebuiltCalls++
    $script:zaiGiveUp = $script:checksumFailed
    return $false
}
function Install-FromSource { $script:sourceCalls++; return $false }
$previousSource = $env:ZAI_FROM_SOURCE
try {
    foreach ($case in @(
        @{Source=''; Checksum=$false; Prebuilt=1; Builds=0},
        @{Source='1'; Checksum=$false; Prebuilt=0; Builds=1},
        @{Source=''; Checksum=$true; Prebuilt=1; Builds=0}
    )) {
        $env:ZAI_FROM_SOURCE = $case.Source
        $script:checksumFailed = $case.Checksum
        $script:zaiGiveUp = $false
        $script:prebuiltCalls = 0; $script:sourceCalls = 0
        $zaiPrevEap = $ErrorActionPreference
        Invoke-Expression $entry
        Assert ($script:prebuiltCalls -eq $case.Prebuilt) 'prebuilt selection'
        Assert ($script:sourceCalls -eq $case.Builds) 'source builds must be explicit'
    }
} finally { $env:ZAI_FROM_SOURCE = $previousSource }
Write-Host "PASS: syntax, $checksumCases checksum cases and 8 mocked installer scenarios (Windows installation not exercised)"
