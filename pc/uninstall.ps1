# Removes soundya-pc: the background task, the firewall rule and its folder.
param([string]$AppData = $env:LOCALAPPDATA)

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    Start-Process powershell.exe -Verb RunAs -ArgumentList @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"", '-AppData', "`"$AppData`"")
    exit
}

Stop-ScheduledTask -TaskName 'soundya-pc' -ErrorAction SilentlyContinue
Unregister-ScheduledTask -TaskName 'soundya-pc' -Confirm:$false -ErrorAction SilentlyContinue
Get-NetFirewallRule -DisplayName 'soundya-pc' -ErrorAction SilentlyContinue | Remove-NetFirewallRule
Remove-Item (Join-Path (Split-Path $AppData) 'Roaming\Microsoft\Windows\Start Menu\Programs\Startup\soundya-pc.vbs') -Force -ErrorAction SilentlyContinue
Remove-Item (Join-Path (Split-Path $AppData) 'Roaming\Microsoft\Windows\Start Menu\Programs\Startup\soundya-pc-tray.vbs') -Force -ErrorAction SilentlyContinue
Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe' OR Name = 'wscript.exe'" |
    Where-Object { $_.CommandLine -like '*soundya-pc.ps1*' -or $_.CommandLine -like '*soundya-pc.vbs*' -or $_.CommandLine -like '*soundya-pc-tray*' } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Remove-Item -Recurse -Force (Join-Path $AppData 'soundya-pc') -ErrorAction SilentlyContinue
Write-Host 'soundya-pc удалён.'
Read-Host 'Нажмите Enter, чтобы закрыть'
