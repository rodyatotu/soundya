# Ставит (или обновляет) soundya на Яндекс Станцию с компьютера на Windows.
# Колонка уже должна быть с root и с adb по сети (см. README, шаг 1).
# Запуск: двойной щелчок по install-windows.cmd в корне проекта.
#
# Что делает: находит или скачивает adb, ищет колонку в домашней сети, берёт
# сборку (dist\soundya-armv7.tar рядом или последний релиз на GitHub),
# загружает её на колонку, устанавливает и открывает веб-панель.
param([string]$Speaker = '')

$ErrorActionPreference = 'Stop'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$Repo = 'rodyatotu/soundya'
$Root = Split-Path -Parent $PSScriptRoot
$Tools = Join-Path $env:LOCALAPPDATA 'soundya-tools'
$Work = Join-Path $env:TEMP 'soundya-install'
New-Item -ItemType Directory -Force -Path $Tools, $Work | Out-Null

function Step($n, $text) { Write-Host ''; Write-Host "[$n/5] $text" -ForegroundColor Cyan }
function Ok($text) { Write-Host "  [ok] $text" -ForegroundColor Green }
function Warn($text) { Write-Host "  [!] $text" -ForegroundColor Yellow }
function Fail($text) {
    Write-Host ''
    Write-Host "  [x] $text" -ForegroundColor Red
    Write-Host ''
    Read-Host 'Нажмите Enter, чтобы закрыть' | Out-Null
    exit 1
}

Write-Host '=== soundya: установка на колонку ===' -ForegroundColor Cyan

# ---- 1. adb ----
Step 1 'adb — программа для связи с колонкой'
$Adb = $null
$cmd = Get-Command adb.exe -ErrorAction SilentlyContinue
if ($cmd) { $Adb = $cmd.Source }
$local = Join-Path $Tools 'platform-tools\adb.exe'
if (-not $Adb -and (Test-Path $local)) { $Adb = $local }
if (-not $Adb) {
    Write-Host '  Скачиваю Android platform-tools с сайта Google…'
    $zip = Join-Path $Work 'platform-tools.zip'
    try {
        Invoke-WebRequest 'https://dl.google.com/android/repository/platform-tools-latest-windows.zip' -OutFile $zip -UseBasicParsing
        Expand-Archive $zip -DestinationPath $Tools -Force
    } catch { Fail "Не удалось скачать adb: $_" }
    $Adb = $local
}
Ok $Adb

function Run-Adb {
    # Returns output as one string; never throws on adb's own errors.
    $old = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    $out = & $Adb @args 2>&1 | Out-String
    $ErrorActionPreference = $old
    $out.Trim()
}

# ---- 2. Колонка ----
Step 2 'Ищу колонку в домашней сети'

function Find-Speakers {
    $mine = @(Get-NetIPAddress -AddressFamily IPv4 -ErrorAction SilentlyContinue | Where-Object {
        $_.IPAddress -match '^(192\.168\.|10\.|172\.(1[6-9]|2\d|3[01])\.)' -and
        $_.InterfaceAlias -notmatch 'vEthernet|VPN|Amnezia|WireGuard|Loopback|VirtualBox|VMware|Hyper-V|Tailscale|ZeroTier'
    })
    $found = @()
    foreach ($a in $mine) {
        $net = ($a.IPAddress -split '\.')[0..2] -join '.'
        Write-Host "  сеть $net.0/24…"
        $probes = @()
        foreach ($i in 1..254) {
            $ip = "$net.$i"
            if ($ip -eq $a.IPAddress) { continue }
            $c = New-Object System.Net.Sockets.TcpClient
            $probes += [pscustomobject]@{ Ip = $ip; Client = $c; Task = $c.ConnectAsync($ip, 5555) }
        }
        $tasks = [System.Threading.Tasks.Task[]]@($probes | ForEach-Object { $_.Task })
        try { [System.Threading.Tasks.Task]::WaitAll($tasks, 1500) | Out-Null } catch {}
        foreach ($p in $probes) {
            if ($p.Task.Status -eq 'RanToCompletion' -and $p.Client.Connected) { $found += $p.Ip }
            try { $p.Client.Close() } catch {}
        }
    }
    $found | Select-Object -Unique
}

