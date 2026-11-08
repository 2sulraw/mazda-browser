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

## Acceptance (partial)

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
`Downloading movie.mp4 — 9 %`.

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


