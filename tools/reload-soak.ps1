# 100-reload leak check (P1 acceptance).
#
# Drives the renderer directly (no window): `load <url>` N times in one
# process, sampling private bytes after every 10 loads. Also checks that
# reloads keep reusing the same frame buffer (one mapped file, seq advances).
#
# Two gates:
#   * default: the RSS tail must be a plateau (no unbounded growth).
#   * -LiveHeap: build the renderer with `--features mazda-engine/count-alloc`
#     and gate on the live Rust heap (<= 2 % steady growth). This is the exact
#     signal: RSS also counts allocator pages committed for a page's transient
#     parse peak, and those can step up long after the first reloads (Wikipedia
#     stepped at cycle ~55 in the first 100-cycle run) even though the live
#     heap stayed byte-identical. Use RSS for the memory envelope, the live
#     heap for "no leak".
#
# Usage:
#   .\tools\reload-soak.ps1
#   .\tools\reload-soak.ps1 -Cycles 200 -Url https://en.wikipedia.org/wiki/Web_browser
#   .\tools\reload-soak.ps1 -LiveHeap -Json .\soak.json
#
# Build for -LiveHeap:
#   cargo build --release --workspace --offline --features mazda-engine/count-alloc

param(
    [string]$RendererExe = (Join-Path $PSScriptRoot '..\target\release\renderer.exe'),
    [string]$Url         = 'https://example.com',
    [int]$Cycles         = 100,
    [int]$SampleEvery    = 10,
    [int]$TimeoutSeconds = 180,
    [string]$Json        = '',
    [double]$WarmupFrac  = 0.25,
    # gate on the live Rust heap (build with --features mazda-engine/count-alloc)
    [switch]$LiveHeap,
    # optional stderr filter, printed before the summary (diagnostics)
    [string]$ShowErr     = ''
)

$ErrorActionPreference = 'Stop'

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $RendererExe
$psi.Arguments = "--size 1000x670 $Url"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.CreateNoWindow = $true
$p = [System.Diagnostics.Process]::Start($psi)

# stderr carries the engine's diagnostics, including the live-heap line when
# the renderer is built with `count-alloc`. Read it incrementally: the leak
# gate needs the line for *this* reload, not the whole stream at exit.
$script:errPending = $null
$script:errEof = $false
$script:errLines = New-Object System.Collections.ArrayList
function Drain-Err {
    if ($script:errEof) { return }
    if ($null -eq $script:errPending) { $script:errPending = $p.StandardError.ReadLineAsync() }
    while ($script:errPending.IsCompleted) {
        $line = $script:errPending.Result
        if ($null -eq $line) { $script:errEof = $true; $script:errPending = $null; return }
        [void]$script:errLines.Add($line)
        $script:errPending = $p.StandardError.ReadLineAsync()
    }
}

function Read-Line([int]$timeoutMs = 0) {
    if ($timeoutMs -le 0) { $timeoutMs = $TimeoutSeconds * 1000 }
    Drain-Err
    # Poll instead of blocking for the whole timeout: a verbose page writes
    # more stderr than the pipe buffer holds, so the renderer would block on
    # stderr (and never answer on stdout) unless it keeps being drained.
    $t = $p.StandardOutput.ReadLineAsync()
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while (-not $t.Wait(50)) {
        Drain-Err
        if ($sw.ElapsedMilliseconds -ge $timeoutMs) {
            try { $p.Kill() } catch {}
            throw "renderer did not answer within $timeoutMs ms"
        }
    }
    Drain-Err
    return $t.Result
}

function Get-LiveMB {
    Drain-Err
    $hit = $script:errLines |
        Where-Object { $_ -match '\[mem\] live heap: ([0-9.]+) MB' } |
        Select-Object -Last 1
    if ($hit -and $hit -match 'live heap: ([0-9.]+) MB') { return [double]$Matches[1] }
    return $null
}

