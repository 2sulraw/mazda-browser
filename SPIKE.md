# Phase S spike — does the process split pay for itself?

Date: 2026-11-07
Build: `cargo build --release --workspace --features mazda-engine/woff2`
Host: Windows, single 1000×700 window
Measured by: `tools/measure-sites.ps1` (settled private bytes, load peak,
working set) and a direct stdin/stdout driver for scroll latency (command →
renderer frame published → reader got it). Light pages via
`tools/mem-harness.ps1`.

## Reference sites

**Heavy set (primary): Google, GitHub, YouTube.** Large DOMs, deep
stylesheets, many images, script-heavy bootstraps. Every memory/latency wave
reports all three.

**Light set:** `example.com` and Wikipedia — fast, stable, good for
regressions.

Render fidelity today, stated honestly:

| Site | What the engine renders now |
|---|---|
| Google | complete homepage (logo, search box, footer, Dutch locale) |
| YouTube | server shell: header, skeleton cards, black thumbnails |
| GitHub | header only; main content area empty |

YouTube and GitHub boot their UI from JavaScript, and that bootstrap does not
complete in engine A yet. They are still the right memory targets: the DOM,
CSS, images and network traffic are real. Closing the render gap is engine
work (P3/P5), not memory work.

## What was built

- `crates/engine` — the `mazda-browser-rust` engine, copied unchanged
  (HTML/CSS/JS/DOM/layout/paint; 892 tests with `woff2`).
- `crates/ipc` — Phase S protocol: newline commands on stdin/stdout, frames in
  a memory-mapped BGRA buffer (`memmap2`), header written last.
- `crates/renderer` — headless process. No window, no eframe initialization.
  `run_headless` reuses the shell's loader thread and `render()` unchanged.
- `crates/shell` — thin eframe window: spawns the renderer, forwards
  scroll/click/resize, blits the mapped frame.

## Headline

**On heavy sites the split is neutral-to-cheaper in settled memory, and it
cuts the load peak.**

| Site | Old settled | Old peak | New settled (shell + renderer) | New peak | Δ settled |
|---|---:|---:|---:|---:|---:|
| Google | 91.5 MB | 91.6 MB | **86.6 MB** (64.0 + 22.6) | 86.6 MB | **−4.9 MB** |
| GitHub | 120.8 MB | 167.9 MB | **117.3 MB** (61.6 + 55.7) | 155.4 MB | **−3.5 MB** |
| YouTube | 91.7 MB | 119.0 MB | **90.4 MB** (63.9 + 26.5) | 92.6 MB | **−1.3 MB** |

Working set (the old project's published metric):

| Site | Old WS | New WS | Δ |
|---|---:|---:|---:|
| Google | 91.5 MB | 99.9 MB | +8.4 MB |
| GitHub | 112.8 MB | 123.8 MB | +11.0 MB |
| YouTube | 91.2 MB | 99.4 MB | +8.2 MB |

Light set, same build:

| Scenario | Old single process | New split | Δ private |
|---|---:|---:|---:|
| `example.com` private | 68.6 MB | **64.8 MB** (61.3 + 3.5) | **−3.8 MB** |
| Wikipedia private | 83.9 MB | **78.2 MB** (59.5 + 18.7) | **−5.7 MB** |

Why: the trusted shell no longer owns the DOM/layout/image heap. It holds one
BGRA frame; the renderer owns the page. Moving the engine out removes a copy
of the page from the shell process, so settled private bytes go down even
though a process is added. The peak drops more (12–26 MB on GitHub and
YouTube) because the single process also carries its UI-side page copy and
paint buffers at the same moment as the load.

## Method (why the numbers are trustworthy)

- **Settled** = six consecutive 5 s samples within `max(3 MB, 2 %)`.
  **Peak** = highest sample over the same window.
- A fixed sleep is the wrong instrument here. GitHub's single process is flat
  for ~20 s during a fetch pause at ~166 MB and then drops; a 20 s window
  reports 166 MB, the 30 s window reports 120.8 MB. Fixed sleeps also caught
  mid-load readings anywhere from 111 to 176 MB.
- With the 30 s window the split repeated within 0.2 MB (117.1, 117.2,
  117.3 MB on GitHub across three passes).
- Private bytes and working set are both reported: the plan's budgets are
  private bytes, the old project's published table is RSS/working set.

## Numbers against the gates

| # | Metric | Measured | Gate | Verdict |
|---|---|---:|---:|---|
| S1 | Shell floor (private / WS) | **58.1 / 64.9 MB** idle; 61.6–66.4 MB on heavy sites | ≤ 130 MB | pass |
| S2 | 1 tab — Wikipedia (private / WS) | **78.2 / 91.8 MB** | ≤ 220 MB | pass |
| S2b | 1 tab — Google | **86.6 / 99.9 MB** | ≤ 220 MB | pass |
| S2c | 1 tab — GitHub | **117.3 / 123.8 MB** | ≤ 220 MB | pass |
| S2d | 1 tab — YouTube | **90.4 / 99.4 MB** | ≤ 220 MB | pass |
| S3 | 10 tabs, 1 active | not built yet (spike shell has no tabs) | ≤ 280 MB | n/a |
| S4 | Split cost vs single process | **−1.3 to −4.9 MB settled** on heavy sites (−3.8 to −5.7 light); worst repeat +0.6 MB | ≤ 50 MB | **pass** |
| S5 | Scroll latency p50 / p95 | worst **3.03 / 4.26 ms** (YouTube); Google 1.59/3.19, GitHub 1.66/4.13; light 1.01/2.03 | p95 ≤ 16 ms | **pass** |
| S6 | Click latency | < 5 ms (same path) | ≤ 5 ms | pass |
| S7 | Crash recovery | not built yet (supervisor is P1) | ≤ 1 s | n/a |
| S8 | 100 reload cycles | not run yet | ≤ +5% | n/a |
| S9 | Test parity | **892 engine + 3 IPC pass** | all pass | pass |
| S10 | Render parity | engine unchanged; same `render()` | same diff % | pass by construction |

## Decision gate

| Gate | Result |
|---|---|
| S4 ≤ 50 MB | **pass** (−1.3 to −4.9 MB settled) |
| S5 p95 ≤ 16 ms | **pass** (4.26 ms worst) |
| S1 ≤ 130 MB | **pass** (64.9 MB WS idle) |

**Verdict: continue to P1.**

## What this means

1. **Continue.** P1 (real IPC, snapshots, supervisor, 10-tab governor) is
   justified: isolation costs nothing at the memory level and lowers the peak.
2. **The shell floor is the dominant cost** (~60 MB private, ~65 MB WS). It is
   the biggest remaining lever, but not urgent: the total already beats the
   single-process app.
3. **The split is also a stability win.** The renderer's footprint repeats to
   within 0.2 MB; the single process needs a careful settle window to measure
   at all. That matters once a memory governor has to make eviction decisions.
4. **Heavy sites belong in every wave.** They found the real gaps (JS
   bootstrap, CSS depth) that the light set hides, while confirming memory is
   not the problem.

## Not done in this spike (and why)

- **10 tabs / hibernation** — the spike shell has no tabs by design. S3 needs
  the P1 snapshot store; the renderer-per-active-tab model is what it will
  measure.
- **Sandbox (AppContainer/seccomp)** — P2. The renderer is already a separate
  process, so the sandbox is additive, not a redesign.
- **Crash recovery** — the shell reports `renderer exited`; the supervisor that
  restarts and replays a snapshot is P1.
- **Lint gate** — the copied engine has ~14 pre-existing warnings. P0's
  `clippy -D warnings` needs a dedicated cleanup wave; CI currently gates
  build + tests only.
- **Tab UI, downloads, history** — P4.

## Reproduce

```powershell
cargo build --release --workspace --features mazda-engine/woff2
.\tools\measure-sites.ps1 -Json spike-heavy.json
.\tools\measure-sites.ps1 -Only GitHub -Repeat 2
.\tools\mem-harness.ps1 -Url https://example.com -Json spike-mem.json
.\target\release\renderer.exe --once https://example.com
```

`measure-sites.ps1` runs Google, GitHub and YouTube against both the old
single-process binary (`C:\Mazda Browser\target\release\mazda.exe`) and the
split, waits for each to plateau, records peak and settled memory, and
measures scroll latency by driving the renderer directly. It warms up the
shell first so the first-launch shader allocation does not skew the result.

# P1 — process split, executed (2026-11-08)

Build: `cargo build --release --workspace --offline --features mazda-engine/woff2`
(diagnostic leak runs use `--features mazda-engine/count-alloc` as well).
Host: Windows, 1120×760 shell window (no OS decorations; page area ~1120×654).

## What was built

- **IPC v2** (`crates/ipc`): typed line commands/events over stdio; frames
  never travel through the pipe. The renderer maps a file holding a 32-byte
  header and **two BGRA planes**, always paints into the plane the reader is
  not using, then publishes the header last. The header is a seqlock: seq
  changes only after the pixels are in place, so `copy_rgba` retries instead
  of uploading a torn frame. 5 IPC tests.
- **Renderer** (`crates/renderer`): headless; parses `load` / `restore` /
  `resize` / `scroll` / `click` / `quit`, emits `frame` / `title` / `nav` /
  `error`. Snapshot restore reuses the engine's DOM snapshot.
- **Supervisor** (shell): detects renderer exit, restarts it, replays the
  active tab from its snapshot. Max 5 restarts without a successful title;
  a loaded page resets the budget. Governor reclaims do not count.
- **Tabs**: Chrome-like strip in the titlebar. Background tabs are metadata
  only (URL, label, scroll) — the single renderer holds the active page, so
  hibernation is structural, not a separate timeout. Switching a tab sends
  `restore`, and the shell keeps a FIFO of in-flight requests so a late title
  cannot land on the wrong tab.
- **Memory governor**: samples the renderer's private bytes every 3 s; two
  consecutive over-budget samples (`MAZDA_BUDGET_MB`, default 280) and a
  60 s cooldown trigger reclaim = restart + snapshot restore.
- **Custom chrome**: `with_decorations(false)`; 40 px titlebar that *is* the
  tab strip, 44 px toolbar (reload + address pill), 22 px status bar, window
  controls (min/maximize/restore/close), logo mark, drag-to-move, middle-click
  close, `+`, `Ctrl+T/W/R/L/Tab/1…9`. Light theme, 1120×760 default, 900×560
  minimum.
- **Diagnostics**: counting global allocator (`count-alloc`) reports live Rust
  bytes/peak/buckets; page graph registries (`LIVE_OBJS` / `LIVE_FNS` /
  `LIVE_ENVS`) let `JsEngine::drop` break object/closure cycles.

## Isolated by construction

Ten tabs cost ten metadata structs. The gate therefore measures shell + the
one renderer, not ten engines.

## 10 tabs, one active

`tools/measure-tabs.ps1` (release, new chrome, all ten tabs cycled):

| Metric | Value |
|---|---:|
| Settled before cycling (GitHub active) | 135.7 MB |
| Settled after cycling all ten | **180.8 MB** = 79.7 shell + 101.1 renderer |
| Peak during cycling | 238.7 MB |
| Renderer processes | **1** |
| Tabs restored with the requested URL | **10/10** |
| Gate ≤ 300 MB | **PASS** |

Per-tab restore was 0.2–14.2 s — that is the page's fetch/layout time (the
heavy pages dominate), not tab bookkeeping; background tabs cost no CPU and
no page memory.

## Crash recovery

`tools/crash-check.ps1`: kill the renderer, expect
`[supervisor] restart 1`, then the same tab's `[page]` again from a new
renderer PID. Result: **PASS** (restart, restore, new pid). Five crashes
without a successful title is the hard stop; a loaded page resets the count.

## Memory governor

`tools/governor-check.ps1` with `MAZDA_BUDGET_MB=1` and `example.com`:

```
[status] shell_private=72.3 renderer_private=3.6
[gov] renderer 3.6 MB > budget 1 MB
[gov] reclaimed renderer
[page] tab=0 https://example.com
```

**PASS**: over-budget detected, renderer restarted, page restored. At the
default 280 MB budget the governor never fires on the reference pages.

Idle shell (no page) is **59.8 MB** private + 3.1 MB renderer = 62.9 MB, well
under the 120 MB idle envelope.

## 100 reload cycles

Two instruments, because they answer different questions:

- **RSS (private bytes)** is the envelope metric and includes allocator pages
  committed for a page's transient parse peak. Those commits can step up late
  in a run and are never returned to the OS; a bounded cache behaves the same.
  The gate here is therefore *no unbounded growth*: the tail of the run must
  be a plateau within `max(3 MB, 2 %)`.
