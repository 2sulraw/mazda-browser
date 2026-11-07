# Heavy reference-site measurement (Phase S follow-up).
#
# Measures the three reference sites — Google, GitHub, YouTube — old
# single-process vs the new shell + renderer split, plus scroll latency
# driven straight at the renderer (no window).
#
# Memory is sampled until it plateaus, not after a fixed sleep: GitHub peaks
# at ~165 MB while loading and settles at ~117 MB, so a sleep-based reading
# catches whichever phase it lands in. A site counts as settled when the last
# six 5 s samples (30 s) stay within max(3 MB, 2 %). The 30 s window matters:
# a long fetch pause can look flat for 20 s and then drop 50 MB.
#
# Usage:
#   .\tools\measure-sites.ps1
#   .\tools\measure-sites.ps1 -Only GitHub,YouTube -Repeat 2
#   .\tools\measure-sites.ps1 -Json .\spike-heavy.json

param(
    [string]$ShellExe    = (Join-Path $PSScriptRoot '..\target\release\shell.exe'),
    [string]$OldExe      = 'C:\Mazda Browser\target\release\mazda.exe',
    [string]$RendererExe = (Join-Path $PSScriptRoot '..\target\release\renderer.exe'),
    [string]$Json        = '',
    # names from the list below, e.g. -Only GitHub,YouTube
    [string[]]$Only      = @(),
    # measure each site N times; the last run is reported
    [int]$Repeat         = 1
)

$ErrorActionPreference = 'Stop'

# `powershell -File` hands "-Only GitHub,YouTube" over as one string
$Only = @($Only | ForEach-Object { $_ -split ',' } | Where-Object { $_ })

$sites = @(
    [pscustomobject]@{ Name = 'Google';  Url = 'https://www.google.com';  Min = 25; Max = 120 },
    [pscustomobject]@{ Name = 'GitHub';  Url = 'https://github.com';      Min = 90; Max = 240 },
    [pscustomobject]@{ Name = 'YouTube'; Url = 'https://www.youtube.com'; Min = 45; Max = 150 }
)

function Get-Mem($p) {
    $p.Refresh()
    [pscustomobject]@{
        Private = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        WS      = [math]::Round($p.WorkingSet64 / 1MB, 1)
    }
}

function Get-Renderer($shellPid) {
    $kid = Get-CimInstance Win32_Process -Filter "ParentProcessId=$shellPid" -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -eq 'renderer.exe' } | Select-Object -First 1
    if (-not $kid) { return $null }
    Get-Process -Id $kid.ProcessId -ErrorAction SilentlyContinue
}

# Poll a memory reading until it stops moving. $measure returns MB.
# Returns { Value = settled MB; Peak = highest sample }.
function Wait-Settled($measure, $minSeconds, $maxSeconds) {
    $t0 = Get-Date
    $samples = @()
    $peak = 0.0
    while (((Get-Date) - $t0).TotalSeconds -lt $maxSeconds) {
        Start-Sleep -Seconds 5
        $v = & $measure
        if ($null -eq $v) { return $null }
        if ($v -gt $peak) { $peak = $v }
        $samples += $v
        $elapsed = ((Get-Date) - $t0).TotalSeconds
        if ($elapsed -ge $minSeconds -and $samples.Count -ge 6) {
            $last = $samples[-6..-1]
            $lo = ($last | Measure-Object -Minimum).Minimum
            $hi = ($last | Measure-Object -Maximum).Maximum
            if (($hi - $lo) -le [math]::Max(3.0, $v * 0.02)) {
                return [pscustomobject]@{ Value = $v; Peak = [math]::Round($peak, 1) }
            }
        }
    }
    if ($samples.Count -eq 0) { return $null }
    [pscustomobject]@{ Value = $samples[-1]; Peak = [math]::Round($peak, 1) }
}

function Stop-Tree($pid_) {
    Get-CimInstance Win32_Process -Filter "ParentProcessId=$pid_" -ErrorAction SilentlyContinue |
        ForEach-Object { Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }
    Stop-Process -Id $pid_ -Force -ErrorAction SilentlyContinue
}

function Measure-Split($url, $min, $max) {
    $p = Start-Process -FilePath $ShellExe -ArgumentList @($url) -PassThru
    $res = Wait-Settled {
        $p.Refresh()
        if ($p.HasExited) { return $null }
        $r = Get-Renderer $p.Id
        $p.PrivateMemorySize64 / 1MB + $(if ($r) { $r.PrivateMemorySize64 / 1MB } else { 0 })
    } $min $max
    if ($null -eq $res) { throw "shell exited early: $url" }
    $sh = Get-Mem $p
    $rn = [pscustomobject]@{ Private = 0.0; WS = 0.0 }
    $r = Get-Renderer $p.Id
    if ($r) { $rn = Get-Mem $r }
    $id = $p.Id
    Stop-Tree $id
    [pscustomobject]@{
        Shell    = $sh.Private
        Renderer = $rn.Private
        Total    = [math]::Round($sh.Private + $rn.Private, 1)
        WS       = [math]::Round($sh.WS + $rn.WS, 1)
        Peak     = $res.Peak
    }
}

