# Builds the release exe and stages a copy at the repository root, so the
# running tray instance never locks target\release (Windows refuses to
# overwrite a running exe, which would make the next cargo build fail).
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
cargo build --release
Copy-Item target\release\wsl-tray.exe .\wsl-tray.exe -Force
Get-Item .\wsl-tray.exe | Select-Object Name, Length, LastWriteTime