- **Live Rust heap** (counting allocator) is the exact leak signal: bytes
  allocated and not freed, independent of allocator slack. The gate is
  ≤ 2 % steady growth over the post-warm-up window (0.6 MB on a 30 MB page).

RSS, normal release build, 100 reloads each (10 s sampling):

| URL | first MB | steady MB | last MB | peak MB | tail spread | verdict |
|---|---:|---:|---:|---:|---:|---|
| `example.com` | 4.2 | 4.3 | 4.4 | 4.4 | 0.2 | plateau |
| Wikipedia (Web browser) | 24.9 | 27.8 | 39.7 | 39.7 | 1.3 (cycles 60–100) | plateau after a one-time commit at ~cycle 55 |
| Google | 34.8 | 51.3 | 54.0 | 54.7 | 0.7 | plateau after a one-time commit at ~cycle 30 |
| local external script (200 KB) | 10.1 | 9.3 | 10.1 | 10.1 | 0.8 | plateau |
| GitHub | not run (9–27 s per load) | | | | | live heap confirms flat |

The Wikipedia and Google steps are exactly the case the old +5 %-from-25 %
gate misread as a leak: the live heap stayed flat while the allocator
committed pages for the transient parse peak. The corrected gate is the tail
plateau, and the authoritative run is the live-heap one below.

Live heap, `count-alloc` build (diagnostic: RSS in this build is inflated by
the counting overhead and is not an envelope number):

| URL | live first MB | live steady MB | live last MB | steady growth | verdict |
|---|---:|---:|---:|---:|---|
| `example.com` | 2.1 | 2.1 | 2.1 | 0 % | flat |
| local external script (200 KB) | 6.0 | 6.0 | 6.0 | 0 % | flat |
| Wikipedia (Web browser) | 12.9 | 12.9 | 13.0 | 0.78 % | flat (RSS stepped 27→39 MB once) |
| Google | 11.1 | 11.1 | 11.1 | 0 % | flat (RSS stepped 24→47 MB once) |
| GitHub | 35.5 | 35.5 | 35.5 | 0 % | flat; diagnostic-build RSS 87→144 MB, early step then flat |

All five are PASS. The counts include the whole process (engine + fetch + paint),
so "live last" is the retained Rust heap after the final reload, not the
transient peak. The RSS carried by the diagnostic build is inflated by the
counting allocator and is not an envelope number; the normal-build RSS table
above is the memory figure.

`reload-soak.ps1` drains renderer stderr while it waits for a reply. Without
that, a verbose page (Wikipedia, Google) fills the stderr pipe and the renderer
blocks writing diagnostics, which looks exactly like a hung load.

## P1 gate

| Acceptance | Result |
|---|---|
| 10 tabs, 1 active, ≤ 300 MB | **PASS** — 180.8 MB, one renderer |
| Crash → tab recovers | **PASS** — supervisor restart + restore |
| 100 reload cycles, no leak | **PASS** — live heap flat (table above) |
| Memory governor | **PASS** — reclaim + restore |

## Reproduce

```powershell
cargo build --release --workspace --features mazda-engine/woff2
.\tools\measure-tabs.ps1 -Json .\tabs.json
.\tools\crash-check.ps1
.\tools\governor-check.ps1
.\tools\reload-soak.ps1 -Cycles 100

# exact leak gate
cargo build --release --workspace --features mazda-engine/count-alloc
.\tools\reload-soak.ps1 -Cycles 100 -LiveHeap -Url https://www.google.com
```

# P2 — real sandbox, executed (2026-11-08)

Build: `cargo build --release --workspace --offline --features mazda-engine/woff2`.
Host: Windows 11, standard user (no administrator token).

## What was built

New `crates/sandbox`:

- **Windows**: the renderer runs in an **AppContainer** (`MazdaBrowser.Renderer`)
  with only the `internetClient` capability. The shell spawns it with
  `CreateProcessW` + `PROC_THREAD_ATTRIBUTE_SECURITY_CAPABILITIES`, which needs
  no privileges (a standard user can do it) and gives the kernel-built lowbox
  token. A **Job Object** caps committed memory (`MAZDA_RENDERER_MB`, default
  512), allows exactly one process, applies UI restrictions (desktop,
  clipboard, system parameters, global atoms) and kills the renderer when the
  shell exits (`KILL_ON_JOB_CLOSE`). The child is created suspended, assigned
  to the job, then resumed, so no allocation or child spawn can outrun the
  limits.
- **Access**: `prepare_dir` creates `%TEMP%\mazda-sandbox-<shell pid>` and adds
  an inheritable package-SID ACE. `TMP`/`TEMP`/`LOCALAPPDATA` point there, so
  the frame file and cache are writable while `C:\Users\<user>` is not. The
  AppContainer redirects `%TEMP%` into
  `localappdata\Packages\mazdabrowser.renderer\AC\Temp`; that path is created
  up front. The renderer binary gets a read+execute ACE for the package SID.
- **Linux** (compile-checked, not yet run on a Linux host here):
  `PR_SET_NO_NEW_PRIVS`, `RLIMIT_AS`, and a seccomp BPF filter that returns
  `EPERM` for `execve`/`execveat` (threads keep working). Landlock and network
  namespaces are the next increment.
- **Probe**: `sandbox-probe` spawns itself through the same code path and
  checks from the inside; `tools/sandbox-check.ps1` runs it.

Why not `CreateProcessAsUserW`: the account holds no
`SeAssignPrimaryToken`/`SeIncreaseQuota`, so the call fails with 1314 even for
a restricted token. AppContainer via `SECURITY_CAPABILITIES` is the route that
works unprivileged — and it is the stronger isolation anyway.

## Acceptance

`tools/sandbox-check.ps1` (memory cap 96 MB):

```
PASS home read denied
PASS child spawn denied
PASS network ok
PASS temp writable
alloc denied
memory cap enforced
gate: home read denied, no child process, network ok, memory cap enforced -> PASS
```

- **Home directory**: reading a canary in `%USERPROFILE%` fails; the
  AppContainer token has no ACE on the profile.
- **Child process**: `cmd.exe /c exit 0` fails (job `ActiveProcessLimit = 1`).
- **Network**: `example.com:443` connects (internetClient capability); the real
  shell loads Google inside the container.
- **Memory**: a 224 MB commit in a 96 MB job is refused.
- **Real shell**: `crash-check.ps1` passes under the sandbox (page loads,
  renderer killed, supervisor restarts, page restored); example.com renders
  with system fonts in a screenshot.

### 10 tabs under the sandbox

`tools/measure-tabs.ps1`, ten real sites, one active, one renderer:

| Run | Before (shell+renderer) | After | Peak | Restored | Gate |
|-----|-------------------------|-------|------|----------|------|
| 1 | 117.1 MB | 174.5 MB | 238.5 MB | 10/10 | PASS |
| 2 | 114.6 MB | 181.8 MB | 240.2 MB | 10/10 | PASS |

Shell ~65–79 MB, renderer ~96–103 MB, one `renderer.exe` in both runs. The
AppContainer and job object add no measurable private bytes; the number is
unchanged from P1 (174–182 MB vs 180.8 MB), comfortably under the 300 MB
budget. Tab switches cost 1.3–15 s, dominated by the page fetch (no warm
cache in a fresh sandbox temp dir), not by the sandbox.

# P3 — media, executed (2026-11-08)

Build: `cargo build --release --workspace --offline --features mazda-engine/woff2`.
Host: Windows 11, standard user. Test clip: 3 s 320x180 H.264 baseline + AAC
(ffmpeg), page with two `<video>` (one `autoplay muted`, one click-to-play),
served by `python -m http.server`.

## What was built

New `crates/media` (`mazda-media`) — the whole playback path in Rust, borrowing
only the OS codecs:

- **`mp4.rs`** — pure-Rust ISO-BMFF walk: `ftyp`/`moov`, recursive `mdia`/`hdlr`,
  track count, size from `tkhd`, duration from `mvhd`, and a DRM byte scan
  (`encv`/`enca`/`sinf`/...). Four unit tests, no I/O.
- **Windows backend** — Media Foundation `IMFSourceReader` with
  `MF_SOURCE_READER_ENABLE_ADVANCED_VIDEO_PROCESSING` asking for `RGB32`, so
  video is decoded and converted at the display size; a negative stride is
  flipped on read. Audio runs on its own thread through WASAPI shared mode
  (reader format), started paused so a muted or idle player costs no endpoint.
- **`stub.rs`** — non-Windows returns `Unsupported`; the crate still compiles
  and links everywhere, so Linux keeps building.
- **`Player`** — `open` reads the file and rejects non-MP4/DRM *before* any
  decoder exists, then `play`/`pause`/`toggle`/`seek`/`next_frame`/`set_muted`/
  `set_looping`, paced on the wall clock.

`crates/media-probe` self-tests it: once directly, once through the real
sandbox (`mazda-sandbox::spawn`), because Media Foundation and WASAPI have to
work inside the AppContainer.

Engine integration (`crates/engine`):

- **`layout.rs`** — `<video>` is a replaced element (`MediaSlot`) sized from the
  `width`/`height` attributes; `width` with auto height takes the intrinsic
  ratio once probed; `object-fit` reuses the shared replaced-element rect.
  `<audio>` gets no box. Candidates: `src`, then every `<source src>` in order.
- **Worker** — `probe_media` fetches each candidate through the shared `fetch`
  path (20 MB cap, disk cache), parses the header, copies the bytes to
  `cache_dir()/media/<hash>.mp4`, decodes the first frame into `Layout::images`,
  and ships a `MediaEntry` (file, draw rect, flags) on the page wire.
- **Renderer** — opens its own `Player` for a `MediaEntry` and replaces that
  image's pixels per frame. Only `autoplay muted` starts by itself; every other
  video opens lazily on the first click (no decoder memory until watched), and
  a click inside the video rect toggles play/pause. Players are dropped on
  navigation, and the 16 ms loop repaints only while something plays.

## Acceptance

`media-probe`, plain and sandboxed:

```
PASS decoded 59 frames
PASS audio endpoint
PASS DRM rejected
media probe: all checks passed
```

Local page: 172 frames in 6 s, correct colours and orientation; a 160 px-wide
video lays out at the true 16:9 height; click pauses (86 frames, ≈3 s played);
a click on the second, non-autoplay video prints `play (opened)`, then `pause`.

Real page `https://www.w3schools.com/html/html5_video.asp`: 2 playable videos,
0 refused; click → `[media] play`, 100 frames; click again → `[media] pause`.

Sandboxed shell (the real product path): the same page loads with
`[media] 2 playable, 0 refused` and `2 player(s) handed to the renderer`, the
painted frame advances while it plays, a click prints `play` and the next click
`pause`. Shell + renderer while playing: **76.4 + 83.6 = 160.0 MB WS**
(74.6 + 58.2 MB private) — under the 350 MB video envelope. Closing the shell
takes the renderer with it (job object).

Memory, release renderer, 1000x700:

| Scenario | Working set | Private |
|---|---:|---:|
| idle, no page | 10.0 MB | 3.7 MB |
| page loaded, `MAZDA_NO_MEDIA=1` | 15.9 MB | 5.8 MB |
| same page, two videos playing | **62.9 MB** | **42.5 MB** |

Media cost while playing: **+47 MB WS / +36.7 MB private** — inside the +80 MB
budget; with no media (or playback disabled) the cost is **0**. Eight reloads
of the playing page: WS flat at 61.2–61.7 MB (no player leak).

Gate: **PASS** — a local MP4 and a real page video play, +47 MB ≤ +80 MB,
0 MB idle, DRM refuses before a decoder is created.

## Shortcuts (deliberate)

- Every video on a page can play at once (one decoder each); there is no
  per-page or per-tab media budget yet, and no audio ducking.
- No `HTMLMediaElement` JS API and no native controls: clicks toggle
  play/pause, and playback state is invisible to the DOM.
- MP4/H.264/AAC only (what Media Foundation is sure to have); no WebM/VP9;
  no streaming, MSE or EME — the whole file is fetched.
- The HTTP disk cache refuses bodies > 5 MB, so large clips are re-fetched and
  copied to `cache_dir()/media/` every load instead of replaying from cache.
- AppContainer loopback is still blocked: local test pages need
  `MAZDA_SANDBOX=off` until a loopback exemption or socket broker exists.

## Reproduce

