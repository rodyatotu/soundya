# soundya-pc: lets the Yandex Station (soundya) open and close programs, put the PC
# to sleep, shut it down, restart and lock it. Runs in the background after logon.
#
# Protocol: POST http://<pc>:8766/<action> with header "X-Soundya-Key: <key>",
# body = program name (UTF-8) for open/close. Actions: ping, open, close, sleep,
# shutdown, restart, lock. The answer is a short Russian sentence that Alice says.
#
# Settings: %LOCALAPPDATA%\soundya-pc\config.json (created by install.ps1):
#   { "port": 8766, "key": "...", "apps": { "моя игра": "C:\\Games\\game.exe" } }
# "apps" adds your own names: spoken name -> program name, shortcut or .exe path.

$ErrorActionPreference = 'Stop'
$Dir = Join-Path $env:LOCALAPPDATA 'soundya-pc'
$LogFile = Join-Path $Dir 'log.txt'

function Log($text) {
    try {
        if ((Test-Path $LogFile) -and (Get-Item $LogFile).Length -gt 256KB) { Remove-Item $LogFile }
        Add-Content -Path $LogFile -Value ("{0:yyyy-MM-dd HH:mm:ss} {1}" -f (Get-Date), $text) -Encoding UTF8
    } catch {}
}

$Config = Get-Content (Join-Path $Dir 'config.json') -Raw -Encoding UTF8 | ConvertFrom-Json
$Port = if ($Config.port) { [int]$Config.port } else { 8766 }
$Key = [string]$Config.key
if ($Key.Length -lt 16) { Log 'no key in config.json; run install.ps1'; exit 1 }

# Spoken (Russian) names of common programs -> the name to look for in the Start menu.
$Aliases = @{
    'телеграм' = 'Telegram'; 'телеграмм' = 'Telegram'; 'телега' = 'Telegram'
    'хром' = 'Google Chrome'; 'гугл хром' = 'Google Chrome'; 'гугл' = 'Google Chrome'; 'браузер' = 'Google Chrome'
    'яндекс браузер' = 'Yandex'; 'яндекс' = 'Yandex'; 'опера' = 'Opera'; 'опера джи экс' = 'Opera GX'
    'файрфокс' = 'Firefox'; 'фаерфокс' = 'Firefox'; 'эдж' = 'Microsoft Edge'
    'стим' = 'Steam'; 'дискорд' = 'Discord'; 'спотифай' = 'Spotify'; 'зум' = 'Zoom'
    'эпик' = 'Epic Games Launcher'; 'эпик геймс' = 'Epic Games Launcher'; 'батл нет' = 'Battle.net'
    'обс' = 'OBS Studio'; 'ватсап' = 'WhatsApp'; 'вотсап' = 'WhatsApp'; 'скайп' = 'Skype'
    'ворд' = 'Word'; 'эксель' = 'Excel'; 'пауэрпоинт' = 'PowerPoint'; 'аутлук' = 'Outlook'
    'вс код' = 'Visual Studio Code'; 'визуал студио код' = 'Visual Studio Code'; 'вскод' = 'Visual Studio Code'
    'блокнот' = 'notepad'; 'калькулятор' = 'calc'; 'проводник' = 'explorer'; 'паинт' = 'mspaint'
    'диспетчер задач' = 'taskmgr'; 'параметры' = 'ms-settings:'; 'настройки' = 'ms-settings:'
    'командная строка' = 'cmd'; 'терминал' = 'wt'; 'вк' = 'VK Messenger'; 'кс' = 'Counter-Strike 2'
    'контра' = 'Counter-Strike 2'; 'дота' = 'Dota 2'; 'майнкрафт' = 'Minecraft'
}
if ($Config.apps) {
    foreach ($p in $Config.apps.PSObject.Properties) { $Aliases[$p.Name.ToLower()] = [string]$p.Value }
}

$Translit = @{
    'а'='a';'б'='b';'в'='v';'г'='g';'д'='d';'е'='e';'ё'='e';'ж'='zh';'з'='z';'и'='i';'й'='y';'к'='k';'л'='l'
    'м'='m';'н'='n';'о'='o';'п'='p';'р'='r';'с'='s';'т'='t';'у'='u';'ф'='f';'х'='h';'ц'='c';'ч'='ch';'ш'='sh'
    'щ'='sch';'ъ'='';'ы'='y';'ь'='';'э'='e';'ю'='yu';'я'='ya'
}
function To-Latin([string]$s) {
    -join ($s.ToCharArray() | ForEach-Object { $c = [string]$_; if ($Translit.ContainsKey($c)) { $Translit[$c] } else { $c } })
}

