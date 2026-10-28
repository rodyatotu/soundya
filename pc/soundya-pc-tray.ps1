# soundya-pc tray icon: shows whether the agent runs and how the speaker is
# reachable (round trip in ms), and gives a menu: restart the agent, copy the key,
# the IP and MAC address for the speaker's settings page, open the log, etc.
#
# Started at logon by soundya-pc-tray.vbs in the Startup folder (install.ps1).
# Reads %LOCALAPPDATA%\soundya-pc\config.json and status.json (written by the agent).

$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Windows.Forms, System.Drawing, System.Net.Http

# One icon per user.
$created = $false
$mutex = New-Object System.Threading.Mutex($true, 'Local\soundya-pc-tray', [ref]$created)
if (-not $created) { exit }

$Dir = Join-Path $env:LOCALAPPDATA 'soundya-pc'
$ConfigPath = Join-Path $Dir 'config.json'
$StatusPath = Join-Path $Dir 'status.json'
$LogPath = Join-Path $Dir 'log.txt'
$Startup = Join-Path $env:APPDATA 'Microsoft\Windows\Start Menu\Programs\Startup'

Add-Type -Namespace SoundyaTray -Name Native -MemberDefinition @'
[DllImport("user32.dll")] public static extern bool DestroyIcon(IntPtr handle);
'@

function Read-Json($path) {
    try { Get-Content $path -Raw -Encoding UTF8 -ErrorAction Stop | ConvertFrom-Json } catch { $null }
}

function Config { Read-Json $ConfigPath }

function Agent-Port {
    $c = Config
    if ($c -and $c.port) { [int]$c.port } else { 8766 }
}

# This PC's address in the home network: the adapter with a default gateway,
# skipping VPN and virtual adapters; the wired one first.
function Pc-Address {
    $configs = @(Get-NetIPConfiguration -ErrorAction SilentlyContinue | Where-Object {
        $_.IPv4DefaultGateway -and $_.IPv4Address -and
        $_.InterfaceAlias -notmatch 'vEthernet|VPN|Amnezia|WireGuard|Loopback|VirtualBox|VMware|Hyper-V|Tailscale|ZeroTier|TAP'
    })
    $best = $configs | Sort-Object { if ($_.NetAdapter.MediaType -eq '802.3') { 0 } else { 1 } } | Select-Object -First 1
    if (-not $best) { return [pscustomobject]@{ Ip = ''; Mac = ''; Wired = $false; Name = '' } }
    [pscustomobject]@{
        Ip = [string]($best.IPv4Address | Select-Object -First 1).IPAddress
        Mac = ([string]$best.NetAdapter.MacAddress).Replace('-', ':')
        Wired = $best.NetAdapter.MediaType -eq '802.3'
        Name = [string]$best.InterfaceAlias
    }
}

# The wired adapter's MAC: wake-on-LAN only works over a cable.
function Wired-Mac {
    $a = Get-NetAdapter -Physical -ErrorAction SilentlyContinue | Where-Object { $_.MediaType -eq '802.3' } |
        Sort-Object { if ($_.Status -eq 'Up') { 0 } else { 1 } } | Select-Object -First 1
    if ($a) { $a.MacAddress.Replace('-', ':') } else { '' }
}

# ---- state, refreshed every few seconds ----
$State = @{
    AgentUp = $false
    Speaker = ''          # IP of the speaker (from config or the last request)
    PingMs = -1           # round trip to the speaker's page; -1 = no answer
    Track = ''
    Playing = ''
    LastSeen = $null
    LastText = ''
    LastMs = 0
    Pending = $null       # HttpClient task in flight
    PendingClock = $null
}

$Http = New-Object System.Net.Http.HttpClient
$Http.Timeout = [TimeSpan]::FromSeconds(4)

function Speaker-Host {
    $c = Config
    if ($c -and $c.speaker) { return [string]$c.speaker }
    $s = Read-Json $StatusPath
    if ($s -and $s.speaker) { return [string]$s.speaker }
    'soundya.local'
}

function Agent-Alive {
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $t = $client.ConnectAsync('127.0.0.1', (Agent-Port))
        $t.Wait(700) -and $client.Connected
    } catch { $false } finally { $client.Close() }
}

