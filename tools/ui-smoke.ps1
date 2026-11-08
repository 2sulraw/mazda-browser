# Chrome smoke test that never moves the real cursor.
#
# Posts WM_MOUSEMOVE/WM_LBUTTONDOWN/WM_LBUTTONUP to the shell window as one
# uninterrupted batch, so the press lands before Windows can post
# WM_MOUSELEAVE (which clears egui's pointer position and drops the click).
# The first move parks the pointer elsewhere: winit ignores a repeat move at
# the same pixel, and a click needs a known pointer position.
#
# Screenshots use PrintWindow(PW_RENDERFULLCONTENT), which captures the wgpu
# window even while another fullscreen app owns the foreground. Everything the
# shell writes goes to a scratch profile, so the real one is untouched.

param(
    [string]$Url = 'data:text/html,<title>UI Test</title><h1>UI Test</h1><p>hello ui test page</p>',
    [string]$OutDir = (Join-Path $env:TEMP 'mazda-ui-smoke')
)

$ErrorActionPreference = 'Stop'
Add-Type @"
using System;
using System.Runtime.InteropServices;
public class Ui {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int Left, Top, Right, Bottom; }
  [DllImport("user32.dll")] public static extern bool GetClientRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern uint GetDpiForWindow(IntPtr h);
  [DllImport("user32.dll")] public static extern bool PostMessageW(IntPtr h, uint msg, IntPtr wp, IntPtr lp);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  public static IntPtr Lp(int x, int y) { return (IntPtr)(((y & 0xFFFF) << 16) | (x & 0xFFFF)); }
  public static bool Click(IntPtr h, int px, int py, int tx, int ty) {
    return PostMessageW(h, 0x0200, IntPtr.Zero, Lp(px, py))
        && PostMessageW(h, 0x0200, IntPtr.Zero, Lp(tx, ty))
        && PostMessageW(h, 0x0201, (IntPtr)1, Lp(tx, ty))
        && PostMessageW(h, 0x0202, IntPtr.Zero, Lp(tx, ty));
  }
}
"@
Add-Type -AssemblyName System.Drawing

if (Test-Path $OutDir) { Remove-Item $OutDir -Recurse -Force }
$profileDir = Join-Path $OutDir 'profile'
$localDir = Join-Path $OutDir 'local'
New-Item -ItemType Directory -Force -Path (Join-Path $profileDir 'Downloads'), $localDir | Out-Null

