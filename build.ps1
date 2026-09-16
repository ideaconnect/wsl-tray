# Builds the release exe and stages a copy at the repository root, so the
# running tray instance never locks target\release (Windows refuses to
# overwrite a running exe, which would make the next cargo build fail).
# -Win10 builds the Windows 10 variant (looks for vmmem instead of vmmemWSL).
param([switch]$Win10)
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
$features = if ($Win10) { '--features', 'win10' } else { @() }
cargo build --release @features
Copy-Item target\release\wsl-tray.exe .\wsl-tray.exe -Force
Get-Item .\wsl-tray.exe | Select-Object Name, Length, LastWriteTime