function Refresh {
    $State.AgentUp = Agent-Alive
    $s = Read-Json $StatusPath
    if ($s) {
        $State.LastText = if ($s.last_text) { [string]$s.last_text } else { [string]$s.last_action }
        $State.LastMs = [int]$s.last_ms
        $State.LastSeen = if ($s.last_seen) { [datetime]$s.last_seen } else { $null }
    }
    $State.Speaker = Speaker-Host

    # Finish the previous measurement, then start the next one (never blocks the menu).
    $p = $State.Pending
    if ($p) {
        if ($p.IsCompleted) {
            $State.Pending = $null
            if ($p.Status -eq 'RanToCompletion' -and $p.Result.IsSuccessStatusCode) {
                $State.PingMs = [int]$State.PendingClock.Elapsed.TotalMilliseconds
                try {
                    $j = $p.Result.Content.ReadAsStringAsync().Result | ConvertFrom-Json
                    $State.Playing = [string]$j.state
                    $State.Track = [string]$j.track
                    if ($j.airplay -and $j.airplay.active) { $State.Playing = 'airplay'; $State.Track = 'AirPlay (звук с iPhone или Mac)' }
                } catch {}
            } else {
                $State.PingMs = -1; $State.Track = ''; $State.Playing = ''
            }
        } elseif ($State.PendingClock.Elapsed.TotalSeconds -gt 5) {
            $State.Pending = $null; $State.PingMs = -1
        }
    }
    if (-not $State.Pending) {
        $State.PendingClock = [Diagnostics.Stopwatch]::StartNew()
        try { $State.Pending = $Http.GetAsync("http://$($State.Speaker):8765/api/status") } catch { $State.PingMs = -1 }
    }
    Update-Icon
}

# ---- icon: orange speaker badge with a status dot ----
$Colors = @{
    Good = [Drawing.Color]::FromArgb(46, 204, 113)
    Slow = [Drawing.Color]::FromArgb(241, 196, 15)
    Bad  = [Drawing.Color]::FromArgb(231, 76, 60)
    Off  = [Drawing.Color]::FromArgb(140, 140, 140)
}
$script:IconHandle = [IntPtr]::Zero
$script:IconKey = ''

function Make-Icon([Drawing.Color]$dot) {
    $bmp = New-Object Drawing.Bitmap 32, 32
    $g = [Drawing.Graphics]::FromImage($bmp)
    $g.SmoothingMode = 'AntiAlias'
    $g.Clear([Drawing.Color]::Transparent)
    # Speaker body: a rounded column like the Station Lite.
    $body = New-Object Drawing.Drawing2D.GraphicsPath
    $r = 8
    $x = 4; $y = 2; $w = 20; $h = 28
    $body.AddArc($x, $y, $r, $r, 180, 90)
    $body.AddArc($x + $w - $r, $y, $r, $r, 270, 90)
    $body.AddArc($x + $w - $r, $y + $h - $r, $r, $r, 0, 90)
    $body.AddArc($x, $y + $h - $r, $r, $r, 90, 90)
    $body.CloseFigure()
    $g.FillPath((New-Object Drawing.SolidBrush ([Drawing.Color]::FromArgb(255, 112, 32))), $body)
    # Light ring on top.
    $g.DrawEllipse((New-Object Drawing.Pen ([Drawing.Color]::FromArgb(255, 235, 215)), 2), 8, 6, 12, 5)
    # Status dot, bottom right, with a dark outline so it reads on any taskbar.
    $g.FillEllipse((New-Object Drawing.SolidBrush ([Drawing.Color]::FromArgb(30, 30, 30))), 17, 17, 15, 15)
    $g.FillEllipse((New-Object Drawing.SolidBrush $dot), 19, 19, 11, 11)
    $g.Dispose()
    $h = $bmp.GetHicon()
    $bmp.Dispose()
    $h
}

function Ago($t) {
    if (-not $t) { return 'ещё не было' }
    $s = ((Get-Date) - $t).TotalSeconds
    if ($s -lt 60) { 'только что' }
    elseif ($s -lt 3600) { '{0} мин назад' -f [int]($s / 60) }
    elseif ($s -lt 86400) { '{0} ч назад' -f [int]($s / 3600) }
    else { $t.ToString('dd.MM HH:mm') }
}

