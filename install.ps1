# Zaivern Code ワンライナーインストーラ (Windows)
#   irm https://raw.githubusercontent.com/tacyan/zaivern-code/main/install.ps1 | iex
#
# GitHub Releases のビルド済み zai.exe を %LOCALAPPDATA%\Zaivern\bin へ配置し、
# ユーザー PATH に追加します。
$ErrorActionPreference = "Stop"

$repo = "tacyan/zaivern-code"
$dir = Join-Path $env:LOCALAPPDATA "Zaivern\bin"

Write-Host "[zaivern-code] 最新リリースを確認しています..." -ForegroundColor Cyan
$tag = (Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest").tag_name
$name = "zai-$tag-windows-x86_64"
$url = "https://github.com/$repo/releases/download/$tag/$name.zip"

$zip = Join-Path $env:TEMP "$name.zip"
$extract = Join-Path $env:TEMP "zai-extract"
Write-Host "[zaivern-code] ダウンロード: $url" -ForegroundColor Cyan
Invoke-WebRequest $url -OutFile $zip
if (Test-Path $extract) { Remove-Item $extract -Recurse -Force }
Expand-Archive $zip -DestinationPath $extract -Force

New-Item -ItemType Directory -Force -Path $dir | Out-Null
Copy-Item (Join-Path $extract "$name\zai.exe") (Join-Path $dir "zai.exe") -Force
Remove-Item $zip -Force
Remove-Item $extract -Recurse -Force

# ユーザー PATH へ追加 (未登録の場合のみ)
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (($userPath -split ";") -notcontains $dir) {
    [Environment]::SetEnvironmentVariable("Path", "$userPath;$dir", "User")
    Write-Host "[zaivern-code] PATH に $dir を追加しました (新しいターミナルから有効)" -ForegroundColor Yellow
}

Write-Host ""
Write-Host "[zaivern-code] ✅ インストール完了: $dir\zai.exe ($tag)" -ForegroundColor Green
Write-Host "[zaivern-code]    起動: zai [ワークスペースのパス]"