$ip = $Speaker
if (-not $ip) {
    $list = @(Find-Speakers)
    if ($list.Count -eq 1) {
        $ip = $list[0]
    } elseif ($list.Count -gt 1) {
        Write-Host '  Нашла несколько устройств с adb по сети:'
        for ($i = 0; $i -lt $list.Count; $i++) { Write-Host "    $($i + 1). $($list[$i])" }
        $n = Read-Host '  Номер колонки'
        $ip = $list[[int]$n - 1]
    } else {
        Warn 'Сама не нашла. Посмотрите IP колонки в списке устройств роутера.'
        $ip = (Read-Host '  IP колонки (например 192.168.0.112)').Trim()
    }
}
if (-not $ip) { Fail 'Не указан IP колонки' }
$dev = "${ip}:5555"

Run-Adb disconnect $dev | Out-Null
$c = Run-Adb connect $dev
if ($c -notmatch 'connected to') { Fail "Не подключилась к $dev ($c). Колонка включена, adb по сети включён?" }
$id = Run-Adb -s $dev shell id
if ($id -notmatch 'uid=0') { Fail "Колонка ответила, но без root ($id). Сначала шаг 1 из README." }
Ok "колонка $ip, root есть"

# ---- 3. Сборка ----
Step 3 'Сборка soundya'
$tar = Join-Path $Root 'dist\soundya-armv7.tar'
if (Test-Path $tar) {
    Ok "беру собранную рядом: $tar"
} else {
    try {
        $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest" -UseBasicParsing -Headers @{ 'User-Agent' = 'soundya-installer' }
        $asset = $rel.assets | Where-Object { $_.name -eq 'soundya-armv7.tar' } | Select-Object -First 1
        if (-not $asset) { throw 'в релизе нет soundya-armv7.tar' }
        $tar = Join-Path $Work 'soundya-armv7.tar'
        Write-Host "  Скачиваю релиз $($rel.tag_name)…"
        Invoke-WebRequest $asset.browser_download_url -OutFile $tar -UseBasicParsing
        Ok "релиз $($rel.tag_name)"
    } catch {
        Fail "Не нашла сборку: ни dist\soundya-armv7.tar рядом, ни релиза на GitHub ($_)."
    }
}
$size = (Get-Item $tar).Length

# ---- 4. Установка ----
Step 4 'Загружаю и устанавливаю'
$uploaded = $false
for ($try = 1; $try -le 5 -and -not $uploaded; $try++) {
    Run-Adb connect $dev | Out-Null
    Run-Adb -s $dev push $tar /tmp/soundya-armv7.tar | Out-Null
    $remote = (Run-Adb -s $dev shell 'wc -c < /tmp/soundya-armv7.tar') -replace '\D', ''
    if ($remote -eq "$size") { $uploaded = $true } else { Warn "связь оборвалась, попытка $($try + 1)…"; Start-Sleep 3 }
}
if (-not $uploaded) { Fail 'Не удалось загрузить сборку на колонку: связь всё время обрывается.' }
Ok 'загружено'

# Runs on the speaker by itself, so a dropped connection cannot stop it halfway.
$start = "cd /tmp && rm -rf soundya /tmp/soundya-install.done /tmp/soundya-install.log && tar -xf soundya-armv7.tar && " +
    "nohup setsid sh -c 'sh /tmp/soundya/install.sh > /tmp/soundya-install.log 2>&1; touch /tmp/soundya-install.done' " +
    "</dev/null >/dev/null 2>&1 & sleep 1; [ -f /tmp/soundya-install.log ] && echo started"
$started = $false
for ($try = 1; $try -le 5 -and -not $started; $try++) {
    Run-Adb connect $dev | Out-Null
    if ((Run-Adb -s $dev shell $start) -match 'started') { $started = $true } else { Start-Sleep 3 }
}
if (-not $started) { Fail 'Не удалось запустить установку на колонке.' }

$log = ''
for ($i = 0; $i -lt 30; $i++) {
    Start-Sleep 2
    Run-Adb connect $dev | Out-Null
    $log = Run-Adb -s $dev shell '[ -f /tmp/soundya-install.done ] && cat /tmp/soundya-install.log'
    if ($log -match 'Installed') { break }
}
if ($log -notmatch 'Installed') { Fail 'Установка не подтвердилась. Посмотрите: adb shell cat /tmp/soundya-install.log' }
Ok 'установлено'

# ---- 5. Готово ----
Step 5 'Готово'
$url = "http://${ip}:8765/"
Write-Host "  Веб-панель: $url  (или http://soundya.local:8765/)"
Write-Host '  Скажите: «Алиса, включи рекомендации».'
Write-Host ''
Write-Host '  Совет: закрепите за колонкой IP в настройках роутера, чтобы адрес не менялся.'
Start-Process $url
Write-Host ''
Read-Host 'Нажмите Enter, чтобы закрыть' | Out-Null