function Distance([string]$a, [string]$b) {
    # Levenshtein distance on one flat array (row i, column j at i * $w + j).
    $w = $b.Length + 1
    $d = New-Object 'int[]' (($a.Length + 1) * $w)
    for ($i = 0; $i -le $a.Length; $i++) { $d[$i * $w] = $i }
    for ($j = 0; $j -le $b.Length; $j++) { $d[$j] = $j }
    for ($i = 1; $i -le $a.Length; $i++) {
        for ($j = 1; $j -le $b.Length; $j++) {
            $cost = 1
            if ($a[$i - 1] -eq $b[$j - 1]) { $cost = 0 }
            $up = $d[($i - 1) * $w + $j] + 1
            $left = $d[$i * $w + $j - 1] + 1
            $diag = $d[($i - 1) * $w + $j - 1] + $cost
            $best = $up
            if ($left -lt $best) { $best = $left }
            if ($diag -lt $best) { $best = $diag }
            $d[$i * $w + $j] = $best
        }
    }
    $d[$a.Length * $w + $b.Length]
}

# Start-menu and desktop shortcuts: @{ Name; Path }.
function Shortcuts {
    $dirs = @(
        (Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs'),
        (Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs'),
        [Environment]::GetFolderPath('Desktop'),
        (Join-Path $env:PUBLIC 'Desktop')
    )
    foreach ($d in $dirs) {
        if (-not (Test-Path $d)) { continue }
        Get-ChildItem -Path $d -Recurse -Include *.lnk, *.url -ErrorAction SilentlyContinue |
            Where-Object { $_.BaseName -notmatch 'uninstall|удал|readme|help|справк' } |
            ForEach-Object { [pscustomobject]@{ Name = $_.BaseName; Path = $_.FullName } }
    }
}

# Finds what to start for a spoken name. Returns @{ Name; Path } or $null.
function Find-App([string]$spoken) {
    $spoken = $spoken.Trim().ToLower()
    $target = if ($Aliases.ContainsKey($spoken)) { $Aliases[$spoken] } else { $spoken }
    if ($target -match '[\\/]' -and (Test-Path $target)) {
        return [pscustomobject]@{ Name = [IO.Path]::GetFileNameWithoutExtension($target); Path = $target }
    }
    $wanted = @($target.ToLower())
    if ($target -match '[а-я]') { $wanted += (To-Latin $target.ToLower()) }
    $all = @(Shortcuts)
    foreach ($w in $wanted) {
        $exact = $all | Where-Object { $_.Name.ToLower() -eq $w } | Select-Object -First 1
        if ($exact) { return $exact }
        $starts = $all | Where-Object { $_.Name.ToLower().StartsWith($w) } | Sort-Object { $_.Name.Length } | Select-Object -First 1
        if ($starts) { return $starts }
        $has = $all | Where-Object { $_.Name.ToLower().Contains($w) } | Sort-Object { $_.Name.Length } | Select-Object -First 1
        if ($has) { return $has }
    }
    # Store apps (Windows 10/11).
    try {
        $apps = @(Get-StartApps)
        foreach ($w in $wanted) {
            $app = $apps | Where-Object { $_.Name.ToLower().Contains($w) } | Sort-Object { $_.Name.Length } | Select-Object -First 1
            if ($app) { return [pscustomobject]@{ Name = $app.Name; Path = "shell:AppsFolder\$($app.AppID)" } }
        }
    } catch {}
    # Close spelling: «дискорд» -> «diskord» ~ «Discord».
    foreach ($w in $wanted) {
        if ($w.Length -lt 4) { continue }
        $best = $null; $bestD = 3
        foreach ($s in $all) {
            $first = ($s.Name.ToLower() -split '\s+')[0]
            $dist = [Math]::Min((Distance $w $s.Name.ToLower()), (Distance $w $first))
            if ($dist -lt $bestD) { $best = $s; $bestD = $dist }
        }
        if ($best) { return $best }
    }
    # Built-in programs: notepad, calc, explorer, ms-settings:, …
    if ($target -notmatch '[а-я\s]') { return [pscustomobject]@{ Name = $target; Path = $target } }
    $null
}

# Process names that belong to an app, from its shortcut's target .exe.
function Process-Names($app) {
    $names = @()
    if ($app.Path -like '*.lnk') {
        try {
            $exe = (New-Object -ComObject WScript.Shell).CreateShortcut($app.Path).TargetPath
            if ($exe) { $names += [IO.Path]::GetFileNameWithoutExtension($exe) }
        } catch {}
    } elseif ($app.Path -like '*.exe') {
        $names += [IO.Path]::GetFileNameWithoutExtension($app.Path)
    }
    $names += ($app.Name -split '\s+')[0]
    $names | Where-Object { $_ } | Select-Object -Unique
}

function Do-Open([string]$name) {
    if (-not $name) { return 'Скажите, что открыть' }
    $app = Find-App $name
    if (-not $app) { return "Не нашла на компьютере программу $name" }
    try {
        Start-Process -FilePath $app.Path
        Log "open '$name' -> $($app.Path)"
        "Открываю $($app.Name)"
    } catch {
        Log "open '$name' failed: $_"
        "Не получилось открыть $($app.Name)"
    }
}

function Do-Close([string]$name) {
    if (-not $name) { return 'Скажите, что закрыть' }
    $app = Find-App $name
    $names = if ($app) { @(Process-Names $app) } else { @((To-Latin $name.ToLower())) }
    $procs = @(Get-Process | Where-Object {
        $p = $_
        $names | Where-Object { $p.ProcessName -like "$_*" }
    })
    if ($procs.Count -eq 0) { return "$name сейчас не запущен" }
    foreach ($p in $procs) { try { [void]$p.CloseMainWindow() } catch {} }
    Start-Sleep -Seconds 2
    foreach ($p in $procs) { try { if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force } } catch {} }
    Log "close '$name' -> $($procs.Count) processes"
    $title = if ($app) { $app.Name } else { $name }
    "Закрыла $title"
}

# Power actions run a moment later, so the answer reaches the speaker first.
function Later([scriptblock]$action) {
    Start-Job -ScriptBlock $action | Out-Null
}

function Handle([string]$action, [string]$body) {
    switch ($action) {
        'ping' { "Компьютер $env:COMPUTERNAME на связи" }
        'open' { Do-Open $body }
        'close' { Do-Close $body }
        'lock' { Start-Process rundll32.exe 'user32.dll,LockWorkStation'; 'Заблокировала компьютер' }
        'sleep' {
            Later { Start-Sleep -Seconds 2; Add-Type -AssemblyName System.Windows.Forms; [System.Windows.Forms.Application]::SetSuspendState('Suspend', $false, $false) | Out-Null }
            'Усыпляю компьютер'
        }
        'shutdown' { Start-Process shutdown.exe '/s /t 3'; 'Выключаю компьютер' }
        'restart' { Start-Process shutdown.exe '/r /t 3'; 'Перезагружаю компьютер' }
        default { $null }
    }
}

function Read-Request($stream) {
    $head = New-Object System.Collections.Generic.List[byte]
    while ($head.Count -lt 8192) {
        $b = $stream.ReadByte()
        if ($b -lt 0) { return $null }
        $head.Add([byte]$b)
        $n = $head.Count
        if ($n -ge 4 -and $head[$n - 4] -eq 13 -and $head[$n - 3] -eq 10 -and $head[$n - 2] -eq 13 -and $head[$n - 1] -eq 10) { break }
    }
    $lines = [Text.Encoding]::ASCII.GetString($head.ToArray()) -split "`r`n"
    $parts = $lines[0] -split ' '
    $headers = @{}
    foreach ($l in $lines[1..($lines.Count - 1)]) {
        $i = $l.IndexOf(':')
        if ($i -gt 0) { $headers[$l.Substring(0, $i).Trim().ToLower()] = $l.Substring($i + 1).Trim() }
    }
    $len = 0
    if ($headers['content-length']) { $len = [Math]::Min([int]$headers['content-length'], 4096) }
    $buf = New-Object byte[] $len
    $got = 0
    while ($got -lt $len) {
        $r = $stream.Read($buf, $got, $len - $got)
        if ($r -le 0) { break }
        $got += $r
    }
    [pscustomobject]@{
        Method = $parts[0]; Path = $parts[1]; Headers = $headers
        Body = [Text.Encoding]::UTF8.GetString($buf, 0, $got)
    }
}

function Send-Answer($stream, [int]$code, [string]$text) {
    $reason = @{ 200 = 'OK'; 403 = 'Forbidden'; 404 = 'Not Found'; 400 = 'Bad Request' }[$code]
    $body = [Text.Encoding]::UTF8.GetBytes($text)
    $head = "HTTP/1.1 $code $reason`r`nContent-Type: text/plain; charset=utf-8`r`nContent-Length: $($body.Length)`r`nConnection: close`r`n`r`n"
    $h = [Text.Encoding]::ASCII.GetBytes($head)
    $stream.Write($h, 0, $h.Length)
    $stream.Write($body, 0, $body.Length)
    $stream.Flush()
}

$listener = New-Object System.Net.Sockets.TcpListener ([System.Net.IPAddress]::Any), $Port
try { $listener.Start() } catch { Log "cannot listen on port ${Port}: $_ (already running?)"; exit 1 }
Log "listening on port $Port"

while ($true) {
    $client = $listener.AcceptTcpClient()
    try {
        $client.ReceiveTimeout = 5000
        $client.SendTimeout = 5000
        $stream = $client.GetStream()
        $req = Read-Request $stream
        if (-not $req) { continue }
        if ($req.Headers['x-soundya-key'] -ne $Key) {
            Log "wrong key from $($client.Client.RemoteEndPoint)"
            Send-Answer $stream 403 'Неверный ключ'
            continue
        }
        $action = $req.Path.Trim('/').ToLower()
        $answer = Handle $action $req.Body.Trim()
        if ($null -eq $answer) { Send-Answer $stream 404 "Неизвестная команда $action" }
        else { Send-Answer $stream 200 ([string]$answer) }
    } catch {
        Log "error: $_"
    } finally {
        $client.Close()
    }
}