```powershell
# 3 s 320x180 H.264 + AAC clip
ffmpeg -f lavfi -i testsrc=size=320x180:rate=30 -f lavfi -i sine=frequency=440 `
  -t 3 -c:v libx264 -pix_fmt yuv420p -c:a aac "$env:TEMP\mazda-test.mp4"
cargo run --release -p media-probe -- --file "$env:TEMP\mazda-test.mp4"
# runs twice: plain, then through the real sandbox

# local page (renderer unsandboxed: AppContainer blocks loopback)
python -m http.server 8766 --bind 127.0.0.1 --directory "$env:TEMP\mazda-media-page"
$env:MAZDA_SANDBOX="off"; .\target\release\renderer.exe --size 1000x700

# A/B the media cost: same page with playback probed out
$env:MAZDA_NO_MEDIA="1"; .\target\release\renderer.exe --size 1000x700
```

# P4 - browser gaps (in progress)

## Downloads, executed (2026-11-08)

`<a href download>` is a save, not a navigation. The renderer collects every
such link at layout time (`Page.downloads`), and a click on one starts a
download thread instead of emitting `nav`.

**Streaming, not buffering.** The renderer opens the request with the same
agent/cookies/headers as navigation, then copies `into_reader()` into a file in
`%TEMP%\mazda-downloads\` in 64 KB chunks. It reports `done`/`total` every
150 ms (plus a final 100% tick), so a 20 MB file neither blocks the page loop
(main loop polls at 50 ms while active) nor holds the body in RAM.

**The shell owns the profile.** The renderer is sandboxed and cannot write
`%USERPROFILE%`, so `DownloadDone` carries a temp path and the trusted shell
moves it into `%USERPROFILE%\Downloads` (rename, falling back to copy+remove
across volumes) with `name (n)` dedupe. `DownloadFailed` surfaces the error.

**Names are sanitized.** The `download` attribute is used when present, else
the URL's last segment; path separators, control characters and Windows-reserved
characters become `_`, leading/trailing dots are stripped, and an empty result
falls back to `download`. A page cannot choose a path outside the temp dir.

UI: a toolbar download button (accent dot while a transfer is live) toggles a
Downloads panel with a progress bar per active transfer and clickable saved-file
links; the status bar reads `Downloading <name> — <pct>`.

## Acceptance

Headless renderer, `data:` page with one download link:

```
download 0 0 example.html
download 577 577 example.html
download-done C:\...\Temp\mazda-downloads\example.html
```

Split shell, local page `http://127.0.0.1:8767/dl.html` (unsandboxed renderer:
AppContainer blocks loopback), two links:

```
[download] movie.mp4 0/788493
[download] movie.mp4 23760/788493
[download] movie.mp4 145165/788493
[download] movie.mp4 275421/788493
[download] movie.mp4 516096/788493
[download] movie.mp4 788493/788493
[download] saved C:\Users\werlos\Downloads\movie.mp4
```

788493 bytes on disk, equal to the server's `Content-Length`. A second click
saved `movie (1).mp4` (dedupe, no overwrite). The panel showed a live `9 %`
bar, the finished entry as a link, and the status bar
`Downloading movie.mp4 — 9 %`. The toolbar badge was also clicked with posted
messages in the later find/zoom run: the panel listed `data_text.pdf saved`
and stayed open across the steps (`%TEMP%\mazda-ui-test11\1-downloads.png`).

## Shortcuts (deliberate)

- No cancel/pause/resume, no retry, no `Content-Disposition` filename
  preference (the `download` attribute and URL win), no download history on
  disk: the panel is per session.
- Downloads ignore the memory governor and the disk cache; the file streams
  straight to disk and is not capped.
- Multiple simultaneous downloads are allowed; the panel shows them all but the
  status bar only names the newest active one.
- `data:`/`blob:` targets are not downloadable: download hrefs go through
  `resolve`, which only accepts `http(s)`.

## Reproduce

```powershell
# page with two download links, served locally
python -m http.server 8767 --bind 127.0.0.1 --directory "$env:TEMP\mazda-dl-page"
$env:MAZDA_SANDBOX="off"; .\target\release\shell.exe http://127.0.0.1:8767/dl.html
# click a link -> Downloads\file; click the toolbar button -> panel
```

## File upload, executed (2026-11-08)

`<input type=file>` is a real control now: layout gives it `FormKind::File`
and the UA label `Choose File`; once a file is picked the live value map shows
the filename in the same box.

**The picker is the shell's job.** A click on the control emits
`pick-file <node>` instead of doing anything in the page. The trusted shell
opens the native `GetOpenFileNameW` dialog (modal to its own window), then
copies the file into the renderer's granted temp dir at
`...\mazda-sandbox-<pid>\tmp\mazda-uploads\<name>` — the renderer cannot read
the user profile, so the bytes have to cross as a copy. The chosen name and
staged path go back as `file-chosen <node> <name>\t<path>`; the renderer stores
a `FilePick` by node id and re-lays out, so the control shows the filename. Each
pick gets a fresh sandbox-local copy; `stage_upload` unit test covers the
`name (n)` dedupe.

**Submission is multipart.** A POST (submit button, Enter, or a scripted
`form.submit()`) with a picked file, or with
`enctype="multipart/form-data"`, builds `multipart/form-data` with a
per-submission boundary: text pairs first, then one part per picked file with
`Content-Type: application/octet-stream` and the user-visible filename (quotes,
backslashes and line breaks escaped). A file that cannot be read is logged and
skipped; the rest still goes out. GET always stays urlencoded. The body is sent
through `send_bytes`: `send` treats a byte slice as a reader and would use
`Transfer-Encoding: chunked`, which some form endpoints reject.

## Acceptance

Headless renderer, local page (`MAZDA_SANDBOX` irrelevant here) with a text
field, a file input and a submit button; stdin drove the flow:

```
click 20.0 60.0
pick-file 12
file-chosen 12 My Notes.txt<TAB>C:\...\mazda-up-page\My Notes.txt
[form] picked My Notes.txt (...) for node 12
click 20.0 100.0
[form] submit method=POST url=http://127.0.0.1:8768/echo body=328 bytes (multipart/form-data; boundary=----MazdaFormBoundary3cc818e629d3413db318)
```

The echo server received `Content-Length: 328` and the exact body:

```
------MazdaFormBoundary3cc818e629d3413db318
Content-Disposition: form-data; name="note"

hi
------MazdaFormBoundary3cc818e629d3413db318
Content-Disposition: form-data; name="doc"; filename="My Notes.txt"
Content-Type: application/octet-stream

hello upload
second line

------MazdaFormBoundary3cc818e629d3413db318--
```

The page then loaded the server's `uploaded` response through the normal POST
navigation path. Workspace tests: 899 engine + 1 shell + 5 IPC + 5 media +
4 probe green.

The native picker itself was not driven by synthetic input on this host (a
fullscreen game owned the desktop); `pick_file` is a direct `comdlg32` call and
`stage_upload` plus the renderer half are covered above and by tests. Manual
check: click the box, choose a file, the box shows its name, then Send.

## Shortcuts (deliberate)

- One file per `<input>`; no `multiple`, no drag-and-drop, no `accept` filter
  (the dialog lists all files).
- Page scripts never see the path or the bytes: `value` is the filename and
  `input.files` is not implemented. A scripted `form.submit()` therefore posts
  no file part even after a pick (only the UI-driven submit carries it).
- No upload progress beyond the POST itself (no chunked progress events) and no
  resumable uploads.
- The picker is Windows-only; other builds log `the OS file picker is
  Windows-only for now` and leave the control unchanged.
- Staged copies are not deleted after submission: they live in the sandbox temp
  dir, which the shell recreates per run.

## Reproduce

```powershell
# echo server + page (also used by the download repro)
python "$env:TEMP\mazda-up-page\echo.py"   # writes the last POST to last.txt
$env:MAZDA_SANDBOX="off"; .\target\release\shell.exe http://127.0.0.1:8768/up.html
# click the file box -> pick a file -> the box shows the name -> Send
```

## Find in page, executed (2026-11-08)

Ctrl+F opens a find bar over the top-right of the page: a text box, an
`active/total` counter and a close button. Typing sends `find <query>`, Enter
sends `find-step 1` (Shift+Enter `-1`), Escape or the close button sends
`find-close`.

**The renderer owns the hits.** Every non-empty query scans `Layout::runs`
(case-insensitively) and records one `FindHit` per match: document-space rect,
`fixed` flag, and a 1-based active index. The scan runs again after every
adopted layout (navigation, resize, relayout), so the counter survives a page
change and the query stays in the bar like a browser's. A new query starts at
its first hit; `find-step` wraps around. Each step emits `scroll-to <y>` (the
offset that brings the hit into view with a 40 px margin; fixed hits never
scroll) and `find-result <active> <total>`; the shell adopts the offset into the
active tab's remembered scroll. An empty `find` or `find-close` drops every
hit, emits `0 0` and repaints.

**Highlights are a post-render blend.** `headless_paint` renders the page into
the frame, then `paint_find` alpha-blends one rect per hit on top (inactive
`#ffeb3b` at 35%, active `#ff8a00` at 45%) before the scrollbar goes down. No
layout or paint code knows about find: closing the bar simply repaints without
the overlay.

## Acceptance

Headless renderer against a local page with 81 occurrences of "alpha" plus two
of "needle" (the second one deep in the document); stdin drove the flow:

```
find alpha
scroll-to 0.0
find-result 1 81
find-step 1
find-result 2 81
find-step 1
find-result 3 81
find-step -1
find-result 2 81
find needle
scroll-to 561.4
find-result 1 2
find-step -1
scroll-to 594.3
find-result 2 2
find-close
find-result 0 0
find alpha
find-result 1 81
find
find-result 0 0
```

Pixel check on the published frame: the plane painted with an active
`find alpha` result has 2,845 yellowish and 127 orange pixels; the next plane
(painted by the bare `find` clear) has none. Two new unit tests cover the
scroll clamp and the blend math. Workspace tests: 901 engine + 7 shell + 5 IPC
+ 4 media green.

The real shell was then driven through its stdin control channel (`find alpha`, `find-step 1`, `find-step -1`,
`find-close`), because this host's fullscreen game takes the foreground back
before a synthetic Ctrl+F can land. The bar showed `alpha`, `1/3` and a `×`; the
`<h1>` hit was orange and the two paragraph hits yellow
(`%TEMP%\mazda-ui-test11\2-find.png`). Step and back logged `2/3` and `1/3`,
close logged `0/0` and repainted without highlights
(`3-find-step.png` / `4-find-closed.png`). The Ctrl+F, Enter and Esc handlers
call the same methods, so the keyboard path ends in the code verified here.

## Shortcuts (deliberate)

- Matches are per text run: a phrase split across runs (an inline `<b>` in the
  middle) is not found, and RTL/bidi text is scanned in logical order.
- The highlight x/width are estimated by character share of the run, not the
  real per-glyph advances (layout does not keep them), so a hit's box can be a
  character wide of the glyphs.
- Case folding uses Rust's `to_lowercase`; when folding changes byte length the
  scan falls back to a case-sensitive match rather than mapping offsets.
- No whole-word / regex / match-highlight-all options, no find history, no
  scrollbar tick marks, and typing does not live-scroll: the first hit is shown
  when the query changes and further hits only on Enter.
- Find state is per renderer, so switching tabs keeps the bar and rescans the
  newly adopted page; there is no per-tab query memory.

## Reproduce

```powershell
# any long page served locally
python -m http.server 8769 --bind 127.0.0.1 --directory "$env:TEMP\mazda-find-page"
$env:MAZDA_SANDBOX="off"; .\target\release\shell.exe http://127.0.0.1:8769/find.html
# Ctrl+F, type "alpha", Enter cycles, Esc closes
# The same bar is reachable from stdin once the page is loaded:
#   find alpha / find-step 1 / find-close
```

## Zoom, executed (2026-11-08)

Ctrl+= / Ctrl+- step through 25% … 500% (Ctrl+0 is 100%); the shell shows the
percentage in the status bar. Each step sends `zoom <factor>`; the renderer
clamps to `0.25..=5.0` and re-lays the kept page out at the new CSS width.

**Layout in CSS pixels, frame in physical pixels.** `zoomed_w/zoomed_h` divide
the physical viewport by the factor before layout, so a zoomed-in page reflows
exactly as if the window were narrower. `headless_paint` then paints into a
`pw/zoom × ph/zoom` buffer and bilinearly scales it to the frame's physical
size. Scroll offsets stay in CSS pixels end to end (the shell divides wheel
deltas by the factor), and `Command::Click` divides the shell's physical
coordinates before the hit tests. At 100% the scale is skipped entirely, so the
common path is byte-identical to before.