$savedUser, $savedLocal = $env:USERPROFILE, $env:LOCALAPPDATA
$env:USERPROFILE = $profileDir
$env:LOCALAPPDATA = $localDir
$exe = Join-Path $PSScriptRoot '..\target\release\shell.exe'
# Start-Process joins the argument list with spaces, so a data: URL with spaces
# would arrive as several arguments (one tab each). Quote it.
$argLine = '"' + $Url.Replace('"', '\"') + '"'
$p = Start-Process -FilePath $exe -ArgumentList $argLine -PassThru
try {
    $hwnd = [IntPtr]::Zero
    for ($i = 0; $i -lt 150 -and $hwnd -eq [IntPtr]::Zero; $i++) {
        Start-Sleep -Milliseconds 200
        $p.Refresh()
        $h = $p.MainWindowHandle
        if ($h -eq [IntPtr]::Zero) { continue }
        $probe = New-Object Ui+RECT
        # MainWindowHandle can be a tiny helper window early on; wait for the
        # real 1120x760 shell window.
        if ([Ui]::GetClientRect($h, [ref]$probe) -and $probe.Right -gt 300 -and $probe.Bottom -gt 200) {
            $hwnd = $h
        }
    }
    if ($hwnd -eq [IntPtr]::Zero) { throw 'shell window did not appear' }
    Start-Sleep -Milliseconds 2000  # data: page loads in a few ms; let a frame land

    $cr = New-Object Ui+RECT
    [Ui]::GetClientRect($hwnd, [ref]$cr) | Out-Null
    $scale = [Ui]::GetDpiForWindow($hwnd) / 96.0
    $cw = [int]($cr.Right / $scale)
    "window $cw x $([int]($cr.Bottom / $scale)) logical" | Write-Host

    $shots = @{}
    function Save-Shot([string]$name) {
        $wr = New-Object Ui+RECT
        [Ui]::GetWindowRect($hwnd, [ref]$wr) | Out-Null
        $bmp = New-Object System.Drawing.Bitmap ($wr.Right - $wr.Left), ($wr.Bottom - $wr.Top)
        $g = [System.Drawing.Graphics]::FromImage($bmp)
        $hdc = $g.GetHdc()
        $ok = [Ui]::PrintWindow($hwnd, $hdc, 2)
        $g.ReleaseHdc($hdc)
        $g.Dispose()
        $path = Join-Path $OutDir $name
        $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
        $script:shots[$name] = $bmp
        if (-not $ok) { Write-Warning "PrintWindow failed for $name" }
        $path
    }

    # Toolbar button centers, in logical pixels: 40px title + 44px toolbar.
    $y = 62
    $printX = $cw - 63
    $starX = $cw - 99
    $clockX = $cw - 135
    $park = @(@(400, 300), @(420, 320), @(440, 340))
    $clicks = 0
    function Click([int]$x, [int]$y) {
        $pp = $script:park[$script:clicks % 3]
        $script:clicks++
        [Ui]::Click($hwnd, $pp[0], $pp[1], $x, $y) | Out-Null
        Start-Sleep -Milliseconds 800
    }

    function Star-Pixels([System.Drawing.Bitmap]$bmp) {
        $n = 0
        for ($x = $starX - 10; $x -le $starX + 10; $x++) {
            for ($yy = 52; $yy -le 72; $yy++) {
                $c = $bmp.GetPixel($x, $yy)
                if ($c.B -gt 150 -and ($c.B - $c.R) -gt 40) { $n++ }
            }
        }
        $n
    }

    $fail = @()
    Save-Shot '0-initial.png' | Out-Null

    Click $printX $y
    $pdf = $null
    for ($i = 0; $i -lt 100 -and -not $pdf; $i++) {
        Start-Sleep -Milliseconds 200
        $pdf = Get-ChildItem (Join-Path $profileDir 'Downloads') -Filter *.pdf -ErrorAction SilentlyContinue |
            Select-Object -First 1
    }
    if ($pdf) { "print -> $($pdf.Name), $($pdf.Length) bytes" | Write-Host }
    else { $fail += 'print produced no PDF' }

    $bm = Join-Path $localDir 'mazda-browser\bookmarks.tsv'
    Click $starX $y
    Save-Shot '1-bookmarked.png' | Out-Null
    $saved = (Test-Path $bm) -and [IO.File]::ReadAllText($bm).Trim().Length -gt 0
    $blue = Star-Pixels $shots['1-bookmarked.png']
    "bookmark on -> store=$saved, blue pixels=$blue" | Write-Host
    if (-not $saved) { $fail += 'star did not write bookmarks.tsv' }
    if ($blue -eq 0) { $fail += 'star did not turn blue' }

    Click $starX $y
    Save-Shot '2-unbookmarked.png' | Out-Null
    $empty = (-not (Test-Path $bm)) -or [IO.File]::ReadAllText($bm).Trim().Length -eq 0
    $blue = Star-Pixels $shots['2-unbookmarked.png']
    "bookmark off -> store empty=$empty, blue pixels=$blue" | Write-Host
    if (-not $empty) { $fail += 'star did not clear bookmarks.tsv' }
    if ($blue -ne 0) { $fail += 'star stayed blue' }

    Click $starX $y
    Click $clockX $y
    Save-Shot '3-library.png' | Out-Null
    $hist = Join-Path $localDir 'mazda-browser\history.tsv'
    $visits = if (Test-Path $hist) { @(Get-Content $hist).Count } else { 0 }
    "library -> history rows=$visits" | Write-Host
    if ($visits -lt 1) { $fail += 'history.tsv is empty' }

    foreach ($k in $shots.Keys) { $shots[$k].Dispose() }
    if ($fail.Count) {
        "FAIL: $($fail -join '; ')" | Write-Host
        exit 1
    }
    "PASS: chrome clicks, stores and screenshots in $OutDir" | Write-Host
}
finally {
    $env:USERPROFILE = $savedUser
    $env:LOCALAPPDATA = $savedLocal
    if (-not $p.HasExited) {
        $p.CloseMainWindow() | Out-Null
        Start-Sleep -Milliseconds 500
        if (-not $p.HasExited) { $p | Stop-Process -Force }
    }
}
