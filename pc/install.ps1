# Installs soundya-pc: run once from this folder.
#   Double-click install.cmd, or in PowerShell:
#   powershell -ExecutionPolicy Bypass -File install.ps1
# Asks for administrator rights (firewall rule, wake-on-LAN settings).

param([string]$User = $env:USERNAME, [string]$AppData = $env:LOCALAPPDATA)
$ErrorActionPreference = 'Stop'

$admin = ([Security.Principal.WindowsPrincipal][Security.Principal.WindowsIdentity]::GetCurrent()).IsInRole(
    [Security.Principal.WindowsBuiltInRole]::Administrator)
if (-not $admin) {
    # Restart elevated, but install for the user who started it.
    Start-Process powershell.exe -Verb RunAs -ArgumentList @(
        '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', "`"$PSCommandPath`"",
        '-User', $User, '-AppData', "`"$AppData`"")
    exit
}
$Port = 8766
$Dir = Join-Path $AppData 'soundya-pc'
$Task = 'soundya-pc'

Write-Host ''
Write-Host '=== soundya-pc: установка ===' -ForegroundColor Cyan

# 1. Files and key.
New-Item -ItemType Directory -Force -Path $Dir | Out-Null
Copy-Item -Force (Join-Path $PSScriptRoot 'soundya-pc.ps1') (Join-Path $Dir 'soundya-pc.ps1')
$ConfigPath = Join-Path $Dir 'config.json'
if (Test-Path $ConfigPath) {
    $Config = Get-Content $ConfigPath -Raw -Encoding UTF8 | ConvertFrom-Json
} else {
    $bytes = New-Object byte[] 18
    [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
    $key = [Convert]::ToBase64String($bytes).Replace('+', 'x').Replace('/', 'y')
    $Config = [pscustomobject]@{ port = $Port; key = $key; apps = [pscustomobject]@{} }
    [IO.File]::WriteAllText($ConfigPath, ($Config | ConvertTo-Json), (New-Object Text.UTF8Encoding $false))
}
Write-Host "[ok] файлы: $Dir"

# 2. Firewall: only devices of the home network may connect.
Get-NetFirewallRule -DisplayName $Task -ErrorAction SilentlyContinue | Remove-NetFirewallRule
New-NetFirewallRule -DisplayName $Task -Direction Inbound -Protocol TCP -LocalPort $Config.port `
    -Action Allow -Profile Any -RemoteAddress LocalSubnet | Out-Null
Write-Host "[ok] брандмауэр: порт $($Config.port) открыт для домашней сети"

# 3. Start in the background at every logon, as the normal user (not as
# administrator: Steam and others refuse to be started by an elevated program).
# Uses the Startup folder; the Task Scheduler refused to register a task on some PCs.
$script = Join-Path $Dir 'soundya-pc.ps1'
$startup = Join-Path (Split-Path $AppData) 'Roaming\Microsoft\Windows\Start Menu\Programs\Startup'
New-Item -ItemType Directory -Force -Path $startup | Out-Null
$vbs = Join-Path $startup 'soundya-pc.vbs'
# Hidden window; started again a few seconds after it stops for any reason.
@(
    'Set sh = CreateObject("WScript.Shell")',
    'Do',
    ('    sh.Run "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File ""' + $script + '""", 0, True'),
    '    WScript.Sleep 5000',
    'Loop'
) | Set-Content -Path $vbs -Encoding Unicode

# Remove the old scheduled task, if an earlier version made one.
Unregister-ScheduledTask -TaskName $Task -Confirm:$false -ErrorAction SilentlyContinue
cmd.exe /c "schtasks /Delete /TN $Task /F >nul 2>&1"

# Stop running copies (also ones started by hand as administrator), then start
# it through Explorer, which runs it as the normal user.
Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe' OR Name = 'wscript.exe'" -ErrorAction SilentlyContinue |
    Where-Object { $_.ProcessId -ne $PID -and ($_.CommandLine -like '*soundya-pc.ps1*' -or $_.CommandLine -like '*soundya-pc.vbs*') } |
    ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
Start-Sleep -Seconds 1
Start-Process explorer.exe -ArgumentList "`"$vbs`""
Write-Host "[ok] автозапуск при входе в Windows (папка «Автозагрузка»), уже запущен"

# 4. Wake-on-LAN: wired adapter wakes on a magic packet; fast startup off
# (with it on, most PCs ignore wake-on-LAN after "Shut down").
$wired = Get-NetAdapter -Physical -ErrorAction SilentlyContinue |
    Where-Object { $_.Status -eq 'Up' -and $_.MediaType -eq '802.3' } | Select-Object -First 1
if ($wired) {
    try { Set-NetAdapterPowerManagement -Name $wired.Name -WakeOnMagicPacket Enabled -ErrorAction Stop } catch {}
    foreach ($p in 'Wake on Magic Packet', '*WakeOnMagicPacket', 'Wake on magic packet') {
        try { Set-NetAdapterAdvancedProperty -Name $wired.Name -DisplayName $p -DisplayValue 'Enabled' -ErrorAction Stop } catch {}
        try { Set-NetAdapterAdvancedProperty -Name $wired.Name -RegistryKeyword $p -RegistryValue 1 -ErrorAction Stop } catch {}
    }
    Set-ItemProperty -Path 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -Value 0
    Write-Host "[ok] включение по сети: сетевая карта «$($wired.InterfaceDescription)», быстрый запуск выключен"
    $mac = $wired.MacAddress.Replace('-', ':')
    $ip = (Get-NetIPAddress -InterfaceIndex $wired.ifIndex -AddressFamily IPv4 -ErrorAction SilentlyContinue | Select-Object -First 1).IPAddress
} else {
    Write-Host '[!] кабель не подключён: включать компьютер по сети не получится (только по проводу).' -ForegroundColor Yellow
    $mac = ''
    $up = Get-NetAdapter -Physical | Where-Object Status -eq 'Up' | Select-Object -First 1
    $ip = if ($up) { (Get-NetIPAddress -InterfaceIndex $up.ifIndex -AddressFamily IPv4 | Select-Object -First 1).IPAddress } else { '' }
}

Write-Host ''
Write-Host 'Готово. Откройте http://soundya.local:8765 -> Настройки -> Компьютер и впишите:' -ForegroundColor Green
Write-Host "  Адрес компьютера:  $ip"
Write-Host "  Ключ:              $($Config.key)"
if ($mac) { Write-Host "  MAC-адрес:         $mac" }
Write-Host ''
Write-Host 'Ещё для включения по сети: в BIOS/UEFI включите Wake on LAN (Power On By PCI-E / LAN),'
Write-Host 'а в роутере закрепите за компьютером IP-адрес.'
Write-Host ''
Read-Host 'Нажмите Enter, чтобы закрыть'