function Update-Icon {
    if (-not $State.AgentUp) { $color = 'Off'; $line = 'агент не запущен' }
    elseif ($State.PingMs -lt 0) { $color = 'Bad'; $line = 'колонка не отвечает' }
    elseif ($State.PingMs -gt 300) { $color = 'Slow'; $line = "колонка: $($State.PingMs) мс (медленно)" }
    else { $color = 'Good'; $line = "колонка: $($State.PingMs) мс" }

    if ($script:IconKey -ne $color) {
        $old = $script:IconHandle
        $script:IconHandle = Make-Icon $Colors[$color]
        $Tray.Icon = [Drawing.Icon]::FromHandle($script:IconHandle)
        if ($old -ne [IntPtr]::Zero) { [SoundyaTray.Native]::DestroyIcon($old) | Out-Null }
        $script:IconKey = $color
    }
    # The tooltip may hold at most 63 characters.
    $tip = "soundya-pc · $line"
    if ($tip.Length -gt 63) { $tip = $tip.Substring(0, 62) + '…' }
    $Tray.Text = $tip

    $ItemAgent.Text = if ($State.AgentUp) { "Агент: работает (порт $(Agent-Port))" } else { 'Агент: не запущен' }
    $ItemSpeaker.Text = if ($State.PingMs -ge 0) { "Колонка $($State.Speaker): $($State.PingMs) мс" } else { "Колонка $($State.Speaker): не отвечает" }
    $ItemTrack.Visible = [bool]$State.Track
    if ($State.Track) {
        $icon = if ($State.Playing -eq 'paused') { '‖' } else { '▶' }
        $t = $State.Track
        if ($t.Length -gt 48) { $t = $t.Substring(0, 47) + '…' }
        $ItemTrack.Text = "$icon $t"
    }
    $last = if ($State.LastText) { $State.LastText } else { '—' }
    if ($last.Length -gt 32) { $last = $last.Substring(0, 31) + '…' }
    $took = if ($State.LastSeen) { ", $($State.LastMs) мс" } else { '' }
    $ItemLast.Text = "Последняя команда: $last ($(Ago $State.LastSeen)$took)"
    $ItemToggle.Enabled = $State.PingMs -ge 0
    $ItemNext.Enabled = $State.PingMs -ge 0
}

# ---- actions ----
function Notify([string]$text) { $Tray.ShowBalloonTip(2500, 'soundya-pc', $text, 'None') }

function Copy-Text([string]$what, [string]$text) {
    if (-not $text) { Notify "$what не найден"; return }
    [Windows.Forms.Clipboard]::SetText($text)
    Notify "$what скопирован: $text"
}