## Acceptance

Headless renderer on a page of long paragraphs (reflow is visible), stdin
drove the flow:

```
zoom 2.0   doc_height 367.2 -> 799.2   (half the CSS width, more lines)
zoom 0.5   doc_height -> 194.4         (double the CSS width, fewer lines)
zoom 1.0   doc_height -> 367.2
find needle (at 200%)  scroll-to 488.2  find-result 1 1
zoom 99    ignored (out of range, no repaint)
```

Frame check: the first text band is 17 px tall at 100%, 32 px at 200% and 8 px
at 50%; ink pixels 56k / 107k / 18k for the same viewport. A new shell unit
test covers the step ladder and its sticky ends. Workspace tests: 901 engine +
7 shell + 5 IPC + 4 media green.

The real shell was driven through the stdin verbs (`zoom in` twice, then
`zoom reset`) because the key chords cannot hold focus on this host. The status
badge read 110%, then 125%, then dropped back to the 100% default, and the
shell logged `[zoom] 1.10`, `1.25`, `1.00` (`%TEMP%\mazda-ui-test11\5-zoom-in.png`
… `7-zoom-reset.png`). Each step also logged `[nav] worker finished …` and
`[page] …`: those are the zoom relayout jobs at the new CSS width, not page
loads. Ctrl+= / Ctrl+- / Ctrl+0 run the same `zoom_by` path.

## Shortcuts (deliberate)

- Zoom is a raster scale: at 100% layout and frame match exactly, above 100%
  the already-painted CSS frame is upscaled bilinearly (text gets soft rather
  than re-rasterized at the new size). A vector-crisp zoom would need paint to
  run at the physical scale.
- One zoom for every tab; no per-site zoom memory, no Ctrl+wheel, no pinch.
- Fixed elements, media rects and find highlights all scale with the frame;
  there is no separate compositor layer to keep crisp.
- 25% allocates a temporary 10.7 M-pixel CSS buffer (about 43 MB) for the
  duration of one paint, on top of the physical frame.

## Reproduce

```powershell
python -m http.server 8769 --bind 127.0.0.1 --directory "$env:TEMP\mazda-find-page"
$env:MAZDA_SANDBOX="off"; .\target\release\shell.exe http://127.0.0.1:8769/zoom.html
# Ctrl+= twice, Ctrl+-, Ctrl+0
# or send zoom in / zoom in / zoom reset on stdin after the page loads
```

## Print to PDF, executed (2026-11-08)

The toolbar printer button (left of the download badge) or Ctrl+P sends
`print`. The renderer answers with `print-done <path>` once the file is in its
temp dir, or `print-failed <err>`; the shell moves the finished file into
Downloads as `<host>.pdf` and records it like a download.

**Slices, not pagination.** The print path reuses the page that is already laid
out instead of asking layout for page breaks. Each A4 page covers
`(1123 - 96) / scale` CSS pixels of document height, where `scale` maps the
physical page width onto `794 - 96` printable pixels. `render` paints that
slice over the page background, `scale_pixels` resizes it to the printable
width, and the result is composited into a white 794x1123 page with a 48 px
margin. The RGB plane is zlib-compressed (`/Filter /FlateDecode`) and
`build_pdf` writes catalog, pages, page, content-stream and image objects with
a real xref table (`startxref` points at `xref`). MediaBox is `[0 0 595 842]`;
the content stream draws one image across the whole box because the margin is
already baked into the bitmap.

## Acceptance

Headless renderer against a local page of 70 paragraphs (`print.html`):

```
print
[print] 3 pages, 667853 bytes -> C:\...\Temp\mazda-print\page-468.pdf
print-done C:\...\Temp\mazda-print\page-468.pdf
```

The file was checked independently: `%PDF-1.4` header, 3 `/Type /Page`
objects and one `/Type /Pages` node with `/Count 3`, three
`/Subtype /Image` objects, and `startxref 667545` landing exactly on the
`xref` keyword. Each image stream decompresses to 2,674,986 bytes
(`794*1123*3`), and the three pages hold 95,903 / 96,658 / 6,460 ink pixels -
the last is sparse, as expected for the short tail of the document (70
paragraphs do not divide evenly into the page height). One new engine unit
test builds a two-image PDF and checks the page/image counts and the xref
offset against the raw bytes. Workspace tests: 901 engine + 7 shell + 5 IPC +
4 media green.

The printer button was clicked in the real window by `tools/ui-smoke.ps1`
(posted messages, no cursor movement): it wrote a 5,448-byte PDF into the
scratch Downloads folder. A second run on a `data:` page wrote 5,202 bytes and
the Downloads panel listed `data_text.pdf saved`. Ctrl+P itself cannot be held
on this host, but it dispatches to the same `print` command.

## Shortcuts (deliberate)

- Pages are bitmaps: the PDF has no text layer, so text is not selectable or
  searchable and the file is larger than a vector print would be.
- Page breaks are fixed-height slices, not paragraph-aware pagination; a line
  can be cut in half at a boundary. There are no headers, footers or page
  numbers.
- Fixed-position elements are re-rendered at the top of every page (there is no
  compositor to place them per page), so a fixed header repeats.
- One paper size (A4) with a fixed 48 px margin, no page range, scale or
  orientation options; the whole document is painted into page images while the
  PDF is assembled, so peak memory grows with page count.
- The shell name comes from the tab label (the host), not the page title, and a
  print in flight dies with a renderer restart.

## Reproduce

```powershell
python -m http.server 8769 --bind 127.0.0.1 --directory "$env:TEMP\mazda-find-page"
$env:MAZDA_SANDBOX="off"; .\target\release\shell.exe http://127.0.0.1:8769/print.html
# click the printer button (or Ctrl+P); the PDF appears in Downloads
```

## History and bookmarks, executed (2026-11-08)

The clock button (or Ctrl+H) opens a "Bookmarks & History" window anchored
under the toolbar with bookmarks on top and the visit log below. The star button (or
Ctrl+D) adds the active page to the bookmarks and fills the star while that URL
is saved. Clicking a bookmark or a visit navigates the active tab and closes
the panel; bookmarks have a `×` remove button and the log has `Clear`.

**Two text files in the trusted process.** Both stores live under
`%LOCALAPPDATA%\mazda-browser` (XDG on Linux) and are read once at startup.
Every committed load appends `<secs>\t<url>\t<title>` to `history.tsv`; a
reload of the last URL collapses into that row instead of adding another, and
the list is capped at 5,000 entries. Teammate tabs and crash/governor replays
send `Restore`, which is deliberately not logged - each `inflight` request now
carries a `record` flag beside its tab and URL. A bookmark toggle rewrites the
small `bookmarks.tsv` immediately. Fields are stripped of tabs and newlines
before writing, so a page cannot corrupt the format, and every disk error is
ignored so the session list keeps working even when the profile is read-only.

**UI stays shell-only.** The renderer and the IPC protocol are untouched: this
is trusted-process chrome, like tabs and downloads. The panel is an egui
`Window` with a capped `ScrollArea` of at most 300 visits, and each visit's
age is formatted as "just now", "N min", "N h" or "N d".

## Acceptance

`cargo test -p mazda-shell` is green with 6 tests. Three cover the store:

```
history_lines_round_trip_and_collapse_reloads
  two URLs + one reload -> 2 entries, reload takes the newest timestamp
  tabs/newlines are cleaned, malformed lines are skipped
ages_are_human_readable
  just now / 5 min / 3 h / 2 d
stores_round_trip_through_disk
  append -> load -> write bookmarks -> load -> clear, against a scratch
  LOCALAPPDATA so the real profile is untouched
```

Workspace tests: 901 engine + 7 shell + 5 IPC + 4 media green.

The panel and both buttons were driven on this host without touching the real
cursor. `tools/ui-smoke.ps1` posts `WM_MOUSEMOVE`/`WM_LBUTTONDOWN`/
`WM_LBUTTONUP` to the shell window and reads the scratch stores; `PrintWindow`
with `PW_RENDERFULLCONTENT` captures the wgpu window even when another window
owns the foreground. One run: printer button -> `[print] saved
...\Downloads\data_text.pdf` (5,448 bytes, one page); star -> `bookmarks.tsv`
written and 106 blue star pixels; star again -> file empty and 0 blue pixels;
clock -> the panel with the bookmark row and a "just now" visit.

Ordering matters for the click: send the park move, the target move, the press
and the release back to back. A sleep between messages lets Windows post
`WM_MOUSELEAVE`, which clears egui's pointer position, so the press is
silently dropped. The park move also defeats winit's duplicate-position filter
(a second move to the same pixel emits no `CursorMoved`). Both `✕` buttons
(panel close, bookmark remove) had no glyph in the shell font and rendered as
hollow boxes, so they are now `×`.

## Shortcuts (deliberate)

- Plain tab-separated files, loaded whole: no index, query, folders, tags,
  import/export, favicons or sync. 5,000 visits is the cap.
- Only fresh `Load`s are logged; switching tabs replays from memory snapshots
  and does not create history entries (so the log is a visit log, not a
  session transcript).
- History click records the new visit, so an older row stays in the log and a
  fresh row appears at the top; only consecutive reloads collapse into one
  row.
- Clearing history keeps bookmarks, and the panel scrolls only the log (the
  bookmark list is short by assumption).
- Title text is the tab label (the host), because the renderer's `title` event
  carries the URL; there is no page `<title>` yet.
- No confirmation dialog for Clear; the file is removed and the in-memory list
  cleared.

## Reproduce

```powershell
python -m http.server 8769 --bind 127.0.0.1 --directory "$env:TEMP\mazda-find-page"
$env:MAZDA_SANDBOX="off"; .\target\release\shell.exe http://127.0.0.1:8769/find.html
# Ctrl+D, then Ctrl+H, click the visit, Clear
```

Same buttons without a cursor, no focus change:

```powershell
powershell -NoProfile -File tools\ui-smoke.ps1
# PASS: chrome clicks, stores and screenshots in ...\Temp\mazda-ui-smoke
```

## JavaScript performance baseline (2026-11-09)

P5 starts with a number, not an optimization. Two binaries measure the engine
as shipped: a test262 runner for correctness and a small bench set for speed.
The point is to pick the first change from evidence - the interpreter is a
tree-walker with linear property lookup, so the guess was property access, and
the bench had to confirm it.

The tables below are the before numbers. Step 1 (own-property fast path) is
recorded at the end of this section with its own A/B measurement.

### What was built

- `crates/engine/src/bin/test262.rs` parses the suite's `/*--- ---*/`
  frontmatter (includes, flags, negative phase/type), prepends `sta.js` and
  `assert.js` (plus any file an `includes` entry names), and runs each test
  once. A panic is caught per file, so one pathological test cannot abort the
  run; with `MAZDA_TEST262_TRACE=1` every path is printed as it starts, which
  makes a hung run diagnosable.
- `crates/engine/src/bin/js-bench.rs` runs small programs
  (`benches-js/loop.js`, `fib.js`, `property.js`, `array.js`, `string.js`,
  `closure.js`, plus `call.js` from step 2) N times and reports
  min/median/mean. Every sample builds a
  fresh `JsEngine`, so parse plus execution is included - what a page pays per
  script. Each program checksums its own result, so a broken optimization
  cannot silently benchmark different work.

### test262: what the number means

The suite is checked out sparsely (only `test/language` and `harness`) and run
in **non-strict mode only**. This is a measurement of the engine that exists,
not a conformance claim:

| | count |
|---|---:|
| `.js` files in `test/language` | 24,115 |
| files after excluding `_FIXTURE` fixtures | 23,832 |
| executed | 17,678 |
| **pass** | **6,950 (39.3% of executed)** |
| fail | 10,728 |
| skipped | 6,154 |

Skips are deliberate: `onlyStrict` (strict mode is not implemented, running it
sloppily would test different semantics), `module` and `async` flags, and
module-resolution phase tests. They are reported as skips, not passes and not
failures. 17.8 s total on an idle host (0.7 ms per executed test), zero
panics. The first run took 193 s while the host was busy and produced the
identical counts; wall time moves with machine load, pass/fail counts do not.

The failure list is useful as a work queue, not just a score:

- `eval-code` (7 pass / 272 fail) and `arguments-object` (19 / 175) are
  dominated by one missing feature each: `eval` and a real `arguments` object.
