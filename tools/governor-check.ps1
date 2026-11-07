# Memory-governor reclaim check (P1).
#
# Starts the real shell with a tiny renderer budget, so the renderer is over
# budget as soon as the page settles. Asserts the governor logs the overage,
# reclaims the renderer (restart + snapshot restore), and the page comes back.
#
# The shell samples the renderer every 3 s and needs two consecutive
# over-budget samples, so the check polls for up to -TimeoutSeconds.
#
# Usage:
#   .\tools\governor-check.ps1
#   .\tools\governor-check.ps1 -BudgetMB 1 -Url https://github.com

param(
    [string]$ShellExe = (Join-Path $PSScriptRoot '..\target\release\shell.exe'),
    [string]$Url = 'https://example.com',
    [double]$BudgetMB = 1.0,
    [int]$TimeoutSeconds = 60
)

$ErrorActionPreference = 'Stop'

$psi = New-Object System.Diagnostics.ProcessStartInfo
$psi.FileName = $ShellExe
$psi.Arguments = $Url
$psi.RedirectStandardInput = $true
$psi.RedirectStandardOutput = $true
$psi.RedirectStandardError = $true
$psi.UseShellExecute = $false
$psi.EnvironmentVariables['MAZDA_BUDGET_MB'] = "$BudgetMB"
$p = [System.Diagnostics.Process]::Start($psi)
$errTask = $p.StandardError.ReadToEndAsync()

$lines = New-Object System.Collections.ArrayList
$script:pending = $null
function Read-Line([int]$timeoutMs = 2000) {
    if ($null -eq $script:pending) { $script:pending = $p.StandardOutput.ReadLineAsync() }
    if (-not $script:pending.Wait($timeoutMs)) { return $null }
    $line = $script:pending.Result
    $script:pending = $null
    return $line
}
function Pump([int]$ms) {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.ElapsedMilliseconds -lt $ms) {
        $line = Read-Line 250
        if ($null -ne $line) { [void]$lines.Add($line) }
    }
}
# a status poke keeps the shell's UI loop awake even when its window is hidden
function Poke-Status { $p.StandardInput.WriteLine('status'); $p.StandardInput.Flush() }

$pages = 0
$govSeen = $false
$reclaimSeen = $false
$overLine = $null
$sw = [Diagnostics.Stopwatch]::StartNew()
try {
    Pump 3000
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSeconds) {
        Poke-Status
        Pump 2000
        $pages = @($lines | Where-Object { $_ -match '^\[page\]' }).Count
        $gov = @($lines | Where-Object { $_ -match '^\[gov\] renderer' })
        if ($gov.Count -gt 0) { $overLine = $gov[0] }
        $reclaimSeen = @($lines | Where-Object { $_ -match '^\[gov\] reclaimed renderer' }).Count -gt 0
        $govSeen = $gov.Count -gt 0
        # first page came from the initial load, the second from the restore
        if ($govSeen -and $reclaimSeen -and $pages -ge 2) { break }
    }
} finally {
    try { $p.StandardInput.WriteLine('quit'); $p.StandardInput.Flush() } catch {}
    $null = $p.WaitForExit(5000)
    if (-not $p.HasExited) { try { $p.Kill() } catch {} }
}

"--- shell output"
$lines | ForEach-Object { $_ }
"--- stderr (engine)"
($errTask.Result -split "`n" | Select-Object -First 6) | ForEach-Object { $_.TrimEnd() }

$restored = $pages -ge 2
$pass = $govSeen -and $reclaimSeen -and $restored
"gate: over-budget=$govSeen reclaim=$reclaimSeen restored-page=$restored -> $(if ($pass) { 'PASS' } else { 'FAIL' })"
if (-not $pass) { exit 1 }