function Measure-Old($url, $min, $max) {
    if (-not (Test-Path $OldExe)) { return $null }
    $p = Start-Process -FilePath $OldExe -ArgumentList @($url) -PassThru
    $res = Wait-Settled {
        $p.Refresh()
        if ($p.HasExited) { return $null }
        $p.PrivateMemorySize64 / 1MB
    } $min $max
    if ($null -eq $res) { return $null }
    $m = Get-Mem $p
    Stop-Process -Id $p.Id -Force -ErrorAction SilentlyContinue
    [pscustomobject]@{ Private = $m.Private; WS = $m.WS; Peak = $res.Peak }
}

function Measure-Scroll($url) {
    $psi = New-Object System.Diagnostics.ProcessStartInfo
    $psi.FileName = $RendererExe
    $psi.Arguments = $url
    $psi.RedirectStandardInput = $true
    $psi.RedirectStandardOutput = $true
    $psi.RedirectStandardError = $true
    $psi.UseShellExecute = $false
    $psi.CreateNoWindow = $true
    $p = [System.Diagnostics.Process]::Start($psi)
    $drain = $p.StandardError.ReadToEndAsync()
    $first = $p.StandardOutput.ReadLine()   # blocks until the first frame
    if ($first -notlike 'frame *') { Stop-Process -Id $p.Id -Force; return $null }
    $lat = @()
    for ($i = 1; $i -le 30; $i++) {
        $y = 80 * $i
        $t0 = [DateTime]::UtcNow
        $p.StandardInput.WriteLine("scroll $y")
        $p.StandardInput.Flush()
        $null = $p.StandardOutput.ReadLine()
        $lat += ([DateTime]::UtcNow - $t0).TotalMilliseconds
    }
    $p.StandardInput.WriteLine('quit')
    $p.StandardInput.Flush()
    $null = $p.WaitForExit(5000)
    $sorted = $lat | Sort-Object
    [pscustomobject]@{
        P50 = [math]::Round($sorted[[int]($sorted.Count * 0.50)], 2)
        P95 = [math]::Round($sorted[[int]($sorted.Count * 0.95)], 2)
        Max = [math]::Round($sorted[-1], 2)
    }
}

# discard the first launch's warm-up allocations (shader/pipeline setup)
$warm = Start-Process -FilePath $ShellExe -ArgumentList @('https://example.com') -PassThru
Start-Sleep -Seconds 8
Stop-Tree $warm.Id

$rows = @()
foreach ($s in $sites) {
    if ($Only.Count -gt 0 -and $Only -notcontains $s.Name) { continue }
    Write-Host "measuring $($s.Name) (settle window $($s.Min)-$($s.Max) s, repeat $Repeat)..."
    $old = $null
    $new = $null
    for ($i = 1; $i -le $Repeat; $i++) {
        $old = Measure-Old $s.Url $s.Min $s.Max
        $new = Measure-Split $s.Url $s.Min $s.Max
        if ($Repeat -gt 1) {
            Write-Host ("  run {0}: old {1} MB, new {2} MB" -f $i, $old.Private, $new.Total)
        }
    }
    $lat = Measure-Scroll $s.Url
    $rows += [pscustomobject]@{
        Site         = $s.Name
        Url          = $s.Url
        OldPrivate   = if ($old) { $old.Private } else { $null }
        OldPeak      = if ($old) { $old.Peak } else { $null }
        OldWS        = if ($old) { $old.WS } else { $null }
        NewPrivate   = $new.Total
        NewPeak      = $new.Peak
        NewShell     = $new.Shell
        NewRenderer  = $new.Renderer
        NewWS        = $new.WS
        DeltaPrivate = if ($old) { [math]::Round($new.Total - $old.Private, 1) } else { $null }
        ScrollP50    = if ($lat) { $lat.P50 } else { $null }
        ScrollP95    = if ($lat) { $lat.P95 } else { $null }
        ScrollMax    = if ($lat) { $lat.Max } else { $null }
    }
    $rows[-1] | Format-List | Out-String | Write-Host
}

$rows | Format-Table -AutoSize | Out-String | Write-Host

if ($Json) {
    $rows | ConvertTo-Json -Depth 4 | Set-Content -Path $Json -Encoding utf8
    Write-Host "wrote $Json"
}
