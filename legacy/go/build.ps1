# Builds wsl-tray.exe (windowed, stripped). Run from the project directory.
$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot
go vet ./...
go build -ldflags="-s -w -H windowsgui" -o wsl-tray.exe .
Get-Item wsl-tray.exe | Select-Object Name, Length, LastWriteTime
