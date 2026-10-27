# soundya-pc: lets the Yandex Station (soundya) open and close programs, put the PC
# to sleep, shut it down, restart and lock it. Runs in the background after logon.
#
# Protocol: POST http://<pc>:8766/<action> with header "X-Soundya-Key: <key>",
# body = program name (UTF-8) for open/close, the whole phrase for do. Actions:
# ping, open, close, do, sleep, shutdown, restart, lock. The answer is a short Russian sentence that Alice says.
#
# Settings: %LOCALAPPDATA%\soundya-pc\config.json (created by install.ps1):
#   { "port": 8766, "key": "...", "apps": { "моя игра": "C:\\Games\\game.exe" },
#     "sites": { "работа": "https://jira.example.com" },
#     "contacts": { "мама|маме|маму": "@username", "саша|саше": "+79991234567" },
#     "telegram_send": true, "vpn": "AmneziaVPN" }
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
    'впн' = 'AmneziaVPN'; 'амнезия' = 'AmneziaVPN'; 'амнезию' = 'AmneziaVPN'
    'дедлок' = 'Deadlock'; 'варфрейм' = 'Warframe'; 'раст' = 'Rust'; 'валорант' = 'VALORANT'
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

# Does this shortcut still lead somewhere? (Old ones keep pointing at a moved program.)
$Shell = New-Object -ComObject WScript.Shell
function Works($item) {
    if ($item.Path -notlike '*.lnk') { return $true }
    try {
        $target = $Shell.CreateShortcut($item.Path).TargetPath
        return (-not $target) -or (Test-Path -LiteralPath $target)
    } catch { return $true }
}