- `expressions` (3,379 / 5,285) is the broad bucket. Early samples show boxed
  primitives, classes/trailing commas, object `ToPrimitive`, and
  `Test262Error` printing as `[object Object]` in the failure reason.
- Many `[object Object]` failures are the assert helpers throwing a value the
  print path does not format; the test still failed, but the reason is opaque.

### Benchmarks (median of 10 runs, 3 warmup)

| program | min | median | mean |
|---|---:|---:|---:|
| loop x300k | 71.62 ms | **75.02 ms** | 75.97 ms |
| fib(22) | 31.81 ms | **33.20 ms** | 33.57 ms |
| property x300k | 249.30 ms | **271.70 ms** | 271.64 ms |
| array x100k | 85.93 ms | **95.33 ms** | 95.28 ms |
| string x2k | 15.84 ms | **18.54 ms** | 18.30 ms |
| closure x300k | 209.95 ms | **224.52 ms** | 231.88 ms |

`call x300k` was added during step 2, after a diagnostic run showed calls were
the larger cost; its before/after lives in that section.

### Interpretation

Property-heavy work is the outlier: 300,000 property reads/writes cost ~3.6x
the arithmetic loop and ~2.9x the closure loop, while the raw loop and `fib`
show the evaluator itself is not the bottleneck. `Obj.props` is a linear
`Vec<(name, value)>`; every `obj.x` walks the whole list, so a call-heavy script
pays again through `this` and globals.

Order for P5, from this data:

1. Property access: string-keyed map or a small inline cache keyed by
   `(shape, name)`, with the `Vec` retained for ordering when enumerating.
2. Object shaping (same key sequence -> same shape) so a cache can be hit.
3. Calls and closures (`this`, environment lookup) once property reads stop
   dominating; `closure x300k` is the second-largest number.
4. Bytecode only if the tree-walker's arithmetic/loop numbers stop improving -
   the loop and `fib` numbers do not justify it yet.

A rerun of this exact bench and the same test262 invocation is the before/after
contract for each step.

### Step 1: own-property fast path (2026-11-09)

The first change follows the data and touches only the common case:
`get_member`/`set_member` now check a plain object's own property first, in one
borrow, before the lazy-prototype attach and the exotic-kind probes. A read
that hits a data property returns immediately; a write that overwrites one
does the same. Accessors, misses, arrays, functions, host wrappers and the
prototype chain still take the old path, so semantics are unchanged.

**Method.** Both binaries were built from this commit and its parent with the
same release profile. They were then run alternately (the same host, the same
10-iteration/3-warmup command) so machine state affects both sides:

```powershell
.\target\release\js-bench.exe --iterations 10 --warmup 3 --filter property
```

Property-only medians across seven alternating rounds:

| | round 1 | 2 | 3 | 4 | 5 | 6 | 7 | median |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| before | 246.9 | 305.4 | 229.0 | 227.0 | 244.2 | 237.9 | 245.1 | **244.2 ms** |
| after | 197.3 | 206.9 | 190.6 | 196.8 | 213.3 | 221.5 | 211.1 | **206.9 ms** |

Minimums moved the same way (fastest sample across all rounds: 213.7 ->
180.0 ms, -16%). That is **-15% median** on the benchmark the baseline
singled out. As a load-independent check, property now costs 2.7x the
arithmetic loop instead of 3.6x.

Unrelated benches drifted by up to ~20% between the two binaries (the loop
benchmark, which executes no member access, moved 61-76 ms), so no claim is
made for them; only the targeted benchmark is compared.

### Step 1: correctness

Same test262 invocation, same 23,832 files: **17,678 run, 6,950 pass, 10,728
fail, 6,154 skip** - identical counts to the baseline, zero panics. The change
moves speed, not behavior. Workspace tests stay green (902 engine).

### Step 2: call frames (2026-11-09)

A temporary diagnostic bench (`s += f()` 300k times) showed the interpreter
spends more in one call than in four property operations, so calls became the
next target. The step removes per-call work rather than adding a compiler:

- `this` and `new.target` are no longer bindings. Each `Env` has a `this` and
  `new_target` slot; block scopes leave them empty and the lookup walks to the
  nearest function frame. One ordinary frame stores `Some(Undefined)` for
  `new.target`, which shadows an outer constructor exactly like the spec;
  arrows leave the slot empty so they see the enclosing function's value.
- `arguments`, `this` and `new.target` used to build a fresh `Rc<str>` name on
  every call. The one binding that remains (`arguments`) reuses a cached name,
  so a zero-argument call now allocates only its `Env` (the bindings `Vec`
  stays empty).
- The global frame carries the global object as `this`, so top-level `this`
  stays the global object now that it no longer resolves through a binding.

`call x300k` joined the permanent bench set (`benches-js/call.js`), so call
cost is measured directly from now on.

**Method.** Same as step 1: binaries from the step-1 commit and this working
tree, same release profile, three alternating full runs.

| program | before (median of 3) | after (median of 3) | change |
|---|---:|---:|---:|
| call x300k | 181.7 ms | **130.7 ms** | **-28%** |
| closure x300k | 225.8 ms | **163.4 ms** | **-28%** |
| fib(22) | 29.9 ms | **26.3 ms** | **-12%** |
| property x300k | 188.2 ms | 202.4 ms | wash (see below) |

Property looked 7% slower in the interleaved full runs, so it was re-measured
on its own with the order reversed: after 188.9 / 202.7 / 191.9 ms against
before 191.8 / 203.6 / 177.8 ms. That is a wash, not a regression; the call
numbers reproduce in both orders.

### Step 2: correctness

Test262 initially lost 47 tests: top-level `this` had been resolving through
the global object's own `"this"` property, and the frame walk returned
undefined once that fallback was gone. Giving the global frame an explicit
`this` restored the exact baseline counts: **17,678 run, 6,950 pass, 10,728
fail, 6,154 skip**, zero panics, 15.2 s. Two regression assertions cover
top-level `this === globalThis` and the plain nested call that must shadow an
outer `new.target`. Workspace tests stay green (902 engine).

### Step 3: recycled call frames (2026-11-09)

A call still allocated and freed one `Env` each time. Two changes make that
cost disappear for ordinary calls:

- The page registry that breaks reference cycles now records a scope only when
  a closure captures it. A scope can only enter a cycle through a `Closure`
  (the sole `Rc<Env>` owner besides the call stack), so uncaptured frames do
  not need a `Weak` entry. This also removes a thread-local push from every
  call.
- A call frame goes back to a per-engine pool (cap 64) after the body if its
  `Rc` is unique. A closure or generator that captured the frame keeps its own
  `Rc`, so the strong-count check alone decides safety; an escaped frame is
  simply dropped as before. The pooled frame keeps its binding-`Vec` capacity,
  so recursive calls reuse both the allocation and the storage.

`Rc::get_mut` refuses a value that still has a `Weak`, which is exactly why
the registry had to move first: a frame with a registry entry is never
recycled (safe fallback, just no reuse).

**Method.** Same alternating A/B against the step-2 binary, three full runs.

| program | before (median of 3) | after (median of 3) | change |
|---|---:|---:|---:|
| fib(22) | 24.7 ms | **16.8 ms** | **-32%** |
| closure x300k | 158.8 ms | **131.8 ms** | **-17%** |
| call x300k | 122.3 ms | **106.7 ms** | **-13%** |
| property x300k | 187.6 ms | 164.3 ms | bonus, not claimed |
| array x100k | 77.0 ms | 79.3 ms | wash |
| loop x300k | 63.2 ms | 62.7 ms | wash |

`fib` gains most because recursion reuses one frame per depth instead of
allocating ~57k frames; the flat call bench gains less because its loop frame
is already reused and only the callee frame cycles.

### Step 3: correctness

Test262 counts are again identical: **17,678 run, 6,950 pass, 10,728 fail,
6,154 skip**, zero panics, 16.1 s. Escaping frames are covered by the existing
closure, generator and `Function.prototype.bind` tests, and the suite's
17,678 fresh engines exercised pool teardown on every test. Workspace tests
stay green (902 engine).

### Step 4: page-level timing (2026-11-09)

The bench programs are synthetic, so the question is whether the gains reach a
real page. `run_scripts` now logs its own turn under `MAZDA_TIME=1`:

```text
[time] scripts: 1581 ms (fetch 1417 ms, exec 164 ms)
```

`fetch` is the time this worker thread spent inside `crate::fetch` during the
turn (classic scripts, module graphs, and `fetch()` calls). `exec` is the
remainder: parse + evaluation. The split matters because page load fetches
scripts synchronously; without it a reference site's "script time" is mostly
download time.

**Method.** Baseline is worktree `%TEMP%\mazda-p5-base` at `dbddfa7` (before
step 1) with only the timing patch applied; both binaries were built from the
same host and run alternately, three pairs per page.

| page | exec before (median of 3) | exec after (median of 3) | change |
|---|---:|---:|---:|
| Google | 141 ms | 147 ms | wash |
| YouTube | 42 ms | 43 ms | wash |
| GitHub | 250 ms | 251 ms | wash |

That is the expected result, and it is not evidence against the steps. On all
three pages the big bundles abort at parse before the call/property-heavy code
runs: GitHub's `core.js` stops at line 27 (`yield` form), its marketing bundle
fails on a source character, and the Google/YouTube bundles hit the same
parser gaps. What the turn does execute is bootstrap code with few calls.
Total script time is also network-dominated where the page snapshot cache
misses: YouTube fetched ~2.1-2.7 s per run against 42 ms of execution, GitHub
~0.7-1.0 s against 250 ms.

To confirm the optimizations do reach page code when a page actually runs it,
the same measurement was taken on a local loopback page whose only script does
300k iterations of one call plus one property read:

| page | exec before (median of 3) | exec after (median of 3) | change |
|---|---:|---:|---:|
| local call+property page | 246 ms | **166 ms** | **-33%** |

So the page-level path shows the same gain as `call`/`property` once the script
survives parsing. The reference pages remain the acceptance target, and their
honest before/after is "no measurable change yet, because they do not reach the
optimized code". Closing that gap is parser coverage (P7), not another
interpreter micro-optimization.

### Shortcuts (deliberate)

- Sloppy mode only; strict, module, async and module-resolution tests are
  skipped. The 39.3% is the sloppy subset, not the whole suite.
- One engine per test, no fixture reuse. Isolation is worth the 8 ms/test.
- No test262 score is published as "conformance"; directories with known
  missing features (`eval-code`, `arguments-object`) are visible in the table.
- Bench programs are small and hand-written, not a browser workload. They
  isolate the interpreter; step 4 adds the page-level check on top.
- The reference-page exec numbers cover only the scripts that parse; the
  bundles that abort contribute parse cost and nothing else.
- No JIT, by product rule. The target is a faster interpreter, not a compiler.

### Reproduce

```powershell
# once: sparse suite (~2 min, needs the network)
git clone --depth 1 --filter=blob:none --sparse https://github.com/tc39/test262.git "$env:TEMP\test262"
git -C "$env:TEMP\test262" sparse-checkout set test/language harness

cargo build --release --offline -p mazda-engine --bin test262 --bin js-bench
.\target\release\test262.exe --dir "$env:TEMP\test262\test\language" --harness "$env:TEMP\test262\harness" --failures 50
.\target\release\js-bench.exe --iterations 10 --warmup 3

# page-level script turn (needs the network for the reference sites)
$env:MAZDA_TIME="1"
.\target\release\renderer.exe --once https://www.google.com 2>&1 | Select-String '^\[time\] scripts'
Remove-Item Env:MAZDA_TIME
```

## Parser coverage from the reference sites (2026-11-09)

P5's page-level measurement located the next bottleneck: the reference bundles
abort at parse before the call/property code the interpreter optimizations
target can run. Two parser gaps are closed; both are parser-only changes, and
the runtime paths are untouched.

### Unicode escapes in identifiers

