# 10-tab memory check (P1 acceptance).
#
# Starts the real shell (window) with ten tabs, one active, and drives its
# stdin control channel. Background tabs are metadata only: the shell keeps a
# URL/label/scroll, the single renderer holds one page. This script proves that
# by activating every tab in turn and checking that memory returns to the same
# plateau, with exactly one renderer process.
#
# Gate: shell_private + renderer_private <= -BudgetMB after the plateau, ten
# tabs present, one renderer, and every tab restored a page.
#
# Usage:
#   .\tools\measure-tabs.ps1
#   .\tools\measure-tabs.ps1 -Active 1 -BudgetMB 300
#   .\tools\measure-tabs.ps1 -Json .\tabs.json

param(
    [string]$ShellExe = (Join-Path $PSScriptRoot '..\target\release\shell.exe'),
    [string]$TabList  = 'https://www.google.com,https://github.com,https://www.youtube.com,https://en.wikipedia.org/wiki/Main_Page,https://example.com,https://en.wikipedia.org/wiki/Web_browser,https://github.com/rust-lang/rust,https://www.google.com/search?q=rust,https://en.wikipedia.org/wiki/Rust_(programming_language),https://www.youtube.com/results?search_query=rust',
    [int]$Active       = 1,
    [double]$BudgetMB = 300.0,
    [int]$TimeoutSeconds = 300,
    [int]$FlatSamples = 6,
    [int]$SampleSeconds = 5,
    [switch]$SkipCycle,
    [string]$Json = ''
)

$ErrorActionPreference = 'Stop'

$tabs = @($TabList.Split(',') | ForEach-Object { $_.Trim() } | Where-Object { $_ })
if ($tabs.Count -ne 10) {
    throw "measure-tabs expects exactly 10 tabs, got $($tabs.Count)"
}
if ($Active -lt 0 -or $Active -ge $tabs.Count) {
    throw "active index $Active is outside 0..$($tabs.Count - 1)"
}

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ShellExe
$psi.Arguments = "--tabs $TabList --active $Active"
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$p = [System.Diagnostics.Process]::Start($psi)
$errTask = $p.StandardError.ReadToEndAsync()

function Read-Line([int]$timeoutMs = 0) {
    if ($timeoutMs -le 0) { $timeoutMs = $TimeoutSeconds * 1000 }
    $t = $p.StandardOutput.ReadLineAsync()
    if (-not $t.Wait($timeoutMs)) {
        try { $p.Kill() } catch {}
        throw "shell did not answer within $timeoutMs ms"
    }
    return $t.Result
}

function Read-Until([string]$pattern) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($true) {
        $line = Read-Line
        if ($null -eq $line) { throw "shell closed stdout waiting for /$pattern/" }
        if ($line -match $pattern) { return $line }
    }
}

function Send-Ctl([string]$cmd) {
    $p.StandardInput.WriteLine($cmd)
    $p.StandardInput.Flush()
}

function Get-Sample {
    Send-Ctl 'status'
    $line = Read-Until '^\[status\]'
    $m = [regex]::Match($line, 'shell_private=([0-9.]+) renderer_private=([0-9.]+)')
    if (-not $m.Success) { throw "unparsable status line: $line" }
    $shell = [double]$m.Groups[1].Value
    $rend = [double]$m.Groups[2].Value
    return [pscustomobject]@{
        ShellMB    = $shell
        RendererMB = $rend
        TotalMB    = [math]::Round($shell + $rend, 1)
    }
}

