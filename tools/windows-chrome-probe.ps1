$ErrorActionPreference = 'Stop'
$chrome = @("$env:ProgramFiles/Google/Chrome/Application/chrome.exe", "${env:ProgramFiles(x86)}/Google/Chrome/Application/chrome.exe") | Where-Object { Test-Path $_ } | Select-Object -First 1
if (-not $chrome) { throw 'Chrome not found' }
Write-Host "Chrome: $chrome $((Get-Item $chrome).VersionInfo.ProductVersion)"
$root = Join-Path ([IO.Path]::GetTempPath()) ('zai-chrome-probe-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory $root | Out-Null
try {
    $page = Join-Path $root 'index.html'
    [IO.File]::WriteAllText($page, '<html><body style="margin:0"><div style="width:100vw;height:40px;background:red"></div></body></html>')
    $url = 'file:///' + $page.Replace('\', '/')
    $wrap = Join-Path $root 'wrap.html'
    [IO.File]::WriteAllText($wrap, "<html><body style='margin:0'><iframe src='$url' style='border:0;display:block;width:375px;height:812px'></iframe></body></html>")
    foreach ($mode in @('direct', 'wrapper', 'wrapper-created', 'wrapper-log')) {
        $profile = Join-Path $root $mode
        if ($mode -in @('wrapper-created', 'wrapper-log')) { New-Item -ItemType Directory $profile | Out-Null }
        $target = if ($mode -eq 'direct') { $url } else { 'file:///' + $wrap.Replace('\', '/') }
        $png = Join-Path $root "$mode.png"
        $log = Join-Path $root "$mode.log"
        $stdout = Join-Path $root "$mode.stdout"
        $stderr = Join-Path $root "$mode.stderr"
        $chromeArgs = @('--headless=new', '--disable-gpu', '--no-first-run', '--no-default-browser-check', '--disable-extensions', '--allow-file-access-from-files', '--hide-scrollbars', "--user-data-dir=$profile", '--window-size=500,812', '--virtual-time-budget=4000', "--screenshot=$png", $target)
        if ($mode -eq 'wrapper-log') { $chromeArgs += @('--enable-logging', "--log-file=$log", '--v=0') }
        $process = Start-Process -FilePath $chrome -ArgumentList ($chromeArgs | ForEach-Object { '"' + $_ + '"' }) -RedirectStandardOutput $stdout -RedirectStandardError $stderr -PassThru
        $watch = [Diagnostics.Stopwatch]::StartNew()
        try {
            while ($watch.Elapsed.TotalSeconds -lt 25 -and -not $process.HasExited -and -not (Test-Path $png)) { Start-Sleep -Milliseconds 100 }
            Write-Host "MODE=$mode elapsed=$($watch.Elapsed.TotalSeconds) exited=$($process.HasExited) png=$(Test-Path $png)"
            if (Test-Path $png) { Write-Host "PNG bytes=$((Get-Item $png).Length)" }
        } finally {
            if (-not $process.HasExited) { & taskkill /PID $process.Id /T /F | Out-Host }
            $process.Dispose()
        }
        foreach ($file in @($stdout, $stderr, $log)) {
            if (Test-Path $file) { Write-Host "LOG $file"; Get-Content $file -Tail 30 }
        }
    }
} finally { Remove-Item -LiteralPath $root -Recurse -Force -ErrorAction SilentlyContinue }