function Read-Until([string]$kind) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    $frames = 0
    while ($true) {
        $line = Read-Line
        if ($null -eq $line) { throw "renderer closed stdout waiting for '$kind'" }
        if ($line -like 'frame *') { $frames++ }
        elseif ($line -like 'error *') { throw "renderer error: $line" }
        elseif ($line -like "$kind *") {
            return [pscustomobject]@{ Line = $line; Frames = $frames; Ms = $sw.Elapsed.TotalMilliseconds }
        }
    }
}

# initial load (renderer was started with the URL)
$first = Read-Until 'title'
$f = Get-ChildItem $env:TEMP -Filter "mazda-frame-$($p.Id)-*.bin" | Select-Object -First 1
if (-not $f) { throw 'renderer never mapped a frame' }
$framePath = $f.FullName
$frameLen = $f.Length
"warm-up: $($first.Line)"

function Frame-Seq {
    $fs = [System.IO.File]::Open($framePath, 'Open', 'Read', 'ReadWrite')
    $b = New-Object byte[] 32
    [void]$fs.Read($b, 0, 32)
    $fs.Dispose()
    return [BitConverter]::ToUInt32($b, 16)
}

$samples = @()
$lat = @()
for ($i = 1; $i -le $Cycles; $i++) {
    $t0 = [Diagnostics.Stopwatch]::StartNew()
    $p.StandardInput.WriteLine("load $Url")
    $p.StandardInput.Flush()
    $null = Read-Until 'title'
    $lat += $t0.Elapsed.TotalMilliseconds
    if ($i % $SampleEvery -eq 0) {
        Start-Sleep -Milliseconds 300
        $p.Refresh()
        $mb = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        $live = Get-LiveMB
        $samples += [pscustomobject]@{ Cycle = $i; MB = $mb; LiveMB = $live; Seq = (Frame-Seq) }
        $liveTxt = if ($null -ne $live) { "{0,5} MB" -f $live } else { '     -' }
        Write-Host ("  cycle {0,3}: {1,6} MB  live {2}  seq {3}" -f $i, $mb, $liveTxt, $samples[-1].Seq)
    }
}

$p.StandardInput.WriteLine('quit')
$p.StandardInput.Flush()
$null = $p.WaitForExit(10000)
Drain-Err
$err = ($script:errLines -join "`n")

if ($ShowErr) {
    $hits = $err -split "`n" | Where-Object { $_ -match $ShowErr }
    $i = 0
    foreach ($h in $hits) {
        $i++
        Write-Host ("  [{0,3}] {1}" -f $i, $h.Trim())
    }
}

$firstMb = $samples[0].MB
$lastMb = $samples[-1].MB
$growth = if ($firstMb -gt 0) { [math]::Round(($lastMb - $firstMb) / $firstMb * 100, 2) } else { 0 }
# steady-state growth: start after the allocator warm-up (see header). A real
# leak keeps growing, so it still fails; the one-time commit to the page's
# transient peak does not.
$warmIdx = if ($samples.Count -ge 2) {
    [Math]::Min($samples.Count - 2, [Math]::Max(1, [int][Math]::Floor($samples.Count * $WarmupFrac)))
} else { 0 }
$steadyMb = if ($samples.Count -ge 2) { $samples[$warmIdx].MB } else { $firstMb }
$steadyGrowth = if ($steadyMb -gt 0) { [math]::Round(($lastMb - $steadyMb) / $steadyMb * 100, 2) } else { 0 }
$sorted = $lat | Sort-Object
$earlyN = [Math]::Min(20, $lat.Count)
$early = ($lat[0..($earlyN - 1)] | Measure-Object -Average).Average
$lateN = [Math]::Min(20, $lat.Count)
$late = ($lat[($lat.Count - $lateN)..($lat.Count - 1)] | Measure-Object -Average).Average
$files = (Get-ChildItem $env:TEMP -Filter "mazda-frame-$($p.Id)-*.bin").Count

