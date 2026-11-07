# Visual parity harness: render a URL in this engine and in a headless
# reference browser at the SAME viewport, then report the pixel diff and the
# worst 32x32 cells. This is a measurement tool, not a conformance suite:
# dynamic pages (A/B variants, geolocation, cookies) can differ for reasons
# unrelated to the engine, so every report names the URL and the settings.
#
#   powershell -File tools\visual-diff.ps1 -Url https://example.com -Tag example
#
# Outputs (default under %TEMP%\mazda-local\vd):
#   <tag>-engine.png    engine page area (titlebar cropped)
#   <tag>-chrome.png    reference screenshot
#   <tag>-diff.png      engine render with differing pixels tinted red
#   <tag>-side.png      engine | reference, side by side
#   <tag>-report.txt    percentages + worst cells
param(
    [Parameter(Mandatory = $true)][string]$Url,
    [string]$Tag = "page",
    [int]$Width = 1000,
    [int]$Height = 616,
    # the engine's custom titlebar + address pill (TAB_H + ADDR_H in main.rs)
    [int]$Titlebar = 84,
    # match the engine's request profile: Chrome desktop UA, en-US
    [string]$Lang = "en-US",
    [int]$WaitAfterLayout = 12,
    [int]$TimeoutSec = 120,
    # The engine sends `prefers-color-scheme: dark`, so a page with a dark
    # theme renders light in a default headless reference and the diff then
    # measures the theme, not the engine. -Dark asks the reference for the
    # same preference.
    [switch]$Dark,
    # max per-channel difference still counted as "same" (antialiasing, JPEG)
    [int]$Tolerance = 32,
    [string]$OutDir = "$env:TEMP\mazda-local\vd",
    [string]$Engine = "C:\Mazda Browser\target\release\mazda.exe",
    [string]$Chrome = "C:\Program Files\Google\Chrome\Application\chrome.exe",
    [string]$CapScript = "$env:TEMP\mazda-cap.ps1"
)

$ErrorActionPreference = "Stop"
New-Item -ItemType Directory -Force -Path $OutDir | Out-Null

Add-Type -AssemblyName System.Drawing
Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @"
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;

public class VDiff {
    // Crop the engine's window capture down to the page area. `w`/`h` are the
    // viewport the engine laid out for (main.rs pins it); a capture larger
    // than that (a window manager restoring a previous geometry mid-run)
    // keeps the top-left corner, which is still exactly that viewport.
    public static string Crop(string src, string dst, int titlebar, int w, int h) {
        using (var b = new Bitmap(src)) {
            int ch = b.Height - titlebar;
            if (ch < 1) throw new Exception("capture shorter than the titlebar");
            int cw = Math.Min(w, b.Width);
            ch = Math.Min(h, ch);
            using (var c = new Bitmap(cw, ch))
            using (var g = Graphics.FromImage(c)) {
                g.DrawImage(b, new Rectangle(0, 0, cw, ch),
                            new Rectangle(0, titlebar, cw, ch),
                            GraphicsUnit.Pixel);
                c.Save(dst, ImageFormat.Png);
                return cw + "x" + ch;
            }
        }
    }

    static byte[] Bytes(Bitmap b, out int stride) {
        var r = new Rectangle(0, 0, b.Width, b.Height);
        var d = b.LockBits(r, ImageLockMode.ReadOnly, PixelFormat.Format32bppArgb);
        stride = d.Stride;
        var buf = new byte[stride * b.Height];
        Marshal.Copy(d.Scan0, buf, 0, buf.Length);
        b.UnlockBits(d);
        return buf;
    }

    // Compare two same-sized page renders. Cells are 32x32; the right edge
    // (engine scrollbar) and the bottom-right RSS meter are masked out.
    public static string Compare(string aPath, string bPath, int tol,
                                 string diffPath, string sidePath, int cell) {
        using (var a = new Bitmap(aPath))
        using (var b = new Bitmap(bPath)) {
            int w = Math.Min(a.Width, b.Width);
            int h = Math.Min(a.Height, b.Height);
            using (var aa = new Bitmap(a, w, h))
            using (var bb = new Bitmap(b, w, h))
            using (var diff = new Bitmap(aa))
            using (var side = new Bitmap(w * 2, h)) {
                int sa, sb;
                var pa = Bytes(aa, out sa);
                var pb = Bytes(bb, out sb);
                int cw = (w + cell - 1) / cell;
                int ch = (h + cell - 1) / cell;
                var cells = new int[cw * ch];
                int differing = 0, counted = 0;
                for (int y = 0; y < h; y++) {
                    for (int x = 0; x < w; x++) {
                        // mask: scrollbar strip + RSS meter
                        if (x >= w - 14 || (x >= w - 150 && y >= h - 34)) continue;
                        counted++;
                        int i = y * sa + x * 4;
                        int j = y * sb + x * 4;
                        int d0 = Math.Abs(pa[i] - pb[j]);
                        int d1 = Math.Abs(pa[i + 1] - pb[j + 1]);
                        int d2 = Math.Abs(pa[i + 2] - pb[j + 2]);
                        int d = Math.Max(d0, Math.Max(d1, d2));
                        if (d > tol) {
                            differing++;
                            cells[(y / cell) * cw + (x / cell)]++;
                            // tint the differing pixel red on the engine copy
                            diff.SetPixel(x, y, Color.FromArgb(220, 255, 40, 40));
                        }
                    }
                }
                using (var g = Graphics.FromImage(side)) {
                    g.DrawImage(aa, 0, 0);
                    g.DrawImage(bb, w, 0);
                }
                diff.Save(diffPath, ImageFormat.Png);
                side.Save(sidePath, ImageFormat.Png);
                var sb2 = new System.Text.StringBuilder();
                double pct = counted == 0 ? 0 : 100.0 * differing / counted;
                sb2.AppendLine(string.Format("differs: {0} / {1} px = {2:F2}%", differing, counted, pct));
                sb2.AppendLine("worst cells (x,y,differing,share):");
                var idx = new int[cells.Length];
                for (int k = 0; k < idx.Length; k++) idx[k] = k;
                Array.Sort(idx, delegate(int p, int q) { return cells[q].CompareTo(cells[p]); });
                int per = cell * cell;
                for (int k = 0; k < idx.Length && k < 10 && cells[idx[k]] > 0; k++) {
                    int id = idx[k];
                    int cx = (id % cw) * cell;
                    int cy = (id / cw) * cell;
                    sb2.AppendLine(string.Format("  ({0,4},{1,4}) {2,5} px {3,5:F1}%",
                        cx, cy, cells[id], 100.0 * cells[id] / per));
                }
                return sb2.ToString();
            }
        }
    }
}
"@