Bundles write property names with `\u` escapes: YouTube's main chunk contains
`v.id.\u0275prov` (Angular's `ɵ` prefix) and GitHub's `pb` bundle has the same
shape. The lexer accepted raw non-ASCII identifier bytes but not escapes, so
both bundles died with ``unexpected character `\` in source``.

`read_ident` now decodes `\uXXXX` and `\u{...}` at identifier start and
continuation positions. A name that used an escape is deliberately not matched
against the keyword table: per the spec, a reserved word cannot be written as
an escape, so `\u0069f` is an identifier named `if`, not the statement keyword.
Invalid or lone-surrogate escapes are a syntax error, not a panic.

test262: **6,950 -> 7,003** (+53 pass, same run/skip counts).

### Computed class accessors

`class C { get[Symbol.toStringTag]() { ... } }` was read as a field named
`get`, then rejected with ``expected `;` after class field``. Accessor
detection now scans a computed key to its matching `]` and requires `(` after
it; `ClassItem::Accessor` and `StmtKind::DefineAcc` carry a `ClassKey`
(name or computed expression, boxed to keep `Stmt` at 72 bytes). Computed keys
are evaluated at class-definition time. Duplicate computed accessors are not
detected statically (the spec throws at definition); ours installs in order,
so the last definition wins.

test262: **7,003 -> 7,073** (+70 pass).

### Progress against the live bundles

Same pages, same host, error line/column from the renderer:

| bundle | before | after |
|---|---|---|
| GitHub `pb-2530...js` | line 136, ``\`` in source | line 214, `yield` form (365 lines) |
| GitHub `core-1244...js` | line 27, `yield` form | unchanged |
| YouTube main chunk | line 1323, ``\`` in source | line 1290, `yield` in `try` (32,041 lines) |
| Google `xjs` | line 946, `yield` in `try` | unchanged |

Exec time on a single run each rose (Google 167 ms, YouTube 351 ms, GitHub
328 ms) because more source now parses and runs; those are not regressions and
not comparable to the paired P5 numbers.

### Remaining blocker: generator resumption

Every remaining site error is the same feature: `yield` used as an expression
and/or inside `try`/`finally`.

```text
GitHub core:    ... && (yield [a, b])          line 27
GitHub pb:      "part" === n.type && (yield new h(...))
YouTube:        v in k || w(yield b(v))        inside try { } finally { }
Google xjs:     e(yield d(b.get()))            inside try { } finally { }
```

The generator VM (`gen.rs`) resumes at statement boundaries only, and its
parser validation rejects yields in other positions. Supporting these shapes
means either lowering yield-containing expressions to statement-level
temporaries (plus a `try` frame in the VM) or moving generator bodies onto a
bytecode state machine. That is the next scoped step; it is not another
interpreter micro-optimization.

## Generator yield lifting (P7 step 19.1, 2026-11-09)

Expression-position `yield` is rewritten in the parser, not in the VM. A
`YieldLift` pass runs over every generator body (function, method, object
method; sync and async) before the position check. It walks each expression
post-order and emits statement-level temporaries for everything evaluated
before a suspension:

```js
f(g(), yield x, h())   ->  let $t0 = g();
                           let $t1 = yield x;
                           f($t0, $t1, h())

a && (yield b)         ->  let $l = a; let $r = $l;
                           if ($l) { let $t = yield b; $r = $t }
```

Rules that keep it honest:

- evaluation stays left to right - once a later operand may suspend, every
  earlier operand is frozen in a temp first;
- `&&`, `||`, `??`, `?:`, and `,` lower to `if`/sequencing and assign their
  operand value, never a boolean;
- a call target keeps its receiver: `obj.m(yield x)` freezes `obj` and
  rebuilds `obj.m`, so `this` is unchanged;
- an `if` test may contain a yield (it runs once); loop conditions, switch
  case tests, `return yield`, `throw yield`, compound-assignment targets, and
  try bodies stay honest parse errors;
- temporaries are named with a NUL prefix, which the lexer cannot produce
  from source, so they cannot collide with or shadow user bindings;
- an expression with no yield is left untouched (the pass is a no-op for
  ordinary code).

### Two VM bugs the lift exposed

Both were latent in `gen.rs` and are fixed here:

1. `SStep::Pushed` (an if/loop pushed child frames) made the current batch
   continue with the statements *after* the construct. That only worked when
   the construct was the last statement in the batch. The remnant batch is now
   inserted *under* the child frames.
2. The same path is what lets lifted pre-statements and the statement that
   consumes their temps live in one batch (`let $t = yield x; f($t);`).

### Measured

- test262 sloppy subset: **7,073 -> 7,078** (+5), run/skip unchanged
  (17,678 run, 6,154 skip), 16.5 s clean.
- engine tests: **905** green (one new test exercises call-argument order,
  short-circuit values, `?:`, nested yields, receiver preservation,
  assignment targets, if conditions, loop bodies, and declarator splits).
- GitHub: the page's remaining script errors are module-fetch 404s for bare
  specifiers (`react`, `react-dom/client`) - every bundle now parses. Exec
  328 -> 515 ms on one run each because much more of the app runs; a single
  network run is not a paired benchmark.
- YouTube main chunk still stops at `try`/`finally` containing a yield
  (line 1290); Google `xjs` at line 946. That is step 19.2, not this one.

## Try frames in generator resumption (P7 step 19.2, 2026-11-09)

`yield` inside `try`/`catch`/`finally` now works. The walker keeps a
`GFrame::Try { catch, finally, env, phase, pending }` under the block it is
executing, so a suspension leaves the try scope live on the frame stack.

- normal completion of the try block (or catch block) runs `finally`, then
  continues;
- a throw from a resumed section unwinds frames to the innermost try frame,
  binds the catch parameter in a child env (the catch block may suspend), or
  runs `finally` and rethrows;
- `return`/`break`/`continue` unwind the same way: every `finally` between the
  completion and its target runs first. A `break`/`continue` that leaves the
  try resumes after the finally completes;
- a throw inside `finally` replaces the pending completion (the frame's
  `pending: Option<Abrupt>` is discarded);
- `.return(v)` runs live finally blocks; if one of them suspends, `.return`
  reports that yield (`done: false`) and the generator stays resumable, with
  the pending return still on the Try frame;
- budget errors are not catchable and do not run finally (a runaway finally
  must not get another turn).

### Array elisions the reference bundle exposed

YouTube's ES5 chunk contains `return[,{filterRaw:...}]` - a *leading* hole in
an array literal. The parser only handled interior holes (`[a,,b]`), so this
failed with `unexpected token Comma`. The literal grammar now treats a comma
at an element position as an elision (`[,a]`, `[a,,b]`, `[,]`) while a
trailing comma still adds no hole (`[a,]` has length 1).

This one character shape was worth **+92 test262 passes** on its own, because
array-destructuring tests use leading elisions everywhere.

### Measured

- test262 sloppy subset: **7,078 -> 7,172** (+94: +2 try frames, +92 array
  elisions), run/skip unchanged (17,678 run, 6,154 skip), 34-37 s for two
  consecutive runs after a fresh build.
- engine tests: **906** green; `generator_try_resumption` covers normal
  completion, throw-after-resume, yield in catch, return through finally,
  yield in finally, break through finally, `.return()` with a live finally,
  `.return()` suspending in a finally, and a throw from finally replacing the
  pending error; `array_basics` covers leading/interior/trailing elisions.
- Live pages, one run each after the fix (coverage numbers, not paired
  benchmarks; exec rises because the whole bundle now runs instead of stopping
  at a syntax error):
  - YouTube `kevlar_base` ES5 chunk: 442 -> 2,892 ms exec, **syntax errors 0**
    (was: try/finally yield, then a leading elision);
  - Google `xjs`: 303 -> 594 ms exec, **syntax errors 0** (was: try/finally
    yield, then an inline `[,{...}]`);
  - GitHub: 515 -> 822 ms exec, syntax errors 0 (remaining messages are
    module-fetch 404s for bare specifiers).
- Every reference bundle now parses end to end. The remaining live-page
  errors are runtime gaps (missing DOM/API surface), not syntax.

### One honest gap the design leaves

A destructuring `yield` initializer that throws while binding (e.g.
`let [a] = yield null`) errors before the walker resumes inside the try, so an
enclosing catch does not see it. No reference bundle uses that shape.

## Destructuring grammar and generator iterables (P7, 2026-11-09)

The failure histogram after 19.2 put generated destructuring tests at the top
of the parser queue, and they exposed two grammar holes rather than runtime
behaviour.

### Nested patterns after an array rest

`[...[x, y, z]]` and `[...{length}]` are legal: the rest target is a
BindingPattern, not just a BindingIdentifier. The parser rejected everything
but a simple name (331 `expected binding name in pattern` failures, plus the
same shape in arrow parameters). `pattern_inner` now parses a nested pattern
after `...` in an array pattern; the runtime already binds the rest array
against any pattern via `bind_arr`'s `PatPart::Rest` arm.

### Trailing comma in an object pattern

`const {a,} = x`, `function f({x,})` and `({x,}) => x` were rejected with an
`unexpected token RBrace` (139 failures). The property loop consumed the
trailing comma, then broke out; the loop's own `eat(RBrace)` only runs when
the comma was *not* consumed inside the loop, so the `}` was left in the
token stream. The trailing-comma arm now consumes the brace itself.

### Generators are their own iterators

`get_iterator` knew arrays, strings, `Iterator` objects and `[Symbol.iterator]`
objects but not `ObjKind::Generator(_)`. A generator already has `next` (and
`return`), so `[...g()]`, `for (const v of g())` and destructuring a
generator's result all failed with `object is not iterable` (320 failures,
mostly `f(g())` in the generated destructuring templates). The generator is
now returned as its own iterator.

### Measured

- test262 sloppy subset: 7,172 -> **7,334** from the two parser fixes alone
  (+162: nested rest +51, object trailing comma +111), then **7,719** (43.7%)
  once generators become iterable (+385).
- engine tests: **907** green; `destructuring_arrays` /
  `destructuring_objects` cover nested rest targets and trailing commas in
  declarations, parameters and arrows; `generator_is_iterable` covers spread,
  for-of and destructuring of a generator.
- The remaining generated-test failures are no longer template syntax; they
  are runtime semantics (iterator closing, `assert` helper surface, strict
  mode, argument-object shape).

## Engine errors caught as real Error objects (P7, 2026-11-09)

`JsError::throw` has always carried a plain string (`"TypeError: ..."`), so
`catch (e)` bound a string: `e instanceof TypeError` was false, `e.message`
was undefined, and test262's `assert.throws` aborted inside its own
comparison (`thrown.constructor.name` where `constructor` was missing) -
892 failures with `Cannot read property 'name' of undefined`.

Three changes, all at the boundary where an engine error becomes a value
script can observe:

- the Error family is complete: `ReferenceError`, `EvalError` and `URIError`
  join `Error`/`TypeError`/`RangeError`/`SyntaxError`. Before this,
  `ReferenceError` itself resolved to undefined, so `assert.throws(
  ReferenceError, ...)` failed while evaluating its first argument.
- every error prototype exposes `constructor` (the shared `Error.prototype`
  points at `Error`); `e.constructor === TypeError` now works.
- `interp::caught_value` turns a caught engine string into an instance of the
  named class - used by `try`/`catch`, the generator walker's catch frames,
  async-function settlement and promise reaction/finally callbacks. A user
  `throw "plain string"` is untouched; only strings whose prefix is exactly
  one of the seven class names plus `": "` are materialised.

Engine-internal throws still travel as strings, so nothing on the hot path
changed; the cost is one object allocation when a script actually catches.

### Measured

- test262 sloppy subset: 7,850 -> **8,366** (47.3%), +516 with no skip
  change (17,678 run, 6,154 skip).
- engine tests: **907** green; the error-family test now checks caught
  engine errors (`instanceof`, `name`, `message`, `constructor`) and the
  three added classes.

## Lexer escapes, hashbang, and function names (P7, 2026-11-09)

### Private names with escapes

The lexer already modelled `#field` as an ordinary identifier with a `#`
prefix (so member access and class members reuse the normal property path),
but it only accepted a literal identifier character after `#`. The 551
remaining `unexpected character #` failures were all generated
private-membership tests using `#\u{6F}_`-style escaped names. `read_ident`
was split into a decoding core (`read_ident_name`) reused by the private-name
arm, so escapes resolve exactly as they do for public identifiers. This
alone moved 8,368 -> **8,530**.

### Hashbang

`#!...` at byte 0 is a comment (`comments/hashbang`). Two of the five cases
pass; the rest require strict-mode parsing or LS/PS line terminators in the
hashbang line, which are unaffected by this change. It is kept because Node
launchers put one in real bundles.

### NamedEvaluation for anonymous initializers

`var fn = function () {}`, `gen = function* () {}`, `arrow = () => {}`,
destructuring defaults, object-pattern defaults, parameter defaults and plain
`x = function () {}` assignment now give the anonymous function the bound
name, so `.name` is `'fn'` instead of `''`. The parser helper
`name_anon_fn` rewrites the `FnDef` name at parse time; ClassExpression
inference (`cls = class {}` -> `.name === 'cls'`) is still absent because the
class lowering needs its generated binding name, and parenthesized or computed
initializers keep the empty name.

