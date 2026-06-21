# replaykit Windows installer.
#
# Usage:  irm https://raw.githubusercontent.com/aryxnsdfs/replaykit/main/install.ps1 | iex
#
# Downloads the latest GitHub release for x86_64-pc-windows-msvc, extracts the
# binary into %LOCALAPPDATA%\Programs\replaykit, and prepends that directory to
# the current user's PATH (User scope; does not touch system PATH).

$ErrorActionPreference = "Stop"

$Repo    = "aryxnsdfs/replaykit"
$InstallDir = Join-Path $env:LOCALAPPDATA "Programs\replaykit"
$Asset   = "replaykit-x86_64-pc-windows-msvc.zip"

Write-Host ""
Write-Host "replaykit installer" -ForegroundColor Cyan
Write-Host ""

# 1. Find latest release tag.
Write-Host "  resolving latest release..." -ForegroundColor DarkGray
$release = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/releases/latest" `
    -Headers @{ "User-Agent" = "replaykit-installer" }
$tag = $release.tag_name
$assetUrl = ($release.assets | Where-Object { $_.name -eq $Asset }).browser_download_url
if (-not $assetUrl) {
    throw "could not find asset '$Asset' on release $tag"
}
Write-Host "  found $tag" -ForegroundColor DarkGray

# 2. Download into a temp file.
$tmp = New-TemporaryFile
Rename-Item -Path $tmp -NewName ($tmp.Name + ".zip") -Force
$tmpZip = $tmp.FullName + ".zip"
Write-Host "  downloading $Asset ..." -ForegroundColor DarkGray
Invoke-WebRequest -Uri $assetUrl -OutFile $tmpZip -UseBasicParsing

# 3. Extract into install dir.
if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir | Out-Null
}
Write-Host "  extracting into $InstallDir ..." -ForegroundColor DarkGray
Expand-Archive -Path $tmpZip -DestinationPath $InstallDir -Force
Remove-Item $tmpZip -Force

$exe = Join-Path $InstallDir "replaykit.exe"
if (-not (Test-Path $exe)) {
    throw "extraction did not produce $exe"
}

# 4. Add to user PATH (persistent) if missing.
$userPath = [Environment]::GetEnvironmentVariable("Path", "User")
if (-not ($userPath -split ";" | Where-Object { $_ -ieq $InstallDir })) {
    Write-Host "  adding $InstallDir to user PATH ..." -ForegroundColor DarkGray
    $newPath = if ([string]::IsNullOrEmpty($userPath)) { $InstallDir } else { "$InstallDir;$userPath" }
    [Environment]::SetEnvironmentVariable("Path", $newPath, "User")
    # Also patch the current session so the user can run it immediately.
    $env:Path = "$InstallDir;$env:Path"
}

# 5. Verify.
$version = & $exe --version
Write-Host ""
Write-Host "  installed: $version" -ForegroundColor Green
Write-Host "  binary   : $exe" -ForegroundColor DarkGray
Write-Host ""
Write-Host "  Open a new terminal (or restart your shell) so PATH picks up,"
Write-Host "  then try:"
Write-Host "    replaykit run --cassette runs/demo --preset google -- python agent.py" -ForegroundColor Cyan
Write-Host ""