# six samples whose spread is within max(3 MB, 2 %) = a flat plateau
function Wait-Plateau([string]$what) {
    $vals = @()
    $maxSamples = 24
    while ($true) {
        $s = Get-Sample
        $vals += $s
        Write-Host ("  {0} {1,2}: shell {2,6} MB + renderer {3,6} MB = {4,6} MB" -f `
            $what, $vals.Count, $s.ShellMB, $s.RendererMB, $s.TotalMB)
        if ($vals.Count -ge $FlatSamples) {
            $start = $vals.Count - $FlatSamples
            $last = @($vals[$start..($vals.Count - 1)])
            $totals = @($last | ForEach-Object { $_.TotalMB })
            $mean = ($totals | Measure-Object -Average).Average
            $tol = [math]::Max(3.0, $mean * 0.02)
            $spread = ($totals | Measure-Object -Maximum).Maximum -
                      ($totals | Measure-Object -Minimum).Minimum
            if ($spread -le $tol) { return $vals }
        }
        if ($vals.Count -ge $maxSamples) {
            Write-Warning "$what did not flatten after $maxSamples samples"
            return $vals
        }
        Start-Sleep -Seconds $SampleSeconds
    }
}

$startup = $null
try {
    # the shell loads the active tab before it answers; a heavy page takes a
    # few seconds on first fetch
    $startup = Read-Until "^\[page\] tab=$Active "
    Write-Host "startup: $startup"

    $before = Wait-Plateau 'settle '

    $cycled = @()
    $cycleOk = $true
    if (-not $SkipCycle) {
        for ($i = 0; $i -lt $tabs.Count; $i++) {
            $sw = [Diagnostics.Stopwatch]::StartNew()
            Send-Ctl "tab $i"
            try {
                # events from earlier requests are skipped: only this tab's
                # own title counts
                $page = Read-Until "^\[page\] tab=$i "
                $ms = [math]::Round($sw.Elapsed.TotalMilliseconds, 0)
                Write-Host ("  tab {0,2}: {1,6} ms  {2}" -f $i, $ms, $page)
                $cycled += [pscustomobject]@{ Tab = $i; Ms = $ms; Page = $page }
            } catch {
                Write-Host ("  tab {0,2}: FAILED after {1:N0} ms" -f $i, $sw.Elapsed.TotalMilliseconds)
                $cycled += [pscustomobject]@{ Tab = $i; Ms = $sw.Elapsed.TotalMilliseconds; Page = '' }
                $cycleOk = $false
            }
        }
        # land back on the measured tab
        Send-Ctl "tab $Active"
        try { $null = Read-Until "^\[page\] tab=$Active " } catch { $cycleOk = $false }
    }

    $after = Wait-Plateau 'settled'

    $kids = @(Get-CimInstance Win32_Process -Filter "ParentProcessId=$($p.Id)" |
        Where-Object { $_.Name -eq 'renderer.exe' })
    $final = $after[-1]
    $restored = @($cycled | Where-Object { $_.Page -ne '' }).Count
    $tabsOk = $cycleOk -and ($restored -eq $tabs.Count)
    $pass = ($final.TotalMB -le $BudgetMB) -and ($kids.Count -eq 1) -and $tabsOk

    $row = [pscustomobject]@{
        Tabs          = $tabs.Count
        Active        = $Active
        Startup       = "$startup"
        RendererPID   = if ($kids.Count -eq 1) { $kids[0].ProcessId } else { 0 }
        Renderers     = $kids.Count
        BeforeTotalMB = $before[-1].TotalMB
        AfterTotalMB  = $final.TotalMB
        AfterShellMB  = $final.ShellMB
        AfterRenderMB = $final.RendererMB
        PeakTotalMB   = (@($before + $after) | Measure-Object TotalMB -Maximum).Maximum
        TabsRestored  = @($cycled | Where-Object { $_.Page -ne '' }).Count
        BudgetMB      = $BudgetMB
        Pass          = $pass
    }
    $row | Format-List
    if ($Json) { $row | ConvertTo-Json | Set-Content -Path $Json -Encoding UTF8 }
    if ($pass) {
        Write-Host ("gate: {0} tabs, one renderer, {1} MB <= {2} MB -> PASS" -f $tabs.Count, $final.TotalMB, $BudgetMB)
    } else {
        Write-Host ("gate: {0} tabs, {1} renderer(s), {2} MB (budget {3} MB), {4}/{5} tabs restored -> FAIL" -f `
            $tabs.Count, $kids.Count, $final.TotalMB, $BudgetMB, $row.TabsRestored, $tabs.Count)
    }
    if (-not $pass) { exit 1 }
} finally {
    try { Send-Ctl 'quit' } catch {}
    $null = $p.WaitForExit(5000)
    if (-not $p.HasExited) { try { $p.Kill() } catch {} }
}