$stamp = Get-Date -Format "HH:mm:ss"
Write-Output "== $Tag $Url ($Width x $Height, tol $Tolerance) $stamp"

# ---- engine: initial-URL launch, wait for the layout log, capture the window
$raw = Join-Path $OutDir "$Tag-raw.png"
$eng = Join-Path $OutDir "$Tag-engine.png"
$log = Join-Path $OutDir "$Tag-engine.log"
Remove-Item $raw, $eng, $log -Force -ErrorAction SilentlyContinue
# MAZDA_DUMP_RUNS=0 logs `[layout] total height=` (the geometry number the
# wave tables track) without dumping every run.
$env:MAZDA_DUMP_RUNS = "0"
$p = Start-Process $Engine -ArgumentList $Url -PassThru -RedirectStandardError $log
try {
    $sw = [Diagnostics.Stopwatch]::StartNew()
    while ($sw.Elapsed.TotalSeconds -lt $TimeoutSec) {
        Start-Sleep -Milliseconds 400
        if (Test-Path $log) {
            # `[nav] applied` = the finished layout is on screen; waiting on
            # `[layout] total height=` alone never matches without the dump
            # env var, and every run then burned the full timeout.
            if (Select-String -Path $log -Pattern '\[nav\] applied' -Quiet) {
                Start-Sleep -Seconds $WaitAfterLayout
                break
            }
        }
        if ($p.HasExited) { throw "engine exited before layout (see $log)" }
    }
    powershell -ExecutionPolicy Bypass -File $CapScript $raw $p.Id | Out-Null
} finally {
    if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
}
if (-not (Test-Path $raw)) { throw "engine capture missing" }
$pageSize = [VDiff]::Crop($raw, $eng, $Titlebar, $Width, $Height)
$pw, $ph = $pageSize -split 'x'
$pw = [int]$pw; $ph = [int]$ph
Write-Output "engine page: $($pw)x$($ph)"
$lh = Select-String -Path $log -Pattern '\[layout\] total height=(\d+)' | Select-Object -Last 1
if ($lh) { Write-Output "engine layout height: $($lh.Matches[0].Groups[1].Value)" }

# ---- reference browser: same viewport, same language
$chromePng = Join-Path $OutDir "$Tag-chrome.png"
Remove-Item $chromePng -Force -ErrorAction SilentlyContinue
# A fresh profile is REQUIRED: the default profile carries cookies and site
# preferences, so a site can serve it a different variant (Wikipedia, for
# example, renders a different default font-size preference) and the diff
# then measures the variant, not the engine.
$profile = Join-Path $OutDir ("profile-" + [guid]::NewGuid().ToString("N"))
$cargs = @(
    "--headless=new", "--disable-gpu", "--hide-scrollbars", "--no-first-run",
    "--no-default-browser-check", "--user-data-dir=$profile",
    "--window-size=$pw,$ph", "--lang=$Lang", "--accept-lang=$Lang",
    "--screenshot=$chromePng", "--virtual-time-budget=15000", $Url
)
if ($Dark) { $cargs = @("--force-dark-mode") + $cargs }
Start-Process $Chrome -ArgumentList $cargs -Wait `
    -RedirectStandardError (Join-Path $OutDir "$Tag-chrome.log") `
    -RedirectStandardOutput (Join-Path $OutDir "$Tag-chrome.out") | Out-Null
Remove-Item -Recurse -Force $profile -ErrorAction SilentlyContinue
if (-not (Test-Path $chromePng)) { throw "reference screenshot missing" }
Write-Output "reference: $((Get-Item $chromePng).Length) bytes"

# ---- compare
$diff = Join-Path $OutDir "$Tag-diff.png"
$side = Join-Path $OutDir "$Tag-side.png"
$report = [VDiff]::Compare($eng, $chromePng, $Tolerance, $diff, $side, 32)
$reportPath = Join-Path $OutDir "$Tag-report.txt"
[System.IO.File]::WriteAllText($reportPath, "$Tag $Url`n$($Width)x$($Height) tol=$Tolerance`n$report")
Write-Output $report
Write-Output "wrote $reportPath"