function Restart-Agent {
    # The Startup-folder script restarts the agent 5 s after it stops.
    $procs = @(Get-CimInstance Win32_Process -Filter "Name = 'powershell.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like '*soundya-pc.ps1*' -and $_.CommandLine -notlike '*tray*' })
    $loop = @(Get-CimInstance Win32_Process -Filter "Name = 'wscript.exe'" -ErrorAction SilentlyContinue |
        Where-Object { $_.CommandLine -like '*soundya-pc.vbs*' })
    foreach ($p in $procs) { Stop-Process -Id $p.ProcessId -Force -ErrorAction SilentlyContinue }
    if (-not $loop) {
        # Nothing to bring it back: start the Startup-folder script ourselves.
        $vbs = Join-Path $Startup 'soundya-pc.vbs'
        if (Test-Path $vbs) { Start-Process wscript.exe -ArgumentList "`"$vbs`"" }
        else { Notify 'Не нашла автозапуск агента. Запустите install.cmd ещё раз.'; return }
    }
    Notify 'Перезапускаю агент…'
}

function Show-Setup {
    $c = Config
    $a = Pc-Address
    $mac = Wired-Mac
    $text = "Впишите в панели колонки (Настройки → Компьютер):`r`n`r`n" +
        "Адрес компьютера:  $($a.Ip)`r`n" +
        "Ключ:  $($c.key)`r`n" +
        "MAC-адрес:  $(if ($mac) { $mac } else { 'нет сетевого кабеля' })`r`n`r`n" +
        "Нажмите «Да», чтобы скопировать всё в буфер обмена."
    $r = [Windows.Forms.MessageBox]::Show($text, 'soundya-pc: данные для колонки', 'YesNo', 'Information')
    if ($r -eq 'Yes') {
        [Windows.Forms.Clipboard]::SetText("Адрес: $($a.Ip)`r`nКлюч: $($c.key)`r`nMAC: $mac")
        Notify 'Скопировано'
    }
}

function Speaker-Cmd([string]$cmd) {
    try {
        $content = New-Object System.Net.Http.StringContent($cmd)
        $Http.PostAsync("http://$($State.Speaker):8765/api/cmd", $content) | Out-Null
    } catch { Notify "Колонка не отвечает: $($_.Exception.Message)" }
}

function Open-Speaker {
    Start-Process "http://$($State.Speaker):8765/"
}

function Check-Now {
    $State.Pending = $null
    Refresh
    for ($i = 0; $i -lt 40 -and $State.Pending -and -not $State.Pending.IsCompleted; $i++) { Start-Sleep -Milliseconds 100 }
    Refresh
    $agent = if ($State.AgentUp) { 'агент работает' } else { 'агент НЕ запущен' }
    $sp = if ($State.PingMs -ge 0) { "колонка $($State.Speaker) отвечает за $($State.PingMs) мс" } else { "колонка $($State.Speaker) не отвечает" }
    Notify "$agent; $sp"
}

# ---- menu ----
$Tray = New-Object Windows.Forms.NotifyIcon
$Menu = New-Object Windows.Forms.ContextMenuStrip

function Item([string]$text, [scriptblock]$action) {
    $i = New-Object Windows.Forms.ToolStripMenuItem $text
    if ($action) { $i.add_Click($action) } else { $i.Enabled = $false }
    [void]$Menu.Items.Add($i)
    $i
}
function Separator { [void]$Menu.Items.Add((New-Object Windows.Forms.ToolStripSeparator)) }

$ItemAgent = Item 'Агент: …' $null
$ItemSpeaker = Item 'Колонка: …' $null
$ItemTrack = Item '' $null
$ItemLast = Item 'Последняя команда: …' $null
Separator
$ItemToggle = Item 'Пауза / продолжить' { Speaker-Cmd 'toggle' }
$ItemNext = Item 'Следующий трек' { Speaker-Cmd 'next' }
Item 'Открыть панель колонки' { Open-Speaker } | Out-Null
Separator
Item 'Данные для колонки (IP, ключ, MAC)…' { Show-Setup } | Out-Null
Item 'Скопировать ключ' { Copy-Text 'Ключ' ([string](Config).key) } | Out-Null
Item 'Скопировать IP компьютера' { Copy-Text 'IP' (Pc-Address).Ip } | Out-Null
Item 'Скопировать MAC (для включения по сети)' { Copy-Text 'MAC' (Wired-Mac) } | Out-Null
Separator
Item 'Проверить связь' { Check-Now } | Out-Null
Item 'Перезапустить агент' { Restart-Agent } | Out-Null
Item 'Открыть лог' { if (Test-Path $LogPath) { Start-Process notepad.exe "`"$LogPath`"" } else { Notify 'Лога пока нет' } } | Out-Null
Item 'Открыть настройки (config.json)' { Start-Process notepad.exe "`"$ConfigPath`"" } | Out-Null
Item 'Открыть папку программы' { Start-Process explorer.exe "`"$Dir`"" } | Out-Null
Separator
Item 'Убрать значок (агент продолжит работать)' {
    $Timer.Stop(); $Tray.Visible = $false; [Windows.Forms.Application]::Exit()
} | Out-Null

$Tray.ContextMenuStrip = $Menu
$Tray.add_DoubleClick({ Open-Speaker })
# A single left click shows the current state.
$Tray.add_MouseClick({ param($s, $e) if ($e.Button -eq 'Left') { Notify ($ItemAgent.Text + "`n" + $ItemSpeaker.Text) } })
# Refresh right before the menu shows, so it is never stale.
$Menu.add_Opening({ Refresh })

$Timer = New-Object Windows.Forms.Timer
$Timer.Interval = 5000
$Timer.add_Tick({ try { Refresh } catch {} })

Update-Icon
$Tray.Visible = $true
Refresh
$Timer.Start()
[Windows.Forms.Application]::Run()

$Tray.Visible = $false
$Tray.Dispose()
$mutex.ReleaseMutex()
