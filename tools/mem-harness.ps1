# Phase S memory harness.
#
# Starts the shell, waits for it (and the renderer it spawns) to settle, and
# reports private bytes + working set for each process. Both are reported
# because the old project's baseline numbers are RSS (working set) while the
# plan's budgets are private bytes.
#
# Usage:
#   .\tools\mem-harness.ps1
#   .\tools\mem-harness.ps1 -Url https://en.wikipedia.org/wiki/Web_browser
#   .\tools\mem-harness.ps1 -Json .\spike-mem.json

param(
    [string]$ShellExe = (Join-Path $PSScriptRoot '..\target\release\shell.exe'),
    [string]$Url = 'https://example.com',
    [int]$SettleSeconds = 10,
    [string]$Json = ''
)

$ErrorActionPreference = 'Stop'

if (-not (Test-Path $ShellExe)) {
    throw "shell binary not found: $ShellExe`nbuild it with: cargo build --release --workspace"
}

$proc = Start-Process -FilePath $ShellExe -ArgumentList @($Url) -PassThru
Start-Sleep -Seconds $SettleSeconds

$proc.Refresh()
if ($proc.HasExited) {
    throw "shell exited early (code $($proc.ExitCode))"
}

$rows = @()
$rows += [pscustomobject]@{
    Process = 'shell'
    Pid     = $proc.Id
    Private = [math]::Round($proc.PrivateMemorySize64 / 1MB, 1)
    WS      = [math]::Round($proc.WorkingSet64 / 1MB, 1)
}

$kids = Get-CimInstance Win32_Process -Filter "ParentProcessId=$($proc.Id)" -ErrorAction SilentlyContinue
foreach ($kid in $kids) {
    $p = Get-Process -Id $kid.ProcessId -ErrorAction SilentlyContinue
    if (-not $p) { continue }
    if ($p.ProcessName -notin @('renderer', 'shell')) { continue }
    $rows += [pscustomobject]@{
        Process = $p.ProcessName
        Pid     = $p.Id
        Private = [math]::Round($p.PrivateMemorySize64 / 1MB, 1)
        WS      = [math]::Round($p.WorkingSet64 / 1MB, 1)
    }
}

$rows += [pscustomobject]@{
    Process = 'TOTAL'
    Pid     = ''
    Private = [math]::Round((($rows | Measure-Object Private -Sum).Sum), 1)
    WS      = [math]::Round((($rows | Measure-Object WS -Sum).Sum), 1)
}

$rows | Format-Table -AutoSize | Out-String | Write-Host

if ($Json) {
    [pscustomobject]@{
        url    = $Url
        settle = $SettleSeconds
        rows   = $rows
    } | ConvertTo-Json -Depth 4 | Set-Content -Path $Json -Encoding utf8
    Write-Host "wrote $Json"
}

Stop-Process -Id $proc.Id -Force -ErrorAction SilentlyContinue
Start-Sleep -Milliseconds 500
Get-Process renderer -ErrorAction SilentlyContinue | ForEach-Object {
    $parent = (Get-CimInstance Win32_Process -Filter "ProcessId=$($_.Id)" -ErrorAction SilentlyContinue).ParentProcessId
    if ($parent -eq $proc.Id) {
        Stop-Process -Id $_.Id -Force -ErrorAction SilentlyContinue
    }
}