Measured: 8,530 -> **8,971** (50.7% of run), +441 (110 each for `fn`, `gen`,
`arrow`, and the mixed generated defaults). Engine tests **909** green.

### Measurement-tool visibility

Every assertion failure previously reported `Uncaught [object Object]`, because
`Val::to_string` prints that for a user error object whose text lives in
`message`. `JsEngine::describe_thrown` reads `name`/`message` for the test262
report (it never calls user code), which turned the 4,040 opaque failures into
readable reason rows and made this whole wave measurable.

## Early errors, eval, boxed primitives, and descriptors (P7, 2026-11-09)

Five measured waves after the lexer/name work. Each one started from a
reason bucket in the test262 failure dump, not from a guess.

### Rest must be last in a pattern

`[...a, b]` and `[{...o, x}]` are early errors. The parser accepted them as
ordinary elements, so the immediately-invoked `assert.throws(SyntaxError, ...)`
templates failed. `pattern_inner` now rejects any element after a rest target
and any property after `...` (including a trailing comma) in both array and
object patterns. 8,971 -> **9,226** (+255).

### Strict directive with a non-simple parameter list

`function f(a = 1) { "use strict"; }` is an early error, as are the method,
accessor and arrow forms. `has_use_strict` looks only at the leading
expression-statement directives, so a string after a real statement is not
mistaken for one; `check_strict_body` / `check_strict_arrow` run once the
parameter list is known. 9,226 -> **9,336** (+110).

### `eval`

Direct `eval(...)` with an `Ident("eval")` callee resolving to the intrinsic
runs in the calling environment; every other path (an aliased variable,
`Reflect`) goes through the `n_eval` native and uses the global environment.
Non-string arguments are returned unchanged and syntax errors are catchable
`SyntaxError` objects. `var`/function leakage and `let`/`const` leakage both
follow the same env, which is a documented shortcut. 9,336 -> **9,666**
(+330); the 355 `different error constructor` failures disappeared because
those tests now throw the right `SyntaxError`.

### Boxed primitives

`new String/Number/Boolean` (and `Object(prim)`) produce
`ObjKind::Boxed(Val)`. `to_string`/`to_number` unwrap the primitive,
`call_method` unboxes `this`, a boxed String answers `length`/index reads,
`Add` coerces a boxed Number/Boolean numerically, and
`Object.prototype.toString`/`JSON.stringify` report the primitive tag/value.
9,666 -> **10,069** (+403).

### Class members are non-enumerable own properties

A class method or accessor must report `enumerable: false` from
`Object.getOwnPropertyDescriptor`, must be skipped by `Object.keys` and
`for (k in ...)`, while still being an own property. Methods are installed by
a new `StmtKind::DefineMethod` instead of a plain assignment, and both method
and accessor lowering record the key in a hidden-key list. The list lives
inside `ObjKind::Plain(Option<Box<Vec<_>>>)` and in a combined
`ClosureMeta` (with the `extends` [[Prototype]]) so `Obj` stays 64 bytes and
`Closure` does not grow. The implicit `prototype.constructor` back-reference
is recorded as hidden too. 10,069 -> **10,553** (+484, 59.7% of run).

### Measured

- test262 language subset: 8,971 -> **10,553** pass (59.7% of 17,678 run,
  6,154 skipped). Engine tests **913** green.
- One reusable RAM guard: `js_allocation_units_stay_compact` caught the first
  version of the descriptor change growing `Obj` to 72 bytes; the hidden list
  was folded into the existing `Plain` variant instead of a new field.

## Import grammar, escaped reserved words, and function contexts (P7, 2026-11-09)

Three more measured waves, all driven by the refreshed failure histogram.

### Dynamic-import call grammar

`ImportCall` takes exactly one argument plus optional import attributes.
The parser accepted `import()`, a second/third argument, a spread, and bare
`import` / `import.x` / `new import(...)`. Those shapes are now rejected;
`import(...)` and `import.meta` still parse. 10,553 -> **10,836** (+283).

### Escaped reserved words, duplicate params, `void yield`

Escaped identifiers (`\u0062reak`) decode to the same StringValue, so they
are rejected as identifiers, bindings and shorthand properties, while
staying legal as property names (`o.\u0275prov`). A non-simple parameter
list may not repeat a bound name; a simple list still may in sloppy mode.
A `yield` expression is not a unary operand (`void yield`). 10,836 ->
**11,037** (+201).

### Escaped `await` / `yield` in async and generator contexts

`await` is a reserved word only inside async functions and `yield` only
inside generators. The escaped spellings lex as identifiers, so the parser
now keeps a `fn_ctx` stack (`is_async`, `is_generator`) pushed around
function, method, accessor and arrow bodies (arrow parameters inherit
`yield`, async arrow parameters reserve `await`). `check_ctx_ident` runs
on identifier references, bindings, parameters, labels and shorthand
properties. A nested non-async/non-generator function gets a fresh context,
so `async function f() { function g() { var \u0061wait; } }` still parses.
11,037 -> **11,187** (+150).

### Measured

- test262 language subset: 10,553 -> **11,187** pass (63.3% of 17,678 run,
  6,154 skipped). Engine tests **915** green.

### `delete` of a private reference

`delete obj.#x` and `delete obj.#m()` are early errors even in sloppy
mode: a private reference is never a valid delete operand. Parenthesized
forms are covered too because the AST drops parens. `unary_expr` now
rejects a `Member` whose key starts with `#` and a `Call` whose callee is
such a member. 11,187 -> **11,379** (+192).

### Measured

- test262 language subset: 11,187 -> **11,379** pass (64.4% of 17,678 run,
  6,154 skipped). Engine tests **916** green.

### Destructuring catch parameters

`catch ([a, b])` / `catch ({ x })` are legal catch parameter forms. The
catch clause now carries an optional `Pat` and binds the caught value with
the existing `bind_pat` machinery (`BindKind::Decl(Let)`); the interpreter
and the resumable generator walker both use it, and the simple-name and
no-binding forms are unchanged. 11,379 -> **11,440** (+61).

### Measured

- test262 language subset: 11,379 -> **11,440** pass (64.7% of 17,678 run,
  6,154 skipped). Engine tests **917** green.

### Trailing comma in parameter lists

`function f(a, b,) {}`, `(a, b = 2,) => ...` and `m(a,) {}` are legal;
a rest parameter may not be followed by a comma (`(...a,)`). The shared
`params_after_lparen` accepts the trailing comma and rejects it after a
rest element. 11,440 -> **11,470** (+30).

### Measured

- test262 language subset: 11,440 -> **11,470** pass (64.9% of 17,678 run,
  6,154 skipped). Engine tests **918** green.

### `delete` removes own properties of functions

`delete_member` only handled `Val::Obj`, so `delete C.staticMethod` on a
class constructor silently returned false. Functions are objects: the
closure's own-property list is now pruned too. This is what
`verifyProperty`'s configurable check exercises, so the class member
descriptor tests stop reporting `m descriptor should be configurable`.
11,470 -> **11,588** (+118).

### Measured

- test262 language subset: 11,470 -> **11,588** pass (65.6% of 17,678 run,
  6,154 skipped). Engine tests **919** green.

### Array iterator methods

`Array.prototype` had no `values`/`keys`/`entries` and no
`[Symbol.iterator]`, so `[1][Symbol.iterator]` was `undefined` even though
engine-internal iteration fast-pathed arrays. The three methods now exist
(reusing `make_iterator` / `IterKind`), and `Symbol.iterator` is the same
native as `values`. `get_iterator` no longer unconditionally fast-paths
arrays: it reads the live `Array.prototype[Symbol.iterator]`, uses the fast
path only while it is still the builtin native, calls a user replacement,
and reports "not iterable" after a delete. 11,588 -> **11,702** (+114).

### Measured

- test262 language subset: 11,588 -> **11,702** pass (66.2% of 17,678 run,
  6,154 skipped). Engine tests **921** green.

### Object destructuring requires a coercible value

`var {} = null` / `function f({})` with `undefined` are TypeErrors: an
object binding pattern runs RequireObjectCoercible before reading any
property, so even an empty pattern throws. `bind_obj` now rejects
`null`/`undefined`; primitives still coerce. 11,702 -> **11,824** (+122).

### Measured

- test262 language subset: 11,702 -> **11,824** pass (66.9% of 17,678 run,
  6,154 skipped). Engine tests **922** green.

### Unresolvable identifier reads throw ReferenceError

An identifier read that resolved to no binding and no global-object property
silently returned `undefined`, which is why the `-unresolvable` destructuring
defaults never threw. `env_get_checked` now walks the same chain and throws
`ReferenceError: x is not defined` when the name is absent, while `typeof
ident` and the host-facing `get_global` keep the lenient `env_get`. A global
property that exists but holds `undefined` stays resolvable, and sloppy
assignment still creates the global. `undefined` is now installed as a real
global property (as are `NaN`/`Infinity` already). 11,824 -> **12,770**
(+946).

### Measured

- test262 language subset: 11,824 -> **12,770** pass (72.2% of 17,678 run,
  6,154 skipped). Engine tests **923** green.

### Class element reserved names

`static prototype`, `static constructor` and `#constructor` cannot be class
member names, and a `constructor` cannot be an accessor, generator or async.
`class_body` now rejects those shapes while leaving instance methods named
`prototype` legal. 12,770 -> **12,806** (+36).

### Measured

- test262 language subset: 12,770 -> **12,806** pass (72.4% of 17,678 run,
  6,154 skipped). Engine tests **924** green.

### Private name scope and uniqueness

Each class body now records its declared private names and the private
references parsed inside it. When the body ends, every reference must name a
member of that class or an enclosing one, so `something.#x` with no `#x`
declaration is a SyntaxError (forward references stay legal). Duplicate
private names are rejected, except the getter/setter halves of one accessor;
`super.#m` and private keys in object patterns are rejected outright.
Heritage expressions are parsed before the class context is pushed, so they
cannot see the subclass's own private names. 12,806 -> **12,931** (+125).

### Measured

- test262 language subset: 12,806 -> **12,931** pass (73.1% of 17,678 run,
  6,154 skipped). Engine tests **925** green.

### `super()` only inside a class constructor

`super(...)` was accepted anywhere the class had a superclass, so method
bodies that should be early errors parsed clean. `ClassCtx` now carries
`in_constructor`, reset per class element and set only while the constructor
body is parsed; the `super(...)` path rejects it elsewhere. `super.m()` in
ordinary methods is untouched. 12,931 -> **12,971** (+40).

### Measured

- test262 language subset: 12,931 -> **12,971** pass (73.4% of 17,678 run,
  6,154 skipped). Engine tests **926** green.

### Block / switch lexical redeclaration early errors

A StatementList may not declare the same name both lexically (`let`/`const`/
`class`/block-level function) and with `var`, and its lexical names must not
repeat. The parser now keeps a stack of StatementLists (program, function
bodies, blocks, switch CaseBlocks), records declaration names as they are
parsed, propagates `var` names outward until the nearest function body or the
program, and checks the scope when it closes. Function declarations are
var-scoped at the top of a function body/program and lexical inside a block.
12,971 -> **13,128** (+157).

### Measured

- test262 language subset: 12,971 -> **13,128** pass (74.3% of 17,678 run,
  6,154 skipped). Engine tests **927** green.

### `with` statement

`with` was lexed as an identifier and rejected as a reserved word, so every
sloppy-mode `with` test failed at parse time. It is now a keyword parsed into
a `With` statement. Evaluation builds an object environment record
(`Env::with_obj`): identifier reads consult the object (through its prototype
chain) before the enclosing scopes, `typeof` does the same, and assignment
writes the property when the object has it, otherwise falls through to the
outer binding. `var` inside the body still hoists. 13,128 -> **13,238**
(+110).

### Measured

- test262 language subset: 13,128 -> **13,238** pass (74.9% of 17,678 run,
  6,154 skipped). Engine tests **928** green.

### `yield*` delegation

`yield*` did not parse (`unexpected token Mul`): the parser read `yield`
and then failed on the `*`. It is now a distinct `YieldStar` expression. The
generator walker gained a `Delegate` frame: each resume calls the iterator's
`next(arg)` (forwarding the value fed to the outer `.next`), forwards the
yielded values, and binds the delegate's final `value` when `done` is true
(`let x = yield* it`). Non-iterable operands throw a TypeError at first
resume; `yield*` outside a generator is still a SyntaxError. 13,238 ->
**13,305** (+67).