# Live Rust heap: exact retained bytes, so a leak cannot hide in allocator
# slack. Same post-warm-up window; 2 % is one fifth of a byte per reload on a
# 30 MB page, far below any real leak.
$liveVals = @($samples | Where-Object { $null -ne $_.LiveMB } | ForEach-Object { $_.LiveMB })
$liveFirst = if ($liveVals.Count) { $liveVals[0] } else { 0 }
$liveLast = if ($liveVals.Count) { $liveVals[-1] } else { 0 }
$liveWarm = if ($liveVals.Count -ge 2) {
    [Math]::Min($liveVals.Count - 2, [Math]::Max(1, [int][Math]::Floor($liveVals.Count * $WarmupFrac)))
} else { 0 }
$liveSteady = if ($liveVals.Count -ge 2) { $liveVals[$liveWarm] } else { $liveFirst }
$liveGrowth = if ($liveSteady -gt 0) {
    [math]::Round(($liveLast - $liveSteady) / $liveSteady * 100, 2)
} else { 0 }
$liveSpread = if ($liveVals.Count) {
    [math]::Round((($liveVals | Measure-Object -Maximum).Maximum -
                   ($liveVals | Measure-Object -Minimum).Minimum), 2)
} else { 0 }

$leakOk = if ($LiveHeap) {
    if ($liveVals.Count -lt 2) {
        throw "no '[mem] live heap' samples: build with --features mazda-engine/count-alloc"
    }
    ($liveGrowth -le 2.0)
} else {
    # no-unbounded-growth: the tail must be a plateau. A bounded cache or a
    # one-time allocator commit is not a leak; a leak never flattens.
    $plateauN = [Math]::Min(6, $samples.Count)
    $tail = @($samples[($samples.Count - $plateauN)..($samples.Count - 1)] |
        ForEach-Object { $_.MB })
    $tailMean = ($tail | Measure-Object -Average).Average
    $plateauTol = [Math]::Max(3.0, $tailMean * 0.02)
    $plateauSpread = ($tail | Measure-Object -Maximum).Maximum -
                     ($tail | Measure-Object -Minimum).Minimum
    ($plateauSpread -le $plateauTol)
}

$row = [pscustomobject]@{
    Url          = $Url
    Cycles       = $Cycles
    FirstMB      = $firstMb
    SteadyMB     = $steadyMb
    LastMB       = $lastMb
    PeakMB       = ($samples | Measure-Object MB -Maximum).Maximum
    GrowthPct    = $growth
    SteadyGrowth = $steadyGrowth
    LiveFirstMB  = $liveFirst
    LiveSteadyMB = $liveSteady
    LiveLastMB   = $liveLast
    LiveGrowthPct = $liveGrowth
    LiveSpreadMB = $liveSpread
    PlateauSpreadMB = if ($null -ne $plateauSpread) { [math]::Round($plateauSpread, 1) } else { 0 }
    PlateauTolMB    = if ($null -ne $plateauTol) { [math]::Round($plateauTol, 1) } else { 0 }
    ReloadEarly  = [math]::Round($early, 0)
    ReloadLate   = [math]::Round($late, 0)
    ReloadP50    = [math]::Round($sorted[[int]($sorted.Count * 0.50)], 0)
    ReloadP95    = [math]::Round($sorted[[int]($sorted.Count * 0.95)], 0)
    FrameFiles   = $files
    FrameLen     = $frameLen
    FinalSeq     = (Frame-Seq)
    Gate         = if ($LiveHeap) { 'live-heap' } else { 'rss' }
    Pass         = ($leakOk -and $files -eq 1 -and $late -le ($early * 1.25))
}
$row | Format-List | Out-String | Write-Host
$gateTxt = if ($LiveHeap) {
    "live heap steady growth <= 2 % (RSS is allocator slack; reported only)"
} else {
    "RSS tail plateau within max(3 MB, 2 %) (use -LiveHeap for the exact gate)"
}
"gate: $gateTxt, one frame buffer, late reloads <= 1.25x early -> $(if ($row.Pass) { 'PASS' } else { 'FAIL' })"

if ($Json) {
    $row | ConvertTo-Json -Depth 3 | Set-Content -Path $Json -Encoding utf8
    Write-Host "wrote $Json"
}
if (-not $row.Pass) { exit 1 }
