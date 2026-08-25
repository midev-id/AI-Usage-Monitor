# Builds the release binary in the current directory (a worktree or the main
# repo) and copies it to the main repo folder so there is one stable location.
#
# Run this from the worktree you want to build:
#   .\..\..\build.ps1            # build + copy
#   .\..\..\build.ps1 -SkipBuild # copy only (reuse existing build)

param(
    [switch]$SkipBuild,
    [switch]$NoRestart
)

$ErrorActionPreference = "Stop"

$mainRepo = "C:\Users\MAHDI\Documents\Autopreneur\Claude-Code-Usage-Monitor"
$buildRoot = (Get-Location).Path
$source = Join-Path $buildRoot "target\release\claude-code-usage-monitor.exe"
$destination = Join-Path $mainRepo "claude-code-usage-monitor.exe"

if (-not $SkipBuild) {
    Write-Host "Building release in: $buildRoot"
    cargo build --release
    if ($LASTEXITCODE -ne 0) { throw "cargo build failed with exit code $LASTEXITCODE" }
}

if (-not (Test-Path -LiteralPath $source)) {
    throw "Build output not found: $source"
}

Copy-Item -LiteralPath $source -Destination $destination -Force
$item = Get-Item -LiteralPath $destination
Write-Host "Copied to: $destination"
Write-Host ("  Size: {0:N1} MB  Time: {1}" -f ($item.Length / 1MB), $item.LastWriteTime.ToString("yyyy-MM-dd HH:mm:ss"))

if (-not $NoRestart) {
    Write-Host "Restarting claude-code-usage-monitor..."
    Get-Process claude-code-usage-monitor -ErrorAction SilentlyContinue | Stop-Process -Force
    Start-Sleep -Milliseconds 500
    Start-Process -FilePath $destination
    Write-Host "Started: $destination"
}