### Measured

- test262 language subset: 13,238 -> **13,305** pass (75.3% of 17,678 run,
  6,154 skipped). Engine tests **929** green.
- Honest gaps: `.throw()` forwarding to the delegate and calling the
  delegate's `return()` on a `.return()` are not implemented yet.

### `arguments` in class field initializers

Field initializers run with no `arguments` binding, so any reference — including
one reached through an arrow inside the initializer — is a SyntaxError.
`scan_expr_arguments` now treats arrows as transparent (their `arguments` is
the enclosing function's, which also fixes functions that only referenced
`arguments` through an arrow) and still stops at plain function boundaries.
A field initializer containing one is rejected at parse time. 13,305 ->
**13,363** (+58).

### Measured

- test262 language subset: 13,305 -> **13,363** pass (75.6% of 17,678 run,
  6,154 skipped). Engine tests **930** green.

### Escaped `get` / `set` / `async` are not modifiers

Escaped spellings never lex as keywords, so `\u0067et m() {}` in an object
literal or a class body is an ordinary property named `get` followed by junk —
a SyntaxError — not an accessor. The parser now checks the raw token text for
an escape before treating an identifier as the `get`/`set` accessor keyword or
the `async` method modifier (object literals and class bodies). Unescaped
forms are unchanged, and `\u0067et: 7` stays a legal property. 13,363 ->
**13,387** (+24).

### Measured

- test262 language subset: 13,363 -> **13,387** pass (75.7% of 17,678 run,
  6,154 skipped). Engine tests **931** green.

## No memory cap

The renderer job no longer sets `JOB_OBJECT_LIMIT_PROCESS_MEMORY`/
`JOB_OBJECT_LIMIT_JOB_MEMORY` and the Linux sandbox no longer sets `RLIMIT_AS`;
the shell's governor no longer has a budget or reclaims. The 512 MB job cap was
what turned a heavy page into a killed renderer at 508 MB, and a cap hides the
real problem instead of fixing it: the renderer must stay lean by what it
allocates. The job still allows one process and kills the renderer with the
shell.

### Measured

- cold `renderer.exe https://github.com --once` (fresh cache): 32 s, peak
  private 133 MB; stages: scripts 7.9 s, css parse 154 ms, cascade + box tree
  7.9 s, image probe 5.1 s, media probe 3.2 s, layout 4 ms.
- uncapped `shell.exe https://github.com`: first page at 27 s, shell peak
  68 MB, renderer peak 164 MB.

## Cascade rule index

`Sheet::resolve` walked every rule for every element. GitHub ships 31.5k rules
and 2,665 DOM nodes, so the cascade did ~84M selector matches for one page.
Rules are now bucketed by the necessary key of their rightmost compound
(id > class > tag, else universal): each element tests only the buckets it
falls into, and the `:hover` bucket is merged back at its exact
`(spec, order)` position. Candidate indices reuse a thread-local scratch
buffer, and `subst_scoped` borrows the declaration when it has no `var()`
instead of cloning both strings. `slot[name=X]` rules stay universal because
they also select the light-DOM nodes projected into the slot.

### Measured

- cold `renderer.exe https://github.com --once`: cascade + box tree
  7,891 ms -> **1,240 ms**; total 32 s -> 27.5 s (network fetch varies
  23-28 s); peak private 133 MB -> **131 MB**.
- `shell.exe https://github.com`: renderer settles at **75 MB private** and
  stays flat for a minute (no timer/RAF growth).
- engine tests **932** green (new rule-index test).

## Destructuring rest early errors (P7, 2026-11-09)

`AssignmentRestElement` / `AssignmentRestProperty` must be the last element of
a destructuring assignment and take no trailing comma. The literal parsers
already accept `[...a,]` and `{...a,}` (legal as expressions), so
`ArrayElem::Spread` / `ObjElem::Spread` now carry a `trailing_comma` flag and
`expr_to_pat` rejects a non-final spread or one followed by a comma. A rest
parameter must likewise be the last parameter, so `params_after_lparen`
rejects `function f(a, ...b, c) {}` (the `(...a,)` case lands on the same
check). `array-rest-nested-array-invalid` and `array-rest-nested-obj-invalid`
already passed; the new flags cover the plain and object forms.

### Measured

- `language/rest-parameters`: 8 -> **9** pass (11 run); only unrelated
  `arguments.length` / `Array` constructor failures remain.
- `language/expressions/assignment/dstr`: 228 -> **233** pass (340 run).
- test262 language subset: 13,387 -> **13,403** pass (75.8% of 17,678 run,
  6,154 skipped).
- engine tests **932** green (rest-last assertions in the existing
  trailing-comma, array/object destructuring and assignment tests).

## Number static constants (P7 fix, 2026-11-09)

`Number` only exposed its prototype, so `Number.POSITIVE_INFINITY`,
`MAX_VALUE`, `MIN_VALUE`, `NaN` and friends were `undefined`. Arithmetic
tests using them produced `NaN` (e.g. `Number.MAX_VALUE + -0 === NaN`) and
threw. The constructor now carries its value properties: `MAX_VALUE`,
`MIN_VALUE` (smallest positive subnormal), `POSITIVE_INFINITY`,
`NEGATIVE_INFINITY`, `NaN`, `EPSILON`, `MAX_SAFE_INTEGER`,
`MIN_SAFE_INTEGER`.

### Measured

- test262 language subset: 13,403 -> **13,483** pass (76.3% of 17,678 run,
  6,154 skipped).
- `language/expressions/addition`: 27/48 -> 34/48 pass; the remaining 14 are
  ToPrimitive / `Symbol.toPrimitive` cases.
- engine tests **932** green.
## Unicode white space in the lexer (P7 fix, 2026-11-09)

`skip_ws` only knew ASCII space/tab/VT/FF and CR/LF, so NBSP (U+00A0), the
BOM/ZWNBSP (U+FEFF) and the other Zs characters lexed as identifiers. Any
source that used them between tokens failed with a spurious parse error; the
addition tests hit this through `eval("1\u00a0+\u00a02")`. The lexer now
decodes a non-ASCII character at a token boundary and skips it when it is
white space (or U+FEFF), while U+2028/U+2029 count as line terminators.
Anything else still starts an identifier, so the loose UTF-8 identifier rule
is unchanged.

### Measured

- test262 language subset: 13,483 -> **13,528** pass (76.5% of 17,678 run,
  6,154 skipped).
- `language/white-space`: 45/67 -> **64/67** pass.
- engine tests **933** green (new unicode-whitespace test).
## Observable ToPrimitive in binary operators (P7, 2026-11-09)

Arithmetic and comparison used `Val::to_string`/`to_number`, which never ran
user code: `({ valueOf: () => 1 }) + 1` produced `"[object Object]1"` and
`{ valueOf() { throw x } } + 1` did not throw. `binary_op` now performs
`ToPrimitive` with the spec hint before coercing: `Symbol.toPrimitive` wins
(with the hint string), otherwise `valueOf`/`toString` run in hint order and
must return a primitive or raise a TypeError. `+` uses the default hint
(string concat when either result is a string), arithmetic/bitwise/shift use
the number hint, relational uses the number hint, and `==` against an object
coerces it (object-vs-object stays identity). Boxed primitives unwrap
directly, `Date` uses the string hint for `default`, and
`Array.prototype.toString` now aliases `join` so `[1, 2] + ''` stays `"1,2"`.

### Measured

- test262 language subset: 13,528 -> **13,607** pass (77.0% of 17,678 run,
  6,154 skipped).
- `language/expressions/addition`: 34/48 -> **44/48**; `equals`: 22/47 ->
  **43/47**; `subtraction`: 26/38 -> **35/38**; `less-than`: 33/45 ->
  **41/45**; `bitwise-and`: 22/30 -> **25/30**.
- engine tests **934** green (new ToPrimitive test; the old `{} == {}` test
  now expects identity).
- Remaining gap: `String(obj)` / template literals still use the non-observable
  `Val::to_string` fast path.
## String.fromCharCode (P7 fix, 2026-11-09)

`String.fromCharCode` was missing, so the legacy string-literal tests that
compare against `String.fromCharCode(...)` failed with a TypeError. The
static now builds a string from UTF-16 code units (arguments go through
ToUint16; a surrogate pair combines). Lone surrogates become U+FFFD because
`Val::Str` is UTF-8.

### Measured

- test262 language subset: 13,607 -> **13,622** pass (77.1% of 17,678 run,
  6,154 skipped).
- engine tests **934** green.
## Numeric separators (P7, 2026-11-09)

The lexer read runs of digits, so `1_000`, `0x1_0` and `1_0n` split into a
number plus an identifier. Numbers now scan with `_` allowed only between two
digits of the same radix; any other placement (`1_`, `1__0`, `0x_1`,
`10._1`, `1_e1`) is a SyntaxError. Legacy leading-zero forms (`0_1`, `08_0`)
reject separators too, and the underscore is stripped before the value is
parsed.

### Measured

- test262 language subset: 13,622 -> **13,681** pass (77.4% of 17,678 run,
  6,154 skipped).
- `language/literals/numeric`: 114/146 -> **144/146** pass.
- engine tests **935** green (new numeric-separator test).
## Numeric literal edges (P7, 2026-11-09)

Two lexer gaps kept `literals/numeric` from passing: Annex B legacy octal
(`070` was read as decimal 70 instead of 56) and a numeric literal running
straight into an identifier (`3in []`, `1foo`, `0x1g` were accepted). The
scanner now parses a leading-zero run of octal digits as base 8 (while
`08`/`09` stay decimal), and rejects an identifier start or digit right after
a number or BigInt suffix.

### Measured

- test262 language subset: 13,681 -> **13,683** pass (77.4% of 17,678 run,
  6,154 skipped).
- `language/literals/numeric`: 144/146 -> **146/146** pass.
- engine tests **936** green.
## Computed accessor names in object literals (P7, 2026-11-09)

`get [expr]() {}` / `set [expr](v) {}` were not recognized as accessors: the
`get`/`set` lookahead only accepted literal property names, so the literal
fell through to shorthand and failed with `expected Comma, found LBrack`.
`ObjElem` now has `ComputedAccessor { key, getter, setter }`; the key is
evaluated once when the object is created and the getter/setter pair installs
an `Acc` entry under it, matching the static accessor path. Yield lifting,
`yield`/`arguments` scanning and pattern rejection cover the new variant.

### Measured

- test262 language subset: 13,683 -> **13,714** pass (77.6% of 17,678 run,
  6,154 skipped).
- `language/expressions/object`: 742/935 -> **763/935** pass.
- engine tests **937** green (new computed-accessor test).
## BigInt legacy leading-zero rejection (P7 fix, 2026-11-09)

A decimal BigInt literal could carry a leading zero (`00n`, `01n`, `08n`,
`0008n`). Those are legacy-octal / non-octal forms and have no BigInt
spelling, so they are SyntaxErrors. The scanner rejects a decimal BigInt
whose digit text starts with `0` and is longer than one character. (The
matching test landed with the computed-accessor commit; this commit adds the
lexer change that makes it pass.)

### Measured

- `language/literals/bigint`: 52/59 -> **59/59** pass.
- test262 language subset stays at **13,714** pass (the full run that
  produced that number already included this working-tree change).
- engine tests **937** green.

## Class name inference from binding initializers (P7, 2026-11-09)

An anonymous class expression was lowered under a synthetic `__classN` name,
so `var cls = class {}` reported `cls.name === "__class1"` instead of `"cls"`.
`name_anon_fn` (the existing NamedEvaluation pass for function/arrow
initializers) now also handles `ClassExpr`: it rewrites the generated
scaffolding — the constructor's `FnDef.name`, the `prototype`/`static`
owner references and static-field assignment targets — to the bound name and
updates the class's internal binding. Method and constructor bodies are
user code and are left untouched. `ClassExpr` carries an `anon` flag so an
explicit `class Named {}` is never renamed.

### Measured

- test262 language subset: 13,714 -> **13,827** pass (78.2% of 17,678 run,
  6,154 skipped).
- `language/statements/class`: 2,703 -> **2,736** pass (3,354 run);
  `language/expressions/class`: 2,566 -> **2,602** pass (3,048 run).
- `language/statements/class/dstr`: 144 -> **112** fail; every
  `*-fn-name-class` case now passes.
- engine tests **938** green (new binding-name test).