# Games installed through Steam: @{ Name; Path = steam://rungameid/<id> }.
function SteamGames {
    $root = $null
    try { $root = (Get-ItemProperty 'HKCU:\Software\Valve\Steam' -ErrorAction Stop).SteamPath } catch {}
    if (-not $root) {
        foreach ($p in "${env:ProgramFiles(x86)}\Steam", "$env:ProgramFiles\Steam") { if (Test-Path $p) { $root = $p; break } }
    }
    if (-not $root) { return }
    $root = $root -replace '/', '\'
    $libs = @($root)
    $vdf = Join-Path $root 'steamapps\libraryfolders.vdf'
    if (Test-Path $vdf) {
        foreach ($m in [regex]::Matches((Get-Content $vdf -Raw), '"path"\s+"([^"]+)"')) {
            $libs += ($m.Groups[1].Value -replace '\\\\', '\')
        }
    }
    foreach ($lib in ($libs | Select-Object -Unique)) {
        Get-ChildItem (Join-Path $lib 'steamapps') -Filter 'appmanifest_*.acf' -ErrorAction SilentlyContinue | ForEach-Object {
            $text = Get-Content $_.FullName -Raw
            $id = [regex]::Match($text, '"appid"\s+"(\d+)"').Groups[1].Value
            $name = [regex]::Match($text, '"name"\s+"([^"]+)"').Groups[1].Value
            if ($id -and $name -and $name -notmatch 'Steamworks|Redistributable|Proton|Runtime') {
                [pscustomobject]@{ Name = $name; Path = "steam://rungameid/$id" }
            }
        }
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
    # Steam itself: where it really is now, from the registry.
    if ($target -eq 'steam') {
        try {
            $exe = (Get-ItemProperty 'HKCU:\Software\Valve\Steam' -ErrorAction Stop).SteamExe -replace '/', '\'
            if ($exe -and (Test-Path -LiteralPath $exe)) { return [pscustomobject]@{ Name = 'Steam'; Path = $exe } }
        } catch {}
    }
    $all = @(Shortcuts | Where-Object { Works $_ })
    foreach ($w in $wanted) {
        $exact = $all | Where-Object { $_.Name.ToLower() -eq $w } | Select-Object -First 1
        if ($exact) { return $exact }
        $starts = $all | Where-Object { $_.Name.ToLower().StartsWith($w) } | Sort-Object { $_.Name.Length } | Select-Object -First 1
        if ($starts) { return $starts }
        $has = $all | Where-Object { $_.Name.ToLower().Contains($w) } | Sort-Object { $_.Name.Length } | Select-Object -First 1
        if ($has) { return $has }
    }
    # Steam games: «открой дедлок», «запусти доту».
    $games = @(SteamGames)
    foreach ($w in $wanted) {
        $game = $games | Where-Object { $_.Name.ToLower() -eq $w -or $_.Name.ToLower().StartsWith($w) -or $_.Name.ToLower().Contains($w) } |
            Sort-Object { $_.Name.Length } | Select-Object -First 1
        if ($game) { return $game }
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
        # Longer names may be further off: «dedlok» is 3 edits from «deadlock».
        $best = $null; $bestD = 3
        if ($w.Length -ge 6) { $bestD = 4 }
        foreach ($s in ($all + $games)) {
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
        if ($app.Path -match '\.(lnk|url|exe)$' -or $app.Path -match '^(steam|shell):') {
            # Through Explorer, exactly like a double click: works for programs that
            # ask for administrator rights (the prompt shows on the screen) and keeps
            # everything else running as the normal user.
            Start-Process explorer.exe -ArgumentList "`"$($app.Path)`""
        } else {
            Start-Process -FilePath $app.Path
        }
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

# ---- Free phrases: «открой ютуб в профиле работа», «создай документ и напиши …» ----

# Spoken site names -> addresses. Add your own in config.json: "sites": { "работа": "https://…" }.
$Sites = @{
    'ютуб' = 'https://www.youtube.com'; 'youtube' = 'https://www.youtube.com'; 'ютьюб' = 'https://www.youtube.com'
    'вк' = 'https://vk.com'; 'вконтакте' = 'https://vk.com'; 'гугл' = 'https://www.google.com'
    'почта' = 'https://mail.google.com'; 'почту' = 'https://mail.google.com'; 'гмейл' = 'https://mail.google.com'
    'яндекс' = 'https://ya.ru'; 'яндекс почта' = 'https://mail.yandex.ru'; 'яндекс почту' = 'https://mail.yandex.ru'
    'твич' = 'https://www.twitch.tv'; 'гитхаб' = 'https://github.com'; 'кинопоиск' = 'https://www.kinopoisk.ru'
    'саундклауд' = 'https://soundcloud.com'; 'авито' = 'https://www.avito.ru'; 'озон' = 'https://www.ozon.ru'
    'вайлдберриз' = 'https://www.wildberries.ru'; 'вб' = 'https://www.wildberries.ru'; 'карты' = 'https://yandex.ru/maps'
    'переводчик' = 'https://translate.google.com'; 'реддит' = 'https://www.reddit.com'; 'твиттер' = 'https://x.com'
    'инстаграм' = 'https://www.instagram.com'; 'чат гпт' = 'https://chatgpt.com'; 'чатгпт' = 'https://chatgpt.com'
    'клод' = 'https://claude.ai'; 'телеграм веб' = 'https://web.telegram.org'; 'вотсап веб' = 'https://web.whatsapp.com'
    'ютуб музыку' = 'https://music.youtube.com'; 'спотифай веб' = 'https://open.spotify.com'
}
if ($Config.sites) {
    foreach ($p in $Config.sites.PSObject.Properties) { $Sites[$p.Name.ToLower()] = [string]$p.Value }
}

function Chrome-Exe {
    foreach ($k in 'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe',
                   'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\chrome.exe') {
        try {
            $p = (Get-ItemProperty $k -ErrorAction Stop).'(default)'
            if ($p -and (Test-Path -LiteralPath $p)) { return $p }
        } catch {}
    }
    foreach ($p in "$env:ProgramFiles\Google\Chrome\Application\chrome.exe",
                   "${env:ProgramFiles(x86)}\Google\Chrome\Application\chrome.exe",
                   "$env:LOCALAPPDATA\Google\Chrome\Application\chrome.exe") {
        if (Test-Path -LiteralPath $p) { return $p }
    }
    $null
}

# Chrome profiles: @{ Dir = 'Profile 1'; Name = 'Работа' }.
function Chrome-Profiles {
    $file = Join-Path $env:LOCALAPPDATA 'Google\Chrome\User Data\Local State'
    if (-not (Test-Path -LiteralPath $file)) { return }
    try { $state = Get-Content -LiteralPath $file -Raw -Encoding UTF8 | ConvertFrom-Json } catch { return }
    foreach ($p in $state.profile.info_cache.PSObject.Properties) {
        $v = $p.Value
        # The name shown in Chrome: the one you typed, or the Google account's name.
        $names = @(@($v.name, $v.shortcut_name, $v.gaia_given_name, $v.gaia_name, $v.user_name) |
            Where-Object { $_ } | ForEach-Object { [string]$_ } | Select-Object -Unique)
        if ($names.Count -eq 0) { $names = @($p.Name) }
        # «Person 1» / «Пользователь 1» is Chrome's placeholder: show a real name if there is one.
        $shown = @($names | Where-Object { $_ -notmatch '^(Person|Пользователь)\s*\d*$' }) + $names
        [pscustomobject]@{ Dir = $p.Name; Name = [string]$shown[0]; Names = $names }
    }
}

# The profile a spoken name means, or $null.
function Find-Profile([string]$spoken) {
    $want = $spoken.Trim().ToLower()
    if (-not $want) { return $null }
    $lat = To-Latin $want
    $best = $null
    $bestD = 3
    foreach ($p in @(Chrome-Profiles)) {
        $all = @($p.Name)
        if ($p.Names) { $all = @($p.Names) }
        foreach ($name in $all) {
            $n = $name.ToLower()
            if ($n -eq $want -or $n -eq $lat) { return $p }
            $d = 9
            if ($n.Contains($want) -or $want.Contains($n) -or $n.Contains($lat)) { $d = 1 }
            else { $d = [Math]::Min((Distance $want $n), (Distance $lat $n)) }
            if ($d -lt $bestD) { $best = $p; $bestD = $d }
        }
    }
    $best
}

function Capitalize([string]$s) {
    $s = $s.Trim()
    if ($s.Length -eq 0) { return $s }
    $s.Substring(0, 1).ToUpper() + $s.Substring(1)
}

function Clean-Name([string]$s) {
    $s = ($s -replace '[\\/:*?"<>|]', '').Trim()
    if ($s.Length -gt 60) { $s = $s.Substring(0, 60).Trim() }
    $s
}

# Opens Chrome (with a profile, a site or a search); falls back to the default browser.
function Do-Browser([string]$t) {
    $prof = $null
    # «в профиле работа», «профиль работа»: try the longest name that matches a profile.
    $m = [regex]::Match($t, '(?:\bв\s+)?\bпрофил[а-я]*\s+(.+)$')
    if ($m.Success) {
        $after = @($m.Groups[1].Value -split '\s+' | Where-Object { $_ })
        for ($k = $after.Count; $k -ge 1; $k--) {
            $prof = Find-Profile (($after | Select-Object -First $k) -join ' ')
            if ($prof) {
                $t = ($t.Substring(0, $m.Index) + ' ' + (($after | Select-Object -Skip $k) -join ' ')).Trim()
                break
            }
        }
        if (-not $prof) {
            $known = (@(Chrome-Profiles) | ForEach-Object { $_.Name }) -join ', '
            Log "no profile '$($m.Groups[1].Value)'; profiles: $known"
            return "Не нашла в хроме профиль $($m.Groups[1].Value). Есть: $known"
        }
    }
    $url = $null
    $what = 'браузер'
    $yt = [regex]::Match($t, '\b(?:найди|поищи|открой|включи|поиск)\b\s*(?:мне\s+)?(?:на|в)\s+(?:ютубе|youtube|ютюбе|ютьюбе)\s+(.+)$')
    $s = [regex]::Match($t, '\b(?:найди|поищи|загугли|погугли|поиск)\b\s*(?:в\s+(?:браузере|гугле|интернете|хроме)\s*)?(.*)$')
    if ($yt.Success) {
        $q = $yt.Groups[1].Value.Trim()
        $url = 'https://www.youtube.com/results?search_query=' + [Uri]::EscapeDataString($q)
        $what = "ютуб: $q"
    } elseif ($s.Success -and $s.Groups[1].Value.Trim()) {
        $q = $s.Groups[1].Value.Trim()
        $url = 'https://www.google.com/search?q=' + [Uri]::EscapeDataString($q)
        $what = "поиск: $q"
    } else {
        $name = ' ' + $t + ' '
        foreach ($w in 'открой', 'запусти', 'включи', 'мне', 'в браузере', 'браузер', 'в хроме', 'хром', 'сайт', 'страницу', 'вкладку', 'новую', 'новой', 'в', 'на', 'и', 'а', 'там', 'с') {
            $name = $name -replace (' ' + [regex]::Escape($w) + ' '), ' '
        }
        $name = $name.Trim()
        if ($name) {
            if ($Sites.ContainsKey($name)) { $url = $Sites[$name]; $what = $name }
            elseif ($name -match '^[a-z0-9-]+(\.[a-z0-9-]+)+(/.*)?$') { $url = 'https://' + $name; $what = $name }
            else {
                $url = 'https://www.google.com/search?q=' + [Uri]::EscapeDataString($name)
                $what = "поиск: $name"
            }
        }
    }
    $chrome = Chrome-Exe
    $chromeArgs = @()
    if ($prof) { $chromeArgs += "--profile-directory=`"$($prof.Dir)`"" }
    if ($url) { $chromeArgs += $url }
    if ($chrome) {
        if ($chromeArgs.Count -gt 0) { Start-Process -FilePath $chrome -ArgumentList $chromeArgs } else { Start-Process -FilePath $chrome }
    } elseif ($url) {
        Start-Process explorer.exe -ArgumentList "`"$url`""
    } else {
        return 'Не нашла браузер Chrome'
    }
    $logProfile = ''
    if ($prof) { $logProfile = " (profile $($prof.Name))" }
    Log "browser: $what$logProfile"
    $answer = if ($what -like 'поиск:*') { 'Ищу в гугле ' + $what.Substring(7) } elseif ($what -like 'ютуб:*') { 'Ищу на ютубе ' + $what.Substring(6) } elseif ($what -eq 'браузер') { 'Открываю браузер' } else { "Открываю $what" }
    if ($prof) { $answer += " в профиле $($prof.Name)" }
    $answer
}

# A note: its own .txt file in Documents\Заметки, opened in the text editor.
function Do-Note([string]$body) {
    $body = ($body -replace '^(?:в|во)\s+(?:заметки|заметку|блокнот|блокноте|файл|текстовый файл)\s+', '').Trim()
    $dir = Join-Path ([Environment]::GetFolderPath('MyDocuments')) 'Заметки'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $title = (($body -split '\s+' | Select-Object -First 6) -join ' ')
    if (-not $title) { $title = 'заметка' }
    $path = Join-Path $dir ("{0:yyyy-MM-dd HH-mm} {1}.txt" -f (Get-Date), (Clean-Name $title))
    [IO.File]::WriteAllText($path, (Capitalize $body) + "`r`n", (New-Object Text.UTF8Encoding $true))
    Start-Process explorer.exe -ArgumentList "`"$path`""
    Log "note: $path"
    if ($body) { "Записала: $body" } else { 'Создала пустую заметку' }
}

# «допиши …»: adds a line to the latest note (or starts one) and opens it.
function Do-Append([string]$body) {
    $body = ($body -replace '^(?:в|во)\s+(?:блокнот|блокноте|заметку|заметки|последнюю\s+заметку|файл)\s+', '').Trim()
    if (-not $body) { return 'Скажите, что дописать' }
    $dir = Join-Path ([Environment]::GetFolderPath('MyDocuments')) 'Заметки'
    $last = Get-ChildItem -LiteralPath $dir -Filter *.txt -ErrorAction SilentlyContinue | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    if (-not $last) { return Do-Note $body }
    [IO.File]::AppendAllText($last.FullName, (Capitalize $body) + "`r`n", (New-Object Text.UTF8Encoding $false))
    Start-Process explorer.exe -ArgumentList "`"$($last.FullName)`""
    Log "append: $($last.FullName)"
    "Дописала: $body"
}

# Telegram: opens the chat, pastes the text and sends it.
# Contacts in config.json: "contacts": { "мама|маме|маму": "@username", "саша|саше": "+79991234567" }
# ("telegram_send": false: paste only, press Enter yourself).
function Do-Telegram([string]$t) {
    $contacts = @{}
    $sources = @()
    if ($Config.contacts) { $sources += $Config.contacts }
    if ($global:SpeakerContacts) {
        try {
            $json = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($global:SpeakerContacts))
            $sources += ($json | ConvertFrom-Json)
        } catch { Log "bad contacts from the speaker: $_" }
    }
    foreach ($src in $sources) {
        foreach ($p in $src.PSObject.Properties) {
            foreach ($n in ($p.Name.ToLower() -split '\|')) { if ($n.Trim()) { $contacts[$n.Trim()] = [string]$p.Value } }
        }
    }
    if ($contacts.Count -eq 0) {
        if (-not $explicit) { return $null }
        return 'Сначала добавьте контакты телеграма: страница soundya, Настройки, Компьютер'
    }
    $explicit = $t -match '(?:телег|телеграм|сообщени|контакт)'
    $rest = ' ' + ($t -replace '^(?:напиши|написать|отправь|скинь|пошли|передай)\s+', '') + ' '
    $rest = $rest -replace '\s(?:сообщение|сообщения|в\s+телеграм[а-я]*|в\s+телег[а-я]*|телеграм[а-я]*|контакту|пользователю|мне)(?=\s)', ' '
    $who = $null
    foreach ($n in ($contacts.Keys | Sort-Object Length -Descending)) {
        $i = $rest.IndexOf(' ' + $n + ' ')
        if ($i -ge 0) { $who = $n; $rest = $rest.Remove($i, $n.Length + 1); break }
    }
    if (-not $who) {
        # «напиши в блокнот …» and the like: not a message at all.
        if (-not $explicit) { return $null }
        return 'Не знаю такого контакта. Добавьте его на странице soundya, в настройках компьютера'
    }
    $text = (($rest -replace '^\s*(?:что|текст|с\s+текстом)\s+', ' ') -replace '\s+', ' ').Trim()
    $target = $contacts[$who]
    if (($target -replace '[\s\-()]', '') -match '^\+?\d{6,}$') { $link = 'tg://resolve?phone=' + ($target -replace '\D', '') }
    else { $link = 'tg://resolve?domain=' + ($target -replace '^@', '') }
    Start-Process explorer.exe -ArgumentList "`"$link`""
    if (-not $text) { return "Открыла чат: ${who}" }
    Start-Sleep -Milliseconds 2000
    $saved = $null
    try { $saved = Get-Clipboard -Raw -ErrorAction Stop } catch {}
    # The clipboard may be busy for a moment (another program holds it): try a few times.
    $copied = $false
    for ($i = 0; $i -lt 10 -and -not $copied; $i++) {
        try { Set-Clipboard -Value (Capitalize $text) -ErrorAction Stop; $copied = $true }
        catch { Start-Sleep -Milliseconds 200 }
    }
    if (-not $copied) {
        Log "telegram: clipboard busy"
        return "Открыла чат: ${who}, но не смогла вставить текст. Буфер обмена занят"
    }
    $keys = New-Object -ComObject WScript.Shell
    $keys.SendKeys('^v')
    $send = $true
    if ($null -ne $Config.telegram_send) { $send = [bool]$Config.telegram_send }
    if ($send) {
        Start-Sleep -Milliseconds 400
        $keys.SendKeys('{ENTER}')
    }
    Start-Sleep -Milliseconds 500
    if ($saved) { try { Set-Clipboard -Value $saved -ErrorAction Stop } catch {} }
    Log "telegram: $who <- $text (sent: $send)"
    if ($send) { "Отправила ${who}: $text" } else { "Написала ${who}: $text. Нажмите Enter, чтобы отправить" }
}

# VPN: the VPN app (AmneziaVPN unless "vpn" in config.json names another).
function Do-Vpn([string]$t) {
    $app = 'AmneziaVPN'
    if ($Config.vpn) { $app = [string]$Config.vpn }
    if ($t -match '^(?:отключи|выключи|разорви|закрой)') { return Do-Close $app }
    Do-Open $app
}

# A Word document with the text (a .txt note when Word is not installed).
function Do-Document([string]$title, [string]$body) {
    try {
        $word = New-Object -ComObject Word.Application
    } catch {
        return Do-Note $body
    }
    $word.Visible = $true
    $doc = $word.Documents.Add()
    if ($body) { $doc.Content.Text = Capitalize $body }
    $answer = 'Создала документ'
    if ($title) {
        $path = Join-Path ([Environment]::GetFolderPath('MyDocuments')) ((Clean-Name (Capitalize $title)) + '.docx')
        try { $doc.SaveAs2($path); $answer = "Создала документ $title в Документах" } catch {}
    }
    try { $word.Activate() } catch {}
    Log "document: $title"
    $answer
}

function Do-Folder([string]$rest) {
    $base = [Environment]::GetFolderPath('Desktop')
    $where = 'на рабочем столе'
    if ($rest -match '\b(?:в\s+документах)\b') { $base = [Environment]::GetFolderPath('MyDocuments'); $where = 'в документах' }
    $name = ($rest -replace '\b(?:на\s+рабочем\s+столе|в\s+документах|с\s+названием|под\s+названием|новую)\b', '').Trim()
    if (-not $name) { $name = 'Новая папка' }
    $path = Join-Path $base (Clean-Name (Capitalize $name))
    New-Item -ItemType Directory -Force -Path $path | Out-Null
    Start-Process explorer.exe -ArgumentList "`"$path`""
    Log "folder: $path"
    "Создала папку $name $where"
}

# Works out what a phrase means and does it. The answer is said by Alice.
function Do-Command([string]$t) {
    # Dots stay inside site names («github.com»), go at the end of the phrase.
    $t = ($t.ToLower() -replace 'ё', 'е' -replace '[,!?]', ' ' -replace '\.(\s|$)', ' ' -replace '\s+', ' ').Trim()
    $t = (' ' + $t + ' ') -replace ' (?:пожалуйста|плиз) ', ' '
    $t = $t.Trim()

    # Search: «найди в гугле …», «загугли …», «найди на ютубе …», «открой на ютубе …»
    if ($t -match '^(?:найди|поищи|загугли|погугли)\b') { return Do-Browser $t }
    if ($t -match '^(?:открой|включи)\s+(?:мне\s+)?(?:на|в)\s+(?:ютубе|youtube|ютюбе|ютьюбе)\s+') { return Do-Browser $t }

    # «допиши в блокнот …»
    if ($t -match '^(?:допиши|добавь)\s+(.+)$') { return Do-Append $Matches[1] }
    # «напиши маме в телеграм …», «отправь саше сообщение …»
    # (or just «напиши лизе …» when Лиза is one of the contacts)
    if ($t -match '^(?:напиши|написать|отправь|скинь|пошли|передай)\b') {
        $r = Do-Telegram $t
        if ($null -ne $r) { return $r }
    }
    # «подключи впн», «отключи впн»
    if ($t -match '\b(?:впн|vpn)\b') { return Do-Vpn $t }

    # «… и запиши X», «запиши X», «создай документ с текстом X»
    $head = $t
    $body = ''
    $m = [regex]::Match($t, '^(.*?)\s*(?:\bи\s+)?\b(?:запиши|напиши|впиши|с\s+текстом)\b\s*(.*)$')
    if ($m.Success) { $head = $m.Groups[1].Value.Trim(); $body = $m.Groups[2].Value.Trim() }

    if ($head -match '\b(?:документ|ворд|word)\b' -and $head -match '^(?:создай|сделай|открой|новый)') {
        $title = ($head -replace '^(?:создай|сделай|открой)\s+', '' -replace '\b(?:новый|документ|ворд|word|в|с\s+названием|под\s+названием)\b', '').Trim()
        return Do-Document $title $body
    }
    if ($head -match '^(?:создай|сделай)\s+(?:новую\s+)?папку\b(.*)$') { return Do-Folder $Matches[1] }
    if ($m.Success -and ($head -eq '' -or $head -match '\b(?:блокнот|заметк|текстов|файл)')) { return Do-Note $body }
    if ($head -match '^(?:создай|сделай)\s+(?:новую\s+)?(?:заметку|текстовый\s+файл)') { return Do-Note '' }

    # Browser: profiles, sites, «открой в браузере …»
    $obj = ($head -replace '^(?:открой|запусти|включи)\s+', '').Trim()
    if ($head -match '\b(?:браузер|браузере|профил|сайт|вкладк)' -or $Sites.ContainsKey($obj) -or $obj -match '^[a-z0-9-]+(\.[a-z0-9-]+)+$') {
        return Do-Browser $head
    }

    # Programs and games
    if ($head -match '^(?:открой|запусти|включи|открыть|запустить)\s+(.+)$') {
        $name = $Matches[1] -replace '^(?:программу|приложение|игру)\s+', ''
        return Do-Open $name
    }
    if ($head -match '^(?:закрой|выключи|выруби|закрыть)\s+(.+)$') {
        $name = $Matches[1] -replace '^(?:программу|приложение|игру)\s+', ''
        return Do-Close $name
    }
    "Не поняла, что сделать на компьютере: $t"
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
        'do' { Do-Command $body }
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
        # Telegram contacts set on the speaker's settings page (base64 JSON).
        $global:SpeakerContacts = $req.Headers['x-soundya-contacts']
        $answer = $null
        try {
            $answer = Handle $action $req.Body.Trim()
        } catch {
            # Say what went wrong instead of dropping the connection («компьютер не отвечает»).
            Log "error in '$action': $_"
            $answer = "Не получилось: $($_.Exception.Message)"
        }
        if ($null -eq $answer) { Send-Answer $stream 404 "Неизвестная команда $action" }
        else { Send-Answer $stream 200 ([string]$answer) }
    } catch {
        Log "error: $_"
    } finally {
        $client.Close()
    }
}
